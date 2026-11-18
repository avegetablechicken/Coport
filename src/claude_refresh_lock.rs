//! Claude Code's cross-process OAuth refresh lock.
//!
//! Claude Code renews its login only while holding two `proper-lockfile`
//! locks: `<config dir>/.oauth_refresh.lock` and the legacy
//! `<realpath(config dir)>.lock`. Inside the lock it rereads the saved login
//! and skips its own renewal when another process already rotated it. Every
//! other program that renews the same saved login must take the same locks,
//! or both may spend one single-use refresh token.
//!
//! The protocol: a lock is a directory created atomically; its holder bumps
//! the directory's mtime while it holds it; a lock whose mtime is older than
//! [`STALE`] belongs to a dead holder and may be removed. Locks are taken in
//! Claude Code's order (current, then legacy), so two holders never wait on
//! each other.
//!
//! Never hold this lock while waiting for a request that Claude Code itself
//! may be sending while it holds the lock (for example, a token refresh it
//! relays through this process): that waits for itself until the lock goes
//! stale.
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

/// The age after which Claude Code treats a lock as abandoned.
pub(crate) const STALE: Duration = Duration::from_secs(60);
/// Claude Code bumps its lock every 5 s and takes over one that has not moved
/// in 7.5 s; bump more often so a busy holder is never taken for dead.
const UPDATE: Duration = Duration::from_secs(2);

/// The two lock directories Claude Code takes for a configuration directory.
pub(crate) fn lock_paths(config_dir: &Path) -> [PathBuf; 2] {
    // `path.join` ignores a trailing separator; `realpath` does as well.
    let config_dir: PathBuf = config_dir.components().collect();
    let real = fs::canonicalize(&config_dir).unwrap_or_else(|_| config_dir.clone());
    let mut legacy = real.into_os_string();
    legacy.push(".lock");
    [
        config_dir.join(".oauth_refresh.lock"),
        PathBuf::from(legacy),
    ]
}

/// How long to wait for a lock another process holds.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Wait {
    pub total: Duration,
    /// Each retry waits a random time in `[backoff, 2 * backoff)`, like Claude Code.
    pub backoff: Duration,
}
impl Wait {
    pub(crate) fn up_to(total: Duration) -> Self {
        Self {
            total,
            backoff: Duration::from_secs(1),
        }
    }
}

/// Held until dropped; dropping releases both locks.
pub(crate) struct RefreshLock {
    held: Vec<(PathBuf, SystemTime)>,
    compromised: Arc<AtomicBool>,
    stop: Option<mpsc::Sender<()>>,
    keeper: Option<thread::JoinHandle<Vec<(PathBuf, SystemTime)>>>,
}

impl RefreshLock {
    /// Takes both locks for `config_dir`, waiting up to `wait` while another
    /// process holds them. `Ok(None)`: still held elsewhere when the wait ran out.
    pub(crate) fn acquire(config_dir: &Path, wait: Wait) -> io::Result<Option<Self>> {
        let started = Instant::now();
        let [current, legacy] = lock_paths(config_dir);
        let mut attempt = 0u32;
        loop {
            if let Some(first) = try_lock(&current)? {
                match try_lock(&legacy) {
                    Ok(Some(second)) => {
                        return Ok(Some(Self::hold(vec![(current, first), (legacy, second)])));
                    }
                    Ok(None) => release(&current, first),
                    Err(error) => {
                        release(&current, first);
                        return Err(error);
                    }
                }
            }
            let left = wait.total.saturating_sub(started.elapsed());
            if left.is_zero() {
                return Ok(None);
            }
            attempt = attempt.wrapping_add(1);
            thread::sleep(jitter(wait.backoff, attempt).min(left));
        }
    }

    fn hold(held: Vec<(PathBuf, SystemTime)>) -> Self {
        let compromised = Arc::new(AtomicBool::new(false));
        let (stop, stopped) = mpsc::channel();
        let flag = compromised.clone();
        let mut kept = held.clone();
        let keeper = thread::Builder::new()
            .name("claude-refresh-lock".into())
            .spawn(move || {
                while let Err(mpsc::RecvTimeoutError::Timeout) = stopped.recv_timeout(UPDATE) {
                    for (path, ours) in &mut kept {
                        // A lock whose mtime is not ours was taken over.
                        match bump(path, *ours) {
                            Some(mtime) => *ours = mtime,
                            None => flag.store(true, Ordering::SeqCst),
                        }
                    }
                }
                kept
            });
        match keeper {
            Ok(keeper) => Self {
                held,
                compromised,
                stop: Some(stop),
                keeper: Some(keeper),
            },
            // Without a keeper the lock goes stale after `STALE`; report it
            // compromised so no renewal starts under it.
            Err(_) => {
                compromised.store(true, Ordering::SeqCst);
                Self {
                    held,
                    compromised,
                    stop: None,
                    keeper: None,
                }
            }
        }
    }

    /// Whether another process took a lock over, as a dead holder's.
    pub(crate) fn compromised(&self) -> bool {
        self.compromised.load(Ordering::SeqCst)
    }
}

impl Drop for RefreshLock {
    fn drop(&mut self) {
        drop(self.stop.take());
        let held = match self.keeper.take().map(thread::JoinHandle::join) {
            Some(Ok(kept)) => kept,
            _ => std::mem::take(&mut self.held),
        };
        // Release in reverse order, and only locks that are still ours.
        for (path, ours) in held.into_iter().rev() {
            release(&path, ours);
        }
    }
}

/// One attempt; `Ok(None)` while a live holder has it.
fn try_lock(path: &Path) -> io::Result<Option<SystemTime>> {
    for _ in 0..2 {
        match fs::create_dir(path) {
            Ok(()) => {
                let now = SystemTime::now();
                return match touch(path, now).and_then(|()| modified(path)) {
                    Ok(mtime) => Ok(Some(mtime)),
                    Err(error) => {
                        let _ = fs::remove_dir(path);
                        Err(error)
                    }
                };
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let mtime = match modified(path) {
            Ok(mtime) => mtime,
            // Released meanwhile: try once more.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if !is_stale(mtime) {
            return Ok(None);
        }
        // A dead holder's lock. Recheck right before removing, so a lock that
        // was just taken over again is left alone.
        if modified(path).ok() != Some(mtime) {
            return Ok(None);
        }
        match fs::remove_dir(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

fn is_stale(mtime: SystemTime) -> bool {
    SystemTime::now()
        .checked_sub(STALE)
        .is_some_and(|limit| mtime < limit)
}

/// Bumps a lock that is still ours, returning its new mtime.
fn bump(path: &Path, ours: SystemTime) -> Option<SystemTime> {
    if modified(path).ok()? != ours {
        return None;
    }
    touch(path, SystemTime::now()).ok()?;
    modified(path).ok()
}

fn release(path: &Path, ours: SystemTime) {
    if modified(path).ok() == Some(ours) {
        let _ = fs::remove_dir(path);
    }
}

fn modified(path: &Path) -> io::Result<SystemTime> {
    fs::metadata(path)?.modified()
}

/// Sets a directory's mtime; directories need a handle of their own on Windows.
pub(crate) fn touch(path: &Path, time: SystemTime) -> io::Result<()> {
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        fs::OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?
    };
    #[cfg(not(windows))]
    let file = fs::File::open(path)?;
    file.set_modified(time)
}

fn jitter(backoff: Duration, attempt: u32) -> Duration {
    let seed = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos())
        ^ std::process::id().rotate_left(7)
        ^ attempt.wrapping_mul(0x9E37_79B9);
    backoff + backoff.mul_f64(f64::from(seed % 1000) / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quick(total_ms: u64) -> Wait {
        Wait {
            total: Duration::from_millis(total_ms),
            backoff: Duration::from_millis(10),
        }
    }

    fn age(path: &Path, by: Duration) {
        touch(path, SystemTime::now() - by).unwrap();
    }

    #[test]
    fn takes_claude_codes_current_and_legacy_locks_and_releases_them() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".claude");
        fs::create_dir(&dir).unwrap();
        let [current, legacy] = lock_paths(&dir);
        assert_eq!(current, dir.join(".oauth_refresh.lock"));
        let real = fs::canonicalize(&dir).unwrap();
        assert_eq!(legacy, real.with_file_name(".claude.lock"));
        // A trailing separator names the same locks.
        let mut slashed = dir.clone().into_os_string();
        slashed.push(std::path::MAIN_SEPARATOR_STR);
        assert_eq!(
            lock_paths(Path::new(&slashed)),
            [current.clone(), legacy.clone()]
        );

        let lock = RefreshLock::acquire(&dir, quick(0)).unwrap().unwrap();
        assert!(current.is_dir() && legacy.is_dir());
        assert!(!lock.compromised());
        // Held: a second taker, in this or another process, waits.
        assert!(RefreshLock::acquire(&dir, quick(50)).unwrap().is_none());
        drop(lock);
        assert!(!current.exists() && !legacy.exists());
        assert!(RefreshLock::acquire(&dir, quick(0)).unwrap().is_some());
    }

    #[test]
    fn a_held_legacy_lock_releases_the_current_one_and_waits() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("config");
        fs::create_dir(&dir).unwrap();
        let [current, legacy] = lock_paths(&dir);
        // An older Claude Code holds only the legacy lock.
        fs::create_dir(&legacy).unwrap();
        assert!(RefreshLock::acquire(&dir, quick(50)).unwrap().is_none());
        assert!(!current.exists());
        assert!(legacy.is_dir());
        fs::remove_dir(&legacy).unwrap();
        assert!(RefreshLock::acquire(&dir, quick(0)).unwrap().is_some());
    }

    #[test]
    fn a_dead_holders_stale_lock_is_taken_over_but_a_live_one_is_not() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("config");
        fs::create_dir(&dir).unwrap();
        let [current, legacy] = lock_paths(&dir);
        fs::create_dir(&current).unwrap();
        fs::create_dir(&legacy).unwrap();
        age(&current, STALE / 2);
        assert!(RefreshLock::acquire(&dir, quick(30)).unwrap().is_none());
        age(&current, STALE + Duration::from_secs(5));
        age(&legacy, STALE + Duration::from_secs(5));
        let lock = RefreshLock::acquire(&dir, quick(0)).unwrap().unwrap();
        // The taken-over locks are fresh, so others now wait for us.
        assert!(!is_stale(modified(&current).unwrap()));
        assert!(!is_stale(modified(&legacy).unwrap()));
        drop(lock);
        assert!(!current.exists() && !legacy.exists());
    }

    #[test]
    fn a_lock_taken_over_is_reported_and_left_to_its_new_holder() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("config");
        fs::create_dir(&dir).unwrap();
        let [current, _] = lock_paths(&dir);
        let lock = RefreshLock::acquire(&dir, quick(0)).unwrap().unwrap();
        // Another process decided we were dead and took the lock.
        let theirs = SystemTime::now() + Duration::from_secs(3);
        touch(&current, theirs).unwrap();
        let deadline = Instant::now() + UPDATE * 3;
        while !lock.compromised() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(lock.compromised());
        drop(lock);
        assert!(current.is_dir(), "the new holder's lock must survive");
        assert_eq!(
            modified(&current).unwrap(),
            modified(&current).unwrap().max(theirs)
        );
    }

    #[test]
    fn a_held_lock_stays_fresh_while_a_renewal_runs() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("config");
        fs::create_dir(&dir).unwrap();
        let [current, legacy] = lock_paths(&dir);
        let lock = RefreshLock::acquire(&dir, quick(0)).unwrap().unwrap();
        let before = modified(&current).unwrap();
        // Pretend the renewal has run for a long time: the keeper must bump
        // the locks before anyone may treat them as stale.
        thread::sleep(UPDATE + Duration::from_millis(500));
        assert!(modified(&current).unwrap() > before);
        assert!(modified(&legacy).unwrap() > before);
        assert!(!lock.compromised());
    }

    #[test]
    fn concurrent_takers_never_hold_the_lock_together() {
        let home = tempfile::tempdir().unwrap();
        let dir = Arc::new(home.path().join("config"));
        fs::create_dir(&*dir).unwrap();
        let holders = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let overlap = Arc::new(AtomicBool::new(false));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (dir, holders, overlap) = (dir.clone(), holders.clone(), overlap.clone());
                thread::spawn(move || {
                    for _ in 0..5 {
                        let lock = RefreshLock::acquire(&dir, quick(5_000)).unwrap().unwrap();
                        if holders.fetch_add(1, Ordering::SeqCst) != 0 {
                            overlap.store(true, Ordering::SeqCst);
                        }
                        thread::sleep(Duration::from_millis(2));
                        holders.fetch_sub(1, Ordering::SeqCst);
                        drop(lock);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert!(!overlap.load(Ordering::SeqCst));
    }

    #[test]
    fn a_missing_configuration_directory_is_an_error_not_a_lock() {
        let home = tempfile::tempdir().unwrap();
        assert!(RefreshLock::acquire(&home.path().join("missing"), quick(0)).is_err());
    }
}
