//! One-time, non-destructive migration out of the disposable application cache.
use std::{fs, io, path::Path};

const MARKER: &str = ".cache-migration-complete";
const ITEMS: [&str; 3] = ["config.yaml", "tls", "logs"];

/// Called after acquiring the GUI instance lock and before opening any data.
pub fn prepare() -> io::Result<()> {
    prepare_paths(
        &crate::settings::legacy_cache_dir(),
        &crate::settings::app_dir(),
    )
}

fn prepare_paths(source: &Path, target: &Path) -> io::Result<()> {
    if target.join(MARKER).try_exists()? || !source.try_exists()? {
        return Ok(());
    }
    private_dir(target)?;
    // Hold the daemon's own lock throughout the copy. Discovery alone can fail
    // while the daemon is starting/stopping and cannot prove files are idle.
    let lock = crate::daemon::lock_file(&target.join("daemon.lock"))?;
    lock.try_lock().map_err(|error| io::Error::other(format!(
        "Stop the proxy in the previous Coport app, then reopen this version to migrate its data. No source files will be deleted. Cannot acquire the daemon lock: {error}"
    )))?;
    migrate(source, target)
}

fn migrate(source: &Path, target: &Path) -> io::Result<()> {
    if target.join(MARKER).try_exists()? || !source.try_exists()? {
        return Ok(());
    }
    // Check every conflict before publishing anything. A retry after interruption
    // accepts identical files, but never mixes two different configurations/CAs.
    let mut files = Vec::new();
    for name in ITEMS {
        collect(&source.join(name), &target.join(name), &mut files)?;
    }
    for (from, to) in &files {
        if to.try_exists()? && fs::read(from)? != fs::read(to)? {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "Data already exists at {} and differs from the cache; no files were overwritten",
                    to.display()
                ),
            ));
        }
    }
    for (from, to) in files {
        if to.try_exists()? {
            continue;
        }
        private_dir(to.parent().unwrap())?;
        let mut temp = tempfile::NamedTempFile::new_in(to.parent().unwrap())?;
        io::copy(&mut fs::File::open(from)?, &mut temp)?;
        temp.as_file().sync_all()?;
        temp.persist_noclobber(to).map_err(|e| e.error)?;
    }
    // Keep the source as a recovery copy, but do not reimport stale data on the
    // next launch after the destination has legitimately changed.
    crate::settings::write_private(&target.join(MARKER), b"1\n")
}

fn collect(
    source: &Path,
    target: &Path,
    files: &mut Vec<(std::path::PathBuf, std::path::PathBuf)>,
) -> io::Result<()> {
    let meta = match fs::symlink_metadata(source) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if meta.file_type().is_symlink() {
        return Err(io::Error::other(format!(
            "Cannot migrate a symbolic link: {}",
            source.display()
        )));
    }
    match fs::symlink_metadata(target) {
        Ok(existing) if existing.file_type().is_symlink() || existing.is_dir() != meta.is_dir() => {
            return Err(io::Error::other(format!(
                "Conflicting destination: {}",
                target.display()
            )));
        }
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    if meta.is_dir() {
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            collect(&entry.path(), &target.join(entry.file_name()), files)?;
        }
    } else if meta.is_file() {
        files.push((source.to_owned(), target.to_owned()));
    } else {
        return Err(io::Error::other(format!(
            "Unsupported file: {}",
            source.display()
        )));
    }
    Ok(())
}

fn private_dir(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_config_ca_history_and_existing_preferences_and_is_one_time() {
        for newline in ["\n", "\r\n"] {
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("cache");
            let target = dir.path().join("support");
            fs::create_dir_all(source.join("tls")).unwrap();
            fs::create_dir_all(source.join("logs/history")).unwrap();
            fs::create_dir_all(&target).unwrap();
            let config = format!(
                "# keep formatting{newline}listen_port: 8787{newline}request_timeout_seconds: 30{newline}"
            );
            fs::write(source.join("config.yaml"), &config).unwrap();
            fs::write(source.join("tls/ca-key.pem"), b"original key").unwrap();
            fs::write(source.join("tls/ca.pem"), b"original certificate").unwrap();
            fs::write(source.join("logs/proxy.log"), b"current").unwrap();
            fs::write(source.join("logs/proxy.log.1"), b"rotated").unwrap();
            fs::write(source.join("logs/history/proxy.log.1.jsonl"), b"archive").unwrap();
            fs::write(target.join("gui.json"), b"preferences").unwrap();
            migrate(&source, &target).unwrap();
            for name in [
                "config.yaml",
                "tls/ca-key.pem",
                "tls/ca.pem",
                "logs/proxy.log",
                "logs/proxy.log.1",
                "logs/history/proxy.log.1.jsonl",
            ] {
                assert_eq!(
                    fs::read(source.join(name)).unwrap(),
                    fs::read(target.join(name)).unwrap()
                );
            }
            assert_eq!(fs::read(target.join("gui.json")).unwrap(), b"preferences");
            assert!(coport::config::Config::read(&target.join("config.yaml")).is_ok());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    fs::metadata(target.join("tls/ca-key.pem"))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o600
                );
            }
            fs::write(target.join("config.yaml"), b"new configuration").unwrap();
            migrate(&source, &target).unwrap();
            assert_eq!(
                fs::read(target.join("config.yaml")).unwrap(),
                b"new configuration"
            );
        }
    }

    #[test]
    fn conflict_does_not_publish_partial_data_and_retry_accepts_identical_files() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("cache");
        let target = dir.path().join("support");
        fs::create_dir_all(source.join("tls")).unwrap();
        fs::create_dir_all(target.join("tls")).unwrap();
        fs::write(source.join("config.yaml"), b"original config").unwrap();
        fs::write(source.join("tls/ca-key.pem"), b"original key").unwrap();
        fs::write(target.join("tls/ca-key.pem"), b"different key").unwrap();
        assert!(migrate(&source, &target).is_err());
        assert!(!target.join("config.yaml").exists());
        assert!(!target.join(MARKER).exists());
        fs::write(target.join("tls/ca-key.pem"), b"original key").unwrap();
        migrate(&source, &target).unwrap();
        assert!(target.join(MARKER).exists());
    }

    #[test]
    fn live_daemon_lock_prevents_migration_even_without_discovery() {
        let _guard = crate::daemon::spawn_guard();
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("cache");
        let target = dir.path().join("support");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(source.join("config.yaml"), b"original config").unwrap();
        let lock = crate::daemon::lock_file(&target.join("daemon.lock")).unwrap();
        lock.try_lock().unwrap();
        assert!(prepare_paths(&source, &target).is_err());
        assert!(!target.join("config.yaml").exists());
        assert!(!target.join(MARKER).exists());
        drop(lock);
        prepare_paths(&source, &target).unwrap();
        assert_eq!(
            fs::read(target.join("config.yaml")).unwrap(),
            b"original config"
        );
    }

    #[test]
    fn missing_legacy_directory_is_a_fresh_install() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("support");
        migrate(&dir.path().join("absent"), &target).unwrap();
        assert!(!target.exists());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_files_without_touching_their_targets() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("cache");
        let target = dir.path().join("support");
        fs::create_dir_all(&source).unwrap();
        let outside = dir.path().join("outside");
        fs::write(&outside, b"keep").unwrap();
        std::os::unix::fs::symlink(&outside, source.join("config.yaml")).unwrap();
        assert!(migrate(&source, &target).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"keep");
        assert!(!target.exists());
    }
}
