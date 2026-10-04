use serde_json::{Map, Value, json};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
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

pub struct Logger {
    path: PathBuf,
    file: Mutex<Option<File>>,
}
impl Logger {
    pub fn new(path: PathBuf) -> Self {
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
        Self {
            path,
            file: Mutex::new(file),
        }
    }
    pub fn write(&self, event: &str, mut fields: Map<String, Value>) {
        fields.insert("event".into(), json!(event));
        fields.insert(
            "timestamp".into(),
            json!(
                time::OffsetDateTime::now_utc()
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default()
            ),
        );
        let mut line = serde_json::to_vec(&fields).unwrap_or_default();
        line.push(b'\n');
        #[cfg(not(test))]
        let _ = std::io::stderr().write_all(&line);
        #[cfg(test)]
        eprint!("{}", String::from_utf8_lossy(&line));
        let Ok(mut file) = self.file.lock() else {
            return;
        };
        if file
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .is_some_and(|m| m.len() + line.len() as u64 > 5 * 1024 * 1024)
        {
            *file = None;
            if let Err(e) = rotate(&self.path) {
                // Keep appending to the original file if archiving fails.
                eprintln!("Cannot rotate request log: {e}");
            }
            *file = open_private(&self.path).ok();
        }
        if let Some(f) = file.as_mut() {
            if f.write_all(&line).is_err() {
                *file = None;
                eprintln!("Cannot write request log; continuing with stderr logging.");
            }
        }
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
        assert!(std::fs::metadata(path).unwrap().len() > 5 * 1024 * 1024);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("proxy.log.1")).unwrap(),
            "previous log"
        );
    }
}
