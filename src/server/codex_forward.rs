//! Codex's requests: routed by the ChatGPT login they carry or a configured
//! URL route; OpenAI's docs MCP by a login when one is sent.
use super::pipeline::{Adapter, Codex, Credential, Plan, Relay, RouteRecord, TokenRoute};
use super::*;
use hyper::http::request::Parts;

/// A Codex Route: its credential's, or none for the docs MCP's fallback.
pub(super) struct CodexRoute {
    route: Option<crate::routing::Route>,
    proxy: Choice,
    docs: bool,
}

fn header<'a>(parts: &'a Parts, name: &str) -> Option<&'a str> {
    parts.headers.get(name).and_then(|h| h.to_str().ok())
}

impl Adapter for Codex {
    type Own = ();
    type Pending = CodexRoute;
    type Route = CodexRoute;

    async fn plan(
        server: &Server,
        parts: &mut Parts,
        target: &Target,
    ) -> Result<Plan<(), CodexRoute>> {
        if target.kind() == Kind::TokenRefresh {
            return Ok(Plan::Own(()));
        }
        let rest = target.rest();
        if crate::routing::by_url::match_route(&server.config.codex.routing.api_key, rest)?
            .is_some()
        {
            if let Some(route) = server
                .config
                .resolve_url(header(parts, "authorization"), rest)?
            {
                return Ok(Plan::Route(CodexRoute {
                    proxy: route.proxy.clone(),
                    route: Some(route),
                    docs: false,
                }));
            }
        }
        Ok(Plan::Account)
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
        let learned = server.codex_refresh_routes.identity(&token);
        let (proxy, identity, configured) = server
            .config
            .resolve_refresh(&token, learned.as_ref())
            .await?;
        // A learned or saved refresh token names its account; a probe has none.
        RouteRecord {
            account_id: identity.as_ref().map(|i| i.account_id.clone()),
            account_label: identity.as_ref().and_then(|i| {
                crate::identity::routing_account_label(
                    &server.config.codex.routing,
                    &i.account_id,
                    &i.usernames,
                    "",
                )
            }),
            configured,
            ..Default::default()
        }
        .stamp(log);
        // The refresh token it replaces names the account even where the
        // routing does not map it.
        let route = TokenRoute {
            url: Url::parse(TOKEN_REFRESH_UPSTREAM).unwrap(),
            proxy,
            routes: server.codex_refresh_routes.clone(),
            previous: identity,
            sent: None,
            observe: None,
        };
        server.relay_token_refresh(parts, bytes, route, log).await
    }

    fn renews(_: &Server) -> bool {
        true
    }

    async fn account(server: &Server, parts: &Parts, target: &Target) -> Result<CodexRoute> {
        let auth = header(parts, "authorization");
        let account = header(parts, "chatgpt-account-id");
        let path = target.rest().split('?').next().unwrap_or("");
        if path == MCP_PATH {
            return Ok(match server.config.resolve(auth, false).await {
                Ok(r) if account.is_none() || account == r.account_id.as_deref() => CodexRoute {
                    proxy: r.proxy.clone(),
                    route: Some(r),
                    docs: true,
                },
                _ => CodexRoute {
                    route: None,
                    proxy: server
                        .config
                        .codex
                        .routing
                        .mcp_fallback
                        .clone()
                        .unwrap_or_else(Choice::direct),
                    docs: true,
                },
            });
        }
        let r = server.config.resolve(auth, true).await?;
        if r.account_id.is_some() && account.is_some() && account != r.account_id.as_deref() {
            return Err(Error::new(
                409,
                "Account changed; retry with the current login.",
            ));
        }
        Ok(CodexRoute {
            proxy: r.proxy.clone(),
            route: Some(r),
            docs: false,
        })
    }

    fn saved_login(pending: &CodexRoute) -> Option<&str> {
        pending
            .route
            .as_ref()
            .filter(|r| {
                r.account_id.is_some()
                    && r.provider.is_none()
                    && !r.custom_upstream
                    && !pending.docs
            })
            .map(|r| r.token.as_str())
    }

    async fn complete(_: &Server, pending: CodexRoute, _: &mut RequestLog) -> Result<CodexRoute> {
        Ok(pending)
    }

    fn record(_: &Server, route: &CodexRoute) -> RouteRecord {
        let Some(r) = &route.route else {
            return RouteRecord::default();
        };
        RouteRecord {
            provider: r.provider.clone(),
            upstream_base_url: Some(r.upstream.clone()),
            credential_ref: r
                .account_id
                .is_none()
                .then(|| crate::identity::shared_api_reference("Codex", &r.upstream, &r.token))
                .flatten(),
            account_id: r.account_id.clone(),
            account_label: r.account_label.clone(),
            configured: r.configured,
        }
    }

    fn build(
        server: &Server,
        route: CodexRoute,
        parts: &Parts,
        target: &Target,
        log: &mut RequestLog,
    ) -> Result<Relay> {
        let CodexRoute { route, proxy, docs } = route;
        let target = target.rest();
        let path = target.split('?').next().unwrap_or("");
        if path.starts_with("/mcp/") && !docs {
            return Err(Error::new(404, "Unknown MCP endpoint."));
        }
        if docs && !matches!(parts.method.as_str(), "GET" | "POST" | "DELETE") {
            return Err(Error::new(405, "MCP supports GET, POST and DELETE."));
        }
        if docs {
            if target.contains('#') {
                return Err(Error::config("Invalid MCP request target."));
            }
            log.field(
                "routing",
                if route.is_some() {
                    "credential"
                } else {
                    "mcp_fallback"
                },
            );
            return Ok(Relay {
                url: Url::parse(&format!("{MCP_UPSTREAM}{}", &target[MCP_PATH.len()..]))
                    .map_err(|_| Error::config("Invalid MCP request target."))?,
                proxy,
                native_tls: route.as_ref().is_some_and(|r| r.custom_upstream),
                credential: Credential::Docs,
                cooldown: None,
                renews: false,
                claude_identity: None,
            });
        }
        let r = route.unwrap();
        let url = if let Some(method) = account_query_for_route(target, r.account_id.is_some()) {
            if r.account_id.is_none() {
                return Err(Error::new(
                    403,
                    "Account usage queries require a matched ChatGPT login credential.",
                ));
            }
            if parts.method != method {
                return Err(Error::new(
                    405,
                    if method == "GET" {
                        "Account usage queries support GET only."
                    } else {
                        "Reset credit consumption supports POST only."
                    },
                ));
            }
            query_url(&r.upstream, target)?
        } else {
            let url = upstream_url(&r.upstream, target, r.account_id.is_some())?;
            if r.provider.is_some() {
                server.config.codex.base_url.validate_api_upstream(&url)?;
            }
            url
        };
        Ok(Relay {
            url,
            proxy,
            native_tls: r.custom_upstream,
            credential: Credential::Codex {
                token: r.token,
                account_id: r.account_id,
            },
            cooldown: None,
            renews: true,
            claude_identity: None,
        })
    }
}
