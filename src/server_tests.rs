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
        "relay.invalid".into(),
        "182.92.106.196".into(),
        "developers.openai.com".into(),
        "auth.openai.com".into(),
        "platform.claude.com".into(),
        "api.anthropic.com".into(),
        "daily-cloudcode-pa.googleapis.com".into(),
        "cloudcode-pa.googleapis.com".into(),
        "www.googleapis.com".into(),
        "oauth2.googleapis.com".into(),
        "aicode.googleapis.com".into(),
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
                else if response_mode.starts_with("auto_") {
                    let refresh = request.starts_with("POST /oauth/token ") || request.starts_with("POST /v1/oauth/token ") || request.starts_with("POST /token ");
                    if refresh && response_mode == "auto_wait" { ready.notified().await; }
                    let (status, body) = if refresh {
                        match response_mode {
                            "auto_rejected" => (400, r#"{"error":"invalid_grant"}"#),
                            "auto_invalid" => (200, r#"{"access_token":"invalid token"}"#),
                            "auto_rate_limit" => (429, "{}"),
                            _ => (200, r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#),
                        }
                    } else if request.contains("authorization: Bearer new-access") && response_mode != "auto_retry_401" {
                        (200, "model-result")
                    } else {
                        (401, "original-unauthorized")
                    };
                    io.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                }
                else if let Some(status) = response_mode.strip_prefix("status_") {
                    io.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Length: 4\r\nRetry-After: 120\r\nConnection: close\r\n\r\noops").as_bytes()).await.unwrap();
                }
                else if profile {
                    let (status, body) = match response_mode {
                        "profile_unauthorized" => (401, "{}"),
                        "profile_retry_identity" if attempt == 0 => (401, "{}"),
                        "profile_retry_identity" => (200, r#"{"account":{"uuid":"12345678-1234-4234-8234-123456789abc","email":"remote@example.invalid"}}"#),
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
                else if response_mode=="websocket_hold" {
                    io.write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n").await.unwrap();
                    let mut byte = [0];
                    while matches!(io.read(&mut byte).await, Ok(1..)) {}
                    let _ = tx.send("CLOSED".into());
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
                else if response_mode == "location_once" && attempt == 0 {
                    let body = r#"{"error":{"code":400,"status":"FAILED_PRECONDITION","message":"User location is not supported for the API use."}}"#;
                    io.write_all(format!("HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
                }
                else if response_mode == "location_once" {
                    io.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").await.unwrap();
                }
                else if response_mode=="oauth_json" {
                    let body = if request.starts_with("POST /v1/oauth/token ") {
                        r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#
                    } else {
                        r#"{"five_hour":{"utilization":42},"seven_day":{"utilization":12},"extra_usage":{"is_enabled":true}}"#
                    };
                    io.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nRetry-After: 120\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                }
                else if response_mode=="oauth_account" {
                    let body = r#"{"access_token":"new-access","refresh_token":"learned-refresh","expires_in":3600,"account":{"uuid":"acct-1","email_address":"one@example.invalid"}}"#;
                    io.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                }
                else if response_mode=="codex_oauth_account" {
                    // Codex token responses name the account in their JWT claims.
                    let body = r#"{"access_token":"h.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdC0xIn0sImVtYWlsIjoib25lQGV4YW1wbGUuaW52YWxpZCJ9.s","refresh_token":"learned-refresh","id_token":"h.e30.s"}"#;
                    io.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
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
                else if request.starts_with("GET /oauth2/v2/userinfo ") {
                    let body = r#"{"id":"1","email":"remote@example.invalid","verified_email":true}"#;
                    io.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                }
                else if response_mode=="google_token" {
                    // Google's refresh response: no new refresh token; its ID token names the account.
                    let body = r#"{"access_token":"renewed-access","expires_in":3599,"scope":"openid","token_type":"Bearer","id_token":"h.eyJlbWFpbCI6InJlbW90ZUBleGFtcGxlLmludmFsaWQiLCJzdWIiOiIxIn0.s"}"#;
                    io.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                }
                else if response_mode=="gemini_sse" {
                    // Cloud Code's stream: Gemini chunks wrapped in `response`, usage last.
                    let body = concat!(
                        "data: {\"response\":{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"private-output\"}]}}],\"modelVersion\":\"gemini-test\",\"responseId\":\"gemini-response\"},\"traceId\":\"t\"}\r\n\r\n",
                        "data: {\"response\":{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"!\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":12,\"candidatesTokenCount\":5,\"thoughtsTokenCount\":3,\"cachedContentTokenCount\":4,\"totalTokenCount\":20},\"modelVersion\":\"gemini-test\",\"responseId\":\"gemini-response\"},\"traceId\":\"t\"}\r\n\r\n",
                    );
                    io.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
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
    running_with_tls(config, |_| {}).await
}
/// `running`, after `seed` prepares the TLS directory, e.g. with a stored CA.
async fn running_with_tls(config: &str, seed: impl FnOnce(&std::path::Path)) -> Running {
    running_with_server(config, seed, |server| server).await
}
/// `running_with_tls`, after `customize` adds state to the server.
async fn running_with_server(
    config: &str,
    seed: impl FnOnce(&std::path::Path),
    customize: impl FnOnce(Server) -> Server,
) -> Running {
    let temp = tempfile::tempdir().unwrap();
    seed(&temp.path().join("tls"));
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
    let credential_proxy =
        crate::credential_proxy::CredentialProxy::prepare(&config, &temp.path().join("tls"))
            .unwrap();
    let mut server = customize(Server::new(config, logger));
    if let Some(proxy) = credential_proxy {
        server = server.with_credential_proxy(proxy);
    }
    let server = Arc::new(server);
    // Routing fixtures drive diagnostics explicitly after installing test TLS
    // trust; background profile I/O would race their request assertions.
    *server.credential_checks.lock().await = Some((tokio::time::Instant::now(), Vec::new()));
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
        // The proxy reads the response to learn its refresh route.
        assert!(request.contains("accept-encoding: identity"));
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
    let refreshes: Vec<serde_json::Value> = log
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .filter(|record: &serde_json::Value| {
            record["path"]
                .as_str()
                .is_some_and(|path| path.ends_with("/oauth/token"))
        })
        .collect();
    assert!(
        refreshes
            .iter()
            .any(|record| record["event"] == "request_received")
    );
    assert!(
        refreshes.iter().all(|record| record["service"] == "claude"),
        "Every record of a token refresh names its app"
    );
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
        let running = running(&format!("proxies:\n  selected: {endpoint}\nclaude:\n  config_dirs: [{dir}]\n  base_url: https://upstream.invalid\n  routing:\n    account_fallback: selected\n    fallback: selected\n")).await;
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
async fn claude_shared_url_selector_keeps_oauth_on_the_account_route() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join(".credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"test-account"}}"#,
    )
    .unwrap();
    let mut account = fixture("http", "oauth_json").await;
    let mut keys = fixture("http", "oauth_json").await;
    let account_endpoint = format!("http://{}", account.addr);
    let key_endpoint = format!("http://{}", keys.addr);
    let running = running(&format!(
        "proxies:\n  account: {account_endpoint}\n  keys: {key_endpoint}\nclaude:\n  config_dirs: [{}]\n  base_url: https://upstream.invalid\n  routing:\n    account: {{default: account}}\n    fallback: keys\n    api_key: {{upstream.invalid: keys}}",
        serde_json::to_string(home.path()).unwrap()
    )).await;
    trust(&running, &account, &account_endpoint);
    trust(&running, &keys, &key_endpoint);
    let client = http();
    let profile = format!(
        "{}/anthropic/https://upstream.invalid/api/oauth/profile",
        running.url
    );
    let denied = client
        .get(&profile)
        .header("x-api-key", "test-key")
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 403);
    assert!(keys.requests.try_recv().is_err() && account.requests.try_recv().is_err());
    let allowed = client
        .get(profile)
        .bearer_auth("test-account")
        .send()
        .await
        .unwrap();
    assert_eq!(allowed.status(), 200);
    assert!(
        account
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("CONNECT ")
    );
    assert!(
        account
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("GET /api/oauth/profile ")
    );
    assert!(keys.requests.try_recv().is_err());
}

#[tokio::test]
async fn codex_provider_edits_cannot_send_keys_to_account_paths_or_fallback() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("config.toml");
    let write_provider = |base: &str| {
        std::fs::write(&path, format!("cli_auth_credentials_store = 'file'\n[model_providers.relay]\nbase_url = '{base}'\nexperimental_bearer_token = 'test-provider-key'\n")).unwrap();
    };
    write_provider("https://upstream.invalid/v1");
    let config = Config::parse(&format!(
        "listen_port: 8787\nrequest_timeout_seconds: 3\ncodex:\n  homes: [{}]\n  routing:\n    api_key: {{relay: none}}\n    fallback: none",
        serde_json::to_string(home.path()).unwrap()
    )).unwrap();
    let auth = Some("Bearer test-provider-key");
    assert_eq!(
        config.resolve(auth, true).await.unwrap().upstream,
        "https://upstream.invalid/v1"
    );
    write_provider("http://127.0.0.1:8787/https://chatgpt.com/backend-api");
    assert!(
        config
            .resolve(auth, true)
            .await
            .is_err_and(|e| e.message.contains("overlaps"))
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
        "claude:\n  config_dirs: [{:?}]\n  routing:\n    account_fallback: none\n    fallback: none\ncodex:\n  routing:\n    fallback: none\n",
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
    let running = self::running("codex:\n  routing:\n    fallback: none\n").await;
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
        for _ in 0..3 {
            let cached = running.server.cached_traffic_credential_labels().await;
            assert!(!cached.contains_key(&("Claude".into(), "remote-account".into())));
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(100), lookup.requests.recv())
                .await
                .is_err(),
            "Reading cached display labels must not contact the upstream"
        );
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
        // Only failures are recorded; identities live in the profile cache.
        let before = running.server.failed_probes.lock().await.clone();
        assert_eq!(before.len(), usize::from(!succeeds));
        assert_eq!(
            running.server.cached_traffic_credential_labels().await,
            first
        );
        assert_eq!(running.server.traffic_credential_labels().await, first);
        assert!(lookup.requests.try_recv().is_err());
        assert_eq!(*running.server.failed_probes.lock().await, before);
    }
}

#[tokio::test]
async fn periodic_credentials_enrich_missing_identity_retry_failures_and_cache_success() {
    let mut lookup = fixture("http", "profile_retry_identity").await;
    let endpoint = format!("http://127.0.0.1:{}", lookup.addr.port());
    let dir = tempfile::tempdir().unwrap();
    let credentials = dir.path().join(".credentials.json");
    let saved = r#"{"claudeAiOauth":{"accessToken":"saved-secret"}}"#;
    std::fs::write(&credentials, saved).unwrap();
    let running = running(&format!("proxies:\n  lookup: {endpoint}\nclaude:\n  config_dirs: [{}]\n  base_url: https://upstream.invalid\n  routing:\n    account:\n      remote@example.invalid: none\n    account_probe: lookup\n", serde_json::to_string(dir.path()).unwrap())).await;
    trust(&running, &lookup, &endpoint);
    let identity = (
        "Claude".into(),
        "12345678-1234-4234-8234-123456789abc".into(),
    );
    *running.server.credential_checks.lock().await = None;
    running.server.credential_reports().await;
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
    assert!(
        !running
            .server
            .cached_traffic_credential_labels()
            .await
            .contains_key(&identity)
    );
    for _ in 0..3 {
        running.server.credential_reports().await;
        running.server.cached_traffic_credential_labels().await;
    }
    assert!(
        lookup.requests.try_recv().is_err(),
        "Neither cached reports nor display reads retry a failed probe"
    );
    // The next periodic check, a minute later, still holds the failure.
    *running.server.credential_checks.lock().await = None;
    for at in running.server.failed_probes.lock().await.values_mut() {
        *at = Instant::now() - Duration::from_secs(61);
    }
    running.server.credential_reports().await;
    assert!(
        lookup.requests.try_recv().is_err(),
        "A failed probe is not retried on every periodic check"
    );
    // Expire both periodic and failed-probe caches, without waiting.
    *running.server.credential_checks.lock().await = None;
    for at in running.server.failed_probes.lock().await.values_mut() {
        *at = Instant::now() - FAILED_PROBE_RETRY - Duration::from_secs(1);
    }
    running.server.credential_reports().await;
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
    assert_eq!(
        running
            .server
            .cached_traffic_credential_labels()
            .await
            .get(&identity)
            .map(String::as_str),
        Some("remote@example.invalid")
    );
    *running.server.credential_checks.lock().await = None;
    running.server.credential_reports().await;
    assert!(
        lookup.requests.try_recv().is_err(),
        "Successful identities are reused across cycles"
    );
    std::fs::write(
        &credentials,
        r#"{"claudeAiOauth":{"accessToken":"rotated-secret"}}"#,
    )
    .unwrap();
    *running.server.credential_checks.lock().await = None;
    running.server.credential_reports().await;
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
            .contains("authorization: Bearer rotated-secret")
    );
    assert!(
        !dir.path().join(".claude.json").exists(),
        "Probes must not rewrite Claude metadata"
    );
}

#[tokio::test]
async fn rebuilt_servers_reuse_shared_claude_identities() {
    let mut lookup = fixture("http", "sse").await;
    let endpoint = format!("http://127.0.0.1:{}", lookup.addr.port());
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"saved-secret"}}"#,
    )
    .unwrap();
    let running = running(&format!("proxies:\n  lookup: {endpoint}\nclaude:\n  config_dirs: [{}]\n  base_url: https://upstream.invalid\n  routing:\n    account:\n      remote@example.invalid: none\n    account_probe: lookup\n", serde_json::to_string(dir.path()).unwrap())).await;
    trust(&running, &lookup, &endpoint);
    let identity = ("Claude".into(), "remote-account".into());
    assert!(
        running
            .server
            .traffic_credential_labels()
            .await
            .contains_key(&identity)
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
    // A configuration reload starts a server without account checks.
    let rebuilt = Server::new(running.server.config.clone(), running.server.logger.clone())
        .with_claude_profiles(running.server.claude_profiles.clone());
    for _ in 0..3 {
        assert_eq!(
            rebuilt
                .traffic_credential_labels()
                .await
                .get(&identity)
                .map(String::as_str),
            Some("remote@example.invalid")
        );
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(100), lookup.requests.recv())
            .await
            .is_err(),
        "A shared identity is not looked up again"
    );
}

#[tokio::test]
async fn periodic_identity_probe_requires_explicit_probe_and_skips_known_local_identity() {
    for (probe, metadata) in [(false, false), (false, true), (true, true)] {
        let mut lookup = fixture("http", "sse").await;
        let endpoint = format!("http://127.0.0.1:{}", lookup.addr.port());
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"saved-secret"}}"#,
        )
        .unwrap();
        if metadata {
            std::fs::write(dir.path().join(".claude.json"), r#"{"oauthAccount":{"accountUuid":"local-account","emailAddress":"remote@example.invalid"}}"#).unwrap();
        }
        let probe = if probe {
            "    account_probe: lookup\n"
        } else {
            ""
        };
        let running = running(&format!("proxies:\n  lookup: {endpoint}\nclaude:\n  config_dirs: [{}]\n  base_url: https://upstream.invalid\n  routing:\n    account:\n      remote@example.invalid: none\n    account_fallback: lookup\n{probe}",serde_json::to_string(dir.path()).unwrap())).await;
        trust(&running, &lookup, &endpoint);
        *running.server.credential_checks.lock().await = None;
        running.server.credential_reports().await;
        assert!(
            lookup.requests.try_recv().is_err(),
            "Fallback alone cannot authorize identity discovery, and local metadata needs no probe"
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
async fn account_status_reads_discovered_identities_without_probing() {
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
        // Before discovery, the saved login could belong to either route.
        for _ in 0..3 {
            let states = running.server.account_route_states().await;
            assert_eq!(states[1]["remote@example.invalid"], "unknown");
            assert_eq!(states[1]["other@example.invalid"], "unknown");
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(100), lookup.requests.recv())
                .await
                .is_err(),
            "Reading account status never probes"
        );
        // Periodic discovery probes once; status then reads its result.
        *running.server.credential_checks.lock().await = None;
        running.server.credential_reports().await;
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
        // A token's identity does not change, and a failure stays the latest
        // result after its retry delay: neither expires with time.
        for (at, _) in running.server.claude_profiles.lock().unwrap().values_mut() {
            *at = Instant::now() - Duration::from_secs(24 * 3600);
        }
        for at in running.server.failed_probes.lock().await.values_mut() {
            *at = Instant::now() - FAILED_PROBE_RETRY - Duration::from_secs(1);
        }
        for _ in 0..3 {
            assert_eq!(running.server.account_route_states().await, states);
        }
        assert!(lookup.requests.try_recv().is_err());
        // Metadata that no longer matches a route still uses the saved token's result.
        std::fs::write(
            dir.path().join(".claude.json"),
            r#"{"oauthAccount":{"accountUuid":"stale","emailAddress":"stale@example.invalid"}}"#,
        )
        .unwrap();
        assert_eq!(running.server.account_route_states().await, states);
        // A changed credential cannot reuse another token's result.
        std::fs::write(
            &credentials,
            r#"{"claudeAiOauth":{"accessToken":"replacement-secret"}}"#,
        )
        .unwrap();
        assert_eq!(
            running.server.account_route_states().await[1]["remote@example.invalid"],
            "unknown"
        );
        // Local matches take precedence over any remote result.
        std::fs::write(
            dir.path().join(".claude.json"),
            r#"{"oauthAccount":{"accountUuid":"local","emailAddress":"remote@example.invalid"}}"#,
        )
        .unwrap();
        assert_eq!(
            running.server.account_route_states().await[1]["remote@example.invalid"],
            "active"
        );
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
async fn account_status_leaves_unreadable_environment_sources_unknown() {
    let running = running("claude:\n  config_dirs: []\n  auth_env: COPORT_TEST_UNSET_ACCOUNT_STATE\n  routing:\n    account: {'user@example.invalid': none}\n    account_probe: none\n").await;
    for _ in 0..2 {
        assert_eq!(
            running.server.account_route_states().await[1]["user@example.invalid"],
            "unknown"
        );
    }
    assert!(running.server.claude_profiles.lock().unwrap().is_empty());
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
    // A client's own token counts as configured once its account is mapped.
    let routed: Vec<serde_json::Value> = log
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|row| {
            row["account_id"] == "remote-account" && row.get("credential_match").is_some()
        })
        .collect();
    assert!(!routed.is_empty());
    assert!(
        routed
            .iter()
            .all(|row| row["credential_match"] == "configured"),
        "{routed:?}"
    );
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
        let running=running(&format!("proxies:\n  selected: {endpoint}\ncodex:\n  routing:\n    fallback: selected\n  base_url:\n    api_key: https://upstream.invalid/v1\n")).await;
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
    let running=running(&format!("proxies:\n  dead: http://127.0.0.1:{dead_port}\ncodex:\n  routing:\n    fallback: [dead, none]\n  base_url:\n    api_key: https://upstream.invalid/v1\n")).await;
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
        "codex:\n  routing:\n    fallback: none\n  base_url:\n    api_key: https://upstream.invalid/v1\n",
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
    // A forwarded response keeps the connection open for further requests.
    let output = read_response_head(&mut socket).await;
    assert!(output.starts_with("HTTP/1.1 302"), "{output}");
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
        let running=running("codex:\n  routing:\n    fallback: none\n  base_url:\n    api_key: https://upstream.invalid/v1\n").await;
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

#[tokio::test]
async fn claude_refresh_routes_are_learned_from_token_responses() {
    for bootstrap in ["account_probe", "account_fallback", "fallback"] {
        let mut probe = fixture("http", "oauth_account").await;
        let mut mapped = fixture("http", "oauth_account").await;
        let probe_endpoint = format!("http://127.0.0.1:{}", probe.addr.port());
        let mapped_endpoint = format!("http://127.0.0.1:{}", mapped.addr.port());
        let running = running(&format!("proxies:\n  probe: {probe_endpoint}\n  mapped: {mapped_endpoint}\nclaude:\n  config_dirs: []\n  account_auth_file_only: false\n  routing:\n    account:\n      one@example.invalid: mapped\n    {bootstrap}: probe\n")).await;
        trust(&running, &probe, &probe_endpoint);
        trust(&running, &mapped, &mapped_endpoint);
        let refresh = |token: &str| {
            http()
                .post(format!("{}/anthropic/v1/oauth/token", running.url))
                .header("content-type", "application/json")
                .header("accept-encoding", "gzip, br")
                .body(format!(
                    r#"{{"grant_type":"refresh_token","refresh_token":"{token}"}}"#
                ))
                .send()
        };
        // An unknown login renews through the profile probe's route, and the
        // answer names it.
        let response = refresh("first-refresh").await.unwrap();
        assert_eq!(response.status(), 200);
        let value: serde_json::Value =
            serde_json::from_str(&response.text().await.unwrap()).unwrap();
        assert_eq!(value["refresh_token"], "learned-refresh");
        assert!(
            probe
                .requests
                .recv()
                .await
                .unwrap()
                .starts_with("CONNECT platform.claude.com:443")
        );
        let request = probe.requests.recv().await.unwrap();
        assert!(request.contains("accept-encoding: identity"));
        assert!(request.ends_with(r#""refresh_token":"first-refresh"}"#));
        // Its next renewal takes the account's own route.
        let response = refresh("learned-refresh").await.unwrap();
        assert_eq!(response.status(), 200);
        response.bytes().await.unwrap();
        assert!(
            mapped
                .requests
                .recv()
                .await
                .unwrap()
                .starts_with("CONNECT platform.claude.com:443")
        );
        assert!(
            mapped
                .requests
                .recv()
                .await
                .unwrap()
                .ends_with(r#""refresh_token":"learned-refresh"}"#)
        );
        assert!(probe.requests.try_recv().is_err());
        running.server.logger.flush().unwrap();
        let log = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
        let routed: Vec<serde_json::Value> = log
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .filter(|record: &serde_json::Value| record["event"] == "route_selected")
            .collect();
        assert_eq!(routed.len(), 2);
        assert_eq!(routed[0]["credential_match"], "unmatched");
        assert_eq!(routed[1]["credential_match"], "configured");
        assert_eq!(routed[1]["account_id"], "acct-1");
        assert_eq!(routed[1]["account_label"], "one@example.invalid");
        assert!(!log.contains("learned-refresh"));
    }
}

#[tokio::test]
async fn codex_refresh_routes_are_learned_from_token_responses() {
    for bootstrap in ["account_fallback", "fallback"] {
        let mut fallback = fixture("http", "codex_oauth_account").await;
        let mut mapped = fixture("http", "codex_oauth_account").await;
        let fallback_endpoint = format!("http://127.0.0.1:{}", fallback.addr.port());
        let mapped_endpoint = format!("http://127.0.0.1:{}", mapped.addr.port());
        let running = running(&format!("proxies:\n  fallback: {fallback_endpoint}\n  mapped: {mapped_endpoint}\ncodex:\n  account_auth_file_only: false\n  routing:\n    account:\n      one@example.invalid: mapped\n    {bootstrap}: fallback\n")).await;
        trust(&running, &fallback, &fallback_endpoint);
        trust(&running, &mapped, &mapped_endpoint);
        let refresh = |token: &str| {
            http()
                .post(format!("{}/oauth/token", running.url))
                .header("content-type", "application/x-www-form-urlencoded")
                .header("accept-encoding", "gzip, br")
                .body(format!("grant_type=refresh_token&refresh_token={token}"))
                .send()
        };
        // A login no saved auth.json holds renews through account_fallback, and
        // the answer names it.
        let response = refresh("first-refresh").await.unwrap();
        assert_eq!(response.status(), 200);
        let value: serde_json::Value =
            serde_json::from_str(&response.text().await.unwrap()).unwrap();
        assert_eq!(value["refresh_token"], "learned-refresh");
        assert!(
            fallback
                .requests
                .recv()
                .await
                .unwrap()
                .starts_with("CONNECT auth.openai.com:443")
        );
        let request = fallback.requests.recv().await.unwrap();
        assert!(request.contains("accept-encoding: identity"));
        assert!(request.ends_with("refresh_token=first-refresh"));
        // Its next renewal takes the account's own route.
        let response = refresh("learned-refresh").await.unwrap();
        assert_eq!(response.status(), 200);
        response.bytes().await.unwrap();
        assert!(
            mapped
                .requests
                .recv()
                .await
                .unwrap()
                .starts_with("CONNECT auth.openai.com:443")
        );
        assert!(
            mapped
                .requests
                .recv()
                .await
                .unwrap()
                .ends_with("refresh_token=learned-refresh")
        );
        assert!(fallback.requests.try_recv().is_err());
        running.server.logger.flush().unwrap();
        let log = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
        let routed: Vec<serde_json::Value> = log
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .filter(|record: &serde_json::Value| record["event"] == "route_selected")
            .collect();
        assert_eq!(routed.len(), 2);
        assert_eq!(routed[0]["credential_match"], "unmatched");
        assert_eq!(routed[1]["credential_match"], "configured");
        assert_eq!(routed[1]["account_id"], "acct-1");
        assert_eq!(routed[1]["account_label"], "one@example.invalid");
        assert!(!log.contains("learned-refresh"));
    }
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
async fn inbound_connect_without_credential_routing_is_refused() {
    // Without credential routing no destination is served, and none is
    // contacted; malformed requests are refused as such.
    let destination = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = destination.local_addr().unwrap().to_string();
    let running = running("").await;
    for (destination, extra, status) in [
        (address.as_str(), "", 403),
        ("api.anthropic.com:443", "", 403),
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
    assert!(
        tokio::time::timeout(Duration::from_millis(50), destination.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn upgraded_relays_close_on_timeout_disconnect_and_shutdown() {
    for mode in ["timeout", "disconnect", "shutdown"] {
        let mut fixture = fixture("http", "websocket_hold").await;
        let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
        let running = running(&format!(
            "proxies:\n  selected: {endpoint}\ncodex:\n  routing:\n    api_key:\n      upstream.invalid: selected\n"
        ))
        .await;
        trust(&running, &fixture, &endpoint);
        running.task.abort();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local = listener.local_addr().unwrap();
        let (shutdown, stop) = tokio::sync::oneshot::channel();
        let serve = tokio::spawn(running.server.clone().serve(listener, async {
            let _ = stop.await;
        }));
        let mut socket = tokio::net::TcpStream::connect(local).await.unwrap();
        // Not a model endpoint, so the upgraded connection is an opaque relay.
        socket.write_all(b"GET /codex/https://upstream.invalid/v1/socket HTTP/1.1\r\nHost: local\r\nAuthorization: Bearer ws-secret\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n").await.unwrap();
        let head = read_response_head(&mut socket).await;
        assert!(head.starts_with("HTTP/1.1 101"), "{head}");
        assert!(
            fixture
                .requests
                .recv()
                .await
                .unwrap()
                .starts_with("CONNECT upstream.invalid:443")
        );
        assert!(
            fixture
                .requests
                .recv()
                .await
                .unwrap()
                .starts_with("GET /v1/socket ")
        );
        let mut socket = Some(socket);
        let mut shutdown = Some(shutdown);
        if mode == "disconnect" {
            drop(socket.take());
        }
        if mode == "shutdown" {
            shutdown.take().unwrap().send(()).unwrap();
        }
        // The upstream sees its side closed: by the idle timeout, the client
        // leaving, or service shutdown.
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), fixture.requests.recv())
                .await
                .unwrap()
                .unwrap(),
            "CLOSED",
            "{mode}"
        );
        if let Some(mut socket) = socket {
            let mut bytes = [0];
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
    let running = running("codex:\n  routing:\n    fallback: none\n  base_url:\n    api_key: https://upstream.invalid/v1\n").await;
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
            let running = running(&format!("proxies:\n  selected: {endpoint}\ncodex:\n  routing:\n    fallback: selected\n  base_url:\n    api_key: https://upstream.invalid/v1\n")).await;
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
        let running = running("codex:\n  routing:\n    fallback: none\n  base_url:\n    api_key: https://upstream.invalid/v1\n").await;
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
    let running = running("codex:\n  routing:\n    fallback: none\n  base_url:\n    api_key: https://upstream.invalid/v1\n").await;
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
    let running = running("codex:\n  routing:\n    fallback: [none]\n  base_url:\n    api_key: https://upstream.invalid/v1\n").await;
    trust(&running, &fixture, "none");
    let key = ProbeKey {
        endpoint: "none".into(),
        origin: "https://upstream.invalid/".into(),
        native_tls: false,
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
        let running = running("codex:\n  routing:\n    fallback: none\n  base_url:\n    api_key: https://upstream.invalid/v1\n").await;
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
            .header("session_id", "http-session")
            .header("x-client-request-id", "http-client-request")
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
        assert_eq!(call["session_id"], "http-session");
        assert_eq!(call["client_request_id"], "http-client-request");
        assert_ne!(call["request_id"], call["client_request_id"]);
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
    socket.write_all(b"GET /codex/https://upstream.invalid/v1/responses HTTP/1.1\r\nHost: local\r\nAuthorization: Bearer ws-secret\r\nsession_id: ws-session\r\nx-client-request-id: handshake-request\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n").await.unwrap();
    assert!(
        read_response_head(&mut socket)
            .await
            .starts_with("HTTP/1.1 101")
    );
    for id in ["turn-one", "turn-two"] {
        socket
            .write_all(&test_ws_message(
                serde_json::json!({"type":"response.create","model":"test-model","input":"PRIVATE PROMPT", "request_id":id}).to_string().as_bytes(),
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
    for (call, id) in calls.iter().zip(["turn-one", "turn-two"]) {
        assert_eq!(call["session_id"], "ws-session");
        assert_eq!(call["client_request_id"], id);
        assert_eq!(call["connection_client_request_id"], "handshake-request");
    }
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
async fn a_connection_serves_requests_one_after_another() {
    let mut fixture = fixture("direct", "oauth_json").await;
    let running = running("codex:\n  routing:\n    fallback: none\n  base_url:\n    api_key: https://upstream.invalid/v1\n").await;
    trust(&running, &fixture, "none");
    let mut socket = tokio::net::TcpStream::connect(running.url.trim_start_matches("http://"))
        .await
        .unwrap();
    for _ in 0..2 {
        socket
            .write_all(b"GET /models HTTP/1.1\r\nHost: local\r\nAuthorization: Bearer model-secret\r\n\r\n")
            .await
            .unwrap();
        let head = read_response_head(&mut socket).await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(
            !head.to_ascii_lowercase().contains("connection: close"),
            "{head}"
        );
        // The chunked body's terminator ends the response, not the connection.
        let mut body = Vec::new();
        while !body.ends_with(b"\r\n0\r\n\r\n") {
            body.push(socket.read_u8().await.unwrap());
        }
        assert!(String::from_utf8_lossy(&body).contains("five_hour"));
        assert!(
            fixture
                .requests
                .recv()
                .await
                .unwrap()
                .starts_with("GET /v1/models ")
        );
    }
}

#[tokio::test]
async fn a_stream_failing_midway_closes_the_connection_without_completing_the_body() {
    let mut fixture = fixture("direct", "sse").await;
    let running = running("codex:\n  routing:\n    fallback: none\n  base_url:\n    api_key: https://upstream.invalid/v1\n").await;
    trust(&running, &fixture, "none");
    let mut socket = tokio::net::TcpStream::connect(running.url.trim_start_matches("http://"))
        .await
        .unwrap();
    socket
        .write_all(b"POST /responses HTTP/1.1\r\nHost: local\r\nAuthorization: Bearer model-secret\r\nContent-Length: 2\r\n\r\n{}")
        .await
        .unwrap();
    let head = read_response_head(&mut socket).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let mut body = Vec::new();
    while !String::from_utf8_lossy(&body).contains("data: first") {
        body.push(socket.read_u8().await.unwrap());
    }
    assert!(
        fixture
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("POST /v1/responses")
    );
    // The upstream goes away midway: the client must see the connection end
    // before the body does, rather than a complete response on a live one.
    fixture.task.abort();
    let _ = tokio::time::timeout(Duration::from_secs(5), socket.read_to_end(&mut body))
        .await
        .expect("the connection closes");
    assert!(
        !body.ends_with(b"0\r\n\r\n"),
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert!(!String::from_utf8_lossy(&body).contains("data: last"));
}

#[tokio::test]
async fn repeated_list_headers_are_forwarded_but_sensitive_duplicates_are_rejected() {
    let mut fixture = fixture("direct", "redirect").await;
    let running = running("codex:\n  routing:\n    fallback: none\n  base_url:\n    api_key: https://upstream.invalid/v1\n").await;
    trust(&running, &fixture, "none");
    let mut socket = tokio::net::TcpStream::connect(running.url.trim_start_matches("http://"))
        .await
        .unwrap();
    socket.write_all(b"GET /models HTTP/1.1\r\nHost: local\r\nAuthorization: Bearer model-secret\r\nAccept: application/json\r\nAccept: text/event-stream\r\nConnection: keep-alive\r\nConnection: x-test-hop\r\nx-test-hop: remove-me\r\n\r\n").await.unwrap();
    let response = read_response_head(&mut socket).await;
    assert!(response.starts_with("HTTP/1.1 302"), "{response}");
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

#[tokio::test]
async fn statistics_preserve_aged_claude_identity() {
    let Some(stale) = Instant::now().checked_sub(Duration::from_secs(301)) else {
        return; // Some platforms count Instant from a recent system boot.
    };
    let mut lookup = fixture("http", "profile_unauthorized").await;
    let endpoint = format!("http://127.0.0.1:{}", lookup.addr.port());
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"saved-secret"}}"#,
    )
    .unwrap();
    let running = running(&format!("proxies:\n  lookup: {endpoint}\nclaude:\n  config_dirs: [{}]\n  base_url: https://upstream.invalid\n  routing:\n    account:\n      remote@example.invalid: none\n    account_probe: lookup\n", serde_json::to_string(dir.path()).unwrap())).await;
    trust(&running, &lookup, &endpoint);
    let identity = crate::claude::ClaudeIdentity::profile(
        &json!({"account":{"uuid":"remote-account", "email":"remote@example.invalid"}}),
    )
    .unwrap();
    running
        .server
        .cache_claude_profile("saved-secret".into(), identity)
        .unwrap();
    running
        .server
        .claude_profiles
        .lock()
        .unwrap()
        .get_mut("saved-secret")
        .unwrap()
        .0 = stale;
    let labels = running.server.traffic_credential_labels().await;
    assert_eq!(
        labels
            .get(&("Claude".into(), "remote-account".into()))
            .map(String::as_str),
        Some("remote@example.invalid")
    );
    let retained = running
        .server
        .claude_profiles
        .lock()
        .unwrap()
        .contains_key("saved-secret");
    let sent_lookup = lookup.requests.try_recv().is_ok();
    assert!(
        retained && !sent_lookup,
        "statistics invalidated request cache: retained={retained}, sent_lookup={sent_lookup}, has_label={}",
        labels.contains_key(&("Claude".into(), "remote-account".into()))
    );
}

fn proxy_port_client(running: &Running) -> reqwest::Client {
    let ca = std::fs::read(
        running
            ._temp
            .path()
            .join("tls")
            .join(crate::local_tls::PROXY_CA_FILE),
    )
    .unwrap();
    reqwest::Client::builder()
        .use_rustls_tls()
        .tls_built_in_root_certs(false)
        .add_root_certificate(reqwest::Certificate::from_pem(&ca).unwrap())
        .proxy(reqwest::Proxy::https(&running.url).unwrap())
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

// Synthetic upstream.invalid stays on loopback; the client trusts only the
// generated proxy CA, which must vouch for the decrypted destination.
#[tokio::test]
async fn proxy_port_requests_are_routed_by_credential() {
    let mut fixture = fixture("http", "oauth_json").await;
    let port = fixture.addr.port();
    let dirs = tempfile::tempdir().unwrap();
    for label in ["a", "b"] {
        let home = dirs.path().join(label);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join(".credentials.json"),
            json!({"claudeAiOauth":{"accessToken":format!("token-{label}")}}).to_string(),
        )
        .unwrap();
        std::fs::write(
            home.join(".claude.json"),
            json!({"oauthAccount":{"accountUuid":label,"emailAddress":format!("{label}@example.invalid")}})
                .to_string(),
        )
        .unwrap();
    }
    let dir = |label: &str| serde_json::to_string(&dirs.path().join(label)).unwrap();
    let opaque = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let opaque_port = opaque.local_addr().unwrap().port();
    let running = running(&format!(
        "connect_credential_routing: true\nproxies:\n  pa: http://a:x@127.0.0.1:{port}\n  pb: http://b:y@127.0.0.1:{port}\nclaude:\n  config_dirs: [{}, {}]\n  base_url: https://upstream.invalid\n  routing:\n    account:\n      a@example.invalid: pa\n      b@example.invalid: pb\n",
        dir("a"),
        dir("b")
    ))
    .await;
    for endpoint in ["a:x", "b:y"] {
        trust(
            &running,
            &fixture,
            &format!("http://{endpoint}@127.0.0.1:{port}"),
        );
    }
    let client = proxy_port_client(&running);
    for (token, authorization) in [("token-a", "YTp4"), ("token-b", "Yjp5")] {
        let response = client
            .get("https://upstream.invalid/api/oauth/usage?scope=all")
            .bearer_auth(token)
            .header("anthropic-beta", "oauth-2025-04-20")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let value: serde_json::Value =
            serde_json::from_str(&response.text().await.unwrap()).unwrap();
        assert_eq!(value["five_hour"]["utilization"], 42);
        let connect = fixture.requests.recv().await.unwrap();
        assert!(
            connect.starts_with("CONNECT upstream.invalid:443"),
            "{connect}"
        );
        assert!(
            connect.contains(&format!("Proxy-Authorization: Basic {authorization}")),
            "{connect}"
        );
        let request = fixture.requests.recv().await.unwrap();
        assert!(
            request.starts_with("GET /api/oauth/usage?scope=all "),
            "{request}"
        );
        assert!(request.contains(&format!("authorization: Bearer {token}")));
    }
    // An unmatched login is rejected locally, before any outbound connection.
    let response = client
        .get("https://upstream.invalid/v1/messages")
        .bearer_auth("token-unknown")
        .send()
        .await
        .unwrap();
    assert!(response.status().is_client_error(), "{}", response.status());
    // Other destinations are denied without being contacted.
    let opaque_destination = format!("127.0.0.1:{opaque_port}");
    for destination in ["other.invalid:443", opaque_destination.as_str()] {
        let mut socket = tokio::net::TcpStream::connect(running.url.trim_start_matches("http://"))
            .await
            .unwrap();
        socket
            .write_all(
                format!("CONNECT {destination} HTTP/1.1\r\nHost: {destination}\r\n\r\n").as_bytes(),
            )
            .await
            .unwrap();
        let head = read_response_head(&mut socket).await;
        assert!(head.starts_with("HTTP/1.1 403"), "{destination}: {head}");
    }
    assert!(fixture.requests.try_recv().is_err());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), opaque.accept())
            .await
            .is_err()
    );
    // A denied destination is logged with the host the client asked for.
    let denied = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            running.server.logger.flush().unwrap();
            let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
            if let Some(row) = raw
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .find(|row| row["event"] == "request_rejected" && row["method"] == "CONNECT")
            {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(denied["destination"], "other.invalid:443");
    // Activity shows a CONNECT's target, not `/`.
    assert_eq!(denied["path"], "other.invalid:443");
}

// A client keeps its tunnel for the next request, as with any HTTP proxy.
#[tokio::test]
async fn a_decrypted_tunnel_serves_requests_one_after_another() {
    let mut fixture = fixture("http", "oauth_json").await;
    let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        "[model_providers.ShareCoder]\nbase_url = 'https://upstream.invalid/v1'\nexperimental_bearer_token = 'fake-provider-secret'\n",
    )
    .unwrap();
    let running = running(&format!(
        "connect_credential_routing: true\nproxies:\n  selected: {endpoint}\ncodex:\n  homes: [{}]\n  routing:\n    api_key: {{ShareCoder: selected}}\n",
        serde_json::to_string(home.path()).unwrap()
    ))
    .await;
    trust(&running, &fixture, &endpoint);
    let proxy = proxy_port_client(&running);
    for _ in 0..2 {
        let response = proxy
            .get("https://upstream.invalid/v1/models")
            .bearer_auth("fake-provider-secret")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        response.text().await.unwrap();
        // Outbound, each request still takes its own route.
        assert!(
            fixture
                .requests
                .recv()
                .await
                .unwrap()
                .starts_with("CONNECT upstream.invalid:443")
        );
        assert!(
            fixture
                .requests
                .recv()
                .await
                .unwrap()
                .contains(" /v1/models ")
        );
    }
    let rows = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            running.server.logger.flush().unwrap();
            let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
            let rows: Vec<serde_json::Value> = raw
                .lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect();
            if rows
                .iter()
                .filter(|row| row["event"] == "request_finished" && row["method"] == "GET")
                .count()
                == 2
            {
                return rows;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        rows.iter()
            .filter(|row| row["event"] == "request_received" && row["method"] == "CONNECT")
            .count(),
        1,
        "Both requests share one tunnel"
    );
    let connect = rows
        .iter()
        .find(|row| row["event"] == "request_finished" && row["method"] == "CONNECT")
        .unwrap();
    assert_eq!(connect["service"], "codex", "{connect}");
    assert_eq!(connect["provider"], "ShareCoder", "{connect}");
    assert_eq!(connect["proxy"], "selected", "{connect}");
    assert!(rows.iter().any(|row| row["event"] == "request_finished"
        && row["method"] == "GET"
        && row["credential_match"] == "configured"));
}

// Any local program may open a tunnel; one whose first request only a
// fallback carries is not taken as a configured client's.
#[tokio::test]
async fn a_tunnel_opened_for_a_fallback_request_has_no_app() {
    let mut fixture = fixture("direct", "oauth_json").await;
    let running = running(
        "connect_credential_routing: true\ncodex:\n  homes: []\n  routing:\n    fallback: none\n  base_url:\n    api_key: https://upstream.invalid/v1\n",
    )
    .await;
    trust(&running, &fixture, "none");
    let response = proxy_port_client(&running)
        .get("https://upstream.invalid/v1/models")
        .bearer_auth("unknown-client-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    response.text().await.unwrap();
    assert!(
        fixture
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("GET /v1/models ")
    );
    let rows = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            running.server.logger.flush().unwrap();
            let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
            let rows: Vec<serde_json::Value> = raw
                .lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect();
            if rows
                .iter()
                .any(|row| row["event"] == "request_finished" && row["method"] == "CONNECT")
            {
                return rows;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let request = rows
        .iter()
        .find(|row| row["event"] == "request_finished" && row["method"] == "GET")
        .unwrap();
    assert_eq!(request["service"], "codex", "{request}");
    assert_eq!(request["credential_match"], "unmatched", "{request}");
    let connect = rows
        .iter()
        .find(|row| row["event"] == "request_finished" && row["method"] == "CONNECT")
        .unwrap();
    assert!(connect.get("service").is_none(), "{connect}");
    assert!(connect.get("provider").is_none(), "{connect}");
}

#[tokio::test]
async fn decrypted_tunnels_only_serve_their_destination() {
    let running = running(
        "connect_credential_routing: true\nclaude:\n  base_url: https://upstream.invalid\n  routing:\n    account_fallback: none\n",
    )
    .await;
    use rustls::pki_types::pem::PemObject;
    let ca = running
        ._temp
        .path()
        .join("tls")
        .join(crate::local_tls::PROXY_CA_FILE);
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from_pem_file(ca).unwrap())
        .unwrap();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth(),
    ));
    for (request, status) in [
        (
            "CONNECT chatgpt.com:443 HTTP/1.1\r\nHost: chatgpt.com:443\r\n\r\n",
            "400",
        ),
        (
            "GET https://other.invalid/v1/models HTTP/1.1\r\nHost: other.invalid\r\n\r\n",
            "400",
        ),
        // Absolute-form naming the tunnel's own destination is routed as usual.
        (
            "GET https://upstream.invalid/v1/models HTTP/1.1\r\nHost: upstream.invalid\r\n\r\n",
            "401",
        ),
        // Local endpoints stay on the listener itself, not inside tunnels.
        (
            "GET /health HTTP/1.1\r\nHost: upstream.invalid\r\n\r\n",
            "401",
        ),
    ] {
        let mut socket = tokio::net::TcpStream::connect(running.url.trim_start_matches("http://"))
            .await
            .unwrap();
        socket
            .write_all(
                b"CONNECT upstream.invalid:443 HTTP/1.1\r\nHost: upstream.invalid:443\r\n\r\n",
            )
            .await
            .unwrap();
        assert!(
            read_response_head(&mut socket)
                .await
                .starts_with("HTTP/1.1 200")
        );
        let name = rustls::pki_types::ServerName::try_from("upstream.invalid").unwrap();
        let mut tls = connector.connect(name, socket).await.unwrap();
        tls.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        let _ = tls.read_to_end(&mut response).await;
        let response = String::from_utf8_lossy(&response);
        assert!(
            response.starts_with(&format!("HTTP/1.1 {status}")),
            "{request}: {response}"
        );
    }
}

// Proxy clients (Copool with chatgpt_base_url on this listener) send loopback
// base URLs through the proxy as absolute-form requests naming the listener.
// They are served only while the listener is used as a proxy port.
#[tokio::test]
async fn absolute_form_requests_are_served_only_for_this_proxy_port() {
    let routes = "claude:\n  routing:\n    account_fallback: none\n";
    for enabled in [true, false] {
        let running = running(&format!("connect_credential_routing: {enabled}\n{routes}")).await;
        let address = running.url.trim_start_matches("http://").to_owned();
        let port = address.rsplit(':').next().unwrap().to_owned();
        let served = |status: &'static str| if enabled { status } else { "400" };
        for (target, status) in [
            (format!("http://{address}/health"), served("200")),
            (format!("http://localhost:{port}/health"), served("200")),
            (
                format!("HTTP://127.0.0.1:{port}/backend-api/wham/usage"),
                served("401"),
            ),
            (
                format!("http://127.0.0.1:{port}/backend-api/wham/usage?x=1"),
                served("401"),
            ),
            ("http://example.invalid/health".to_owned(), "400"),
            ("http://127.0.0.1:9/health".to_owned(), "400"),
            (format!("http://user@127.0.0.1:{port}/health"), "400"),
        ] {
            let mut socket = tokio::net::TcpStream::connect(&address).await.unwrap();
            socket
                .write_all(format!("GET {target} HTTP/1.1\r\nHost: ignored\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let head = read_response_head(&mut socket).await;
            assert!(
                head.starts_with(&format!("HTTP/1.1 {status}")),
                "{enabled} {target}: {head}"
            );
        }
        // Origin-form base URL requests are unaffected by the setting.
        let response = http()
            .get(format!("{}/health", running.url))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
    }
}

// Copool's usage refresh: a reqwest client using this listener as its proxy
// (without a loopback exemption) and a loopback chatgpt_base_url sends an
// absolute-form request naming the listener; it reaches the account's exit.
#[tokio::test]
async fn proxy_clients_reach_the_account_exit_with_a_loopback_base_url() {
    let mut fixture = fixture("http", "oauth_json").await;
    let port = fixture.addr.port();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("auth.json"),
        r#"{"tokens":{"account_id":"acct-1","access_token":"access-secret","refresh_token":"refresh-secret"}}"#,
    )
    .unwrap();
    let running = running(&format!(
        "connect_credential_routing: true\nproxies:\n  selected: http://a:x@127.0.0.1:{port}\ncodex:\n  homes: [{}]\n  base_url:\n    account: https://upstream.invalid/backend-api\n  routing:\n    account:\n      acct-1: selected\n",
        serde_json::to_string(&home.path()).unwrap()
    ))
    .await;
    trust(&running, &fixture, &format!("http://a:x@127.0.0.1:{port}"));
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(&running.url).unwrap())
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let local = running.url.trim_start_matches("http://");
    let response = client
        .get(format!("http://{local}/backend-api/wham/usage"))
        .bearer_auth("access-secret")
        .header("ChatGPT-Account-Id", "acct-1")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let value: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert_eq!(value["five_hour"]["utilization"], 42);
    let connect = fixture.requests.recv().await.unwrap();
    assert!(
        connect.starts_with("CONNECT upstream.invalid:443"),
        "{connect}"
    );
    assert!(
        connect.contains("Proxy-Authorization: Basic YTp4"),
        "{connect}"
    );
    let request = fixture.requests.recv().await.unwrap();
    assert!(
        request.starts_with("GET /backend-api/wham/usage "),
        "{request}"
    );
    assert!(request.contains("authorization: Bearer access-secret"));
    assert!(request.contains("chatgpt-account-id: acct-1"));
    // The request log names the matched account and the exit it used.
    let finished = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            running.server.logger.flush().unwrap();
            let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
            if let Some(row) = raw
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .find(|row| {
                    row["event"] == "request_finished" && row["path"] == "/backend-api/wham/usage"
                })
            {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(finished["status"], "200");
    assert_eq!(finished["account_id"], "acct-1");
    assert_eq!(finished["proxy"], "selected");
    // Copool tries these paths when the primary account usage request fails.
    for fallback in ["/api/codex/usage", "/wham/usage"] {
        let url = format!("http://{local}{fallback}?x=a%2Fb");
        let response = client
            .get(&url)
            .bearer_auth("access-secret")
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
        assert!(request.starts_with("GET /backend-api/wham/usage?x=a%2Fb "));
        assert!(request.contains("chatgpt-account-id: acct-1"));
        let denied = client
            .post(&url)
            .bearer_auth("access-secret")
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), 405);
        assert!(fixture.requests.try_recv().is_err());
    }
}

// Copool's Claude usage through the proxy port before the proxy CA is
// trusted, or while it still trusts a replaced proxy CA of the same name: the
// client refuses the certificate and nothing is forwarded. Clients trust only
// the given root, never this machine's trust store.
#[tokio::test]
async fn clients_that_do_not_trust_the_proxy_ca_are_logged_as_such() {
    let unrelated = rcgen::generate_simple_self_signed(vec!["unrelated.invalid".into()])
        .unwrap()
        .cert
        .pem();
    let replaced = tempfile::tempdir().unwrap();
    crate::local_tls::proxy_acceptors(
        replaced.path(),
        &std::collections::BTreeSet::from(["upstream.invalid".to_owned()]),
    )
    .unwrap();
    let replaced =
        std::fs::read_to_string(replaced.path().join(crate::local_tls::PROXY_CA_FILE)).unwrap();
    for root in [unrelated, replaced] {
        let running = running(
            "connect_credential_routing: true\nclaude:\n  base_url: https://upstream.invalid\n  routing:\n    account_fallback: none\n",
        )
        .await;
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .tls_built_in_root_certs(false)
            .add_root_certificate(reqwest::Certificate::from_pem(root.as_bytes()).unwrap())
            .proxy(reqwest::Proxy::https(&running.url).unwrap())
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let error = client
            .get("https://upstream.invalid/api/oauth/usage")
            .bearer_auth("token")
            .send()
            .await
            .unwrap_err();
        assert!(error.is_connect(), "{error:?}");
        let reason = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                running.server.logger.flush().unwrap();
                let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
                if let Some(row) = raw
                    .lines()
                    .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                    .find(|row| row["event"] == "request_failed" && row["method"] == "CONNECT")
                {
                    return row;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(reason["reason"], "client_rejected_proxy_ca");
        // No request reached the tunnel, so nothing shows whose it was.
        assert!(reason.get("service").is_none(), "{reason}");
    }
}

// Hosts added after the proxy CA was trusted are not decrypted with a new CA;
// their CONNECT is refused with the reason, and nothing is forwarded.
#[tokio::test]
async fn connect_to_a_host_outside_the_stored_proxy_ca_explains_the_refusal() {
    let running = running_with_tls(
        "connect_credential_routing: true\nclaude:\n  base_url: https://upstream.invalid\n  routing:\n    account_fallback: none\n    api_key:\n      'https://182.92.106.196:6060': none\n",
        |tls| {
            crate::local_tls::proxy_acceptors(
                tls,
                &std::collections::BTreeSet::from([
                    "upstream.invalid".to_owned(),
                    "platform.claude.com".to_owned(),
                ]),
            )
            .unwrap();
        },
    )
    .await;
    for (authority, expected) in [
        ("182.92.106.196:6060", "HTTP/1.1 403"),
        ("upstream.invalid:443", "HTTP/1.1 200"),
    ] {
        let mut socket = tokio::net::TcpStream::connect(running.url.trim_start_matches("http://"))
            .await
            .unwrap();
        socket
            .write_all(
                format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n").as_bytes(),
            )
            .await
            .unwrap();
        let head = read_response_head(&mut socket).await;
        assert!(head.starts_with(expected), "{authority}: {head}");
    }
    let row = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            running.server.logger.flush().unwrap();
            let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
            if let Some(row) = raw
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .find(|row| row["event"] == "request_rejected" && row["method"] == "CONNECT")
            {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(row["destination"], "182.92.106.196:6060");
    assert_eq!(
        row["reason"],
        "The proxy CA does not cover this destination; use its base URL or replace the CA."
    );
}

#[tokio::test]
async fn a_decrypted_tunnel_closed_without_a_request_is_logged() {
    use rustls::pki_types::pem::PemObject;
    let running = running(
        "connect_credential_routing: true\nclaude:\n  base_url: https://upstream.invalid\n  routing:\n    account_fallback: none\n",
    )
    .await;
    let ca = running
        ._temp
        .path()
        .join("tls")
        .join(crate::local_tls::PROXY_CA_FILE);
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from_pem_file(ca).unwrap())
        .unwrap();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth(),
    ));
    let mut socket = tokio::net::TcpStream::connect(running.url.trim_start_matches("http://"))
        .await
        .unwrap();
    socket
        .write_all(b"CONNECT upstream.invalid:443 HTTP/1.1\r\nHost: upstream.invalid:443\r\n\r\n")
        .await
        .unwrap();
    assert!(
        read_response_head(&mut socket)
            .await
            .starts_with("HTTP/1.1 200")
    );
    let name = rustls::pki_types::ServerName::try_from("upstream.invalid").unwrap();
    let mut tls = connector.connect(name, socket).await.unwrap();
    tls.shutdown().await.unwrap();
    let row = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            running.server.logger.flush().unwrap();
            let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
            if let Some(row) = raw
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .find(|row| row["event"] == "request_rejected")
            {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(row["client_connect"], "upstream.invalid");
    assert_eq!(row["status"], "400");
    assert_eq!(row["reason"], "Incomplete request headers.");
    // Without a routed request, nothing shows whose the tunnel was.
    let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
    let connect = raw
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|row| row["event"] == "request_finished" && row["method"] == "CONNECT")
        .unwrap();
    assert!(connect.get("service").is_none(), "{connect}");
    assert!(connect.get("account_id").is_none(), "{connect}");
    assert!(connect.get("provider").is_none(), "{connect}");
}

// Named providers/settings must behave identically through a base URL and an
// HTTPS proxy. All outbound connections terminate at the local fixture.
#[tokio::test]
async fn named_upstreams_route_models_and_generation_through_both_entries() {
    for claude in [false, true] {
        for host in ["upstream.invalid", "182.92.106.196:6060"] {
            let mut fixture = fixture("http", "oauth_json").await;
            let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
            let home = tempfile::tempdir().unwrap();
            let home_yaml = serde_json::to_string(home.path()).unwrap();
            let service = if claude { "anthropic" } else { "codex" };
            let config = if claude {
                std::fs::write(
                    home.path().join("custom.settings.json"),
                    json!({"env": {
                        "ANTHROPIC_BASE_URL":format!("https://{host}/v1"),
                        "ANTHROPIC_AUTH_TOKEN":"fake-provider-secret"
                    }})
                    .to_string(),
                )
                .unwrap();
                format!(
                    "claude:\n  config_dirs: [{home_yaml}]\n  routing:\n    api_key: {{custom.settings.json: selected}}\n"
                )
            } else {
                std::fs::write(home.path().join("config.toml"), format!(
                    "[model_providers.ShareCoder]\nbase_url = 'https://{host}/v1'\nexperimental_bearer_token = 'fake-provider-secret'\n"
                )).unwrap();
                format!(
                    "codex:\n  homes: [{home_yaml}]\n  routing:\n    api_key: {{ShareCoder: selected}}\n"
                )
            };
            let running = running(&format!(
                "connect_credential_routing: true\nproxies:\n  selected: {endpoint}\n{config}"
            ))
            .await;
            trust(&running, &fixture, &endpoint);
            let proxy = proxy_port_client(&running);
            for entry in ["base", "explicit", "proxy"] {
                let tunneled = entry == "proxy";
                for path in ["models", if claude { "messages" } else { "responses" }] {
                    let url = if entry == "base" {
                        if claude {
                            format!("{}/anthropic/{path}", running.url)
                        } else {
                            format!("{}/v1/{path}", running.url)
                        }
                    } else if tunneled {
                        format!("https://{host}/v1/{path}")
                    } else {
                        format!("{}/{service}/https://{host}/v1/{path}", running.url)
                    };
                    let client = if tunneled { &proxy } else { &http() };
                    let request = if path == "models" {
                        client.get(&url)
                    } else {
                        client.post(&url).body("{}")
                    };
                    let response = request
                        .bearer_auth("fake-provider-secret")
                        .send()
                        .await
                        .unwrap();
                    assert_eq!(
                        response.status(),
                        200,
                        "{url}: {}",
                        response.text().await.unwrap()
                    );
                    let connect = fixture.requests.recv().await.unwrap();
                    let authority = if host.contains(':') {
                        host.to_owned()
                    } else {
                        format!("{host}:443")
                    };
                    assert!(
                        connect.starts_with(&format!("CONNECT {authority} ")),
                        "{connect}"
                    );
                    let request = fixture.requests.recv().await.unwrap();
                    assert!(request.contains(&format!(" /v1/{path} ")), "{request}");
                    assert!(request.contains("authorization: Bearer fake-provider-secret"));
                }
            }
            // Account-only usage is not opened to provider credentials.
            let usage = if claude {
                "/v1/api/oauth/usage"
            } else {
                "/backend-api/wham/usage"
            };
            let response = proxy
                .get(format!("https://{host}{usage}"))
                .bearer_auth("fake-provider-secret")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 403);
            assert!(fixture.requests.try_recv().is_err());
            // A discovered destination still requires a matching credential.
            let response = proxy
                .get(format!("https://{host}/v1/models"))
                .bearer_auth("wrong-secret")
                .send()
                .await
                .unwrap();
            assert!(response.status().is_client_error());
            assert!(fixture.requests.try_recv().is_err());
            // Every record of every request names its app, from the first one on.
            let rows = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    running.server.logger.flush().unwrap();
                    let raw =
                        std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
                    let rows: Vec<serde_json::Value> = raw
                        .lines()
                        .filter_map(|l| serde_json::from_str(l).ok())
                        .filter(|row: &serde_json::Value| row["method"].is_string())
                        .collect();
                    let count = |end: bool| {
                        rows.iter()
                            .filter(|row| {
                                row["event"].as_str().is_some_and(|event| {
                                    (event == "request_received") != end
                                        && event.starts_with("request_")
                                })
                            })
                            .count()
                    };
                    if count(false) == count(true) {
                        return rows;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert!(rows.iter().any(|row| row["method"] == "CONNECT"));
            assert!(
                rows.iter()
                    .any(|row| row["event"] == "request_received" && row["method"] != "CONNECT")
            );
            assert!(rows.iter().any(|row| row["method"] == "CONNECT"
                && row["event"] == "request_finished"
                && row["service"] == if claude { "claude" } else { "codex" }));
            for row in &rows {
                if row["method"] == "CONNECT" {
                    // Named only by a configured first request in its tunnel.
                    assert!(
                        row.get("service").is_none()
                            || row["service"] == if claude { "claude" } else { "codex" },
                        "{row}"
                    );
                } else {
                    assert_eq!(
                        row["service"],
                        if claude { "claude" } else { "codex" },
                        "{row}"
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn named_provider_websocket_works_through_base_url_and_connect() {
    let mut fixture = fixture("http", "websocket").await;
    let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("config.toml"),
        "[model_providers.ShareCoder]\nbase_url = 'https://upstream.invalid/v1'\nexperimental_bearer_token = 'ws-secret'\n"
    ).unwrap();
    let running = running(&format!("connect_credential_routing: true\nproxies:\n  selected: {endpoint}\ncodex:\n  homes: [{}]\n  routing:\n    api_key: {{ShareCoder: selected}}\n", serde_json::to_string(home.path()).unwrap())).await;
    trust(&running, &fixture, &endpoint);
    let proxy = proxy_port_client(&running);
    for tunneled in [false, true] {
        let url = if tunneled {
            "https://upstream.invalid/v1/socket".into()
        } else {
            format!("{}/codex/https://upstream.invalid/v1/socket", running.url)
        };
        let direct = http();
        let client = if tunneled { &proxy } else { &direct };
        let response = client
            .get(url)
            .bearer_auth("ws-secret")
            .header("connection", "Upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .header("sec-websocket-protocol", "test")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 101);
        let mut socket = response.upgrade().await.unwrap();
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
        assert!(request.starts_with("GET /v1/socket HTTP/1.1"));
        assert!(request.contains("authorization: Bearer ws-secret"));
    }
}

// One public IP named by both a Codex provider and a Claude settings file stays
// unprefixed, so each request resolves the same app through either entry.
#[tokio::test]
async fn a_named_host_shared_by_both_apps_routes_like_its_base_url() {
    let mut fixture = fixture("http", "oauth_json").await;
    let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
    let home = tempfile::tempdir().unwrap();
    let host = "182.92.106.196:6060";
    std::fs::write(
        home.path().join("config.toml"),
        format!("[model_providers.myServer]\nbase_url = 'https://127.0.0.1:8787/https://{host}'\nexperimental_bearer_token = 'codex-secret'\n"),
    )
    .unwrap();
    std::fs::create_dir_all(home.path().join("profiles")).unwrap();
    std::fs::write(
        home.path().join("profiles").join("myServer.json"),
        json!({"env": {
            "ANTHROPIC_BASE_URL": format!("http://127.0.0.1:8787/https://{host}"),
            "ANTHROPIC_AUTH_TOKEN": "claude-secret"
        }})
        .to_string(),
    )
    .unwrap();
    let dir = serde_json::to_string(home.path()).unwrap();
    let running = running(&format!(
        "connect_credential_routing: true\nproxies:\n  selected: {endpoint}\ncodex:\n  homes: [{dir}]\n  routing:\n    api_key: {{myServer: selected}}\nclaude:\n  config_dirs: [{dir}]\n  routing:\n    api_key: {{myServer: selected}}\n"
    ))
    .await;
    trust(&running, &fixture, &endpoint);
    let proxy = proxy_port_client(&running);
    let base = http();
    for tunneled in [false, true] {
        let (client, origin) = if tunneled {
            (&proxy, format!("https://{host}"))
        } else {
            (&base, format!("{}/https://{host}", running.url))
        };
        for (path, secret) in [
            ("/responses", "codex-secret"),
            ("/v1/messages", "claude-secret"),
        ] {
            let response = client
                .post(format!("{origin}{path}"))
                .bearer_auth(secret)
                .body("{}")
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                200,
                "{origin}{path}: {}",
                response.text().await.unwrap()
            );
            let connect = fixture.requests.recv().await.unwrap();
            assert!(
                connect.starts_with(&format!("CONNECT {host} ")),
                "{connect}"
            );
            let request = fixture.requests.recv().await.unwrap();
            assert!(request.starts_with(&format!("POST {path} ")), "{request}");
            assert!(request.contains(secret), "{request}");
        }
        // Each app accepts only its own credential, through either entry.
        for (path, secret) in [
            ("/responses", "claude-secret"),
            ("/v1/messages", "codex-secret"),
        ] {
            let response = client
                .post(format!("{origin}{path}"))
                .bearer_auth(secret)
                .body("{}")
                .send()
                .await
                .unwrap();
            assert!(response.status().is_client_error(), "{origin}{path}");
            assert!(fixture.requests.try_recv().is_err());
        }
    }
}

async fn auto_refresh_setup(
    claude: bool,
    enabled: bool,
    mode: &'static str,
) -> (tempfile::TempDir, std::path::PathBuf, Running, Fixture) {
    let fixture = fixture("http", mode).await;
    let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
    let home = tempfile::tempdir().unwrap();
    let (filename, value, section) = if claude {
        (
            ".credentials.json",
            json!({"other":"preserved","claudeAiOauth":{"accessToken":"access-secret","refreshToken":"refresh-secret","scopes":["user:inference","user:profile"]}}),
            format!(
                "claude:\n  config_dirs: [{}]\n  routing:\n    account_fallback: selected\n    api_key: {{\"upstream.invalid/v1\": selected}}\n",
                serde_json::to_string(home.path()).unwrap()
            ),
        )
    } else {
        (
            "auth.json",
            json!({"other":"preserved","tokens":{"account_id":"acct-1","access_token":"access-secret","refresh_token":"refresh-secret"}}),
            format!(
                "codex:\n  homes: [{}]\n  base_url:\n    account: https://upstream.invalid/backend-api\n  routing:\n    account: {{acct-1: selected}}\n    api_key: {{\"upstream.invalid/v1\": selected}}\n",
                serde_json::to_string(home.path()).unwrap()
            ),
        )
    };
    let file = home.path().join(filename);
    std::fs::write(&file, value.to_string()).unwrap();
    let running = running(&format!(
        "auto_refresh_access_token: {enabled}\nproxies:\n  selected: {endpoint}\n{section}"
    ))
    .await;
    trust(&running, &fixture, &endpoint);
    (home, file, running, fixture)
}
fn auto_model_path(claude: bool) -> &'static str {
    if claude {
        "/anthropic/v1/messages"
    } else {
        "/responses"
    }
}
async fn auto_model(running: &Running, claude: bool) -> reqwest::Response {
    http()
        .post(format!("{}{}", running.url, auto_model_path(claude)))
        .bearer_auth("access-secret")
        .header("cookie", "private-cookie")
        .header("user-agent", "test-model-client")
        .body("model-body")
        .send()
        .await
        .unwrap()
}
fn take_requests(fixture: &mut Fixture) -> Vec<String> {
    let mut requests = Vec::new();
    while let Ok(request) = fixture.requests.try_recv() {
        if !request.starts_with("CONNECT ") {
            requests.push(request);
        }
    }
    requests
}
#[tokio::test]
async fn auto_refresh_rotates_saved_tokens_once_for_concurrent_and_repeated_model_calls() {
    for claude in [false, true] {
        let (_home, file, running, mut fixture) =
            auto_refresh_setup(claude, true, "auto_success").await;
        let (one, two) = tokio::join!(auto_model(&running, claude), auto_model(&running, claude));
        assert_eq!(one.status(), 200);
        assert_eq!(two.status(), 200);
        assert_eq!(
            auto_model(&running, claude).await.text().await.unwrap(),
            "model-result"
        );
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        let service = if claude {
            Service::Claude
        } else {
            Service::Codex
        };
        assert_eq!(
            crate::oauth_credentials::access(&saved, service),
            Some("new-access")
        );
        assert_eq!(
            crate::oauth_credentials::refresh(&saved, service),
            Some("new-refresh")
        );
        assert_eq!(saved["other"], "preserved");
        // A configured API gateway must receive the caller's original token,
        // even if it also happens to be an old OAuth alias in this process.
        let scope = if claude { "anthropic" } else { "codex" };
        let response = http()
            .post(format!(
                "{}/{scope}/https://upstream.invalid/v1/responses",
                running.url
            ))
            .bearer_auth("access-secret")
            .body("api-body")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
        let requests = take_requests(&mut fixture);
        let refreshes: Vec<_> = requests
            .iter()
            .filter(|r| r.contains("refresh-secret"))
            .collect();
        assert_eq!(refreshes.len(), 1, "{requests:?}");
        let refresh = refreshes[0];
        for forbidden in [
            "access-secret",
            "private-cookie",
            "test-model-client",
            "authorization:",
            "model-body",
        ] {
            assert!(!refresh.contains(forbidden));
        }
        if claude {
            let body: serde_json::Value =
                serde_json::from_str(refresh.split("\r\n\r\n").nth(1).unwrap()).unwrap();
            assert_eq!(body["scope"], "user:inference user:profile");
            assert_eq!(body["client_id"], "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
        } else {
            assert!(
                refresh
                    .contains("grant_type=refresh_token&refresh_token=refresh-secret&client_id=")
            );
        }
        assert!(
            requests
                .iter()
                .filter(|r| r.contains("authorization: Bearer new-access"))
                .all(|r| r.ends_with("model-body"))
        );
        running.server.logger.flush().unwrap();
        let log = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
        for secret in [
            "access-secret",
            "refresh-secret",
            "new-access",
            "new-refresh",
        ] {
            assert!(!log.contains(secret));
        }
        // A logout revokes old access-token aliases.
        std::fs::remove_file(file).unwrap();
        assert_ne!(auto_model(&running, claude).await.status(), 200);
    }
}
#[tokio::test]
async fn auto_refresh_is_opt_in_and_never_runs_for_usage_reads() {
    for claude in [false, true] {
        let (_home, file, running, mut fixture) =
            auto_refresh_setup(claude, false, "auto_success").await;
        let before = std::fs::read(&file).unwrap();
        assert_eq!(auto_model(&running, claude).await.status(), 401);
        assert_eq!(take_requests(&mut fixture).len(), 1);
        assert_eq!(std::fs::read(file).unwrap(), before);
        let (_home, file, running, mut fixture) =
            auto_refresh_setup(claude, true, "auto_success").await;
        let before = std::fs::read(&file).unwrap();
        let path = if claude {
            "/anthropic/api/oauth/usage"
        } else {
            "/wham/usage"
        };
        for _ in 0..3 {
            let response = http()
                .get(format!("{}{path}", running.url))
                .bearer_auth("access-secret")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 401);
        }
        assert_eq!(take_requests(&mut fixture).len(), 3);
        assert_eq!(std::fs::read(file).unwrap(), before);
    }
}
#[tokio::test]
async fn auto_refresh_failure_is_bounded_and_keeps_the_original_response() {
    for mode in [
        "auto_rejected",
        "auto_invalid",
        "auto_rate_limit",
        "auto_retry_401",
    ] {
        let (_home, file, running, mut fixture) = auto_refresh_setup(false, true, mode).await;
        let before = std::fs::read(&file).unwrap();
        for _ in 0..2 {
            let response = auto_model(&running, false).await;
            assert_eq!(response.status(), 401);
            assert_eq!(response.text().await.unwrap(), "original-unauthorized");
        }
        let requests = take_requests(&mut fixture);
        // A second 401 after rotation is allowed one refresh on its newer
        // generation, but the original request itself is never retried twice.
        let count = requests
            .iter()
            .filter(|r| r.starts_with("POST /oauth/token "))
            .count();
        assert_eq!(
            count,
            if mode == "auto_retry_401" { 2 } else { 1 },
            "{mode}"
        );
        if mode != "auto_retry_401" {
            assert_eq!(std::fs::read(file).unwrap(), before);
        }
    }
}
/// Waits for the proxy's refresh request to reach the upstream fixture.
async fn await_refresh(fixture: &mut Fixture) {
    loop {
        let request = fixture.requests.recv().await.unwrap();
        if request.starts_with("POST /oauth/token ") || request.starts_with("POST /v1/oauth/token ")
        {
            break;
        }
    }
}
fn spawn_auto_model(running: &Running, claude: bool) -> tokio::task::JoinHandle<reqwest::Response> {
    let url = format!("{}{}", running.url, auto_model_path(claude));
    tokio::spawn(async move {
        http()
            .post(url)
            .bearer_auth("access-secret")
            .body("model-body")
            .send()
            .await
            .unwrap()
    })
}
fn read_json(file: &std::path::Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap()
}
#[tokio::test]
async fn auto_refresh_does_not_overwrite_an_external_account_switch() {
    for claude in [false, true] {
        let (_home, file, running, mut fixture) =
            auto_refresh_setup(claude, true, "auto_wait").await;
        let call = spawn_auto_model(&running, claude);
        await_refresh(&mut fixture).await;
        let switched = if claude {
            r#"{"claudeAiOauth":{"accessToken":"external-access","refreshToken":"external-refresh"}}"#
        } else {
            r#"{"tokens":{"account_id":"another","access_token":"external-access","refresh_token":"external-refresh"}}"#
        };
        std::fs::write(&file, switched).unwrap();
        fixture.release.notify_one();
        assert_eq!(call.await.unwrap().status(), 401);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), switched);
        assert!(take_requests(&mut fixture).is_empty());
    }
}
#[tokio::test]
async fn auto_refresh_merges_its_rotation_into_unrelated_edits_made_meanwhile() {
    for claude in [false, true] {
        let (home, file, running, mut fixture) =
            auto_refresh_setup(claude, true, "auto_wait").await;
        let locks = crate::claude_refresh_lock::lock_paths(home.path());
        let call = spawn_auto_model(&running, claude);
        await_refresh(&mut fixture).await;
        // Claude Code's lock is held for the whole Claude renewal, and only
        // for Claude: Codex has no such protocol.
        assert_eq!(locks.iter().all(|lock| lock.is_dir()), claude);
        // Another program changes an unrelated part of the saved login while
        // the refresh token is being spent, e.g. Claude Code's MCP logins.
        let mut edited = read_json(&file);
        edited["mcpOAuth"] = json!({"server":{"accessToken":"mcp-access"}});
        let section = if claude { "claudeAiOauth" } else { "tokens" };
        edited[section]["clientId"] = "kept-client".into();
        std::fs::write(&file, edited.to_string()).unwrap();
        fixture.release.notify_one();
        let response = call.await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.text().await.unwrap(), "model-result");
        let saved = read_json(&file);
        let service = if claude {
            Service::Claude
        } else {
            Service::Codex
        };
        assert_eq!(
            crate::oauth_credentials::access(&saved, service),
            Some("new-access")
        );
        assert_eq!(
            crate::oauth_credentials::refresh(&saved, service),
            Some("new-refresh")
        );
        assert_eq!(saved["mcpOAuth"]["server"]["accessToken"], "mcp-access");
        assert_eq!(saved[section]["clientId"], "kept-client");
        assert_eq!(saved["other"], "preserved");
        assert!(locks.iter().all(|lock| !lock.exists()));
        // Requests still carrying the old access token use the rotation.
        assert_eq!(auto_model(&running, claude).await.status(), 200);
        let refreshes = take_requests(&mut fixture)
            .iter()
            .filter(|r| r.contains("refresh-secret"))
            .count();
        assert_eq!(refreshes, 0);
    }
}
#[tokio::test]
async fn auto_refresh_waits_for_claude_code_and_never_spends_the_token_it_renewed() {
    let (home, file, running, mut fixture) = auto_refresh_setup(true, true, "auto_success").await;
    let locks = crate::claude_refresh_lock::lock_paths(home.path());
    // Claude Code holds its refresh lock (both directories) and is renewing.
    for lock in &locks {
        std::fs::create_dir(lock).unwrap();
    }
    let call = spawn_auto_model(&running, true);
    // Claude Code renews once the model call has been refused, while this
    // server waits for its lock; a fixed delay raced both.
    loop {
        let request = fixture.requests.recv().await.unwrap();
        if request.starts_with("POST /v1/messages") {
            break;
        }
    }
    let renewed = json!({"other":"preserved","claudeAiOauth":{"accessToken":"code-access","refreshToken":"code-refresh","expiresAt":4102444800000u64}});
    std::fs::write(&file, renewed.to_string()).unwrap();
    for lock in locks.iter().rev() {
        std::fs::remove_dir(lock).unwrap();
    }
    // The request is left for the client to retry with its current login.
    let response = call.await.unwrap();
    assert_eq!(response.status(), 401);
    assert_eq!(response.text().await.unwrap(), "original-unauthorized");
    assert_eq!(read_json(&file), renewed);
    assert!(
        !take_requests(&mut fixture)
            .iter()
            .any(|r| r.contains("/oauth/token")),
        "the spent refresh token must not be sent again"
    );
    assert!(locks.iter().all(|lock| !lock.exists()));
}
#[cfg(unix)]
#[tokio::test]
async fn auto_refresh_takes_over_from_a_claude_code_that_died_holding_its_lock() {
    let (home, file, running, mut fixture) = auto_refresh_setup(true, true, "auto_success").await;
    // Claude Code exited mid-renewal seconds ago: its locks are far from
    // stale, but its owner record proves it gone.
    let locks = crate::claude_refresh_lock::hold_as_dead_claude_code(home.path());
    let response = auto_model(&running, true).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "model-result");
    assert_eq!(
        crate::oauth_credentials::access(&read_json(&file), Service::Claude),
        Some("new-access")
    );
    let refreshes = take_requests(&mut fixture)
        .iter()
        .filter(|r| r.contains("refresh-secret"))
        .count();
    assert_eq!(refreshes, 1);
    assert!(locks.iter().all(|lock| !lock.exists()));
    assert!(
        !home
            .path()
            .join(crate::claude_refresh_lock::OWNER_FILE)
            .exists()
    );
}
#[tokio::test]
async fn auto_refresh_gives_up_while_another_renewal_holds_the_lock() {
    let (home, file, running, mut fixture) = auto_refresh_setup(true, true, "auto_success").await;
    let before = std::fs::read(&file).unwrap();
    let locks = crate::claude_refresh_lock::lock_paths(home.path());
    // Held by a live process: the current lock alone, or the legacy one alone.
    for held in [&locks[..], &locks[..1], &locks[1..]] {
        for lock in held {
            std::fs::create_dir(lock).unwrap();
        }
        let response = auto_model(&running, true).await;
        assert_eq!(response.status(), 401);
        assert_eq!(response.text().await.unwrap(), "original-unauthorized");
        assert_eq!(std::fs::read(&file).unwrap(), before);
        for lock in &locks {
            // Another holder's lock is never removed, nor ours left behind.
            assert_eq!(lock.exists(), held.contains(lock));
        }
        for lock in held {
            std::fs::remove_dir(lock).unwrap();
        }
    }
    assert!(
        !take_requests(&mut fixture)
            .iter()
            .any(|r| r.contains("/oauth/token"))
    );
    // A dead holder's stale lock does not block renewal for good.
    for lock in &locks {
        std::fs::create_dir(lock).unwrap();
        crate::claude_refresh_lock::touch(
            lock,
            std::time::SystemTime::now() - Duration::from_secs(120),
        )
        .unwrap();
    }
    assert_eq!(auto_model(&running, true).await.status(), 200);
    assert_eq!(
        crate::oauth_credentials::access(&read_json(&file), Service::Claude),
        Some("new-access")
    );
    assert!(locks.iter().all(|lock| !lock.exists()));
}

#[tokio::test]
async fn auto_refresh_ignores_other_statuses_and_logins_without_refresh_tokens() {
    for claude in [false, true] {
        for status in ["status_403", "status_429", "status_500"] {
            let (_home, file, running, mut fixture) =
                auto_refresh_setup(claude, true, status).await;
            let original = std::fs::read(&file).unwrap();
            let response = auto_model(&running, claude).await;
            assert_eq!(
                response.status().as_u16().to_string(),
                status.trim_start_matches("status_")
            );
            assert_eq!(take_requests(&mut fixture).len(), 1);
            assert_eq!(std::fs::read(file).unwrap(), original);
        }
        let (_home, file, running, mut fixture) =
            auto_refresh_setup(claude, true, "auto_success").await;
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        if claude {
            value["claudeAiOauth"]
                .as_object_mut()
                .unwrap()
                .remove("refreshToken");
        } else {
            value["tokens"]
                .as_object_mut()
                .unwrap()
                .remove("refresh_token");
        }
        std::fs::write(&file, value.to_string()).unwrap();
        assert_eq!(auto_model(&running, claude).await.status(), 401);
        assert_eq!(take_requests(&mut fixture).len(), 1);
    }
}

#[tokio::test]
async fn claude_auto_rotation_registers_the_new_token_for_manual_renewal() {
    let mut lookup = fixture("http", "sse").await;
    let mut upstream = fixture("http", "auto_success").await;
    let lookup_url = format!("http://{}", lookup.addr);
    let account_url = format!("http://{}", upstream.addr);
    let home = tempfile::tempdir().unwrap();
    let file = home.path().join(".credentials.json");
    std::fs::write(
        &file,
        json!({"claudeAiOauth":{"accessToken":"access-secret","refreshToken":"refresh-secret"}})
            .to_string(),
    )
    .unwrap();
    let config = format!(
        "auto_refresh_access_token: true\nproxies:\n  selected: {account_url}\n  probe: {lookup_url}\nclaude:\n  account_auth_file_only: false\n  config_dirs: [{}]\n  routing:\n    account: {{remote@example.invalid: selected}}\n    account_probe: probe\n",
        serde_json::to_string(home.path()).unwrap()
    );
    let active = running(&config).await;
    trust(&active, &lookup, &lookup_url);
    trust(&active, &upstream, &account_url);
    assert_eq!(auto_model(&active, true).await.status(), 200);
    let requests = take_requests(&mut upstream);
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.starts_with("POST /v1/oauth/token "))
            .count(),
        1
    );
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
    let rotated = saved["claudeAiOauth"]["refreshToken"].as_str().unwrap();
    assert_eq!(rotated, "new-refresh");
    let cached = active.server.refresh_routes.identity(rotated);
    let renew = || {
        http()
            .post(format!("{}/anthropic/v1/oauth/token", active.url))
            .header("content-type", "application/json")
            .body(json!({"grant_type":"refresh_token","refresh_token":rotated}).to_string())
            .send()
    };
    let response = renew().await.unwrap();
    assert_eq!(cached.unwrap().usernames, ["remote@example.invalid"]);
    assert_eq!(response.status(), 200);
    response.bytes().await.unwrap();
    assert_eq!(
        take_requests(&mut upstream)
            .iter()
            .filter(|r| r.starts_with("POST /v1/oauth/token "))
            .count(),
        1
    );

    // An earlier daemon may already have saved a login with no cached route.
    let restarted = running(&config).await;
    take_requests(&mut lookup);
    trust(&restarted, &lookup, &lookup_url);
    trust(&restarted, &upstream, &account_url);
    assert!(restarted.server.refresh_routes.identity(rotated).is_none());
    let response = http()
        .post(format!("{}/anthropic/v1/oauth/token", restarted.url))
        .header("content-type", "application/json")
        .body(json!({"grant_type":"refresh_token","refresh_token":rotated}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    response.bytes().await.unwrap();
    let recovery_probe = take_requests(&mut lookup);
    assert_eq!(recovery_probe.len(), 1);
    assert!(recovery_probe[0].starts_with("GET /api/oauth/profile "));
    assert!(recovery_probe[0].contains("authorization: Bearer new-access"));
    assert_eq!(
        take_requests(&mut upstream)
            .iter()
            .filter(|r| r.starts_with("POST /v1/oauth/token "))
            .count(),
        1
    );
    // A fresh daemon receiving Copool's profile request first identifies the
    // bearer via the probe, then forwards the original GET via the account.
    let cold = running(&config).await;
    trust(&cold, &lookup, &lookup_url);
    trust(&cold, &upstream, &account_url);
    let response = http()
        .get(format!("{}/anthropic/api/oauth/profile", cold.url))
        .bearer_auth("new-access")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    for received in [take_requests(&mut lookup), take_requests(&mut upstream)] {
        assert_eq!(received.len(), 1);
        assert!(received[0].starts_with("GET /api/oauth/profile "));
        assert!(received[0].contains("authorization: Bearer new-access"));
    }
}

/// The log's first record of `event`, once written.
async fn first_record(running: &Running, event: &str) -> serde_json::Value {
    let path = running._temp.path().join("proxy.log");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            running.server.logger.flush().unwrap();
            let raw = std::fs::read_to_string(&path).unwrap_or_default();
            if let Some(row) = raw
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .find(|row| row["event"] == event)
            {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "no {event}: {}",
            std::fs::read_to_string(&path).unwrap_or_default()
        )
    })
}

// Cloud Code hosts are reached only through the loopback fixture proxy; no
// Google host, real login or credential store is contacted.
#[tokio::test]
async fn antigravity_saved_login_streams_through_its_route_and_records_gemini_usage() {
    let _login = crate::antigravity::TEST_LOGIN.lock().await;
    crate::antigravity::set_test_login(crate::antigravity::TEST_SAVED_TOKEN, "saved-refresh");
    let token = crate::antigravity::TEST_SAVED_TOKEN;
    let mut fixture = fixture("http", "gemini_sse").await;
    let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
    let running = running(&format!(
        "proxies:\n  selected: {endpoint}\nantigravity:\n  routing:\n    account:\n      default: selected\n"
    ))
    .await;
    trust(&running, &fixture, &endpoint);
    let response = http()
        .post(format!(
            "{}/antigravity/v1internal:streamGenerateContent?alt=sse",
            running.url
        ))
        .bearer_auth(token)
        .header("cookie", "private-cookie")
        .body(r#"{"model":"gemini-request","project":"p","request":{"contents":"PRIVATE PROMPT"}}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();
    assert!(body.contains("private-output") && body.contains("usageMetadata"));
    let connect = fixture.requests.recv().await.unwrap();
    assert!(connect.starts_with("CONNECT daily-cloudcode-pa.googleapis.com:443 "));
    let forwarded = fixture.requests.recv().await.unwrap();
    assert!(forwarded.starts_with("POST /v1internal:streamGenerateContent?alt=sse HTTP/1.1"));
    assert!(forwarded.contains(&format!("authorization: Bearer {token}")));
    assert!(!forwarded.contains("private-cookie"));
    assert!(forwarded.ends_with("\"PRIVATE PROMPT\"}}"));
    let call = first_record(&running, "model_call_finished").await;
    assert_eq!(call["service"], "antigravity");
    assert_eq!(call["account_label"], "default");
    assert_eq!(call["credential_match"], "configured");
    assert_eq!(call["proxy"], "selected");
    assert_eq!(call["model"], "gemini-test");
    assert_eq!(call["response_id"], "gemini-response");
    assert_eq!(call["model_finish_reason"], "STOP");
    // Input includes cached tokens; output includes thoughts.
    assert_eq!(call["input_tokens"], "12");
    assert_eq!(call["output_tokens"], "8");
    assert_eq!(call["cached_input_tokens"], "4");
    let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
    assert!(!raw.contains(token) && !raw.contains("PRIVATE PROMPT"));

    // Another token, a foreign host and an unconfigured section send nothing.
    let other = http()
        .post(format!(
            "{}/antigravity/v1internal:loadCodeAssist",
            running.url
        ))
        .bearer_auth("other-token")
        .send()
        .await
        .unwrap();
    assert_eq!(other.status(), 401);
    let foreign = http()
        .post(format!(
            "{}/antigravity/https://example.com/v1internal:loadCodeAssist",
            running.url
        ))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(foreign.status(), 502);
    assert!(fixture.requests.try_recv().is_err());
    let unconfigured = running_with_tls("", |_| {}).await;
    let response = http()
        .post(format!(
            "{}/antigravity/v1internal:loadCodeAssist",
            unconfigured.url
        ))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
}

// agy's account lookup and AI Code flags, named by `USER_INFO_OVERRIDE_URL`
// and `AICODE_ENDPOINT_URL`, take the saved login's route as its Cloud Code
// requests; only the loopback fixture proxy is reached.
#[tokio::test]
async fn antigravity_login_endpoints_take_the_saved_logins_route() {
    let _login = crate::antigravity::TEST_LOGIN.lock().await;
    crate::antigravity::set_test_login(crate::antigravity::TEST_SAVED_TOKEN, "saved-refresh");
    let token = crate::antigravity::TEST_SAVED_TOKEN;
    let mut fixture = fixture("http", "gemini_sse").await;
    let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
    let running = running(&format!(
        "connect_credential_routing: true\nproxies:\n  selected: {endpoint}\nantigravity:\n  routing:\n    account:\n      default: selected\n"
    ))
    .await;
    trust(&running, &fixture, &endpoint);
    for (url, host, line) in [
        (
            crate::antigravity::USERINFO_URL,
            "www.googleapis.com",
            "GET /oauth2/v2/userinfo HTTP/1.1",
        ),
        (
            crate::antigravity::AICODE_FETCH_CONFIG,
            "aicode.googleapis.com",
            "GET /v1alpha:fetchConfig HTTP/1.1",
        ),
    ] {
        let response = http()
            .get(format!("{}/antigravity/{url}", running.url))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{url}");
        let connect = fixture.requests.recv().await.unwrap();
        assert!(
            connect.starts_with(&format!("CONNECT {host}:443 ")),
            "{connect}"
        );
        let forwarded = fixture.requests.recv().await.unwrap();
        assert!(forwarded.starts_with(line), "{forwarded}");
        assert!(forwarded.contains(&format!("authorization: Bearer {token}")));
    }
    // A proxy-port client sends the public URL without USER_INFO_OVERRIDE_URL.
    let proxy_client = proxy_port_client(&running);
    let userinfo = proxy_client
        .get(crate::antigravity::USERINFO_URL)
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(userinfo.contains("remote@example.invalid"));
    assert!(
        fixture
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("CONNECT www.googleapis.com:443 ")
    );
    assert!(
        fixture
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("GET /oauth2/v2/userinfo HTTP/1.1")
    );
    let denied = proxy_client
        .get("https://www.googleapis.com/drive/v3/files")
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 502);
    // Other paths on their hosts, and other tokens, send nothing.
    for (path, auth, status) in [
        ("https://www.googleapis.com/drive/v3/files", token, 502),
        ("https://aicode.googleapis.com/v1alpha:other", token, 502),
        (crate::antigravity::USERINFO_URL, "other-token", 401),
    ] {
        let response = http()
            .get(format!("{}/antigravity/{path}", running.url))
            .bearer_auth(auth)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{path}");
    }
    assert!(fixture.requests.try_recv().is_err());
    // They are not model calls, and the log keeps no token.
    running.server.logger.flush().unwrap();
    let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
    assert!(!raw.contains("model_call_finished") && !raw.contains(token));
}

// agy's Gemini API key requests (`modelProvider: "gemini"`) reach a relay
// through the loopback fixture proxy; no relay, Google host or login is used.
#[tokio::test]
async fn antigravity_api_key_routes_relay_the_key_as_sent_through_their_route() {
    const KEY: &str = "AIzaSyFakeGeminiKey-0123456789abcdefXYZ";
    let mut fixture = fixture("http", "gemini_sse").await;
    let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
    // No account route, saved login or fallback: only the API route.
    let running = running(&format!(
        "proxies:\n  selected: {endpoint}\nantigravity:\n  saved_login: false\n  routing:\n    api_key:\n      upstream.invalid/antigravity: selected\n"
    ))
    .await;
    trust(&running, &fixture, &endpoint);
    let model = "/antigravity/v1beta/models/gemini-x:streamGenerateContent";
    let send = |path: String, key: Option<(&'static str, String)>| {
        let mut request = http()
            .post(format!("{}{path}", running.url))
            .header("cookie", "private-cookie")
            .body(r#"{"contents":[{"parts":[{"text":"PRIVATE PROMPT"}]}]}"#);
        if let Some((name, value)) = key {
            request = request.header(name, value);
        }
        request.send()
    };
    // As agy sends it: x-goog-api-key, relayed in place.
    let response = send(
        format!("/antigravity/https://upstream.invalid{model}?alt=sse"),
        Some(("x-goog-api-key", KEY.into())),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.text().await.unwrap().contains("usageMetadata"));
    let connect = fixture.requests.recv().await.unwrap();
    assert!(connect.starts_with("CONNECT upstream.invalid:443 "));
    assert!(!connect.contains(KEY));
    let forwarded = fixture.requests.recv().await.unwrap();
    assert!(forwarded.starts_with(&format!("POST {model}?alt=sse HTTP/1.1")));
    assert!(forwarded.contains(&format!("x-goog-api-key: {KEY}")));
    assert!(!forwarded.to_lowercase().contains("authorization"));
    assert!(!forwarded.contains("private-cookie"));
    assert!(forwarded.ends_with("\"PRIVATE PROMPT\"}]}]}"));
    let call = first_record(&running, "model_call_finished").await;
    assert_eq!(call["service"], "antigravity");
    assert_eq!(call["provider"], "upstream.invalid/antigravity");
    assert_eq!(
        call["upstream_base_url"],
        "https://upstream.invalid/antigravity"
    );
    assert_eq!(call["proxy"], "selected");
    assert_eq!(call["credential_match"], "unmatched");
    assert_eq!(
        call["credential_ref"].as_str(),
        crate::identity::shared_api_reference(
            "Antigravity",
            "https://upstream.invalid/antigravity",
            KEY
        )
        .as_deref()
    );
    assert_eq!(call["model"], "gemini-test");
    assert_eq!(call["input_tokens"], "12");
    assert_eq!(call["output_tokens"], "8");
    assert!(call.get("account_id").is_none() && call.get("account_label").is_none());

    // A Bearer key, and the unscoped form a proxy-port client sends.
    let response = send(
        format!("/https://upstream.invalid{model}"),
        Some(("authorization", format!("Bearer {KEY}"))),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), 200);
    response.text().await.unwrap();
    assert!(
        fixture
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("CONNECT upstream.invalid:443 ")
    );
    let forwarded = fixture.requests.recv().await.unwrap();
    assert!(forwarded.starts_with(&format!("POST {model} HTTP/1.1")));
    assert!(forwarded.contains(&format!("authorization: Bearer {KEY}")));
    assert!(!forwarded.contains("x-goog-api-key"));
    // A key in the query stays there, unlogged.
    let response = send(
        format!("/antigravity/https://upstream.invalid{model}?key={KEY}"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(response.status(), 200);
    response.text().await.unwrap();
    fixture.requests.recv().await.unwrap();
    let forwarded = fixture.requests.recv().await.unwrap();
    assert!(forwarded.starts_with(&format!("POST {model}?key={KEY} HTTP/1.1")));
    assert!(!forwarded.to_lowercase().contains("authorization"));

    // No key, two keys, another host, path or port, and a Cloud Code target
    // without a login route send nothing.
    for (path, key, status) in [
        (
            format!("/antigravity/https://upstream.invalid{model}"),
            None,
            401,
        ),
        (
            format!("/antigravity/https://upstream.invalid{model}?key={KEY}"),
            Some(("x-goog-api-key", KEY.to_owned())),
            400,
        ),
        (
            format!("/antigravity/https://upstream.invalid{model}?key={KEY}&key=other-gemini-key"),
            None,
            400,
        ),
        (
            format!("/antigravity/https://other.invalid{model}"),
            Some(("x-goog-api-key", KEY.to_owned())),
            404,
        ),
        (
            "/antigravity/https://upstream.invalid/other/v1beta/models/g:generateContent".into(),
            Some(("x-goog-api-key", KEY.to_owned())),
            404,
        ),
        (
            format!("/antigravity/https://upstream.invalid:8443{model}"),
            Some(("x-goog-api-key", KEY.to_owned())),
            404,
        ),
        (
            "/antigravity/v1internal:generateContent".into(),
            Some(("authorization", format!("Bearer {KEY}"))),
            404,
        ),
    ] {
        let response = send(path.clone(), key).await.unwrap();
        assert_eq!(response.status(), status, "{path}");
    }
    assert!(fixture.requests.try_recv().is_err());
    running.server.logger.flush().unwrap();
    let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
    assert!(!raw.contains(KEY) && !raw.contains("PRIVATE PROMPT"));
}

// As Claude's environment selectors: a key named by a variable takes its
// route to `base_url.api_key`, another `fallback`. `cargo test` sets
// `CARGO_PKG_NAME` in this process, so no variable is set and no login shell
// runs; the relay is the loopback fixture.
#[tokio::test]
async fn antigravity_named_api_keys_route_by_value_and_others_take_fallback() {
    let named = std::env::var("CARGO_PKG_NAME").expect("cargo test sets CARGO_PKG_NAME");
    const OTHER: &str = "other-gemini-key";
    let mut fixture = fixture("http", "gemini_sse").await;
    let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
    let running = running(&format!(
        "proxies:\n  named: {endpoint}\n  spare: {endpoint}\nantigravity:\n  saved_login: false\n  account_auth_file_only: false\n  base_url:\n    api_key: https://upstream.invalid/gemini\n  routing:\n    api_key:\n      CARGO_PKG_NAME: named\n    fallback: spare\n"
    ))
    .await;
    trust(&running, &fixture, &endpoint);
    let model = "/v1beta/models/gemini-x:streamGenerateContent";
    let send = |path: String, key: Option<String>| {
        let mut request = http()
            .post(format!("{}{path}", running.url))
            .body(r#"{"contents":[{"parts":[{"text":"PRIVATE PROMPT"}]}]}"#);
        if let Some(key) = key {
            request = request.header("x-goog-api-key", key);
        }
        request.send()
    };
    // agy pointed at /antigravity sends origin-form requests.
    for (path, key, forwarded_path) in [
        (
            format!("/antigravity{model}?alt=sse"),
            Some(named.clone()),
            format!("/gemini{model}?alt=sse"),
        ),
        (
            format!("/antigravity{model}?key={OTHER}"),
            None,
            format!("/gemini{model}?key={OTHER}"),
        ),
        // The explicit form may name the API base.
        (
            format!("/antigravity/https://upstream.invalid/gemini{model}"),
            Some(named.clone()),
            format!("/gemini{model}"),
        ),
    ] {
        let response = send(path.clone(), key.clone()).await.unwrap();
        assert_eq!(response.status(), 200, "{path}");
        response.text().await.unwrap();
        let connect = fixture.requests.recv().await.unwrap();
        assert!(connect.starts_with("CONNECT upstream.invalid:443 "));
        let forwarded = fixture.requests.recv().await.unwrap();
        assert!(
            forwarded.starts_with(&format!("POST {forwarded_path} HTTP/1.1")),
            "{forwarded}"
        );
        if let Some(key) = key {
            assert!(forwarded.contains(&format!("x-goog-api-key: {key}")));
        }
        assert!(!forwarded.to_lowercase().contains("authorization"));
    }
    // A named key goes to the upstream its request names, through its own
    // route, as a Claude settings file's key goes to that file's upstream;
    // also as agy sends it, its base URL's `//` collapsed into the path.
    for path in [
        format!("/antigravity/https://relay.invalid/gemini{model}"),
        format!("/antigravity/https:/relay.invalid/gemini{model}"),
    ] {
        let response = send(path.clone(), Some(named.clone())).await.unwrap();
        assert_eq!(response.status(), 200, "{path}");
        response.text().await.unwrap();
        let connect = fixture.requests.recv().await.unwrap();
        assert!(
            connect.starts_with("CONNECT relay.invalid:443 "),
            "{connect}"
        );
        let forwarded = fixture.requests.recv().await.unwrap();
        assert!(
            forwarded.starts_with(&format!("POST /gemini{model} HTTP/1.1")),
            "{forwarded}"
        );
        assert!(forwarded.contains("host: relay.invalid\r\n"), "{forwarded}");
        assert!(forwarded.contains(&format!("x-goog-api-key: {named}")));
    }
    // A key `fallback` carries reaches only the API base, and an ambiguous
    // key goes nowhere: neither sends anything.
    let fallback_elsewhere = send(
        format!("/antigravity/https://relay.invalid/gemini{model}?key={OTHER}"),
        None,
    )
    .await
    .unwrap();
    assert_ne!(fallback_elsewhere.status(), 200);
    let twice = send(
        format!("/antigravity{model}?key={OTHER}"),
        Some(named.clone()),
    )
    .await
    .unwrap();
    assert_eq!(twice.status(), 400);
    assert!(fixture.requests.try_recv().is_err());

    let log = running._temp.path().join("proxy.log");
    let (raw, calls) = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            running.server.logger.flush().unwrap();
            let raw = std::fs::read_to_string(&log).unwrap_or_default();
            let calls: Vec<serde_json::Value> = raw
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .filter(|row| row["event"] == "model_call_finished")
                .collect();
            if calls.len() >= 5 {
                return (raw, calls);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(calls.len(), 5);
    for call in &calls {
        assert_eq!(call["service"], "antigravity");
        assert_eq!(call["input_tokens"], "12");
    }
    let named_calls: Vec<_> = calls
        .iter()
        .filter(|call| call["provider"] == "CARGO_PKG_NAME")
        .collect();
    assert_eq!(named_calls.len(), 4);
    for call in &named_calls {
        assert_eq!(call["proxy"], "named");
        assert_eq!(call["credential_match"], "configured");
    }
    let bases: Vec<_> = named_calls
        .iter()
        .map(|call| call["upstream_base_url"].as_str().unwrap())
        .collect();
    assert_eq!(
        bases,
        [
            "https://upstream.invalid/gemini",
            "https://upstream.invalid/gemini",
            "https://relay.invalid",
            "https://relay.invalid",
        ]
    );
    let fallback = calls
        .iter()
        .find(|call| call["provider"] == crate::antigravity::API_KEY_FALLBACK)
        .unwrap();
    assert_eq!(
        fallback["upstream_base_url"],
        "https://upstream.invalid/gemini"
    );
    assert_eq!(fallback["proxy"], "spare");
    assert_eq!(fallback["credential_match"], "unmatched");
    assert!(!raw.contains(OTHER) && !raw.contains("PRIVATE PROMPT"));
}

#[tokio::test]
async fn antigravity_other_accounts_are_named_through_the_probe_then_routed_and_cached() {
    let mut lookup = fixture("http", "sse").await;
    let mut payload = fixture("http", "gemini_sse").await;
    let lookup_endpoint = format!("http://127.0.0.1:{}", lookup.addr.port());
    let payload_endpoint = format!("http://127.0.0.1:{}", payload.addr.port());
    let running = running(&format!(
        "proxies:\n  lookup: {lookup_endpoint}\n  selected: {payload_endpoint}\nantigravity:\n  saved_login: false\n  account_auth_file_only: false\n  routing:\n    account:\n      Remote@Example.invalid: selected\n    account_probe: lookup\n"
    ))
    .await;
    trust(&running, &lookup, &lookup_endpoint);
    trust(&running, &payload, &payload_endpoint);
    for (token, expect_lookup) in [("first-secret", true), ("first-secret", false)] {
        let response = http()
            .post(format!(
                "{}/antigravity/https://cloudcode-pa.googleapis.com/v1internal:generateContent",
                running.url
            ))
            .bearer_auth(token)
            .body("private-payload")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        if expect_lookup {
            let connect = lookup.requests.recv().await.unwrap();
            assert!(connect.starts_with("CONNECT www.googleapis.com:443 "));
            let request = lookup.requests.recv().await.unwrap();
            assert!(request.starts_with("GET /oauth2/v2/userinfo HTTP/1.1"));
            assert!(request.contains(&format!("authorization: Bearer {token}")));
            assert!(!request.contains("private-payload"));
        }
        assert!(lookup.requests.try_recv().is_err());
        let connect = payload.requests.recv().await.unwrap();
        assert!(connect.starts_with("CONNECT cloudcode-pa.googleapis.com:443 "));
        let request = payload.requests.recv().await.unwrap();
        assert!(request.starts_with("POST /v1internal:generateContent HTTP/1.1"));
        assert!(request.ends_with("private-payload"));
    }
    assert_eq!(running.server.antigravity_profiles.lock().unwrap().len(), 1);
    let call = first_record(&running, "model_call_finished").await;
    assert_eq!(call["account_id"], "remote@example.invalid");
    assert_eq!(call["account_label"], "Remote@Example.invalid");
    assert_eq!(call["credential_match"], "configured");
    assert_eq!(call["profile_proxy"], "lookup");
}

/// The lookup of `token`'s account, and nothing more, reached `lookup`.
fn expect_lookup(lookup: &mut Fixture, token: &str) {
    let connect = lookup.requests.try_recv().unwrap();
    assert!(connect.starts_with("CONNECT www.googleapis.com:443 "));
    let request = lookup.requests.try_recv().unwrap();
    assert!(request.starts_with("GET /oauth2/v2/userinfo HTTP/1.1"));
    assert!(request.contains(&format!("authorization: Bearer {token}")));
    assert!(lookup.requests.try_recv().is_err());
}
/// A Cloud Code request with `token` reached `payload`.
async fn expect_payload(payload: &mut Fixture, token: &str) {
    let connect = payload.requests.recv().await.unwrap();
    assert!(connect.starts_with("CONNECT daily-cloudcode-pa.googleapis.com:443 "));
    let request = payload.requests.recv().await.unwrap();
    assert!(request.contains(&format!("authorization: Bearer {token}")));
}
/// A request to Antigravity's scope with `token`, which must succeed.
async fn antigravity_request(running: &Running, token: &str) {
    let response = http()
        .post(format!(
            "{}/antigravity/v1internal:generateContent",
            running.url
        ))
        .bearer_auth(token)
        .body("private-payload")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
}

// The token rotation lock is shared by every service: a model call must not
// hold it while its account is looked up, or a renewal waiting for it holds
// up every other model call behind that remote request.
#[tokio::test]
async fn an_antigravity_account_lookup_holds_no_rotation_lock() {
    let _login = crate::antigravity::TEST_LOGIN.lock().await;
    let mut lookup = fixture("http", "delayed_headers").await;
    let endpoint = format!("http://127.0.0.1:{}", lookup.addr.port());
    let running = running(&format!(
        "auto_refresh_access_token: true\nproxies:\n  lookup: {endpoint}\nantigravity:\n  routing:\n    account:\n      remote@example.invalid: none\n    account_probe: lookup\n"
    ))
    .await;
    trust(&running, &lookup, &endpoint);
    crate::antigravity::set_test_login("unnamed-access", "unnamed-refresh");
    let call = tokio::spawn(
        http()
            .post(format!(
                "{}/antigravity/v1internal:generateContent",
                running.url
            ))
            .bearer_auth("unnamed-access")
            .body("private-payload")
            .send(),
    );
    // The lookup is in flight, and is never answered.
    assert!(
        lookup
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("CONNECT www.googleapis.com:443 ")
    );
    assert!(
        lookup
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("GET /oauth2/v2/userinfo ")
    );
    let renewal = tokio::time::timeout(
        Duration::from_secs(2),
        running.server.auto_refresh.rotation.write(),
    )
    .await;
    assert!(
        renewal.is_ok(),
        "the rotation lock is held across the lookup"
    );
    drop(renewal);
    call.abort();
    crate::antigravity::set_test_login(crate::antigravity::TEST_SAVED_TOKEN, "saved-refresh");
}

#[tokio::test]
async fn antigravity_saved_login_is_named_once_per_login_across_renewals_and_restarts() {
    let _login = crate::antigravity::TEST_LOGIN.lock().await;
    let mut lookup = fixture("http", "sse").await;
    let mut payload = fixture("http", "gemini_sse").await;
    let lookup_endpoint = format!("http://127.0.0.1:{}", lookup.addr.port());
    let payload_endpoint = format!("http://127.0.0.1:{}", payload.addr.port());
    let state = tempfile::tempdir().unwrap();
    let file = state.path().join(crate::refresh_routes::ANTIGRAVITY_FILE);
    let config = format!(
        "proxies:\n  lookup: {lookup_endpoint}\n  selected: {payload_endpoint}\nantigravity:\n  routing:\n    account:\n      Remote@Example.invalid: selected\n      other@example.invalid: none\n    account_probe: lookup\n"
    );
    let start = || {
        running_with_server(
            &config,
            |_| {},
            |server| {
                server.with_antigravity_refresh_routes(crate::refresh_routes::RefreshRoutes::load(
                    file.clone(),
                ))
            },
        )
    };
    let running = start().await;
    trust(&running, &lookup, &lookup_endpoint);
    trust(&running, &payload, &payload_endpoint);

    crate::antigravity::set_test_login("first-access", "login-refresh");
    // Until a lookup names it, the saved login's route is unknown.
    let states = running.server.account_route_states().await;
    assert!(
        states[2].values().all(|state| *state == "unknown"),
        "{states:?}"
    );
    antigravity_request(&running, "first-access").await;
    expect_lookup(&mut lookup, "first-access");
    expect_payload(&mut payload, "first-access").await;
    let saved = std::fs::read_to_string(&file).unwrap();
    assert!(!saved.contains("login-refresh") && !saved.contains("first-access"));
    let states = running.server.account_route_states().await;
    assert_eq!(states[2]["Remote@Example.invalid"], "active");
    assert_eq!(states[2]["other@example.invalid"], "inactive");
    let labels = running.server.cached_traffic_credential_labels().await;
    assert_eq!(
        labels[&(
            "Antigravity".to_owned(),
            "remote@example.invalid".to_owned()
        )],
        "Remote@Example.invalid"
    );

    // A renewed access token of the same login needs no lookup.
    crate::antigravity::set_test_login("renewed-access", "login-refresh");
    antigravity_request(&running, "renewed-access").await;
    expect_payload(&mut payload, "renewed-access").await;
    assert!(lookup.requests.try_recv().is_err());

    // Nor after a restart, with an empty identity cache.
    drop(running);
    let restarted = start().await;
    trust(&restarted, &lookup, &lookup_endpoint);
    trust(&restarted, &payload, &payload_endpoint);
    crate::antigravity::set_test_login("restarted-access", "login-refresh");
    antigravity_request(&restarted, "restarted-access").await;
    expect_payload(&mut payload, "restarted-access").await;
    assert!(lookup.requests.try_recv().is_err());
    let call = first_record(&restarted, "model_call_finished").await;
    assert_eq!(call["account_id"], "remote@example.invalid");
    assert_eq!(call["account_label"], "Remote@Example.invalid");
    assert!(call.get("profile_proxy").is_none());
    // Statistics label the learned account from the saved routes alone.
    let labels = restarted.server.cached_traffic_credential_labels().await;
    assert_eq!(
        labels[&(
            "Antigravity".to_owned(),
            "remote@example.invalid".to_owned()
        )],
        "Remote@Example.invalid"
    );
    assert_eq!(
        labels[&("Antigravity".to_owned(), "other@example.invalid".to_owned())],
        "other@example.invalid"
    );

    // Another login is looked up anew, not taken for the learned account.
    crate::antigravity::set_test_login("switched-access", "switched-refresh");
    antigravity_request(&restarted, "switched-access").await;
    expect_lookup(&mut lookup, "switched-access");
    expect_payload(&mut payload, "switched-access").await;
    crate::antigravity::set_test_login(crate::antigravity::TEST_SAVED_TOKEN, "saved-refresh");
}

#[tokio::test]
async fn antigravity_background_discovery_needs_an_explicit_probe_and_backs_off_failures() {
    let _login = crate::antigravity::TEST_LOGIN.lock().await;
    crate::antigravity::set_test_login("background-access", "background-refresh");
    // A fallback route alone does not authorize a lookup.
    let mut silent = fixture("http", "sse").await;
    let endpoint = format!("http://127.0.0.1:{}", silent.addr.port());
    let quiet = running(&format!(
        "proxies:\n  lookup: {endpoint}\nantigravity:\n  routing:\n    account:\n      remote@example.invalid: none\n    fallback: lookup\n"
    ))
    .await;
    trust(&quiet, &silent, &endpoint);
    quiet.server.traffic_credential_labels().await;
    assert!(silent.requests.try_recv().is_err());
    // The saved login without a known account routes by fallback.
    assert_eq!(
        quiet.server.account_route_states().await[2]["remote@example.invalid"],
        "inactive"
    );

    // Without Antigravity account routes, states read no login at all.
    let unconfigured = running("").await;
    let reads = crate::keychain::test_loads("gemini");
    assert!(unconfigured.server.account_route_states().await[2].is_empty());
    assert_eq!(crate::keychain::test_loads("gemini"), reads);

    let mut failing = fixture("http", "status_401").await;
    let endpoint = format!("http://127.0.0.1:{}", failing.addr.port());
    let config = format!(
        "proxies:\n  lookup: {endpoint}\nantigravity:\n  routing:\n    account:\n      remote@example.invalid: none\n    account_probe: lookup\n"
    );
    let failed = running_with_tls(&config, |_| {}).await;
    trust(&failed, &failing, &endpoint);
    failed.server.traffic_credential_labels().await;
    assert!(
        failing
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("CONNECT www.googleapis.com")
    );
    assert!(
        failing
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("GET /oauth2/v2/userinfo ")
    );
    assert_eq!(
        failed.server.account_route_states().await[2]["remote@example.invalid"],
        "probe_failed"
    );
    // A failure is not retried at once.
    failed.server.traffic_credential_labels().await;
    assert!(failing.requests.try_recv().is_err());

    let mut lookup = fixture("http", "sse").await;
    let endpoint = format!("http://127.0.0.1:{}", lookup.addr.port());
    let running = running(&config.replace(
        &format!("{}", failing.addr.port()),
        &format!("{}", lookup.addr.port()),
    ))
    .await;
    trust(&running, &lookup, &endpoint);
    let labels = running.server.traffic_credential_labels().await;
    assert_eq!(
        labels[&(
            "Antigravity".to_owned(),
            "remote@example.invalid".to_owned()
        )],
        "remote@example.invalid"
    );
    assert!(
        lookup
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("CONNECT www.googleapis.com")
    );
    assert!(
        lookup
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("GET /oauth2/v2/userinfo ")
    );
    // Learned for the login: requests and later discovery need no lookup.
    assert_eq!(
        running.server.account_route_states().await[2]["remote@example.invalid"],
        "active"
    );
    running.server.traffic_credential_labels().await;
    assert!(lookup.requests.try_recv().is_err());
    crate::antigravity::set_test_login(crate::antigravity::TEST_SAVED_TOKEN, "saved-refresh");
}

/// Antigravity's refresh of `refresh` through the explicit token URL.
async fn antigravity_refresh(running: &Running, refresh: &str, client: &str) -> reqwest::Response {
    http()
        .post(format!(
            "{}/antigravity/https://oauth2.googleapis.com/token",
            running.url
        ))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!(
            "client_id={client}&client_secret={}&grant_type=refresh_token&refresh_token={refresh}",
            crate::antigravity::TEST_CLIENT_SECRET
        ))
        .send()
        .await
        .unwrap()
}

// Copool and OpenQuota renew Antigravity's saved login themselves and keep
// the new access token without saving it where the login is read; their
// next requests were refused as not the saved login's.
#[tokio::test]
async fn an_access_token_a_relayed_refresh_issued_for_the_saved_login_is_that_login() {
    let _login = crate::antigravity::TEST_LOGIN.lock().await;
    let client = crate::antigravity::TEST_CLIENT_ID;
    let token = fixture("http", "google_token").await;
    let mut payload = fixture("http", "gemini_sse").await;
    let token_endpoint = format!("http://127.0.0.1:{}", token.addr.port());
    let payload_endpoint = format!("http://127.0.0.1:{}", payload.addr.port());
    let running = running(&format!(
        "proxies:\n  login: {token_endpoint}\n  account: {payload_endpoint}\nantigravity:\n  routing:\n    account:\n      default: login\n      remote@example.invalid: account\n"
    ))
    .await;
    trust(&running, &token, &token_endpoint);
    trust(&running, &payload, &payload_endpoint);
    crate::antigravity::set_test_login("saved-access", "saved-refresh");
    let status = |token: &'static str| {
        let running = &running;
        async move {
            http()
                .post(format!(
                    "{}/antigravity/v1internal:generateContent",
                    running.url
                ))
                .bearer_auth(token)
                .body("private-payload")
                .send()
                .await
                .unwrap()
                .status()
        }
    };
    assert_eq!(status("renewed-access").await, 401);

    let response = antigravity_refresh(&running, "saved-refresh", client).await;
    assert_eq!(response.status(), 200);
    assert!(response.text().await.unwrap().contains("renewed-access"));
    // The client keeps the new token; the saved login still holds the old one.
    // The recent read of that login answers for it, as for the saved token.
    let reads = crate::keychain::test_loads("gemini");
    antigravity_request(&running, "renewed-access").await;
    expect_payload(&mut payload, "renewed-access").await;
    assert_eq!(crate::keychain::test_loads("gemini"), reads);
    antigravity_request(&running, "saved-access").await;
    expect_payload(&mut payload, "saved-access").await;
    assert_eq!(status("other-access").await, 401);

    // Once Antigravity signs in to another login, it no longer is.
    crate::antigravity::set_test_login("next-access", "next-refresh");
    assert_eq!(status("renewed-access").await, 401);
    assert!(payload.requests.try_recv().is_err());
    crate::antigravity::set_test_login(crate::antigravity::TEST_SAVED_TOKEN, "saved-refresh");
}

#[tokio::test]
async fn antigravity_token_refresh_routes_like_codex_and_learns_its_id_token_account() {
    let _login = crate::antigravity::TEST_LOGIN.lock().await;
    let client = crate::antigravity::TEST_CLIENT_ID;
    let mut token = fixture("http", "google_token").await;
    let mut payload = fixture("http", "gemini_sse").await;
    let token_endpoint = format!("http://127.0.0.1:{}", token.addr.port());
    let payload_endpoint = format!("http://127.0.0.1:{}", payload.addr.port());
    let state = tempfile::tempdir().unwrap();
    let file = state.path().join(crate::refresh_routes::ANTIGRAVITY_FILE);
    let config = |file_only: bool| {
        format!(
            "proxies:\n  login: {token_endpoint}\n  account: {payload_endpoint}\nantigravity:\n  account_auth_file_only: {file_only}\n  routing:\n    account:\n      default: login\n      remote@example.invalid: account\n"
        )
    };
    let start = |file_only: bool| {
        let config = config(file_only);
        let file = file.clone();
        async move {
            running_with_server(
                &config,
                |_| {},
                |server| {
                    server.with_antigravity_refresh_routes(
                        crate::refresh_routes::RefreshRoutes::load(file),
                    )
                },
            )
            .await
        }
    };
    let running = start(true).await;
    trust(&running, &token, &token_endpoint);
    trust(&running, &payload, &payload_endpoint);
    crate::antigravity::set_test_login("saved-access", "saved-refresh");

    // The saved login's refresh takes its route, `default` while unnamed.
    let response = antigravity_refresh(&running, "saved-refresh", client).await;
    assert_eq!(response.status(), 200);
    assert!(response.text().await.unwrap().contains("renewed-access"));
    assert!(
        token
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("CONNECT oauth2.googleapis.com:443 ")
    );
    let forwarded = token.requests.recv().await.unwrap();
    assert!(forwarded.starts_with("POST /token HTTP/1.1"));
    assert!(forwarded.contains("accept-encoding: identity"));
    assert!(forwarded.ends_with("refresh_token=saved-refresh"));
    let refresh = first_record(&running, "request_finished").await;
    assert_eq!(refresh["service"], "antigravity");
    assert_eq!(refresh["account_label"], "default");
    assert_eq!(refresh["credential_match"], "configured");

    // Its ID token named the account: requests now take its route, with no lookup.
    let saved = std::fs::read_to_string(&file).unwrap();
    assert!(saved.contains("remote@example.invalid") && !saved.contains("saved-refresh"));
    antigravity_request(&running, "saved-access").await;
    expect_payload(&mut payload, "saved-access").await;

    // Refused before any connection: another Google client, an unknown
    // refresh token, another method or another path on the token host.
    assert_eq!(
        antigravity_refresh(
            &running,
            "saved-refresh",
            "gcloud.apps.googleusercontent.com"
        )
        .await
        .status(),
        403
    );
    assert_eq!(
        antigravity_refresh(&running, "unknown-refresh", client)
            .await
            .status(),
        403
    );
    let get = http()
        .get(format!(
            "{}/antigravity/https://oauth2.googleapis.com/token",
            running.url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(get.status(), 405);
    let other = http()
        .post(format!(
            "{}/antigravity/https://oauth2.googleapis.com/tokeninfo",
            running.url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(other.status(), 404);
    assert!(token.requests.try_recv().is_err());
    assert!(payload.requests.try_recv().is_err());

    // With account_auth_file_only: false, a learned refresh token that is no
    // longer the saved login's still takes its account's route.
    drop(running);
    crate::antigravity::set_test_login("switched-access", "switched-refresh");
    let open = start(false).await;
    trust(&open, &token, &token_endpoint);
    trust(&open, &payload, &payload_endpoint);
    let response = antigravity_refresh(&open, "saved-refresh", client).await;
    assert_eq!(response.status(), 200);
    assert!(
        payload
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("CONNECT oauth2.googleapis.com:443 ")
    );
    assert!(
        payload
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("POST /token HTTP/1.1")
    );
    assert!(token.requests.try_recv().is_err());
    crate::antigravity::set_test_login(crate::antigravity::TEST_SAVED_TOKEN, "saved-refresh");
}

/// The Antigravity rows `startup_log` writes for `config`.
async fn antigravity_startup_rows(config: &str) -> Vec<(String, serde_json::Value)> {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("proxy.log");
    let config = Config::parse(&format!(
        "listen_port: 7889\nrequest_timeout_seconds: 3\ncodex:\n  homes: []\nclaude:\n  config_dirs: []\n{config}"
    ))
    .unwrap();
    Server::new(config, Arc::new(Logger::new(path.clone())))
        .startup_log()
        .await;
    std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|row| row["service"] == "antigravity")
        .map(|row| {
            (
                row["event"].as_str().unwrap().to_owned(),
                row["reason"].clone(),
            )
        })
        .collect()
}

// The saved login is renewed once its access token has expired, so a lookup
// with that token is refused; refusing the refresh then left the login
// unable to renew. It goes on unnamed, and its ID token names the account.
#[tokio::test]
async fn an_unnamed_saved_login_with_an_expired_token_still_renews_through_the_probe() {
    let _login = crate::antigravity::TEST_LOGIN.lock().await;
    let client = crate::antigravity::TEST_CLIENT_ID;
    let mut lookup = fixture("http", "status_401").await;
    let mut token = fixture("http", "google_token").await;
    let lookup_endpoint = format!("http://127.0.0.1:{}", lookup.addr.port());
    let token_endpoint = format!("http://127.0.0.1:{}", token.addr.port());
    for default in [true, false] {
        let state = tempfile::tempdir().unwrap();
        let file = state.path().join(crate::refresh_routes::ANTIGRAVITY_FILE);
        // `default` takes it as it would without a probe; else the probe.
        let routes = if default {
            "      default: login\n      remote@example.invalid: none\n"
        } else {
            "      remote@example.invalid: none\n"
        };
        let running = running_with_server(
            &format!(
                "proxies:\n  lookup: {lookup_endpoint}\n  login: {token_endpoint}\nantigravity:\n  routing:\n    account:\n{routes}    account_probe: lookup\n"
            ),
            |_| {},
            |server| {
                server.with_antigravity_refresh_routes(crate::refresh_routes::RefreshRoutes::load(
                    file.clone(),
                ))
            },
        )
        .await;
        trust(&running, &lookup, &lookup_endpoint);
        trust(&running, &token, &token_endpoint);
        crate::antigravity::set_test_login("expired-access", "saved-refresh");
        let response = antigravity_refresh(&running, "saved-refresh", client).await;
        let mut lookups = Vec::new();
        while let Ok(request) = lookup.requests.try_recv() {
            lookups.push(request);
        }
        assert!(
            lookups.len() >= 2
                && lookups[0].starts_with("CONNECT www.googleapis.com:443 ")
                && lookups[1].starts_with("GET /oauth2/v2/userinfo "),
            "{lookups:?}"
        );
        let refreshed = if default {
            assert_eq!(response.status(), 200);
            assert!(response.text().await.unwrap().contains("renewed-access"));
            assert_eq!(lookups.len(), 2, "{lookups:?}");
            vec![
                token.requests.recv().await.unwrap(),
                token.requests.recv().await.unwrap(),
            ]
        } else {
            // The probe's relay answers every request with 401 here.
            assert_eq!(response.status(), 401);
            lookups[2..].to_vec()
        };
        assert!(
            refreshed[0].starts_with("CONNECT oauth2.googleapis.com:443 ")
                && refreshed[1].starts_with("POST /token HTTP/1.1")
                && refreshed[1].ends_with("refresh_token=saved-refresh"),
            "{refreshed:?}"
        );
        if default {
            let saved = std::fs::read_to_string(&file).unwrap();
            assert!(saved.contains("remote@example.invalid"), "{saved}");
        }
        assert!(token.requests.try_recv().is_err());
    }
    crate::antigravity::set_test_login(crate::antigravity::TEST_SAVED_TOKEN, "saved-refresh");
}

/// As for Claude, startup reports whether Antigravity's saved login is in the
/// OS credential store; a section without routes, or without the saved
/// login, reads nothing.
#[tokio::test]
async fn startup_reports_whether_antigravity_has_its_saved_login() {
    let _login = crate::antigravity::TEST_LOGIN.lock().await;
    let rows = antigravity_startup_rows;
    let routed = "antigravity:\n  routing:\n    account:\n      default: none\n";
    crate::antigravity::set_test_login("startup-access", "startup-refresh");
    assert_eq!(
        rows(routed).await,
        [("current_route".to_owned(), serde_json::Value::Null)]
    );
    // An unusable item, not a failing read: failed reads cool down for
    // every test reading the item.
    crate::keychain::set_test_entry("gemini", "antigravity", Some("{}"));
    assert_eq!(
        rows(routed).await,
        [(
            "route_unavailable".to_owned(),
            "Invalid Antigravity login in the OS credential store.".into()
        )]
    );
    assert!(rows("").await.is_empty());
    assert!(
        rows("antigravity:\n  saved_login: false\n  account_auth_file_only: false\n  routing:\n    fallback: none\n")
            .await
            .is_empty()
    );
    crate::antigravity::set_test_login(crate::antigravity::TEST_SAVED_TOKEN, "saved-refresh");
}

/// Antigravity's saved login as agy's go-keyring keeps it on macOS.
fn antigravity_wrapped_login() -> String {
    use base64::Engine;
    let document = json!({"token": {"access_token": "access-secret", "token_type": "Bearer", "refresh_token": "refresh-secret", "expiry": "2026-01-01T00:00:00Z", "expires_in": 3599}, "auth_method": "consumer"});
    format!(
        "go-keyring-base64:{}",
        base64::engine::general_purpose::STANDARD.encode(document.to_string())
    )
}
async fn antigravity_saved_item() -> String {
    crate::keychain::read_go_keyring("gemini", "antigravity")
        .await
        .unwrap()
        .unwrap()
}
async fn antigravity_auto_setup(enabled: bool, mode: &'static str) -> (Running, Fixture) {
    // As a refresh agy sent through the proxy teaches it.
    crate::antigravity::learn_client(
        format!(
            "client_id={}&client_secret={}",
            crate::antigravity::TEST_CLIENT_ID,
            crate::antigravity::TEST_CLIENT_SECRET
        )
        .as_bytes(),
    );
    crate::keychain::set_test_entry("gemini", "antigravity", Some(&antigravity_wrapped_login()));
    let fixture = fixture("http", mode).await;
    let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
    let running = running(&format!(
        "auto_refresh_access_token: {enabled}\nproxies:\n  selected: {endpoint}\nantigravity:\n  routing:\n    account:\n      default: selected\n"
    ))
    .await;
    trust(&running, &fixture, &endpoint);
    (running, fixture)
}
async fn antigravity_call(running: &Running, path: &str) -> reqwest::Response {
    http()
        .post(format!("{}/antigravity/v1internal:{path}", running.url))
        .bearer_auth("access-secret")
        .header("cookie", "private-cookie")
        .body("model-body")
        .send()
        .await
        .unwrap()
}
#[tokio::test]
async fn auto_refresh_renews_antigravity_in_its_item_once_and_in_its_own_form() {
    let _login = crate::antigravity::TEST_LOGIN.lock().await;
    let (running, mut fixture) = antigravity_auto_setup(true, "auto_success").await;
    let (one, two) = tokio::join!(
        antigravity_call(&running, "generateContent"),
        antigravity_call(&running, "streamGenerateContent?alt=sse")
    );
    assert_eq!(one.status(), 200);
    assert_eq!(two.status(), 200);
    // The old token now names the renewed login.
    let again = antigravity_call(&running, "generateContent").await;
    assert_eq!(again.text().await.unwrap(), "model-result");
    let item = antigravity_saved_item().await;
    let (saved, wrapped) = crate::antigravity::decode_item(&item).unwrap();
    assert!(wrapped);
    let token = &saved["token"];
    assert_eq!(token["access_token"], "new-access");
    assert_eq!(token["refresh_token"], "new-refresh");
    assert_eq!(token["token_type"], "Bearer");
    assert_eq!(token["expires_in"], 3600);
    assert_eq!(saved["auth_method"], "consumer");
    // RFC 3339 in UTC to the second, as Go writes it, an hour ahead.
    let at = |seconds: i64| {
        time::OffsetDateTime::from_unix_timestamp(
            time::OffsetDateTime::now_utc().unix_timestamp() + seconds,
        )
        .unwrap()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap()
    };
    let expiry = token["expiry"].as_str().unwrap();
    assert!(
        at(3540).as_str() < expiry && expiry <= at(3600).as_str(),
        "{expiry}"
    );
    let requests = take_requests(&mut fixture);
    let refreshes: Vec<_> = requests
        .iter()
        .filter(|r| r.starts_with("POST /token "))
        .collect();
    assert_eq!(refreshes.len(), 1, "{requests:?}");
    let form: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(refreshes[0].split("\r\n\r\n").nth(1).unwrap().as_bytes())
            .into_owned()
            .collect();
    assert_eq!(form["client_id"], crate::antigravity::TEST_CLIENT_ID);
    assert_eq!(
        form["client_secret"],
        crate::antigravity::TEST_CLIENT_SECRET
    );
    assert_eq!(form["grant_type"], "refresh_token");
    assert_eq!(form["refresh_token"], "refresh-secret");
    for forbidden in [
        "access-secret",
        "private-cookie",
        "authorization:",
        "model-body",
    ] {
        assert!(!refreshes[0].contains(forbidden), "{forbidden}");
    }
    // Every retry carries the original body and the renewed token.
    let retried: Vec<_> = requests
        .iter()
        .filter(|r| r.contains("authorization: Bearer new-access"))
        .collect();
    assert_eq!(retried.len(), 3, "{requests:?}");
    assert!(retried.iter().all(|r| r.ends_with("model-body")));
    running.server.logger.flush().unwrap();
    let log = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
    assert!(log.contains("\"oauth_refresh\":\"succeeded\""));
    assert!(log.contains("\"path\":\"/token\""));
    for secret in [
        "access-secret",
        "refresh-secret",
        "new-access",
        "new-refresh",
    ] {
        assert!(!log.contains(secret), "{secret}");
    }
    // Another sign-in revokes the old token's alias.
    crate::antigravity::set_test_login("switched-access", "switched-refresh");
    assert_eq!(
        antigravity_call(&running, "generateContent").await.status(),
        401
    );
    crate::antigravity::set_test_login(crate::antigravity::TEST_SAVED_TOKEN, "saved-refresh");
}
#[tokio::test]
async fn auto_refresh_of_antigravity_is_opt_in_for_model_calls_and_stops_once_rejected() {
    let _login = crate::antigravity::TEST_LOGIN.lock().await;
    let (running, mut fixture) = antigravity_auto_setup(false, "auto_success").await;
    assert_eq!(
        antigravity_call(&running, "generateContent").await.status(),
        401
    );
    assert_eq!(take_requests(&mut fixture).len(), 1);
    assert_eq!(antigravity_saved_item().await, antigravity_wrapped_login());
    let (running, mut fixture) = antigravity_auto_setup(true, "auto_success").await;
    for _ in 0..2 {
        let response = antigravity_call(&running, "loadCodeAssist").await;
        assert_eq!(response.status(), 401);
    }
    assert_eq!(take_requests(&mut fixture).len(), 2);
    assert_eq!(antigravity_saved_item().await, antigravity_wrapped_login());
    let (running, mut fixture) = antigravity_auto_setup(true, "auto_rejected").await;
    for _ in 0..2 {
        let response = antigravity_call(&running, "generateContent").await;
        assert_eq!(response.status(), 401);
        assert_eq!(response.text().await.unwrap(), "original-unauthorized");
    }
    let requests = take_requests(&mut fixture);
    let refreshes = requests
        .iter()
        .filter(|r| r.starts_with("POST /token "))
        .count();
    assert_eq!(refreshes, 1, "{requests:?}");
    assert_eq!(antigravity_saved_item().await, antigravity_wrapped_login());
    crate::antigravity::set_test_login(crate::antigravity::TEST_SAVED_TOKEN, "saved-refresh");
}

#[tokio::test]
async fn antigravity_location_cooldown_disconnects_before_headers_and_only_delays_model_calls() {
    let _login = crate::antigravity::TEST_LOGIN.lock().await;
    let (running, mut fixture) = antigravity_auto_setup(false, "location_once").await;
    let call = |method: &str| {
        http()
            .post(format!("{}/antigravity/v1internal:{method}", running.url))
            .bearer_auth("access-secret")
            .body("model-body")
            .send()
    };
    assert!(call("streamGenerateContent").await.is_err());
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
            .starts_with("POST /v1internal:streamGenerateContent ")
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), call("generateContent"))
            .await
            .is_err()
    );
    assert_eq!(
        call("loadCodeAssist").await.unwrap().text().await.unwrap(),
        "ok"
    );
    fixture.requests.recv().await.unwrap();
    assert!(
        fixture
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("POST /v1internal:loadCodeAssist ")
    );
    assert!(fixture.requests.try_recv().is_err());
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(180)).await;
    tokio::time::resume();
    assert_eq!(
        call("streamGenerateContent")
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "ok"
    );
    fixture.requests.recv().await.unwrap();
    // The cancelled generateContent must never reach the upstream at expiry.
    assert!(
        fixture
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("POST /v1internal:streamGenerateContent ")
    );
    assert!(fixture.requests.try_recv().is_err());
    crate::antigravity::set_test_login(crate::antigravity::TEST_SAVED_TOKEN, "saved-refresh");
}
