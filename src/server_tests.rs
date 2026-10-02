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
    Ok(head + &String::from_utf8(body).unwrap())
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
    let task = tokio::spawn(async move {
        let mut children = tokio::task::JoinSet::new();
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let acceptor = acceptor.clone();
            let tx = tx.clone();
            let ready = ready.clone();
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
                if head { io.write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n").await.unwrap(); }
                else if profile {
                    let (status, body) = match response_mode {
                        "profile_unauthorized" => (401, "{}"),
                        "profile_invalid" => (200, "{}"),
                        "profile_redirect" => (302, "{}"),
                        _ => (200, r#"{"account":{"uuid":"remote-account","email":"remote@example.invalid"}}"#),
                    };
                    io.write_all(format!("HTTP/1.1 {status} Profile\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
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
    let config = Config::parse(&format!(
        "listen_port: 7889\nrequest_timeout_seconds: 3\n{config}"
    ))
    .unwrap();
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
        let log = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
        assert!(!log.contains("url-route-secret"));
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
        let file = credentials.path().join("credentials.json");
        std::fs::write(&file, r#"{"claudeAiOauth":{"accessToken":"model-secret"}}"#).unwrap();
        let dir = serde_json::to_string(&credentials.path()).unwrap();
        let running = running(&format!("proxies:\n  selected: {endpoint}\nclaude:\n  config_dirs: [{dir}]\n  auth_file: credentials.json\n  base_url: https://upstream.invalid\n  routing:\n    account_fallback: selected\n    api_key_fallback: selected\n")).await;
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
async fn claude_saved_token_without_metadata_probes_then_routes_and_caches() {
    for file_only in [false, true] {
        let mut lookup = fixture("http", "sse").await;
        let mut payload = fixture("http", "redirect").await;
        let lookup_endpoint = format!("http://127.0.0.1:{}", lookup.addr.port());
        let payload_endpoint = format!("http://127.0.0.1:{}", payload.addr.port());
        let credentials = tempfile::tempdir().unwrap();
        let file = credentials.path().join("credentials.json");
        std::fs::write(&file, r#"{"claudeAiOauth":{"accessToken":"saved-secret"}}"#).unwrap();
        let dir = serde_json::to_string(&credentials.path()).unwrap();
        let running = running(&format!("proxies:\n  lookup: {lookup_endpoint}\n  selected: {payload_endpoint}\nclaude:\n  config_dirs: [{dir}]\n  auth_file: credentials.json\n  account_auth_file_only: {file_only}\n  base_url: https://upstream.invalid\n  routing:\n    account:\n      remote@example.invalid: selected\n    account_probe: lookup\n")).await;
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
        .0 = Instant::now() - Duration::from_secs(30 * 24 * 60 * 60);
    let response = http()
        .post(format!("{}/anthropic/api/oauth/usage", running.url))
        .bearer_auth("first-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 405); // A remotely identified account passes auth, then method validation.
    assert!(lookup.requests.try_recv().is_err());
    assert!(payload.requests.try_recv().is_err());
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
    for index in 0..128 {
        running
            .server
            .cache_claude_profile(format!("token-{index}"), identity.clone())
            .unwrap();
    }
    {
        let mut cache = running.server.claude_profiles.lock().unwrap();
        cache.get_mut("token-42").unwrap().0 = Instant::now() - Duration::from_secs(86400);
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
    for disconnect in [true, false] {
        let mut fixture = fixture("direct", "sse").await;
        let running=running("codex:\n  routing:\n    api_key_fallback: none\n  base_url:\n    api_key: https://upstream.invalid/v1\n").await;
        trust(&running, &fixture, "none");
        let response = http()
            .post(format!("{}/responses", running.url))
            .bearer_auth("model-secret")
            .send()
            .await
            .unwrap();
        let mut stream = response.bytes_stream();
        assert_eq!(stream.next().await.unwrap().unwrap(), "data: first\n\n");
        assert!(fixture.requests.recv().await.unwrap().starts_with("POST "));
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
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("api.json"), r#"{"env":{"ANTHROPIC_BASE_URL":"https://upstream.invalid/custom","ANTHROPIC_AUTH_TOKEN":"profile-secret"}}"#).unwrap();
    let mut fixture = fixture("http", "sse").await;
    let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
    let running = running(&format!("proxies:\n  selected: {endpoint}\nclaude:\n  config_dirs: [{}]\n  routing:\n    api_key:\n      api: selected\n", serde_json::to_string(dir.path()).unwrap())).await;
    trust(&running, &fixture, &endpoint);
    let mut response = http()
        .post(format!("{}/anthropic/v1/messages", running.url))
        .bearer_auth("profile-secret")
        .body("model-body")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let connect = fixture.requests.recv().await.unwrap();
    assert!(connect.starts_with("CONNECT upstream.invalid:443"));
    assert!(!connect.contains("profile-secret"));
    let request = fixture.requests.recv().await.unwrap();
    assert!(request.starts_with("POST /custom/v1/messages HTTP/1.1"));
    assert!(request.contains("authorization: Bearer profile-secret"));
    assert!(!request.contains("oauth-2025-04-20"));
    assert!(request.ends_with("model-body"));
    assert_eq!(response.chunk().await.unwrap().unwrap(), "data: first\n\n");
    fixture.release.notify_one();
    assert_eq!(response.chunk().await.unwrap().unwrap(), "data: last\n\n");
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
            let status = if h.load(Ordering::SeqCst) { 200 } else { 503 };
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
    tokio::time::timeout(Duration::from_secs(36), async {
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
            if *state.lock().await == Some(true) {
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
