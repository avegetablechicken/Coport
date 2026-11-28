//! One app instance per user. A later launch asks the running instance to
//! show its window, then exits instead of adding a second tray icon and
//! competing for the proxy port.

use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    net::{Ipv4Addr, TcpListener, TcpStream},
    path::PathBuf,
    time::Duration,
};

const SHOW: &[u8] = b"show";

pub enum Instance {
    Primary(Guard),
    /// Another instance is running and was asked to show its window.
    Secondary,
}

/// Holds the lock for the life of the process.
pub struct Guard {
    _lock: Option<File>,
    listener: Option<TcpListener>,
}

pub fn acquire(dir: PathBuf) -> Instance {
    let _ = std::fs::create_dir_all(&dir);
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("gui.lock"));
    let Ok(lock) = lock else {
        // Without a lock file, run anyway rather than refuse to start.
        return Instance::Primary(Guard {
            _lock: None,
            listener: None,
        });
    };
    // The port lives in a separate file: Windows locks block reading the locked file.
    let port_file = dir.join("gui.port");
    match lock.try_lock() {
        Ok(()) => {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).ok();
            if let Some(port) = listener.as_ref().and_then(|l| l.local_addr().ok()) {
                let _ = std::fs::write(&port_file, port.port().to_string());
            }
            Instance::Primary(Guard {
                _lock: Some(lock),
                listener,
            })
        }
        Err(std::fs::TryLockError::WouldBlock) => {
            let port = std::fs::read_to_string(&port_file)
                .ok()
                .and_then(|p| p.trim().parse::<u16>().ok());
            if let Some(port) = port
                && let Ok(mut stream) = TcpStream::connect_timeout(
                    &(Ipv4Addr::LOCALHOST, port).into(),
                    Duration::from_secs(1),
                )
            {
                let _ = stream.write_all(SHOW);
            }
            Instance::Secondary
        }
        Err(std::fs::TryLockError::Error(_)) => Instance::Primary(Guard {
            _lock: None,
            listener: None,
        }),
    }
}

impl Guard {
    /// Calls `show` whenever a later launch asks for the window.
    pub fn listen(&mut self, show: impl Fn() + Send + 'static) {
        let Some(listener) = self.listener.take() else {
            return;
        };
        let _ = std::thread::Builder::new()
            .name("single-instance".into())
            .spawn(move || {
                for mut stream in listener.incoming().flatten() {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                    let mut buf = [0u8; 4];
                    if stream.read_exact(&mut buf).is_ok() && buf == SHOW {
                        show();
                    }
                }
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn second_launch_signals_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let Instance::Primary(mut guard) = acquire(dir.path().into()) else {
            panic!("first launch must be primary");
        };
        assert!(
            guard.listener.is_some(),
            "first launch must bind its control listener"
        );
        let (shown, received) = mpsc::channel();
        guard.listen(move || {
            let _ = shown.send(());
        });
        assert!(matches!(acquire(dir.path().into()), Instance::Secondary));
        received
            .recv_timeout(Duration::from_secs(5))
            .expect("second launch must show the panel");
        drop(guard);
        assert!(matches!(acquire(dir.path().into()), Instance::Primary(_)));
    }
}
