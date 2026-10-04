mod common;
use common::{App, Probe};
use serde_json::json;
use std::{fs, thread, time::Duration};

fn login(app: &App, account: &str, token: &str) {
    app.write(
        "home/auth.json",
        json!({"tokens":{"account_id":account,"access_token":token}}).to_string(),
    );
}
fn account_config(app: &App, a: u16, b: u16) {
    app.config(&format!("proxies:\n  us: http://127.0.0.1:{a}\n  jp: http://127.0.0.1:{b}\ncodex:\n  homes: [{}]\n  base_url:\n    account: https://upstream.invalid/backend-api\n  routing:\n    account:\n      account-a: us\n      account-b: jp\n", json!(app.path("home"))));
}
fn total(a: &Probe, b: &Probe) -> usize {
    a.count() + b.count()
}
fn refused(app: &App, a: &Probe, b: &Probe, token: Option<&str>, status: u16) {
    let before = total(a, b);
    assert_eq!(app.status(token), status);
    assert_eq!(total(a, b), before);
}

#[test]
fn account_routing_refresh_startup_snapshot_and_private_logs() {
    let a = Probe::new();
    let b = Probe::new();
    let mut app = App::new();
    login(&app, "account-a", "token-a");
    account_config(&app, a.port, b.port);
    assert!(app.command().arg("--check").status().unwrap().success());
    app.restart();
    refused(&app, &a, &b, Some("wrong"), 401);
    a.reaches(
        &app,
        Some("token-a"),
        None,
        "/responses?private_query=secret-query",
        "upstream.invalid",
    );
    assert_eq!(b.count(), 0);
    let previous_a = a.count();
    login(&app, "account-b", "token-b");
    assert_eq!(app.status(Some("token-a")), 401);
    assert_eq!(
        app.request(Some("token-b"), Some("account-a"), "/responses", "POST")
            .0,
        409
    );
    b.reaches(
        &app,
        Some("token-b"),
        Some("account-b"),
        "/responses",
        "upstream.invalid",
    );
    assert_eq!(a.count(), previous_a);
    login(&app, "unmapped", "token-c");
    refused(&app, &a, &b, Some("token-c"), 502);
    login(&app, "account-a", "token-a");
    account_config(&app, b.port, b.port);
    let previous_b = b.count();
    a.reaches(
        &app,
        Some("token-a"),
        None,
        "/responses",
        "upstream.invalid",
    );
    assert_eq!(b.count(), previous_b);
    app.restart();
    b.reaches(
        &app,
        Some("token-a"),
        None,
        "/responses",
        "upstream.invalid",
    );
    let dead_port = common::port();
    account_config(&app, dead_port, b.port);
    app.restart();
    refused(&app, &a, &b, Some("token-a"), 502);
    app.write("config.yaml", "invalid: [");
    refused(&app, &a, &b, Some("token-a"), 502);
    assert!(!app.command().status().unwrap().success());
    fs::remove_file(app.path("config.yaml")).unwrap();
    refused(&app, &a, &b, Some("token-a"), 502);
    assert_eq!(app.request(None, None, "/health", "GET").0, 200);
    let mut records = vec![];
    let mut terminal = vec![];
    for _ in 0..100 {
        records = app.records();
        terminal = records
            .iter()
            .filter(|r| {
                matches!(
                    r["event"].as_str(),
                    Some("request_finished" | "request_failed" | "request_rejected")
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        if terminal.len() == 11 {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(terminal.len(), 11);
    let current = records
        .iter()
        .find(|r| r["event"] == "current_route")
        .unwrap();
    assert_eq!(current["account_id"], "account-a");
    assert_eq!(current["proxy"], "us");
    let routes: Vec<_> = records
        .iter()
        .filter(|r| r["event"] == "route_selected")
        .map(|r| {
            (
                r["account_id"].as_str().unwrap(),
                r["proxy"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        routes,
        [
            ("account-a", "us"),
            ("account-b", "jp"),
            ("account-a", "us"),
            ("account-a", "us"),
            ("account-a", "us"),
            ("account-a", "us"),
            ("account-a", "us")
        ]
    );
    assert!(
        terminal
            .iter()
            .all(|r| r["request_id"].is_string() && !r["duration_ms"].is_null())
    );
    let statuses: std::collections::BTreeSet<_> = terminal
        .iter()
        .map(|r| r["status"].as_str().unwrap())
        .collect();
    assert_eq!(statuses, ["401", "409", "502"].into_iter().collect());
    for secret in [
        "token-a",
        "token-b",
        "token-c",
        "secret-query",
        "secret-body",
    ] {
        assert!(!app.log().contains(secret));
    }
    assert!(records.iter().all(|r| r["path"] != "/health"));
}

#[test]
fn api_keys_rotate_without_account_file() {
    let a = Probe::new();
    let b = Probe::new();
    let mut app = App::new();
    app.write("home/.env", "API_PROVIDER_KEY=provider-key-one\n");
    app.config(&format!("proxies:\n  chosen: http://127.0.0.1:{}\n  unused: http://127.0.0.1:{}\ncodex:\n  homes: [{}]\n  base_url:\n    api_key: https://api-provider.invalid/v1\n  routing:\n    api_key:\n      API_PROVIDER_KEY: chosen\n",a.port,b.port,json!(app.path("home"))));
    app.restart();
    assert!(app.command().arg("--check").status().unwrap().success());
    refused(&app, &a, &b, Some("wrong"), 401);
    a.reaches(
        &app,
        Some("provider-key-one"),
        Some("irrelevant"),
        "/v1/responses",
        "api-provider.invalid",
    );
    assert_eq!(b.count(), 0);
    app.write("home/.env", "API_PROVIDER_KEY=provider-key-two\n");
    refused(&app, &a, &b, Some("provider-key-one"), 401);
    a.reaches(
        &app,
        Some("provider-key-two"),
        None,
        "/responses",
        "api-provider.invalid",
    );
    fs::remove_file(app.path("home/.env")).unwrap();
    refused(&app, &a, &b, Some("provider-key-two"), 502);
    for secret in ["provider-key-one", "provider-key-two"] {
        assert!(!app.log().contains(secret));
    }
}

fn mixed(app: &App, a: &Probe, b: &Probe, base: Option<&str>, fallbacks: &str) {
    let base = base
        .map(|s| format!("    api_key: {s}\n"))
        .unwrap_or_default();
    app.config(&format!("proxies:\n  us: http://127.0.0.1:{}\n  jp: http://127.0.0.1:{}\ncodex:\n  homes: [{}]\n  base_url:\n    account: https://chatgpt-mixed.invalid/backend-api\n{base}  routing:\n    account:\n      account-a: us\n    api_key:\n      REVERSE_TEST_KEY: us\n      provider-b: jp\n      EXTRA_KEY_A: us\n      EXTRA_KEY_B: jp\n{fallbacks}",a.port,b.port,json!(app.path("home"))));
}
#[test]
fn mixed_providers_mcp_account_endpoints_and_independent_fallbacks() {
    let a = Probe::new();
    let b = Probe::new();
    let mut app = App::new();
    login(&app, "account-a", "chat-mixed-token");
    for (k, v) in [
        ("REVERSE_TEST_KEY", "provider-key-one"),
        ("EXTRA_KEY_A", "extra-key-a"),
        ("EXTRA_KEY_B", "extra-key-b"),
    ] {
        app.env.insert(k.into(), v.into());
    }
    app.write("ignored/config.toml", "malformed = [");
    app.write("ignored/.credentials.json", "invalid JSON");
    for k in ["CODEX_HOME", "CLAUDE_CONFIG_DIR"] {
        app.env
            .insert(k.into(), app.path("ignored").to_string_lossy().into_owned());
    }
    let providers = [
        ("reverse", "REVERSE_TEST_KEY", "provider-a.invalid"),
        ("provider-b", "PROVIDER_B_KEY", "provider-b.invalid"),
        ("extra-a", "EXTRA_KEY_A", "provider-a.invalid"),
        ("extra-b", "EXTRA_KEY_B", "provider-a.invalid"),
    ];
    app.write("home/config.toml",providers.iter().map(|(name,key,host)|format!("[model_providers.{name}]\nenv_key = \"{key}\"\nbase_url = \"https://{host}/v1\"\n")).collect::<String>());
    app.write("home/.env", "PROVIDER_B_KEY=provider-key-two\n");
    mixed(&app, &a, &b, None, "");
    app.restart();
    for (token, probe, other, host) in [
        ("chat-mixed-token", &a, &b, "chatgpt-mixed.invalid"),
        ("provider-key-one", &a, &b, "provider-a.invalid"),
        ("provider-key-two", &b, &a, "provider-b.invalid"),
        ("extra-key-a", &a, &b, "provider-a.invalid"),
        ("extra-key-b", &b, &a, "provider-a.invalid"),
    ] {
        let before = other.count();
        probe.reaches(&app, Some(token), None, "/v1/responses", host);
        assert_eq!(other.count(), before);
        probe.reaches(
            &app,
            Some(token),
            None,
            "/mcp/openaiDeveloperDocs",
            "developers.openai.com",
        );
        assert_eq!(other.count(), before);
    }
    mixed(&app, &a, &b, None, "    mcp_fallback: jp\n");
    app.restart();
    for credential in [None, Some("unknown")] {
        let before = a.count();
        b.reaches(
            &app,
            credential,
            None,
            "/mcp/openaiDeveloperDocs",
            "developers.openai.com",
        );
        assert_eq!(a.count(), before);
    }
    mixed(&app, &a, &b, None, "    mcp_fallback: us\n");
    app.restart();
    let before = b.count();
    a.reaches(
        &app,
        None,
        None,
        "/mcp/openaiDeveloperDocs",
        "developers.openai.com",
    );
    assert_eq!(b.count(), before);
    for path in [
        "/backend-api/wham/usage",
        "/backend-api/wham/profiles/me",
        "/backend-api/wham/rate-limit-reset-credits",
    ] {
        let before = a.count();
        let other = b.count();
        assert_eq!(
            app.request(Some("chat-mixed-token"), None, path, "GET").0,
            502
        );
        assert!(a.count() > before);
        assert_eq!(b.count(), other);
        assert!(a.last().starts_with(b"CONNECT chatgpt-mixed.invalid:443 "));
        let before = total(&a, &b);
        assert_eq!(
            app.request(Some("provider-key-one"), None, path, "GET").0,
            403
        );
        assert_eq!(total(&a, &b), before);
    }
    refused(&app, &a, &b, Some("unknown"), 401);
    app.write("home/.env", "PROVIDER_B_KEY=provider-key-one\n");
    refused(&app, &a, &b, Some("provider-key-one"), 409);
    app.write("home/.env", "PROVIDER_B_KEY=chat-mixed-token\n");
    refused(&app, &a, &b, Some("chat-mixed-token"), 409);
    mixed(&app, &a, &b, None, "    api_key_fallback: jp\n");
    app.restart();
    refused(&app, &a, &b, Some("chat-mixed-token"), 409);
    let before = a.count();
    b.reaches(
        &app,
        Some("unmatched-openai-key"),
        None,
        "/responses",
        "api.openai.com",
    );
    assert_eq!(a.count(), before);
    fs::remove_file(app.path("home/auth.json")).unwrap();
    b.reaches(
        &app,
        Some("another-unmatched-token"),
        None,
        "/responses",
        "api.openai.com",
    );
    let before = total(&a, &b);
    assert_eq!(
        app.request(
            Some("unmatched-openai-key"),
            None,
            "/https://other.invalid/v1/responses",
            "POST"
        )
        .0,
        502
    );
    assert_eq!(total(&a, &b), before);
    mixed(
        &app,
        &a,
        &b,
        Some("https://fallback-default.invalid/v1"),
        "    api_key_fallback: jp\n",
    );
    app.restart();
    b.reaches(
        &app,
        Some("unmatched-openai-key"),
        None,
        "/responses",
        "fallback-default.invalid",
    );
    mixed(
        &app,
        &a,
        &b,
        Some("https://fallback-default.invalid/v1"),
        "    api_key_fallback: missing\n",
    );
    b.reaches(
        &app,
        Some("unmatched-openai-key"),
        None,
        "/responses",
        "fallback-default.invalid",
    );
    assert!(!app.command().status().unwrap().success());
    login(&app, "fallback-account", "fallback-account-token");
    app.write("home/config.toml","[model_providers.reverse]\nenv_key = \"REVERSE_TEST_KEY\"\nbase_url = \"https://provider-a.invalid/v1\"\n");
    fs::remove_file(app.path("home/.env")).unwrap();
    app.config(&format!("proxies:\n  us: http://127.0.0.1:{}\n  jp: http://127.0.0.1:{}\ncodex:\n  homes: [{}]\n  base_url:\n    account: https://chatgpt-mixed.invalid/backend-api\n    api_key: https://fallback-default.invalid/v1\n  routing:\n    api_key:\n      reverse: us\n      EXTRA_KEY_B: jp\n    account_fallback: us\n    api_key_fallback: jp\n    mcp_fallback: us\n",a.port,b.port,json!(app.path("home"))));
    app.restart();
    for (token, path, probe, host) in [
        (
            Some("fallback-account-token"),
            "/v1/responses",
            &a,
            "chatgpt-mixed.invalid",
        ),
        (
            Some("fallback-account-token"),
            "/backend-api/ps/plugins/installed",
            &a,
            "chatgpt-mixed.invalid",
        ),
        (
            Some("unknown-api-token"),
            "/v1/responses",
            &b,
            "fallback-default.invalid",
        ),
        (
            Some("provider-key-one"),
            "/v1/responses",
            &a,
            "provider-a.invalid",
        ),
        (
            Some("extra-key-b"),
            "/v1/responses",
            &b,
            "fallback-default.invalid",
        ),
        (
            None,
            "/mcp/openaiDeveloperDocs",
            &a,
            "developers.openai.com",
        ),
    ] {
        probe.reaches(&app, token, None, path, host);
    }
}

fn set_provider(app: &App, flag: Option<bool>, extra: &str) {
    let flag = flag
        .map(|v| format!("requires_openai_auth = {v}\n"))
        .unwrap_or_default();
    app.write("codex/config.toml",format!("[model_providers.custom]\nbase_url = \"https://custom-provider.invalid/v1\"\n{flag}{extra}"));
}
#[test]
fn provider_auth_precedence_dotenv_and_live_rotation() {
    let a = Probe::new();
    let b = Probe::new();
    let mut app = App::new();
    app.env
        .insert("REVERSE_TEST_KEY".into(), "provider-key-one".into());
    app.config(&format!("proxies:\n  chosen: http://127.0.0.1:{}\ncodex:\n  homes: [{}]\n  routing:\n    api_key:\n      custom: chosen\n",a.port,json!(app.path("codex"))));
    app.write(
        "codex/auth.json",
        json!({"OPENAI_API_KEY":"saved-provider-key"}).to_string(),
    );
    set_provider(&app, Some(true), "");
    app.restart();
    assert!(app.command().arg("--check").status().unwrap().success());
    let reaches = |app: &App, token: &str, account: Option<&str>| {
        let other = b.count();
        a.reaches(
            app,
            Some(token),
            account,
            "/responses",
            "custom-provider.invalid",
        );
        assert_eq!(b.count(), other);
    };
    reaches(&app, "saved-provider-key", None);
    refused(&app, &a, &b, Some("wrong"), 401);
    for flag in [Some(true), Some(false), None] {
        set_provider(
            &app,
            flag,
            "env_key = \"REVERSE_TEST_KEY\"\nexperimental_bearer_token = \"explicit-provider-key\"\n",
        );
        reaches(&app, "provider-key-one", None);
        refused(&app, &a, &b, Some("explicit-provider-key"), 401);
        refused(&app, &a, &b, Some("saved-provider-key"), 401);
        set_provider(
            &app,
            flag,
            "experimental_bearer_token = \"explicit-provider-key\"\n",
        );
        reaches(&app, "explicit-provider-key", None);
        refused(&app, &a, &b, Some("saved-provider-key"), 401);
        set_provider(&app, flag, "");
        if flag == Some(true) {
            reaches(&app, "saved-provider-key", None);
        } else {
            refused(&app, &a, &b, Some("saved-provider-key"), 502);
            refused(&app, &a, &b, None, 401);
        }
    }
    set_provider(
        &app,
        Some(true),
        "env_key = \"REVERSE_TEST_KEY\"\nexperimental_bearer_token = \"explicit-provider-key\"\n",
    );
    app.write("codex/.env","BASE=dotenv\nnot valid\nREVERSE_TEST_KEY=first\nexport REVERSE_TEST_KEY=${BASE}-key # last wins\n");
    let pid = app.pid();
    assert!(app.command().arg("--check").status().unwrap().success());
    reaches(&app, "dotenv-key", None);
    refused(&app, &a, &b, Some("provider-key-one"), 401);
    app.write("codex/.env", "REVERSE_TEST_KEY=\"rotated-dotenv-key\"\n");
    reaches(&app, "rotated-dotenv-key", None);
    assert_eq!(app.pid(), pid);
    refused(&app, &a, &b, Some("dotenv-key"), 401);
    app.env.insert("REVERSE_TEST_KEY".into(), "".into());
    app.restart();
    reaches(&app, "rotated-dotenv-key", None);
    app.env
        .insert("REVERSE_TEST_KEY".into(), "provider-key-one".into());
    app.write("codex/.env", "REVERSE_TEST_KEY=\n");
    for token in [
        "saved-provider-key",
        "explicit-provider-key",
        "rotated-dotenv-key",
        "provider-key-one",
    ] {
        refused(&app, &a, &b, Some(token), 502);
    }
    set_provider(&app, Some(true), "env_key = \"CODEX_FILTERED\"\n");
    app.env
        .insert("CODEX_FILTERED".into(), "process-protected-key".into());
    app.write(
        "codex/.env",
        "CODEX_FILTERED=file-protected-key\nCoDeX_IGNORED=ignored\n",
    );
    app.restart();
    reaches(&app, "process-protected-key", None);
    refused(&app, &a, &b, Some("file-protected-key"), 401);
    app.env.remove("CODEX_FILTERED");
    fs::remove_file(app.path("codex/.env")).unwrap();
    set_provider(&app, Some(true), "");
    app.write("codex/auth.json",json!({"auth_mode":"chatgpt","OPENAI_API_KEY":"stale-key","tokens":{"access_token":"saved-chat-token","account_id":"custom-account"}}).to_string());
    reaches(&app, "saved-chat-token", Some("custom-account"));
    refused(&app, &a, &b, Some("stale-key"), 401);
    let before = a.count();
    assert_eq!(
        app.request(
            Some("saved-chat-token"),
            Some("wrong-account"),
            "/responses",
            "POST"
        )
        .0,
        409
    );
    assert_eq!(a.count(), before);
    app.write(
        "codex/auth.json",
        json!({"OPENAI_API_KEY":"rotated-provider-key"}).to_string(),
    );
    reaches(&app, "rotated-provider-key", None);
    refused(&app, &a, &b, Some("saved-chat-token"), 401);
    fs::remove_file(app.path("codex/auth.json")).unwrap();
    refused(&app, &a, &b, Some("rotated-provider-key"), 502);
}
