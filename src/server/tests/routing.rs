use super::*;

#[tokio::test]
async fn codex_url_routes_stream_through_declared_transport_without_credential_lookup() {
    for mode in ["direct", "http"] {
        let mut fixture = fixture(mode, "sse").await;
        let endpoint = if mode == "direct" {
            "none".into()
        } else {
            format!("http://127.0.0.1:{}", fixture.addr.port())
        };
        let running = running(&format!("proxies:\n  selected: {endpoint}\ncodex:\n  routing:\n    api_key:\n      'upstream.invalid/v1': selected\n")).await;
        trust(&running, &fixture, &endpoint);
        let mut response = http()
            .post(format!(
                "{}/codex/https://upstream.invalid/v1/responses?stream=true",
                running.url
            ))
            .bearer_auth("url-route-secret")
            .header("cookie", "private-cookie")
            .body("model-body")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        if mode != "direct" {
            let connect = fixture.requests.recv().await.unwrap();
            assert!(connect.starts_with("CONNECT upstream.invalid:443"));
            assert!(!connect.contains("url-route-secret"));
        }
        let request = fixture.requests.recv().await.unwrap();
        assert!(request.starts_with("POST /v1/responses?stream=true HTTP/1.1"));
        assert!(request.contains("authorization: Bearer url-route-secret"));
        assert!(!request.contains("private-cookie"));
        assert!(request.ends_with("model-body"));
        assert_eq!(response.chunk().await.unwrap().unwrap(), "data: first\n\n");
        fixture.release.notify_one();
        assert_eq!(response.text().await.unwrap(), "data: last\n\n");
        assert!(fixture.requests.try_recv().is_err());
        running.server.logger.flush().unwrap();
        let log = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
        assert!(!log.contains("url-route-secret"));
        assert!(
            log.lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .any(|row| row["upstream_base_url"] == "https://upstream.invalid/v1")
        );
    }
}

#[tokio::test]
async fn application_namespaces_disambiguate_shared_api_upstreams() {
    let mut codex = fixture("http", "redirect").await;
    let mut claude = fixture("http", "redirect").await;
    let codex_endpoint = format!("http://127.0.0.1:{}", codex.addr.port());
    let claude_endpoint = format!("http://127.0.0.1:{}", claude.addr.port());
    let running = running(&format!("proxies:\n  codex: {codex_endpoint}\n  claude: {claude_endpoint}\ncodex:\n  routing:\n    api_key:\n      'https://upstream.invalid/v1': codex\nclaude:\n  routing:\n    api_key:\n      'https://upstream.invalid/v1': claude\n")).await;
    trust(&running, &codex, &codex_endpoint);
    trust(&running, &claude, &claude_endpoint);
    let response = http()
        .get(format!(
            "{}/https://upstream.invalid/v1/models",
            running.url
        ))
        .bearer_auth("key")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    assert!(codex.requests.try_recv().is_err());
    assert!(claude.requests.try_recv().is_err());
    let response = http()
        .post(format!(
            "{}/codex/https://upstream.invalid/v1/responses",
            running.url
        ))
        .bearer_auth("key")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 302);
    assert!(codex.requests.recv().await.unwrap().starts_with("CONNECT "));
    assert!(
        codex
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("POST /v1/responses ")
    );
    assert!(claude.requests.try_recv().is_err());
    let response = http()
        .post(format!(
            "{}/anthropic/https://upstream.invalid/v1/messages",
            running.url
        ))
        .bearer_auth("key")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 302);
    assert!(
        claude
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("CONNECT ")
    );
    assert!(
        claude
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("POST /v1/messages ")
    );
    assert!(codex.requests.try_recv().is_err());
}

// Synthetic upstream.invalid traffic stays entirely on loopback. No Anthropic
// host, real credential, or external proxy is contacted by this fixture.
#[tokio::test]
async fn claude_native_auth_paths_and_sse_passthrough() {
    for bearer in [false, true] {
        let mut fixture = fixture("http", "sse").await;
        let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
        let credentials = tempfile::tempdir().unwrap();
        let file = credentials.path().join(".credentials.json");
        std::fs::write(&file, r#"{"claudeAiOauth":{"accessToken":"model-secret"}}"#).unwrap();
        let dir = serde_json::to_string(&credentials.path()).unwrap();
        let running = running(&format!("proxies:\n  selected: {endpoint}\nclaude:\n  config_dirs: [{dir}]\n  base_url: https://upstream.invalid\n  routing:\n    account_fallback: selected\n    fallback: selected\n")).await;
        trust(&running, &fixture, &endpoint);
        let name = if bearer { "authorization" } else { "x-api-key" };
        let value = if bearer {
            "Bearer model-secret"
        } else {
            "model-secret"
        };
        let mut response = http()
            .post(format!("{}/anthropic/v1/messages?beta=true", running.url))
            .header(name, value)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "custom-beta")
            .header("cookie", "private-cookie")
            .header("chatgpt-account-id", "private-account")
            .body(r#"{"model":"test-model","stream":true}"#)
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
                .starts_with("CONNECT upstream.invalid:443")
        );
        let request = fixture.requests.recv().await.unwrap();
        assert!(request.starts_with("POST /v1/messages?beta=true HTTP/1.1"));
        assert!(request.contains(&format!("{name}: {value}\r\n")));
        assert!(!request.contains(if bearer {
            "x-api-key:"
        } else {
            "authorization:"
        }));
        assert!(request.contains(if bearer {
            "anthropic-beta: custom-beta,oauth-2025-04-20"
        } else {
            "anthropic-beta: custom-beta\r\n"
        }));
        assert!(!request.contains("private-cookie"));
        assert!(!request.contains("private-account"));
        assert!(request.ends_with(r#"{"model":"test-model","stream":true}"#));
        assert_eq!(response.chunk().await.unwrap().unwrap(), "data: first\n\n");
        fixture.release.notify_one();
        assert_eq!(response.text().await.unwrap(), "data: last\n\n");
        running.server.logger.flush().unwrap();
        let log = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
        assert!(log.contains("claude"));
        assert!(!log.contains("model-secret"));
    }
}

#[tokio::test]
async fn claude_third_party_explicit_url_streams_directly_without_oauth_lookup_or_fallback() {
    for bearer in [false, true] {
        let mut fixture = fixture("direct", "sse").await;
        let running = running("claude:\n  account_auth_file_only: true\n  routing:\n    api_key:\n      'upstream.invalid': none\n").await;
        trust(&running, &fixture, "none");
        let mut response = http()
            .post(format!(
                "{}/https://upstream.invalid/v1/messages?beta=true",
                running.url
            ))
            .header(
                if bearer { "authorization" } else { "x-api-key" },
                if bearer {
                    "Bearer api-secret"
                } else {
                    "api-secret"
                },
            )
            .body("model-body")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let request = fixture.requests.recv().await.unwrap();
        assert!(request.starts_with("POST /v1/messages?beta=true HTTP/1.1"));
        assert!(request.ends_with("model-body"));
        assert!(!request.contains("oauth-2025-04-20"));
        assert_eq!(response.chunk().await.unwrap().unwrap(), "data: first\n\n");
        fixture.release.notify_one();
        assert_eq!(response.text().await.unwrap(), "data: last\n\n");
        assert!(fixture.requests.try_recv().is_err());
        assert!((running.server.claude_identities.len() == 0));
    }
    let mut fixture = fixture("direct", "redirect").await;
    let running =
        running("claude:\n  routing:\n    api_key:\n      'upstream.invalid': none\n").await;
    trust(&running, &fixture, "none");
    let response = http()
        .get(format!(
            "{}/https://upstream.invalid/v1/models",
            running.url
        ))
        .bearer_auth("api-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 302);
    assert!(
        fixture
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("GET /v1/models HTTP/1.1")
    );
}

#[tokio::test]
async fn claude_shared_url_selector_keeps_oauth_on_the_account_route() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join(".credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"test-account"}}"#,
    )
    .unwrap();
    let mut account = fixture("http", "oauth_json").await;
    let mut keys = fixture("http", "oauth_json").await;
    let account_endpoint = format!("http://{}", account.addr);
    let key_endpoint = format!("http://{}", keys.addr);
    let running = running(&format!(
        "proxies:\n  account: {account_endpoint}\n  keys: {key_endpoint}\nclaude:\n  config_dirs: [{}]\n  base_url: https://upstream.invalid\n  routing:\n    account: {{default: account}}\n    fallback: keys\n    api_key: {{upstream.invalid: keys}}",
        serde_json::to_string(home.path()).unwrap()
    )).await;
    trust(&running, &account, &account_endpoint);
    trust(&running, &keys, &key_endpoint);
    let client = http();
    let profile = format!(
        "{}/anthropic/https://upstream.invalid/api/oauth/profile",
        running.url
    );
    let denied = client
        .get(&profile)
        .header("x-api-key", "test-key")
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 403);
    assert!(keys.requests.try_recv().is_err() && account.requests.try_recv().is_err());
    let allowed = client
        .get(profile)
        .bearer_auth("test-account")
        .send()
        .await
        .unwrap();
    assert_eq!(allowed.status(), 200);
    assert!(
        account
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("CONNECT ")
    );
    assert!(
        account
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("GET /api/oauth/profile ")
    );
    assert!(keys.requests.try_recv().is_err());
}

#[tokio::test]
async fn codex_provider_edits_cannot_send_keys_to_account_paths_or_fallback() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("config.toml");
    let write_provider = |base: &str| {
        std::fs::write(&path, format!("cli_auth_credentials_store = 'file'\n[model_providers.relay]\nbase_url = '{base}'\nexperimental_bearer_token = 'test-provider-key'\n")).unwrap();
    };
    write_provider("https://upstream.invalid/v1");
    let config = Config::parse(&format!(
        "listen_port: 8787\nrequest_timeout_seconds: 3\ncodex:\n  homes: [{}]\n  routing:\n    api_key: {{relay: none}}\n    fallback: none",
        serde_json::to_string(home.path()).unwrap()
    )).unwrap();
    let auth = Some("Bearer test-provider-key");
    assert_eq!(
        config.resolve(auth, true).await.unwrap().upstream,
        "https://upstream.invalid/v1"
    );
    write_provider("http://127.0.0.1:8787/https://chatgpt.com/backend-api");
    assert!(
        config
            .resolve(auth, true)
            .await
            .is_err_and(|e| e.message.contains("overlaps"))
    );
}

#[tokio::test]
async fn claude_usage_requires_saved_account_and_cannot_use_openai_fallback() {
    // A readable saved login that differs from the request token, independent
    // of the machine's own ~/.claude.
    let login = tempfile::tempdir().unwrap();
    std::fs::write(
        login.path().join(".credentials.json"),
        r#"{"claudeAiOauth": {"accessToken": "saved-secret"}}"#,
    )
    .unwrap();
    let running = running(&format!(
        "claude:\n  config_dirs: [{:?}]\n  routing:\n    account_fallback: none\n    fallback: none\ncodex:\n  routing:\n    fallback: none\n",
        login.path().to_string_lossy()
    ))
    .await;
    for path in [
        "/anthropic/api/oauth/usage",
        "/anthropic/api/oauth/%75sage",
        "/anthropic/api/oauth/usage/",
        "/api/oauth/usage",
        "/https://api.anthropic.com/api/oauth/usage",
    ] {
        let response = http()
            .get(format!("{}{path}", running.url))
            .bearer_auth("unmatched-secret")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
    }
    let running = self::running("codex:\n  routing:\n    fallback: none\n").await;
    let response = http()
        .post(format!("{}/v1/messages", running.url))
        .bearer_auth("openai-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn claude_named_settings_forward_to_file_upstream_through_selected_proxy() {
    for (base_path, target) in [
        ("/custom", "/anthropic/v1/messages"),
        (
            "/custom",
            "/anthropic/https://upstream.invalid/custom/v1/messages",
        ),
        (
            "/custom",
            "/claude/https://upstream.invalid/custom/v1/messages",
        ),
        ("", "/https://upstream.invalid/v1/messages"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("api.json"), serde_json::to_string(&serde_json::json!({"env": {
        "ANTHROPIC_BASE_URL": format!("http://127.0.0.1:8787/https://upstream.invalid{base_path}"),
        "ANTHROPIC_AUTH_TOKEN": "profile-secret"
    }})).unwrap()).unwrap();
        let mut fixture = fixture("http", "sse").await;
        let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
        let running = running(&format!("proxies:\n  selected: {endpoint}\nclaude:\n  config_dirs: [{}]\n  routing:\n    api_key:\n      api: selected\n", serde_json::to_string(dir.path()).unwrap())).await;
        trust(&running, &fixture, &endpoint);
        let mut response = http()
            .post(format!("{}{target}", running.url))
            .bearer_auth("profile-secret")
            .body("model-body")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{target}");
        let connect = fixture.requests.recv().await.unwrap();
        assert!(connect.starts_with("CONNECT upstream.invalid:443"));
        assert!(!connect.contains("profile-secret"));
        let request = fixture.requests.recv().await.unwrap();
        assert!(request.starts_with(&format!("POST {base_path}/v1/messages HTTP/1.1")));
        assert!(request.contains("authorization: Bearer profile-secret"));
        assert!(!request.contains("oauth-2025-04-20"));
        assert!(request.ends_with("model-body"));
        assert_eq!(response.chunk().await.unwrap().unwrap(), "data: first\n\n");
        fixture.release.notify_one();
        assert_eq!(response.chunk().await.unwrap().unwrap(), "data: last\n\n");
    }
}

#[tokio::test]
async fn claude_explicit_target_is_checked_before_forwarding_or_profile_lookup() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("api.json"), r#"{"env":{"ANTHROPIC_BASE_URL":"https://upstream.invalid/custom","ANTHROPIC_AUTH_TOKEN":"profile-secret"}}"#).unwrap();
    let mut fixture = fixture("http", "sse").await;
    let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
    let running = running(&format!("proxies:\n  selected: {endpoint}\nclaude:\n  config_dirs: [{}]\n  account_auth_file_only: false\n  routing:\n    account_probe: selected\n    api_key:\n      api: selected\n", serde_json::to_string(dir.path()).unwrap())).await;
    trust(&running, &fixture, &endpoint);
    for token in ["profile-secret", "unknown-account-secret"] {
        for target in [
            "/https://other.invalid/custom/v1/messages",
            "/https://upstream.invalid:444/custom/v1/messages",
            "/https://upstream.invalid/custom-evil/v1/messages",
            "/https://upstream.invalid/v1/messages",
        ] {
            let response = http()
                .post(format!("{}/anthropic{target}", running.url))
                .bearer_auth(token)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 502, "{target}");
            assert!(response.text().await.unwrap().contains(
                "Explicit upstream must match the credential's configured HTTPS upstream and API base."
            ));
            assert!(fixture.requests.try_recv().is_err());
        }
    }
}

// Named providers/settings must behave identically through a base URL and an
// HTTPS proxy. All outbound connections terminate at the local fixture.
#[tokio::test]
async fn named_upstreams_route_models_and_generation_through_both_entries() {
    for claude in [false, true] {
        for host in ["upstream.invalid", "182.92.106.196:6060"] {
            let mut fixture = fixture("http", "oauth_json").await;
            let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
            let home = tempfile::tempdir().unwrap();
            let home_yaml = serde_json::to_string(home.path()).unwrap();
            let service = if claude { "anthropic" } else { "codex" };
            let config = if claude {
                std::fs::write(
                    home.path().join("custom.settings.json"),
                    json!({"env": {
                        "ANTHROPIC_BASE_URL":format!("https://{host}/v1"),
                        "ANTHROPIC_AUTH_TOKEN":"fake-provider-secret"
                    }})
                    .to_string(),
                )
                .unwrap();
                format!(
                    "claude:\n  config_dirs: [{home_yaml}]\n  routing:\n    api_key: {{custom.settings.json: selected}}\n"
                )
            } else {
                std::fs::write(home.path().join("config.toml"), format!(
                    "[model_providers.ShareCoder]\nbase_url = 'https://{host}/v1'\nexperimental_bearer_token = 'fake-provider-secret'\n"
                )).unwrap();
                format!(
                    "codex:\n  homes: [{home_yaml}]\n  routing:\n    api_key: {{ShareCoder: selected}}\n"
                )
            };
            let running = running(&format!(
                "connect_credential_routing: true\nproxies:\n  selected: {endpoint}\n{config}"
            ))
            .await;
            trust(&running, &fixture, &endpoint);
            let proxy = proxy_port_client(&running);
            for entry in ["base", "explicit", "proxy"] {
                let tunneled = entry == "proxy";
                for path in ["models", if claude { "messages" } else { "responses" }] {
                    let url = if entry == "base" {
                        if claude {
                            format!("{}/anthropic/{path}", running.url)
                        } else {
                            format!("{}/v1/{path}", running.url)
                        }
                    } else if tunneled {
                        format!("https://{host}/v1/{path}")
                    } else {
                        format!("{}/{service}/https://{host}/v1/{path}", running.url)
                    };
                    let client = if tunneled { &proxy } else { &http() };
                    let request = if path == "models" {
                        client.get(&url)
                    } else {
                        client.post(&url).body("{}")
                    };
                    let response = request
                        .bearer_auth("fake-provider-secret")
                        .send()
                        .await
                        .unwrap();
                    assert_eq!(
                        response.status(),
                        200,
                        "{url}: {}",
                        response.text().await.unwrap()
                    );
                    let connect = fixture.requests.recv().await.unwrap();
                    let authority = if host.contains(':') {
                        host.to_owned()
                    } else {
                        format!("{host}:443")
                    };
                    assert!(
                        connect.starts_with(&format!("CONNECT {authority} ")),
                        "{connect}"
                    );
                    let request = fixture.requests.recv().await.unwrap();
                    assert!(request.contains(&format!(" /v1/{path} ")), "{request}");
                    assert!(request.contains("authorization: Bearer fake-provider-secret"));
                }
            }
            // Account-only usage is not opened to provider credentials.
            let usage = if claude {
                "/v1/api/oauth/usage"
            } else {
                "/backend-api/wham/usage"
            };
            let response = proxy
                .get(format!("https://{host}{usage}"))
                .bearer_auth("fake-provider-secret")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 403);
            assert!(fixture.requests.try_recv().is_err());
            // A discovered destination still requires a matching credential.
            let response = proxy
                .get(format!("https://{host}/v1/models"))
                .bearer_auth("wrong-secret")
                .send()
                .await
                .unwrap();
            assert!(response.status().is_client_error());
            assert!(fixture.requests.try_recv().is_err());
            // Every record of every request names its app, from the first one on.
            let rows = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    running.server.logger.flush().unwrap();
                    let raw =
                        std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
                    let rows: Vec<serde_json::Value> = raw
                        .lines()
                        .filter_map(|l| serde_json::from_str(l).ok())
                        .filter(|row: &serde_json::Value| row["method"].is_string())
                        .collect();
                    let count = |end: bool| {
                        rows.iter()
                            .filter(|row| {
                                row["event"].as_str().is_some_and(|event| {
                                    (event == "request_received") != end
                                        && event.starts_with("request_")
                                })
                            })
                            .count()
                    };
                    if count(false) == count(true) {
                        return rows;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert!(rows.iter().any(|row| row["method"] == "CONNECT"));
            assert!(
                rows.iter()
                    .any(|row| row["event"] == "request_received" && row["method"] != "CONNECT")
            );
            assert!(rows.iter().any(|row| row["method"] == "CONNECT"
                && row["event"] == "request_finished"
                && row["service"] == if claude { "claude" } else { "codex" }));
            for row in &rows {
                if row["method"] == "CONNECT" {
                    // Named only by a configured first request in its tunnel.
                    assert!(
                        row.get("service").is_none()
                            || row["service"] == if claude { "claude" } else { "codex" },
                        "{row}"
                    );
                } else {
                    assert_eq!(
                        row["service"],
                        if claude { "claude" } else { "codex" },
                        "{row}"
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn named_provider_websocket_works_through_base_url_and_connect() {
    let mut fixture = fixture("http", "websocket").await;
    let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("config.toml"),
        "[model_providers.ShareCoder]\nbase_url = 'https://upstream.invalid/v1'\nexperimental_bearer_token = 'ws-secret'\n"
    ).unwrap();
    let running = running(&format!("connect_credential_routing: true\nproxies:\n  selected: {endpoint}\ncodex:\n  homes: [{}]\n  routing:\n    api_key: {{ShareCoder: selected}}\n", serde_json::to_string(home.path()).unwrap())).await;
    trust(&running, &fixture, &endpoint);
    let proxy = proxy_port_client(&running);
    for tunneled in [false, true] {
        let url = if tunneled {
            "https://upstream.invalid/v1/socket".into()
        } else {
            format!("{}/codex/https://upstream.invalid/v1/socket", running.url)
        };
        let direct = http();
        let client = if tunneled { &proxy } else { &direct };
        let response = client
            .get(url)
            .bearer_auth("ws-secret")
            .header("connection", "Upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .header("sec-websocket-protocol", "test")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 101);
        let mut socket = response.upgrade().await.unwrap();
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
        assert!(request.starts_with("GET /v1/socket HTTP/1.1"));
        assert!(request.contains("authorization: Bearer ws-secret"));
    }
}

// One public IP named by both a Codex provider and a Claude settings file stays
// unprefixed, so each request resolves the same app through either entry.
#[tokio::test]
async fn a_named_host_shared_by_both_apps_routes_like_its_base_url() {
    let mut fixture = fixture("http", "oauth_json").await;
    let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
    let home = tempfile::tempdir().unwrap();
    let host = "182.92.106.196:6060";
    std::fs::write(
        home.path().join("config.toml"),
        format!("[model_providers.myServer]\nbase_url = 'https://127.0.0.1:8787/https://{host}'\nexperimental_bearer_token = 'codex-secret'\n"),
    )
    .unwrap();
    std::fs::create_dir_all(home.path().join("profiles")).unwrap();
    std::fs::write(
        home.path().join("profiles").join("myServer.json"),
        json!({"env": {
            "ANTHROPIC_BASE_URL": format!("http://127.0.0.1:8787/https://{host}"),
            "ANTHROPIC_AUTH_TOKEN": "claude-secret"
        }})
        .to_string(),
    )
    .unwrap();
    let dir = serde_json::to_string(home.path()).unwrap();
    let running = running(&format!(
        "connect_credential_routing: true\nproxies:\n  selected: {endpoint}\ncodex:\n  homes: [{dir}]\n  routing:\n    api_key: {{myServer: selected}}\nclaude:\n  config_dirs: [{dir}]\n  routing:\n    api_key: {{myServer: selected}}\n"
    ))
    .await;
    trust(&running, &fixture, &endpoint);
    let proxy = proxy_port_client(&running);
    let base = http();
    for tunneled in [false, true] {
        let (client, origin) = if tunneled {
            (&proxy, format!("https://{host}"))
        } else {
            (&base, format!("{}/https://{host}", running.url))
        };
        for (path, secret) in [
            ("/responses", "codex-secret"),
            ("/v1/messages", "claude-secret"),
        ] {
            let response = client
                .post(format!("{origin}{path}"))
                .bearer_auth(secret)
                .body("{}")
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                200,
                "{origin}{path}: {}",
                response.text().await.unwrap()
            );
            let connect = fixture.requests.recv().await.unwrap();
            assert!(
                connect.starts_with(&format!("CONNECT {host} ")),
                "{connect}"
            );
            let request = fixture.requests.recv().await.unwrap();
            assert!(request.starts_with(&format!("POST {path} ")), "{request}");
            assert!(request.contains(secret), "{request}");
        }
        // Each app accepts only its own credential, through either entry.
        for (path, secret) in [
            ("/responses", "claude-secret"),
            ("/v1/messages", "codex-secret"),
        ] {
            let response = client
                .post(format!("{origin}{path}"))
                .bearer_auth(secret)
                .body("{}")
                .send()
                .await
                .unwrap();
            assert!(response.status().is_client_error(), "{origin}{path}");
            assert!(fixture.requests.try_recv().is_err());
        }
    }
}
