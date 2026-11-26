//! What a proxied request is, decided once from its method and target: the
//! **Scoped target**'s prefix, the target the **App** asked for without it,
//! whether it is a **Model call** or a **Token refresh**, and its **App**.
//!
//! Everything but the App follows from the request alone, so the proxy and
//! readers of its logs share it; the App can also need the configuration.
use crate::{Error, Result, config::Config, routing::Service};
use std::borrow::Cow;

/// What a request asks for, as far as more than one step of serving or
/// counting it cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// One HTTP Model call, POSTed with a body to observe.
    ModelCall,
    /// A GET of the Responses endpoint, which opens a WebSocket of Model calls.
    ModelWebSocket,
    /// Codex's, Claude's or Antigravity's Token refresh. Grok's is one of the
    /// subscription endpoints its adapter recognizes.
    TokenRefresh,
    Other,
}

#[derive(Clone, Debug)]
pub struct Target {
    raw: String,
    rest: String,
    scope: Option<Service>,
    kind: Kind,
}

impl Target {
    /// A request target as received, with its query; for a request in a
    /// decrypted tunnel, with that tunnel's scope.
    pub fn new(method: &str, raw: &str) -> Self {
        let (scope, rest) = split(raw);
        let kind = model_kind(method, &rest).unwrap_or_else(|| {
            let refresh = match scope {
                None => crate::claude::token_refresh(&rest) || crate::routing::token_refresh(&rest),
                Some(Service::Claude) => crate::claude::token_refresh(&rest),
                Some(Service::Codex) => crate::routing::token_refresh(&rest),
                Some(Service::Antigravity) => crate::antigravity::token_refresh(&rest),
                Some(Service::Grok) => false,
            };
            if refresh {
                Kind::TokenRefresh
            } else {
                Kind::Other
            }
        });
        Self {
            raw: raw.to_owned(),
            rest: rest.into_owned(),
            scope,
            kind,
        }
    }

    /// The target as received.
    pub fn raw(&self) -> &str {
        &self.raw
    }
    /// The target the App asked for: without the scope prefix, with an
    /// upstream URL's `//` that agy collapses restored.
    pub fn rest(&self) -> &str {
        &self.rest
    }
    /// Whether the target named its App in a scope prefix.
    pub fn scoped(&self) -> bool {
        self.scope.is_some()
    }
    pub fn kind(&self) -> Kind {
        self.kind
    }
    /// Whether the request is a Model call or opens a WebSocket of them.
    pub fn model_call(&self) -> bool {
        matches!(self.kind, Kind::ModelCall | Kind::ModelWebSocket)
    }

    /// Antigravity's and Grok's scopes, then other scopes and Token refreshes
    /// decide the App first, then URL routes, then Claude's own paths;
    /// anything else is Codex. `url_route` matches the target without its
    /// scope against configured URL routes, given whether it was scoped to
    /// Claude.
    fn app<E>(
        &self,
        url_route: impl FnOnce(&str, bool) -> std::result::Result<Option<Service>, E>,
    ) -> std::result::Result<Service, E> {
        let claude_scoped = match self.scope {
            Some(Service::Claude) => true,
            Some(app) => return Ok(app),
            None => false,
        };
        if self.kind == Kind::TokenRefresh {
            return Ok(if crate::claude::token_refresh(&self.rest) {
                Service::Claude
            } else {
                Service::Codex
            });
        }
        if let Some(app) = url_route(&self.rest, claude_scoped)? {
            return Ok(app);
        }
        Ok(if claude_scoped || claude_path(&self.rest) {
            Service::Claude
        } else {
            Service::Codex
        })
    }
}

impl Config {
    /// The App of a request; see [`Service`].
    pub fn app(&self, target: &Target) -> Result<Service> {
        if self.grok.enabled()
            && target.raw.starts_with("/https://")
            && self.grok.endpoint(&target.raw).is_some()
        {
            return Ok(Service::Grok);
        }
        target.app(|rest, claude_scoped| {
            let claude = self.claude.url_route_matches(rest)?;
            let codex = !claude_scoped
                && crate::url_routing::match_route(&self.codex.routing.api_key, rest)?.is_some();
            let antigravity = !claude_scoped && self.antigravity.url_route_matches(rest)?;
            let grok = !claude_scoped && self.grok.url_route_matches(rest)?;
            match (claude, codex, antigravity, grok) {
                (true, false, false, false) => Ok(Some(Service::Claude)),
                (false, true, false, false) => Ok(Some(Service::Codex)),
                (false, false, true, false) => Ok(Some(Service::Antigravity)),
                (false, false, false, true) => Ok(Some(Service::Grok)),
                (false, false, false, false) => Ok(None),
                _ => Err(Error::new(
                    409,
                    "API upstream is configured for several apps; use /codex/https://, /anthropic/https://, /antigravity/https:// or /grok/https://.",
                )),
            }
        })
    }
}

/// The App of a logged request path, for records written before the proxy
/// logged `service` on each of them. The configuration in force then is
/// unknown, so URL routes do not apply; the proxy logged those requests' App
/// on their later records.
pub fn logged_service(path: &str) -> Service {
    let Ok(app) = Target::new("", path).app::<std::convert::Infallible>(|_, _| Ok(None));
    app
}

/// Whether a logged request is a Model call or opens a WebSocket of them.
pub fn is_model_endpoint(method: &str, path: &str) -> bool {
    Target::new(method, path).model_call()
}

/// The App a target's scope prefix names, and the target after it.
fn split(raw: &str) -> (Option<Service>, Cow<'_, str>) {
    let after = |prefix: &str| {
        raw.strip_prefix(prefix)
            .filter(|rest| rest.starts_with('/'))
    };
    if let Some(rest) = after("/antigravity") {
        return (Some(Service::Antigravity), named_upstream_target(rest));
    }
    if let Some(rest) = after("/grok") {
        return (Some(Service::Grok), named_upstream_target(rest));
    }
    // Codex's own paths need no scope; it names only explicit upstreams.
    if let Some(rest) = raw
        .strip_prefix("/codex")
        .filter(|rest| rest.starts_with("/https://"))
    {
        return (Some(Service::Codex), rest.into());
    }
    if let Some(rest) = after("/anthropic").or_else(|| after("/claude")) {
        return (Some(Service::Claude), rest.into());
    }
    (None, raw.into())
}

/// A target with the `//` of the upstream it names: agy joins its
/// `GOOGLE_GEMINI_BASE_URL` with each request's path collapsing `//`, so
/// `<Coport>/antigravity/https://relay.example.com/antigravity` arrives as
/// `/https:/relay.example.com/antigravity/…`.
fn named_upstream_target(rest: &str) -> Cow<'_, str> {
    for scheme in ["/https:/", "/http:/"] {
        if let Some(after) = rest.strip_prefix(scheme).filter(|a| !a.starts_with('/')) {
            return format!("{scheme}/{after}").into();
        }
    }
    rest.into()
}

/// Claude's unscoped paths: Messages and OAuth endpoints, also at an explicit
/// upstream. Ambiguous endpoints such as /v1/models need its scope.
fn claude_path(target: &str) -> bool {
    let path = target.split('?').next().unwrap_or("");
    let path = match path.strip_prefix('/').filter(|p| p.starts_with("https://")) {
        Some(url) => match url::Url::parse(url) {
            Ok(url) => url.path().to_owned(),
            Err(_) => return false,
        },
        None => path.to_owned(),
    };
    path == "/v1/messages" || path.starts_with("/v1/messages/") || path.starts_with("/api/oauth/")
}

fn model_kind(method: &str, rest: &str) -> Option<Kind> {
    let path = rest.split('?').next().unwrap_or("").trim_start_matches('/');
    // Only explicit upstream targets are URLs; `v1internal:x` would parse as one.
    let url = (path.starts_with("https://") || path.starts_with("http://"))
        .then(|| url::Url::parse(path).ok())
        .flatten();
    let path = url
        .as_ref()
        .map(|u| u.path())
        .unwrap_or(path)
        .trim_matches('/');
    let ends = |suffix: &str| path == suffix || path.ends_with(&format!("/{suffix}"));
    // Gemini methods, as Cloud Code's `v1internal:streamGenerateContent`.
    let gemini = path
        .rsplit('/')
        .next()
        .and_then(|last| last.rsplit_once(':'))
        .is_some_and(|(_, method)| matches!(method, "generateContent" | "streamGenerateContent"));
    let post = gemini
        || [
            "responses",
            "responses/compact",
            "messages",
            "chat/completions",
            "completions",
        ]
        .iter()
        .any(|s| ends(s));
    match method {
        "POST" if post => Some(Kind::ModelCall),
        "GET" if ends("responses") => Some(Kind::ModelWebSocket),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Kind::*;
    use Service::*;

    fn config() -> Config {
        Config::parse("listen_port: 7889\nrequest_timeout_seconds: 3\ncodex:\n  homes: []\n  routing:\n    api_key:\n      'https://codex.invalid/v1': none\n      'https://both.invalid/v1': none\n      'https://shared.invalid/v1': none\nclaude:\n  config_dirs: []\n  routing:\n    api_key:\n      'https://gateway.invalid/v1': none\n      'https://both.invalid/v1': none\nantigravity:\n  routing:\n    api_key:\n      relay.invalid/antigravity: none\n      gateway.invalid/v1beta: none\n      shared.invalid/v1: none\ngrok:\n  homes: []\n  routing:\n    api_key:\n      grok.invalid/v1: none\n      shared.invalid/v1: none\n").unwrap()
    }

    /// A request's method and target, its App with and without the
    /// configuration, its kind and its target without the scope.
    type Case = (
        &'static str,
        &'static str,
        Service,
        Service,
        Kind,
        &'static str,
    );

    /// Every request a client sends, classified.
    #[test]
    fn every_request_target_is_classified_once() {
        let config = config();
        #[rustfmt::skip]
        let cases: &[Case] = &[
            // Codex: its own paths, its Token refresh, explicit upstreams.
            ("POST", "/v1/responses", Codex, Codex, ModelCall, "/v1/responses"),
            ("GET", "/v1/responses", Codex, Codex, ModelWebSocket, "/v1/responses"),
            ("GET", "/v1/responses/response-id", Codex, Codex, Other, "/v1/responses/response-id"),
            ("POST", "/backend-api/codex/responses/compact", Codex, Codex, ModelCall, "/backend-api/codex/responses/compact"),
            ("POST", "/backend-api/codex/analytics-events/events", Codex, Codex, Other, "/backend-api/codex/analytics-events/events"),
            ("GET", "/backend-api/wham/usage", Codex, Codex, Other, "/backend-api/wham/usage"),
            ("POST", "/mcp/openaiDeveloperDocs", Codex, Codex, Other, "/mcp/openaiDeveloperDocs"),
            ("POST", "/v1/chat/completions", Codex, Codex, ModelCall, "/v1/chat/completions"),
            ("POST", "/v1/completions", Codex, Codex, ModelCall, "/v1/completions"),
            ("POST", "/v1/not-responses", Codex, Codex, Other, "/v1/not-responses"),
            ("GET", "/v1/models", Codex, Codex, Other, "/v1/models"),
            ("POST", "/oauth/token", Codex, Codex, TokenRefresh, "/oauth/token"),
            ("POST", "/https://auth.openai.com/oauth/token", Codex, Codex, TokenRefresh, "/https://auth.openai.com/oauth/token"),
            ("POST", "/oauth/token?x=1", Codex, Codex, Other, "/oauth/token?x=1"),
            ("POST", "/https://evil.com/oauth/token", Codex, Codex, Other, "/https://evil.com/oauth/token"),
            ("POST", "/https://auth.openai.com/oauth/authorize", Codex, Codex, Other, "/https://auth.openai.com/oauth/authorize"),
            ("POST", "/codex/https://auth.openai.com/oauth/token", Codex, Codex, TokenRefresh, "/https://auth.openai.com/oauth/token"),
            ("GET", "/codex/https://chatgpt.com/backend-api/wham/usage", Codex, Codex, Other, "/https://chatgpt.com/backend-api/wham/usage"),
            ("GET", "/codex/https://example.invalid/v1/responses?model=x", Codex, Codex, ModelWebSocket, "/https://example.invalid/v1/responses?model=x"),
            ("POST", "/codex/https://gateway.invalid/v1/messages", Codex, Codex, ModelCall, "/https://gateway.invalid/v1/messages"),
            ("POST", "/codex/https://platform.claude.com/v1/oauth/token", Codex, Codex, Other, "/https://platform.claude.com/v1/oauth/token"),
            ("POST", "/https://sharecoder.invalid/responses", Codex, Codex, ModelCall, "/https://sharecoder.invalid/responses"),
            // Only explicit upstreams take Codex's scope.
            ("POST", "/codex/v1/responses", Codex, Codex, ModelCall, "/codex/v1/responses"),
            ("POST", "/https://codex.invalid/v1/messages", Codex, Claude, ModelCall, "/https://codex.invalid/v1/messages"),
            // A prefix that only begins with an App's name is not its scope.
            ("POST", "/grokking/v1/responses", Codex, Codex, ModelCall, "/grokking/v1/responses"),
            ("POST", "/anthropic-evil/v1/oauth/token", Codex, Codex, Other, "/anthropic-evil/v1/oauth/token"),
            ("CONNECT", "api.anthropic.com:443", Codex, Codex, Other, "api.anthropic.com:443"),
            // Claude: its scope, its unscoped Messages and OAuth paths, its Token refresh.
            ("POST", "/v1/messages", Claude, Claude, ModelCall, "/v1/messages"),
            ("POST", "/https://other.invalid/v1/messages", Claude, Claude, ModelCall, "/https://other.invalid/v1/messages"),
            ("GET", "/api/oauth/usage", Claude, Claude, Other, "/api/oauth/usage"),
            ("POST", "/anthropic/v1/messages", Claude, Claude, ModelCall, "/v1/messages"),
            ("POST", "/anthropic/v1/messages/count_tokens", Claude, Claude, Other, "/v1/messages/count_tokens"),
            ("GET", "/anthropic/api/oauth/usage", Claude, Claude, Other, "/api/oauth/usage"),
            ("POST", "/claude/https://example.invalid/api/v1/messages", Claude, Claude, ModelCall, "/https://example.invalid/api/v1/messages"),
            ("GET", "/claude/v1/models", Claude, Claude, Other, "/v1/models"),
            ("GET", "/anthropic/https://api.anthropic.com/v1/models", Claude, Claude, Other, "/https://api.anthropic.com/v1/models"),
            ("POST", "/v1/oauth/token", Claude, Claude, TokenRefresh, "/v1/oauth/token"),
            ("POST", "/https://platform.claude.com/v1/oauth/token", Claude, Claude, TokenRefresh, "/https://platform.claude.com/v1/oauth/token"),
            ("POST", "/anthropic/v1/oauth/token", Claude, Claude, TokenRefresh, "/v1/oauth/token"),
            ("POST", "/anthropic/https://platform.claude.com/v1/oauth/token", Claude, Claude, TokenRefresh, "/https://platform.claude.com/v1/oauth/token"),
            ("POST", "/anthropic/v1/oauth/token?redirect=evil", Claude, Claude, Other, "/v1/oauth/token?redirect=evil"),
            ("POST", "/https://platform.claude.com.evil.invalid/v1/oauth/token", Codex, Codex, Other, "/https://platform.claude.com.evil.invalid/v1/oauth/token"),
            ("POST", "/https://platform.claude.com:444/v1/oauth/token", Codex, Codex, Other, "/https://platform.claude.com:444/v1/oauth/token"),
            // Codex's Token refresh in Claude's scope is not one.
            ("POST", "/anthropic/oauth/token", Claude, Claude, Other, "/oauth/token"),
            // Antigravity: its scope, with the upstream `//` agy collapses restored.
            ("POST", "/antigravity/v1internal:streamGenerateContent?alt=sse", Antigravity, Antigravity, ModelCall, "/v1internal:streamGenerateContent?alt=sse"),
            ("GET", "/antigravity/v1internal:streamGenerateContent", Antigravity, Antigravity, Other, "/v1internal:streamGenerateContent"),
            ("POST", "/antigravity/v1internal:generateContent", Antigravity, Antigravity, ModelCall, "/v1internal:generateContent"),
            ("POST", "/antigravity/https://cloudcode-pa.googleapis.com/v1internal:streamGenerateContent", Antigravity, Antigravity, ModelCall, "/https://cloudcode-pa.googleapis.com/v1internal:streamGenerateContent"),
            ("POST", "/antigravity/https:/relay.example.com/antigravity/v1internal:generateContent", Antigravity, Antigravity, ModelCall, "/https://relay.example.com/antigravity/v1internal:generateContent"),
            ("POST", "/antigravity/v1internal:loadCodeAssist", Antigravity, Antigravity, Other, "/v1internal:loadCodeAssist"),
            ("POST", "/antigravity/v1internal:countTokens", Antigravity, Antigravity, Other, "/v1internal:countTokens"),
            ("POST", "/antigravity/v1internal:generateContentX", Antigravity, Antigravity, Other, "/v1internal:generateContentX"),
            ("POST", "/antigravity/https://oauth2.googleapis.com/token", Antigravity, Antigravity, TokenRefresh, "/https://oauth2.googleapis.com/token"),
            ("POST", "/antigravity/https://oauth2.googleapis.com/revoke", Antigravity, Antigravity, Other, "/https://oauth2.googleapis.com/revoke"),
            ("POST", "/antigravityx/v1internal:x", Codex, Codex, Other, "/antigravityx/v1internal:x"),
            // Gemini's API is Antigravity's only through its URL routes.
            ("POST", "/v1beta/models/gemini-3-pro:generateContent", Codex, Codex, ModelCall, "/v1beta/models/gemini-3-pro:generateContent"),
            ("POST", "/https://relay.invalid/antigravity/v1beta/models/g:generateContent", Antigravity, Codex, ModelCall, "/https://relay.invalid/antigravity/v1beta/models/g:generateContent"),
            ("POST", "/https://gateway.invalid/v1beta/models/g:generateContent", Antigravity, Codex, ModelCall, "/https://gateway.invalid/v1beta/models/g:generateContent"),
            // Claude's scope never considers another App's URL routes.
            ("POST", "/anthropic/https://relay.invalid/antigravity/v1beta/models/g:generateContent", Claude, Claude, ModelCall, "/https://relay.invalid/antigravity/v1beta/models/g:generateContent"),
            // Grok: its scope, and its URL routes.
            ("POST", "/grok/v1/responses", Grok, Grok, ModelCall, "/v1/responses"),
            ("POST", "/grok/v1/chat/completions", Grok, Grok, ModelCall, "/v1/chat/completions"),
            ("POST", "/grok/chat/completions", Grok, Grok, ModelCall, "/chat/completions"),
            ("POST", "/grok/https://cli-chat-proxy.grok.com/v1/responses", Grok, Grok, ModelCall, "/https://cli-chat-proxy.grok.com/v1/responses"),
            ("GET", "/grok/https://cli-chat-proxy.grok.com/v1/models", Grok, Grok, Other, "/https://cli-chat-proxy.grok.com/v1/models"),
            ("GET", "/grok/v1/settings", Grok, Grok, Other, "/v1/settings"),
            ("GET", "/grok/v1/billing?format=credits", Grok, Grok, Other, "/v1/billing?format=credits"),
            ("POST", "/grok/oauth2/token", Grok, Grok, Other, "/oauth2/token"),
            ("POST", "/https://grok.invalid/v1/chat/completions", Grok, Codex, ModelCall, "/https://grok.invalid/v1/chat/completions"),
            // Only the configuration decides unscoped URL routes; records logged
            // without it fall back to the proxy's default for those.
            ("GET", "/https://gateway.invalid/v1/models", Claude, Codex, Other, "/https://gateway.invalid/v1/models"),
            // Every scope takes a URL several Apps route.
            ("GET", "/codex/https://both.invalid/v1/models", Codex, Codex, Other, "/https://both.invalid/v1/models"),
            ("GET", "/anthropic/https://both.invalid/v1/models", Claude, Claude, Other, "/https://both.invalid/v1/models"),
            ("POST", "/codex/https://shared.invalid/v1/messages", Codex, Codex, ModelCall, "/https://shared.invalid/v1/messages"),
            ("POST", "/antigravity/https://shared.invalid/v1/messages", Antigravity, Antigravity, ModelCall, "/https://shared.invalid/v1/messages"),
            ("POST", "/grok/https://shared.invalid/v1/messages", Grok, Grok, ModelCall, "/https://shared.invalid/v1/messages"),
        ];
        for &(method, raw, app, logged, kind, rest) in cases {
            let target = Target::new(method, raw);
            assert_eq!(config.app(&target).unwrap(), app, "{method} {raw}");
            assert_eq!(logged_service(raw), logged, "{method} {raw}");
            assert_eq!(target.kind(), kind, "{method} {raw}");
            assert_eq!(target.rest(), rest, "{method} {raw}");
            assert_eq!(
                is_model_endpoint(method, raw),
                matches!(kind, ModelCall | ModelWebSocket),
                "{method} {raw}"
            );
        }
        // Unscoped, a URL several Apps route is ambiguous.
        for raw in [
            "/https://both.invalid/v1/models",
            // Codex, Antigravity and Grok all route shared.invalid/v1.
            "/https://shared.invalid/v1/messages",
        ] {
            let target = Target::new("POST", raw);
            assert_eq!(config.app(&target).unwrap_err().status, 409, "{raw}");
        }
    }

    #[test]
    fn a_named_upstream_gets_back_the_slash_agy_collapses() {
        for (sent, named) in [
            (
                "/https:/182.92.106.196:6060/antigravity/v1beta/models",
                "/https://182.92.106.196:6060/antigravity/v1beta/models",
            ),
            (
                "/http:/relay.example.com/v1beta/models",
                "/http://relay.example.com/v1beta/models",
            ),
            // Already whole, or naming no upstream: as sent.
            (
                "/https://relay.example.com/v1beta/models",
                "/https://relay.example.com/v1beta/models",
            ),
            (
                "/v1beta/models/g:streamGenerateContent",
                "/v1beta/models/g:streamGenerateContent",
            ),
            ("/https:relay", "/https:relay"),
        ] {
            assert_eq!(named_upstream_target(sent), named, "{sent}");
            assert_eq!(
                Target::new("GET", &format!("/antigravity{sent}")).rest(),
                named,
                "{sent}"
            );
            assert_eq!(
                Target::new("GET", &format!("/grok{sent}")).rest(),
                named,
                "{sent}"
            );
        }
    }
}
