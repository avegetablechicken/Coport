use serde_json::{Map, Value, json};
use std::{
    fs::{File, OpenOptions},
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Archives belong to this log basename; unrelated files and symlinks are ignored.
pub fn history_paths(path: &Path) -> std::io::Result<Vec<PathBuf>> {
    let directory = path.with_file_name("history");
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let prefix = format!(
        "{}.",
        path.file_name().unwrap_or_default().to_string_lossy()
    );
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if entry.file_type()?.is_file() && name.starts_with(&prefix) && name.ends_with(".jsonl") {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

fn prune_history(path: &Path, now: SystemTime) -> std::io::Result<()> {
    let Some(cutoff) = now.checked_sub(RETENTION) else {
        return Ok(());
    };
    let cutoff_ms = cutoff
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let prefix = format!(
        "{}.",
        path.file_name().unwrap_or_default().to_string_lossy()
    );
    for archive in history_paths(path)? {
        let name = archive.file_name().unwrap_or_default().to_string_lossy();
        let archived_ms = name
            .strip_prefix(&prefix)
            .and_then(|s| s.split('.').next())
            .and_then(|s| s.parse::<u128>().ok());
        // Retain for at least 30 days after archiving; later modifications extend
        // retention too. Never delete unrecognized/imported filenames by guessing.
        if archived_ms.is_some_and(|at| at < cutoff_ms)
            && std::fs::metadata(&archive)?
                .modified()
                .is_ok_and(|at| at < cutoff)
        {
            std::fs::remove_file(archive)?;
        }
    }
    Ok(())
}

fn rotate(path: &Path) -> std::io::Result<()> {
    let backup = path.with_file_name(format!(
        "{}.1",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    if backup.exists() {
        let history = path.with_file_name("history");
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&history)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let archive = history.join(format!(
            "{}.{now}.{}.jsonl",
            path.file_name().unwrap_or_default().to_string_lossy(),
            uuid::Uuid::new_v4()
        ));
        std::fs::rename(&backup, archive)?;
    }
    std::fs::rename(path, backup)?;
    if let Err(e) = prune_history(path, SystemTime::now()) {
        eprintln!("Cannot prune request log history: {e}");
    }
    Ok(())
}

/// Destination for records that cannot reach the log file, and for every record
/// when the proxy runs interactively.
type Console = Box<dyn Write + Send>;

/// Writes stderr through the print macros so the unit test harness captures it.
struct Stderr;
impl Write for Stderr {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if cfg!(test) {
            eprint!("{}", String::from_utf8_lossy(buf));
            Ok(buf.len())
        } else {
            std::io::stderr().write(buf)
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stderr().flush()
    }
}

const QUEUE_RECORDS: usize = 1024;
const MAX_RECORD: usize = 256 * 1024;
static QUEUE_BUDGET: LazyLock<Arc<crate::observation_memory::Budget>> =
    LazyLock::new(|| Arc::new(crate::observation_memory::Budget::new(8 * 1024 * 1024)));

struct RecordBytes(crate::observation_memory::Buffer);
impl Write for RecordBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > MAX_RECORD {
            return Err(std::io::Error::other("Log record exceeds 256 KiB"));
        }
        self.0.extend_from_slice(bytes)?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

enum Message {
    ReportDrops,
    Record(crate::observation_memory::Buffer),
    Flush(mpsc::SyncSender<std::io::Result<()>>),
}

/// Producers never perform file I/O or wait for a slow disk. Accepted records
/// are drained by the worker on drop; saturation is reported explicitly.
pub struct Logger {
    sender: Option<mpsc::SyncSender<Message>>,
    worker: Option<std::thread::JoinHandle<()>>,
    dropped: Arc<AtomicU64>,
}
impl Logger {
    pub fn new(path: PathBuf) -> Self {
        Self::with_console(path, Box::new(Stderr), std::io::stderr().is_terminal())
    }
    fn with_console(path: PathBuf, console: Console, mirror: bool) -> Self {
        Self::with_capacity(path, console, mirror, QUEUE_RECORDS)
    }
    fn with_capacity(path: PathBuf, console: Console, mirror: bool, capacity: usize) -> Self {
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let dropped = Arc::new(AtomicU64::new(0));
        let lost = dropped.clone();
        let worker = std::thread::Builder::new()
            .name("request-log".into())
            .spawn(move || {
                let mut sink = FileLog::new(path, console, mirror);
                while let Ok(message) = receiver.recv() {
                    sink.report_dropped(&lost);
                    match message {
                        Message::ReportDrops => {}
                        Message::Record(bytes) => sink.write(&bytes),
                        Message::Flush(reply) => {
                            let _ = reply.send(sink.flush());
                        }
                    }
                }
                sink.report_dropped(&lost);
                let _ = sink.flush();
            })
            .expect("request log thread");
        Self {
            sender: Some(sender),
            worker: Some(worker),
            dropped,
        }
    }
    pub fn write(&self, event: &str, mut fields: Map<String, Value>) {
        fields.insert("event".into(), json!(event));
        fields.insert("timestamp".into(), json!(timestamp()));
        let mut bytes = RecordBytes(crate::observation_memory::Buffer::with_budget(
            QUEUE_BUDGET.clone(),
        ));
        if serde_json::to_writer(&mut bytes, &fields).is_err() || bytes.write_all(b"\n").is_err() {
            self.report_drop();
            return;
        }
        if self
            .sender
            .as_ref()
            .is_none_or(|sender| sender.try_send(Message::Record(bytes.0)).is_err())
        {
            self.report_drop();
        }
    }
    fn report_drop(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
        if let Some(sender) = &self.sender {
            let _ = sender.try_send(Message::ReportDrops);
        }
    }
    /// Waits for records already accepted by this logger. Intended for explicit
    /// shutdown/inspection, not the async request path.
    pub fn flush(&self) -> std::io::Result<()> {
        let failed = || std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Log worker stopped");
        let (send, receive) = mpsc::sync_channel(0);
        self.sender
            .as_ref()
            .ok_or_else(failed)?
            .send(Message::Flush(send))
            .map_err(|_| failed())?;
        receive.recv().map_err(|_| failed())?
    }
}
impl Drop for Logger {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                eprintln!("Request log worker stopped unexpectedly.");
            }
        }
    }
}
fn timestamp() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Owned only by the logging thread: rotation and pruning need no shared lock.
struct FileLog {
    path: PathBuf,
    file: Option<File>,
    length: u64,
    checked: Instant,
    console: Console,
    mirror: bool,
}
impl FileLog {
    fn new(path: PathBuf, console: Console, mirror: bool) -> Self {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = prune_history(&path, SystemTime::now()) {
            eprintln!("Cannot prune request log history: {e}");
        }
        let file = open_private(&path).ok();
        if file.is_none() {
            eprintln!("Cannot open request log; continuing with stderr logging.");
        }
        let length = file
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .map_or(0, |m| m.len());
        Self {
            path,
            file,
            length,
            checked: Instant::now(),
            console,
            mirror,
        }
    }
    fn write(&mut self, bytes: &[u8]) {
        let written = self.write_file(bytes);
        if self.mirror || !written {
            let _ = self.console.write_all(bytes);
        }
    }
    fn write_file(&mut self, line: &[u8]) -> bool {
        // Account for external appends periodically, not with a syscall per record.
        if self.checked.elapsed() >= Duration::from_secs(1) {
            if let Some(meta) = self.file.as_ref().and_then(|f| f.metadata().ok()) {
                self.length = meta.len();
            }
            self.checked = Instant::now();
        }
        if self.file.is_some() && self.length.saturating_add(line.len() as u64) > 5 * 1024 * 1024 {
            self.file = None;
            if let Err(e) = rotate(&self.path) {
                eprintln!("Cannot rotate request log: {e}");
            }
            self.file = open_private(&self.path).ok();
            self.length = self
                .file
                .as_ref()
                .and_then(|f| f.metadata().ok())
                .map_or(0, |m| m.len());
        }
        let Some(file) = self.file.as_mut() else {
            return false;
        };
        if file.write_all(line).is_err() {
            self.file = None;
            eprintln!("Cannot write request log; continuing with stderr logging.");
            return false;
        }
        self.length += line.len() as u64;
        true
    }
    fn report_dropped(&mut self, dropped: &AtomicU64) {
        let count = dropped.swap(0, Ordering::AcqRel);
        if count > 0 {
            let mut bytes = serde_json::to_vec(&json!({"event":"log_records_dropped","timestamp":timestamp(),"count":count.to_string(),"reason":"queue_or_record_limit"})).unwrap();
            bytes.push(b'\n');
            self.write(&bytes);
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(file) = &mut self.file {
            file.flush()?;
        }
        self.console.flush()
    }
}
pub fn open_private(path: &Path) -> std::io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}
pub struct RequestLog {
    pub logger: std::sync::Arc<Logger>,
    pub fields: Map<String, Value>,
    pub started: Instant,
    /// Zero means no response status was established.
    pub status: u16,
    pub bytes: usize,
    pub outcome: &'static str,
}
impl RequestLog {
    pub fn event(&self, event: &str) {
        self.logger.write(event, self.fields.clone());
        crate::model_calls::http_event(self, event);
    }
    pub fn field(&mut self, key: &str, value: impl ToString) {
        self.fields.insert(key.into(), json!(value.to_string()));
    }
}
impl Drop for RequestLog {
    fn drop(&mut self) {
        if self.status != 0 {
            self.field("status", self.status);
        }
        if self.outcome == "request_cancelled" {
            // Includes client disconnects and shutdown cancellation, not a gateway response.
            self.field("reason", "request_dropped");
        }
        self.field("received_bytes", self.bytes);
        self.field("duration_ms", self.started.elapsed().as_millis());
        self.event(self.outcome);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<Mutex<Vec<u8>>>);
    impl Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Captured {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    #[test]
    fn a_slow_sink_does_not_block_producers_and_reports_overflow() {
        struct Slow {
            entered: Option<mpsc::Sender<()>>,
            resume: mpsc::Receiver<()>,
        }
        impl Write for Slow {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if let Some(entered) = self.entered.take() {
                    entered.send(()).unwrap();
                    self.resume.recv().unwrap();
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let (entered, ready) = mpsc::channel();
        let (resume, receive) = mpsc::channel();
        let logger = Logger::with_capacity(
            path.clone(),
            Box::new(Slow {
                entered: Some(entered),
                resume: receive,
            }),
            true,
            1,
        );
        logger.write("first", Map::new());
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        let (finished, done) = mpsc::channel();
        std::thread::scope(|scope| {
            let logger = &logger;
            let producer = scope.spawn(move || {
                logger.write("queued", Map::new());
                logger.write("dropped", Map::new());
                finished.send(()).unwrap();
            });
            let completed = done.recv_timeout(Duration::from_secs(1));
            resume.send(()).unwrap();
            producer.join().unwrap();
            assert!(completed.is_ok(), "producer blocked behind the slow sink");
        });
        logger.flush().unwrap();
        let rows: Vec<Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert!(rows.iter().any(|r| r["event"] == "first"));
        assert!(rows.iter().any(|r| r["event"] == "queued"));
        assert!(!rows.iter().any(|r| r["event"] == "dropped"));
        assert!(
            rows.iter()
                .any(|r| r["event"] == "log_records_dropped" && r["count"] == "1")
        );
    }

    #[test]
    fn a_dropped_record_is_reported_even_without_a_later_write() {
        struct Notify(mpsc::Sender<Vec<u8>>);
        impl Write for Notify {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.send(bytes.to_vec()).unwrap();
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let (notice, receive) = mpsc::channel();
        let logger =
            Logger::with_console(dir.path().join("proxy.log"), Box::new(Notify(notice)), true);
        logger.write(
            "too_large",
            json!({"data":"x".repeat(MAX_RECORD)})
                .as_object()
                .unwrap()
                .clone(),
        );
        let bytes = receive.recv_timeout(Duration::from_secs(5)).unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["event"], "log_records_dropped");
        assert_eq!(value["count"], "1");
    }

    #[test]
    fn shutdown_drains_accepted_records_and_oversized_records_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        {
            let logger = Logger::new(path.clone());
            for i in 0..100 {
                logger.write("accepted", json!({"n":i}).as_object().unwrap().clone());
            }
            logger.write(
                "too_large",
                json!({"value":"x".repeat(MAX_RECORD)})
                    .as_object()
                    .unwrap()
                    .clone(),
            );
        }
        let text = std::fs::read_to_string(path).unwrap();
        assert_eq!(text.matches("\"event\":\"accepted\"").count(), 100);
        assert!(text.contains("log_records_dropped"));
        assert!(!text.contains("too_large"));
    }

    #[test]
    fn open_log_file_keeps_records_off_noninteractive_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let console = Captured::default();
        let logger = Logger::with_console(path.clone(), Box::new(console.clone()), false);
        logger.write("server_started", Map::new());
        logger.flush().unwrap();
        assert!(console.text().is_empty());
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("\"event\":\"server_started\"")
        );
    }

    #[test]
    fn records_reach_stderr_when_interactive_or_without_a_log_file() {
        let dir = tempfile::tempdir().unwrap();
        let interactive = Captured::default();
        let path = dir.path().join("proxy.log");
        Logger::with_console(path.clone(), Box::new(interactive.clone()), true)
            .write("mirrored", Map::new());
        assert!(interactive.text().contains("\"event\":\"mirrored\""));
        assert!(std::fs::read_to_string(&path).unwrap().contains("mirrored"));

        // A directory in place of the file makes the log impossible to open.
        let blocked = dir.path().join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        let fallback = Captured::default();
        Logger::with_console(blocked, Box::new(fallback.clone()), false)
            .write("fallback", Map::new());
        let line = fallback.text();
        assert!(line.ends_with('\n'));
        assert!(line.contains("\"event\":\"fallback\""));
    }

    #[test]
    fn repeated_rotation_archives_every_previous_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        for value in ["first", "second", "third"] {
            std::fs::write(&path, value).unwrap();
            rotate(&path).unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("proxy.log.1")).unwrap(),
            "third"
        );
        let mut archived: Vec<_> = history_paths(&path)
            .unwrap()
            .into_iter()
            .map(|p| std::fs::read_to_string(p).unwrap())
            .collect();
        archived.sort();
        assert_eq!(archived, ["first", "second"]);
    }

    #[test]
    fn pruning_requires_both_archive_age_and_modification_age_over_thirty_days() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let history = dir.path().join("history");
        std::fs::create_dir(&history).unwrap();
        let now = SystemTime::now();
        let old = now - RETENTION - Duration::from_secs(60);
        let recent = now - Duration::from_secs(60);
        let archive = |prefix: &str, at: SystemTime, modified: SystemTime| {
            let file = history.join(format!(
                "{prefix}.{}.{}.jsonl",
                at.duration_since(UNIX_EPOCH).unwrap().as_millis(),
                uuid::Uuid::new_v4()
            ));
            std::fs::write(&file, "retained data").unwrap();
            File::options()
                .write(true)
                .open(&file)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(modified))
                .unwrap();
            file
        };
        let expired = archive("proxy.log", old, old);
        let recently_archived = archive("proxy.log", recent, old);
        let recently_modified = archive("proxy.log", old, recent);
        let unrelated = archive("other.log", old, old);
        prune_history(&path, now).unwrap();
        assert!(!expired.exists());
        assert!(recently_archived.exists());
        assert!(recently_modified.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn rotation_failure_keeps_logging_to_the_original_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        std::fs::write(dir.path().join("proxy.log.1"), "previous log").unwrap();
        std::fs::write(dir.path().join("history"), "blocks directory creation").unwrap();
        // Windows append-only handles do not permit set_len. Prepare the large
        // file with write access before Logger opens its normal append handle.
        File::create(&path)
            .unwrap()
            .set_len(5 * 1024 * 1024)
            .unwrap();
        let logger = Logger::new(path.clone());
        logger.write("still_logged", Map::new());
        logger.flush().unwrap();
        assert!(std::fs::metadata(path).unwrap().len() > 5 * 1024 * 1024);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("proxy.log.1")).unwrap(),
            "previous log"
        );
    }
}
