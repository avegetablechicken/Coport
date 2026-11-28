use super::*;

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
    let mut log = RequestLog::for_test(running.server.logger.clone(), 200);
    let mut concurrent_log = RequestLog::for_test(running.server.logger.clone(), 200);
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
