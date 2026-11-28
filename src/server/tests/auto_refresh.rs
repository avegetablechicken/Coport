use super::*;

async fn auto_refresh_setup(
    claude: bool,
    enabled: bool,
    mode: &'static str,
) -> (tempfile::TempDir, std::path::PathBuf, Running, Fixture) {
    let fixture = fixture("http", mode).await;
    let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
    let home = tempfile::tempdir().unwrap();
    let (filename, value, section) = if claude {
        (
            ".credentials.json",
            json!({"other":"preserved","claudeAiOauth":{"accessToken":"access-secret","refreshToken":"refresh-secret","scopes":["user:inference","user:profile"]}}),
            format!(
                "claude:\n  config_dirs: [{}]\n  routing:\n    account_fallback: selected\n    api_key: {{\"upstream.invalid/v1\": selected}}\n",
                serde_json::to_string(home.path()).unwrap()
            ),
        )
    } else {
        (
            "auth.json",
            json!({"other":"preserved","tokens":{"account_id":"acct-1","access_token":"access-secret","refresh_token":"refresh-secret"}}),
            format!(
                "codex:\n  homes: [{}]\n  base_url:\n    account: https://upstream.invalid/backend-api\n  routing:\n    account: {{acct-1: selected}}\n    api_key: {{\"upstream.invalid/v1\": selected}}\n",
                serde_json::to_string(home.path()).unwrap()
            ),
        )
    };
    let file = home.path().join(filename);
    std::fs::write(&file, value.to_string()).unwrap();
    let running = running(&format!(
        "auto_refresh_access_token: {enabled}\nproxies:\n  selected: {endpoint}\n{section}"
    ))
    .await;
    trust(&running, &fixture, &endpoint);
    (home, file, running, fixture)
}

fn auto_model_path(claude: bool) -> &'static str {
    if claude {
        "/anthropic/v1/messages"
    } else {
        "/responses"
    }
}

async fn auto_model(running: &Running, claude: bool) -> reqwest::Response {
    http()
        .post(format!("{}{}", running.url, auto_model_path(claude)))
        .bearer_auth("access-secret")
        .header("cookie", "private-cookie")
        .header("user-agent", "test-model-client")
        .body("model-body")
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn auto_refresh_rotates_saved_tokens_once_for_concurrent_and_repeated_model_calls() {
    for claude in [false, true] {
        let (_home, file, running, mut fixture) =
            auto_refresh_setup(claude, true, "auto_success").await;
        let (one, two) = tokio::join!(auto_model(&running, claude), auto_model(&running, claude));
        assert_eq!(one.status(), 200);
        assert_eq!(two.status(), 200);
        assert_eq!(
            auto_model(&running, claude).await.text().await.unwrap(),
            "model-result"
        );
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        let service = if claude {
            Service::Claude
        } else {
            Service::Codex
        };
        crate::saved_login::with_login!(service, L => {
            assert_eq!(crate::saved_login::access::<L>(&saved), Some("new-access"));
            assert_eq!(crate::saved_login::refresh::<L>(&saved), Some("new-refresh"));
        });
        assert_eq!(saved["other"], "preserved");
        // A configured API gateway must receive the caller's original token,
        // even if it also happens to be an old OAuth alias in this process.
        let scope = if claude { "anthropic" } else { "codex" };
        let response = http()
            .post(format!(
                "{}/{scope}/https://upstream.invalid/v1/responses",
                running.url
            ))
            .bearer_auth("access-secret")
            .body("api-body")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
        let requests = take_requests(&mut fixture);
        let refreshes: Vec<_> = requests
            .iter()
            .filter(|r| r.contains("refresh-secret"))
            .collect();
        assert_eq!(refreshes.len(), 1, "{requests:?}");
        let refresh = refreshes[0];
        for forbidden in [
            "access-secret",
            "private-cookie",
            "test-model-client",
            "authorization:",
            "model-body",
        ] {
            assert!(!refresh.contains(forbidden));
        }
        if claude {
            let body: serde_json::Value =
                serde_json::from_str(refresh.split("\r\n\r\n").nth(1).unwrap()).unwrap();
            assert_eq!(body["scope"], "user:inference user:profile");
            assert_eq!(body["client_id"], "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
        } else {
            assert!(
                refresh
                    .contains("grant_type=refresh_token&refresh_token=refresh-secret&client_id=")
            );
        }
        assert!(
            requests
                .iter()
                .filter(|r| r.contains("authorization: Bearer new-access"))
                .all(|r| r.ends_with("model-body"))
        );
        running.server.logger.flush().unwrap();
        let log = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
        for secret in [
            "access-secret",
            "refresh-secret",
            "new-access",
            "new-refresh",
        ] {
            assert!(!log.contains(secret));
        }
        // A logout revokes old access-token aliases.
        std::fs::remove_file(file).unwrap();
        assert_ne!(auto_model(&running, claude).await.status(), 200);
    }
}

#[tokio::test]
async fn auto_refresh_is_opt_in_and_never_runs_for_usage_reads() {
    for claude in [false, true] {
        let (_home, file, running, mut fixture) =
            auto_refresh_setup(claude, false, "auto_success").await;
        let before = std::fs::read(&file).unwrap();
        assert_eq!(auto_model(&running, claude).await.status(), 401);
        assert_eq!(take_requests(&mut fixture).len(), 1);
        assert_eq!(std::fs::read(file).unwrap(), before);
        let (_home, file, running, mut fixture) =
            auto_refresh_setup(claude, true, "auto_success").await;
        let before = std::fs::read(&file).unwrap();
        let path = if claude {
            "/anthropic/api/oauth/usage"
        } else {
            "/wham/usage"
        };
        for _ in 0..3 {
            let response = http()
                .get(format!("{}{path}", running.url))
                .bearer_auth("access-secret")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 401);
        }
        assert_eq!(take_requests(&mut fixture).len(), 3);
        assert_eq!(std::fs::read(file).unwrap(), before);
    }
}

#[tokio::test]
async fn auto_refresh_failure_is_bounded_and_keeps_the_original_response() {
    for mode in [
        "auto_rejected",
        "auto_invalid",
        "auto_rate_limit",
        "auto_retry_401",
    ] {
        let (_home, file, running, mut fixture) = auto_refresh_setup(false, true, mode).await;
        let before = std::fs::read(&file).unwrap();
        for _ in 0..2 {
            let response = auto_model(&running, false).await;
            assert_eq!(response.status(), 401);
            assert_eq!(response.text().await.unwrap(), "original-unauthorized");
        }
        let requests = take_requests(&mut fixture);
        // A second 401 after rotation is allowed one refresh on its newer
        // generation, but the original request itself is never retried twice.
        let count = requests
            .iter()
            .filter(|r| r.starts_with("POST /oauth/token "))
            .count();
        assert_eq!(
            count,
            if mode == "auto_retry_401" { 2 } else { 1 },
            "{mode}"
        );
        if mode != "auto_retry_401" {
            assert_eq!(std::fs::read(file).unwrap(), before);
        }
    }
}

/// Waits for the proxy's refresh request to reach the upstream fixture.
async fn await_refresh(fixture: &mut Fixture) {
    loop {
        let request = fixture.requests.recv().await.unwrap();
        if request.starts_with("POST /oauth/token ") || request.starts_with("POST /v1/oauth/token ")
        {
            break;
        }
    }
}

fn spawn_auto_model(running: &Running, claude: bool) -> tokio::task::JoinHandle<reqwest::Response> {
    let url = format!("{}{}", running.url, auto_model_path(claude));
    tokio::spawn(async move {
        http()
            .post(url)
            .bearer_auth("access-secret")
            .body("model-body")
            .send()
            .await
            .unwrap()
    })
}

fn read_json(file: &std::path::Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap()
}

#[tokio::test]
async fn auto_refresh_does_not_overwrite_an_external_account_switch() {
    for claude in [false, true] {
        let (_home, file, running, mut fixture) =
            auto_refresh_setup(claude, true, "auto_wait").await;
        let call = spawn_auto_model(&running, claude);
        await_refresh(&mut fixture).await;
        let switched = if claude {
            r#"{"claudeAiOauth":{"accessToken":"external-access","refreshToken":"external-refresh"}}"#
        } else {
            r#"{"tokens":{"account_id":"another","access_token":"external-access","refresh_token":"external-refresh"}}"#
        };
        std::fs::write(&file, switched).unwrap();
        fixture.release.notify_one();
        assert_eq!(call.await.unwrap().status(), 401);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), switched);
        assert!(take_requests(&mut fixture).is_empty());
    }
}

#[tokio::test]
async fn auto_refresh_merges_its_rotation_into_unrelated_edits_made_meanwhile() {
    for claude in [false, true] {
        let (home, file, running, mut fixture) =
            auto_refresh_setup(claude, true, "auto_wait").await;
        let locks = crate::claude::refresh_lock::lock_paths(home.path());
        let call = spawn_auto_model(&running, claude);
        await_refresh(&mut fixture).await;
        // Claude Code's lock is held for the whole Claude renewal, and only
        // for Claude: Codex has no such protocol.
        assert_eq!(locks.iter().all(|lock| lock.is_dir()), claude);
        // Another program changes an unrelated part of the saved login while
        // the refresh token is being spent, e.g. Claude Code's MCP logins.
        let mut edited = read_json(&file);
        edited["mcpOAuth"] = json!({"server":{"accessToken":"mcp-access"}});
        let section = if claude { "claudeAiOauth" } else { "tokens" };
        edited[section]["clientId"] = "kept-client".into();
        std::fs::write(&file, edited.to_string()).unwrap();
        fixture.release.notify_one();
        let response = call.await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.text().await.unwrap(), "model-result");
        let saved = read_json(&file);
        let service = if claude {
            Service::Claude
        } else {
            Service::Codex
        };
        crate::saved_login::with_login!(service, L => {
            assert_eq!(crate::saved_login::access::<L>(&saved), Some("new-access"));
            assert_eq!(crate::saved_login::refresh::<L>(&saved), Some("new-refresh"));
        });
        assert_eq!(saved["mcpOAuth"]["server"]["accessToken"], "mcp-access");
        assert_eq!(saved[section]["clientId"], "kept-client");
        assert_eq!(saved["other"], "preserved");
        assert!(locks.iter().all(|lock| !lock.exists()));
        // Requests still carrying the old access token use the rotation.
        assert_eq!(auto_model(&running, claude).await.status(), 200);
        let refreshes = take_requests(&mut fixture)
            .iter()
            .filter(|r| r.contains("refresh-secret"))
            .count();
        assert_eq!(refreshes, 0);
    }
}

#[tokio::test]
async fn auto_refresh_waits_for_claude_code_and_never_spends_the_token_it_renewed() {
    let (home, file, running, mut fixture) = auto_refresh_setup(true, true, "auto_success").await;
    let locks = crate::claude::refresh_lock::lock_paths(home.path());
    // Claude Code holds its refresh lock (both directories) and is renewing.
    for lock in &locks {
        std::fs::create_dir(lock).unwrap();
    }
    let call = spawn_auto_model(&running, true);
    // Claude Code renews once the model call has been refused, while this
    // server waits for its lock; a fixed delay raced both.
    loop {
        let request = fixture.requests.recv().await.unwrap();
        if request.starts_with("POST /v1/messages") {
            break;
        }
    }
    let renewed = json!({"other":"preserved","claudeAiOauth":{"accessToken":"code-access","refreshToken":"code-refresh","expiresAt":4102444800000u64}});
    std::fs::write(&file, renewed.to_string()).unwrap();
    for lock in locks.iter().rev() {
        std::fs::remove_dir(lock).unwrap();
    }
    // The request is left for the client to retry with its current login.
    let response = call.await.unwrap();
    assert_eq!(response.status(), 401);
    assert_eq!(response.text().await.unwrap(), "original-unauthorized");
    assert_eq!(read_json(&file), renewed);
    assert!(
        !take_requests(&mut fixture)
            .iter()
            .any(|r| r.contains("/oauth/token")),
        "the spent refresh token must not be sent again"
    );
    assert!(locks.iter().all(|lock| !lock.exists()));
}

#[cfg(unix)]
#[tokio::test]
async fn auto_refresh_takes_over_from_a_claude_code_that_died_holding_its_lock() {
    let (home, file, running, mut fixture) = auto_refresh_setup(true, true, "auto_success").await;
    // Claude Code exited mid-renewal seconds ago: its locks are far from
    // stale, but its owner record proves it gone.
    let locks = crate::claude::refresh_lock::hold_as_dead_claude_code(home.path());
    let response = auto_model(&running, true).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "model-result");
    assert_eq!(
        crate::saved_login::access::<crate::saved_login::Claude>(&read_json(&file)),
        Some("new-access")
    );
    let refreshes = take_requests(&mut fixture)
        .iter()
        .filter(|r| r.contains("refresh-secret"))
        .count();
    assert_eq!(refreshes, 1);
    assert!(locks.iter().all(|lock| !lock.exists()));
    assert!(
        !home
            .path()
            .join(crate::claude::refresh_lock::OWNER_FILE)
            .exists()
    );
}

#[tokio::test]
async fn auto_refresh_gives_up_while_another_renewal_holds_the_lock() {
    let (home, file, running, mut fixture) = auto_refresh_setup(true, true, "auto_success").await;
    let before = std::fs::read(&file).unwrap();
    let locks = crate::claude::refresh_lock::lock_paths(home.path());
    // Held by a live process: the current lock alone, or the legacy one alone.
    for held in [&locks[..], &locks[..1], &locks[1..]] {
        for lock in held {
            std::fs::create_dir(lock).unwrap();
        }
        let response = auto_model(&running, true).await;
        assert_eq!(response.status(), 401);
        assert_eq!(response.text().await.unwrap(), "original-unauthorized");
        assert_eq!(std::fs::read(&file).unwrap(), before);
        for lock in &locks {
            // Another holder's lock is never removed, nor ours left behind.
            assert_eq!(lock.exists(), held.contains(lock));
        }
        for lock in held {
            std::fs::remove_dir(lock).unwrap();
        }
    }
    assert!(
        !take_requests(&mut fixture)
            .iter()
            .any(|r| r.contains("/oauth/token"))
    );
    // A dead holder's stale lock does not block renewal for good.
    for lock in &locks {
        std::fs::create_dir(lock).unwrap();
        crate::claude::refresh_lock::touch(
            lock,
            std::time::SystemTime::now() - Duration::from_secs(120),
        )
        .unwrap();
    }
    assert_eq!(auto_model(&running, true).await.status(), 200);
    assert_eq!(
        crate::saved_login::access::<crate::saved_login::Claude>(&read_json(&file)),
        Some("new-access")
    );
    assert!(locks.iter().all(|lock| !lock.exists()));
}

#[tokio::test]
async fn auto_refresh_ignores_other_statuses_and_logins_without_refresh_tokens() {
    for claude in [false, true] {
        for status in ["status_403", "status_429", "status_500"] {
            let (_home, file, running, mut fixture) =
                auto_refresh_setup(claude, true, status).await;
            let original = std::fs::read(&file).unwrap();
            let response = auto_model(&running, claude).await;
            assert_eq!(
                response.status().as_u16().to_string(),
                status.trim_start_matches("status_")
            );
            assert_eq!(take_requests(&mut fixture).len(), 1);
            assert_eq!(std::fs::read(file).unwrap(), original);
        }
        let (_home, file, running, mut fixture) =
            auto_refresh_setup(claude, true, "auto_success").await;
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        if claude {
            value["claudeAiOauth"]
                .as_object_mut()
                .unwrap()
                .remove("refreshToken");
        } else {
            value["tokens"]
                .as_object_mut()
                .unwrap()
                .remove("refresh_token");
        }
        std::fs::write(&file, value.to_string()).unwrap();
        assert_eq!(auto_model(&running, claude).await.status(), 401);
        assert_eq!(take_requests(&mut fixture).len(), 1);
    }
}

#[tokio::test]
async fn claude_auto_rotation_registers_the_new_token_for_manual_renewal() {
    let mut lookup = fixture("http", "sse").await;
    let mut upstream = fixture("http", "auto_success").await;
    let lookup_url = format!("http://{}", lookup.addr);
    let account_url = format!("http://{}", upstream.addr);
    let home = tempfile::tempdir().unwrap();
    let file = home.path().join(".credentials.json");
    std::fs::write(
        &file,
        json!({"claudeAiOauth":{"accessToken":"access-secret","refreshToken":"refresh-secret"}})
            .to_string(),
    )
    .unwrap();
    let config = format!(
        "auto_refresh_access_token: true\nproxies:\n  selected: {account_url}\n  probe: {lookup_url}\nclaude:\n  account_auth_file_only: false\n  config_dirs: [{}]\n  routing:\n    account: {{remote@example.invalid: selected}}\n    account_probe: probe\n",
        serde_json::to_string(home.path()).unwrap()
    );
    let active = running(&config).await;
    trust(&active, &lookup, &lookup_url);
    trust(&active, &upstream, &account_url);
    assert_eq!(auto_model(&active, true).await.status(), 200);
    let requests = take_requests(&mut upstream);
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.starts_with("POST /v1/oauth/token "))
            .count(),
        1
    );
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
    let rotated = saved["claudeAiOauth"]["refreshToken"].as_str().unwrap();
    assert_eq!(rotated, "new-refresh");
    let cached = active.server.refresh_routes.identity(rotated);
    let renew = || {
        http()
            .post(format!("{}/anthropic/v1/oauth/token", active.url))
            .header("content-type", "application/json")
            .body(json!({"grant_type":"refresh_token","refresh_token":rotated}).to_string())
            .send()
    };
    let response = renew().await.unwrap();
    assert_eq!(cached.unwrap().usernames, ["remote@example.invalid"]);
    assert_eq!(response.status(), 200);
    response.bytes().await.unwrap();
    assert_eq!(
        take_requests(&mut upstream)
            .iter()
            .filter(|r| r.starts_with("POST /v1/oauth/token "))
            .count(),
        1
    );

    // An earlier daemon may already have saved a login with no cached route.
    let restarted = running(&config).await;
    take_requests(&mut lookup);
    trust(&restarted, &lookup, &lookup_url);
    trust(&restarted, &upstream, &account_url);
    assert!(restarted.server.refresh_routes.identity(rotated).is_none());
    let response = http()
        .post(format!("{}/anthropic/v1/oauth/token", restarted.url))
        .header("content-type", "application/json")
        .body(json!({"grant_type":"refresh_token","refresh_token":rotated}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    response.bytes().await.unwrap();
    let recovery_probe = take_requests(&mut lookup);
    assert_eq!(recovery_probe.len(), 1);
    assert!(recovery_probe[0].starts_with("GET /api/oauth/profile "));
    assert!(recovery_probe[0].contains("authorization: Bearer new-access"));
    assert_eq!(
        take_requests(&mut upstream)
            .iter()
            .filter(|r| r.starts_with("POST /v1/oauth/token "))
            .count(),
        1
    );
    // A fresh daemon receiving Copool's profile request first identifies the
    // bearer via the probe, then forwards the original GET via the account.
    let cold = running(&config).await;
    trust(&cold, &lookup, &lookup_url);
    trust(&cold, &upstream, &account_url);
    let response = http()
        .get(format!("{}/anthropic/api/oauth/profile", cold.url))
        .bearer_auth("new-access")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    for received in [take_requests(&mut lookup), take_requests(&mut upstream)] {
        assert_eq!(received.len(), 1);
        assert!(received[0].starts_with("GET /api/oauth/profile "));
        assert!(received[0].contains("authorization: Bearer new-access"));
    }
}

// As Antigravity's: a Claude model call does not hold the rotation lock
// while its profile is looked up.
#[tokio::test]
async fn a_claude_profile_lookup_holds_no_rotation_lock() {
    let mut lookup = fixture("http", "profile_hang").await;
    let endpoint = format!("http://127.0.0.1:{}", lookup.addr.port());
    let running = running(&format!(
        "auto_refresh_access_token: true\nproxies:\n  lookup: {endpoint}\nclaude:\n  account_auth_file_only: false\n  base_url: https://upstream.invalid\n  routing:\n    account:\n      remote@example.invalid: none\n    account_probe: lookup\n"
    ))
    .await;
    trust(&running, &lookup, &endpoint);
    let call = tokio::spawn(
        http()
            .post(format!("{}/anthropic/v1/messages", running.url))
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
            .starts_with("CONNECT upstream.invalid:443")
    );
    assert!(
        lookup
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("GET /api/oauth/profile ")
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
    assert!(!call.is_finished(), "the lookup is still in flight");
    drop(renewal);
    call.abort();
}
