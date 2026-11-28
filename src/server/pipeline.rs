//! The one forwarding pipeline every App's request goes through, and the
//! seam its adapters fill. The pipeline owns the order: plan the Route,
//! renew under the rotation lock, look accounts up after releasing it, record
//! the Route, build the upstream request, observe and send it, and retry once
//! with a renewed Saved login. Each App's adapter only answers what its Route
//! is.
use super::*;
use crate::antigravity::cooldown::Scope;
use crate::claude::ClaudeIdentity;
use auto_refresh::{Renewal, Snapshot};
use hyper::http::request::Parts;

/// What one App contributes to forwarding its requests.
pub(super) trait Adapter {
    /// The App's own relays the pipeline does not shape, such as its Token
    /// refresh.
    type Own;
    /// An account Route chosen from the request and local state alone.
    type Pending;
    /// A Route as final as its record, before its upstream request is built.
    type Route;

    /// What the App makes of the request before the account its credential
    /// names. May reject it, or rewrite its headers.
    async fn plan(
        server: &Server,
        parts: &mut Parts,
        target: &Target,
    ) -> Result<Plan<Self::Own, Self::Route>>;

    /// Relays a request `plan` gave to the App itself.
    async fn own(
        server: &Server,
        incoming: Request<Incoming>,
        own: Self::Own,
        log: &mut RequestLog,
    ) -> Result<reqwest::Response>;

    /// Whether a Model call routed by account may renew a Saved login.
    fn renews(server: &Server) -> bool;

    /// The account Route, with no remote request: the rotation lock may be
    /// held.
    async fn account(server: &Server, parts: &Parts, target: &Target) -> Result<Self::Pending>;

    /// The access token of the Saved login the Route may renew.
    fn saved_login(pending: &Self::Pending) -> Option<&str>;

    /// Finishes an account Route with any account lookup it needs, after the
    /// rotation lock is released.
    async fn complete(
        server: &Server,
        pending: Self::Pending,
        log: &mut RequestLog,
    ) -> Result<Self::Route>;

    /// The fields every record of the request carries for its Route.
    fn record(server: &Server, route: &Self::Route) -> RouteRecord;

    /// The upstream request's destination and credential. Endpoint checks
    /// made here reject a request whose record already names its Route.
    fn build(
        server: &Server,
        route: Self::Route,
        parts: &Parts,
        target: &Target,
        log: &mut RequestLog,
    ) -> Result<Relay>;
}

pub(super) enum Plan<O, R> {
    /// A request the App relays itself.
    Own(O),
    /// A Route that needs no account: an API key or URL route. It renews no
    /// Saved login.
    Route(R),
    /// A Route by the account the request's credential names.
    Account,
}

/// The Route fields of a request's records. Unset fields are not logged.
#[derive(Default)]
pub(super) struct RouteRecord {
    pub(super) provider: Option<String>,
    pub(super) upstream_base_url: Option<String>,
    pub(super) credential_ref: Option<String>,
    pub(super) account_id: Option<String>,
    pub(super) account_label: Option<String>,
    /// The credential is one the configuration holds or maps.
    pub(super) configured: bool,
}

impl RouteRecord {
    /// An account Route's fields, as every App records them, with the
    /// upstream its request goes to.
    pub(super) fn account<I: crate::routing::accounts::RouteIdentity, E>(
        route: &crate::routing::accounts::AccountRoute<I, E>,
        upstream: String,
    ) -> Self {
        Self {
            provider: route.provider.clone(),
            upstream_base_url: Some(upstream),
            account_id: route.account_id().map(str::to_owned),
            account_label: route.account_label.clone(),
            configured: route.configured,
            ..Default::default()
        }
    }

    /// An API Key Route's fields, as every App records them.
    pub(super) fn api<E>(app: &str, route: &crate::routing::api_keys::ApiRoute<E>) -> Self {
        Self {
            provider: Some(route.selector.clone()),
            upstream_base_url: Some(route.upstream.clone()),
            credential_ref: crate::identity::shared_api_reference(app, &route.upstream, &route.key),
            configured: route.configured,
            ..Default::default()
        }
    }

    pub(super) fn stamp(self, log: &mut RequestLog) {
        for (name, value) in [
            ("provider", self.provider),
            ("upstream_base_url", self.upstream_base_url),
            ("credential_ref", self.credential_ref),
            ("account_id", self.account_id),
            ("account_label", self.account_label),
        ] {
            if let Some(value) = value {
                log.field(name, value);
            }
        }
        log.field("credential_match", credential_match(self.configured));
    }
}

/// The upstream request a Route sends.
pub(super) struct Relay {
    pub(super) url: Url,
    pub(super) proxy: Choice,
    /// Sent with the platform's TLS, as to a custom upstream.
    pub(super) native_tls: bool,
    pub(super) credential: Credential,
    /// The login an Antigravity location cooldown pauses.
    pub(super) cooldown: Option<Scope>,
    /// Whether a Saved login snapshot taken for the Route still applies.
    pub(super) renews: bool,
    pub(super) claude_identity: Option<ClaudeIdentity>,
}

/// How the Route's credential replaces the client's.
pub(super) enum Credential {
    /// The client's own, as a Gemini API key it sent.
    AsSent,
    Bearer(String),
    /// Codex's: its token, and the ChatGPT account it names.
    Codex {
        token: String,
        account_id: Option<String>,
    },
    Claude(crate::claude::ClaudeRoute),
    /// OpenAI's docs MCP: no credential, and only these client headers.
    Docs,
}

impl Credential {
    fn apply(self, headers: &mut HeaderMap) -> Result<()> {
        let bearer = |token: &str| {
            format!("Bearer {token}")
                .parse()
                .map_err(|_| Error::new(401, "Invalid credential."))
        };
        match self {
            Self::AsSent => {}
            Self::Bearer(token) => {
                headers.insert("authorization", bearer(&token)?);
            }
            Self::Codex { token, account_id } => {
                headers.insert("authorization", bearer(&token)?);
                if let Some(id) = account_id {
                    headers.insert(
                        "chatgpt-account-id",
                        id.parse()
                            .map_err(|_| Error::new(401, "Invalid account ID."))?,
                    );
                }
            }
            Self::Claude(route) => route.headers(headers)?,
            Self::Docs => {
                let allowed = [
                    "accept",
                    "accept-encoding",
                    "content-type",
                    "mcp-session-id",
                    "mcp-protocol-version",
                    "last-event-id",
                ];
                let names: Vec<_> = headers
                    .keys()
                    .filter(|n| !allowed.contains(&n.as_str()))
                    .cloned()
                    .collect();
                for n in names {
                    headers.remove(n);
                }
            }
        }
        Ok(())
    }
}

impl Server {
    pub(super) async fn forward(
        &self,
        incoming: Request<Incoming>,
        target: &Target,
        app: Service,
        log: &mut RequestLog,
    ) -> Result<reqwest::Response> {
        if incoming.headers().keys().any(|name| {
            !repeatable_request_header(name.as_str())
                && incoming.headers().get_all(name).iter().count() > 1
        }) {
            return Err(Error::new(400, "Duplicate request header."));
        }
        if incoming
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .is_some_and(|n| n > 32 * 1024 * 1024)
        {
            return Err(Error::new(413, "Request body exceeds 32 MiB."));
        }
        let websocket = super::tunnel::websocket(&incoming)?;
        if incoming.headers().contains_key("expect") {
            return Err(Error::new(417, "Expect is unsupported."));
        }
        if incoming.method() == "CONNECT" {
            return Err(Error::new(400, "Invalid CONNECT request."));
        }
        match app {
            Service::Antigravity if websocket => {
                return Err(Error::new(400, "Antigravity does not use WebSockets."));
            }
            Service::Grok if websocket => {
                return Err(Error::new(400, "Grok does not use WebSockets."));
            }
            _ => {}
        }
        match app {
            Service::Codex => {
                self.pipeline::<Codex>(incoming, target, app, websocket, log)
                    .await
            }
            Service::Claude => {
                self.pipeline::<Claude>(incoming, target, app, websocket, log)
                    .await
            }
            Service::Antigravity => {
                self.pipeline::<Antigravity>(incoming, target, app, websocket, log)
                    .await
            }
            Service::Grok => {
                self.pipeline::<Grok>(incoming, target, app, websocket, log)
                    .await
            }
        }
    }

    async fn pipeline<A: Adapter>(
        &self,
        incoming: Request<Incoming>,
        target: &Target,
        app: Service,
        websocket: bool,
        log: &mut RequestLog,
    ) -> Result<reqwest::Response> {
        let (mut parts, body) = incoming.into_parts();
        let (route, snapshot) = match A::plan(self, &mut parts, target).await? {
            Plan::Own(own) => {
                return A::own(self, Request::from_parts(parts, body), own, log).await;
            }
            Plan::Route(route) => (route, None),
            Plan::Account => {
                // Only a Model call renews its Saved login; it routes under the
                // rotation lock so the token it is routed by stays current.
                let renews =
                    self.config.auto_refresh_access_token && target.model_call() && A::renews(self);
                let rotation_guard = if renews {
                    Some(self.auto_refresh.rotation.read().await)
                } else {
                    None
                };
                if renews {
                    if let Some(token) = parts
                        .headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.strip_prefix("Bearer "))
                    {
                        if let Some(current) = self.refreshed_alias(app, token).await {
                            parts.headers.insert(
                                "authorization",
                                format!("Bearer {current}").parse().unwrap(),
                            );
                        }
                    }
                }
                let pending = A::account(self, &parts, target).await?;
                let snapshot = match A::saved_login(&pending).filter(|_| renews) {
                    Some(token) => self.refresh_snapshot(app, token).await,
                    None => None,
                };
                // An account lookup is a remote request: a renewal waiting to
                // rotate a login never waits behind one.
                drop(rotation_guard);
                (A::complete(self, pending, log).await?, snapshot)
            }
        };
        A::record(self, &route).stamp(log);
        let relay = A::build(self, route, &parts, target, log)?;
        self.send(parts, body, target, app, relay, websocket, snapshot, log)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn send(
        &self,
        parts: Parts,
        body: Incoming,
        target: &Target,
        app: Service,
        relay: Relay,
        websocket: bool,
        snapshot: Option<Snapshot>,
        log: &mut RequestLog,
    ) -> Result<reqwest::Response> {
        let bytes = read_body(body).await?;
        // Drain the input before waiting so Hyper can observe a disconnected
        // client and cancel its pending call throughout the cooldown.
        self.location_cooldowns
            .wait(relay.cooldown.as_ref(), log)
            .await;
        if target.kind() == Kind::ModelCall {
            crate::observability::model_calls::observe_request(&bytes, log);
        }
        let mut headers = filtered_headers(&parts.headers);
        relay.credential.apply(&mut headers)?;
        if websocket {
            headers.insert("connection", "Upgrade".parse().unwrap());
            headers.insert("upgrade", "websocket".parse().unwrap());
            for name in ["sec-websocket-key", "sec-websocket-version"] {
                headers.insert(name, parts.headers[name].clone());
            }
        }
        // The client's Accept-Encoding is forwarded, so responses arrive as the
        // client negotiated and are relayed byte for byte; Model calls are
        // observed by decoding them as they stream through.
        let mut request = reqwest::Request::new(parts.method, relay.url);
        *request.headers_mut() = headers;
        *request.body_mut() = Some(bytes.into());
        let deadline = Deadline::new(self.config.request_timeout_seconds);
        let renewal = Renewal::new(
            snapshot.filter(|_| relay.renews),
            &request,
            relay.claude_identity,
        );
        let response = self
            .send_via(
                &relay.proxy,
                request,
                relay.native_tls,
                deadline,
                Upstream::relay(target),
                log,
            )
            .await?;
        let response = self
            .retry_after_renewal(
                response,
                renewal,
                &relay.proxy,
                relay.native_tls,
                deadline,
                log,
            )
            .await?;
        // Only Antigravity's errors are read before their headers: another
        // App's error message can echo the request.
        Ok(if app == Service::Antigravity {
            self.location_cooldowns.observe(relay.cooldown, response)
        } else {
            response
        })
    }
}

/// Where a Token refresh renews its login, and what its response teaches.
pub(super) struct TokenRoute<A: crate::refresh_routes::Account> {
    pub(super) url: Url,
    pub(super) proxy: Choice,
    /// The account learned from the token's response is kept here.
    pub(super) routes: Arc<crate::refresh_routes::RefreshRoutes<A>>,
    /// The account the refresh token was known to name.
    pub(super) previous: Option<A>,
    /// The refresh token sent, when the response may omit its successor.
    pub(super) sent: Option<String>,
    pub(super) observe: Option<ResponseObserver>,
}

impl Server {
    /// A Token refresh's parts and body: only POSTed ones are relayed.
    pub(super) async fn token_refresh_body(
        incoming: Request<Incoming>,
    ) -> Result<(hyper::http::request::Parts, Bytes)> {
        if incoming.method() != "POST" {
            return Err(Error::new(405, "Token refresh supports POST only."));
        }
        let (parts, body) = incoming.into_parts();
        Ok((parts, read_body(body).await?))
    }

    /// Relays a Token refresh unchanged to its App's token endpoint and, on
    /// success, learns which account its new refresh token belongs to.
    pub(super) async fn relay_token_refresh<A: crate::refresh_routes::Account>(
        &self,
        parts: hyper::http::request::Parts,
        bytes: Bytes,
        route: TokenRoute<A>,
        log: &mut RequestLog,
    ) -> Result<reqwest::Response> {
        let mut request = reqwest::Request::new(parts.method, route.url);
        *request.headers_mut() = filtered_headers(&parts.headers);
        // The proxy reads the response to learn its account and does not
        // decompress it.
        request
            .headers_mut()
            .insert("accept-encoding", "identity".parse().unwrap());
        *request.body_mut() = Some(bytes.into());
        let response = self
            .send_via(
                &route.proxy,
                request,
                false,
                Deadline::new(self.config.request_timeout_seconds),
                Upstream::Relay,
                log,
            )
            .await?;
        if !response.status().is_success() {
            return Ok(response);
        }
        Ok(self.learn_refresh_route(
            response,
            route.routes,
            route.previous,
            route.sent,
            route.observe,
        ))
    }
}

pub(super) struct Codex;
pub(super) struct Claude;
pub(super) struct Antigravity;
pub(super) struct Grok;

#[cfg(test)]
mod tests {
    use super::*;

    /// A key long and varied enough to be referenced across devices.
    const KEY: &str = "sk-0123456789abcdefghijklmnopqrstuvwxyz";

    fn server(yaml: &str) -> (Server, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::parse(&format!(
            "listen_port: 7889\nrequest_timeout_seconds: 3\n{yaml}"
        ))
        .unwrap();
        let logger = Arc::new(Logger::new(dir.path().join("proxy.log")));
        (Server::new(config, logger), dir)
    }

    /// The Relay and record an App's adapter gives a request it routes before
    /// any account: an API key or URL route.
    async fn planned<A: Adapter>(
        server: &Server,
        raw: &str,
        headers: &[(&str, &str)],
    ) -> (Relay, RouteRecord) {
        let target = Target::new("POST", raw);
        let mut request = Request::builder().method("POST").uri(raw);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let mut parts = request.body(()).unwrap().into_parts().0;
        let Ok(Plan::Route(route)) = A::plan(server, &mut parts, &target).await else {
            panic!("{raw} is not routed before its account");
        };
        let record = A::record(server, &route);
        let mut log = RequestLog::for_test(server.logger.clone(), 0);
        let relay = A::build(server, route, &parts, &target, &mut log).unwrap();
        (relay, record)
    }

    #[tokio::test]
    async fn codex_url_routes_relay_the_supplied_key_to_their_upstream() {
        let bearer = format!("Bearer {KEY}");
        let (server, _dir) =
            server("codex:\n  routing:\n    api_key:\n      'https://api.invalid/v1': none\n");
        for raw in [
            "/https://api.invalid/v1/responses",
            "/codex/https://api.invalid/v1/responses",
        ] {
            let (relay, record) =
                planned::<Codex>(&server, raw, &[("authorization", bearer.as_str())]).await;
            assert_eq!(relay.url.as_str(), "https://api.invalid/v1/responses");
            assert_eq!(relay.proxy.label(), "none");
            assert!(relay.native_tls && relay.cooldown.is_none());
            assert!(matches!(
                relay.credential,
                Credential::Codex { ref token, account_id: None } if token == KEY
            ));
            assert_eq!(
                record.upstream_base_url.as_deref(),
                Some("https://api.invalid/v1")
            );
            assert!(record.credential_ref.is_some() && record.account_id.is_none());
            assert!(!record.configured);
        }
    }

    #[tokio::test]
    async fn claude_url_routes_keep_the_api_key_and_renew_nothing() {
        let (server, _dir) = server(
            "claude:\n  config_dirs: []\n  routing:\n    api_key:\n      'https://gateway.invalid/v1': none\n",
        );
        let (relay, record) = planned::<Claude>(
            &server,
            "/anthropic/https://gateway.invalid/v1/messages",
            &[("x-api-key", KEY)],
        )
        .await;
        assert_eq!(relay.url.as_str(), "https://gateway.invalid/v1/messages");
        assert!(relay.native_tls && !relay.renews && relay.claude_identity.is_none());
        assert!(matches!(
            relay.credential,
            Credential::Claude(crate::claude::ClaudeRoute::Api(ref route))
                if route.key == KEY && route.ext == crate::claude::KeyHeader::ApiKey
        ));
        assert_eq!(
            record.provider.as_deref(),
            Some("https://gateway.invalid/v1")
        );
        assert!(record.credential_ref.is_some() && record.account_id.is_none());
    }

    #[tokio::test]
    async fn antigravity_api_keys_pass_as_sent_under_a_location_cooldown() {
        let (server, _dir) = server(
            "antigravity:\n  routing:\n    api_key:\n      relay.invalid/antigravity: none\n",
        );
        let (relay, record) = planned::<Antigravity>(
            &server,
            "/https://relay.invalid/antigravity/v1beta/models/g:generateContent",
            &[("x-goog-api-key", KEY)],
        )
        .await;
        assert_eq!(
            relay.url.as_str(),
            "https://relay.invalid/antigravity/v1beta/models/g:generateContent"
        );
        assert!(matches!(relay.credential, Credential::AsSent));
        assert!(relay.cooldown.is_some() && !relay.renews);
        assert_eq!(
            record.provider.as_deref(),
            Some("relay.invalid/antigravity")
        );
        assert!(record.credential_ref.is_some());
    }

    #[tokio::test]
    async fn grok_api_routes_relay_the_supplied_key() {
        let bearer = format!("Bearer {KEY}");
        let (server, _dir) =
            server("grok:\n  homes: []\n  routing:\n    api_key:\n      grok.invalid/v1: none\n");
        let (relay, record) = planned::<Grok>(
            &server,
            "/https://grok.invalid/v1/chat/completions",
            &[("authorization", bearer.as_str())],
        )
        .await;
        assert_eq!(
            relay.url.as_str(),
            "https://grok.invalid/v1/chat/completions"
        );
        assert!(matches!(relay.credential, Credential::Bearer(ref key) if key == KEY));
        assert!(!relay.renews && relay.cooldown.is_none());
        assert_eq!(record.provider.as_deref(), Some("grok.invalid/v1"));
        assert!(record.credential_ref.is_some());
    }
}
