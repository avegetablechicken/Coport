use super::*;

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
