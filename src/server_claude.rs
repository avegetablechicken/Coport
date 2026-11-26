//! Claude's requests: routed by the Claude login or API key they carry, or a
//! configured URL route for an explicit upstream.
use super::pipeline::{Adapter, Claude, Credential, Plan, Relay, RouteRecord};
use super::*;
use crate::claude::ClaudeRoute;

impl Adapter for Claude {
    type Own = ();
    type Pending = ClaudeRoute;
    type Route = ClaudeRoute;

    async fn plan(
        server: &Server,
        incoming: &mut Request<Incoming>,
        target: &Target,
    ) -> Result<Plan<(), ClaudeRoute>> {
        if target.kind() == Kind::TokenRefresh {
            return Ok(Plan::Own(()));
        }
        Ok(
            match server
                .config
                .claude
                .explicit_api_route(incoming.headers(), target.rest())?
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
        server.forward_token_refresh(incoming, log, true).await
    }

    fn renews(_: &Server) -> bool {
        true
    }

    async fn account(
        server: &Server,
        incoming: &Request<Incoming>,
        target: &Target,
    ) -> Result<ClaudeRoute> {
        let route = server.config.claude.resolve(incoming.headers()).await?;
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
                .lookup_claude_identity(&route.token, &route.proxy, log)
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
        incoming: &Request<Incoming>,
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
            if incoming.method() != "GET" {
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
