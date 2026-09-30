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
                let request=read_request(&mut io).await.unwrap(); let head=request.starts_with("HEAD "); let profile=request.starts_with("GET /api/oauth/profile "); tx.send(request).unwrap();
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
        let running = running(&format!("proxies:\n  selected: {endpoint}\nrouting:\n  api_key:\n    'upstream.invalid/v1': selected\n")).await;
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
        let file = serde_json::to_string(&file.to_string_lossy()).unwrap();
        let running = running(&format!("proxies:\n  selected: {endpoint}\nclaude:\n  auth_file: {file}\n  base_url: https://upstream.invalid\n  routing:\n    account_fallback: selected\n    api_key_fallback: selected\n")).await;
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
    let running = running("routing:\n  api_key_fallback: none\nclaude:\n  account_fallback: none\n  api_key_fallback: none\n").await;
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
    let running = self::running("routing:\n  api_key_fallback: none\n").await;
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
        let file = serde_json::to_string(&file).unwrap();
        let running = running(&format!("proxies:\n  lookup: {lookup_endpoint}\n  selected: {payload_endpoint}\nclaude:\n  auth_file: {file}\n  account_auth_file_only: {file_only}\n  base_url: https://upstream.invalid\n  routing:\n    account:\n      remote@example.invalid: selected\n    account_probe: lookup\n")).await;
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
        let running=running(&format!("proxies:\n  selected: {endpoint}\nrouting:\n  api_key_fallback: selected\nbase_url:\n  api_key: https://upstream.invalid/v1\n")).await;
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
async fn ordered_probes_are_credential_free_and_repeated_per_request() {
    let mut fixture = fixture("direct", "redirect").await;
    let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_port = dead.local_addr().unwrap().port();
    drop(dead);
    let running=running(&format!("proxies:\n  dead: http://127.0.0.1:{dead_port}\nrouting:\n  api_key_fallback: [dead, none]\nbase_url:\n  api_key: https://upstream.invalid/v1\n")).await;
    trust(&running, &fixture, "none");
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    for _ in 0..2 {
        let response = client
            .post(format!("{}/responses?secret=query", running.url))
            .bearer_auth("model-secret")
            .body("private-body")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 302);
        let probe = fixture.requests.recv().await.unwrap().to_lowercase();
        assert!(probe.starts_with("head / http"));
        for s in ["authorization", "private-body", "query", "model-secret"] {
            assert!(!probe.contains(s));
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
    let running = running("routing:\n  mcp_fallback: none\n").await;
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
        "routing:\n  api_key_fallback: none\nbase_url:\n  api_key: https://upstream.invalid/v1\n",
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
        let running=running("routing:\n  api_key_fallback: none\nbase_url:\n  api_key: https://upstream.invalid/v1\n").await;
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
    }
}
