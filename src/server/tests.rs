use super::*;
use futures_util::FutureExt;
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
        "auth.x.ai".into(),
        "cli-chat-proxy.grok.com".into(),
        "platform.claude.com".into(),
        "api.anthropic.com".into(),
        "daily-cloudcode-pa.googleapis.com".into(),
        "cloudcode-pa.googleapis.com".into(),
        "www.googleapis.com".into(),
        "oauth2.googleapis.com".into(),
        "aicode.googleapis.com".into(),
        "api.x.ai".into(),
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
                else if profile && response_mode == "profile_hang" {
                    // A profile lookup left in flight until the client gives up.
                    let _ = io.read_u8().await;
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
                else if response_mode=="grok" {
                    // The ID token names the user and team. The access token is opaque.
                    let body = if request.starts_with("POST /oauth2/token ") {
                        r#"{"id_token":"h.eyJzdWIiOiJhbGljZSIsInRlYW1faWQiOiJ0ZWFtIn0.s","access_token":"issued-access","refresh_token":"issued-refresh"}"#
                    } else if request.starts_with("GET /.well-known/openid-configuration ") {
                        r#"{"issuer":"https://auth.x.ai","authorization_endpoint":"https://accounts.x.ai/oauth2/auth","token_endpoint":"https://auth.x.ai/oauth2/token","device_authorization_endpoint":"https://auth.x.ai/oauth2/device/code","jwks_uri":"https://auth.x.ai/.well-known/jwks.json"}"#
                    } else if request.starts_with("POST /oauth2/device/code ") {
                        r#"{"device_code":"device-secret","user_code":"ABCD-EFGH","verification_uri":"https://accounts.x.ai/device","interval":5}"#
                    } else if request.starts_with("GET /v1/billing?format=credits ") {
                        r#"{"creditUsagePercent":42}"#
                    } else {
                        r#"{"subscription_tier_display":"SuperGrok"}"#
                    };
                    io.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
                }
                else if response_mode == "permission_denied" {
                    let body = r#"{"error":{"code":403,"status":"PERMISSION_DENIED","message":"The caller does not have permission","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"IAM_PERMISSION_DENIED","metadata":{"consumer":"projects/secret-project"}}]}}"#;
                    io.write_all(format!("HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
                }
                else if matches!(response_mode, "location_once" | "location_once_sse") && attempt == 0 {
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
                else if matches!(response_mode, "gemini_sse" | "location_once_sse") {
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
    if config.codex.homes == crate::codex::default_codex_homes() {
        config.codex.homes = isolated("codex");
    }
    if config.grok.homes.is_none() && config.grok.auth_file.is_none() {
        config.grok.auth_file = Some(
            temp.path()
                .join("grok/auth.json")
                .to_string_lossy()
                .into_owned(),
        );
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

async fn read_response_head(socket: &mut tokio::net::TcpStream) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        bytes.push(socket.read_u8().await.unwrap());
    }
    String::from_utf8(bytes).unwrap()
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

fn take_requests(fixture: &mut Fixture) -> Vec<String> {
    let mut requests = Vec::new();
    while let Ok(request) = fixture.requests.try_recv() {
        if !request.starts_with("CONNECT ") {
            requests.push(request);
        }
    }
    requests
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

/// The lookup of `token`'s account, and nothing more, reached `lookup`.
fn expect_lookup(lookup: &mut Fixture, token: &str) {
    let connect = lookup.requests.try_recv().unwrap();
    assert!(connect.starts_with("CONNECT www.googleapis.com:443 "));
    let request = lookup.requests.try_recv().unwrap();
    assert!(request.starts_with("GET /oauth2/v2/userinfo HTTP/1.1"));
    assert!(request.contains(&format!("authorization: Bearer {token}")));
    assert!(lookup.requests.try_recv().is_err());
}

mod antigravity_forwarding;
mod auto_refresh;
mod connections;
mod grok_forwarding;
mod health;
mod identities;
mod model_calls;
mod proxy_port;
mod relays;
mod requests;
mod routing;
mod token_refresh;
