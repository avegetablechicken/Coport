//! The Daemon's lifecycle: finding the one that serves a data directory,
//! starting one, and the Restart that replaces it. A Restart checks the new
//! configuration before the old Daemon stops, waits for its requests in
//! flight unless forced, never starts over a Daemon that still holds its
//! lock, and falls back to the previous program when the new one does not
//! become healthy. One Restart runs at a time per data directory, across
//! processes; a Restart that finds another running joins it. What differs
//! by operating system sits behind `Platform`.

use crate::daemon::{self, Client, Status};
use std::{
    fs::{self, File},
    io,
    net::{Ipv4Addr, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

mod native;
pub use native::Native;

/// What differs by operating system.
pub trait Platform {
    /// Detach the process `command` starts from the caller's session or
    /// console, so it outlives the caller.
    fn detach(&self, command: &mut Command);
    /// The unit that supervises process `pid` and would start it again on
    /// its own, if any.
    fn supervisor(&self, pid: u32) -> Option<String>;
    /// Copy the program process `pid` runs to `to`, even if its file was
    /// replaced since; false where the platform cannot.
    fn copy_program(&self, pid: u32, to: &Path) -> io::Result<bool>;
    /// Established TCP connections on local `port`, where they can be
    /// counted: what a Daemon too old to count its requests is waited on by.
    fn connections(&self, port: u16) -> Option<usize>;
}

/// The configuration and log a Daemon runs with.
#[derive(Clone, Copy)]
pub struct Launch<'a> {
    pub config: &'a Path,
    pub log: &'a Path,
}

/// Whether a Restart waits for the old Daemon's requests in flight.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Wait {
    /// Stop it at once, interrupting them.
    Force,
    /// Wait for them up to this long, then cancel the Restart.
    Idle(Duration),
}

/// How a Restart is going.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Progress {
    /// Another Restart is running; this one waits for its end.
    Joining,
    /// This many requests are still in flight.
    Waiting(usize),
    /// The old Daemon is about to stop. From here the Restart must run to
    /// its end, or nothing serves.
    Stopping,
    /// The old Daemon stopped.
    Stopped,
}

/// The Daemon a Restart left running.
pub struct Restarted {
    pub client: Client,
    pub status: Status,
    /// Why the new Daemon failed, when the previous program runs again.
    pub rolled_back: Option<String>,
}

#[derive(Clone)]
pub struct Lifecycle<P = Native> {
    dir: PathBuf,
    binary: PathBuf,
    platform: P,
}

/// How long a started Daemon has to register and accept connections. Under
/// heavy load, the fsyncs of startup alone took over 5 s; a slow Daemon is
/// not a broken one, and one that exits is noticed at once.
const READY: Duration = Duration::from_secs(15);
/// Idle checks in a row, a check interval apart, before a Restart proceeds.
const QUIET_CHECKS: u32 = 4;
const CHECK_INTERVAL: Duration = Duration::from_millis(250);

impl Lifecycle {
    /// This user's Daemon, started from the program beside this one.
    pub fn local() -> Self {
        Self::new(
            crate::settings::app_dir(),
            daemon::binary_path().unwrap_or_default(),
        )
    }
    /// The Daemon serving `dir`, started from `binary`.
    pub fn new(dir: PathBuf, binary: PathBuf) -> Self {
        Self::on(dir, binary, Native)
    }
}

impl<P: Platform> Lifecycle<P> {
    pub fn on(dir: PathBuf, binary: PathBuf, platform: P) -> Self {
        Self {
            dir,
            binary,
            platform,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The running Daemon, if it answers. Not finding one is no proof that
    /// none runs: see `find`.
    pub fn discover(&self) -> Option<(Client, Status)> {
        Client::discover(&self.dir)
    }

    /// The running Daemon, or none. An error when one still holds its lock
    /// without answering, since nothing may start over it.
    pub fn find(&self) -> io::Result<Option<(Client, Status)>> {
        if let Some(found) = self.discover() {
            return Ok(Some(found));
        }
        if self.unlocked()? {
            Ok(None)
        } else {
            Err(io::Error::other(
                "The proxy daemon holds its lock but does not answer; it was left running.",
            ))
        }
    }

    /// Replace the running Daemon with one running `launch`, or start one
    /// if none runs. `progress` hears how it goes.
    pub fn restart(
        &self,
        launch: Launch,
        wait: Wait,
        progress: &mut dyn FnMut(Progress),
    ) -> io::Result<Restarted> {
        fs::create_dir_all(self.restarts())?;
        let lock = daemon::lock_file(&self.dir.join("restart.lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return self.join(&lock, wait, progress),
            Err(fs::TryLockError::Error(error)) => return Err(error),
        }
        // A request a cancelled Restart left must not force this one.
        let _ = fs::remove_file(self.force_request());
        let _held = daemon::Held(lock);
        self.restart_locked(launch, wait, progress)
    }

    /// Wait for the Restart holding `lock`; a forced one forces it.
    fn join(
        &self,
        lock: &File,
        wait: Wait,
        progress: &mut dyn FnMut(Progress),
    ) -> io::Result<Restarted> {
        let before = self.discover().map(|(_, status)| status.pid);
        if wait == Wait::Force {
            File::create(self.force_request())?;
        }
        progress(Progress::Joining);
        lock.lock()?;
        lock.unlock()?;
        let (client, status) = self.find()?.ok_or_else(|| {
            io::Error::other("The restart in progress left no proxy daemon running.")
        })?;
        if before == Some(status.pid) {
            return Err(io::Error::other(
                "The restart in progress was cancelled; the proxy daemon keeps running.",
            ));
        }
        Ok(Restarted {
            client,
            status,
            rolled_back: None,
        })
    }

    fn restart_locked(
        &self,
        launch: Launch,
        wait: Wait,
        progress: &mut dyn FnMut(Progress),
    ) -> io::Result<Restarted> {
        let running = self.find()?;
        if let Some((_, status)) = &running
            && let Some(unit) = self.platform.supervisor(status.pid)
        {
            return Err(io::Error::other(format!(
                "The proxy daemon is supervised by {unit}; restart that unit instead."
            )));
        }
        let config = fs::read(launch.config)?;
        check(launch.config, running.is_some())?;
        let Some((client, before)) = running else {
            let (client, status) = self.spawn(launch)?;
            return Ok(Restarted {
                client,
                status,
                rolled_back: None,
            });
        };
        let previous = self.restarts().join("previous-coportd");
        let fallback = self
            .platform
            .copy_program(before.pid, &previous)
            .unwrap_or(false);
        if let Wait::Idle(limit) = wait {
            self.wait_idle(&client, &before, limit, progress)?;
        }
        let current = client.status()?;
        if (
            current.pid,
            current.port,
            &current.config_path,
            &current.log_path,
        ) != (
            before.pid,
            before.port,
            &before.config_path,
            &before.log_path,
        ) {
            return Err(io::Error::other(
                "The proxy daemon changed during the restart; nothing was stopped.",
            ));
        }
        if fs::read(launch.config)? != config {
            return Err(io::Error::other(
                "The configuration changed during the restart; nothing was stopped.",
            ));
        }
        progress(Progress::Stopping);
        client
            .stop()
            .map_err(|error| io::Error::other(format!("Cannot stop proxy daemon: {error}")))?;
        progress(Progress::Stopped);
        let error = match self.spawn(launch) {
            Ok((client, status)) => {
                return Ok(Restarted {
                    client,
                    status,
                    rolled_back: None,
                });
            }
            Err(error) if fallback => error,
            Err(error) => return Err(error),
        };
        if !self.unlocked()? {
            return Err(io::Error::other(format!(
                "{error}; another proxy daemon holds the lock, so the previous one was not restored."
            )));
        }
        let restore = Launch {
            config: &before.config_path,
            log: &before.log_path,
        };
        let (client, status) = self.spawn_program(&previous, restore).map_err(|restore| {
            io::Error::other(format!(
                "{error}; restoring the previous proxy daemon failed too: {restore}"
            ))
        })?;
        Ok(Restarted {
            client,
            status,
            rolled_back: Some(error.to_string()),
        })
    }

    /// Wait until the Daemon has no request in flight for a few checks in
    /// a row, or a later forced Restart asks it to go ahead.
    fn wait_idle(
        &self,
        client: &Client,
        before: &Status,
        limit: Duration,
        progress: &mut dyn FnMut(Progress),
    ) -> io::Result<()> {
        let deadline = Instant::now() + limit;
        let mut quiet = 0;
        let mut reported = None;
        loop {
            if self.force_request().exists() {
                return Ok(());
            }
            let busy = match client.status()?.requests_in_flight {
                Some(requests) => requests,
                None => self.platform.connections(before.port).unwrap_or(0),
            };
            if busy == 0 {
                quiet += 1;
                if quiet >= QUIET_CHECKS {
                    return Ok(());
                }
            } else {
                quiet = 0;
                if reported != Some(busy) {
                    progress(Progress::Waiting(busy));
                    reported = Some(busy);
                }
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other(format!(
                    "{busy} request(s) still in flight; the restart was cancelled and the proxy daemon keeps running. Force the restart to interrupt them."
                )));
            }
            std::thread::sleep(CHECK_INTERVAL);
        }
    }

    /// Start a Daemon running `launch` now, without a Restart's checks,
    /// and wait until it registers and its proxy port accepts connections.
    pub fn spawn(&self, launch: Launch) -> io::Result<(Client, Status)> {
        self.spawn_program(&self.binary, launch)
    }

    /// Detaches at spawn, not at GUI exit. No pipes or runtime tasks owned
    /// by the GUI are required to keep the listener or in-flight requests
    /// alive.
    fn spawn_program(&self, binary: &Path, launch: Launch) -> io::Result<(Client, Status)> {
        fs::create_dir_all(&self.dir)?;
        let stderr = daemon::private_file(&self.dir.join("daemon.stderr.log"))?;
        let mut command = Command::new(binary);
        command
            .arg(&self.dir)
            .arg(launch.config)
            .arg(launch.log)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr);
        self.platform.detach(&mut command);
        let mut child = command.spawn().map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "Cannot start {}: {e}. Keep the daemon executable beside the GUI.",
                    binary.display()
                ),
            )
        })?;
        let deadline = Instant::now() + READY;
        loop {
            if let Some(code) = child.try_wait()? {
                let path = self.dir.join("daemon.stderr.log");
                // The log is appended across starts; its last line is this exit's reason.
                let reason = fs::read_to_string(&path)
                    .ok()
                    .and_then(|text| {
                        text.lines()
                            .rev()
                            .find(|l| !l.trim().is_empty())
                            .map(str::to_owned)
                    })
                    .map(|line| format!(": {line}"))
                    .unwrap_or_default();
                return Err(io::Error::other(format!(
                    "Proxy daemon exited ({code}){reason}; see {}",
                    path.display()
                )));
            }
            if let Some((client, status)) = self.discover()
                && status.pid == child.id()
                && listening(status.port)
            {
                // Reap while the caller is alive; dropping this thread on
                // exit has no effect on the independently running child.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return Ok((client, status));
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Proxy daemon did not become ready",
                ));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Whether no Daemon holds the data directory's lock.
    fn unlocked(&self) -> io::Result<bool> {
        let path = self.dir.join("daemon.lock");
        if !path.exists() {
            return Ok(true);
        }
        Ok(daemon::try_hold(&path)?.is_some())
    }

    fn restarts(&self) -> PathBuf {
        self.dir.join("restart")
    }

    /// Left by a forced Restart for the one it joins.
    fn force_request(&self) -> PathBuf {
        self.restarts().join("force-request")
    }
}

/// The configuration at `path` can run; with `credentials`, its saved
/// logins and keys are usable too, as a running Daemon's are.
fn check(path: &Path, credentials: bool) -> io::Result<()> {
    let config = coport::config::Config::read(path)
        .and_then(|config| config.check_external_data().map(|()| config))
        .map_err(|error| io::Error::other(error.message.to_string()))?;
    if !credentials {
        return Ok(());
    }
    // The caller may be on a runtime thread already; check on a thread of
    // its own.
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?
                    .block_on(config.check_credentials())
                    .map_err(|error| {
                        io::Error::other(format!(
                            "{}; the running proxy daemon was not stopped.",
                            error.message
                        ))
                    })
            })
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("The credential check failed.")))
    })
}

/// Whether the proxy port accepts connections. Not `/health`: a Daemon
/// forwarding to another device relays every connection there, and an
/// offline device would fail a healthy Daemon.
fn listening(port: u16) -> bool {
    TcpStream::connect_timeout(
        &(Ipv4Addr::LOCALHOST, port).into(),
        Duration::from_millis(500),
    )
    .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::tests::fake_daemon_with;
    use std::sync::{Arc, Mutex, atomic::AtomicBool};

    /// A platform whose supervisor and connection count the test chooses,
    /// and which keeps no previous program.
    #[derive(Clone, Default)]
    struct Fake {
        supervisor: Option<String>,
        connections: Option<usize>,
    }
    impl Platform for Fake {
        fn detach(&self, _: &mut Command) {}
        fn supervisor(&self, _: u32) -> Option<String> {
            self.supervisor.clone()
        }
        fn copy_program(&self, _: u32, _: &Path) -> io::Result<bool> {
            Ok(false)
        }
        fn connections(&self, _: u16) -> Option<usize> {
            self.connections
        }
    }

    struct Daemon {
        dir: tempfile::TempDir,
        actions: Arc<Mutex<Vec<String>>>,
    }
    impl Daemon {
        /// A fake Daemon reporting `requests` in flight, with a valid
        /// configuration to restart it with.
        fn serving(requests: Option<usize>) -> Self {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join("config.yaml"),
                "listen_port: 7889\nrequest_timeout_seconds: 3\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n",
            )
            .unwrap();
            let actions = Arc::default();
            fake_daemon_with(
                dir.path(),
                Arc::new(AtomicBool::new(true)),
                move |status| status.requests_in_flight = requests,
                Arc::clone(&actions),
            );
            Self { dir, actions }
        }
        fn lifecycle(&self, platform: Fake) -> Lifecycle<Fake> {
            Lifecycle::on(
                self.dir.path().to_owned(),
                PathBuf::from("missing-binary"),
                platform,
            )
        }
        fn restart(&self, platform: Fake, wait: Wait) -> (io::Result<Restarted>, Vec<Progress>) {
            let config = self.dir.path().join("config.yaml");
            let log = self.dir.path().join("proxy.log");
            let mut heard = Vec::new();
            let result = self.lifecycle(platform).restart(
                Launch {
                    config: &config,
                    log: &log,
                },
                wait,
                &mut |progress| heard.push(progress),
            );
            (result, heard)
        }
        fn stopped(&self) -> bool {
            self.actions.lock().unwrap().iter().any(|a| a == "\"Stop\"")
        }
    }

    fn message(result: io::Result<Restarted>) -> String {
        result.err().expect("the restart failed").to_string()
    }

    #[test]
    fn a_restart_leaves_a_daemon_it_must_not_replace_running() {
        let _guard = daemon::spawn_guard();
        let supervised = Daemon::serving(Some(0));
        let platform = Fake {
            supervisor: Some("coportd.service".into()),
            ..Fake::default()
        };
        let (result, _) = supervised.restart(platform, Wait::Force);
        assert!(message(result).contains("coportd.service"));
        assert!(!supervised.stopped());

        let invalid = Daemon::serving(Some(0));
        std::fs::write(invalid.dir.path().join("config.yaml"), "listen_port: [").unwrap();
        let (result, _) = invalid.restart(Fake::default(), Wait::Force);
        assert!(result.is_err());
        assert!(!invalid.stopped());

        // A daemon that holds its lock without answering is never started over.
        let dir = tempfile::tempdir().unwrap();
        let lock = daemon::lock_file(&dir.path().join("daemon.lock")).unwrap();
        lock.try_lock().unwrap();
        std::fs::write(dir.path().join("config.yaml"), "listen_port: 7889\n").unwrap();
        let unresponsive = Lifecycle::on(
            dir.path().to_owned(),
            PathBuf::from("missing-binary"),
            Fake::default(),
        );
        let config = dir.path().join("config.yaml");
        let result = unresponsive.restart(
            Launch {
                config: &config,
                log: &dir.path().join("proxy.log"),
            },
            Wait::Force,
            &mut |_| {},
        );
        assert!(message(result).contains("does not answer"));
    }

    #[test]
    fn a_restart_that_waits_too_long_is_cancelled_before_anything_stops() {
        let _guard = daemon::spawn_guard();
        let busy = Daemon::serving(Some(2));
        let (result, heard) = busy.restart(Fake::default(), Wait::Idle(Duration::from_millis(300)));
        assert!(message(result).contains("2 request(s) still in flight"));
        assert_eq!(heard, [Progress::Waiting(2)]);
        assert!(!busy.stopped());

        // A daemon too old to count its requests is waited on by its connections.
        let old = Daemon::serving(None);
        let platform = Fake {
            connections: Some(1),
            ..Fake::default()
        };
        let (result, heard) = old.restart(platform, Wait::Idle(Duration::from_millis(300)));
        assert!(message(result).contains("1 request(s)"));
        assert_eq!(heard, [Progress::Waiting(1)]);
        assert!(!old.stopped());
    }

    #[test]
    fn an_idle_or_forced_restart_stops_the_daemon_at_once() {
        let _guard = daemon::spawn_guard();
        for (requests, wait) in [
            (Some(0), Wait::Idle(Duration::from_secs(10))),
            (None, Wait::Idle(Duration::from_secs(10))),
            (Some(3), Wait::Force),
        ] {
            let daemon = Daemon::serving(requests);
            let started = Instant::now();
            let (result, heard) = daemon.restart(Fake::default(), wait);
            // The fake daemon answers Stop as no daemon does.
            assert!(message(result).contains("Cannot stop proxy daemon"));
            assert_eq!(heard, [Progress::Stopping]);
            assert!(daemon.stopped());
            assert!(started.elapsed() < Duration::from_secs(5));
        }
    }

    #[test]
    fn a_forced_restart_joins_a_waiting_one_and_forces_it() {
        let _guard = daemon::spawn_guard();
        let daemon = Arc::new(Daemon::serving(Some(1)));
        let waiting = std::thread::spawn({
            let daemon = daemon.clone();
            move || daemon.restart(Fake::default(), Wait::Idle(Duration::from_secs(30)))
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        // Its third Status request is its first check for requests in flight.
        let statuses = || {
            let actions = daemon.actions.lock().unwrap();
            actions.iter().filter(|a| *a == "\"Status\"").count()
        };
        while statuses() < 3 {
            assert!(Instant::now() < deadline, "the first restart never waited");
            std::thread::sleep(Duration::from_millis(10));
        }
        let started = Instant::now();
        let (joined, heard) = daemon.restart(Fake::default(), Wait::Force);
        assert_eq!(heard, [Progress::Joining]);
        let (forced, progress) = waiting.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(progress, [Progress::Waiting(1), Progress::Stopping]);
        assert!(forced.is_err());
        assert!(daemon.stopped());
        // The fake daemon outlives its Stop, so the joined restart sees it
        // unchanged.
        assert!(message(joined).contains("cancelled"));
    }
}
