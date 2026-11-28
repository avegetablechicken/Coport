use super::*;

#[tokio::test]
async fn only_antigravity_errors_are_read_before_their_headers() {
    let fixture = fixture("direct", "permission_denied").await;
    let running = running("proxies:\n  selected: none\ncodex:\n  routing:\n    api_key:\n      'upstream.invalid/v1': selected\n").await;
    trust(&running, &fixture, "none");
    let response = http()
        .post(format!(
            "{}/codex/https://upstream.invalid/v1/responses",
            running.url
        ))
        .bearer_auth("url-route-secret")
        .body("model-body")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("IAM_PERMISSION_DENIED")
    );
    running.server.logger.flush().unwrap();
    let log = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
    // A Codex, Claude or Grok error message can echo the request.
    assert!(log.contains("\"status\":\"403\""), "{log}");
    assert!(!log.contains("upstream_error"), "{log}");
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
        (
            "https://aicode.googleapis.com/v1alpha:fetchLicenses",
            "aicode.googleapis.com",
            "GET /v1alpha:fetchLicenses HTTP/1.1",
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
    // agy's scope check names its token only in the query.
    let tokeninfo = format!("{}?access_token={token}", crate::antigravity::TOKENINFO_URL);
    let response = http()
        .get(format!("{}/antigravity/{tokeninfo}", running.url))
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
            .starts_with("CONNECT www.googleapis.com:443 ")
    );
    let forwarded = fixture.requests.recv().await.unwrap();
    assert!(
        forwarded.starts_with(&format!(
            "GET /oauth2/v3/tokeninfo?access_token={token} HTTP/1.1"
        )),
        "{forwarded}"
    );
    assert!(forwarded.contains(&format!("authorization: Bearer {token}")));
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
    // Without `AICODE_ENDPOINT_URL` or a Bearer header for the scope check.
    for (url, host, line) in [
        (
            "https://aicode.googleapis.com/v1alpha:fetchQuotaStatus".to_owned(),
            "aicode.googleapis.com",
            "GET /v1alpha:fetchQuotaStatus HTTP/1.1".to_owned(),
        ),
        (
            tokeninfo.clone(),
            "www.googleapis.com",
            format!("GET /oauth2/v3/tokeninfo?access_token={token} HTTP/1.1"),
        ),
    ] {
        let mut request = proxy_client.get(&url);
        if host == "aicode.googleapis.com" {
            request = request.bearer_auth(token);
        }
        assert_eq!(request.send().await.unwrap().status(), 200, "{url}");
        assert!(
            fixture
                .requests
                .recv()
                .await
                .unwrap()
                .starts_with(&format!("CONNECT {host}:443 "))
        );
        let forwarded = fixture.requests.recv().await.unwrap();
        assert!(forwarded.starts_with(&line), "{forwarded}");
    }
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
    // Nor does a scope check with another, a repeated, a mismatched or no token.
    let base = format!(
        "{}/antigravity/{}",
        running.url,
        crate::antigravity::TOKENINFO_URL
    );
    for (url, bearer, status) in [
        (format!("{base}?access_token=other-token"), None, 401),
        (
            format!("{base}?access_token={token}&access_token={token}"),
            None,
            400,
        ),
        (
            format!("{base}?access_token={token}"),
            Some("other-token"),
            400,
        ),
        (base.clone(), None, 401),
    ] {
        let mut request = http().get(&url);
        if let Some(bearer) = bearer {
            request = request.bearer_auth(bearer);
        }
        assert_eq!(request.send().await.unwrap().status(), status, "{url}");
    }
    assert!(fixture.requests.try_recv().is_err());
    // They are not model calls, and the log keeps no token.
    running.server.logger.flush().unwrap();
    let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
    assert!(!raw.contains("model_call_finished") && !raw.contains(token));
    assert!(!raw.contains("access_token=") && !raw.contains("other-token"));
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
    assert_eq!(running.server.antigravity_identities.len(), 1);
    let call = first_record(&running, "model_call_finished").await;
    assert_eq!(call["account_id"], "remote@example.invalid");
    assert_eq!(call["account_label"], "Remote@Example.invalid");
    assert_eq!(call["credential_match"], "configured");
    assert_eq!(call["profile_proxy"], "lookup");
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
    // So does the access token its refresh issued, which the client kept
    // across the restart, with no account lookup.
    antigravity_request(&open, "renewed-access").await;
    expect_payload(&mut payload, "renewed-access").await;
    assert!(token.requests.try_recv().is_err());
    let saved = std::fs::read_to_string(&file).unwrap();
    assert!(!saved.contains("renewed-access"));
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
async fn antigravity_upstream_errors_log_their_reason_and_reach_the_client_intact() {
    let _login = crate::antigravity::TEST_LOGIN.lock().await;
    let (running, mut fixture) = antigravity_auto_setup(false, "permission_denied").await;
    // As agy's periodic config fetch: a GET that is not a model call.
    let reply = http()
        .get(format!(
            "{}/antigravity/https://aicode.googleapis.com/v1alpha:fetchConfig",
            running.url
        ))
        .bearer_auth("access-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), 403);
    assert!(
        reply
            .text()
            .await
            .unwrap()
            .contains("IAM_PERMISSION_DENIED")
    );
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
            .starts_with("GET /v1alpha:fetchConfig ")
    );
    // A 403 never starts a location cooldown.
    assert_eq!(
        antigravity_call(&running, "streamGenerateContent")
            .await
            .status(),
        403
    );
    running.server.logger.flush().unwrap();
    let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
    let finished = raw
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|line| line["event"] == "request_finished")
        .collect::<Vec<_>>();
    assert_eq!(finished.len(), 2, "{raw}");
    for line in &finished {
        assert_eq!(line["status"], "403");
        assert_eq!(line["upstream_error_status"], "PERMISSION_DENIED");
        assert_eq!(line["upstream_error_reason"], "IAM_PERMISSION_DENIED");
        assert_eq!(
            line["upstream_error_message"],
            "The caller does not have permission"
        );
    }
    assert!(!raw.contains("secret-project"));
    assert!(!raw.contains("access-secret"));
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
    running.server.logger.flush().unwrap();
    let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
    let lines = raw
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    let upstream = lines
        .iter()
        .find(|line| line["event"] == "upstream_response" && line["status"] == "400")
        .unwrap_or_else(|| panic!("upstream 400 missing from log: {raw}"));
    assert_eq!(upstream["upstream_error_status"], "FAILED_PRECONDITION");
    assert_eq!(
        upstream["upstream_error_message"],
        "User location is not supported for the API use."
    );
    assert!(upstream.get("headers_ms").is_some(), "{upstream}");
    let failed = lines
        .iter()
        .find(|line| {
            line["event"] == "request_failed"
                && line["reason"] == "antigravity_location_cooldown"
                && line["request_id"] == upstream["request_id"]
        })
        .unwrap_or_else(|| panic!("client close missing from log: {raw}"));
    assert!(
        failed.get("status").is_none(),
        "the client was not sent the upstream 400: {failed}"
    );
    assert!(lines.iter().any(|line| {
        line["event"] == "model_call_failed"
            && line["request_id"] == upstream["request_id"]
            && line.get("status").is_none()
    }));
    crate::antigravity::set_test_login(crate::antigravity::TEST_SAVED_TOKEN, "saved-refresh");
}

#[tokio::test]
async fn antigravity_first_token_separates_local_cooldown_from_forwarding() {
    let _login = crate::antigravity::TEST_LOGIN.lock().await;
    let (running, _fixture) = antigravity_auto_setup(false, "location_once_sse").await;
    let call = || {
        http()
            .post(format!(
                "{}/antigravity/v1internal:streamGenerateContent",
                running.url
            ))
            .bearer_auth("access-secret")
            .body(r#"{"model":"test"}"#)
            .send()
    };
    assert!(call().await.is_err());
    // Exercise the real 400 -> next request -> wait -> SSE path without
    // spending three minutes asleep. Long intervals use the paused-clock tests.
    running
        .server
        .location_cooldowns
        .shorten_for_test(Duration::from_millis(200));
    assert!(
        call()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
            .contains("private-output")
    );
    // A repeat after success must not inherit the prior request's wait.
    assert!(
        call()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
            .contains("private-output")
    );
    running.server.logger.flush().unwrap();
    let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
    let rows: Vec<serde_json::Value> = raw
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    let calls: Vec<_> = rows
        .iter()
        .filter(|r| r["event"] == "model_call_finished")
        .collect();
    assert_eq!(calls.len(), 2, "{raw}");
    let ms =
        |row: &serde_json::Value, key: &str| row[key].as_str().unwrap().parse::<u64>().unwrap();
    let waited = calls[0];
    let wait = ms(waited, "location_cooldown_actual_ms");
    assert!(wait > 0, "{waited}");
    let sent = ms(waited, "upstream_started_ms");
    let first = ms(waited, "first_token_ms");
    assert!(sent + 1 >= wait, "{waited}");
    assert_eq!(ms(waited, "forwarded_first_token_ms"), first - sent);
    assert!(calls[1].get("location_cooldown_actual_ms").is_none());
    assert!(calls[1].get("forwarded_first_token_ms").is_some());
    let failed = rows
        .iter()
        .find(|r| r["event"] == "model_call_failed")
        .unwrap();
    assert!(failed.get("first_token_ms").is_none());
    assert!(failed.get("forwarded_first_token_ms").is_none());
    crate::antigravity::set_test_login(crate::antigravity::TEST_SAVED_TOKEN, "saved-refresh");
}
