use super::*;

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
