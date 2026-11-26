//! Claude's requests: routed by the Claude login or API key they carry, or a
//! configured URL route for an explicit upstream.
use super::identity_directory::{Directory, Lookup, Texts};
use super::pipeline::{Adapter, Claude, Credential, Plan, Relay, RouteRecord, TokenRoute};
use super::*;
use crate::claude::{ClaudeIdentity, ClaudeRoute};
use hyper::http::request::Parts;

/// Claude's Account lookup: its OAuth profile.
impl Lookup for Claude {
    type Identity = ClaudeIdentity;
    const SERVICE: &'static str = "claude";
    const HEADERS: &'static [(&'static str, &'static str)] =
        &[("anthropic-beta", "oauth-2025-04-20")];
    const TEXTS: Texts = Texts {
        request: "Invalid Claude profile request.",
        failed: "Claude profile lookup failed.",
        read: "Claude profile read failed.",
        too_large: "Claude profile exceeds 64 KiB.",
        json: "Invalid Claude profile JSON.",
        timeout: "Claude account probe timed out.",
    };
    fn directory(server: &Server) -> &Directory<ClaudeIdentity> {
        &server.claude_identities
    }
    // Before the token's identity is known, account_probe (defaulting to
    // account_fallback) provides the lookup's proxy.
    fn url(server: &Server) -> Result<Url> {
        server.config.claude.url("/api/oauth/profile")
    }
    fn identity(answer: &serde_json::Value) -> Result<ClaudeIdentity> {
        ClaudeIdentity::profile(answer)
    }
}

impl Adapter for Claude {
    type Own = ();
    type Pending = ClaudeRoute;
    type Route = ClaudeRoute;

    async fn plan(
        server: &Server,
        parts: &mut Parts,
        target: &Target,
    ) -> Result<Plan<(), ClaudeRoute>> {
        if target.kind() == Kind::TokenRefresh {
            return Ok(Plan::Own(()));
        }
        Ok(
            match server
                .config
                .claude
                .explicit_api_route(&parts.headers, target.rest())?
            {
                Some(route) => Plan::Route(route),
                None => Plan::Account,
            },
        )
    }

    async fn own(
        server: &Server,
        incoming: Request<Incoming>,
        (): (),
        log: &mut RequestLog,
    ) -> Result<reqwest::Response> {
        let (parts, bytes) = Server::token_refresh_body(incoming).await?;
        let token = refresh_token(&bytes)
            .ok_or(Error::new(400, "Token refresh requires a refresh_token."))?;
        let mut learned = server.refresh_routes.identity(&token);
        let existing = learned.clone();
        let recovered = &mut learned;
        let logger = &mut *log;
        let (proxy, account_id, configured) = server
            .config
            .claude
            .resolve_refresh_with_profile(&token, existing.as_ref(), |access, probe| async move {
                let named = server
                    .lookup_account::<Claude>(&access, &probe, logger)
                    .await?;
                *recovered = Some(named.clone());
                Ok(named)
            })
            .await?;
        // A learned or saved refresh token names its account; a probe has none.
        RouteRecord {
            account_label: learned
                .as_ref()
                .filter(|identity| Some(&identity.account_id) == account_id.as_ref())
                .and_then(|identity| {
                    crate::identity::routing_account_label(
                        &server.config.claude.routing,
                        &identity.account_id,
                        &identity.usernames,
                        "",
                    )
                }),
            account_id,
            configured,
            ..Default::default()
        }
        .stamp(log);
        // The refresh token it replaces names the account even where the
        // routing does not map it.
        let route = TokenRoute {
            url: Url::parse(crate::claude::TOKEN_REFRESH_UPSTREAM).unwrap(),
            proxy,
            routes: server.refresh_routes.clone(),
            previous: learned,
            sent: None,
            observe: None,
        };
        server.relay_token_refresh(parts, bytes, route, log).await
    }

    fn renews(_: &Server) -> bool {
        true
    }

    async fn account(server: &Server, parts: &Parts, target: &Target) -> Result<ClaudeRoute> {
        let route = server.config.claude.resolve(&parts.headers).await?;
        // Validate against the matched credential's upstream, before any
        // OAuth profile lookup. Named settings can override the global base.
        route.url(target.rest())?;
        Ok(route)
    }

    /// A Bearer token on Claude's own upstream may be a Saved login's; only
    /// one whose account the lookup matches is renewed.
    fn saved_login(pending: &ClaudeRoute) -> Option<&str> {
        (pending.bearer && !pending.custom_upstream).then_some(pending.token.as_str())
    }

    async fn complete(
        server: &Server,
        mut route: ClaudeRoute,
        log: &mut RequestLog,
    ) -> Result<ClaudeRoute> {
        if route.needs_profile {
            let identity = server
                .lookup_account::<Claude>(&route.token, &route.proxy, log)
                .await?;
            server.config.claude.apply_profile(&mut route, identity)?;
        }
        Ok(route)
    }

    fn record(server: &Server, r: &ClaudeRoute) -> RouteRecord {
        RouteRecord {
            provider: Some(r.label.clone()),
            upstream_base_url: Some(r.upstream.clone()),
            credential_ref: r
                .identity
                .is_none()
                .then(|| crate::identity::shared_api_reference("Claude", &r.upstream, &r.token))
                .flatten(),
            account_id: r.identity.as_ref().map(|i| i.account_id.clone()),
            account_label: r.identity.as_ref().and_then(|identity| {
                crate::identity::routing_account_label(
                    &server.config.claude.routing,
                    &identity.account_id,
                    &identity.usernames,
                    &r.label,
                )
            }),
            configured: r.configured,
        }
    }

    fn build(
        _: &Server,
        route: ClaudeRoute,
        parts: &Parts,
        target: &Target,
        _: &mut RequestLog,
    ) -> Result<Relay> {
        let url = route.url(target.rest())?;
        let decoded_path = percent_encoding::percent_decode_str(url.path())
            .decode_utf8()
            .map_err(|_| Error::new(400, "Invalid Claude request path."))?;
        if decoded_path
            .trim_end_matches('/')
            .ends_with("/api/oauth/usage")
        {
            if !route.matched_account {
                return Err(Error::new(
                    403,
                    "Claude usage requires a matched OAuth account.",
                ));
            }
            if parts.method != "GET" {
                return Err(Error::new(405, "Claude usage supports GET only."));
            }
        }
        Ok(Relay {
            url,
            proxy: route.proxy.clone(),
            native_tls: route.custom_upstream,
            renews: route.matched_account && route.bearer,
            claude_identity: route.identity.clone(),
            cooldown: None,
            credential: Credential::Claude(route),
        })
    }
}
