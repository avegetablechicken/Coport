use super::*;

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

#[tokio::test]
async fn refresh_failure_is_not_replayed_and_counts_once() {
    let mut fixture = fixture("http", "drop_always").await;
    let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
    let running = running(&format!("proxies:\n  selected: {endpoint}\nclaude:\n  config_dirs: []\n  account_auth_file_only: false\n  routing:\n    account_fallback: selected\n")).await;
    trust(&running, &fixture, &endpoint);
    let response = http()
        .post(format!("{}/anthropic/v1/oauth/token", running.url))
        .header("content-type", "application/json")
        .body(
            r#"{"client_id":"app","grant_type":"refresh_token","refresh_token":"refresh-secret"}"#,
        )
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
async fn codex_refresh_token_saved_in_two_homes_is_ambiguous() {
    let homes = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    for (home, account) in homes.iter().zip(["acct-1", "acct-2"]) {
        std::fs::write(
            home.path().join("auth.json"),
            format!(
                r#"{{"tokens":{{"account_id":"{account}","access_token":"access-{account}","refresh_token":"refresh-secret"}}}}"#
            ),
        )
        .unwrap();
    }
    let running = running(&format!(
        // An unreachable proxy keeps a wrongly forwarded refresh off the network.
        "proxies:\n  dead: http://127.0.0.1:9\ncodex:\n  homes: [{}, {}]\n  routing:\n    account_fallback: dead\n",
        serde_json::to_string(&homes[0].path()).unwrap(),
        serde_json::to_string(&homes[1].path()).unwrap(),
    ))
    .await;
    let response = http()
        .post(format!("{}/oauth/token", running.url))
        .header("content-type", "application/json")
        .body(
            r#"{"client_id":"app","grant_type":"refresh_token","refresh_token":"refresh-secret"}"#,
        )
        .send()
        .await
        .unwrap();
    let status = response.status();
    assert_eq!(status, 409, "{}", response.text().await.unwrap());
}
