use super::*;

/// Renewals are learned whatever the saved-login policy, which only decides
/// whether learned tokens are used: a policy turned off later finds the
/// accounts renewed while it was on.
#[tokio::test]
async fn grok_renewals_are_learned_while_the_saved_login_policy_is_on() {
    let mut mapped = fixture("http", "grok").await;
    let mapped_url = format!("http://{}", mapped.addr);
    let state = tempfile::tempdir().unwrap();
    let routes = state.path().join(crate::refresh_routes::GROK_FILE);
    let auth = state.path().join("auth.json");
    let login = json!({"user_id":"alice","team_id":"team","key":"saved-access","refresh_token":"saved-refresh"});
    std::fs::write(
        &auth,
        json!({format!("{}::{}", crate::grok::ISSUER, crate::grok::CLIENT_ID): login}).to_string(),
    )
    .unwrap();
    let config = |policy: &str| {
        format!(
            "connect_credential_routing: true\nproxies:\n  mapped: {mapped_url}\ngrok:\n  auth_file: {}\n{policy}  routing:\n    account: {{alice/team: mapped}}\n",
            serde_json::to_string(&auth).unwrap()
        )
    };
    let start = |policy: &'static str| {
        let config = config(policy);
        let routes = routes.clone();
        async move {
            running_with_server(
                &config,
                |_| {},
                |server| {
                    server.with_grok_routes(crate::refresh_routes::RefreshRoutes::load(routes))
                },
            )
            .await
        }
    };
    let billing = |client: &reqwest::Client| {
        client
            .get(format!("{}/v1/billing?format=credits", crate::grok::API))
            .bearer_auth("issued-access")
            .send()
    };
    // The default policy: the saved login renews and is learned.
    let strict = start("").await;
    trust(&strict, &mapped, &mapped_url);
    let client = proxy_port_client(&strict);
    let reply = client
        .post("https://auth.x.ai/oauth2/token")
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", crate::grok::CLIENT_ID),
            ("refresh_token", "saved-refresh"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), 200);
    reply.bytes().await.unwrap();
    for _ in 0..2 {
        mapped.requests.recv().await.unwrap();
    }
    let learned =
        crate::refresh_routes::RefreshRoutes::<crate::grok::GrokAccount>::load(routes.clone());
    assert_eq!(
        learned.identity("issued-refresh").unwrap().user_id(),
        "alice"
    );
    assert!(learned.access_identity("issued-access").is_some());
    // Until auth.json holds it, the policy refuses the renewed token.
    assert_eq!(billing(&client).await.unwrap().status(), 403);
    assert!(mapped.requests.try_recv().is_err());
    drop(strict);
    // Turned off, the renewal's account route takes it at once.
    let open = start("  account_auth_file_only: false\n").await;
    trust(&open, &mapped, &mapped_url);
    let reply = billing(&proxy_port_client(&open)).await.unwrap();
    assert_eq!(reply.status(), 200);
    assert!(
        mapped
            .requests
            .recv()
            .await
            .unwrap()
            .starts_with("CONNECT cli-chat-proxy.grok.com:443 ")
    );
}

/// Grok Build's `[auth.oauth2] issuer` pointing at `/grok/https://auth.x.ai`
/// sends its sign-in and renewal here: the discovery document names this
/// listener's token and device endpoints, a device sign-in takes the saved
/// login's route and is learned, and the login Grok Build files under that
/// issuer renews under the saved-login policy. Only the fixture proxy is
/// reached.
#[tokio::test]
async fn grok_issuer_scope_serves_discovery_device_sign_in_and_renewal() {
    let mut mapped = fixture("http", "grok").await;
    let mapped_url = format!("http://{}", mapped.addr);
    let state = tempfile::tempdir().unwrap();
    let routes = state.path().join(crate::refresh_routes::GROK_FILE);
    let auth = state.path().join("auth.json");
    let xai_key = format!("{}::{}", crate::grok::ISSUER, crate::grok::CLIENT_ID);
    let saved = json!({"user_id":"alice","team_id":"team","key":"old-access","refresh_token":"old-refresh"});
    std::fs::write(&auth, json!({&xai_key: saved}).to_string()).unwrap();
    let config = format!(
        "connect_credential_routing: true\nproxies:\n  mapped: {mapped_url}\ngrok:\n  auth_file: {}\n  routing:\n    account: {{alice/team: mapped}}\n",
        serde_json::to_string(&auth).unwrap()
    );
    let running = running_with_server(
        &config,
        |_| {},
        |server| {
            server.with_grok_routes(crate::refresh_routes::RefreshRoutes::load(routes.clone()))
        },
    )
    .await;
    trust(&running, &mapped, &mapped_url);
    let issuer = format!("{}{}", running.url, crate::grok::ISSUER_SCOPE);
    let reached = |mapped: &mut Fixture| {
        let connect = mapped.requests.try_recv().unwrap();
        assert!(connect.starts_with("CONNECT auth.x.ai:443 "), "{connect}");
        mapped.requests.try_recv().unwrap()
    };

    let reply = http()
        .get(format!("{issuer}/.well-known/openid-configuration"))
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), 200);
    let document: serde_json::Value =
        serde_json::from_slice(&reply.bytes().await.unwrap()).unwrap();
    assert_eq!(document["token_endpoint"], format!("{issuer}/oauth2/token"));
    assert_eq!(
        document["device_authorization_endpoint"],
        format!("{issuer}/oauth2/device/code")
    );
    assert_eq!(document["issuer"], "https://auth.x.ai");
    assert_eq!(
        document["authorization_endpoint"],
        "https://accounts.x.ai/oauth2/auth"
    );
    assert_eq!(
        document["jwks_uri"],
        "https://auth.x.ai/.well-known/jwks.json"
    );
    assert!(reached(&mut mapped).starts_with("GET /.well-known/openid-configuration "));

    let reply = http()
        .post(format!("{issuer}/oauth2/device/code"))
        .form(&[
            ("client_id", crate::grok::CLIENT_ID),
            ("scope", "openid offline_access"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), 200);
    assert!(reply.text().await.unwrap().contains("ABCD-EFGH"));
    assert!(reached(&mut mapped).starts_with("POST /oauth2/device/code "));

    let device = [
        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ("client_id", crate::grok::CLIENT_ID),
        ("device_code", "device-secret"),
    ];
    let reply = http()
        .post(format!("{issuer}/oauth2/token"))
        .form(&device)
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), 200);
    assert!(reply.text().await.unwrap().contains("issued-refresh"));
    let request = reached(&mut mapped);
    assert!(request.starts_with("POST /oauth2/token "));
    assert!(request.ends_with("device_code=device-secret"));
    let learned =
        crate::refresh_routes::RefreshRoutes::<crate::grok::GrokAccount>::load(routes.clone());
    assert_eq!(
        learned.identity("issued-refresh").unwrap().user_id(),
        "alice"
    );

    // Grok Build files the new login under the issuer it was given; the
    // saved-login policy finds it there for the renewal.
    let scoped_key = format!("{issuer}::{}", crate::grok::CLIENT_ID);
    let signed_in = json!({"user_id":"alice","team_id":"team","key":"issued-access","refresh_token":"issued-refresh","oidc_issuer":issuer});
    std::fs::write(
        &auth,
        json!({&xai_key: saved, &scoped_key: signed_in}).to_string(),
    )
    .unwrap();
    let reply = http()
        .post(format!("{issuer}/oauth2/token"))
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", crate::grok::CLIENT_ID),
            ("refresh_token", "issued-refresh"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), 200);
    reply.bytes().await.unwrap();
    assert!(reached(&mut mapped).ends_with("refresh_token=issued-refresh"));

    // Inside a decrypted CONNECT the client already reaches xAI's token
    // endpoint through the proxy port, so the document is xAI's own.
    let reply = proxy_port_client(&running)
        .get(crate::grok::DISCOVERY_URL)
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), 200);
    let document: serde_json::Value =
        serde_json::from_slice(&reply.bytes().await.unwrap()).unwrap();
    assert_eq!(document["token_endpoint"], crate::grok::TOKEN_URL);
    assert!(reached(&mut mapped).starts_with("GET /.well-known/openid-configuration "));

    // Anything else is refused before any connection.
    let refused = [
        (
            "POST",
            "/oauth2/token",
            vec![
                ("grant_type", "authorization_code"),
                ("client_id", crate::grok::CLIENT_ID),
                ("code", "code"),
            ],
            403,
        ),
        (
            "POST",
            "/oauth2/token",
            vec![
                device[0],
                device[1],
                device[2],
                ("refresh_token", "old-refresh"),
            ],
            403,
        ),
        (
            "POST",
            "/oauth2/token",
            vec![device[0], ("client_id", "another-client"), device[2]],
            403,
        ),
        (
            "POST",
            "/oauth2/token",
            vec![device[0], device[1], device[2], device[2]],
            400,
        ),
        (
            "POST",
            "/oauth2/device/code",
            vec![("client_id", "another-client")],
            403,
        ),
        ("GET", "/oauth2/token", vec![], 405),
        ("GET", "/oauth2/device/code", vec![], 405),
        ("POST", "/.well-known/openid-configuration", vec![], 405),
        ("GET", "/.well-known/jwks.json", vec![], 404),
    ];
    for (method, path, form, status) in refused {
        let reply = http()
            .request(method.parse().unwrap(), format!("{issuer}{path}"))
            .form(&form)
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), status, "{method} {path} {form:?}");
    }
    let reply = http()
        .post(format!("{issuer}/oauth2/device/code"))
        .header("content-type", "application/json")
        .body(json!({"client_id": crate::grok::CLIENT_ID}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), 415);
    assert!(mapped.requests.try_recv().is_err());
    running.server.logger.flush().unwrap();
    let log = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
    for secret in [
        "device-secret",
        "issued-access",
        "issued-refresh",
        "old-refresh",
    ] {
        assert!(!log.contains(secret), "{secret}");
    }
}

#[tokio::test]
async fn copool_grok_refresh_and_usage_follow_the_account_across_rotation_and_restart() {
    let mut bootstrap = fixture("http", "grok").await;
    let mut mapped = fixture("http", "grok").await;
    let bootstrap_url = format!("http://{}", bootstrap.addr);
    let mapped_url = format!("http://{}", mapped.addr);
    let state = tempfile::tempdir().unwrap();
    let file = state.path().join(crate::refresh_routes::GROK_FILE);
    let config = format!(
        "connect_credential_routing: true\nproxies:\n  first: {bootstrap_url}\n  mapped: {mapped_url}\ngrok:\n  account_auth_file_only: false\n  routing:\n    account_fallback: first\n    account: {{alice/team: mapped}}\n"
    );
    for restart in [false, true] {
        let running = running_with_server(
            &config,
            |_| {},
            |server| {
                server.with_grok_routes(crate::refresh_routes::RefreshRoutes::load(file.clone()))
            },
        )
        .await;
        trust(&running, &bootstrap, &bootstrap_url);
        trust(&running, &mapped, &mapped_url);
        let client = proxy_port_client(&running);
        let refresh = if restart {
            "issued-refresh"
        } else {
            "initial+refresh"
        };
        let form = [
            ("grant_type", "refresh_token"),
            ("client_id", crate::grok::CLIENT_ID),
            ("refresh_token", refresh),
        ];
        let reply = client
            .post("https://auth.x.ai/oauth2/token")
            .form(&form)
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), 200);
        assert!(reply.text().await.unwrap().contains("issued-access"));
        let selected = if restart { &mut mapped } else { &mut bootstrap };
        assert!(
            selected
                .requests
                .recv()
                .await
                .unwrap()
                .starts_with("CONNECT auth.x.ai:443 ")
        );
        let request = selected.requests.recv().await.unwrap();
        assert!(request.starts_with("POST /oauth2/token "));
        assert!(request.ends_with(if restart {
            "refresh_token=issued-refresh"
        } else {
            "refresh_token=initial%2Brefresh"
        }));
        for (path, expected) in [
            ("/v1/billing?format=credits", "creditUsagePercent"),
            ("/v1/settings", "SuperGrok"),
        ] {
            let reply = client
                .get(format!("{}{path}", crate::grok::API))
                .bearer_auth("issued-access")
                .header("x-grok-client-version", "test")
                .send()
                .await
                .unwrap();
            assert_eq!(reply.status(), 200);
            assert!(reply.text().await.unwrap().contains(expected));
            assert!(
                mapped
                    .requests
                    .recv()
                    .await
                    .unwrap()
                    .starts_with("CONNECT cli-chat-proxy.grok.com:443 ")
            );
            let request = mapped.requests.recv().await.unwrap();
            assert!(request.starts_with(&format!("GET {path} ")));
            assert!(request.contains("authorization: Bearer issued-access"));
            assert!(request.contains("x-grok-client-version: test"));
        }
        // A model call with the rotated opaque access token keeps the same
        // account exit, including a mapping by user/team.
        let response = client
            .post(format!("{}/v1/responses", crate::grok::API))
            .bearer_auth("issued-access")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        response.bytes().await.unwrap();
        assert!(
            mapped
                .requests
                .recv()
                .await
                .unwrap()
                .starts_with("CONNECT cli-chat-proxy.grok.com:443 ")
        );
        assert!(
            mapped
                .requests
                .recv()
                .await
                .unwrap()
                .starts_with("POST /v1/responses ")
        );
        for (host, path) in [
            (crate::grok::ISSUER, "/v1/settings"),
            (crate::grok::API, "/oauth2/token"),
            (crate::grok::API, "/v1/chat/completions"),
        ] {
            assert_eq!(
                client
                    .get(format!("{host}{path}"))
                    .bearer_auth("issued-access")
                    .send()
                    .await
                    .unwrap()
                    .status(),
                404
            );
        }
        assert_eq!(
            client
                .get(format!("{}/v1/settings", crate::grok::API))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        assert_eq!(
            client
                .post(format!("{}/v1/settings", crate::grok::API))
                .send()
                .await
                .unwrap()
                .status(),
            405
        );
        let reply = client
            .post("https://auth.x.ai/oauth2/token")
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", "another-client"),
                ("refresh_token", "secret"),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), 403);
        // Base URL clients reach the same endpoints with or without `/v1`,
        // and with the same limits.
        for (method, path, status) in [
            ("POST", "/grok/billing", 405),
            ("DELETE", "/grok/settings", 405),
            ("POST", "/grok/v1/settings", 405),
            ("GET", "/grok/billing?other=1", 404),
            ("GET", "/grok/v1/billing?other=1", 404),
        ] {
            let reply = http()
                .request(method.parse().unwrap(), format!("{}{path}", running.url))
                .bearer_auth("issued-access")
                .send()
                .await
                .unwrap();
            assert_eq!(reply.status(), status, "{method} {path}");
        }
        assert!(mapped.requests.try_recv().is_err());
        assert!(bootstrap.requests.try_recv().is_err());
        running.server.logger.flush().unwrap();
        // Renewal, billing, settings and model calls of one login record one
        // account, as Grok Build's saved logins name it: its user ID.
        let log = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
        let accounts: std::collections::BTreeSet<_> = log
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|row| {
                row["event"] == "request_finished"
                    && row["status"] == "200"
                    && row["path"]
                        .as_str()
                        .is_some_and(|p| p.starts_with("/grok/"))
                    && row["account_id"].is_string()
            })
            .map(|row| {
                (
                    row["path"].as_str().unwrap_or("").to_owned(),
                    row["account_id"].as_str().unwrap_or("").to_owned(),
                )
            })
            .collect();
        for path in ["/v1/billing", "/v1/settings", "/v1/responses"] {
            assert!(
                accounts.iter().any(|(p, _)| p.ends_with(path)),
                "{path}: {accounts:?}"
            );
        }
        assert!(
            accounts.iter().all(|(_, id)| *id == "alice"),
            "{accounts:?}"
        );
        for content in [
            std::fs::read_to_string(&file).unwrap(),
            std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap(),
        ] {
            for token in ["initial+refresh", "issued-access", "issued-refresh"] {
                assert!(!content.contains(token));
            }
        }
    }
}

// Grok Build's login, read from a temporary home's auth.json, and its API
// keys take their own routes; only the loopback fixture proxy is reached.
#[tokio::test]
async fn grok_saved_logins_and_api_keys_take_their_routes_and_record_usage() {
    const TOKEN: &str = "grok-saved-access-token";
    let named = std::env::var("CARGO_PKG_NAME").expect("cargo test sets CARGO_PKG_NAME");
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("auth.json"),
        crate::grok::tests::auth_json(TOKEN, "grok@example.com"),
    )
    .unwrap();
    let mut fixture = fixture("http", "untyped_sse").await;
    let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
    let running = running(&format!(
        "proxies:\n  login: {endpoint}\n  key: {endpoint}\ngrok:\n  homes: [{:?}]\n  routing:\n    account:\n      grok@example.com: login\n    api_key:\n      CARGO_PKG_NAME: key\n",
        home.path().to_string_lossy()
    ))
    .await;
    trust(&running, &fixture, &endpoint);
    let send = |path: &str, token: &str| {
        http()
            .post(format!("{}{path}", running.url))
            .bearer_auth(token)
            .header("cookie", "private-cookie")
            .header("x-grok-session-id", "grok-session")
            .header("x-grok-req-id", "grok-request")
            .body(r#"{"model":"grok-request","input":"PRIVATE PROMPT"}"#)
            .send()
    };
    let response = send("/grok/v1/responses", TOKEN).await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), MODEL_STREAM);
    let connect = fixture.requests.recv().await.unwrap();
    assert!(
        connect.starts_with("CONNECT cli-chat-proxy.grok.com:443 "),
        "{connect}"
    );
    let forwarded = fixture.requests.recv().await.unwrap();
    assert!(
        forwarded.starts_with("POST /v1/responses HTTP/1.1"),
        "{forwarded}"
    );
    assert!(forwarded.contains(&format!("authorization: Bearer {TOKEN}")));
    assert!(!forwarded.contains("private-cookie"));
    let call = first_record(&running, "model_call_finished").await;
    assert_eq!(call["service"], "grok");
    assert!(call.get("provider").is_none());
    assert_eq!(call["account_id"], crate::grok::tests::USER_ID);
    assert_eq!(call["account_label"], "grok@example.com");
    assert_eq!(call["credential_match"], "configured");
    assert_eq!(call["proxy"], "login");
    assert_eq!(call["upstream_base_url"], "https://cli-chat-proxy.grok.com");
    assert_eq!(call["session_id"], "grok-session");
    assert_eq!(call["client_request_id"], "grok-request");
    assert_eq!(call["model"], "gpt-test");
    assert_eq!(call["input_tokens"], "12");
    assert_eq!(call["output_tokens"], "3");
    let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
    assert!(!raw.contains(TOKEN) && !raw.contains("PRIVATE PROMPT"));

    // The named key goes to the xAI API through its own route.
    let response = send("/grok/v1/chat/completions", &named).await.unwrap();
    assert_eq!(response.status(), 200);
    response.text().await.unwrap();
    let connect = fixture.requests.recv().await.unwrap();
    assert!(connect.starts_with("CONNECT api.x.ai:443 "), "{connect}");
    let forwarded = fixture.requests.recv().await.unwrap();
    assert!(
        forwarded.starts_with("POST /v1/chat/completions HTTP/1.1"),
        "{forwarded}"
    );
    assert!(forwarded.contains(&format!("authorization: Bearer {named}")));

    // Another login, an unknown key and a login sent elsewhere reach nothing.
    let other = crate::grok::tests::jwt("other-user", "other@example.com");
    for (path, token, status) in [
        ("/grok/v1/responses", other.as_str(), 401),
        ("/grok/v1/responses", "xai-unknown-key", 401),
        ("/grok/https://relay.invalid/v1/responses", TOKEN, 502),
        ("/grok/v1/responses", "", 401),
    ] {
        let response = send(path, token).await.unwrap();
        assert_eq!(response.status(), status, "{path} {token}");
    }
    assert!(fixture.requests.try_recv().is_err());
    let unconfigured = running_with_tls("", |_| {}).await;
    let response = http()
        .post(format!("{}/grok/v1/responses", unconfigured.url))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);

    // The routing page marks the saved login's route active.
    let states = running.server.account_route_states().await;
    assert_eq!(states[3]["grok@example.com"], "active");
    let labels = running.server.cached_traffic_credential_labels().await;
    assert_eq!(
        labels[&("Grok".to_owned(), crate::grok::tests::USER_ID.to_owned())],
        "grok@example.com"
    );
}
