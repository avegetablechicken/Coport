mod common;
use base64::{Engine, engine::general_purpose::STANDARD};
use common::App;
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
const USER: &str = "test@user";
const PASSWORD: &str = "p:ss@word";
fn bytes(stream: &mut TcpStream, count: usize) -> Vec<u8> {
    let mut data = vec![0; count];
    stream.read_exact(&mut data).unwrap();
    data
}
#[derive(Default)]
struct Results {
    credentials: Vec<(String, String)>,
    authorized: bool,
}
struct AuthProbe {
    port: u16,
    results: Arc<Mutex<Results>>,
    stop: Arc<AtomicBool>,
    task: Option<thread::JoinHandle<()>>,
}
impl AuthProbe {
    fn new(scheme: &'static str, expected: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let results = Arc::new(Mutex::new(Results::default()));
        let saved = results.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let task = thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                let mut stream = match listener.accept() {
                    Ok((s, _)) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(4)))
                    .unwrap();
                // The proxy's client may open a connection and close or reset
                // it unused; only connections that send a request are checked.
                if !matches!(stream.peek(&mut [0]), Ok(1..)) {
                    continue;
                }
                if scheme == "socks5" {
                    let head = bytes(&mut stream, 2);
                    assert_eq!(head[0], 5);
                    assert!(bytes(&mut stream, head[1] as usize).contains(&2));
                    stream.write_all(&[5, 2]).unwrap();
                    let head = bytes(&mut stream, 2);
                    assert_eq!(head[0], 1);
                    let user = String::from_utf8(bytes(&mut stream, head[1] as usize)).unwrap();
                    let len = bytes(&mut stream, 1)[0];
                    let password = String::from_utf8(bytes(&mut stream, len as usize)).unwrap();
                    let good = user == USER && password == expected;
                    saved.lock().unwrap().credentials.push((user, password));
                    stream.write_all(&[1, if good { 0 } else { 1 }]).unwrap();
                    if good {
                        let head = bytes(&mut stream, 4);
                        assert_eq!(&head[..3], &[5, 1, 0]);
                        match head[3] {
                            3 => {
                                let len = bytes(&mut stream, 1)[0];
                                assert_eq!(bytes(&mut stream, len as usize), b"api.openai.com");
                            }
                            1 => {
                                bytes(&mut stream, 4);
                            }
                            4 => {
                                bytes(&mut stream, 16);
                            }
                            _ => panic!("invalid address type"),
                        }
                        assert_eq!(bytes(&mut stream, 2), 443u16.to_be_bytes());
                        saved.lock().unwrap().authorized = true;
                        stream.write_all(&[5, 4, 0, 1, 0, 0, 0, 0, 0, 0]).unwrap();
                    }
                } else {
                    let head = String::from_utf8(common::read_head(&mut stream)).unwrap();
                    assert!(head.starts_with("CONNECT api.openai.com:443 "));
                    assert!(!head.contains("model-secret"));
                    let auth = head
                        .lines()
                        .filter_map(|line| line.split_once(':'))
                        .find(|(k, _)| k.eq_ignore_ascii_case("proxy-authorization"))
                        .map(|(_, v)| v.trim())
                        .unwrap_or("");
                    let good =
                        auth == format!("Basic {}", STANDARD.encode(format!("{USER}:{expected}")));
                    if !auth.is_empty() {
                        let decoded = String::from_utf8(
                            STANDARD
                                .decode(auth.strip_prefix("Basic ").unwrap())
                                .unwrap(),
                        )
                        .unwrap();
                        let (user, password) = decoded.split_once(':').unwrap();
                        saved
                            .lock()
                            .unwrap()
                            .credentials
                            .push((user.into(), password.into()));
                    }
                    if good {
                        saved.lock().unwrap().authorized = true;
                        stream.write_all(b"HTTP/1.1 502 Probe Complete\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    } else {
                        stream.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"probe\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    }
                }
            }
        });
        Self {
            port,
            results,
            stop,
            task: Some(task),
        }
    }
}
impl Drop for AuthProbe {
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
fn encoded(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}
fn exercise(scheme: &'static str, password: &str, expected: &'static str) {
    let probe = AuthProbe::new(scheme, expected);
    let mut app = App::new();
    let endpoint = format!(
        "{scheme}://{}:{}@127.0.0.1:{}",
        encoded(USER),
        encoded(password),
        probe.port
    );
    app.config(&format!("proxies:\n  authenticated: \"{endpoint}\"\ncodex:\n  routing:\n    api_key_fallback: authenticated\n"));
    app.restart();
    assert_eq!(
        app.request(Some("model-secret"), None, "/v1/models", "GET")
            .0,
        502
    );
    {
        let result = probe.results.lock().unwrap();
        assert!(result.credentials.contains(&(USER.into(), password.into())));
        assert_eq!(result.authorized, password == expected);
    }
    app.stop();
    let log = app.log();
    for secret in [
        USER.to_owned(),
        PASSWORD.to_owned(),
        encoded(USER),
        encoded(password),
        "model-secret".into(),
        STANDARD.encode(format!("{USER}:{password}")),
    ] {
        assert!(
            secret.is_empty() || !log.contains(&secret),
            "credentials leaked into logs"
        );
    }
    let records = app.records();
    let route = records
        .iter()
        .rev()
        .find(|r| r["event"] == "route_selected")
        .unwrap();
    assert_eq!(
        route["proxy_endpoint"],
        format!("{scheme}://127.0.0.1:{}", probe.port)
    );
}
#[test]
fn http_valid_password() {
    exercise("http", PASSWORD, PASSWORD);
}
#[test]
fn http_invalid_password() {
    exercise("http", "wrong-password", PASSWORD);
}
#[test]
fn http_empty_password() {
    exercise("http", "", "");
}
#[test]
fn socks_valid_password() {
    exercise("socks5", PASSWORD, PASSWORD);
}
#[test]
fn socks_invalid_password() {
    exercise("socks5", "wrong-password", PASSWORD);
}
