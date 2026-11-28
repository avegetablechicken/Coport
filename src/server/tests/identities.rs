use super::*;

#[tokio::test]
async fn traffic_labels_probe_missing_claude_metadata_and_cache_failures_safely() {
    for (behavior, succeeds) in [("sse", true), ("profile_unauthorized", false)] {
        let mut lookup = fixture("http", behavior).await;
        let endpoint = format!("http://127.0.0.1:{}", lookup.addr.port());
        let dir = tempfile::tempdir().unwrap();
        let credentials = dir.path().join(".credentials.json");
        let saved = r#"{"claudeAiOauth":{"accessToken":"saved-secret"}}"#;
        std::fs::write(&credentials, saved).unwrap();
        let running = running(&format!("proxies:\n  lookup: {endpoint}\nclaude:\n  config_dirs: [{}]\n  base_url: https://upstream.invalid\n  routing:\n    account:\n      remote@example.invalid: none\n    account_probe: lookup\n",serde_json::to_string(dir.path()).unwrap())).await;
        trust(&running, &lookup, &endpoint);
        for _ in 0..3 {
            let cached = running.server.cached_traffic_credential_labels().await;
            assert!(!cached.contains_key(&("Claude".into(), "remote-account".into())));
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(100), lookup.requests.recv())
                .await
                .is_err(),
            "Reading cached display labels must not contact the upstream"
        );
        let (first, second) = tokio::join!(
            running.server.traffic_credential_labels(),
            running.server.traffic_credential_labels()
        );
        assert_eq!(first, second);
        assert_eq!(
            first
                .get(&("Claude".into(), "remote-account".into()))
                .map(String::as_str),
            if succeeds {
                Some("remote@example.invalid")
            } else {
                None
            }
        );
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
        assert!(lookup.requests.try_recv().is_err());
        assert_eq!(std::fs::read_to_string(&credentials).unwrap(), saved);
        // Only failures are recorded; identities live in the profile cache.
        let before = running.server.claude_identities.failures().await;
        assert_eq!(before.len(), usize::from(!succeeds));
        assert_eq!(
            running.server.cached_traffic_credential_labels().await,
            first
        );
        assert_eq!(running.server.traffic_credential_labels().await, first);
        assert!(lookup.requests.try_recv().is_err());
        assert_eq!(running.server.claude_identities.failures().await, before);
    }
}

#[tokio::test]
async fn a_looked_up_account_is_remote_without_discovery() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"saved-secret"}}"#,
    )
    .unwrap();
    // No account_probe or fallback: nothing here looks accounts up.
    let running = running(&format!("claude:\n  config_dirs: [{}]\n  base_url: https://upstream.invalid\n  routing:\n    account:\n      remote@example.invalid: none\n      other@example.invalid: none\n", serde_json::to_string(dir.path()).unwrap())).await;
    let states = || async { running.server.account_route_states().await[1].clone() };
    assert_eq!(states().await["remote@example.invalid"], "inactive");
    // A server before a configuration change looked the saved login up.
    let identity = crate::claude::ClaudeIdentity::profile(
        &json!({"account":{"uuid":"remote-account", "email":"remote@example.invalid"}}),
    )
    .unwrap();
    running
        .server
        .claude_identities
        .remember("saved-secret", async { Ok(identity) })
        .await
        .unwrap();
    let states = states().await;
    assert_eq!(states["remote@example.invalid"], "remote");
    assert_eq!(states["other@example.invalid"], "inactive");
}

#[tokio::test]
async fn a_requests_account_lookup_settles_a_failed_discovery() {
    let mut lookup = fixture("http", "profile_retry_identity").await;
    let endpoint = format!("http://127.0.0.1:{}", lookup.addr.port());
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"saved-secret"}}"#,
    )
    .unwrap();
    let running = running(&format!("proxies:\n  lookup: {endpoint}\nclaude:\n  config_dirs: [{}]\n  base_url: https://upstream.invalid\n  routing:\n    account:\n      remote@example.invalid: none\n    account_probe: lookup\n", serde_json::to_string(dir.path()).unwrap())).await;
    trust(&running, &lookup, &endpoint);
    let identity = (
        "Claude".to_owned(),
        "12345678-1234-4234-8234-123456789abc".to_owned(),
    );
    let looked_up = async |lookup: &mut Fixture| {
        for sent in ["CONNECT ", "GET /api/oauth/profile "] {
            assert!(lookup.requests.recv().await.unwrap().starts_with(sent));
        }
    };
    // Account discovery fails first.
    assert!(
        !running
            .server
            .traffic_credential_labels()
            .await
            .contains_key(&identity)
    );
    looked_up(&mut lookup).await;
    // Then a request carrying the same token looks its account up.
    http()
        .post(format!("{}/anthropic/v1/messages", running.url))
        .bearer_auth("saved-secret")
        .body("payload")
        .send()
        .await
        .unwrap();
    looked_up(&mut lookup).await;
    // Within the retry delay, discovery answers with that account.
    assert_eq!(
        running
            .server
            .traffic_credential_labels()
            .await
            .get(&identity)
            .map(String::as_str),
        Some("remote@example.invalid")
    );
    assert!(lookup.requests.try_recv().is_err());
}

#[tokio::test]
async fn periodic_credentials_enrich_missing_identity_retry_failures_and_cache_success() {
    let mut lookup = fixture("http", "profile_retry_identity").await;
    let endpoint = format!("http://127.0.0.1:{}", lookup.addr.port());
    let dir = tempfile::tempdir().unwrap();
    let credentials = dir.path().join(".credentials.json");
    let saved = r#"{"claudeAiOauth":{"accessToken":"saved-secret"}}"#;
    std::fs::write(&credentials, saved).unwrap();
    let running = running(&format!("proxies:\n  lookup: {endpoint}\nclaude:\n  config_dirs: [{}]\n  base_url: https://upstream.invalid\n  routing:\n    account:\n      remote@example.invalid: none\n    account_probe: lookup\n", serde_json::to_string(dir.path()).unwrap())).await;
    trust(&running, &lookup, &endpoint);
    let identity = (
        "Claude".into(),
        "12345678-1234-4234-8234-123456789abc".into(),
    );
    *running.server.credential_checks.lock().await = None;
    running.server.credential_reports().await;
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
    assert!(
        !running
            .server
            .cached_traffic_credential_labels()
            .await
            .contains_key(&identity)
    );
    for _ in 0..3 {
        running.server.credential_reports().await;
        running.server.cached_traffic_credential_labels().await;
    }
    assert!(
        lookup.requests.try_recv().is_err(),
        "Neither cached reports nor display reads retry a failed probe"
    );
    // The next periodic check, a minute later, still holds the failure.
    *running.server.credential_checks.lock().await = None;
    running
        .server
        .claude_identities
        .backdate(Duration::from_secs(61))
        .await;
    running.server.credential_reports().await;
    assert!(
        lookup.requests.try_recv().is_err(),
        "A failed probe is not retried on every periodic check"
    );
    // Expire both periodic and failed-probe caches, without waiting.
    *running.server.credential_checks.lock().await = None;
    running
        .server
        .claude_identities
        .backdate(FAILED_PROBE_RETRY + Duration::from_secs(1))
        .await;
    running.server.credential_reports().await;
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
    assert_eq!(
        running
            .server
            .cached_traffic_credential_labels()
            .await
            .get(&identity)
            .map(String::as_str),
        Some("remote@example.invalid")
    );
    *running.server.credential_checks.lock().await = None;
    running.server.credential_reports().await;
    assert!(
        lookup.requests.try_recv().is_err(),
        "Successful identities are reused across cycles"
    );
    std::fs::write(
        &credentials,
        r#"{"claudeAiOauth":{"accessToken":"rotated-secret"}}"#,
    )
    .unwrap();
    *running.server.credential_checks.lock().await = None;
    running.server.credential_reports().await;
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
            .contains("authorization: Bearer rotated-secret")
    );
    assert!(
        !dir.path().join(".claude.json").exists(),
        "Probes must not rewrite Claude metadata"
    );
}

#[tokio::test]
async fn rebuilt_servers_reuse_shared_claude_identities() {
    let mut lookup = fixture("http", "sse").await;
    let endpoint = format!("http://127.0.0.1:{}", lookup.addr.port());
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"saved-secret"}}"#,
    )
    .unwrap();
    let running = running(&format!("proxies:\n  lookup: {endpoint}\nclaude:\n  config_dirs: [{}]\n  base_url: https://upstream.invalid\n  routing:\n    account:\n      remote@example.invalid: none\n    account_probe: lookup\n", serde_json::to_string(dir.path()).unwrap())).await;
    trust(&running, &lookup, &endpoint);
    let identity = ("Claude".into(), "remote-account".into());
    assert!(
        running
            .server
            .traffic_credential_labels()
            .await
            .contains_key(&identity)
    );
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
    // A configuration reload starts a server without account checks.
    let rebuilt = Server::new(running.server.config.clone(), running.server.logger.clone())
        .with_claude_profiles(running.server.claude_identities.profiles());
    for _ in 0..3 {
        assert_eq!(
            rebuilt
                .traffic_credential_labels()
                .await
                .get(&identity)
                .map(String::as_str),
            Some("remote@example.invalid")
        );
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(100), lookup.requests.recv())
            .await
            .is_err(),
        "A shared identity is not looked up again"
    );
}

#[tokio::test]
async fn periodic_identity_probe_requires_explicit_probe_and_skips_known_local_identity() {
    for (probe, metadata) in [(false, false), (false, true), (true, true)] {
        let mut lookup = fixture("http", "sse").await;
        let endpoint = format!("http://127.0.0.1:{}", lookup.addr.port());
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"saved-secret"}}"#,
        )
        .unwrap();
        if metadata {
            std::fs::write(dir.path().join(".claude.json"), r#"{"oauthAccount":{"accountUuid":"local-account","emailAddress":"remote@example.invalid"}}"#).unwrap();
        }
        let probe = if probe {
            "    account_probe: lookup\n"
        } else {
            ""
        };
        let running = running(&format!("proxies:\n  lookup: {endpoint}\nclaude:\n  config_dirs: [{}]\n  base_url: https://upstream.invalid\n  routing:\n    account:\n      remote@example.invalid: none\n    account_fallback: lookup\n{probe}",serde_json::to_string(dir.path()).unwrap())).await;
        trust(&running, &lookup, &endpoint);
        *running.server.credential_checks.lock().await = None;
        running.server.credential_reports().await;
        assert!(
            lookup.requests.try_recv().is_err(),
            "Fallback alone cannot authorize identity discovery, and local metadata needs no probe"
        );
    }
}

#[tokio::test]
async fn traffic_labels_do_not_probe_without_an_explicit_lookup_route() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"saved-secret"}}"#,
    )
    .unwrap();
    let running = running(&format!("claude:\n  config_dirs: [{}]\n  base_url: https://upstream.invalid\n  routing:\n    account: {{'remote@example.invalid': none}}\n",serde_json::to_string(dir.path()).unwrap())).await;
    let labels = running.server.traffic_credential_labels().await;
    assert!(!labels.contains_key(&("Claude".into(), "remote-account".into())));
    assert!((running.server.claude_identities.len() == 0));
}

#[tokio::test]
async fn account_status_reads_discovered_identities_without_probing() {
    for (behavior, expected) in [("sse", "remote"), ("profile_unauthorized", "probe_failed")] {
        let mut lookup = fixture("http", behavior).await;
        let endpoint = format!("http://127.0.0.1:{}", lookup.addr.port());
        let dir = tempfile::tempdir().unwrap();
        let credentials = dir.path().join(".credentials.json");
        std::fs::write(
            &credentials,
            r#"{"claudeAiOauth":{"accessToken":"saved-secret"}}"#,
        )
        .unwrap();
        let running = running(&format!("proxies:\n  lookup: {endpoint}\nclaude:\n  config_dirs: [{}]\n  base_url: https://upstream.invalid\n  routing:\n    account:\n      remote@example.invalid: none\n      other@example.invalid: none\n    account_probe: lookup\n", serde_json::to_string(dir.path()).unwrap())).await;
        trust(&running, &lookup, &endpoint);
        // Before discovery, the saved login could belong to either route.
        for _ in 0..3 {
            let states = running.server.account_route_states().await;
            assert_eq!(states[1]["remote@example.invalid"], "unknown");
            assert_eq!(states[1]["other@example.invalid"], "unknown");
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(100), lookup.requests.recv())
                .await
                .is_err(),
            "Reading account status never probes"
        );
        // Periodic discovery probes once; status then reads its result.
        *running.server.credential_checks.lock().await = None;
        running.server.credential_reports().await;
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
        let states = running.server.account_route_states().await;
        assert_eq!(states[1]["remote@example.invalid"], expected);
        assert_eq!(
            states[1]["other@example.invalid"],
            if expected == "remote" {
                "inactive"
            } else {
                "probe_failed"
            }
        );
        // A token's identity does not change, and a failure stays the latest
        // result after its retry delay: neither expires with time.
        running
            .server
            .claude_identities
            .backdate(Duration::from_secs(24 * 3600))
            .await;
        for _ in 0..3 {
            assert_eq!(running.server.account_route_states().await, states);
        }
        assert!(lookup.requests.try_recv().is_err());
        // Metadata that no longer matches a route still uses the saved token's result.
        std::fs::write(
            dir.path().join(".claude.json"),
            r#"{"oauthAccount":{"accountUuid":"stale","emailAddress":"stale@example.invalid"}}"#,
        )
        .unwrap();
        assert_eq!(running.server.account_route_states().await, states);
        // A changed credential cannot reuse another token's result.
        std::fs::write(
            &credentials,
            r#"{"claudeAiOauth":{"accessToken":"replacement-secret"}}"#,
        )
        .unwrap();
        assert_eq!(
            running.server.account_route_states().await[1]["remote@example.invalid"],
            "unknown"
        );
        // Local matches take precedence over any remote result.
        std::fs::write(
            dir.path().join(".claude.json"),
            r#"{"oauthAccount":{"accountUuid":"local","emailAddress":"remote@example.invalid"}}"#,
        )
        .unwrap();
        assert_eq!(
            running.server.account_route_states().await[1]["remote@example.invalid"],
            "active"
        );
        std::fs::remove_file(&credentials).unwrap();
        assert_eq!(
            running.server.account_route_states().await[1]["remote@example.invalid"],
            "inactive"
        );
        assert!(lookup.requests.try_recv().is_err());
        running.server.logger.flush().unwrap();
        let log = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
        assert!(!log.contains("saved-secret") && !log.contains("replacement-secret"));
    }
}

#[tokio::test]
async fn account_status_never_shows_a_discovered_route_a_fallback_takes() {
    let mut lookup = fixture("http", "sse").await;
    let endpoint = format!("http://127.0.0.1:{}", lookup.addr.port());
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"saved-secret"}}"#,
    )
    .unwrap();
    let running = running(&format!("proxies:\n  lookup: {endpoint}\nclaude:\n  config_dirs: [{}]\n  base_url: https://upstream.invalid\n  routing:\n    account:\n      remote@example.invalid: none\n    account_probe: lookup\n    account_fallback: none\n", serde_json::to_string(dir.path()).unwrap())).await;
    trust(&running, &lookup, &endpoint);
    *running.server.credential_checks.lock().await = None;
    running.server.credential_reports().await;
    while let Ok(Some(_)) =
        tokio::time::timeout(Duration::from_millis(200), lookup.requests.recv()).await
    {}
    // The saved login's requests take account_fallback, whatever its account.
    assert_eq!(
        running.server.account_route_states().await[1]["remote@example.invalid"],
        "inactive"
    );
}

#[tokio::test]
async fn account_status_leaves_unreadable_environment_sources_unknown() {
    let running = running("claude:\n  config_dirs: []\n  auth_env: COPORT_TEST_UNSET_ACCOUNT_STATE\n  routing:\n    account: {'user@example.invalid': none}\n    account_probe: none\n").await;
    for _ in 0..2 {
        assert_eq!(
            running.server.account_route_states().await[1]["user@example.invalid"],
            "unknown"
        );
    }
    assert!((running.server.claude_identities.len() == 0));
}

#[tokio::test]
async fn claude_saved_token_without_metadata_probes_then_routes_and_caches() {
    for file_only in [false, true] {
        let mut lookup = fixture("http", "sse").await;
        let mut payload = fixture("http", "redirect").await;
        let lookup_endpoint = format!("http://127.0.0.1:{}", lookup.addr.port());
        let payload_endpoint = format!("http://127.0.0.1:{}", payload.addr.port());
        let credentials = tempfile::tempdir().unwrap();
        let file = credentials.path().join(".credentials.json");
        std::fs::write(&file, r#"{"claudeAiOauth":{"accessToken":"saved-secret"}}"#).unwrap();
        let dir = serde_json::to_string(&credentials.path()).unwrap();
        let running = running(&format!("proxies:\n  lookup: {lookup_endpoint}\n  selected: {payload_endpoint}\nclaude:\n  config_dirs: [{dir}]\n  account_auth_file_only: {file_only}\n  base_url: https://upstream.invalid\n  routing:\n    account:\n      remote@example.invalid: selected\n    account_probe: lookup\n")).await;
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
        assert_eq!(running.server.claude_identities.len(), 1);
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
    assert_eq!(running.server.claude_identities.len(), 2);
    running
        .server
        .claude_identities
        .backdate(Duration::from_secs(30 * 24 * 60 * 60))
        .await;
    let response = http()
        .post(format!("{}/anthropic/api/oauth/usage", running.url))
        .bearer_auth("first-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 405); // A remotely identified account passes auth, then method validation.
    assert!(lookup.requests.try_recv().is_err());
    assert!(payload.requests.try_recv().is_err());
    running.server.logger.flush().unwrap();
    let log = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
    assert!(log.contains("remote-account"));
    assert!(!log.contains("first-secret"));
    assert!(!log.contains("second-secret"));
    // A client's own token counts as configured once its account is mapped.
    let routed: Vec<serde_json::Value> = log
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|row| {
            row["account_id"] == "remote-account" && row.get("credential_match").is_some()
        })
        .collect();
    assert!(!routed.is_empty());
    assert!(
        routed
            .iter()
            .all(|row| row["credential_match"] == "configured"),
        "{routed:?}"
    );
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
        assert!((running.server.claude_identities.len() == 0));
        running.server.logger.flush().unwrap();
        let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
        let lines = raw
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        let upstream = lines
            .iter()
            .find(|line| line["event"] == "upstream_response")
            .unwrap_or_else(|| panic!("profile response missing from log: {raw}"));
        assert_eq!(
            upstream["status"],
            match mode {
                "profile_unauthorized" => "401",
                "profile_invalid" => "200",
                "profile_redirect" => "302",
                _ => unreachable!(),
            },
            "{mode} {upstream}"
        );
        assert_eq!(upstream["upstream_path"], "/api/oauth/profile");
        assert!(upstream.get("model_call_id").is_none(), "{upstream}");
        let client = lines
            .iter()
            .find(|line| line["event"] == "request_failed" || line["event"] == "request_rejected")
            .unwrap_or_else(|| panic!("client outcome missing from log: {raw}"));
        assert_eq!(client["status"], expected.to_string(), "{mode} {client}");
    }
}

#[tokio::test]
async fn ordered_probes_are_credential_free_and_cached() {
    let mut fixture = fixture("direct", "redirect").await;
    let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_port = dead.local_addr().unwrap().port();
    drop(dead);
    let running=running(&format!("proxies:\n  dead: http://127.0.0.1:{dead_port}\ncodex:\n  routing:\n    fallback: [dead, none]\n  base_url:\n    api_key: https://upstream.invalid/v1\n")).await;
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
async fn statistics_preserve_aged_claude_identity() {
    let mut lookup = fixture("http", "profile_unauthorized").await;
    let endpoint = format!("http://127.0.0.1:{}", lookup.addr.port());
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"saved-secret"}}"#,
    )
    .unwrap();
    let running = running(&format!("proxies:\n  lookup: {endpoint}\nclaude:\n  config_dirs: [{}]\n  base_url: https://upstream.invalid\n  routing:\n    account:\n      remote@example.invalid: none\n    account_probe: lookup\n", serde_json::to_string(dir.path()).unwrap())).await;
    trust(&running, &lookup, &endpoint);
    let identity = crate::claude::ClaudeIdentity::profile(
        &json!({"account":{"uuid":"remote-account", "email":"remote@example.invalid"}}),
    )
    .unwrap();
    let identities = &running.server.claude_identities;
    identities
        .remember("saved-secret", async { Ok(identity) })
        .await
        .unwrap();
    identities.backdate(Duration::from_secs(301)).await;
    let labels = running.server.traffic_credential_labels().await;
    assert_eq!(
        labels
            .get(&("Claude".into(), "remote-account".into()))
            .map(String::as_str),
        Some("remote@example.invalid")
    );
    let retained = identities.known("saved-secret", false).is_some();
    let sent_lookup = lookup.requests.try_recv().is_ok();
    assert!(
        retained && !sent_lookup,
        "statistics invalidated request cache: retained={retained}, sent_lookup={sent_lookup}, has_label={}",
        labels.contains_key(&("Claude".into(), "remote-account".into()))
    );
}
