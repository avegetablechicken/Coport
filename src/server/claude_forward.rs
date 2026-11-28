//! Claude's requests: routed by the Claude login or API key they carry, or a
//! configured URL route for an explicit upstream.
use super::identity_directory::{Directory, Lookup, State, Texts};
use super::pipeline::{Adapter, Claude, Credential, Plan, Relay, RouteRecord, TokenRoute};
use super::*;
use crate::claude::{ClaudeIdentity, ClaudeRoute};
use crate::route_activation::Login;
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

impl Server {
    /// Claude's logins for Route activation: local OAuth metadata or a
    /// source's route name first, then what Account lookups found. Never
    /// probes or runs a shell.
    pub(super) async fn claude_logins(&self) -> Vec<Login> {
        let claude = &self.config.claude;
        let routing = &claude.routing;
        // Without a fallback, an unnamed login's route waits on discovery.
        let discovers = routing.account_default().is_none() && routing.account_probe.is_some();
        let mut logins = Vec::new();
        for (source, account) in claude.account_sources() {
            let Ok(token) = account.claude_token_with_shell(false).await else {
                // A login-shell variable is not read for display; it may exist.
                if matches!(account, crate::config::AccountSource::Env(_)) {
                    logins.push(Login::Unreadable);
                }
                continue;
            };
            let label = |identity: &ClaudeIdentity| {
                crate::identity::routing_account_label(
                    routing,
                    &identity.account_id,
                    &identity.usernames,
                    &source,
                )
            };
            // Local OAuth identity takes precedence over a configured source label.
            let local = match account.claude_identity() {
                Some(identity) => label(&identity),
                None => routing
                    .account
                    .contains_key(&source)
                    .then(|| source.clone()),
            };
            if let Some(route) = local {
                logins.push(Login::Active(route));
                continue;
            }
            // Until its account is known, an unnamed login could belong to any
            // route no other evidence settles. A failure stays the latest
            // result after its retry delay. With a fallback, its requests take
            // the fallback whatever its account is.
            logins.extend(match self.claude_identities.state(&token).await {
                State::Known(identity) if routing.account_default().is_none() => {
                    label(&identity).map(Login::Remote)
                }
                State::Known(_) => None,
                State::Failed if discovers => Some(Login::ProbeFailed),
                State::Unknown if discovers => Some(Login::Unidentified),
                State::Failed | State::Unknown => None,
            });
        }
        logins
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

    /// Only an account Route's token can be a Saved login's.
    fn saved_login(pending: &ClaudeRoute) -> Option<&str> {
        match pending {
            ClaudeRoute::Account(route) => route.saved_login(),
            ClaudeRoute::Api(_) => None,
        }
    }

    async fn complete(
        server: &Server,
        mut route: ClaudeRoute,
        log: &mut RequestLog,
    ) -> Result<ClaudeRoute> {
        if let ClaudeRoute::Account(route) = &mut route
            && route.needs_profile
        {
            let identity = server
                .lookup_account::<Claude>(&route.token, &route.proxy, log)
                .await?;
            server.config.claude.apply_profile(route, identity)?;
        }
        Ok(route)
    }

    fn record(_: &Server, r: &ClaudeRoute) -> RouteRecord {
        match r {
            ClaudeRoute::Api(route) => RouteRecord::api("Claude", route),
            // A token whose account is unknown is recorded as a key is.
            ClaudeRoute::Account(r) => RouteRecord {
                credential_ref: r
                    .identity
                    .is_none()
                    .then(|| crate::identity::shared_api_reference("Claude", &r.ext, &r.token))
                    .flatten(),
                ..RouteRecord::account(r, r.ext.clone())
            },
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
        let account = match &route {
            ClaudeRoute::Account(account) => Some(account),
            ClaudeRoute::Api(_) => None,
        };
        let matched_account = account.is_some_and(|a| a.matched_login);
        let decoded_path = percent_encoding::percent_decode_str(url.path())
            .decode_utf8()
            .map_err(|_| Error::new(400, "Invalid Claude request path."))?;
        if decoded_path
            .trim_end_matches('/')
            .ends_with("/api/oauth/usage")
        {
            if !matched_account {
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
            proxy: route.proxy().clone(),
            native_tls: matches!(&route, ClaudeRoute::Api(api) if api.custom_upstream),
            renews: matched_account,
            claude_identity: account.and_then(|a| a.identity.clone()),
            cooldown: None,
            credential: Credential::Claude(route),
        })
    }
}
