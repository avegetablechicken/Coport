#![allow(dead_code)]
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

pub fn binary() -> PathBuf {
    std::env::var_os("COPORT_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_coport")))
}
pub fn port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
pub fn read_head(stream: &mut TcpStream) -> Vec<u8> {
    let mut data = Vec::new();
    while !data.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        data.push(byte[0]);
        assert!(data.len() < 65536);
    }
    data
}
pub fn request(
    port: u16,
    token: Option<&str>,
    account: Option<&str>,
    path: &str,
    method: &str,
) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(8)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(8)))
        .unwrap();
    let body = r#"{"private":"secret-body"}"#;
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(token) = token {
        head.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    if let Some(account) = account {
        head.push_str(&format!("ChatGPT-Account-Id: {account}\r\n"));
    }
    write!(stream, "{head}\r\n{body}").unwrap();
    let head = read_head(&mut stream);
    let status = String::from_utf8_lossy(&head)
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let mut body = vec![];
    stream.read_to_end(&mut body).unwrap();
    (status, body)
}

pub struct App {
    pub dir: tempfile::TempDir,
    pub port: u16,
    pub env: BTreeMap<String, String>,
    child: Option<Child>,
}
impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}
impl App {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut env = BTreeMap::new();
        for key in ["HOME", "USERPROFILE", "CODEX_HOME", "CLAUDE_CONFIG_DIR"] {
            env.insert(key.into(), dir.path().to_string_lossy().into_owned());
        }
        Self {
            dir,
            port: port(),
            env,
            child: None,
        }
    }
    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
    pub fn write(&self, name: &str, value: impl AsRef<[u8]>) {
        let path = self.path(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap()).unwrap();
        file.write_all(value.as_ref()).unwrap();
        file.persist(path).unwrap();
    }
    pub fn config(&self, body: &str) {
        self.write(
            "config.yaml",
            format!(
                "listen_port: {}\nrequest_timeout_seconds: 3\n{body}",
                self.port
            ),
        );
    }
    pub fn command(&self) -> Command {
        let mut cmd = Command::new(binary());
        cmd.args(["--config"])
            .arg(self.path("config.yaml"))
            .envs(&self.env)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd
    }
    pub fn restart(&mut self) {
        self.stop();
        self.child = Some(self.command().spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            assert!(
                self.child.as_mut().unwrap().try_wait().unwrap().is_none(),
                "coport exited before becoming ready"
            );
            if TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                assert_eq!(self.request(None, None, "/health", "GET").0, 200);
                break;
            }
            assert!(Instant::now() < deadline, "listener did not start");
            thread::sleep(Duration::from_millis(30));
        }
    }
    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            if child.try_wait().unwrap().is_none() {
                #[cfg(unix)]
                unsafe {
                    libc::kill(child.id() as i32, libc::SIGTERM);
                }
                #[cfg(not(unix))]
                child.kill().unwrap();
                let deadline = Instant::now() + Duration::from_secs(5);
                while child.try_wait().unwrap().is_none() {
                    if Instant::now() > deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!("coport did not stop");
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }
    pub fn pid(&self) -> u32 {
        self.child.as_ref().unwrap().id()
    }
    pub fn request(
        &self,
        token: Option<&str>,
        account: Option<&str>,
        path: &str,
        method: &str,
    ) -> (u16, Vec<u8>) {
        request(self.port, token, account, path, method)
    }
    pub fn status(&self, token: Option<&str>) -> u16 {
        self.request(token, None, "/responses", "POST").0
    }
    pub fn log(&self) -> String {
        fs::read_to_string(self.path("logs/proxy.log")).unwrap()
    }
    pub fn records(&self) -> Vec<Value> {
        self.log()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect()
    }
}
impl Drop for App {
    fn drop(&mut self) {
        self.stop();
    }
}

pub struct Probe {
    pub port: u16,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    stop: Arc<AtomicBool>,
    task: Option<thread::JoinHandle<()>>,
}
impl Probe {
    pub fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(vec![]));
        let saved = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let task = thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_secs(3)))
                            .unwrap();
                        let data = read_head(&mut stream);
                        saved.lock().unwrap().push(data);
                        let _ = stream.write_all(b"HTTP/1.1 502 Probe Complete\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(e) => panic!("{e}"),
                }
            }
        });
        Self {
            port,
            requests,
            stop,
            task: Some(task),
        }
    }
    pub fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
    pub fn last(&self) -> Vec<u8> {
        self.requests.lock().unwrap().last().unwrap().clone()
    }
    pub fn reaches(
        &self,
        app: &App,
        token: Option<&str>,
        account: Option<&str>,
        path: &str,
        host: &str,
    ) {
        let before = self.count();
        assert_eq!(app.request(token, account, path, "POST").0, 502);
        assert!(self.count() > before);
        let last = self.last();
        assert!(
            last.starts_with(format!("CONNECT {host}:443 ").as_bytes()),
            "{}",
            String::from_utf8_lossy(&last)
        );
        if let Some(token) = token {
            assert!(!String::from_utf8_lossy(&last).contains(token));
        }
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(task) = self.task.take() {
            let result = task.join();
            if !thread::panicking() {
                result.unwrap();
            }
        }
    }
}
