use super::*;

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

// A request its Route's endpoint checks refuse is still recorded with that
// Route, so it counts against the account it was routed to.
#[tokio::test]
async fn a_request_its_route_refuses_is_recorded_with_that_route() {
    let running = running(
        "codex:\n  routing:\n    fallback: none\n  base_url:\n    api_key: https://upstream.invalid/v1\n",
    )
    .await;
    let response = http()
        .get(format!("{}/backend-api/wham/usage", running.url))
        .bearer_auth("sk-0123456789abcdefghijklmnopqrstuvwxyz")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    running.server.logger.flush().unwrap();
    let logs = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
    let rejected = logs
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|entry| entry["event"] == "request_rejected")
        .unwrap();
    assert_eq!(rejected["upstream_base_url"], "https://upstream.invalid/v1");
    assert_eq!(rejected["credential_match"], "unmatched");
    assert!(rejected.get("credential_ref").is_some());
}

#[tokio::test]
async fn request_records_keep_their_events_and_fields() {
    let running = running(
        "codex:\n  routing:\n    fallback: none\n  base_url:\n    api_key: https://upstream.invalid/v1\n",
    )
    .await;
    let key = "sk-0123456789abcdefghijklmnopqrstuvwxyz";
    // A Model call whose upstream cannot be reached, then a refused request.
    let failed = http()
        .post(format!("{}/v1/responses", running.url))
        .bearer_auth(key)
        .body(r#"{"model":"m","input":"x"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(failed.status(), 502);
    let refused = http()
        .get(format!("{}/backend-api/wham/usage", running.url))
        .bearer_auth(key)
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 403);
    running.server.logger.flush().unwrap();
    let logs = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
    let shapes: Vec<String> = logs
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|entry| entry.get("request_id").is_some())
        .map(|entry| {
            let entry = entry.as_object().unwrap();
            let keys: Vec<_> = entry
                .keys()
                .filter(|key| *key != "timestamp")
                .map(String::as_str)
                .collect();
            format!("{}: {}", entry["event"].as_str().unwrap(), keys.join(" "))
        })
        .collect();
    // The panel reads these events and fields; a change here is a change
    // to what it shows.
    let call = "client_request_id event method model_call_id model_transport path request_id service session_id";
    let routed = "client_request_id credential_match credential_ref event method model model_call_id model_transport path provider proxy proxy_endpoint request_id service session_id upstream_base_url upstream_method upstream_path";
    let ended = "client_request_id credential_match credential_ref duration_ms event method model model_call_id model_transport path provider proxy proxy_endpoint reason received_bytes request_id service session_id status transport_error upstream_attempts upstream_base_url upstream_method upstream_path upstream_started_ms";
    assert_eq!(
        shapes,
        [
            format!("request_received: {call}"),
            format!("model_call_started: {call}"),
            format!("route_selected: {routed}"),
            format!("model_call_updated: {routed}"),
            format!("request_failed: {ended}"),
            format!("model_call_failed: {ended}"),
            "request_received: client_request_id event method path request_id service session_id".into(),
            "request_rejected: client_request_id credential_match credential_ref duration_ms event method path provider reason received_bytes request_id service session_id status upstream_base_url".into(),
        ]
    );
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
    // A forwarded response keeps the connection open for further requests,
    // whose framing is held to the first request's rules.
    for framing in [
        "Content-Length: 2\r\nTransfer-Encoding: chunked\r\n",
        "Transfer-Encoding: gzip, chunked\r\n",
    ] {
        let mut socket = tokio::net::TcpStream::connect(running.url.trim_start_matches("http://"))
            .await
            .unwrap();
        socket.write_all(b"POST /responses HTTP/1.1\r\nHost: local\r\nAuthorization: Bearer model-secret\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n").await.unwrap();
        let output = read_response_head(&mut socket).await;
        assert!(output.starts_with("HTTP/1.1 302"), "{output}");
        assert!(fixture.requests.recv().await.unwrap().ends_with("abcde"));
        socket.write_all(format!("POST /responses HTTP/1.1\r\nHost: local\r\nAuthorization: Bearer model-secret\r\n{framing}\r\n0\r\n\r\n").as_bytes()).await.unwrap();
        let mut output = String::new();
        tokio::time::timeout(Duration::from_secs(2), socket.read_to_string(&mut output))
            .await
            .unwrap()
            .unwrap();
        assert!(output.contains("HTTP/1.1 400"), "{framing}: {output}");
        assert!(fixture.requests.try_recv().is_err(), "{framing}");
    }
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

/// Waits until `server` counts `expected` requests in flight.
async fn in_flight(server: &Server, expected: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while server.requests_in_flight() != expected {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{} in flight, not {expected}", server.requests_in_flight()));
}

#[tokio::test]
async fn requests_stay_in_flight_while_a_response_streams_or_a_relay_is_open() {
    let mut sse = fixture("direct", "sse").await;
    let streaming = running(
        "codex:\n  routing:\n    fallback: none\n  base_url:\n    api_key: https://upstream.invalid/v1\n",
    )
    .await;
    trust(&streaming, &sse, "none");
    let health = http()
        .get(format!("{}/health", streaming.url))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), 200);
    assert_eq!(streaming.server.requests_in_flight(), 0);
    let response = http()
        .post(format!("{}/responses", streaming.url))
        .bearer_auth("model-secret")
        .send()
        .await
        .unwrap();
    let mut stream = response.bytes_stream();
    assert_eq!(stream.next().await.unwrap().unwrap(), "data: first\n\n");
    assert!(sse.requests.recv().await.unwrap().starts_with("POST"));
    in_flight(&streaming.server, 1).await;
    drop(stream);
    in_flight(&streaming.server, 0).await;

    let mut relay = fixture("http", "websocket_hold").await;
    let endpoint = format!("http://127.0.0.1:{}", relay.addr.port());
    let relaying = running(&format!(
        "proxies:\n  selected: {endpoint}\ncodex:\n  routing:\n    api_key:\n      upstream.invalid: selected\n"
    ))
    .await;
    trust(&relaying, &relay, &endpoint);
    let mut socket = tokio::net::TcpStream::connect(relaying.url.trim_start_matches("http://"))
        .await
        .unwrap();
    socket.write_all(b"GET /codex/https://upstream.invalid/v1/socket HTTP/1.1\r\nHost: local\r\nAuthorization: Bearer ws-secret\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n").await.unwrap();
    let head = read_response_head(&mut socket).await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    assert!(relay.requests.recv().await.unwrap().starts_with("CONNECT"));
    assert!(
        relay
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("GET /v1/socket ")
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(relaying.server.requests_in_flight(), 1);
    drop(socket);
    in_flight(&relaying.server, 0).await;
}

#[tokio::test]
async fn forwarded_first_token_origin_excludes_profile_and_survives_retry() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let upstream = tokio::spawn(async move {
        for _ in 0..3 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_response_head(&mut socket).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        }
    });
    let running = running("").await;
    let mut log = RequestLog::for_test(running.server.logger.clone(), 200);
    log.model_call("model-call");
    let mut origin = None;
    for (method, path) in [
        (hyper::Method::GET, "/api/oauth/profile"),
        (hyper::Method::POST, "/v1/responses"),
        (hyper::Method::POST, "/v1/responses"),
    ] {
        let request =
            reqwest::Request::new(method, Url::parse(&format!("{endpoint}{path}")).unwrap());
        running
            .server
            .send_via(
                &Choice::direct(),
                request,
                false,
                Deadline::new(3.0),
                if path.ends_with("profile") {
                    Upstream::Auxiliary
                } else {
                    Upstream::ModelCall
                },
                &mut log,
            )
            .await
            .unwrap();
        if path.ends_with("profile") {
            assert!(!log.fields.contains_key("upstream_started_ms"));
        } else if let Some(origin) = &origin {
            assert_eq!(&log.fields["upstream_started_ms"], origin);
        } else {
            origin = Some(log.fields["upstream_started_ms"].clone());
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    upstream.await.unwrap();
    // The account lookup's route is not the Model call's.
    running.server.logger.flush().unwrap();
    let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
    for line in raw.lines() {
        let line: serde_json::Value = serde_json::from_str(line).unwrap();
        if line["upstream_path"] == "/api/oauth/profile" {
            assert!(line.get("model_call_id").is_none(), "{line}");
            assert_ne!(line["event"], "model_call_updated", "{line}");
        }
    }
    assert!(raw.contains("\"event\":\"route_selected\""), "{raw}");
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
    let mut log = RequestLog::for_test(running.server.logger.clone(), 502);
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
            Upstream::Relay,
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
