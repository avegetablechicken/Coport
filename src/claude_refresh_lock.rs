//! Claude Code's cross-process OAuth refresh lock.
//!
//! Claude Code renews its login only while holding two `proper-lockfile`
//! locks: `<config dir>/.oauth_refresh.lock` and the legacy
//! `<realpath(config dir)>.lock`. Inside the lock it rereads the saved login
//! and skips its own renewal when another process already rotated it. Every
//! other program that renews the same saved login must take the same locks,
//! or both may spend one single-use refresh token.
//!
//! A lock is a directory created atomically, taken in Claude Code's order
//! (current, then legacy) so two holders never wait on each other. Its
//! holder bumps the directory's mtime while it holds it. Claude Code
//! (2.1.326) frees a lock whose holder is gone in two ways, both followed
//! here:
//!
//! - **Stale**: a lock whose mtime is older than [`STALE`] (60 s, the
//!   `proper-lockfile` `stale` option) is removed by whoever finds it.
//! - **Dead holder**: right after taking the locks, the holder writes an
//!   owner record, [`OWNER_FILE`], naming its PID, the PID's start time and
//!   namespace, and both locks' birth times. A waiter that has seen the
//!   current lock's mtime stand still for [`WATCH`] (7.5 s) removes both
//!   locks only if that record proves the holder gone: same host and PID
//!   namespace, same locks, and the PID no longer running (or running as
//!   another process). A live holder is never taken over this way, whatever
//!   its mtime; a lock without a record waits for [`STALE`]. Proving a PID
//!   gone is implemented on Unix; on Windows only the stale rule applies,
//!   in both directions.
//!
//! Never hold this lock while waiting for a request that Claude Code itself
//! may be sending while it holds the lock (for example, a token refresh it
//! relays through this process): that waits for itself until the lock goes
//! stale.
use serde_json::Value;
#[cfg(unix)]
use serde_json::json;
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

/// The age after which a lock is stale and anyone may remove it
/// (`proper-lockfile`'s `stale`, which Claude Code sets to 60 s).
pub(crate) const STALE: Duration = Duration::from_secs(60);
/// How long the current lock's mtime must stand still before its owner
/// record is checked for a dead holder (Claude Code's `ZR`, 7.5 s).
pub(crate) const WATCH: Duration = Duration::from_millis(7_500);
/// The holder's owner record, beside the current lock.
pub(crate) const OWNER_FILE: &str = ".oauth_refresh.lock.owner";
/// How often a holder bumps its locks. Claude Code bumps every 5 s
/// (`update`); a moving mtime marks the holder alive, and only a still one
/// starts the dead-holder check, so bump well within [`WATCH`].
const UPDATE: Duration = Duration::from_secs(2);
/// Claude Code reads at most this much of an owner record.
const OWNER_LIMIT: u64 = 4096;

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
    /// How long a still mtime is watched before checking for a dead holder.
    pub watch: Duration,
}
impl Wait {
    pub(crate) fn up_to(total: Duration) -> Self {
        Self {
            total,
            backoff: Duration::from_secs(1),
            // Tests watch briefly, so a dead holder is taken over in time.
            watch: if cfg!(test) {
                Duration::from_millis(200)
            } else {
                WATCH
            },
        }
    }
}

/// Held until dropped; dropping releases both locks.
pub(crate) struct RefreshLock {
    dir: PathBuf,
    held: Vec<(PathBuf, SystemTime)>,
    /// The current lock's birth time, as recorded in our owner record.
    owner: Option<f64>,
    compromised: Arc<AtomicBool>,
    stop: Option<mpsc::Sender<()>>,
    keeper: Option<thread::JoinHandle<Vec<(PathBuf, SystemTime)>>>,
}

impl RefreshLock {
    /// Takes both locks for `config_dir`, waiting up to `wait` while another
    /// process holds them. `Ok(None)`: still held elsewhere when the wait ran out.
    pub(crate) fn acquire(config_dir: &Path, wait: Wait) -> io::Result<Option<Self>> {
        let started = Instant::now();
        let dir: PathBuf = config_dir.components().collect();
        let [current, legacy] = lock_paths(&dir);
        // The current lock's mtime as first seen standing still, and when.
        let mut watched: Option<(SystemTime, Instant)> = None;
        let mut attempt = 0u32;
        loop {
            if let Some(first) = try_lock(&current)? {
                match try_lock(&legacy) {
                    Ok(Some(second)) => {
                        let owner = write_owner(&dir, &current, &legacy);
                        let held = vec![(current, first), (legacy, second)];
                        return Ok(Some(Self::hold(dir, held, owner)));
                    }
                    Ok(None) => release(&current, first),
                    Err(error) => {
                        release(&current, first);
                        return Err(error);
                    }
                }
            }
            // Only a contended current lock is watched, as Claude Code does;
            // a legacy lock alone (an older Claude Code) waits to go stale.
            match modified(&current) {
                Ok(mtime) => match watched {
                    Some((seen, since)) if seen == mtime => {
                        if since.elapsed() >= wait.watch && take_over(&dir, mtime) {
                            watched = None;
                            continue;
                        }
                    }
                    _ => watched = Some((mtime, Instant::now())),
                },
                Err(_) => watched = None,
            }
            let left = wait.total.saturating_sub(started.elapsed());
            if left.is_zero() {
                return Ok(None);
            }
            attempt = attempt.wrapping_add(1);
            thread::sleep(jitter(wait.backoff, attempt).min(left));
        }
    }

    fn hold(dir: PathBuf, held: Vec<(PathBuf, SystemTime)>, owner: Option<f64>) -> Self {
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
                dir,
                held,
                owner,
                compromised,
                stop: Some(stop),
                keeper: Some(keeper),
            },
            // Without a keeper the lock goes stale after `STALE`; report it
            // compromised so no renewal starts under it.
            Err(_) => {
                compromised.store(true, Ordering::SeqCst);
                Self {
                    dir,
                    held,
                    owner,
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
        // As Claude Code releases: our owner record first, then the locks in
        // reverse order, and only what is still ours.
        if let Some(birth) = self.owner {
            let path = self.dir.join(OWNER_FILE);
            if read_owner(&path).is_some_and(|record| {
                record.pid == i64::from(std::process::id()) && same_birth(record.lock_birth, birth)
            }) {
                let _ = fs::remove_file(path);
            }
        }
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
                // mkdir set the mtime. Setting it again here could set it a
                // hair before the birth time, which macOS then lowers to
                // match, so the owner record would no longer name this lock.
                return match modified(path) {
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

/// A directory's birth time as Node and Bun report `birthtimeMs`:
/// seconds × 1000 + nanoseconds ÷ 10⁶. `None` where the file system keeps none.
#[cfg(unix)]
fn birth_ms(path: &Path) -> Option<f64> {
    let birth = fs::metadata(path).ok()?.created().ok()?;
    let since = birth.duration_since(SystemTime::UNIX_EPOCH).ok()?;
    let ms = since.as_secs() as f64 * 1000.0 + f64::from(since.subsec_nanos()) / 1_000_000.0;
    (ms > 0.0).then_some(ms)
}

/// Whether two birth times name one lock. Exact in JavaScript, but
/// serde_json may read a recorded time back one unit in the last place off;
/// a lock is recreated far more than a millisecond apart.
fn same_birth(a: f64, b: f64) -> bool {
    (a - b).abs() < 1.0
}

/// An owner record, as Claude Code validates it.
#[derive(Debug, PartialEq)]
struct Owner {
    pid: i64,
    proc_start: Option<String>,
    pid_domain: String,
    pid_space: String,
    lock_birth: f64,
    legacy_birth: Option<f64>,
}

fn read_owner(path: &Path) -> Option<Owner> {
    use std::io::Read;
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(OWNER_LIMIT + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > OWNER_LIMIT {
        return None;
    }
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    let text = |key: &str, limit: usize| -> Option<Option<String>> {
        match &value[key] {
            Value::Null => Some(None),
            Value::String(text) if text.chars().count() <= limit => Some(Some(text.clone())),
            _ => None,
        }
    };
    let positive = |key: &str| -> Option<Option<f64>> {
        match &value[key] {
            Value::Null => Some(None),
            number => number.as_f64().filter(|ms| *ms > 0.0).map(Some),
        }
    };
    Some(Owner {
        pid: value["pid"].as_i64()?,
        proc_start: text("procStart", 64)?,
        pid_domain: text("pidDomain", 512)??,
        pid_space: text("pidSpace", 512)??,
        lock_birth: positive("lockBirthtimeMs")??,
        legacy_birth: positive("legacyLockBirthtimeMs")?,
    })
}

/// Records this process as the holder, so it can be taken over without
/// waiting for `STALE` should it die holding the locks. Returns the current
/// lock's recorded birth time.
#[cfg(unix)]
fn write_owner(dir: &Path, current: &Path, legacy: &Path) -> Option<f64> {
    use std::io::Write;
    let lock_birth = birth_ms(current)?;
    let pid = std::process::id();
    let mut record = json!({
        "pid": pid,
        "pidDomain": unix::pid_domain(),
        "pidSpace": unix::hostname()?,
        "lockBirthtimeMs": lock_birth,
    });
    if let Some(start) = unix::own_start() {
        record["procStart"] = start.into();
    }
    if let Some(birth) = birth_ms(legacy) {
        record["legacyLockBirthtimeMs"] = birth.into();
    }
    let mut temp = tempfile::NamedTempFile::new_in(dir).ok()?;
    temp.write_all(record.to_string().as_bytes()).ok()?;
    temp.as_file().sync_all().ok()?;
    temp.persist(dir.join(OWNER_FILE)).ok()?;
    Some(lock_birth)
}
#[cfg(not(unix))]
fn write_owner(_: &Path, _: &Path, _: &Path) -> Option<f64> {
    None
}

/// Claude Code's dead-holder takeover: both locks are removed only when the
/// owner record names the locks as they stand and proves their holder gone.
#[cfg(unix)]
fn take_over(dir: &Path, watched: SystemTime) -> bool {
    let [current, legacy] = lock_paths(dir);
    let Some(record) = read_owner(&dir.join(OWNER_FILE)) else {
        return false;
    };
    // Claude Code compares birth times exactly; within a millisecond is as
    // good here, as the still mtime and the dead PID name the holder too.
    let same =
        |recorded: f64, path: &Path| birth_ms(path).is_some_and(|ms| same_birth(ms, recorded));
    let unchanged =
        || modified(&current).ok() == Some(watched) && same(record.lock_birth, &current);
    if !unchanged()
        || record.pid_domain != unix::pid_domain()
        || Some(record.pid_space.as_str()) != unix::hostname().as_deref()
        || !unix::pid_view_trusted()
        || !unix::gone(record.pid, record.proc_start.as_deref())
    {
        return false;
    }
    match fs::metadata(&legacy) {
        Ok(_)
            if !record
                .legacy_birth
                .is_some_and(|birth| same(birth, &legacy)) =>
        {
            return false;
        }
        Ok(_) => {
            if fs::remove_dir(&legacy).is_err() {
                return false;
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return false,
    }
    if !unchanged() || fs::remove_dir(&current).is_err() {
        return false;
    }
    if read_owner(&dir.join(OWNER_FILE)).as_ref() == Some(&record) {
        let _ = fs::remove_file(dir.join(OWNER_FILE));
    }
    true
}
#[cfg(not(unix))]
fn take_over(_: &Path, _: SystemTime) -> bool {
    false
}

#[cfg(unix)]
mod unix {
    use std::{
        process::{Command, Stdio},
        sync::OnceLock,
        thread,
        time::{Duration, Instant},
    };

    /// The host part of Claude Code's PID namespace (`os.hostname()`).
    pub(super) fn hostname() -> Option<String> {
        let mut buffer = [0u8; 256];
        // SAFETY: the buffer is valid for its length; gethostname writes at
        // most that many bytes.
        if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } != 0 {
            return None;
        }
        let end = buffer.iter().position(|byte| *byte == 0)?;
        let name = String::from_utf8(buffer[..end].to_vec()).ok()?;
        (!name.is_empty() && name.chars().count() <= 512).then_some(name)
    }

    /// Claude Code's PID domain: one per host on macOS, and per machine ID
    /// and PID namespace on Linux.
    pub(super) fn pid_domain() -> String {
        static DOMAIN: OnceLock<String> = OnceLock::new();
        DOMAIN
            .get_or_init(|| {
                if cfg!(target_os = "linux") {
                    let machine = std::fs::read_to_string("/etc/machine-id")
                        .map(|id| id.trim().to_owned())
                        .unwrap_or_default();
                    let namespace = std::fs::read_link("/proc/self/ns/pid")
                        .map(|link| link.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    format!("darwin:{machine}:{namespace}")
                } else {
                    "darwin".to_owned()
                }
            })
            .clone()
    }

    /// Whether PIDs seen here are the host's: on Linux, `/proc` must show
    /// this process and enough others, as Claude Code requires.
    pub(super) fn pid_view_trusted() -> bool {
        if !cfg!(target_os = "linux") {
            return true;
        }
        let own = std::fs::read_link("/proc/self")
            .is_ok_and(|link| link.to_string_lossy() == std::process::id().to_string());
        own && std::fs::read_dir("/proc").is_ok_and(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_digit())
                })
                .take(16)
                .count()
                >= 16
        })
    }

    /// This process's start time, read once, as Claude Code reads its own.
    pub(super) fn own_start() -> Option<String> {
        static START: OnceLock<Option<String>> = OnceLock::new();
        START.get_or_init(|| proc_start(std::process::id())).clone()
    }

    /// A PID's start time as Claude Code records it (`ps -o lstart=` in UTC).
    pub(super) fn proc_start(pid: u32) -> Option<String> {
        let mut child = Command::new("ps")
            .args(["-o", "lstart=", "-p", &pid.to_string()])
            .env("LC_ALL", "C")
            .env("TZ", "UTC")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        }
        let output = child.wait_with_output().ok()?;
        let start = String::from_utf8(output.stdout).ok()?.trim().to_owned();
        (output.status.success() && !start.is_empty()).then_some(start)
    }

    /// Whether the recorded holder is proven gone: no such process, a
    /// zombie, or another process now using its PID.
    pub(super) fn gone(pid: i64, recorded_start: Option<&str>) -> bool {
        let Ok(pid) = i32::try_from(pid) else {
            return false;
        };
        if pid <= 1 {
            return false;
        }
        // SAFETY: signal 0 only checks that the process exists.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        }
        if cfg!(target_os = "linux") {
            let zombie = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| {
                    let state = stat.rsplit_once(") ")?.1.chars().next()?;
                    Some(state == 'Z' || state == 'X')
                });
            if zombie == Some(true) {
                return true;
            }
        }
        recorded_start
            .is_some_and(|recorded| proc_start(pid as u32).is_some_and(|now| now != recorded))
    }
}

/// Leaves both locks as a Claude Code that died holding them would: with
/// its owner record, naming a PID that has exited.
#[cfg(all(test, unix))]
pub(crate) fn hold_as_dead_claude_code(dir: &Path) -> [PathBuf; 2] {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    let [current, legacy] = lock_paths(dir);
    fs::create_dir(&current).unwrap();
    fs::create_dir(&legacy).unwrap();
    let record = json!({
        "pid": pid,
        "pidDomain": unix::pid_domain(),
        "pidSpace": unix::hostname().unwrap(),
        "lockBirthtimeMs": birth_ms(&current).unwrap(),
        "legacyLockBirthtimeMs": birth_ms(&legacy).unwrap(),
    });
    fs::write(dir.join(OWNER_FILE), record.to_string()).unwrap();
    [current, legacy]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quick(total_ms: u64) -> Wait {
        Wait {
            total: Duration::from_millis(total_ms),
            backoff: Duration::from_millis(10),
            watch: Duration::from_millis(100),
        }
    }

    fn age(path: &Path, by: Duration) {
        touch(path, SystemTime::now() - by).unwrap();
    }

    fn config_dir(home: &tempfile::TempDir) -> PathBuf {
        let dir = home.path().join("config");
        fs::create_dir(&dir).unwrap();
        dir
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
        assert!(!dir.join(OWNER_FILE).exists());
        assert!(RefreshLock::acquire(&dir, quick(0)).unwrap().is_some());
    }

    #[test]
    fn a_held_legacy_lock_releases_the_current_one_and_waits() {
        let home = tempfile::tempdir().unwrap();
        let dir = config_dir(&home);
        let [current, legacy] = lock_paths(&dir);
        // An older Claude Code holds only the legacy lock.
        fs::create_dir(&legacy).unwrap();
        assert!(RefreshLock::acquire(&dir, quick(300)).unwrap().is_none());
        assert!(!current.exists());
        assert!(legacy.is_dir());
        fs::remove_dir(&legacy).unwrap();
        assert!(RefreshLock::acquire(&dir, quick(0)).unwrap().is_some());
    }

    #[test]
    fn a_stale_lock_is_taken_over_but_a_fresh_one_without_a_record_is_not() {
        let home = tempfile::tempdir().unwrap();
        let dir = config_dir(&home);
        let [current, legacy] = lock_paths(&dir);
        fs::create_dir(&current).unwrap();
        fs::create_dir(&legacy).unwrap();
        // Still for longer than the watch, but no owner record proves the
        // holder gone: it waits for the lock to go stale.
        age(&current, STALE / 2);
        assert!(RefreshLock::acquire(&dir, quick(400)).unwrap().is_none());
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
        let dir = config_dir(&home);
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
    }

    #[test]
    fn a_held_lock_stays_fresh_while_a_renewal_runs() {
        let home = tempfile::tempdir().unwrap();
        let dir = config_dir(&home);
        let [current, legacy] = lock_paths(&dir);
        let lock = RefreshLock::acquire(&dir, quick(0)).unwrap().unwrap();
        let before = modified(&current).unwrap();
        // The keeper bumps the locks: a moving mtime marks the holder alive.
        thread::sleep(UPDATE + Duration::from_millis(500));
        assert!(modified(&current).unwrap() > before);
        assert!(modified(&legacy).unwrap() > before);
        assert!(!lock.compromised());
    }

    #[test]
    fn concurrent_takers_never_hold_the_lock_together() {
        let home = tempfile::tempdir().unwrap();
        let dir = Arc::new(config_dir(&home));
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

    #[cfg(unix)]
    mod owner {
        use super::*;

        /// A change to the record a holder left.
        type Edit = Box<dyn FnOnce(&mut Value)>;

        /// A PID that ran and has exited.
        fn dead_pid() -> u32 {
            let mut child = std::process::Command::new("true").spawn().unwrap();
            let pid = child.id();
            child.wait().unwrap();
            pid
        }

        /// The locks as a holder `pid` left them, with the record it wrote.
        fn held_by(dir: &Path, pid: u32, edit: impl FnOnce(&mut Value)) -> [PathBuf; 2] {
            let [current, legacy] = lock_paths(dir);
            fs::create_dir(&current).unwrap();
            fs::create_dir(&legacy).unwrap();
            let mut record = json!({
                "pid": pid,
                "procStart": "Thu Jan  1 00:00:00 2026",
                "pidDomain": unix::pid_domain(),
                "pidSpace": unix::hostname().unwrap(),
                "lockBirthtimeMs": birth_ms(&current).unwrap(),
                "legacyLockBirthtimeMs": birth_ms(&legacy).unwrap(),
            });
            edit(&mut record);
            fs::write(dir.join(OWNER_FILE), record.to_string()).unwrap();
            [current, legacy]
        }

        #[test]
        fn the_holder_records_itself_as_claude_code_does_and_removes_the_record() {
            let home = tempfile::tempdir().unwrap();
            let dir = config_dir(&home);
            let [current, legacy] = lock_paths(&dir);
            let lock = RefreshLock::acquire(&dir, quick(0)).unwrap().unwrap();
            let raw: Value =
                serde_json::from_slice(&fs::read(dir.join(OWNER_FILE)).unwrap()).unwrap();
            assert_eq!(raw["pid"], std::process::id());
            assert_eq!(raw["pidSpace"], unix::hostname().unwrap());
            if cfg!(target_os = "macos") {
                assert_eq!(raw["pidDomain"], "darwin");
            } else {
                assert!(raw["pidDomain"].as_str().unwrap().starts_with("darwin:"));
            }
            // `ps -o lstart=` in UTC, as Claude Code compares it.
            assert_eq!(
                raw["procStart"].as_str(),
                unix::proc_start(std::process::id()).as_deref()
            );
            // Recorded exactly: JavaScript reads back the very stat value.
            let text = fs::read_to_string(dir.join(OWNER_FILE)).unwrap();
            let recorded = |key: &str| {
                let (_, rest) = text.split_once(&format!("\"{key}\":"))?;
                let end = rest.find([',', '}']).unwrap_or(rest.len());
                rest[..end].trim().parse::<f64>().ok()
            };
            assert_eq!(recorded("lockBirthtimeMs"), birth_ms(&current));
            assert_eq!(recorded("legacyLockBirthtimeMs"), birth_ms(&legacy));
            let record = read_owner(&dir.join(OWNER_FILE)).unwrap();
            assert!(same_birth(record.lock_birth, birth_ms(&current).unwrap()));
            // Alive: never taken over, however long its mtime stands still.
            assert!(!take_over(&dir, modified(&current).unwrap()));
            drop(lock);
            assert!(!dir.join(OWNER_FILE).exists());
        }

        #[test]
        fn birth_times_match_what_node_reports() {
            let home = tempfile::tempdir().unwrap();
            let dir = config_dir(&home);
            let script =
                "process.stdout.write(String(require('fs').statSync(process.argv[1]).birthtimeMs))";
            let Ok(output) = std::process::Command::new("node")
                .args(["-e", script])
                .arg(&dir)
                .output()
            else {
                return; // Node is not installed here.
            };
            let reported: f64 = String::from_utf8(output.stdout).unwrap().parse().unwrap();
            // Node reports 0 where the file system keeps no birth time.
            assert_eq!((reported > 0.0).then_some(reported), birth_ms(&dir));
        }

        #[test]
        fn a_dead_holder_is_taken_over_once_its_mtime_stands_still() {
            let home = tempfile::tempdir().unwrap();
            let dir = config_dir(&home);
            held_by(&dir, dead_pid(), |_| {});
            let started = Instant::now();
            let lock = RefreshLock::acquire(&dir, quick(5_000)).unwrap().unwrap();
            // Well before the locks go stale, and after the watch.
            assert!(started.elapsed() >= Duration::from_millis(100));
            assert!(started.elapsed() < Duration::from_secs(5));
            assert!(!lock.compromised());
            // The record is ours now.
            let record = read_owner(&dir.join(OWNER_FILE)).unwrap();
            assert_eq!(record.pid, i64::from(std::process::id()));
        }

        #[test]
        fn a_reused_pid_is_proven_gone_by_its_start_time() {
            let home = tempfile::tempdir().unwrap();
            let dir = config_dir(&home);
            // This process is alive, but started at another time than recorded.
            held_by(&dir, std::process::id(), |_| {});
            assert!(RefreshLock::acquire(&dir, quick(5_000)).unwrap().is_some());
        }

        #[test]
        fn a_holder_not_proven_gone_keeps_its_locks() {
            let parent = std::os::unix::process::parent_id();
            let cases: Vec<(&str, u32, Edit)> = vec![
                (
                    "alive",
                    parent,
                    Box::new(move |record| {
                        record["procStart"] = unix::proc_start(parent).into();
                    }),
                ),
                (
                    "alive without a start time",
                    parent,
                    Box::new(|record| {
                        record.as_object_mut().unwrap().remove("procStart");
                    }),
                ),
                (
                    "another host",
                    dead_pid(),
                    Box::new(|record| record["pidSpace"] = "elsewhere".into()),
                ),
                (
                    "another PID namespace",
                    dead_pid(),
                    Box::new(|record| record["pidDomain"] = "darwin:other".into()),
                ),
                (
                    "other locks",
                    dead_pid(),
                    Box::new(|record| record["lockBirthtimeMs"] = 1.into()),
                ),
                (
                    "another legacy lock",
                    dead_pid(),
                    Box::new(|record| record["legacyLockBirthtimeMs"] = 1.into()),
                ),
                (
                    "no legacy birth time",
                    dead_pid(),
                    Box::new(|record| {
                        record
                            .as_object_mut()
                            .unwrap()
                            .remove("legacyLockBirthtimeMs");
                    }),
                ),
                (
                    "invalid",
                    dead_pid(),
                    Box::new(|record| record["pid"] = "1".into()),
                ),
            ];
            for (case, pid, edit) in cases {
                let home = tempfile::tempdir().unwrap();
                let dir = config_dir(&home);
                let locks = held_by(&dir, pid, edit);
                assert!(
                    RefreshLock::acquire(&dir, quick(400)).unwrap().is_none(),
                    "{case}"
                );
                assert!(locks.iter().all(|lock| lock.is_dir()), "{case}");
                assert!(dir.join(OWNER_FILE).exists(), "{case}");
            }
            // No record at all: only staleness frees the locks.
            let home = tempfile::tempdir().unwrap();
            let dir = config_dir(&home);
            held_by(&dir, dead_pid(), |_| {});
            fs::remove_file(dir.join(OWNER_FILE)).unwrap();
            assert!(RefreshLock::acquire(&dir, quick(400)).unwrap().is_none());
        }

        #[test]
        fn a_moving_mtime_marks_the_holder_alive() {
            let home = tempfile::tempdir().unwrap();
            let dir = config_dir(&home);
            let [current, _] = held_by(&dir, dead_pid(), |_| {});
            let bumper = {
                let current = current.clone();
                thread::spawn(move || {
                    for step in 0..20u64 {
                        thread::sleep(Duration::from_millis(30));
                        let _ = touch(&current, SystemTime::now() + Duration::from_millis(step));
                    }
                })
            };
            assert!(RefreshLock::acquire(&dir, quick(500)).unwrap().is_none());
            bumper.join().unwrap();
            assert!(current.is_dir());
        }
    }
}
