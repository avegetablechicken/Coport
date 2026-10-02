use serde_json::{Map, Value, json};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Instant,
};

pub struct Logger {
    path: PathBuf,
    file: Mutex<Option<File>>,
}
impl Logger {
    pub fn new(path: PathBuf) -> Self {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
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
            let backup = self.path.with_file_name(format!(
                "{}.1",
                self.path.file_name().unwrap_or_default().to_string_lossy()
            ));
            if backup.exists() && std::fs::remove_file(&backup).is_err() {
                eprintln!("Cannot rotate request log.");
                return;
            }
            if std::fs::rename(&self.path, backup).is_err() {
                eprintln!("Cannot rotate request log.");
                return;
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
