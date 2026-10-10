//! The panel never waits on daemon I/O while holding its application-state lock.
use super::{Controller, Notify, Phase, Probe, Shared, daemon};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, SystemTime},
};

pub type Completion = tokio::sync::oneshot::Receiver<Result<(), String>>;
const INITIALIZING: usize = 1 << (usize::BITS - 1);

type Operation = Box<dyn FnOnce(&mut Controller) -> Result<(), String> + Send>;
struct Pending {
    count: Arc<AtomicUsize>,
    notify: Notify,
    amount: usize,
}
impl Drop for Pending {
    fn drop(&mut self) {
        self.count.fetch_sub(self.amount, Ordering::AcqRel);
        (self.notify)();
    }
}
struct Job {
    run: Operation,
    reply: Option<tokio::sync::oneshot::Sender<Result<(), String>>>,
    pending: Option<Pending>,
    exit: bool,
}

/// A single worker owns the controller and runtime. UI reads never lock it.
pub struct BackgroundController {
    jobs: mpsc::SyncSender<Job>,
    shared: Arc<Mutex<Shared>>,
    status: Arc<Mutex<Option<daemon::Status>>>,
    error: Arc<Mutex<Option<String>>>,
    pending: Arc<AtomicUsize>,
    submission: Mutex<()>,
    closing: Arc<AtomicBool>,
    notify: Notify,
}
// These are display caches, replaced completely from the controller's state.
// No partially updated control state is reused after poison.
fn replace_cache<T>(cache: &Mutex<T>, value: T) {
    let mut guard = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = value;
    cache.clear_poison();
}
impl BackgroundController {
    pub fn new(notify: Notify) -> Self {
        Self::with_daemon(
            notify,
            crate::settings::app_dir(),
            daemon::binary_path().unwrap_or_default(),
        )
    }
    fn with_daemon(notify: Notify, dir: PathBuf, binary: PathBuf) -> Self {
        let (jobs, receiver) = mpsc::sync_channel::<Job>(16);
        let shared = Arc::new(Mutex::new(Shared::default()));
        let status = Arc::new(Mutex::new(None));
        let error = Arc::new(Mutex::new(None));
        let pending = Arc::new(AtomicUsize::new(INITIALIZING));
        let closing = Arc::new(AtomicBool::new(false));
        let result = Self {
            jobs,
            shared: shared.clone(),
            status: status.clone(),
            error: error.clone(),
            pending: pending.clone(),
            submission: Mutex::new(()),
            closing: closing.clone(),
            notify: notify.clone(),
        };
        std::thread::Builder::new()
            .name("proxy-control".into())
            .spawn(move || {
                let initial = Pending {
                    count: pending,
                    notify: notify.clone(),
                    amount: INITIALIZING,
                };
                let mut controller = Controller::with_shared(notify.clone(), dir, binary, shared);
                replace_cache(&status, controller.daemon_status().cloned());
                drop(initial);
                while let Ok(Job {
                    run,
                    reply,
                    pending,
                    exit,
                }) = receiver.recv()
                {
                    let result = run(&mut controller);
                    replace_cache(&status, controller.daemon_status().cloned());
                    if pending.is_some() {
                        replace_cache(&error, result.as_ref().err().cloned());
                    } else {
                        notify();
                    }
                    let finished = exit && result.is_ok();
                    if exit && !finished {
                        closing.store(false, Ordering::Release);
                    }
                    drop(pending);
                    if let Some(reply) = reply {
                        let _ = reply.send(result);
                    }
                    if finished {
                        break;
                    }
                }
            })
            .expect("proxy control thread");
        result
    }
    fn operation(&self, run: Operation, exit: bool) -> Result<Completion, String> {
        if self.shared.is_poisoned() {
            return Err("Proxy control state is unavailable; reopen the application.".into());
        }
        let _submission = self
            .submission
            .lock()
            .map_err(|_| "The proxy operation queue is unavailable.".to_owned())?;
        if exit {
            if self.closing.swap(true, Ordering::AcqRel) {
                return Err("Proxy shutdown is already in progress.".into());
            }
            self.pending.fetch_add(1, Ordering::AcqRel);
        } else {
            if self.closing.load(Ordering::Acquire) {
                return Err("The application is closing.".into());
            }
            // Initialization and the operation count share one atomic value,
            // so completing discovery cannot lose the queued auto-start.
            let mut count = self.pending.load(Ordering::Acquire);
            loop {
                if count != 0 && count != INITIALIZING {
                    return Err("A proxy operation is already in progress.".into());
                }
                match self.pending.compare_exchange_weak(
                    count,
                    count + 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => break,
                    Err(current) => count = current,
                }
            }
        }
        let (reply, receive) = tokio::sync::oneshot::channel();
        let job = Job {
            run,
            reply: Some(reply),
            pending: Some(Pending {
                count: self.pending.clone(),
                notify: self.notify.clone(),
                amount: 1,
            }),
            exit,
        };
        // Clear the old error before publishing the job. A fast failure may
        // already have stored the new error when try_send returns.
        replace_cache(&self.error, None);
        if self.jobs.try_send(job).is_err() {
            if exit {
                self.closing.store(false, Ordering::Release);
            }
            return Err("The proxy control worker is unavailable.".into());
        }
        (self.notify)();
        Ok(receive)
    }
    pub fn start(&self, config: &Path, log: PathBuf) -> Result<Completion, String> {
        let config = config.to_owned();
        self.operation(Box::new(move |c| c.try_start(&config, log)), false)
    }
    pub fn start_if_stopped(&self, config: &Path, log: PathBuf) -> Result<Completion, String> {
        let config = config.to_owned();
        self.operation(
            Box::new(move |c| {
                if c.is_running() {
                    Ok(())
                } else {
                    c.try_start(&config, log)
                }
            }),
            false,
        )
    }
    pub fn stop(&self) -> Result<Completion, String> {
        self.operation(Box::new(Controller::stop), false)
    }
    pub fn on_app_exit(&self, keep: bool) -> Result<Completion, String> {
        self.operation(Box::new(move |c| c.on_app_exit(keep)), true)
    }
    pub fn busy(&self) -> bool {
        self.pending.load(Ordering::Acquire) != 0
    }
    pub fn phase(&self) -> Phase {
        self.shared
            .lock()
            .map(|s| s.phase.clone().unwrap_or(Phase::Stopped))
            .unwrap_or_else(|_| {
                Phase::Failed("Proxy status is unavailable; reopen the application.".into())
            })
    }
    pub fn is_running(&self) -> bool {
        matches!(self.phase(), Phase::Running { .. })
    }
    pub fn daemon_status(&self) -> Option<daemon::Status> {
        self.status.lock().ok().and_then(|s| s.clone())
    }
    pub fn error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|s| s.clone())
    }
    pub fn probes(&self) -> BTreeMap<String, (Probe, SystemTime)> {
        self.shared
            .lock()
            .map(|s| {
                s.probes
                    .iter()
                    .map(|(name, (probe, _, at))| (name.clone(), (probe.clone(), *at)))
                    .collect()
            })
            .unwrap_or_default()
    }
    /// Only expose results measured for the currently configured endpoint.
    pub fn probes_for(
        &self,
        endpoints: &BTreeMap<String, String>,
    ) -> BTreeMap<String, (Probe, SystemTime)> {
        self.shared
            .lock()
            .map(|s| {
                s.probes
                    .iter()
                    .filter(|(name, _)| {
                        endpoints
                            .get(*name)
                            .is_some_and(|endpoint| s.probe_endpoints.get(*name) == Some(endpoint))
                    })
                    .map(|(name, (probe, _, at))| (name.clone(), (probe.clone(), *at)))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn diagnostic(&self, run: Operation) {
        // Startup probes are requested once. Queue them behind discovery/start
        // instead of losing them while the control worker is busy. Serialize
        // submission with shutdown so no diagnostic is queued after exit.
        let Ok(_submission) = self.submission.lock() else {
            return;
        };
        if !self.closing.load(Ordering::Acquire) && !self.shared.is_poisoned() {
            let _ = self.jobs.try_send(Job {
                run,
                reply: None,
                pending: None,
                exit: false,
            });
        }
    }
    pub fn probe(&self, name: &str, endpoint: &str) {
        let (name, endpoint) = (name.to_owned(), endpoint.to_owned());
        self.diagnostic(Box::new(move |c| {
            c.probe(&name, &endpoint);
            Ok(())
        }));
    }
    pub fn probe_stale<'a>(
        &self,
        proxies: impl IntoIterator<Item = (&'a String, &'a String)>,
        age: Duration,
    ) {
        let proxies: Vec<_> = proxies
            .into_iter()
            .map(|(n, p)| (n.clone(), p.clone()))
            .collect();
        self.diagnostic(Box::new(move |c| {
            c.probe_stale(proxies.iter().map(|(n, p)| (n, p)), age);
            Ok(())
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_do_not_relabel_old_probe_results_with_new_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let controller = BackgroundController::with_daemon(
            Arc::new(|| {}),
            dir.path().into(),
            dir.path().join("missing-daemon"),
        );
        {
            let mut shared = controller.shared.lock().unwrap();
            shared
                .probe_endpoints
                .insert("same".into(), "http://127.0.0.1:12345".into());
            shared.probes.insert(
                "same".into(),
                (
                    Probe::Reachable {
                        latency: Duration::from_millis(3),
                        exit: None,
                    },
                    std::time::Instant::now(),
                    SystemTime::now(),
                ),
            );
        }
        let mut endpoints = BTreeMap::from([("same".into(), "http://127.0.0.1:12345".into())]);
        assert_eq!(controller.probes_for(&endpoints).len(), 1);
        endpoints.insert("same".into(), "http://127.0.0.1:23456".into());
        assert!(controller.probes_for(&endpoints).is_empty());
        assert!(controller.probes_for(&BTreeMap::new()).is_empty());
    }

    #[test]
    fn startup_and_manual_probes_wait_for_busy_worker_then_fall_back_locally() {
        for automatic in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let controller = BackgroundController::with_daemon(
                Arc::new(|| {}),
                dir.path().into(),
                dir.path().join("missing-daemon"),
            );
            let (entered, started) = mpsc::channel();
            let (release, released) = mpsc::channel();
            let operation = controller
                .operation(
                    Box::new(move |_| {
                        entered.send(()).unwrap();
                        released.recv_timeout(Duration::from_secs(5)).unwrap();
                        Ok(())
                    }),
                    false,
                )
                .unwrap();
            started.recv_timeout(Duration::from_secs(5)).unwrap();
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let name = "startup".to_owned();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            if automatic {
                controller.probe_stale([(&name, &endpoint)], Duration::from_secs(120));
            } else {
                controller.probe(&name, &endpoint);
            }
            assert!(controller.probes().is_empty());
            release.send(()).unwrap();
            operation.blocking_recv().unwrap().unwrap();
            let mut incoming = None;
            super::super::tests::wait_for(|| {
                incoming = listener.accept().ok();
                incoming.is_some()
            });
            // A real local connection proves the queued test reached GUI fallback.
            drop(incoming);
            super::super::tests::wait_for(|| {
                matches!(
                    controller.probes().get(&name),
                    Some((Probe::Unreachable(_), _))
                )
            });
            controller
                .on_app_exit(true)
                .unwrap()
                .blocking_recv()
                .unwrap()
                .unwrap();
        }
    }

    #[test]
    fn slow_control_operations_leave_status_readable_and_quit_is_serialized() {
        let dir = tempfile::tempdir().unwrap();
        let controller = BackgroundController::with_daemon(
            Arc::new(|| {}),
            dir.path().into(),
            dir.path().join("missing-daemon"),
        );
        let (entered, started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let operation = controller
            .operation(
                Box::new(move |_| {
                    entered.send(()).unwrap();
                    released.recv().unwrap();
                    Ok(())
                }),
                false,
            )
            .unwrap();
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(controller.busy());
        assert!(matches!(controller.phase(), Phase::Stopped));
        assert!(controller.daemon_status().is_none());
        assert!(controller.probes().is_empty());
        assert!(
            controller.stop().is_err(),
            "a second operation must not race the first"
        );
        let mut exiting = controller.on_app_exit(true).unwrap();
        assert!(matches!(
            exiting.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        release.send(()).unwrap();
        operation.blocking_recv().unwrap().unwrap();
        exiting.blocking_recv().unwrap().unwrap();
        assert!(!controller.busy());
        assert!(
            controller.stop().is_err(),
            "no operation starts after exit was accepted"
        );
    }

    #[test]
    fn background_exit_preserves_or_stops_the_daemon_as_requested() {
        let _guard = crate::daemon::spawn_guard();
        for keep in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let (_signal, daemon) = crate::daemon::tests::serve_in_thread(dir.path());
            let (client, status) = crate::daemon::tests::wait_for_daemon(dir.path());
            let controller = BackgroundController::with_daemon(
                Arc::new(|| {}),
                dir.path().into(),
                dir.path().join("missing-daemon"),
            );
            // Auto-start queues behind discovery and must not restart a daemon
            // it discovers there (the replacement helper deliberately does not exist).
            controller
                .start_if_stopped(
                    &dir.path().join("config.yaml"),
                    dir.path().join("proxy.log"),
                )
                .unwrap()
                .blocking_recv()
                .unwrap()
                .unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while controller.busy() {
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(controller.daemon_status().unwrap().pid, status.pid);
            controller
                .on_app_exit(keep)
                .unwrap()
                .blocking_recv()
                .unwrap()
                .unwrap();
            if keep {
                assert_eq!(client.status().unwrap().pid, status.pid);
                client.stop().unwrap();
            }
            daemon.join().unwrap().unwrap();
            assert!(crate::daemon::Client::discover(dir.path()).is_none());
        }
    }

    #[test]
    fn a_fast_failure_remains_visible_after_completion() {
        let dir = tempfile::tempdir().unwrap();
        let controller = BackgroundController::with_daemon(
            Arc::new(|| {}),
            dir.path().into(),
            dir.path().join("missing-daemon"),
        );
        let result = controller
            .operation(Box::new(|_| Err("cannot restart".into())), false)
            .unwrap()
            .blocking_recv()
            .unwrap();
        assert_eq!(result, Err("cannot restart".into()));
        assert_eq!(controller.error().as_deref(), Some("cannot restart"));
        controller
            .on_app_exit(true)
            .unwrap()
            .blocking_recv()
            .unwrap()
            .unwrap();
    }

    #[test]
    fn poisoned_control_state_is_reported_without_reusing_it() {
        let dir = tempfile::tempdir().unwrap();
        let controller = BackgroundController::with_daemon(
            Arc::new(|| {}),
            dir.path().into(),
            dir.path().join("missing-daemon"),
        );
        // Queue a harmless action behind discovery so no worker operation is active.
        controller
            .operation(Box::new(|_| Ok(())), false)
            .unwrap()
            .blocking_recv()
            .unwrap()
            .unwrap();
        let shared = controller.shared.clone();
        let _ = std::thread::spawn(move || {
            let _state = shared.lock().unwrap();
            panic!("interrupted control update");
        })
        .join();
        assert!(matches!(controller.phase(), Phase::Failed(_)));
        assert!(controller.stop().is_err());
        assert!(
            controller.shared.is_poisoned(),
            "control state must not be silently recovered"
        );
    }

    #[test]
    fn poisoned_display_cache_is_replaced_in_full() {
        let cache = Arc::new(Mutex::new(Some("old")));
        let copy = cache.clone();
        let _ = std::thread::spawn(move || {
            let mut value = copy.lock().unwrap();
            *value = Some("partial");
            panic!("interrupted cache replacement");
        })
        .join();
        replace_cache(&cache, Some("complete"));
        assert!(!cache.is_poisoned());
        assert_eq!(*cache.lock().unwrap(), Some("complete"));
    }
}
