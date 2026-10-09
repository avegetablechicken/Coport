use crate::{
    Error, Result,
    config::{Choice, Config, redacted_endpoint},
    logger::{Logger, RequestLog},
    routing::{
        MCP_PATH, MCP_UPSTREAM, TOKEN_REFRESH_UPSTREAM, account_query, query_url, refresh_token,
        token_refresh, upstream_url,
    },
};
use bytes::{Buf, Bytes};
use futures_util::StreamExt;
use http_body_util::{BodyExt, Full, Limited, StreamBody, combinators::UnsyncBoxBody};
use hyper::{
    HeaderMap, Request, Response,
    body::{Frame, Incoming},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::json;
use std::{
    collections::HashMap,
    convert::Infallible,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{net::TcpListener, sync::Semaphore};
use url::Url;

type Relay = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

/// A decrypted client connection; routing happens after local TLS termination.
pub trait ClientStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> ClientStream for T {}
pub type Connection = Box<dyn ClientStream>;
pub type ConnectionHandler = Arc<
    dyn Fn(
            Arc<Server>,
            Connection,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

pub type Body = UnsyncBoxBody<Bytes, std::io::Error>;
#[path = "relay.rs"]
mod relay;
#[path = "server_transport.rs"]
mod transport;
#[path = "websocket_relay.rs"]
mod websocket_relay;
use transport::*;

type AccountCheck = (Instant, Result<Option<String>>);

pub struct Server {
    pub config: Config,
    pub logger: Arc<Logger>,
    clients: Mutex<HashMap<String, reqwest::Client>>,
    probes: Mutex<HashMap<ProbeKey, ProbeState>>,
    // Timestamps track last use for bounded LRU eviction, not expiry.
    claude_profiles: Mutex<HashMap<String, (Instant, crate::claude::ClaudeIdentity)>>,
    // UI checks are serialized and cached briefly, including failures. The key
    // is the credential, so switching a saved login forces a new check.
    account_checks: tokio::sync::Mutex<HashMap<String, AccountCheck>>,
    tls: Option<tokio_rustls::TlsAcceptor>,
}
impl Server {
    pub fn new(config: Config, logger: Arc<Logger>) -> Self {
        Self {
            config,
            logger,
            clients: Mutex::new(HashMap::new()),
            probes: Mutex::new(HashMap::new()),
            claude_profiles: Mutex::new(HashMap::new()),
            account_checks: tokio::sync::Mutex::new(HashMap::new()),
            tls: None,
        }
    }
    /// Also accept TLS on the listening port; plain HTTP keeps working.
    pub fn with_tls(mut self, tls: tokio_rustls::TlsAcceptor) -> Self {
        self.tls = Some(tls);
        self
    }

    /// Resolve locally unknown account routes using the same authenticated
    /// profile probe as requests. Remote success retains its distinct state so
    /// the UI can keep warning that the local OAuth metadata did not match.
    pub async fn account_route_states(
        &self,
    ) -> [std::collections::BTreeMap<String, &'static str>; 2] {
        let mut states = self.config.account_route_states().await;
        if !states[1].values().any(|state| *state == "unknown") {
            return states;
        }
        let mut checks = self.account_checks.lock().await;
        checks.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(30));
        let mut confirmed = Vec::new();
        let mut failed = false;
        for (source, account) in self.config.claude.account_sources() {
            let matched_local = account.claude_identity().is_some_and(|identity| {
                crate::identity::routing_account_label(
                    &self.config.claude.routing,
                    &identity.account_id,
                    &identity.usernames,
                    &source,
                )
                .is_some()
            });
            if matched_local
                || self.config.claude.routing.account.contains_key(&source)
                || self.config.claude.routing.account_fallback.is_some()
            {
                continue;
            }
            let Ok(token) = account.claude_token().await else {
                continue;
            };
            let result = if let Some((_, result)) = checks.get(&token) {
                result.clone()
            } else {
                // A status refresh must revalidate upstream access, not merely
                // reuse the request router's identity cache indefinitely.
                if let Ok(mut profiles) = self.claude_profiles.lock() {
                    profiles.remove(&token);
                }
                let mut log = RequestLog {
                    logger: self.logger.clone(),
                    fields: json!({"service": "claude"}).as_object().unwrap().clone(),
                    started: Instant::now(),
                    status: 0,
                    bytes: 0,
                    outcome: "account_probe_failed",
                };
                let result = async {
                    let mut headers = HeaderMap::new();
                    headers.insert(
                        "authorization",
                        format!("Bearer {token}")
                            .parse()
                            .map_err(|_| Error::config("Invalid Claude account credential."))?,
                    );
                    let route = self.claude_route(&headers, None, &mut log).await?;
                    Ok(route.identity.as_ref().and_then(|identity| {
                        crate::identity::routing_account_label(
                            &self.config.claude.routing,
                            &identity.account_id,
                            &identity.usernames,
                            &source,
                        )
                    }))
                };
                let result = tokio::time::timeout(
                    Duration::from_secs_f64(self.config.request_timeout_seconds.min(10.0)),
                    result,
                )
                .await
                .unwrap_or_else(|_| Err(Error::config("Claude account probe timed out.")));
                match &result {
                    Ok(_) => log.outcome = "account_probe_finished",
                    Err(error) => log.field("reason", error.message),
                }
                if checks.len() >= 128 {
                    checks.clear();
                }
                checks.insert(token, (Instant::now(), result.clone()));
                result
            };
            match result {
                Ok(Some(label)) => confirmed.push(label),
                Ok(None) => {}
                Err(_) => failed = true,
            }
        }
        for state in states[1].values_mut() {
            if *state == "unknown" {
                *state = if failed { "probe_failed" } else { "inactive" };
            }
        }
        for label in confirmed {
            if let Some(state) = states[1].get_mut(&label) {
                if *state != "active" {
                    *state = "remote";
                }
            }
        }
        states
    }
    pub async fn traffic_credential_labels(
        &self,
    ) -> std::collections::BTreeMap<(String, String), String> {
        let mut labels = self.config.traffic_credential_labels().await;
        let Some(proxy) = self.config.claude.routing.account_probe.as_ref().or(self
            .config
            .claude
            .routing
            .account_fallback
            .as_ref())
        else {
            return labels;
        };
        for (source, account) in self.config.claude.account_sources() {
            if account.claude_identity().is_some_and(|identity| {
                labels.contains_key(&("Claude".into(), identity.account_id))
            }) {
                continue;
            }
            let Ok(token) = account.claude_token().await else {
                continue;
            };
            let mut checks = self.account_checks.lock().await;
            checks.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(30));
            if checks
                .get(&token)
                .is_some_and(|(_, result)| result.is_err())
            {
                continue;
            }
            if let Ok(mut profiles) = self.claude_profiles.lock() {
                if profiles
                    .get(&token)
                    .is_some_and(|(at, _)| at.elapsed() >= Duration::from_secs(300))
                {
                    profiles.remove(&token);
                }
            }
            let cached =
                self.claude_profiles.lock().ok().and_then(|profiles| {
                    profiles.get(&token).map(|(_, identity)| identity.clone())
                });
            if let Some(identity) = cached {
                if let Some(label) = crate::identity::routing_account_label(
                    &self.config.claude.routing,
                    &identity.account_id,
                    &identity.usernames,
                    &source,
                ) {
                    labels.insert(("Claude".into(), identity.account_id), label);
                }
                continue;
            }
            let mut log = RequestLog {
                logger: self.logger.clone(),
                fields: json!({"service":"claude"}).as_object().unwrap().clone(),
                started: Instant::now(),
                status: 0,
                bytes: 0,
                outcome: "account_probe_failed",
            };
            let result = tokio::time::timeout(
                Duration::from_secs_f64(self.config.request_timeout_seconds.min(10.0)),
                self.lookup_claude_identity(&token, proxy, &mut log),
            )
            .await
            .unwrap_or_else(|_| Err(Error::config("Claude account probe timed out.")));
            match result {
                Ok(identity) => {
                    log.outcome = "account_probe_finished";
                    let label = crate::identity::routing_account_label(
                        &self.config.claude.routing,
                        &identity.account_id,
                        &identity.usernames,
                        &source,
                    );
                    if let Some(label) = &label {
                        labels.insert(("Claude".into(), identity.account_id), label.clone());
                    }
                    if checks.len() >= 128 {
                        checks.clear();
                    }
                    checks.insert(token, (Instant::now(), Ok(label)));
                }
                Err(error) => {
                    log.field("reason", error.message);
                    if checks.len() >= 128 {
                        checks.clear();
                    }
                    checks.insert(token, (Instant::now(), Err(error)));
                }
            }
        }
        labels
    }
    async fn claude_route(
        &self,
        headers: &HeaderMap,
        target: Option<&str>,
        log: &mut RequestLog,
    ) -> Result<crate::claude::ClaudeRoute> {
        let mut route = self.config.claude.resolve(headers).await?;
        if let Some(target) = target {
            // Validate against the matched credential's upstream, before any
            // OAuth profile lookup. Named settings can override the global base.
            route.url(target)?;
        }
        if !route.needs_profile {
            return Ok(route);
        }
        let identity = self
            .lookup_claude_identity(&route.token, &route.proxy, log)
            .await?;
        self.config.claude.apply_profile(&mut route, identity)?;
        Ok(route)
    }
    async fn lookup_claude_identity(
        &self,
        token: &str,
        proxy: &Choice,
        log: &mut RequestLog,
    ) -> Result<crate::claude::ClaudeIdentity> {
        let cached = self
            .claude_profiles
            .lock()
            .map_err(|_| Error::config("Claude profile cache unavailable."))?
            .get_mut(token)
            .map(|(last_used, identity)| {
                *last_used = Instant::now();
                identity.clone()
            });
        let identity = if let Some(identity) = cached {
            identity
        } else {
            // Before the token's identity is known, account_probe (defaulting to
            // account_fallback) provides the configured lookup transport.
            // Never use a direct
            // or cross-account proxy inferred from an unverified identity.
            let url = self.config.claude.url("/api/oauth/profile")?;
            log.field("service", "claude");
            let request = self
                .client("none")?
                .get(url)
                .bearer_auth(token)
                .header("accept", "application/json")
                // The proxy reads this profile itself and does not decompress it.
                .header("accept-encoding", "identity")
                .header("anthropic-beta", "oauth-2025-04-20")
                .build()
                .map_err(|_| Error::config("Invalid Claude profile request."))?;
            let mut response = self
                .send_via(
                    proxy,
                    request,
                    false,
                    Deadline::new(self.config.request_timeout_seconds.min(10.0)),
                    log,
                )
                .await?;
            if let Some(proxy) = log.fields.get("proxy").cloned() {
                log.fields.insert("profile_proxy".into(), proxy);
            }
            if let Some(endpoint) = log.fields.get("proxy_endpoint").cloned() {
                log.fields.insert("profile_proxy_endpoint".into(), endpoint);
            }
            let status = response.status();
            if !status.is_success() {
                return Err(Error::new(
                    if status.is_client_error() {
                        status.as_u16()
                    } else {
                        502
                    },
                    "Claude profile lookup failed.",
                ));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| Error::config("Claude profile read failed."))?
            {
                if bytes.len() + chunk.len() > 65536 {
                    return Err(Error::config("Claude profile exceeds 64 KiB."));
                }
                bytes.extend_from_slice(&chunk);
            }
            let value = serde_json::from_slice(&bytes)
                .map_err(|_| Error::config("Invalid Claude profile JSON."))?;
            let identity = crate::claude::ClaudeIdentity::profile(&value)?;
            self.cache_claude_profile(token.to_owned(), identity.clone())?;
            identity
        };
        Ok(identity)
    }
    fn cache_claude_profile(
        &self,
        token: String,
        identity: crate::claude::ClaudeIdentity,
    ) -> Result<()> {
        let mut cache = self
            .claude_profiles
            .lock()
            .map_err(|_| Error::config("Claude profile cache unavailable."))?;
        if cache.len() >= 128 && !cache.contains_key(&token) {
            if let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, (last_used, _))| *last_used)
                .map(|(key, _)| key.clone())
            {
                cache.remove(&oldest);
            }
        }
        cache.insert(token, (Instant::now(), identity));
        Ok(())
    }
    pub async fn startup_log(&self) {
        for (label, source) in &self.config.codex.account_sources() {
            match self
                .config
                .codex
                .account_identity(source)
                .await
                .and_then(|i| self.config.account_choice(&i, Some(label)).map(|p| (i, p)))
            {
                Ok((i, p)) => self.logger.write(
                    "current_route",
                    json!({"service":"codex", "account_id":i.account_id,"proxy":p.label()})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
                Err(e) => self.logger.write(
                    "route_unavailable",
                    json!({"service":"codex", "reason":e.message})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            }
        }
        for p in &self.config.codex.providers {
            let event = if p
                .credentials(
                    &self.config.codex.base_url.api_key,
                    true,
                    &self.config.codex,
                )
                .await
                .is_ok()
            {
                "current_route"
            } else {
                "route_unavailable"
            };
            self.logger.write(
                event,
                json!({"provider":p.label(),"proxy":p.proxy.label()})
                    .as_object()
                    .unwrap()
                    .clone(),
            );
        }
        if !self.config.claude.account_sources().is_empty()
            || !self.config.claude.routing.api_key.is_empty()
        {
            let check = self.config.claude.check_credentials().await;
            let mut fields = json!({"service": "claude"}).as_object().unwrap().clone();
            if let Err(error) = &check {
                fields.insert("reason".into(), json!(error.message));
            }
            self.logger.write(
                if check.is_ok() {
                    "current_route"
                } else {
                    "route_unavailable"
                },
                fields,
            );
        }
    }
    async fn handle(
        self: Arc<Self>,
        mut incoming: Request<Incoming>,
        relay: tokio::sync::mpsc::UnboundedSender<Relay>,
    ) -> std::result::Result<Response<Body>, Infallible> {
        let target = incoming
            .uri()
            .path_and_query()
            .map(|v| v.as_str())
            .unwrap_or("/")
            .to_owned();
        if incoming.method() == "GET" && target == "/health" {
            return Ok(response(200, "{\"ok\":true}"));
        }
        let mut log=RequestLog { logger:self.logger.clone(), fields:json!({"request_id":uuid::Uuid::new_v4().to_string(),"method":incoming.method().as_str(),"path":target.split('?').next().unwrap_or("/")}).as_object().unwrap().clone(), started:Instant::now(),status:0,bytes:0,outcome:"request_cancelled" };
        if incoming.method() == "POST" && crate::model_calls::is_model_endpoint("POST", &target) {
            log.field("model_call_id", uuid::Uuid::new_v4());
            log.field("model_transport", "http");
        }
        crate::request_ids::headers(incoming.headers(), &mut log.fields);
        log.event("request_received");
        if incoming.method() == "CONNECT" {
            log.field("service", "connect");
            let upstream = self.connect(&incoming, &mut log).await;
            return Ok(match upstream {
                Ok(upstream) => {
                    log.status = 200;
                    let upgraded = hyper::upgrade::on(&mut incoming);
                    let _ = relay.send(relay_stream(
                        upgraded,
                        upstream,
                        log,
                        self.config.request_timeout_seconds,
                    ));
                    Response::builder().status(200).body(empty()).unwrap()
                }
                Err(error) => reject(&mut log, error),
            });
        }
        let ws_key = incoming.headers().get("sec-websocket-key").cloned();
        let upgrade = incoming
            .headers()
            .contains_key("upgrade")
            .then(|| hyper::upgrade::on(&mut incoming));
        match self.forward(incoming, &target, &mut log).await {
            Ok(upstream) => {
                if upstream.status() == 101 {
                    let Some((upgrade, key)) = upgrade.zip(ws_key) else {
                        return Ok(reject(
                            &mut log,
                            Error::config("Unexpected upstream protocol upgrade."),
                        ));
                    };
                    let accept = crate::tunnel::websocket_accept(key.as_bytes());
                    if !crate::tunnel::header_token(upstream.headers(), "connection", "upgrade")
                        || upstream
                            .headers()
                            .get("upgrade")
                            .is_none_or(|v| !v.as_bytes().eq_ignore_ascii_case(b"websocket"))
                        || upstream
                            .headers()
                            .get("sec-websocket-accept")
                            .is_none_or(|v| v.as_bytes() != accept.as_bytes())
                    {
                        return Ok(reject(
                            &mut log,
                            Error::config("Invalid upstream WebSocket handshake."),
                        ));
                    }
                    let mut headers = filtered_headers(upstream.headers());
                    headers.insert("connection", "Upgrade".parse().unwrap());
                    headers.insert("upgrade", "websocket".parse().unwrap());
                    match upstream.upgrade().await {
                        Ok(upstream) => {
                            log.status = 101;
                            log.field("status", 101);
                            if crate::model_calls::is_model_endpoint("GET", &target) {
                                log.field("model_transport", "websocket");
                                log.field(
                                    "websocket_extensions",
                                    headers
                                        .get("sec-websocket-extensions")
                                        .and_then(|v| v.to_str().ok())
                                        .unwrap_or(""),
                                );
                            }
                            log.event("upstream_response");
                            let _ = relay.send(relay_websocket_or_tunnel(
                                upgrade,
                                Box::new(upstream),
                                log,
                                self.config.request_timeout_seconds,
                                self.config.websocket.clone(),
                            ));
                            let mut response =
                                Response::builder().status(101).body(empty()).unwrap();
                            *response.headers_mut() = headers;
                            return Ok(response);
                        }
                        Err(_) => {
                            return Ok(reject(
                                &mut log,
                                Error::config("WebSocket upgrade failed."),
                            ));
                        }
                    }
                }
                let status = upstream.status();
                log.status = status.as_u16();
                log.field("status", status.as_u16());
                log.field("headers_ms", log.started.elapsed().as_millis());
                log.event("upstream_response");
                let headers = filtered_headers(upstream.headers());
                let mut observer = log.fields.contains_key("model_call_id").then(|| {
                    crate::model_calls::record_response_encoding(&mut log, &headers);
                    let header = |name| headers.get(name).and_then(|v| v.to_str().ok());
                    crate::model_calls::HttpObserver::new(
                        header("content-type").is_some_and(|v| {
                            v.trim()
                                .to_ascii_lowercase()
                                .starts_with("text/event-stream")
                        }),
                        header("content-encoding"),
                        &mut log,
                    )
                });
                let no_body =
                    log.fields["method"] == "HEAD" || matches!(status.as_u16(), 204 | 304);
                let mut response = Response::builder().status(status);
                *response.headers_mut().unwrap() = headers;
                response
                    .headers_mut()
                    .unwrap()
                    .insert("connection", "close".parse().unwrap());
                if no_body {
                    log.outcome = "request_finished";
                    return Ok(response.body(empty()).unwrap());
                }
                let mut stream = upstream.bytes_stream();
                let body = async_stream::stream! {
                    // Owning the upstream stream here propagates disconnect cancellation and backpressure.
                    while let Some(chunk)=stream.next().await {
                        match chunk {
                            Ok(bytes)=> {
                                log.bytes+=bytes.len();
                                if let Some(observer) = observer.as_mut() { observer.feed(&bytes, &mut log); }
                                yield Ok::<_,std::io::Error>(Frame::data(bytes));
                            }
                            Err(_)=> { log.outcome="request_failed"; log.field("reason","transport_error"); yield Err(std::io::Error::other("Upstream stream failed")); return; }
                        }
                    }
                    if let Some(observer) = observer.as_mut() { observer.finish(&mut log); }
                    log.outcome="request_finished";
                    drop(log);
                };
                Ok(response.body(StreamBody::new(body).boxed_unsync()).unwrap())
            }
            Err(e) => {
                log.status = e.status;
                log.outcome = "request_failed";
                if e.status < 500 {
                    log.outcome = "request_rejected";
                }
                log.field("reason", e.message);
                Ok(response(
                    e.status,
                    &json!({"error":{"message":e.message}}).to_string(),
                ))
            }
        }
    }
    async fn forward(
        &self,
        incoming: Request<Incoming>,
        target: &str,
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
        let websocket = crate::tunnel::websocket(&incoming)?;
        if incoming.headers().contains_key("expect") {
            return Err(Error::new(417, "Expect is unsupported."));
        }
        if incoming.method() == "CONNECT"
            || incoming.uri().scheme().is_some()
            || incoming.uri().authority().is_some()
        {
            return Err(Error::new(
                400,
                "Only origin-form HTTP request targets are supported.",
            ));
        }
        let codex_scoped = target.starts_with("/codex/https://");
        let claude_scoped = target.starts_with("/anthropic/") || target.starts_with("/claude/");
        let target = crate::routing::codex_target(target);
        let path = target.split('?').next().unwrap_or("");
        let docs = path == MCP_PATH;
        if !codex_scoped && crate::claude::token_refresh(target) {
            return self.forward_token_refresh(incoming, log, true).await;
        }
        if !claude_scoped && token_refresh(target) {
            return self.forward_token_refresh(incoming, log, false).await;
        }
        let explicit_api_route = if codex_scoped {
            None
        } else {
            self.config
                .claude
                .explicit_api_route(incoming.headers(), target)?
        };
        let codex_url_match = !claude_scoped
            && crate::url_routing::match_route(&self.config.codex.routing.api_key, target)?
                .is_some();
        if codex_url_match && explicit_api_route.is_some() {
            return Err(Error::new(
                409,
                "API upstream is configured for both apps; use /codex/https:// or /anthropic/https://.",
            ));
        }
        let explicit_codex_route = if codex_url_match {
            self.config.resolve_url(
                incoming
                    .headers()
                    .get("authorization")
                    .and_then(|h| h.to_str().ok()),
                target,
            )?
        } else {
            None
        };
        let claude_target = if codex_scoped || explicit_codex_route.is_some() {
            None
        } else {
            crate::claude::target(target).or_else(|| explicit_api_route.as_ref().map(|_| target))
        };
        let claude_route = if claude_target.is_some() {
            Some(if let Some(route) = explicit_api_route {
                route
            } else {
                self.claude_route(incoming.headers(), claude_target, log)
                    .await?
            })
        } else {
            None
        };
        let query = account_query(target);
        let auth = incoming
            .headers()
            .get("authorization")
            .and_then(|h| h.to_str().ok());
        let account = incoming
            .headers()
            .get("chatgpt-account-id")
            .and_then(|h| h.to_str().ok());
        let (route, choice) = if let Some(r) = &claude_route {
            log.field("service", "claude");
            log.field("provider", &r.label);
            log.field("upstream_base_url", &r.upstream);
            if r.identity.is_none() {
                if let Some(reference) =
                    crate::identity::shared_api_reference("Claude", &r.upstream, &r.token)
                {
                    log.field("credential_ref", reference);
                }
            }
            if let Some(identity) = &r.identity {
                log.field("account_id", &identity.account_id);
                if let Some(label) = crate::identity::routing_account_label(
                    &self.config.claude.routing,
                    &identity.account_id,
                    &identity.usernames,
                    &r.label,
                ) {
                    log.field("account_label", label);
                }
            }
            (None, r.proxy.clone())
        } else if docs {
            match self.config.resolve(auth, false).await {
                Ok(r) if account.is_none() || account == r.account_id.as_deref() => {
                    let p = r.proxy.clone();
                    (Some(r), p)
                }
                _ => (
                    None,
                    self.config
                        .codex
                        .routing
                        .mcp_fallback
                        .clone()
                        .unwrap_or_else(Choice::direct),
                ),
            }
        } else {
            let r = if let Some(route) = explicit_codex_route {
                log.field("service", "codex");
                route
            } else {
                self.config.resolve(auth, true).await?
            };
            if r.account_id.is_some() && account.is_some() && account != r.account_id.as_deref() {
                return Err(Error::new(
                    409,
                    "Account changed; retry with the current login.",
                ));
            }
            let p = r.proxy.clone();
            (Some(r), p)
        };
        if let Some(r) = &route {
            log.field("upstream_base_url", &r.upstream);
            if r.account_id.is_none() {
                if let Some(reference) =
                    crate::identity::shared_api_reference("Codex", &r.upstream, &r.token)
                {
                    log.field("credential_ref", reference);
                }
            }
            if let Some(label) = &r.account_label {
                log.field("account_label", label);
            }
            if let Some(id) = &r.account_id {
                log.field("account_id", id);
            }
            if let Some(p) = &r.provider {
                log.field("provider", p);
            }
        }
        if path.starts_with("/mcp/") && !docs {
            return Err(Error::new(404, "Unknown MCP endpoint."));
        }
        if docs && !matches!(incoming.method().as_str(), "GET" | "POST" | "DELETE") {
            return Err(Error::new(405, "MCP supports GET, POST and DELETE."));
        }
        let url = if let Some(claude_target) = claude_target {
            let url = claude_route.as_ref().unwrap().url(claude_target)?;
            let decoded_path = percent_encoding::percent_decode_str(url.path())
                .decode_utf8()
                .map_err(|_| Error::new(400, "Invalid Claude request path."))?;
            if decoded_path
                .trim_end_matches('/')
                .ends_with("/api/oauth/usage")
            {
                if !claude_route.as_ref().unwrap().matched_account {
                    return Err(Error::new(
                        403,
                        "Claude usage requires a matched OAuth account.",
                    ));
                }
                if incoming.method() != "GET" {
                    return Err(Error::new(405, "Claude usage supports GET only."));
                }
            }
            url
        } else if docs {
            if target.contains('#') {
                return Err(Error::config("Invalid MCP request target."));
            }
            log.field("service", "codex");
            log.field(
                "routing",
                if route.is_some() {
                    "credential"
                } else {
                    "mcp_fallback"
                },
            );
            Url::parse(&format!("{MCP_UPSTREAM}{}", &target[MCP_PATH.len()..]))
                .map_err(|_| Error::config("Invalid MCP request target."))?
        } else {
            let r = route.as_ref().unwrap();
            if let Some(method) = query {
                if r.account_id.is_none() {
                    return Err(Error::new(
                        403,
                        "Account usage queries require a matched ChatGPT login credential.",
                    ));
                }
                if incoming.method() != method {
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
                upstream_url(&r.upstream, target, r.account_id.is_some())?
            }
        };
        let (parts, body) = incoming.into_parts();
        let bytes = read_body(body).await?;
        if log.fields.contains_key("model_call_id") {
            crate::model_calls::observe_request(&bytes, log);
        }
        let native_tls = claude_route.as_ref().is_some_and(|r| r.custom_upstream)
            || route.as_ref().is_some_and(|r| r.custom_upstream);
        let mut headers = filtered_headers(&parts.headers);
        if docs {
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
        if let Some(r) = claude_route {
            r.headers(&mut headers)?;
        } else if !docs {
            let r = route.unwrap();
            headers.insert(
                "authorization",
                format!("Bearer {}", r.token)
                    .parse()
                    .map_err(|_| Error::new(401, "Invalid credential."))?,
            );
            if let Some(id) = r.account_id {
                headers.insert(
                    "chatgpt-account-id",
                    id.parse()
                        .map_err(|_| Error::new(401, "Invalid account ID."))?,
                );
            }
        }
        if websocket {
            headers.insert("connection", "Upgrade".parse().unwrap());
            headers.insert("upgrade", "websocket".parse().unwrap());
            for name in ["sec-websocket-key", "sec-websocket-version"] {
                headers.insert(name, parts.headers[name].clone());
            }
        }
        // The client's Accept-Encoding is forwarded, so responses arrive as the
        // client negotiated and are relayed byte for byte; model calls are
        // observed by decoding them as they stream through.
        let mut request = reqwest::Request::new(parts.method, url);
        *request.headers_mut() = headers;
        *request.body_mut() = Some(bytes.into());
        self.send_via(
            &choice,
            request,
            native_tls,
            Deadline::new(self.config.request_timeout_seconds),
            log,
        )
        .await
    }

    async fn connect(
        &self,
        incoming: &Request<Incoming>,
        log: &mut RequestLog,
    ) -> Result<crate::tunnel::Socket> {
        if incoming.uri().scheme().is_some()
            || incoming.uri().path_and_query().is_some()
            || incoming.headers().contains_key("transfer-encoding")
            || incoming
                .headers()
                .get("content-length")
                .is_some_and(|v| v != "0")
            || incoming.headers().contains_key("upgrade")
        {
            return Err(Error::new(400, "Invalid CONNECT request."));
        }
        let authority = incoming
            .uri()
            .authority()
            .ok_or(Error::new(400, "CONNECT requires host:port."))?;
        let (host, port) = crate::tunnel::authority(authority.as_str())?;
        let choice = self
            .config
            .connect
            .iter()
            .find_map(|(name, choice)| {
                (crate::tunnel::authority(name).ok() == Some((host.clone(), port)))
                    .then_some(choice)
            })
            .ok_or(Error::new(403, "CONNECT destination is not configured."))?;
        log.field("destination", format!("{host}:{port}"));
        let authority = if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        let destination = Url::parse(&format!("https://{authority}/"))
            .map_err(|_| Error::new(400, "Invalid CONNECT destination."))?;
        self.connect_via(
            choice,
            &destination,
            Deadline::new(self.config.request_timeout_seconds),
            log,
        )
        .await
    }

    async fn forward_token_refresh(
        &self,
        incoming: Request<Incoming>,
        log: &mut RequestLog,
        claude: bool,
    ) -> Result<reqwest::Response> {
        log.field("service", if claude { "claude_auth" } else { "codex_auth" });
        if incoming.method() != "POST" {
            return Err(Error::new(405, "Token refresh supports POST only."));
        }
        let (parts, body) = incoming.into_parts();
        let bytes = read_body(body).await?;
        let token = refresh_token(&bytes)
            .ok_or(Error::new(400, "Token refresh requires a refresh_token."))?;
        let (choice, account_id) = if claude {
            self.config.claude.resolve_refresh(&token).await?
        } else {
            self.config.resolve_refresh(&token).await?
        };
        if let Some(id) = &account_id {
            log.field("account_id", id);
        }
        let url = Url::parse(if claude {
            crate::claude::TOKEN_REFRESH_UPSTREAM
        } else {
            TOKEN_REFRESH_UPSTREAM
        })
        .unwrap();
        let mut request = reqwest::Request::new(parts.method, url);
        *request.headers_mut() = filtered_headers(&parts.headers);
        *request.body_mut() = Some(bytes.into());
        self.send_via(
            &choice,
            request,
            false,
            Deadline::new(self.config.request_timeout_seconds),
            log,
        )
        .await
    }

    pub async fn serve(
        self: Arc<Self>,
        listener: TcpListener,
        shutdown: impl std::future::Future<Output = ()>,
    ) -> std::io::Result<()> {
        self.serve_routed(listener, shutdown, None).await
    }

    /// Optional routing retains the same listener and local TLS identity.
    pub async fn serve_routed(
        self: Arc<Self>,
        listener: TcpListener,
        shutdown: impl std::future::Future<Output = ()>,
        handler: Option<ConnectionHandler>,
    ) -> std::io::Result<()> {
        let limit = Arc::new(Semaphore::new(128));
        let mut tasks = tokio::task::JoinSet::new();
        let monitor = self.monitor_probes();
        tokio::pin!(monitor);
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                _=&mut shutdown=>break,
                _=&mut monitor=>{},
                Some(_)=tasks.join_next(), if !tasks.is_empty()=>{},
                accepted=listener.accept()=> {
                    // One failed connection must not stop the listener; the pause
                    // keeps a shortage of file descriptors from spinning the loop.
                    let socket=match accepted {
                        Ok((socket,_))=>socket,
                        Err(_)=>{ tokio::time::sleep(Duration::from_millis(50)).await; continue; }
                    };
                    let Ok(permit)=limit.clone().try_acquire_owned() else { drop(socket); continue; };
                    // Fails on macOS for a peer that reset before it was accepted.
                    let _=socket.set_nodelay(true);
                    let server=self.clone();
                    let handler=handler.clone();
                tasks.spawn(async move {
                    let _permit=permit;
                    // A TLS ClientHello starts with the handshake record type 0x16;
                    // no HTTP method does, so both share the port.
                    let mut first=[0u8;1];
                    let tls=match (&server.tls, tokio::time::timeout(Duration::from_secs(30), socket.peek(&mut first)).await) {
                        (_, Ok(Ok(0))|Ok(Err(_))|Err(_))=>return,
                        (Some(tls), _) if first[0]==0x16=>Some(tls.clone()),
                        _=>None,
                    };
                    match tls {
                        Some(tls)=>if let Ok(Ok(socket))=tokio::time::timeout(Duration::from_secs(30), tls.accept(socket)).await {
                            server.dispatch_connection(Box::new(socket), handler).await;
                        },
                        None=>server.dispatch_connection(Box::new(socket), handler).await,
                    }
                    });
                }
            }
        }
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        Ok(())
    }
    async fn dispatch_connection(
        self: Arc<Self>,
        socket: Connection,
        handler: Option<ConnectionHandler>,
    ) {
        match handler {
            Some(handler) => handler(self, socket).await,
            None => self.connection(socket).await,
        }
    }

    pub async fn connection<S>(self: Arc<Self>, mut socket: S)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let prefix = match tokio::time::timeout(Duration::from_secs(30), read_head(&mut socket))
            .await
        {
            Ok(Ok(prefix)) => prefix,
            result => {
                let status = match result {
                    Ok(Err(e)) => e.status,
                    _ => 408,
                };
                use tokio::io::AsyncWriteExt;
                let _=socket.write_all(format!("HTTP/1.1 {status} Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n").as_bytes()).await;
                close_http(&mut socket).await;
                return;
            }
        };
        let (relay_tx, mut relay_rx) = tokio::sync::mpsc::unbounded_channel::<Relay>();
        // Keep Hyper's upgrade handshake intact. Disabling keep-alive
        // overwrites Connection: Upgrade with Connection: close.
        // Ordinary HTTP responses explicitly send Connection: close.
        let mut connection = http1::Builder::new()
            .max_buf_size(65536)
            .timer(TokioTimer::new())
            .header_read_timeout(Duration::from_secs(30))
            .serve_connection(
                TokioIo::new(PrefixedSocket { prefix, socket }),
                service_fn(move |r| self.clone().handle(r, relay_tx.clone())),
            )
            .with_upgrades();
        let _ = (&mut connection).await;
        if let Some(parts) = connection.into_parts() {
            close_http(&mut parts.io.into_inner()).await;
        }
        if let Ok(relay) = relay_rx.try_recv() {
            relay.await;
        }
    }
}

/// Send FIN before discarding unread input. Closing a socket with a rejected
/// request body still queued can reset TCP and erase the response at the peer.
/// Drain only briefly and within the inbound size bound; upgrades never enter here.
async fn close_http<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(socket: &mut S) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let _ = tokio::time::timeout(Duration::from_millis(250), async {
        socket.shutdown().await?;
        tokio::io::copy(
            &mut socket.take(32 * 1024 * 1024 + 65536),
            &mut tokio::io::sink(),
        )
        .await
    })
    .await;
}

fn reject(log: &mut RequestLog, error: Error) -> Response<Body> {
    log.status = error.status;
    log.outcome = "request_failed";
    if error.status < 500 {
        log.outcome = "request_rejected";
    }
    log.field("reason", error.message);
    response(
        error.status,
        &json!({"error":{"message":error.message}}).to_string(),
    )
}

fn relay_websocket_or_tunnel(
    upgrade: hyper::upgrade::OnUpgrade,
    upstream: crate::tunnel::Socket,
    mut log: RequestLog,
    seconds: f64,
    settings: crate::config::WebSocketTimeouts,
) -> Relay {
    if log
        .fields
        .get("model_transport")
        .and_then(serde_json::Value::as_str)
        != Some("websocket")
    {
        return relay_stream(upgrade, upstream, log, seconds);
    }
    Box::pin(async move {
        match tokio::time::timeout(Duration::from_secs_f64(seconds), upgrade).await {
            Ok(Ok(downstream)) => {
                websocket_relay::run(TokioIo::new(downstream), upstream, &mut log, &settings).await
            }
            result => {
                log.outcome = "request_failed";
                log.field(
                    "reason",
                    if result.is_err() {
                        "upgrade_timeout"
                    } else {
                        "upgrade_failed"
                    },
                );
            }
        }
    })
}

fn relay_stream(
    upgrade: hyper::upgrade::OnUpgrade,
    mut upstream: crate::tunnel::Socket,
    mut log: RequestLog,
    seconds: f64,
) -> Relay {
    Box::pin(async move {
        let idle = Duration::from_secs_f64(seconds);
        let downstream = match tokio::time::timeout(idle, upgrade).await {
            Ok(Ok(downstream)) => downstream,
            result => {
                log.outcome = "request_failed";
                log.field(
                    "reason",
                    if result.is_err() {
                        "upgrade_timeout"
                    } else {
                        "upgrade_failed"
                    },
                );
                return;
            }
        };
        let progress = Mutex::new(relay::Progress::new());
        let result =
            relay::copy_idle(TokioIo::new(downstream), &mut upstream, idle, &progress).await;
        let progress = progress.into_inner().unwrap();
        log.bytes = progress.received as usize;
        log.field("sent_bytes", progress.sent);
        match result {
            Ok(()) => log.outcome = "request_finished",
            Err(error) => {
                log.outcome = "request_failed";
                log.field(
                    "reason",
                    if progress.failure.is_none() && error.kind() == std::io::ErrorKind::TimedOut {
                        "tunnel_idle_timeout"
                    } else {
                        "tunnel_transport_error"
                    },
                );
                log.field("transport_error_kind", format!("{:?}", error.kind()));
                if let Some((side, operation, _)) = progress.failure {
                    log.field("error_side", side);
                    log.field("error_operation", operation);
                }
            }
        }
        drop(log);
    })
}

async fn read_body(body: Incoming) -> Result<Bytes> {
    Ok(tokio::time::timeout(
        Duration::from_secs(30),
        Limited::new(body, 32 * 1024 * 1024).collect(),
    )
    .await
    .map_err(|_| Error::new(408, "Request body read timed out."))?
    .map_err(|e| {
        if e.is::<http_body_util::LengthLimitError>() {
            Error::new(413, "Request body exceeds 32 MiB.")
        } else {
            Error::new(400, "Invalid HTTP request body.")
        }
    })?
    .to_bytes())
}
// Only known list-valued request fields may repeat. Authentication, routing,
// Host and message framing remain single-valued; unknown duplicates fail closed.
fn repeatable_request_header(name: &str) -> bool {
    matches!(
        name,
        "accept"
            | "accept-encoding"
            | "accept-language"
            | "cache-control"
            | "connection"
            | "pragma"
            | "via"
    )
}

// Reject ambiguity before Hyper normalizes duplicate Content-Length or TE+CL.
// Buffered bytes (including any body prefix) are then passed to Hyper unchanged.
async fn read_head(socket: &mut (impl tokio::io::AsyncRead + Unpin)) -> Result<Bytes> {
    use tokio::io::AsyncReadExt;
    let mut data = Vec::with_capacity(8192);
    loop {
        let mut chunk = [0; 8192];
        let n = socket
            .read(&mut chunk)
            .await
            .map_err(|_| Error::new(400, "Invalid request headers."))?;
        if n == 0 {
            return Err(Error::new(400, "Incomplete request headers."));
        }
        data.extend_from_slice(&chunk[..n]);
        if let Some(end) = data.windows(4).position(|s| s == b"\r\n\r\n") {
            if end + 4 > 65536 {
                return Err(Error::new(431, "Headers exceed 64 KiB."));
            }
            let head = std::str::from_utf8(&data[..end])
                .map_err(|_| Error::new(400, "Invalid request headers."))?;
            let mut names = std::collections::HashSet::new();
            for line in head.split("\r\n").skip(1) {
                let (name, value) = line
                    .split_once(':')
                    .ok_or(Error::new(400, "Invalid request header."))?;
                let name = name.to_ascii_lowercase();
                if !names.insert(name.clone()) && !repeatable_request_header(&name) {
                    return Err(Error::new(400, "Duplicate request header."));
                }
                if name == "transfer-encoding" && !value.trim().eq_ignore_ascii_case("chunked") {
                    return Err(Error::new(400, "Unsupported body framing."));
                }
                if name == "content-length" {
                    let v = value.trim();
                    if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
                        return Err(Error::new(400, "Invalid Content-Length."));
                    }
                    if v.parse::<u64>().map_or(true, |n| n > 32 * 1024 * 1024) {
                        return Err(Error::new(413, "Body exceeds 32 MiB."));
                    }
                }
            }
            if names.contains("transfer-encoding") && names.contains("content-length") {
                return Err(Error::new(400, "Ambiguous body framing."));
            }
            if names.contains("expect") {
                return Err(Error::new(417, "Expect is unsupported."));
            }
            return Ok(Bytes::from(data));
        }
        if data.len() > 65536 {
            return Err(Error::new(431, "Headers exceed 64 KiB."));
        }
    }
}
struct PrefixedSocket<S> {
    prefix: Bytes,
    socket: S,
}
impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for PrefixedSocket<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if !self.prefix.is_empty() {
            let n = buf.remaining().min(self.prefix.len());
            buf.put_slice(&self.prefix[..n]);
            self.prefix.advance(n);
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.socket).poll_read(cx, buf)
    }
}
impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for PrefixedSocket<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.socket).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.socket).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.socket).poll_shutdown(cx)
    }
}
pub fn filtered_headers(source: &HeaderMap) -> HeaderMap {
    let mut excluded: Vec<String> = [
        "host",
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "content-length",
        "authorization",
        "x-api-key",
        "api-key",
        "chatgpt-account-id",
        "cookie",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    for h in source.get_all("connection") {
        if let Ok(s) = h.to_str() {
            excluded.extend(s.split(',').map(|s| s.trim().to_ascii_lowercase()));
        }
    }
    let mut result = HeaderMap::new();
    for (n, v) in source {
        if !excluded.iter().any(|s| s == n.as_str()) {
            result.append(n.clone(), v.clone());
        }
    }
    result
}
fn empty() -> Body {
    Full::new(Bytes::new())
        .map_err(|e: Infallible| match e {})
        .boxed_unsync()
}
fn response(status: u16, text: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("connection", "close")
        .body(
            Full::new(Bytes::copy_from_slice(text.as_bytes()))
                .map_err(|e: Infallible| match e {})
                .boxed_unsync(),
        )
        .unwrap()
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
