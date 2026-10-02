use super::*;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, mpsc};
use tokio_rustls::{TlsAcceptor, rustls};

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
type TestIo = Box<dyn Io>;

struct Fixture {
    addr: std::net::SocketAddr,
    certificate: reqwest::Certificate,
    requests: mpsc::UnboundedReceiver<String>,
    release: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn read_request(io: &mut TestIo) -> std::io::Result<String> {
    let mut data = Vec::new();
    while !data.ends_with(b"\r\n\r\n") {
        data.push(io.read_u8().await?);
        assert!(data.len() < 65536);
    }
    let head = String::from_utf8(data).unwrap();
    let length = head
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|s| s.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    let mut body = vec![0; length];
    io.read_exact(&mut body).await?;
    // Compressed model request bodies are binary.
    Ok(head + &String::from_utf8_lossy(&body))
}
async fn fixture(mode: &'static str, response_mode: &'static str) -> Fixture {
    let certified = rcgen::generate_simple_self_signed(vec![
        "upstream.invalid".into(),
        "developers.openai.com".into(),
        "auth.openai.com".into(),
        "platform.claude.com".into(),
        "api.anthropic.com".into(),
        "localhost".into(),
    ])
    .unwrap();
    let cert = certified.cert.der().clone();
    let certificate = reqwest::Certificate::from_der(cert.as_ref()).unwrap();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
    let tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key.into())
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(tls));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, requests) = mpsc::unbounded_channel();
    let release = Arc::new(Notify::new());
    let ready = release.clone();
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let task = tokio::spawn(async move {
        let mut children = tokio::task::JoinSet::new();
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let acceptor = acceptor.clone();
            let tx = tx.clone();
            let ready = ready.clone();
            let attempts = attempts.clone();
            children.spawn(async move {
                let mut io:TestIo=Box::new(socket);
                if mode=="https" { io=Box::new(acceptor.accept(io).await.unwrap()); }
                if mode!="direct" {
                    let connect=read_request(&mut io).await.unwrap(); assert!(connect.starts_with("CONNECT ")); assert!(!connect.contains("model-secret"));
                    tx.send(connect).unwrap();
                    io.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await.unwrap();
                }
                let Ok(tls)=acceptor.accept(io).await else { return; }; let mut io:TestIo=Box::new(tls);
                let request=read_request(&mut io).await.unwrap(); let head=request.starts_with("HEAD "); let profile=request.starts_with("GET /api/oauth/profile "); tx.send(request.clone()).unwrap();
                let attempt = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if response_mode == "drop_always" || (response_mode == "drop_twice" && attempt < 2) {
                    return;
                }
                if response_mode == "slow_drop_then_delay" {
                    if attempt == 0 {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    } else {
                        let _ = io.read_u8().await;
                    }
                    return;
                }
                if head || (response_mode == "drop_twice" && !profile) {
                    io.write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n").await.unwrap();
                }
                else if let Some(status) = response_mode.strip_prefix("status_") {
                    io.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Length: 4\r\nRetry-After: 120\r\nConnection: close\r\n\r\noops").as_bytes()).await.unwrap();
                }
                else if profile {
                    let (status, body) = match response_mode {
                        "profile_unauthorized" => (401, "{}"),
                        "profile_invalid" => (200, "{}"),
                        "profile_redirect" => (302, "{}"),
                        _ => (200, r#"{"account":{"uuid":"remote-account","email":"remote@example.invalid"}}"#),
                    };
                    io.write_all(format!("HTTP/1.1 {status} Profile\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
                }
                else if response_mode=="websocket_calls" {
                    io.write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n").await.unwrap();
                    for id in ["turn-one", "turn-two"] {
                        let payload = read_test_ws_message(&mut io, true).await;
                        let event: serde_json::Value = serde_json::from_slice(&payload).unwrap();
                        assert_eq!(event["type"], "response.create");
                        let message = json!({"type":"response.completed", "response":{"id":id,"model":"test-model","usage":{"input_tokens":10,"output_tokens":4}}}).to_string();
                        io.write_all(&test_ws_message(message.as_bytes(), false)).await.unwrap();
                    }
                    let _ = io.read_u8().await;
                }
                else if response_mode=="websocket" || response_mode=="websocket_bad" {
                    let accept = if response_mode=="websocket_bad" { "invalid" } else { "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=" };
                    io.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\nSec-WebSocket-Protocol: test\r\n\r\n").as_bytes()).await.unwrap();
                    // A text frame coalesced with the upgrade must survive both parsers.
                    io.write_all(b"\x81\x02hi").await.unwrap();
                    let mut frame = [0; 8];
                    if io.read_exact(&mut frame).await.is_ok() {
                        assert_eq!(&frame, b"\x81\x82\x01\x02\x03\x04ni");
                        io.write_all(b"\x81\x02ok\x88\x00").await.unwrap();
                        io.shutdown().await.unwrap();
                    }
                }
                else if response_mode=="oauth_json" {
                    let body = if request.starts_with("POST /v1/oauth/token ") {
                        r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#
                    } else {
                        r#"{"five_hour":{"utilization":42},"seven_day":{"utilization":12},"extra_usage":{"is_enabled":true}}"#
                    };
                    io.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nRetry-After: 120\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                }
                else if response_mode=="delayed_headers" {
                    let _ = io.read_u8().await;
                    let _ = tx.send("DISCONNECTED".into());
                }
                else if response_mode=="zstd_sse" || response_mode=="untyped_sse" {
                    // ChatGPT-style responses: compressed, or an event stream without its content type.
                    let (head, body) = model_stream_body(response_mode);
                    io.write_all(format!("HTTP/1.1 200 OK\r\n{head}Content-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
                    io.write_all(&body).await.unwrap();
                }
                else if response_mode=="redirect" { io.write_all(b"HTTP/1.1 302 Found\r\nLocation: https://evil.invalid/leak\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap(); }
                else {
                    io.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close, x-hop\r\nx-hop: remove-me\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\n\r\nD\r\ndata: first\n\n\r\n").await.unwrap(); io.flush().await.unwrap();
                    tokio::select! {
                        _=ready.notified()=>{},
                        _=io.read_u8()=>{ let _=tx.send("DISCONNECTED".into()); return; }
                    }
                    let _=io.write_all(b"C\r\ndata: last\n\n\r\n0\r\n\r\n").await;
                }
            });
        }
    });
    Fixture {
        addr,
        certificate,
        requests,
        release,
        task,
    }
}
struct Running {
    url: String,
    server: Arc<Server>,
    task: tokio::task::JoinHandle<()>,
    _temp: tempfile::TempDir,
}
impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn running(config: &str) -> Running {
    let temp = tempfile::tempdir().unwrap();
    let logger = Arc::new(Logger::new(temp.path().join("proxy.log")));
    let mut config = Config::parse(&format!(
        "listen_port: 7889\nrequest_timeout_seconds: 3\n{config}"
    ))
    .unwrap();
    // Default login directories stand in for empty ones, never the developer's own.
    let isolated = |name: &str| vec![temp.path().join(name).to_string_lossy().into_owned()];
    if config.claude.config_dirs == crate::config::default_claude_config_dirs() {
        config.claude.config_dirs = isolated("claude");
    }
    if config.codex.homes == crate::config::default_codex_homes() {
        config.codex.homes = isolated("codex");
    }
    let server = Arc::new(Server::new(config, logger));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(
        server
            .clone()
            .serve(listener, std::future::pending())
            .map(|r| r.unwrap()),
    );
    Running {
        url,
        server,
        task,
        _temp: temp,
    }
}
use futures_util::FutureExt;
#[tokio::test]
async fn codex_url_routes_stream_through_declared_transport_without_credential_lookup() {
    for mode in ["direct", "http"] {
        let mut fixture = fixture(mode, "sse").await;
        let endpoint = if mode == "direct" {
            "none".into()
        } else {
            format!("http://127.0.0.1:{}", fixture.addr.port())
        };
        let running = running(&format!("proxies:\n  selected: {endpoint}\ncodex:\n  routing:\n    api_key:\n      'upstream.invalid/v1': selected\n")).await;
        trust(&running, &fixture, &endpoint);
        let mut response = http()
            .post(format!(
                "{}/codex/https://upstream.invalid/v1/responses?stream=true",
                running.url
            ))
            .bearer_auth("url-route-secret")
            .header("cookie", "private-cookie")
            .body("model-body")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        if mode != "direct" {
            let connect = fixture.requests.recv().await.unwrap();
            assert!(connect.starts_with("CONNECT upstream.invalid:443"));
            assert!(!connect.contains("url-route-secret"));
        }
        let request = fixture.requests.recv().await.unwrap();
        assert!(request.starts_with("POST /v1/responses?stream=true HTTP/1.1"));
        assert!(request.contains("authorization: Bearer url-route-secret"));
        assert!(!request.contains("private-cookie"));
        assert!(request.ends_with("model-body"));
        assert_eq!(response.chunk().await.unwrap().unwrap(), "data: first\n\n");
        fixture.release.notify_one();
        assert_eq!(response.text().await.unwrap(), "data: last\n\n");
        assert!(fixture.requests.try_recv().is_err());
        running.server.logger.flush().unwrap();
        let log = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
        assert!(!log.contains("url-route-secret"));
        assert!(
            log.lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .any(|row| row["upstream_base_url"] == "https://upstream.invalid/v1")
        );
    }
}

#[tokio::test]
async fn codex_token_refresh_uses_saved_account_proxy_without_injecting_credentials() {
    let mut fixture = fixture("http", "sse").await;
    let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("auth.json"),
        r#"{"tokens":{"account_id":"acct-1","access_token":"access-secret","refresh_token":"refresh-secret"}}"#,
    )
    .unwrap();
    let running = running(&format!(
        "proxies:\n  selected: {endpoint}\ncodex:\n  homes: [{}]\n  routing:\n    account:\n      acct-1: selected\n",
        serde_json::to_string(&home.path()).unwrap()
    ))
    .await;
    trust(&running, &fixture, &endpoint);
    let body =
        r#"{"client_id":"app","grant_type":"refresh_token","refresh_token":"refresh-secret"}"#;
    for path in ["/https://auth.openai.com/oauth/token", "/oauth/token"] {
        let response = http()
            .post(format!("{}{path}", running.url))
            .header("content-type", "application/json")
            .header("cookie", "private-cookie")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let connect = fixture.requests.recv().await.unwrap();
        assert!(connect.starts_with("CONNECT auth.openai.com:443"));
        assert!(!connect.contains("refresh-secret"));
        let request = fixture.requests.recv().await.unwrap();
        assert!(request.starts_with("POST /oauth/token HTTP/1.1"));
        assert!(request.contains("host: auth.openai.com"));
        assert!(!request.to_lowercase().contains("authorization"));
        assert!(!request.contains("access-secret"));
        assert!(!request.contains("private-cookie"));
        // Without a client Accept-Encoding the proxy adds none of its own.
        assert!(!request.to_lowercase().contains("accept-encoding"));
        assert!(request.ends_with(body));
        fixture.release.notify_one();
        assert_eq!(
            response.text().await.unwrap(),
            "data: first\n\ndata: last\n\n"
        );
    }
    for (method, body, status) in [
        ("GET", "", 405),
        ("POST", "{}", 400),
        ("POST", r#"{"refresh_token":"unknown"}"#, 403),
    ] {
        let response = http()
            .request(
                method.parse().unwrap(),
                format!("{}/oauth/token", running.url),
            )
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{method} {body}");
    }
    assert!(fixture.requests.try_recv().is_err());
    running.server.logger.flush().unwrap();
    let log = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
    assert!(log.contains("\"account_id\":\"acct-1\""));
    assert!(!log.contains("refresh-secret"));
}

#[tokio::test]
async fn claude_token_refresh_uses_saved_account_proxy_without_injecting_credentials() {
    let mut fixture = fixture("http", "sse").await;
    let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join(".credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"access-secret","refreshToken":"refresh-secret"}}"#,
    )
    .unwrap();
    let running = running(&format!(
        "proxies:\n  selected: {endpoint}\nclaude:\n  config_dirs: [{}]\n  routing:\n    account_fallback: selected\n",
        serde_json::to_string(&home.path()).unwrap()
    ))
    .await;
    trust(&running, &fixture, &endpoint);
    let body =
        r#"{"client_id":"app","grant_type":"refresh_token","refresh_token":"refresh-secret"}"#;
    for path in [
        "/https://platform.claude.com/v1/oauth/token",
        "/v1/oauth/token",
        "/anthropic/v1/oauth/token",
        "/claude/v1/oauth/token",
        "/anthropic/https://platform.claude.com/v1/oauth/token",
    ] {
        let response = http()
            .post(format!("{}{path}", running.url))
            .header("content-type", "application/json")
            .header("cookie", "private-cookie")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let connect = fixture.requests.recv().await.unwrap();
        assert!(connect.starts_with("CONNECT platform.claude.com:443"));
        assert!(!connect.contains("refresh-secret"));
        let request = fixture.requests.recv().await.unwrap();
        assert!(request.starts_with("POST /v1/oauth/token HTTP/1.1"));
        assert!(request.contains("host: platform.claude.com"));
        assert!(!request.to_lowercase().contains("authorization"));
        assert!(!request.contains("access-secret"));
        assert!(!request.contains("private-cookie"));
        assert!(request.ends_with(body));
        fixture.release.notify_one();
        assert_eq!(
            response.text().await.unwrap(),
            "data: first\n\ndata: last\n\n"
        );
    }
    for (method, body, status) in [
        ("GET", "", 405),
        ("POST", "{}", 400),
        ("POST", r#"{"refresh_token":"unknown"}"#, 403),
    ] {
        let response = http()
            .request(
                method.parse().unwrap(),
                format!("{}/anthropic/v1/oauth/token", running.url),
            )
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{method} {body}");
    }
    assert!(fixture.requests.try_recv().is_err());
    running.server.logger.flush().unwrap();
    let log = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
    assert!(log.contains("claude_auth"));
    assert!(!log.contains("refresh-secret"));
}

#[tokio::test]
async fn application_namespaces_disambiguate_shared_api_upstreams() {
    let mut codex = fixture("http", "redirect").await;
    let mut claude = fixture("http", "redirect").await;
    let codex_endpoint = format!("http://127.0.0.1:{}", codex.addr.port());
    let claude_endpoint = format!("http://127.0.0.1:{}", claude.addr.port());
    let running = running(&format!("proxies:\n  codex: {codex_endpoint}\n  claude: {claude_endpoint}\ncodex:\n  routing:\n    api_key:\n      'https://upstream.invalid/v1': codex\nclaude:\n  routing:\n    api_key:\n      'https://upstream.invalid/v1': claude\n")).await;
    trust(&running, &codex, &codex_endpoint);
    trust(&running, &claude, &claude_endpoint);
    let response = http()
        .get(format!(
            "{}/https://upstream.invalid/v1/models",
            running.url
        ))
        .bearer_auth("key")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    assert!(codex.requests.try_recv().is_err());
    assert!(claude.requests.try_recv().is_err());
    let response = http()
        .post(format!(
            "{}/codex/https://upstream.invalid/v1/responses",
            running.url
        ))
        .bearer_auth("key")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 302);
    assert!(codex.requests.recv().await.unwrap().starts_with("CONNECT "));
    assert!(
        codex
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("POST /v1/responses ")
    );
    assert!(claude.requests.try_recv().is_err());
    let response = http()
        .post(format!(
            "{}/anthropic/https://upstream.invalid/v1/messages",
            running.url
        ))
        .bearer_auth("key")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 302);
    assert!(
        claude
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("CONNECT ")
    );
    assert!(
        claude
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("POST /v1/messages ")
    );
    assert!(codex.requests.try_recv().is_err());
}
fn trust(running: &Running, fixture: &Fixture, endpoint: &str) {
    for native_tls in [false, true] {
        let mut client = reqwest::Client::builder()
            .no_proxy()
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(3))
            .add_root_certificate(fixture.certificate.clone());
        client = if native_tls {
            client.use_native_tls()
        } else {
            client.use_rustls_tls()
        };
        if endpoint == "none" {
            client = client
                .resolve("upstream.invalid", fixture.addr)
                .resolve("developers.openai.com", fixture.addr);
        } else {
            client = client.proxy(reqwest::Proxy::all(endpoint).unwrap());
        }
        let key = if native_tls {
            format!("native-tls:{endpoint}")
        } else {
            endpoint.into()
        };
        running
            .server
            .clients
            .lock()
            .unwrap()
            .insert(key, client.build().unwrap());
    }
}
fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

// Synthetic upstream.invalid traffic stays entirely on loopback. No Anthropic
// host, real credential, or external proxy is contacted by this fixture.
#[tokio::test]
async fn claude_native_auth_paths_and_sse_passthrough() {
    for bearer in [false, true] {
        let mut fixture = fixture("http", "sse").await;
        let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
        let credentials = tempfile::tempdir().unwrap();
        let file = credentials.path().join(".credentials.json");
        std::fs::write(&file, r#"{"claudeAiOauth":{"accessToken":"model-secret"}}"#).unwrap();
        let dir = serde_json::to_string(&credentials.path()).unwrap();
        let running = running(&format!("proxies:\n  selected: {endpoint}\nclaude:\n  config_dirs: [{dir}]\n  base_url: https://upstream.invalid\n  routing:\n    account_fallback: selected\n    api_key_fallback: selected\n")).await;
        trust(&running, &fixture, &endpoint);
        let name = if bearer { "authorization" } else { "x-api-key" };
        let value = if bearer {
            "Bearer model-secret"
        } else {
            "model-secret"
        };
        let mut response = http()
            .post(format!("{}/anthropic/v1/messages?beta=true", running.url))
            .header(name, value)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "custom-beta")
            .header("cookie", "private-cookie")
            .header("chatgpt-account-id", "private-account")
            .body(r#"{"model":"test-model","stream":true}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert!(
            fixture
                .requests
                .recv()
                .await
                .unwrap()
                .starts_with("CONNECT upstream.invalid:443")
        );
        let request = fixture.requests.recv().await.unwrap();
        assert!(request.starts_with("POST /v1/messages?beta=true HTTP/1.1"));
        assert!(request.contains(&format!("{name}: {value}\r\n")));
        assert!(!request.contains(if bearer {
            "x-api-key:"
        } else {
            "authorization:"
        }));
        assert!(request.contains(if bearer {
            "anthropic-beta: custom-beta,oauth-2025-04-20"
        } else {
            "anthropic-beta: custom-beta\r\n"
        }));
        assert!(!request.contains("private-cookie"));
        assert!(!request.contains("private-account"));
        assert!(request.ends_with(r#"{"model":"test-model","stream":true}"#));
        assert_eq!(response.chunk().await.unwrap().unwrap(), "data: first\n\n");
        fixture.release.notify_one();
        assert_eq!(response.text().await.unwrap(), "data: last\n\n");
        running.server.logger.flush().unwrap();
        let log = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
        assert!(log.contains("claude"));
        assert!(!log.contains("model-secret"));
    }
}

#[tokio::test]
async fn claude_third_party_explicit_url_streams_directly_without_oauth_lookup_or_fallback() {
    for bearer in [false, true] {
        let mut fixture = fixture("direct", "sse").await;
        let running = running("claude:\n  account_auth_file_only: true\n  routing:\n    api_key:\n      'upstream.invalid': none\n").await;
        trust(&running, &fixture, "none");
        let mut response = http()
            .post(format!(
                "{}/https://upstream.invalid/v1/messages?beta=true",
                running.url
            ))
            .header(
                if bearer { "authorization" } else { "x-api-key" },
                if bearer {
                    "Bearer api-secret"
                } else {
                    "api-secret"
                },
            )
            .body("model-body")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let request = fixture.requests.recv().await.unwrap();
        assert!(request.starts_with("POST /v1/messages?beta=true HTTP/1.1"));
        assert!(request.ends_with("model-body"));
        assert!(!request.contains("oauth-2025-04-20"));
        assert_eq!(response.chunk().await.unwrap().unwrap(), "data: first\n\n");
        fixture.release.notify_one();
        assert_eq!(response.text().await.unwrap(), "data: last\n\n");
        assert!(fixture.requests.try_recv().is_err());
        assert!(running.server.claude_profiles.lock().unwrap().is_empty());
    }
    let mut fixture = fixture("direct", "redirect").await;
    let running =
        running("claude:\n  routing:\n    api_key:\n      'upstream.invalid': none\n").await;
    trust(&running, &fixture, "none");
    let response = http()
        .get(format!(
            "{}/https://upstream.invalid/v1/models",
            running.url
        ))
        .bearer_auth("api-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 302);
    assert!(
        fixture
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("GET /v1/models HTTP/1.1")
    );
}

#[tokio::test]
async fn claude_usage_requires_saved_account_and_cannot_use_openai_fallback() {
    // A readable saved login that differs from the request token, independent
    // of the machine's own ~/.claude.
    let login = tempfile::tempdir().unwrap();
    std::fs::write(
        login.path().join(".credentials.json"),
        r#"{"claudeAiOauth": {"accessToken": "saved-secret"}}"#,
    )
    .unwrap();
    let running = running(&format!(
        "claude:\n  config_dirs: [{:?}]\n  routing:\n    account_fallback: none\n    api_key_fallback: none\ncodex:\n  routing:\n    api_key_fallback: none\n",
        login.path().to_string_lossy()
    ))
    .await;
    for path in [
        "/anthropic/api/oauth/usage",
        "/anthropic/api/oauth/%75sage",
        "/anthropic/api/oauth/usage/",
        "/api/oauth/usage",
        "/https://api.anthropic.com/api/oauth/usage",
    ] {
        let response = http()
            .get(format!("{}{path}", running.url))
            .bearer_auth("unmatched-secret")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
    }
    let running = self::running("codex:\n  routing:\n    api_key_fallback: none\n").await;
    let response = http()
        .post(format!("{}/v1/messages", running.url))
        .bearer_auth("openai-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn traffic_labels_probe_missing_claude_metadata_and_cache_failures_safely() {
    for (behavior, succeeds) in [("sse", true), ("profile_unauthorized", false)] {
        let mut lookup = fixture("http", behavior).await;
        let endpoint = format!("http://127.0.0.1:{}", lookup.addr.port());
        let dir = tempfile::tempdir().unwrap();
        let credentials = dir.path().join(".credentials.json");
        let saved = r#"{"claudeAiOauth":{"accessToken":"saved-secret"}}"#;
        std::fs::write(&credentials, saved).unwrap();
        let running = running(&format!("proxies:\n  lookup: {endpoint}\nclaude:\n  config_dirs: [{}]\n  base_url: https://upstream.invalid\n  routing:\n    account:\n      remote@example.invalid: none\n    account_probe: lookup\n",serde_json::to_string(dir.path()).unwrap())).await;
        trust(&running, &lookup, &endpoint);
        let (first, second) = tokio::join!(
            running.server.traffic_credential_labels(),
            running.server.traffic_credential_labels()
        );
        assert_eq!(first, second);
        assert_eq!(
            first
                .get(&("Claude".into(), "remote-account".into()))
                .map(String::as_str),
            if succeeds {
                Some("remote@example.invalid")
            } else {
                None
            }
        );
        assert!(
            lookup
                .requests
                .recv()
                .await
                .unwrap()
                .starts_with("CONNECT ")
        );
        assert!(
            lookup
                .requests
                .recv()
                .await
                .unwrap()
                .starts_with("GET /api/oauth/profile ")
        );
        assert!(lookup.requests.try_recv().is_err());
        assert_eq!(std::fs::read_to_string(&credentials).unwrap(), saved);
        let checks = running.server.account_checks.lock().await;
        let before = checks.values().next().unwrap().0;
        drop(checks);
        assert_eq!(running.server.traffic_credential_labels().await, first);
        assert_eq!(
            running
                .server
                .account_checks
                .lock()
                .await
                .values()
                .next()
                .unwrap()
                .0,
            before
        );
    }
}

#[tokio::test]
async fn traffic_labels_do_not_probe_without_an_explicit_lookup_route() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"saved-secret"}}"#,
    )
    .unwrap();
    let running = running(&format!("claude:\n  config_dirs: [{}]\n  base_url: https://upstream.invalid\n  routing:\n    account: {{'remote@example.invalid': none}}\n",serde_json::to_string(dir.path()).unwrap())).await;
    let labels = running.server.traffic_credential_labels().await;
    assert!(!labels.contains_key(&("Claude".into(), "remote-account".into())));
    assert!(running.server.claude_profiles.lock().unwrap().is_empty());
}

#[tokio::test]
async fn account_status_probes_remote_identity_and_preserves_warning_state() {
    for (behavior, expected) in [("sse", "remote"), ("profile_unauthorized", "probe_failed")] {
        let mut lookup = fixture("http", behavior).await;
        let endpoint = format!("http://127.0.0.1:{}", lookup.addr.port());
        let dir = tempfile::tempdir().unwrap();
        let credentials = dir.path().join(".credentials.json");
        std::fs::write(
            &credentials,
            r#"{"claudeAiOauth":{"accessToken":"saved-secret"}}"#,
        )
        .unwrap();
        let running = running(&format!("proxies:\n  lookup: {endpoint}\nclaude:\n  config_dirs: [{}]\n  base_url: https://upstream.invalid\n  routing:\n    account:\n      remote@example.invalid: none\n      other@example.invalid: none\n    account_probe: lookup\n", serde_json::to_string(dir.path()).unwrap())).await;
        trust(&running, &lookup, &endpoint);
        let states = running.server.account_route_states().await;
        assert_eq!(states[1]["remote@example.invalid"], expected);
        assert_eq!(
            states[1]["other@example.invalid"],
            if expected == "remote" {
                "inactive"
            } else {
                "probe_failed"
            }
        );
        assert!(
            lookup
                .requests
                .recv()
                .await
                .unwrap()
                .starts_with("CONNECT ")
        );
        let request = lookup.requests.recv().await.unwrap();
        assert!(request.starts_with("GET /api/oauth/profile "));
        assert!(request.contains("authorization: Bearer saved-secret"));
        // Both successes and failures are cached to avoid polling storms.
        assert_eq!(running.server.account_route_states().await, states);
        assert!(lookup.requests.try_recv().is_err());
        for (at, _) in running.server.account_checks.lock().await.values_mut() {
            *at = Instant::now() - Duration::from_secs(31);
        }
        assert_eq!(running.server.account_route_states().await, states);
        assert!(
            lookup
                .requests
                .recv()
                .await
                .unwrap()
                .starts_with("CONNECT ")
        );
        assert!(
            lookup
                .requests
                .recv()
                .await
                .unwrap()
                .starts_with("GET /api/oauth/profile ")
        );
        // Metadata that no longer matches a route still uses the saved token's probe.
        std::fs::write(
            dir.path().join(".claude.json"),
            r#"{"oauthAccount":{"accountUuid":"stale","emailAddress":"stale@example.invalid"}}"#,
        )
        .unwrap();
        assert_eq!(running.server.account_route_states().await, states);
        // A changed credential cannot reuse another token's probe result.
        std::fs::write(
            &credentials,
            r#"{"claudeAiOauth":{"accessToken":"replacement-secret"}}"#,
        )
        .unwrap();
        assert_eq!(
            running.server.account_route_states().await[1]["remote@example.invalid"],
            expected
        );
        assert!(
            lookup
                .requests
                .recv()
                .await
                .unwrap()
                .starts_with("CONNECT ")
        );
        assert!(
            lookup
                .requests
                .recv()
                .await
                .unwrap()
                .contains("authorization: Bearer replacement-secret")
        );
        // Local matches take precedence, even with a failed cached remote check.
        std::fs::write(
            dir.path().join(".claude.json"),
            r#"{"oauthAccount":{"accountUuid":"local","emailAddress":"remote@example.invalid"}}"#,
        )
        .unwrap();
        assert_eq!(
            running.server.account_route_states().await[1]["remote@example.invalid"],
            "active"
        );
        assert!(lookup.requests.try_recv().is_err());
        std::fs::remove_file(&credentials).unwrap();
        assert_eq!(
            running.server.account_route_states().await[1]["remote@example.invalid"],
            "inactive"
        );
        assert!(lookup.requests.try_recv().is_err());
        running.server.logger.flush().unwrap();
        let log = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
        assert!(!log.contains("saved-secret") && !log.contains("replacement-secret"));
    }
}

#[tokio::test]
async fn claude_saved_token_without_metadata_probes_then_routes_and_caches() {
    for file_only in [false, true] {
        let mut lookup = fixture("http", "sse").await;
        let mut payload = fixture("http", "redirect").await;
        let lookup_endpoint = format!("http://127.0.0.1:{}", lookup.addr.port());
        let payload_endpoint = format!("http://127.0.0.1:{}", payload.addr.port());
        let credentials = tempfile::tempdir().unwrap();
        let file = credentials.path().join(".credentials.json");
        std::fs::write(&file, r#"{"claudeAiOauth":{"accessToken":"saved-secret"}}"#).unwrap();
        let dir = serde_json::to_string(&credentials.path()).unwrap();
        let running = running(&format!("proxies:\n  lookup: {lookup_endpoint}\n  selected: {payload_endpoint}\nclaude:\n  config_dirs: [{dir}]\n  account_auth_file_only: {file_only}\n  base_url: https://upstream.invalid\n  routing:\n    account:\n      remote@example.invalid: selected\n    account_probe: lookup\n")).await;
        trust(&running, &lookup, &lookup_endpoint);
        trust(&running, &payload, &payload_endpoint);
        for first in [true, false] {
            let response = http()
                .post(format!("{}/anthropic/v1/messages", running.url))
                .bearer_auth("saved-secret")
                .body("private-payload")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 302);
            if first {
                assert!(
                    lookup
                        .requests
                        .recv()
                        .await
                        .unwrap()
                        .starts_with("CONNECT ")
                );
                let request = lookup.requests.recv().await.unwrap();
                assert!(request.starts_with("GET /api/oauth/profile "));
                assert!(request.contains("authorization: Bearer saved-secret"));
                assert!(!request.contains("private-payload"));
            }
            assert!(lookup.requests.try_recv().is_err());
            assert!(
                payload
                    .requests
                    .recv()
                    .await
                    .unwrap()
                    .starts_with("CONNECT ")
            );
            let request = payload.requests.recv().await.unwrap();
            assert!(request.starts_with("POST /v1/messages "));
            assert!(request.ends_with("private-payload"));
        }
        assert_eq!(running.server.claude_profiles.lock().unwrap().len(), 1);
        if file_only {
            let response = http()
                .post(format!("{}/anthropic/v1/messages", running.url))
                .bearer_auth("unknown-secret")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 401);
            assert!(lookup.requests.try_recv().is_err());
            assert!(payload.requests.try_recv().is_err());
        }
    }
}

#[tokio::test]
async fn claude_other_accounts_lookup_then_route_by_email_and_cache_per_token() {
    let mut lookup = fixture("http", "sse").await;
    let mut payload = fixture("http", "redirect").await;
    let lookup_endpoint = format!("http://127.0.0.1:{}", lookup.addr.port());
    let payload_endpoint = format!("http://127.0.0.1:{}", payload.addr.port());
    let running = running(&format!("proxies:\n  lookup: {lookup_endpoint}\n  selected: {payload_endpoint}\nclaude:\n  account_auth_file_only: false\n  base_url: https://upstream.invalid\n  routing:\n    account:\n      remote@example.invalid: selected\n    account_probe: lookup\n")).await;
    trust(&running, &lookup, &lookup_endpoint);
    trust(&running, &payload, &payload_endpoint);
    for (token, expect_lookup) in [
        ("first-secret", true),
        ("first-secret", false),
        ("second-secret", true),
    ] {
        let response = http()
            .post(format!("{}/anthropic/v1/messages", running.url))
            .bearer_auth(token)
            .header("cookie", "private-cookie")
            .header("anthropic-beta", "private-beta")
            .body("private-payload")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 302);
        if expect_lookup {
            assert!(
                lookup
                    .requests
                    .recv()
                    .await
                    .unwrap()
                    .starts_with("CONNECT upstream.invalid:443")
            );
            let request = lookup.requests.recv().await.unwrap();
            assert!(request.starts_with("GET /api/oauth/profile HTTP/1.1"));
            assert!(request.contains(&format!("authorization: Bearer {token}")));
            assert!(!request.contains("private-cookie"));
            assert!(!request.contains("private-beta"));
            assert!(!request.contains("private-payload"));
        }
        assert!(lookup.requests.try_recv().is_err());
        assert!(
            payload
                .requests
                .recv()
                .await
                .unwrap()
                .starts_with("CONNECT upstream.invalid:443")
        );
        assert!(
            payload
                .requests
                .recv()
                .await
                .unwrap()
                .ends_with("private-payload")
        );
    }
    assert_eq!(running.server.claude_profiles.lock().unwrap().len(), 2);
    running
        .server
        .claude_profiles
        .lock()
        .unwrap()
        .get_mut("first-secret")
        .unwrap()
        .0 = Instant::now()
        // Windows counts `Instant` from boot, so it may not reach back 30 days.
        .checked_sub(Duration::from_secs(30 * 24 * 60 * 60))
        .unwrap_or_else(Instant::now);
    let response = http()
        .post(format!("{}/anthropic/api/oauth/usage", running.url))
        .bearer_auth("first-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 405); // A remotely identified account passes auth, then method validation.
    assert!(lookup.requests.try_recv().is_err());
    assert!(payload.requests.try_recv().is_err());
    running.server.logger.flush().unwrap();
    let log = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
    assert!(log.contains("remote-account"));
    assert!(!log.contains("first-secret"));
    assert!(!log.contains("second-secret"));
}

#[tokio::test]
async fn claude_profile_cache_evicts_only_least_recently_used_identity() {
    let running = running("claude:\n  routing: {}\n").await;
    let identity = crate::claude::ClaudeIdentity::profile(&json!({
        "account": {"uuid": "id", "email": "person@example.invalid"}
    }))
    .unwrap();
    let start = Instant::now();
    for index in 0..128 {
        running
            .server
            .cache_claude_profile(format!("token-{index}"), identity.clone())
            .unwrap();
    }
    {
        // Older than every other entry, without reaching back past boot on Windows.
        let mut cache = running.server.claude_profiles.lock().unwrap();
        cache.get_mut("token-42").unwrap().0 =
            start.checked_sub(Duration::from_secs(1)).unwrap_or(start);
    }
    // Replacing an existing token at capacity must not evict another account.
    running
        .server
        .cache_claude_profile("token-0".into(), identity.clone())
        .unwrap();
    assert_eq!(running.server.claude_profiles.lock().unwrap().len(), 128);
    running
        .server
        .cache_claude_profile("new-token".into(), identity)
        .unwrap();
    let cache = running.server.claude_profiles.lock().unwrap();
    assert_eq!(cache.len(), 128);
    assert!(!cache.contains_key("token-42"));
    assert!(cache.contains_key("new-token"));
    assert!(cache.contains_key("token-0"));
}

#[tokio::test]
async fn claude_profile_failures_never_forward_payload_or_cache_identity() {
    for (mode, expected) in [
        ("profile_unauthorized", 401),
        ("profile_invalid", 502),
        ("profile_redirect", 502),
    ] {
        let mut lookup = fixture("http", mode).await;
        let endpoint = format!("http://127.0.0.1:{}", lookup.addr.port());
        let running = running(&format!("proxies:\n  lookup: {endpoint}\nclaude:\n  account_auth_file_only: false\n  base_url: https://upstream.invalid\n  routing:\n    account_fallback: lookup\n")).await;
        trust(&running, &lookup, &endpoint);
        let response = http()
            .post(format!("{}/anthropic/v1/messages", running.url))
            .bearer_auth("secret")
            .body("private-payload")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        assert!(
            lookup
                .requests
                .recv()
                .await
                .unwrap()
                .starts_with("CONNECT ")
        );
        let request = lookup.requests.recv().await.unwrap();
        assert!(request.starts_with("GET /api/oauth/profile "));
        assert!(!request.contains("private-payload"));
        assert!(lookup.requests.try_recv().is_err());
        assert!(running.server.claude_profiles.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn tls_connect_and_https_connect_stream_before_completion() {
    for mode in ["http", "https"] {
        let mut fixture = fixture(mode, "sse").await;
        let endpoint = format!(
            "{mode}://test%40user:p%3Ass%40word@localhost:{}",
            fixture.addr.port()
        );
        let running=running(&format!("proxies:\n  selected: {endpoint}\ncodex:\n  routing:\n    api_key_fallback: selected\n  base_url:\n    api_key: https://upstream.invalid/v1\n")).await;
        trust(&running, &fixture, &endpoint);
        let response = http()
            .post(format!("{}/v1/responses?private=hidden", running.url))
            .bearer_auth("model-secret")
            .header("connection", "x-hop")
            .header("x-hop", "private-hop")
            .header("cookie", "private-cookie")
            .body("request-body")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert!(!response.headers().contains_key("x-hop"));
        assert_eq!(response.headers().get_all("set-cookie").iter().count(), 2);
        let mut stream = response.bytes_stream();
        let first = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(first, "data: first\n\n");
        fixture.release.notify_one();
        let mut rest = Vec::new();
        while let Some(b) = stream.next().await {
            rest.extend_from_slice(&b.unwrap());
        }
        assert_eq!(rest, b"data: last\n\n");
        let connect = fixture.requests.recv().await.unwrap().to_lowercase();
        assert!(connect.contains("proxy-authorization: basic dgvzd"));
        let request = fixture.requests.recv().await.unwrap().to_lowercase();
        assert!(request.starts_with("post /v1/responses?private=hidden "));
        assert!(request.contains("authorization: bearer model-secret"));
        assert!(request.ends_with("request-body"));
        assert!(!request.contains("private-hop"));
        assert!(!request.contains("private-cookie"));
        assert!(!request.contains("proxy-authorization"));
        running.server.logger.flush().unwrap();
        let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
        for secret in [
            "model-secret",
            "hidden",
            "request-body",
            "test%40user",
            "p%3Ass",
        ] {
            assert!(!raw.contains(secret));
        }
        assert!(raw.contains("request_finished"));
    }
}

#[tokio::test]
async fn ordered_probes_are_credential_free_and_cached() {
    let mut fixture = fixture("direct", "redirect").await;
    let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_port = dead.local_addr().unwrap().port();
    drop(dead);
    let running=running(&format!("proxies:\n  dead: http://127.0.0.1:{dead_port}\ncodex:\n  routing:\n    api_key_fallback: [dead, none]\n  base_url:\n    api_key: https://upstream.invalid/v1\n")).await;
    trust(&running, &fixture, "none");
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    for attempt in 0..2 {
        let response = client
            .post(format!("{}/responses?secret=query", running.url))
            .bearer_auth("model-secret")
            .body("private-body")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 302);
        if attempt == 0 {
            let probe = fixture.requests.recv().await.unwrap().to_lowercase();
            assert!(probe.starts_with("head / http"));
            for s in ["authorization", "private-body", "query", "model-secret"] {
                assert!(!probe.contains(s));
            }
        }
        let request = fixture.requests.recv().await.unwrap();
        assert!(request.starts_with("POST /v1/responses?secret=query "));
    }
    assert!(
        fixture.requests.try_recv().is_err(),
        "redirect must not be followed"
    );
}

#[tokio::test]
async fn mcp_strips_credentials_and_only_forwards_protocol_headers() {
    let mut fixture = fixture("direct", "redirect").await;
    let running = running("codex:\n  routing:\n    mcp_fallback: none\n").await;
    trust(&running, &fixture, "none");
    let response = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .post(format!("{}/mcp/openaiDeveloperDocs?q=1", running.url))
        .bearer_auth("model-secret")
        .header("cookie", "private-cookie")
        .header("x-private", "private")
        .header("mcp-session-id", "session-1")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 302);
    let request = fixture.requests.recv().await.unwrap().to_lowercase();
    assert!(request.starts_with("post /mcp?q=1 "));
    assert!(request.contains("mcp-session-id: session-1"));
    for s in ["authorization", "cookie", "x-private", "model-secret"] {
        assert!(!request.contains(s));
    }
    running.server.logger.flush().unwrap();
    let logs = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
    let route = logs
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|entry| entry["event"] == "route_selected")
        .unwrap();
    assert_eq!(route["service"], "codex");
}

#[tokio::test]
async fn request_limits_duplicates_and_chunked_upload() {
    let mut fixture = fixture("direct", "redirect").await;
    let running = running(
        "codex:\n  routing:\n    api_key_fallback: none\n  base_url:\n    api_key: https://upstream.invalid/v1\n",
    )
    .await;
    trust(&running, &fixture, "none");
    for (extra, status) in [
        ("Content-Length: 33554433\r\n", 413),
        ("Expect: 100-continue\r\nContent-Length: 2\r\n", 417),
        ("Upgrade: websocket\r\n", 426),
        ("Authorization: Bearer duplicate\r\n", 400),
        ("Content-Length: 2\r\nTransfer-Encoding: chunked\r\n", 400),
        ("Content-Length: 2\r\nContent-Length: 2\r\n", 400),
    ] {
        let mut socket = tokio::net::TcpStream::connect(running.url.trim_start_matches("http://"))
            .await
            .unwrap();
        socket.write_all(format!("POST /responses HTTP/1.1\r\nHost: local\r\nAuthorization: Bearer model-secret\r\n{extra}\r\n").as_bytes()).await.unwrap();
        let mut output = String::new();
        tokio::time::timeout(Duration::from_secs(2), socket.read_to_string(&mut output))
            .await
            .unwrap()
            .unwrap();
        assert!(
            output.starts_with(&format!("HTTP/1.1 {status}")),
            "{output}"
        );
    }
    let mut socket = tokio::net::TcpStream::connect(running.url.trim_start_matches("http://"))
        .await
        .unwrap();
    socket.write_all(b"POST /responses HTTP/1.1\r\nHost: local\r\nAuthorization: Bearer model-secret\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n").await.unwrap();
    let mut output = String::new();
    socket.read_to_string(&mut output).await.unwrap();
    assert!(output.starts_with("HTTP/1.1 302"));
    assert!(fixture.requests.recv().await.unwrap().ends_with("abcde"));
}

#[tokio::test]
async fn disconnect_cancels_upstream_and_stream_timeout_does_not_replay() {
    for (disconnect, method) in [
        (true, hyper::Method::POST),
        (false, hyper::Method::POST),
        (true, hyper::Method::GET),
        (false, hyper::Method::GET),
    ] {
        let mut fixture = fixture("direct", "sse").await;
        let running=running("codex:\n  routing:\n    api_key_fallback: none\n  base_url:\n    api_key: https://upstream.invalid/v1\n").await;
        trust(&running, &fixture, "none");
        let response = http()
            .request(method.clone(), format!("{}/responses", running.url))
            .bearer_auth("model-secret")
            .send()
            .await
            .unwrap();
        let mut stream = response.bytes_stream();
        assert_eq!(stream.next().await.unwrap().unwrap(), "data: first\n\n");
        assert!(
            fixture
                .requests
                .recv()
                .await
                .unwrap()
                .starts_with(method.as_str())
        );
        if disconnect {
            drop(stream);
        } else {
            assert!(
                tokio::time::timeout(Duration::from_secs(5), stream.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .is_err()
            );
        }
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), fixture.requests.recv())
                .await
                .unwrap()
                .unwrap(),
            "DISCONNECTED"
        );
        assert!(
            fixture.requests.try_recv().is_err(),
            "failed streaming request must never be replayed"
        );
        let expected = if disconnect {
            "request_cancelled"
        } else {
            "request_failed"
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                running.server.logger.flush().unwrap();
                let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
                if let Some(event) = raw
                    .lines()
                    .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                    .find(|e| e["event"] == expected)
                {
                    assert_eq!(event["status"], "200");
                    assert_eq!(
                        event["reason"],
                        if disconnect {
                            "request_dropped"
                        } else {
                            "transport_error"
                        }
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn claude_oauth_usage_and_refresh_preserve_json_and_headers() {
    let mut fixture = fixture("http", "oauth_json").await;
    let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
    let home = tempfile::tempdir().unwrap();
    let file = home.path().join(".credentials.json");
    std::fs::write(
        &file,
        r#"{"claudeAiOauth":{"accessToken":"access-secret","refreshToken":"refresh-secret"}}"#,
    )
    .unwrap();
    let running = running(&format!("proxies:\n  selected: {endpoint}\nclaude:\n  config_dirs: [{}]\n  routing:\n    account_fallback: selected\n", serde_json::to_string(&home.path()).unwrap())).await;
    trust(&running, &fixture, &endpoint);
    let response = http()
        .get(format!("{}/anthropic/api/oauth/usage", running.url))
        .bearer_auth("access-secret")
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("user-agent", "claude-code/2.1.69")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["retry-after"], "120");
    let value: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert_eq!(value["five_hour"]["utilization"], 42);
    assert!(
        fixture
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("CONNECT api.anthropic.com:443")
    );
    let request = fixture.requests.recv().await.unwrap();
    assert!(request.contains("authorization: Bearer access-secret"));
    assert!(request.contains("user-agent: claude-code/2.1.69"));
    assert_eq!(request.matches("oauth-2025-04-20").count(), 1);
    let body = r#"{"grant_type":"refresh_token","refresh_token":"refresh-secret","client_id":"client","scope":"user:profile user:inference"}"#;
    let response = http()
        .post(format!("{}/anthropic/v1/oauth/token", running.url))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let value: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert_eq!(value["access_token"], "new-access");
    assert_eq!(value["refresh_token"], "new-refresh");
    assert!(
        fixture
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("CONNECT platform.claude.com:443")
    );
    assert!(fixture.requests.recv().await.unwrap().ends_with(body));
    assert!(
        std::fs::read_to_string(file)
            .unwrap()
            .contains("refresh-secret")
    );
}

async fn read_response_head(socket: &mut tokio::net::TcpStream) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        bytes.push(socket.read_u8().await.unwrap());
    }
    String::from_utf8(bytes).unwrap()
}

#[tokio::test]
async fn websocket_upgrade_preserves_auth_protocol_and_early_frames() {
    for scoped in [false, true] {
        let mut fixture = fixture("http", "websocket").await;
        let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
        let running = running(&format!("proxies:\n  selected: {endpoint}\ncodex:\n  routing:\n    api_key:\n      upstream.invalid: selected\nclaude:\n  routing:\n    api_key:\n      upstream.invalid: selected\n")).await;
        trust(&running, &fixture, &endpoint);
        let mut socket = tokio::net::TcpStream::connect(running.url.trim_start_matches("http://"))
            .await
            .unwrap();
        let scope = if scoped { "anthropic" } else { "codex" };
        socket.write_all(format!("GET /{scope}/https://upstream.invalid/v1/socket?model=test HTTP/1.1\r\nHost: local\r\nAuthorization: Bearer ws-secret\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: test\r\n\r\n").as_bytes()).await.unwrap();
        let head = read_response_head(&mut socket).await;
        assert!(head.starts_with("HTTP/1.1 101"), "{head}");
        assert!(
            head.to_ascii_lowercase()
                .contains("\r\nconnection: upgrade\r\n"),
            "{head}"
        );
        assert!(
            head.to_ascii_lowercase()
                .contains("\r\nupgrade: websocket\r\n"),
            "{head}"
        );
        assert!(head.contains("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="), "{head}");
        assert!(head.to_lowercase().contains("sec-websocket-protocol: test"));
        let mut early = [0; 4];
        socket.read_exact(&mut early).await.unwrap();
        assert_eq!(&early, b"\x81\x02hi");
        socket
            .write_all(b"\x81\x82\x01\x02\x03\x04ni")
            .await
            .unwrap();
        let mut tail = Vec::new();
        socket.read_to_end(&mut tail).await.unwrap();
        assert_eq!(&tail, b"\x81\x02ok\x88\x00");
        assert!(
            fixture
                .requests
                .recv()
                .await
                .unwrap()
                .starts_with("CONNECT upstream.invalid:443")
        );
        let request = fixture.requests.recv().await.unwrap();
        assert!(request.starts_with("GET /v1/socket?model=test HTTP/1.1"));
        assert!(request.contains("authorization: Bearer ws-secret"));
        assert!(request.contains("sec-websocket-protocol: test"));
        assert!(request.contains("upgrade: websocket"));
    }
}

#[tokio::test]
async fn websocket_invalid_accept_is_rejected() {
    let fixture = fixture("direct", "websocket_bad").await;
    let running =
        running("claude:\n  routing:\n    api_key:\n      upstream.invalid: none\n").await;
    trust(&running, &fixture, "none");
    let response = http()
        .get(format!(
            "{}/anthropic/https://upstream.invalid/socket",
            running.url
        ))
        .bearer_auth("key")
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 502);
}

#[tokio::test]
async fn inbound_connect_direct_preserves_early_bytes_and_half_close() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let destination = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut payload = Vec::new();
        socket.read_to_end(&mut payload).await.unwrap();
        assert_eq!(&payload, b"early-later");
        socket.write_all(b"reply-after-half-close").await.unwrap();
    });
    let running = running(&format!("connect:\n  '{destination}': none\n")).await;
    let mut socket = tokio::net::TcpStream::connect(running.url.trim_start_matches("http://"))
        .await
        .unwrap();
    socket
        .write_all(
            format!("CONNECT {destination} HTTP/1.1\r\nHost: {destination}\r\n\r\nearly-")
                .as_bytes(),
        )
        .await
        .unwrap();
    let head = read_response_head(&mut socket).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    socket.write_all(b"later").await.unwrap();
    socket.shutdown().await.unwrap();
    let mut payload = Vec::new();
    socket.read_to_end(&mut payload).await.unwrap();
    assert_eq!(&payload, b"reply-after-half-close");
    peer.await.unwrap();
}

#[tokio::test]
async fn inbound_connect_uses_http_proxy_without_forwarding_client_credentials() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let head = read_response_head(&mut socket).await;
        assert!(head.starts_with("CONNECT api.anthropic.com:443 HTTP/1.1"));
        assert!(head.contains("Proxy-Authorization: Basic dTpw"));
        assert!(!head.contains("private"));
        socket
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\nearly-reply")
            .await
            .unwrap();
        let mut payload = Vec::new();
        socket.read_to_end(&mut payload).await.unwrap();
        assert_eq!(&payload, b"tunnel-data");
    });
    let running = running(&format!(
        "proxies:\n  selected: http://u:p@{endpoint}\nconnect:\n  api.anthropic.com:443: selected\n"
    ))
    .await;
    let mut socket = tokio::net::TcpStream::connect(running.url.trim_start_matches("http://"))
        .await
        .unwrap();
    socket.write_all(b"CONNECT api.anthropic.com:443 HTTP/1.1\r\nHost: api.anthropic.com:443\r\nAuthorization: Bearer private-token\r\nCookie: private-cookie\r\nProxy-Authorization: Basic private\r\n\r\n").await.unwrap();
    assert!(
        read_response_head(&mut socket)
            .await
            .starts_with("HTTP/1.1 200")
    );
    socket.write_all(b"tunnel-data").await.unwrap();
    socket.shutdown().await.unwrap();
    let mut data = Vec::new();
    socket.read_to_end(&mut data).await.unwrap();
    assert_eq!(&data, b"early-reply");
    peer.await.unwrap();
}

#[tokio::test]
async fn inbound_connect_rejects_unconfigured_destinations_and_bodies() {
    let running = running("connect:\n  api.anthropic.com:443: none\n").await;
    for (destination, extra, status) in [
        ("other.invalid:443", "", 403),
        ("api.anthropic.com:444", "", 403),
        ("api.anthropic.com:443", "Content-Length: 1\r\n", 400),
        (
            "api.anthropic.com:443",
            "Transfer-Encoding: chunked\r\n",
            400,
        ),
    ] {
        let mut socket = tokio::net::TcpStream::connect(running.url.trim_start_matches("http://"))
            .await
            .unwrap();
        socket
            .write_all(
                format!("CONNECT {destination} HTTP/1.1\r\nHost: {destination}\r\n{extra}\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let head = read_response_head(&mut socket).await;
        assert!(head.starts_with(&format!("HTTP/1.1 {status}")), "{head}");
    }
}

#[tokio::test]
async fn connect_relays_close_on_timeout_disconnect_and_shutdown() {
    for mode in ["timeout", "disconnect", "shutdown"] {
        let destination = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = destination.local_addr().unwrap();
        let running = running(&format!("connect:\n  '{address}': none\n")).await;
        running.task.abort();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local = listener.local_addr().unwrap();
        let (shutdown, stop) = tokio::sync::oneshot::channel();
        let serve = tokio::spawn(running.server.clone().serve(listener, async {
            let _ = stop.await;
        }));
        let mut socket = tokio::net::TcpStream::connect(local).await.unwrap();
        socket
            .write_all(format!("CONNECT {address} HTTP/1.1\r\nHost: local\r\n\r\n").as_bytes())
            .await
            .unwrap();
        assert!(
            read_response_head(&mut socket)
                .await
                .starts_with("HTTP/1.1 200")
        );
        let (mut peer, _) = destination.accept().await.unwrap();
        let mut socket = Some(socket);
        let mut shutdown = Some(shutdown);
        if mode == "disconnect" {
            drop(socket.take());
        }
        if mode == "shutdown" {
            shutdown.take().unwrap().send(()).unwrap();
        }
        let mut bytes = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), peer.read(&mut bytes))
                .await
                .unwrap()
                .unwrap(),
            0,
            "{mode}"
        );
        drop(peer);
        if let Some(mut socket) = socket {
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), socket.read(&mut bytes))
                    .await
                    .unwrap()
                    .unwrap(),
                0,
                "{mode}"
            );
        }
        drop(shutdown);
        serve.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn connect_only_falls_back_to_explicit_candidates_before_establishment() {
    for allow_direct in [true, false] {
        let destination = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = destination.local_addr().unwrap();
        let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy.local_addr().unwrap();
        let reject = tokio::spawn(async move {
            let (mut socket, _) = proxy.accept().await.unwrap();
            let _ = read_response_head(&mut socket).await;
            socket
                .write_all(
                    b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let candidates = if allow_direct {
            "[rejected, none]"
        } else {
            "[rejected]"
        };
        let running = running(&format!(
            "proxies:\n  rejected: http://{proxy_address}\nconnect:\n  '{address}': {candidates}\n"
        ))
        .await;
        let mut socket = tokio::net::TcpStream::connect(running.url.trim_start_matches("http://"))
            .await
            .unwrap();
        socket
            .write_all(format!("CONNECT {address} HTTP/1.1\r\nHost: local\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let head = read_response_head(&mut socket).await;
        if allow_direct {
            assert!(head.starts_with("HTTP/1.1 200"), "{head}");
            let (mut peer, _) = destination.accept().await.unwrap();
            socket.write_all(b"x").await.unwrap();
            assert_eq!(peer.read_u8().await.unwrap(), b'x');
        } else {
            assert!(head.starts_with("HTTP/1.1 502"), "{head}");
            assert!(
                tokio::time::timeout(Duration::from_millis(50), destination.accept())
                    .await
                    .is_err()
            );
        }
        reject.await.unwrap();
    }
}

#[tokio::test]
async fn claude_named_settings_forward_to_file_upstream_through_selected_proxy() {
    for (base_path, target) in [
        ("/custom", "/anthropic/v1/messages"),
        (
            "/custom",
            "/anthropic/https://upstream.invalid/custom/v1/messages",
        ),
        (
            "/custom",
            "/claude/https://upstream.invalid/custom/v1/messages",
        ),
        ("", "/https://upstream.invalid/v1/messages"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("api.json"), serde_json::to_string(&serde_json::json!({"env": {
        "ANTHROPIC_BASE_URL": format!("http://127.0.0.1:8787/https://upstream.invalid{base_path}"),
        "ANTHROPIC_AUTH_TOKEN": "profile-secret"
    }})).unwrap()).unwrap();
        let mut fixture = fixture("http", "sse").await;
        let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
        let running = running(&format!("proxies:\n  selected: {endpoint}\nclaude:\n  config_dirs: [{}]\n  routing:\n    api_key:\n      api: selected\n", serde_json::to_string(dir.path()).unwrap())).await;
        trust(&running, &fixture, &endpoint);
        let mut response = http()
            .post(format!("{}{target}", running.url))
            .bearer_auth("profile-secret")
            .body("model-body")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{target}");
        let connect = fixture.requests.recv().await.unwrap();
        assert!(connect.starts_with("CONNECT upstream.invalid:443"));
        assert!(!connect.contains("profile-secret"));
        let request = fixture.requests.recv().await.unwrap();
        assert!(request.starts_with(&format!("POST {base_path}/v1/messages HTTP/1.1")));
        assert!(request.contains("authorization: Bearer profile-secret"));
        assert!(!request.contains("oauth-2025-04-20"));
        assert!(request.ends_with("model-body"));
        assert_eq!(response.chunk().await.unwrap().unwrap(), "data: first\n\n");
        fixture.release.notify_one();
        assert_eq!(response.chunk().await.unwrap().unwrap(), "data: last\n\n");
    }
}

#[tokio::test]
async fn claude_explicit_target_is_checked_before_forwarding_or_profile_lookup() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("api.json"), r#"{"env":{"ANTHROPIC_BASE_URL":"https://upstream.invalid/custom","ANTHROPIC_AUTH_TOKEN":"profile-secret"}}"#).unwrap();
    let mut fixture = fixture("http", "sse").await;
    let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
    let running = running(&format!("proxies:\n  selected: {endpoint}\nclaude:\n  config_dirs: [{}]\n  account_auth_file_only: false\n  routing:\n    account_probe: selected\n    api_key:\n      api: selected\n", serde_json::to_string(dir.path()).unwrap())).await;
    trust(&running, &fixture, &endpoint);
    for token in ["profile-secret", "unknown-account-secret"] {
        for target in [
            "/https://other.invalid/custom/v1/messages",
            "/https://upstream.invalid:444/custom/v1/messages",
            "/https://upstream.invalid/custom-evil/v1/messages",
            "/https://upstream.invalid/v1/messages",
        ] {
            let response = http()
                .post(format!("{}/anthropic{target}", running.url))
                .bearer_auth(token)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 502, "{target}");
            assert!(response.text().await.unwrap().contains(
                "Explicit upstream must match the credential's configured HTTPS upstream and API base."
            ));
            assert!(fixture.requests.try_recv().is_err());
        }
    }
}

#[tokio::test]
async fn unavailable_proxy_recovers_while_idle() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let healthy = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let h = healthy.clone();
    let c = calls.clone();
    let upstream = tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let mut io: TestIo = Box::new(socket);
            let request = read_request(&mut io).await.unwrap();
            assert!(request.starts_with("HEAD http://example.invalid/ HTTP"));
            assert!(!request.contains("secret"));
            c.fetch_add(1, Ordering::SeqCst);
            let status = if h.load(Ordering::SeqCst) { 200 } else { 407 };
            io.write_all(
                format!("HTTP/1.1 {status} Test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        }
    });
    let running = running(&format!("proxies:\n  test: {endpoint}\n")).await;
    let choice = Choice::List(vec!["test".into()]);
    let url = Url::parse("http://example.invalid/private?secret=yes").unwrap();
    let mut log = RequestLog {
        logger: running.server.logger.clone(),
        fields: Default::default(),
        started: Instant::now(),
        status: 200,
        bytes: 0,
        outcome: "test_finished",
    };
    let mut concurrent_log = RequestLog {
        logger: running.server.logger.clone(),
        fields: Default::default(),
        started: Instant::now(),
        status: 200,
        bytes: 0,
        outcome: "test_finished",
    };
    let (first, second) = tokio::join!(
        running.server.select(&choice, &url, &mut log),
        running.server.select(&choice, &url, &mut concurrent_log),
    );
    assert!(first.is_err() && second.is_err());
    assert!(
        running
            .server
            .select(&choice, &url, &mut log)
            .await
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    healthy.store(true, Ordering::SeqCst);
    // No client requests: the serve-owned timer must discover recovery itself.
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let state = running
                .server
                .probes
                .lock()
                .unwrap()
                .values()
                .next()
                .unwrap()
                .clone();
            if !state.lock().await.unavailable {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        running
            .server
            .select(&choice, &url, &mut log)
            .await
            .unwrap(),
        "test"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    upstream.abort();
}

#[tokio::test]
async fn disconnect_before_headers_does_not_invent_a_502() {
    let mut fixture = fixture("direct", "delayed_headers").await;
    let running = running("codex:\n  routing:\n    api_key_fallback: none\n  base_url:\n    api_key: https://upstream.invalid/v1\n").await;
    trust(&running, &fixture, "none");
    let mut socket = tokio::net::TcpStream::connect(running.url.trim_start_matches("http://"))
        .await
        .unwrap();
    socket.write_all(b"GET /models HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer model-secret\r\n\r\n").await.unwrap();
    assert!(fixture.requests.recv().await.unwrap().starts_with("GET "));
    drop(socket);
    let path = running._temp.path().join("proxy.log");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            running.server.logger.flush().unwrap();
            let raw = std::fs::read_to_string(&path).unwrap();
            if let Some(event) = raw
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .find(|e| e["event"] == "request_cancelled")
            {
                assert!(event.get("status").is_none());
                assert_eq!(event["reason"], "request_dropped");
                assert!(!raw.contains("request_failed"));
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn safe_get_retries_transport_failures_on_the_same_route_and_stops_at_three() {
    for mode in ["direct", "http"] {
        for (response_mode, status) in [("drop_twice", 204), ("drop_always", 502)] {
            let mut fixture = fixture(mode, response_mode).await;
            let endpoint = if mode == "direct" {
                "none".to_owned()
            } else {
                format!("http://{}", fixture.addr)
            };
            let running = running(&format!("proxies:\n  selected: {endpoint}\ncodex:\n  routing:\n    api_key_fallback: selected\n  base_url:\n    api_key: https://upstream.invalid/v1\n")).await;
            trust(&running, &fixture, &endpoint);
            let response = http()
                .get(format!("{}/models?secret=private-query", running.url))
                .bearer_auth("model-secret")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), status);
            response.bytes().await.unwrap();
            for _ in 0..3 {
                if mode == "http" {
                    assert!(
                        fixture
                            .requests
                            .recv()
                            .await
                            .unwrap()
                            .starts_with("CONNECT upstream.invalid:443 ")
                    );
                }
                let request = fixture.requests.recv().await.unwrap();
                assert!(request.starts_with("GET /v1/models?secret=private-query "));
                assert!(
                    request
                        .to_lowercase()
                        .contains("authorization: bearer model-secret")
                );
            }
            assert!(fixture.requests.try_recv().is_err());
            running.server.logger.flush().unwrap();
            let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
            let events: Vec<serde_json::Value> = raw
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            let retries: Vec<_> = events
                .iter()
                .filter(|e| e["event"] == "upstream_retry")
                .collect();
            assert_eq!(retries.len(), 2);
            assert_eq!(retries[0]["retry_delay_ms"], "200");
            assert_eq!(retries[1]["retry_delay_ms"], "400");
            let end = events
                .iter()
                .find(|e| e["event"] == "request_finished" || e["event"] == "request_failed")
                .unwrap();
            assert_eq!(end["upstream_attempts"], "3");
            assert!(!raw.contains("private-query"));
            assert!(!raw.contains("model-secret"));
        }
    }
}

#[tokio::test]
async fn unsafe_requests_and_http_errors_are_not_retried() {
    for (method, body, websocket, response_mode, status) in [
        (hyper::Method::POST, "", false, "drop_always", 502),
        (hyper::Method::GET, "payload", false, "drop_always", 502),
        (hyper::Method::GET, "", true, "drop_always", 502),
        (hyper::Method::GET, "", false, "status_401", 401),
        (hyper::Method::GET, "", false, "status_429", 429),
        (hyper::Method::GET, "", false, "status_503", 503),
    ] {
        let mut fixture = fixture("direct", response_mode).await;
        let running = running("codex:\n  routing:\n    api_key_fallback: none\n  base_url:\n    api_key: https://upstream.invalid/v1\n").await;
        trust(&running, &fixture, "none");
        let mut request = http()
            .request(method, format!("{}/models", running.url))
            .bearer_auth("model-secret")
            .body(body);
        if websocket {
            request = request
                .header("connection", "Upgrade")
                .header("upgrade", "websocket")
                .header("sec-websocket-version", "13")
                .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==");
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), status);
        if status != 502 {
            assert_eq!(response.headers()["retry-after"], "120");
            assert_eq!(response.text().await.unwrap(), "oops");
        } else {
            response.bytes().await.unwrap();
        }
        fixture.requests.recv().await.unwrap();
        assert!(fixture.requests.try_recv().is_err());
        running.server.logger.flush().unwrap();
        let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
        assert!(!raw.contains("upstream_retry"));
    }
}

#[tokio::test]
async fn safe_get_retries_share_the_original_timeout_budget() {
    let mut fixture = fixture("direct", "slow_drop_then_delay").await;
    let running = running("codex:\n  routing:\n    api_key_fallback: none\n  base_url:\n    api_key: https://upstream.invalid/v1\n").await;
    trust(&running, &fixture, "none");
    let response = tokio::time::timeout(
        Duration::from_millis(3800),
        http()
            .get(format!("{}/models", running.url))
            .bearer_auth("model-secret")
            .send(),
    )
    .await
    .expect("retries must share the three-second budget")
    .unwrap();
    assert_eq!(response.status(), 502);
    response.bytes().await.unwrap();
    for _ in 0..2 {
        assert!(fixture.requests.recv().await.unwrap().starts_with("GET "));
    }
    assert!(fixture.requests.try_recv().is_err());
    running.server.logger.flush().unwrap();
    let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
    assert_eq!(raw.matches("\"event\":\"upstream_retry\"").count(), 1);
    assert!(raw.contains("\"transport_error\":\"timeout\""));
}

#[test]
fn transient_health_failures_need_confirmation_and_recovery_is_immediate() {
    let mut state = ProbeHealth::default();
    state.observe(ProbeResult::Healthy);
    for _ in 0..2 {
        state.observe(ProbeResult::Transient);
        assert!(
            !state.unavailable,
            "one brief outage must not suppress API requests"
        );
        assert!(state.next_check.unwrap() <= Instant::now() + Duration::from_secs(1));
    }
    state.observe(ProbeResult::Transient);
    assert!(state.unavailable);
    assert!(state.next_check.unwrap() <= Instant::now() + Duration::from_secs(3));
    state.observe(ProbeResult::Healthy);
    assert!(!state.unavailable);
    assert_eq!(state.failures, 0);
    state.observe(ProbeResult::HardFailure);
    assert!(state.unavailable);
    for _ in 0..9 {
        state.observe(ProbeResult::Transient);
    }
    assert!(state.next_check.unwrap() > Instant::now() + Duration::from_secs(20));
}

#[tokio::test]
async fn api_response_clears_only_its_own_exit_failures() {
    let fixture = fixture("direct", "redirect").await;
    let running = running("codex:\n  routing:\n    api_key_fallback: [none]\n  base_url:\n    api_key: https://upstream.invalid/v1\n").await;
    trust(&running, &fixture, "none");
    let key = ProbeKey {
        endpoint: "none".into(),
        origin: "https://upstream.invalid/".into(),
        native_tls: false,
        tunnel: false,
    };
    let mut suspect = ProbeHealth::default();
    suspect.observe(ProbeResult::Transient);
    suspect.observe(ProbeResult::Transient);
    let state = Arc::new(tokio::sync::Mutex::new(suspect));
    let mut failed = ProbeHealth::default();
    failed.observe(ProbeResult::HardFailure);
    let other = Arc::new(tokio::sync::Mutex::new(failed));
    {
        let mut cache = running.server.probes.lock().unwrap();
        cache.insert(key.clone(), state.clone());
        cache.insert(
            ProbeKey {
                endpoint: "http://other.invalid:8080".into(),
                ..key
            },
            other.clone(),
        );
    }
    let response = http()
        .post(format!("{}/responses", running.url))
        .bearer_auth("model-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 302);
    assert_eq!(state.lock().await.failures, 0);
    assert!(other.lock().await.unavailable);
}

#[tokio::test]
async fn homepage_failure_does_not_disable_a_working_tunnel() {
    for drop_head in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let head = read_response_head(&mut socket).await;
            assert!(head.starts_with("HEAD "));
            if drop_head {
                drop(socket);
                let (mut socket, _) = listener.accept().await.unwrap();
                let head = read_response_head(&mut socket).await;
                assert!(head.starts_with("CONNECT example.invalid:80 "));
                socket.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await.unwrap();
            } else {
                socket.write_all(b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
            }
        });
        let running = running("").await;
        let result = running
            .server
            .probe(&ProbeKey {
                endpoint,
                origin: "http://example.invalid/".into(),
                native_tls: false,
                tunnel: false,
            })
            .await;
        assert_eq!(result, ProbeResult::Healthy);
        task.await.unwrap();
    }
}

#[tokio::test]
async fn stale_failed_probe_cannot_override_a_successful_request() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let _ = read_response_head(&mut socket).await;
        started_tx.send(()).unwrap();
        release_rx.await.unwrap();
        socket
            .write_all(
                b"HTTP/1.1 407 Auth Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
    });
    let running = running("").await;
    let mut health = ProbeHealth::default();
    health.observe(ProbeResult::Transient);
    health.next_check = Some(Instant::now());
    health.monitored = true;
    let state = Arc::new(tokio::sync::Mutex::new(health));
    running.server.probes.lock().unwrap().insert(
        ProbeKey {
            endpoint: endpoint.clone(),
            origin: "http://example.invalid/".into(),
            native_tls: false,
            tunnel: false,
        },
        state.clone(),
    );
    let server = running.server.clone();
    let refresh = tokio::spawn(async move {
        server.refresh_probes().await;
    });
    started_rx.await.unwrap();
    running
        .server
        .record_route_success(
            &endpoint,
            &Url::parse("http://example.invalid/api?q=secret").unwrap(),
            false,
        )
        .await;
    release_tx.send(()).unwrap();
    refresh.await.unwrap();
    assert!(!state.lock().await.unavailable);
    assert_eq!(state.lock().await.failures, 0);
    task.await.unwrap();
}

#[tokio::test]
async fn profile_uses_shared_safe_get_retry_and_health_feedback() {
    let mut lookup = fixture("http", "drop_twice").await;
    let payload = fixture("http", "redirect").await;
    let lookup_endpoint = format!("http://127.0.0.1:{}", lookup.addr.port());
    let payload_endpoint = format!("http://127.0.0.1:{}", payload.addr.port());
    let running = running(&format!("proxies:\n  lookup: {lookup_endpoint}\n  selected: {payload_endpoint}\nclaude:\n  account_auth_file_only: false\n  base_url: https://upstream.invalid\n  routing:\n    account:\n      remote@example.invalid: selected\n    account_probe: lookup\n")).await;
    trust(&running, &lookup, &lookup_endpoint);
    trust(&running, &payload, &payload_endpoint);
    let response = http()
        .post(format!("{}/anthropic/v1/messages", running.url))
        .bearer_auth("profile-token")
        .body("payload")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 302);
    for _ in 0..3 {
        assert!(
            lookup
                .requests
                .recv()
                .await
                .unwrap()
                .starts_with("CONNECT ")
        );
        assert!(
            lookup
                .requests
                .recv()
                .await
                .unwrap()
                .starts_with("GET /api/oauth/profile ")
        );
    }
    assert!(lookup.requests.try_recv().is_err());
    let key = ProbeKey::http(
        &lookup_endpoint,
        &Url::parse("https://upstream.invalid/").unwrap(),
        false,
    );
    let state = running.server.probes.lock().unwrap()[&key].clone();
    assert_eq!(state.lock().await.failures, 0);
    running.server.logger.flush().unwrap();
    let log = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
    assert_eq!(log.matches("\"event\":\"upstream_retry\"").count(), 2);
    assert!(!log.contains("profile-token"));
}

#[tokio::test]
async fn refresh_failure_is_not_replayed_and_counts_once() {
    let mut fixture = fixture("http", "drop_always").await;
    let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
    let running = running(&format!("proxies:\n  selected: {endpoint}\nclaude:\n  config_dirs: []\n  account_auth_file_only: false\n  routing:\n    account_fallback: selected\n")).await;
    trust(&running, &fixture, &endpoint);
    let response = http()
        .post(format!("{}/anthropic/v1/oauth/token", running.url))
        .header("content-type", "application/json")
        .body(r#"{"grant_type":"refresh_token","refresh_token":"refresh-secret"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 502);
    assert!(
        fixture
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("CONNECT ")
    );
    assert!(
        fixture
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("POST /v1/oauth/token ")
    );
    assert!(fixture.requests.try_recv().is_err());
    let key = ProbeKey::http(
        &endpoint,
        &Url::parse(crate::claude::TOKEN_REFRESH_UPSTREAM).unwrap(),
        false,
    );
    let state = running.server.probes.lock().unwrap()[&key].clone();
    assert_eq!(state.lock().await.failures, 1);
    running.server.logger.flush().unwrap();
    let log = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
    assert!(!log.contains("upstream_retry"));
    assert!(!log.contains("refresh-secret"));
}

#[tokio::test]
async fn connect_candidates_share_one_deadline_and_recover_from_cached_failure() {
    let slow = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", slow.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (mut socket, _) = slow.accept().await.unwrap();
        let _ = read_response_head(&mut socket).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    });
    let destination = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("https://{}/", destination.local_addr().unwrap())).unwrap();
    let running = running(&format!("proxies:\n  slow: {endpoint}\n")).await;
    let mut log = RequestLog {
        logger: running.server.logger.clone(),
        fields: Default::default(),
        started: Instant::now(),
        status: 502,
        bytes: 0,
        outcome: "test_finished",
    };
    let result = running
        .server
        .connect_via(
            &Choice::List(vec!["slow".into(), "none".into()]),
            &url,
            Deadline::new(0.05),
            &mut log,
        )
        .await;
    assert!(result.is_err());
    assert!(
        tokio::time::timeout(Duration::from_millis(30), destination.accept())
            .await
            .is_err(),
        "second candidate must not receive a fresh timeout budget"
    );
    task.abort();

    let key = ProbeKey::tunnel("none", &url);
    let mut cached = ProbeHealth::default();
    cached.observe(ProbeResult::HardFailure);
    cached.monitored = true;
    cached.next_check = Some(Instant::now());
    let state = Arc::new(tokio::sync::Mutex::new(cached));
    running
        .server
        .probes
        .lock()
        .unwrap()
        .insert(key.clone(), state.clone());
    running.server.refresh_probes().await;
    assert!(!state.lock().await.unavailable);
    let _ = destination.accept().await.unwrap(); // Background probe connection.
    let socket = running
        .server
        .connect_via(
            &Choice::List(vec!["none".into()]),
            &url,
            Deadline::new(1.0),
            &mut log,
        )
        .await
        .unwrap();
    let _ = destination.accept().await.unwrap();
    assert_eq!(state.lock().await.failures, 0);
    assert!(
        key != ProbeKey::http("none", &url, false),
        "CONNECT cannot prove HTTP/TLS handshake health"
    );
    drop(socket);
}

#[tokio::test]
async fn cold_selection_consumes_the_request_budget_and_logs_eligibility_separately() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let _ = read_response_head(&mut socket).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    });
    let running = running(&format!("proxies:\n  slow: {endpoint}\n")).await;
    let mut log = RequestLog {
        logger: running.server.logger.clone(),
        fields: Default::default(),
        started: Instant::now(),
        status: 502,
        bytes: 0,
        outcome: "test_finished",
    };
    let request = reqwest::Request::new(
        hyper::Method::GET,
        Url::parse("http://example.invalid/api").unwrap(),
    );
    let result = running
        .server
        .send_via(
            &Choice::List(vec!["slow".into()]),
            request,
            false,
            Deadline::new(0.05),
            &mut log,
        )
        .await;
    assert!(result.is_err());
    assert!(
        !log.fields.contains_key("upstream_attempts"),
        "expired probe budget must not start a payload attempt"
    );
    task.abort();
    let mut cached = ProbeHealth::default();
    cached.observe(ProbeResult::Transient);
    let key = ProbeKey::http(
        "none",
        &Url::parse("https://example.invalid/").unwrap(),
        false,
    );
    running
        .server
        .probes
        .lock()
        .unwrap()
        .insert(key.clone(), Arc::new(tokio::sync::Mutex::new(cached)));
    let _ = running
        .server
        .record_route_success(
            "none",
            &Url::parse("https://example.invalid/").unwrap(),
            false,
        )
        .await;
    running.server.logger.flush().unwrap();
    let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
    assert!(raw.contains("\"event\":\"route_health\""));
    assert!(raw.contains("\"available\":\"true\""));
}

fn test_ws_message(payload: &[u8], masked: bool) -> Vec<u8> {
    let flag = if masked { 128 } else { 0 };
    let mut frame = vec![0x81];
    if payload.len() < 126 {
        frame.push(payload.len() as u8 | flag);
    } else {
        frame.push(126 | flag);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    let mask = [1, 2, 3, 4];
    if masked {
        frame.extend_from_slice(&mask);
    }
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ if masked { mask[i % 4] } else { 0 }),
    );
    frame
}
async fn read_test_ws_message<R: AsyncRead + Unpin>(reader: &mut R, masked: bool) -> Vec<u8> {
    assert_eq!(reader.read_u8().await.unwrap(), 0x81);
    let length = reader.read_u8().await.unwrap();
    assert_eq!(length & 128 != 0, masked);
    let length = match length & 127 {
        126 => reader.read_u16().await.unwrap() as usize,
        n => n as usize,
    };
    let mut mask = [0; 4];
    if masked {
        reader.read_exact(&mut mask).await.unwrap();
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload).await.unwrap();
    if masked {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i % 4];
        }
    }
    payload
}
const MODEL_STREAM: &str = concat!(
    "event: response.created\r\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp\"}}\r\n\r\n",
    "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp\",\"model\":\"gpt-test\",\"usage\":{\"input_tokens\":12,\"output_tokens\":3,\"input_tokens_details\":{\"cached_tokens\":4}}}}\n\n",
);
fn model_stream_body(mode: &str) -> (&'static str, Vec<u8>) {
    if mode == "zstd_sse" {
        (
            "Content-Type: text/event-stream\r\nContent-Encoding: zstd\r\n",
            zstd::encode_all(MODEL_STREAM.as_bytes(), 0).unwrap(),
        )
    } else {
        ("", MODEL_STREAM.as_bytes().to_vec())
    }
}
#[tokio::test]
async fn http_model_calls_observe_compressed_and_untyped_streams_without_changing_them() {
    for mode in ["zstd_sse", "untyped_sse"] {
        let mut fixture = fixture("direct", mode).await;
        let running = running("codex:\n  routing:\n    api_key_fallback: none\n  base_url:\n    api_key: https://upstream.invalid/v1\n").await;
        trust(&running, &fixture, "none");
        // Codex compresses request bodies with zstd when signed in with ChatGPT. They
        // are forwarded untouched and not decoded; the model comes from the response.
        let request = zstd::encode_all(
            &br#"{"model":"gpt-request","input":"PRIVATE PROMPT"}"#[..],
            0,
        )
        .unwrap();
        let response = http()
            .post(format!("{}/responses", running.url))
            .bearer_auth("model-secret")
            .header("content-encoding", "zstd")
            .header("accept-encoding", "zstd, gzip")
            .body(request.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.bytes().await.unwrap(),
            model_stream_body(mode).1,
            "{mode}"
        );
        let forwarded = fixture.requests.recv().await.unwrap();
        let forwarded = forwarded.to_ascii_lowercase();
        assert!(forwarded.contains("content-encoding: zstd"), "{mode}");
        // The client's Accept-Encoding goes upstream as it is.
        assert!(forwarded.contains("accept-encoding: zstd, gzip"), "{mode}");
        assert!(
            forwarded.contains(&format!("content-length: {}", request.len())),
            "{mode}"
        );
        let path = running._temp.path().join("proxy.log");
        let call = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                running.server.logger.flush().unwrap();
                let raw = std::fs::read_to_string(&path).unwrap_or_default();
                assert!(!raw.contains("PRIVATE PROMPT"));
                if let Some(row) = raw
                    .lines()
                    .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                    .find(|row| row["event"] == "model_call_finished")
                {
                    return row;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(call["model_outcome"], "finished", "{mode}");
        assert_eq!(call["input_tokens"], "12", "{mode}");
        assert_eq!(call["output_tokens"], "3", "{mode}");
        assert_eq!(call["cached_input_tokens"], "4", "{mode}");
        assert_eq!(call["model"], "gpt-test", "{mode}");
        assert!(call.get("request_content_encoding").is_none(), "{mode}");
        if mode == "zstd_sse" {
            assert_eq!(call["response_content_encoding"], "zstd");
            assert_eq!(call["response_content_type"], "text/event-stream");
        } else {
            assert!(call.get("response_content_type").is_none());
        }
    }
}
#[tokio::test]
async fn responses_websocket_records_two_model_calls_while_connection_stays_open() {
    let fixture = fixture("http", "websocket_calls").await;
    let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
    let running = running(&format!("proxies:\n  selected: {endpoint}\ncodex:\n  routing:\n    api_key:\n      upstream.invalid: selected\n")).await;
    trust(&running, &fixture, &endpoint);
    let mut socket = tokio::net::TcpStream::connect(running.url.trim_start_matches("http://"))
        .await
        .unwrap();
    socket.write_all(b"GET /codex/https://upstream.invalid/v1/responses HTTP/1.1\r\nHost: local\r\nAuthorization: Bearer ws-secret\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n").await.unwrap();
    assert!(
        read_response_head(&mut socket)
            .await
            .starts_with("HTTP/1.1 101")
    );
    for id in ["turn-one", "turn-two"] {
        socket
            .write_all(&test_ws_message(
                br#"{"type":"response.create","model":"test-model","input":"PRIVATE PROMPT"}"#,
                true,
            ))
            .await
            .unwrap();
        let response = read_test_ws_message(&mut socket, false).await;
        let response: serde_json::Value = serde_json::from_slice(&response).unwrap();
        assert_eq!(response["response"]["id"], id);
    }
    running.server.logger.flush().unwrap();
    let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
    let rows: Vec<serde_json::Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let calls: Vec<_> = rows
        .iter()
        .filter(|r| r["event"] == "model_call_finished")
        .collect();
    assert_eq!(calls.len(), 2);
    assert_ne!(calls[0]["model_call_id"], calls[1]["model_call_id"]);
    assert_eq!(calls[0]["request_id"], calls[1]["request_id"]);
    assert_eq!(calls[1]["output_tokens"], "4");
    assert!(!raw.contains("PRIVATE PROMPT"));
    assert!(!rows.iter().any(|r| r["event"] == "request_finished"));
}
#[tokio::test]
async fn a_connection_reset_before_accept_does_not_stop_the_listener() {
    let temp = tempfile::tempdir().unwrap();
    let logger = Arc::new(Logger::new(temp.path().join("proxy.log")));
    let config = Config::parse("listen_port: 7889\nrequest_timeout_seconds: 3\n").unwrap();
    let server = Arc::new(Server::new(config, logger));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // Queue a connection that the client resets before the server accepts it.
    let early = tokio::net::TcpStream::connect(addr).await.unwrap();
    early.set_zero_linger().unwrap();
    drop(early);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let task = tokio::spawn(server.serve(listener, std::future::pending()));
    let response = http()
        .get(format!("http://{addr}/backend-api/wham/usage"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    assert!(!task.is_finished());
    task.abort();
}
#[tokio::test]
async fn tls_and_plain_http_share_the_listening_port() {
    let temp = tempfile::tempdir().unwrap();
    let logger = Arc::new(Logger::new(temp.path().join("proxy.log")));
    let config = Config::parse("listen_port: 7889\nrequest_timeout_seconds: 3\n").unwrap();
    let tls = crate::local_tls::acceptor(temp.path()).unwrap();
    let server = Arc::new(Server::new(config, logger).with_tls(tls));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let _task = tokio::spawn(server.serve(listener, std::future::pending()));
    let ca = std::fs::read(temp.path().join(crate::local_tls::CA_FILE)).unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .use_rustls_tls()
        .tls_built_in_root_certs(false)
        .add_root_certificate(reqwest::Certificate::from_pem(&ca).unwrap())
        .build()
        .unwrap();
    for base in [
        format!("http://127.0.0.1:{port}"),
        format!("https://127.0.0.1:{port}"),
        format!("https://localhost:{port}"),
    ] {
        let response = client
            .get(format!("{base}/backend-api/wham/usage"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401, "{base}");
    }
    // A client without the CA is refused rather than served in plain text.
    assert!(
        reqwest::Client::builder()
            .no_proxy()
            .use_rustls_tls()
            .tls_built_in_root_certs(false)
            .build()
            .unwrap()
            .get(format!("https://127.0.0.1:{port}/backend-api/wham/usage"))
            .send()
            .await
            .is_err()
    );
}

#[tokio::test]
async fn rejected_requests_with_unread_bodies_receive_a_complete_response() {
    let running = running("codex:\n  homes: []\nclaude:\n  config_dirs: []\n").await;
    for path in ["/responses", "/health"] {
        let mut socket = tokio::net::TcpStream::connect(running.url.trim_start_matches("http://"))
            .await
            .unwrap();
        let body = vec![b'x'; 64 * 1024];
        let mut request = format!(
            "GET {path} HTTP/1.1\r\nHost: local\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        request.extend_from_slice(&body);
        socket.write_all(&request).await.unwrap();
        let mut response = Vec::new();
        let result =
            tokio::time::timeout(Duration::from_secs(2), socket.read_to_end(&mut response))
                .await
                .unwrap();
        assert!(result.is_ok(), "{path}: {result:?}");
        let response = String::from_utf8(response).unwrap();
        assert!(
            response.starts_with(if path == "/health" {
                "HTTP/1.1 200"
            } else {
                "HTTP/1.1 401"
            }),
            "{response}"
        );
        assert!(
            response.contains("{\"ok\":true}")
                || response.contains("A configured Bearer token is required.")
        );
    }
}

#[tokio::test]
async fn repeated_list_headers_are_forwarded_but_sensitive_duplicates_are_rejected() {
    let mut fixture = fixture("direct", "redirect").await;
    let running = running("codex:\n  routing:\n    api_key_fallback: none\n  base_url:\n    api_key: https://upstream.invalid/v1\n").await;
    trust(&running, &fixture, "none");
    let mut socket = tokio::net::TcpStream::connect(running.url.trim_start_matches("http://"))
        .await
        .unwrap();
    socket.write_all(b"GET /models HTTP/1.1\r\nHost: local\r\nAuthorization: Bearer model-secret\r\nAccept: application/json\r\nAccept: text/event-stream\r\nConnection: keep-alive\r\nConnection: x-test-hop\r\nx-test-hop: remove-me\r\n\r\n").await.unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 302"));
    let forwarded = fixture.requests.recv().await.unwrap().to_lowercase();
    assert!(forwarded.contains("accept: application/json"));
    assert!(forwarded.contains("accept: text/event-stream"));
    assert!(!forwarded.contains("x-test-hop"));
    for extra in [
        "Host: other\r\n",
        "Authorization: Bearer other\r\n",
        "x-api-key: first\r\nx-api-key: second\r\n",
        "chatgpt-account-id: first\r\nchatgpt-account-id: second\r\n",
        "Content-Length: 0\r\nContent-Length: 0\r\n",
        "Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n",
    ] {
        let mut socket = tokio::net::TcpStream::connect(running.url.trim_start_matches("http://"))
            .await
            .unwrap();
        socket.write_all(format!("GET /models HTTP/1.1\r\nHost: local\r\nAuthorization: Bearer model-secret\r\n{extra}\r\n").as_bytes()).await.unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    }
    assert!(fixture.requests.try_recv().is_err());
}
