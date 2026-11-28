use super::*;

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
    running.server.logger.flush().unwrap();
    let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
    let lines = raw
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    let upstream = lines
        .iter()
        .find(|line| line["event"] == "upstream_response")
        .unwrap_or_else(|| panic!("upstream handshake missing from log: {raw}"));
    assert_eq!(upstream["status"], "101");
    assert!(upstream.get("headers_ms").is_some(), "{upstream}");
    let rejected = lines
        .iter()
        .find(|line| line["event"] == "request_failed" || line["event"] == "request_rejected")
        .unwrap_or_else(|| panic!("client rejection missing from log: {raw}"));
    assert_eq!(rejected["status"], "502", "{rejected}");
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
