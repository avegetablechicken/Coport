use super::*;

// Synthetic upstream.invalid stays on loopback; the client trusts only the
// generated proxy CA, which must vouch for the decrypted destination.
#[tokio::test]
async fn proxy_port_requests_are_routed_by_credential() {
    let mut fixture = fixture("http", "oauth_json").await;
    let port = fixture.addr.port();
    let dirs = tempfile::tempdir().unwrap();
    for label in ["a", "b"] {
        let home = dirs.path().join(label);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join(".credentials.json"),
            json!({"claudeAiOauth":{"accessToken":format!("token-{label}")}}).to_string(),
        )
        .unwrap();
        std::fs::write(
            home.join(".claude.json"),
            json!({"oauthAccount":{"accountUuid":label,"emailAddress":format!("{label}@example.invalid")}})
                .to_string(),
        )
        .unwrap();
    }
    let dir = |label: &str| serde_json::to_string(&dirs.path().join(label)).unwrap();
    let opaque = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let opaque_port = opaque.local_addr().unwrap().port();
    let running = running(&format!(
        "connect_credential_routing: true\nproxies:\n  pa: http://a:x@127.0.0.1:{port}\n  pb: http://b:y@127.0.0.1:{port}\nclaude:\n  config_dirs: [{}, {}]\n  base_url: https://upstream.invalid\n  routing:\n    account:\n      a@example.invalid: pa\n      b@example.invalid: pb\n",
        dir("a"),
        dir("b")
    ))
    .await;
    for endpoint in ["a:x", "b:y"] {
        trust(
            &running,
            &fixture,
            &format!("http://{endpoint}@127.0.0.1:{port}"),
        );
    }
    let client = proxy_port_client(&running);
    for (token, authorization) in [("token-a", "YTp4"), ("token-b", "Yjp5")] {
        let response = client
            .get("https://upstream.invalid/api/oauth/usage?scope=all")
            .bearer_auth(token)
            .header("anthropic-beta", "oauth-2025-04-20")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let value: serde_json::Value =
            serde_json::from_str(&response.text().await.unwrap()).unwrap();
        assert_eq!(value["five_hour"]["utilization"], 42);
        let connect = fixture.requests.recv().await.unwrap();
        assert!(
            connect.starts_with("CONNECT upstream.invalid:443"),
            "{connect}"
        );
        assert!(
            connect.contains(&format!("Proxy-Authorization: Basic {authorization}")),
            "{connect}"
        );
        let request = fixture.requests.recv().await.unwrap();
        assert!(
            request.starts_with("GET /api/oauth/usage?scope=all "),
            "{request}"
        );
        assert!(request.contains(&format!("authorization: Bearer {token}")));
    }
    // An unmatched login is rejected locally, before any outbound connection.
    let response = client
        .get("https://upstream.invalid/v1/messages")
        .bearer_auth("token-unknown")
        .send()
        .await
        .unwrap();
    assert!(response.status().is_client_error(), "{}", response.status());
    // Other destinations are denied without being contacted.
    let opaque_destination = format!("127.0.0.1:{opaque_port}");
    for destination in ["other.invalid:443", opaque_destination.as_str()] {
        let mut socket = tokio::net::TcpStream::connect(running.url.trim_start_matches("http://"))
            .await
            .unwrap();
        socket
            .write_all(
                format!("CONNECT {destination} HTTP/1.1\r\nHost: {destination}\r\n\r\n").as_bytes(),
            )
            .await
            .unwrap();
        let head = read_response_head(&mut socket).await;
        assert!(head.starts_with("HTTP/1.1 403"), "{destination}: {head}");
    }
    assert!(fixture.requests.try_recv().is_err());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), opaque.accept())
            .await
            .is_err()
    );
    // A denied destination is logged with the host the client asked for.
    let denied = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            running.server.logger.flush().unwrap();
            let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
            if let Some(row) = raw
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .find(|row| row["event"] == "request_rejected" && row["method"] == "CONNECT")
            {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(denied["destination"], "other.invalid:443");
    // Activity shows a CONNECT's target, not `/`.
    assert_eq!(denied["path"], "other.invalid:443");
}

// A client keeps its tunnel for the next request, as with any HTTP proxy.
#[tokio::test]
async fn a_decrypted_tunnel_serves_requests_one_after_another() {
    let mut fixture = fixture("http", "oauth_json").await;
    let endpoint = format!("http://127.0.0.1:{}", fixture.addr.port());
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        "[model_providers.ShareCoder]\nbase_url = 'https://upstream.invalid/v1'\nexperimental_bearer_token = 'fake-provider-secret'\n",
    )
    .unwrap();
    let running = running(&format!(
        "connect_credential_routing: true\nproxies:\n  selected: {endpoint}\ncodex:\n  homes: [{}]\n  routing:\n    api_key: {{ShareCoder: selected}}\n",
        serde_json::to_string(home.path()).unwrap()
    ))
    .await;
    trust(&running, &fixture, &endpoint);
    let proxy = proxy_port_client(&running);
    for _ in 0..2 {
        let response = proxy
            .get("https://upstream.invalid/v1/models")
            .bearer_auth("fake-provider-secret")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        response.text().await.unwrap();
        // Outbound, each request still takes its own route.
        assert!(
            fixture
                .requests
                .recv()
                .await
                .unwrap()
                .starts_with("CONNECT upstream.invalid:443")
        );
        assert!(
            fixture
                .requests
                .recv()
                .await
                .unwrap()
                .contains(" /v1/models ")
        );
    }
    let rows = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            running.server.logger.flush().unwrap();
            let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
            let rows: Vec<serde_json::Value> = raw
                .lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect();
            if rows
                .iter()
                .filter(|row| row["event"] == "request_finished" && row["method"] == "GET")
                .count()
                == 2
            {
                return rows;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        rows.iter()
            .filter(|row| row["event"] == "request_received" && row["method"] == "CONNECT")
            .count(),
        1,
        "Both requests share one tunnel"
    );
    let connect = rows
        .iter()
        .find(|row| row["event"] == "request_finished" && row["method"] == "CONNECT")
        .unwrap();
    assert_eq!(connect["service"], "codex", "{connect}");
    assert_eq!(connect["provider"], "ShareCoder", "{connect}");
    assert_eq!(connect["proxy"], "selected", "{connect}");
    assert!(rows.iter().any(|row| row["event"] == "request_finished"
        && row["method"] == "GET"
        && row["credential_match"] == "configured"));
}

// Any local program may open a tunnel; one whose first request only a
// fallback carries is not taken as a configured client's.
#[tokio::test]
async fn a_tunnel_opened_for_a_fallback_request_has_no_app() {
    let mut fixture = fixture("direct", "oauth_json").await;
    let running = running(
        "connect_credential_routing: true\ncodex:\n  homes: []\n  routing:\n    fallback: none\n  base_url:\n    api_key: https://upstream.invalid/v1\n",
    )
    .await;
    trust(&running, &fixture, "none");
    let response = proxy_port_client(&running)
        .get("https://upstream.invalid/v1/models")
        .bearer_auth("unknown-client-secret")
        .send()
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
            .starts_with("GET /v1/models ")
    );
    let rows = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            running.server.logger.flush().unwrap();
            let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
            let rows: Vec<serde_json::Value> = raw
                .lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect();
            if rows
                .iter()
                .any(|row| row["event"] == "request_finished" && row["method"] == "CONNECT")
            {
                return rows;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let request = rows
        .iter()
        .find(|row| row["event"] == "request_finished" && row["method"] == "GET")
        .unwrap();
    assert_eq!(request["service"], "codex", "{request}");
    assert_eq!(request["credential_match"], "unmatched", "{request}");
    let connect = rows
        .iter()
        .find(|row| row["event"] == "request_finished" && row["method"] == "CONNECT")
        .unwrap();
    assert!(connect.get("service").is_none(), "{connect}");
    assert!(connect.get("provider").is_none(), "{connect}");
}

#[tokio::test]
async fn decrypted_tunnels_only_serve_their_destination() {
    let running = running(
        "connect_credential_routing: true\nclaude:\n  base_url: https://upstream.invalid\n  routing:\n    account_fallback: none\n",
    )
    .await;
    use rustls::pki_types::pem::PemObject;
    let ca = running
        ._temp
        .path()
        .join("tls")
        .join(crate::local_tls::PROXY_CA_FILE);
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from_pem_file(ca).unwrap())
        .unwrap();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth(),
    ));
    for (request, status) in [
        (
            "CONNECT chatgpt.com:443 HTTP/1.1\r\nHost: chatgpt.com:443\r\n\r\n",
            "400",
        ),
        (
            "GET https://other.invalid/v1/models HTTP/1.1\r\nHost: other.invalid\r\n\r\n",
            "400",
        ),
        // Absolute-form naming the tunnel's own destination is routed as usual.
        (
            "GET https://upstream.invalid/v1/models HTTP/1.1\r\nHost: upstream.invalid\r\n\r\n",
            "401",
        ),
        // Local endpoints stay on the listener itself, not inside tunnels.
        (
            "GET /health HTTP/1.1\r\nHost: upstream.invalid\r\n\r\n",
            "401",
        ),
    ] {
        let mut socket = tokio::net::TcpStream::connect(running.url.trim_start_matches("http://"))
            .await
            .unwrap();
        socket
            .write_all(
                b"CONNECT upstream.invalid:443 HTTP/1.1\r\nHost: upstream.invalid:443\r\n\r\n",
            )
            .await
            .unwrap();
        assert!(
            read_response_head(&mut socket)
                .await
                .starts_with("HTTP/1.1 200")
        );
        let name = rustls::pki_types::ServerName::try_from("upstream.invalid").unwrap();
        let mut tls = connector.connect(name, socket).await.unwrap();
        tls.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        let _ = tls.read_to_end(&mut response).await;
        let response = String::from_utf8_lossy(&response);
        assert!(
            response.starts_with(&format!("HTTP/1.1 {status}")),
            "{request}: {response}"
        );
    }
}

// Proxy clients (Copool with chatgpt_base_url on this listener) send loopback
// base URLs through the proxy as absolute-form requests naming the listener.
// They are served only while the listener is used as a proxy port.
#[tokio::test]
async fn absolute_form_requests_are_served_only_for_this_proxy_port() {
    let routes = "claude:\n  routing:\n    account_fallback: none\n";
    for enabled in [true, false] {
        let running = running(&format!("connect_credential_routing: {enabled}\n{routes}")).await;
        let address = running.url.trim_start_matches("http://").to_owned();
        let port = address.rsplit(':').next().unwrap().to_owned();
        let served = |status: &'static str| if enabled { status } else { "400" };
        for (target, status) in [
            (format!("http://{address}/health"), served("200")),
            (format!("http://localhost:{port}/health"), served("200")),
            (
                format!("HTTP://127.0.0.1:{port}/backend-api/wham/usage"),
                served("401"),
            ),
            (
                format!("http://127.0.0.1:{port}/backend-api/wham/usage?x=1"),
                served("401"),
            ),
            ("http://example.invalid/health".to_owned(), "400"),
            ("http://127.0.0.1:9/health".to_owned(), "400"),
            (format!("http://user@127.0.0.1:{port}/health"), "400"),
        ] {
            let mut socket = tokio::net::TcpStream::connect(&address).await.unwrap();
            socket
                .write_all(format!("GET {target} HTTP/1.1\r\nHost: ignored\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let head = read_response_head(&mut socket).await;
            assert!(
                head.starts_with(&format!("HTTP/1.1 {status}")),
                "{enabled} {target}: {head}"
            );
        }
        // Origin-form base URL requests are unaffected by the setting.
        let response = http()
            .get(format!("{}/health", running.url))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
    }
}

// Copool's usage refresh: a reqwest client using this listener as its proxy
// (without a loopback exemption) and a loopback chatgpt_base_url sends an
// absolute-form request naming the listener; it reaches the account's exit.
#[tokio::test]
async fn proxy_clients_reach_the_account_exit_with_a_loopback_base_url() {
    let mut fixture = fixture("http", "oauth_json").await;
    let port = fixture.addr.port();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("auth.json"),
        r#"{"tokens":{"account_id":"acct-1","access_token":"access-secret","refresh_token":"refresh-secret"}}"#,
    )
    .unwrap();
    let running = running(&format!(
        "connect_credential_routing: true\nproxies:\n  selected: http://a:x@127.0.0.1:{port}\ncodex:\n  homes: [{}]\n  base_url:\n    account: https://upstream.invalid/backend-api\n  routing:\n    account:\n      acct-1: selected\n",
        serde_json::to_string(&home.path()).unwrap()
    ))
    .await;
    trust(&running, &fixture, &format!("http://a:x@127.0.0.1:{port}"));
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(&running.url).unwrap())
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let local = running.url.trim_start_matches("http://");
    let response = client
        .get(format!("http://{local}/backend-api/wham/usage"))
        .bearer_auth("access-secret")
        .header("ChatGPT-Account-Id", "acct-1")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let value: serde_json::Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert_eq!(value["five_hour"]["utilization"], 42);
    let connect = fixture.requests.recv().await.unwrap();
    assert!(
        connect.starts_with("CONNECT upstream.invalid:443"),
        "{connect}"
    );
    assert!(
        connect.contains("Proxy-Authorization: Basic YTp4"),
        "{connect}"
    );
    let request = fixture.requests.recv().await.unwrap();
    assert!(
        request.starts_with("GET /backend-api/wham/usage "),
        "{request}"
    );
    assert!(request.contains("authorization: Bearer access-secret"));
    assert!(request.contains("chatgpt-account-id: acct-1"));
    // The request log names the matched account and the exit it used.
    let finished = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            running.server.logger.flush().unwrap();
            let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
            if let Some(row) = raw
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .find(|row| {
                    row["event"] == "request_finished" && row["path"] == "/backend-api/wham/usage"
                })
            {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(finished["status"], "200");
    assert_eq!(finished["account_id"], "acct-1");
    assert_eq!(finished["proxy"], "selected");
    // Copool tries these paths when the primary account usage request fails.
    for fallback in ["/api/codex/usage", "/wham/usage"] {
        let url = format!("http://{local}{fallback}?x=a%2Fb");
        let response = client
            .get(&url)
            .bearer_auth("access-secret")
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
        assert!(request.starts_with("GET /backend-api/wham/usage?x=a%2Fb "));
        assert!(request.contains("chatgpt-account-id: acct-1"));
        let denied = client
            .post(&url)
            .bearer_auth("access-secret")
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), 405);
        assert!(fixture.requests.try_recv().is_err());
    }
}

// Copool's Claude usage through the proxy port before the proxy CA is
// trusted, or while it still trusts a replaced proxy CA of the same name: the
// client refuses the certificate and nothing is forwarded. Clients trust only
// the given root, never this machine's trust store.
#[tokio::test]
async fn clients_that_do_not_trust_the_proxy_ca_are_logged_as_such() {
    let unrelated = rcgen::generate_simple_self_signed(vec!["unrelated.invalid".into()])
        .unwrap()
        .cert
        .pem();
    let replaced = tempfile::tempdir().unwrap();
    crate::local_tls::proxy_acceptors(
        replaced.path(),
        &std::collections::BTreeSet::from(["upstream.invalid".to_owned()]),
    )
    .unwrap();
    let replaced =
        std::fs::read_to_string(replaced.path().join(crate::local_tls::PROXY_CA_FILE)).unwrap();
    for root in [unrelated, replaced] {
        let running = running(
            "connect_credential_routing: true\nclaude:\n  base_url: https://upstream.invalid\n  routing:\n    account_fallback: none\n",
        )
        .await;
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .tls_built_in_root_certs(false)
            .add_root_certificate(reqwest::Certificate::from_pem(root.as_bytes()).unwrap())
            .proxy(reqwest::Proxy::https(&running.url).unwrap())
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let error = client
            .get("https://upstream.invalid/api/oauth/usage")
            .bearer_auth("token")
            .send()
            .await
            .unwrap_err();
        assert!(error.is_connect(), "{error:?}");
        let reason = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                running.server.logger.flush().unwrap();
                let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
                if let Some(row) = raw
                    .lines()
                    .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                    .find(|row| row["event"] == "request_failed" && row["method"] == "CONNECT")
                {
                    return row;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(reason["reason"], "client_rejected_proxy_ca");
        // No request reached the tunnel, so nothing shows whose it was.
        assert!(reason.get("service").is_none(), "{reason}");
    }
}

// Hosts added after the proxy CA was trusted are not decrypted with a new CA;
// their CONNECT is refused with the reason, and nothing is forwarded.
#[tokio::test]
async fn connect_to_a_host_outside_the_stored_proxy_ca_explains_the_refusal() {
    let running = running_with_tls(
        "connect_credential_routing: true\nclaude:\n  base_url: https://upstream.invalid\n  routing:\n    account_fallback: none\n    api_key:\n      'https://182.92.106.196:6060': none\n",
        |tls| {
            crate::local_tls::proxy_acceptors(
                tls,
                &std::collections::BTreeSet::from([
                    "upstream.invalid".to_owned(),
                    "platform.claude.com".to_owned(),
                ]),
            )
            .unwrap();
        },
    )
    .await;
    for (authority, expected) in [
        ("182.92.106.196:6060", "HTTP/1.1 403"),
        ("upstream.invalid:443", "HTTP/1.1 200"),
    ] {
        let mut socket = tokio::net::TcpStream::connect(running.url.trim_start_matches("http://"))
            .await
            .unwrap();
        socket
            .write_all(
                format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n").as_bytes(),
            )
            .await
            .unwrap();
        let head = read_response_head(&mut socket).await;
        assert!(head.starts_with(expected), "{authority}: {head}");
    }
    let row = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            running.server.logger.flush().unwrap();
            let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
            if let Some(row) = raw
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .find(|row| row["event"] == "request_rejected" && row["method"] == "CONNECT")
            {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(row["destination"], "182.92.106.196:6060");
    assert_eq!(
        row["reason"],
        "The proxy CA does not cover this destination; use its base URL or replace the CA."
    );
}

#[tokio::test]
async fn a_decrypted_tunnel_closed_without_a_request_is_logged() {
    use rustls::pki_types::pem::PemObject;
    let running = running(
        "connect_credential_routing: true\nclaude:\n  base_url: https://upstream.invalid\n  routing:\n    account_fallback: none\n",
    )
    .await;
    let ca = running
        ._temp
        .path()
        .join("tls")
        .join(crate::local_tls::PROXY_CA_FILE);
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from_pem_file(ca).unwrap())
        .unwrap();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth(),
    ));
    let mut socket = tokio::net::TcpStream::connect(running.url.trim_start_matches("http://"))
        .await
        .unwrap();
    socket
        .write_all(b"CONNECT upstream.invalid:443 HTTP/1.1\r\nHost: upstream.invalid:443\r\n\r\n")
        .await
        .unwrap();
    assert!(
        read_response_head(&mut socket)
            .await
            .starts_with("HTTP/1.1 200")
    );
    let name = rustls::pki_types::ServerName::try_from("upstream.invalid").unwrap();
    let mut tls = connector.connect(name, socket).await.unwrap();
    tls.shutdown().await.unwrap();
    let row = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            running.server.logger.flush().unwrap();
            let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
            if let Some(row) = raw
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .find(|row| row["event"] == "request_rejected")
            {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(row["client_connect"], "upstream.invalid");
    assert_eq!(row["status"], "400");
    assert_eq!(row["reason"], "Incomplete request headers.");
    // Without a routed request, nothing shows whose the tunnel was.
    let raw = std::fs::read_to_string(running._temp.path().join("proxy.log")).unwrap();
    let connect = raw
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|row| row["event"] == "request_finished" && row["method"] == "CONNECT")
        .unwrap();
    assert!(connect.get("service").is_none(), "{connect}");
    assert!(connect.get("account_id").is_none(), "{connect}");
    assert!(connect.get("provider").is_none(), "{connect}");
}
