use crate::{
    Error, Result,
    config::{Choice, Config, redacted_endpoint},
    logger::{Logger, Outcome, RequestLog},
    routing::{
        MCP_PATH, MCP_UPSTREAM, Service, TOKEN_REFRESH_UPSTREAM, account_query_for_route,
        query_url, refresh_token, upstream_url,
    },
    target::{Kind, Target},
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
/// A decrypted tunnel's CONNECT record, written once a request inside it is routed.
type TunnelLog = Arc<Mutex<Option<RequestLog>>>;

/// Logged on every routed request: whether its credential is one the
/// configuration holds or maps.
fn credential_match(configured: bool) -> &'static str {
    if configured {
        "configured"
    } else {
        "unmatched"
    }
}

/// Anything local may open a tunnel, so a CONNECT belongs to an app and
/// account only as the configured client its first routed request shows.
/// A tunnel whose first request is unmatched, or that closes before one,
/// is recorded without them.
fn record_tunnel(tunnel: &TunnelLog, request: &RequestLog) {
    let Some(mut connect) = tunnel.lock().ok().and_then(|mut slot| slot.take()) else {
        return;
    };
    if request
        .fields
        .get("credential_match")
        .and_then(serde_json::Value::as_str)
        != Some("configured")
    {
        return;
    }
    for key in [
        "service",
        "provider",
        "account_id",
        "account_label",
        "credential_ref",
        "upstream_base_url",
        "proxy",
        "proxy_endpoint",
    ] {
        if let Some(value) = request.fields.get(key) {
            connect.fields.insert(key.into(), value.clone());
        }
    }
}

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
mod credential_checks;
mod relay;
use crate::antigravity::cooldown;
pub use antigravity_forward::AntigravityProfiles;
pub use credential_checks::CredentialReport;
mod antigravity_forward;
mod claude_forward;
mod codex_forward;
mod grok_forward;
mod identity_directory;
mod pipeline;
mod transport;
mod websocket_relay;
use transport::*;
mod auto_refresh;

use identity_directory::Directory;
#[cfg(test)]
use identity_directory::FAILED_PROBE_RETRY;
pub use identity_directory::Profiles;
/// Claude accounts by OAuth access token, which servers rebuilt in one
/// process share.
pub type ClaudeProfiles = Profiles<crate::claude::ClaudeIdentity>;

pub struct Server {
    pub config: Config,
    pub logger: Arc<Logger>,
    clients: Mutex<HashMap<String, reqwest::Client>>,
    probes: Mutex<HashMap<ProbeKey, ProbeState>>,
    claude_identities: Directory<crate::claude::ClaudeIdentity>,
    antigravity_identities: Directory<crate::antigravity::AntigravityIdentity>,
    location_cooldowns: cooldown::LocationCooldowns,
    antigravity_refresh_routes:
        Arc<crate::refresh_routes::RefreshRoutes<crate::antigravity::AntigravityIdentity>>,
    antigravity_renewed: Arc<crate::antigravity::RenewedTokens>,
    refresh_routes: Arc<crate::refresh_routes::RefreshRoutes>,
    grok_routes: Arc<crate::refresh_routes::RefreshRoutes<crate::grok::GrokAccount>>,
    codex_refresh_routes: Arc<crate::refresh_routes::RefreshRoutes<crate::codex::Identity>>,
    auto_refresh: auto_refresh::RefreshState,
    credential_checks: tokio::sync::Mutex<Option<(tokio::time::Instant, Vec<CredentialReport>)>>,
    tls: Option<tokio_rustls::TlsAcceptor>,
    credential_proxy: Option<crate::credential_proxy::CredentialProxy>,
    /// The bound port, which absolute-form requests name to address this listener.
    local_port: std::sync::OnceLock<u16>,
    /// Client requests whose Request record has not ended.
    requests_in_flight: Arc<std::sync::atomic::AtomicUsize>,
}
impl Server {
    pub fn new(config: Config, logger: Arc<Logger>) -> Self {
        Self {
            config,
            logger,
            clients: Mutex::new(HashMap::new()),
            probes: Mutex::new(HashMap::new()),
            claude_identities: Default::default(),
            antigravity_identities: Default::default(),
            location_cooldowns: Default::default(),
            antigravity_refresh_routes: Default::default(),
            antigravity_renewed: Default::default(),
            grok_routes: Default::default(),
            refresh_routes: Default::default(),
            codex_refresh_routes: Default::default(),
            auto_refresh: Default::default(),
            credential_checks: Default::default(),
            tls: None,
            credential_proxy: None,
            local_port: std::sync::OnceLock::new(),
            requests_in_flight: Default::default(),
        }
    }
    /// Client requests received and not yet ended: a response still
    /// streaming and an open WebSocket each count until they close.
    /// Auxiliary requests are part of the request they serve.
    pub fn requests_in_flight(&self) -> usize {
        self.requests_in_flight
            .load(std::sync::atomic::Ordering::Acquire)
    }
    /// Also accept TLS on the listening port; plain HTTP keeps working.
    pub fn with_tls(mut self, tls: tokio_rustls::TlsAcceptor) -> Self {
        self.tls = Some(tls);
        self
    }
    /// Reuse identities already looked up by another server in this process.
    pub fn with_claude_profiles(mut self, profiles: ClaudeProfiles) -> Self {
        self.claude_identities.share(profiles);
        self
    }
    /// Keep the Claude refresh routes learned from token responses.
    pub fn with_refresh_routes(mut self, routes: crate::refresh_routes::RefreshRoutes) -> Self {
        self.refresh_routes = Arc::new(routes);
        self
    }
    /// Keep the Codex refresh routes learned from token responses.
    pub fn with_codex_refresh_routes(
        mut self,
        routes: crate::refresh_routes::RefreshRoutes<crate::codex::Identity>,
    ) -> Self {
        self.codex_refresh_routes = Arc::new(routes);
        self
    }
    /// Claude's refresh tokens and the accounts learned for them.
    pub(crate) fn claude_refresh_routes(&self) -> &crate::refresh_routes::RefreshRoutes {
        &self.refresh_routes
    }
    /// Antigravity's refresh tokens and the accounts learned for them.
    pub(crate) fn antigravity_refresh_routes(
        &self,
    ) -> &crate::refresh_routes::RefreshRoutes<crate::antigravity::AntigravityIdentity> {
        &self.antigravity_refresh_routes
    }
    /// Decrypt CONNECT tunnels to credential-routed API hosts.
    pub fn with_credential_proxy(
        mut self,
        proxy: crate::credential_proxy::CredentialProxy,
    ) -> Self {
        self.credential_proxy = Some(proxy);
        self
    }

    /// Route activation of each App's account routes, from local data and
    /// accounts that requests or Account discovery already looked up. Reading
    /// it never probes or runs a shell. A route only a lookup names stays
    /// `remote`, so the UI can warn that local OAuth metadata did not match.
    pub async fn account_route_states(
        &self,
    ) -> [std::collections::BTreeMap<String, &'static str>; 4] {
        use crate::route_activation::route_activation;
        let config = &self.config;
        // Only file-only Apps report activation, and only with account routes,
        // so no credential store is read otherwise.
        fn routes(
            reported: bool,
            routing: &crate::config::Routing,
        ) -> Option<impl Iterator<Item = &String>> {
            (reported && !routing.account.is_empty()).then_some(routing.account.keys())
        }
        let (codex, claude, antigravity, grok) = (
            &config.codex,
            &config.claude,
            &config.antigravity,
            &config.grok,
        );
        let mut states: [std::collections::BTreeMap<String, &'static str>; 4] = Default::default();
        if let Some(routes) = routes(codex.account_auth_file_only, &codex.routing) {
            states[0] = route_activation(routes, config.codex_logins().await);
        }
        if let Some(routes) = routes(claude.account_auth_file_only, &claude.routing) {
            states[1] = route_activation(routes, self.claude_logins().await);
        }
        if let Some(routes) = routes(antigravity.account_auth_file_only, &antigravity.routing) {
            states[2] = route_activation(routes, self.antigravity_logins().await);
        }
        if let Some(routes) = routes(grok.file_only(), &grok.routing) {
            states[3] = route_activation(routes, grok.route_logins());
        }
        states
    }
    /// Read-only display metadata: local configuration plus identities already
    /// observed by requests or background probes. Never probe here or run a shell.
    pub async fn cached_traffic_credential_labels(
        &self,
    ) -> std::collections::BTreeMap<(String, String), String> {
        let mut labels = self.config.local_traffic_credential_labels().await;
        self.cached_antigravity_labels(&mut labels);
        for identity in self.claude_identities.accounts() {
            if let Some(label) = crate::identity::routing_account_label(
                &self.config.claude.routing,
                &identity.account_id,
                &identity.usernames,
                "",
            ) {
                labels
                    .entry(("Claude".into(), identity.account_id))
                    .or_insert(label);
            }
        }
        labels
    }
    pub async fn traffic_credential_labels(
        &self,
    ) -> std::collections::BTreeMap<(String, String), String> {
        let mut labels = self.config.local_traffic_credential_labels().await;
        // Account discovery requires explicit consent to probing.
        // A payload fallback route is not authorization for a profile request.
        self.cached_antigravity_labels(&mut labels);
        self.discover_antigravity_label(&mut labels).await;
        let Some(proxy) = self.config.claude.routing.account_probe.as_ref() else {
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
            // Statistics reuse the request router's bounded identity cache.
            let Some(identity) = self
                .discover_account::<pipeline::Claude>(&token, proxy)
                .await
            else {
                continue;
            };
            if let Some(label) = crate::identity::routing_account_label(
                &self.config.claude.routing,
                &identity.account_id,
                &identity.usernames,
                &source,
            ) {
                labels.insert(("Claude".into(), identity.account_id), label);
            }
        }
        labels
    }
    pub async fn startup_log(&self) {
        if let Some(proxy) = &self.credential_proxy {
            self.logger.write(
                "credential_proxy",
                json!({
                    "destinations": proxy.authorities(),
                    "uncovered": proxy.uncovered(),
                    "ca_unrestricted": proxy.ca_unrestricted(),
                })
                .as_object()
                .unwrap()
                .clone(),
            );
        }
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
        // As Claude's: whether the saved login is in the OS credential store.
        let antigravity = &self.config.antigravity;
        if antigravity.enabled() && antigravity.saved_login {
            let check = antigravity.check_credentials().await;
            let mut fields = json!({"service": "antigravity"})
                .as_object()
                .unwrap()
                .clone();
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
        // As Codex's: whether a saved login is in a configured home.
        let grok = &self.config.grok;
        if grok.configured() {
            let check = grok.check_credentials().await;
            let mut fields = json!({"service": "grok"}).as_object().unwrap().clone();
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
        destination: Option<Arc<crate::credential_proxy::Destination>>,
        tunnel: Option<TunnelLog>,
    ) -> std::io::Result<Response<Body>> {
        let mut target = incoming
            .uri()
            .path_and_query()
            .map(|v| v.as_str())
            .unwrap_or("/")
            .to_owned();
        // Absolute-form targets (RFC 9112, section 3.2.2) are served when they
        // name this listener, or inside a decrypted CONNECT its destination;
        // proxy clients send them for loopback base URLs. Other absolute-form
        // targets would be plain HTTP forwarding, which is not offered.
        let connect = incoming.method() == "CONNECT";
        let origin_form = incoming.uri().scheme().is_none() && incoming.uri().authority().is_none();
        let addressed_here =
            !origin_form && !connect && self.addressed_here(incoming.uri(), destination.as_deref());
        let valid = match &destination {
            // Inside a decrypted CONNECT, targets take the explicit upstream form.
            Some(_) => !connect && (origin_form && target.starts_with('/') || addressed_here),
            None => connect || origin_form || addressed_here,
        };
        if let Some(destination) = destination.as_ref().filter(|_| valid) {
            target = format!("{}{target}", destination.scope);
        }
        if valid && destination.is_none() && incoming.method() == "GET" && target == "/health" {
            return Ok(response(200, "{\"ok\":true}"));
        }
        // A CONNECT target is its authority (host:port); one carrying user
        // information is not logged.
        let logged_path = if connect {
            incoming
                .uri()
                .authority()
                .map(|a| a.as_str())
                .filter(|a| !a.contains('@'))
                .unwrap_or("/")
        } else {
            target.split('?').next().unwrap_or("/")
        };
        let mut log =
            RequestLog::received(self.logger.clone(), incoming.method().as_str(), logged_path);
        log.in_flight(&self.requests_in_flight);
        let head = incoming.method() == hyper::Method::HEAD;
        // What the request is needs no configuration, so even a request
        // refused below is recorded as the Model call it asked for.
        let target = crate::target::Target::new(incoming.method().as_str(), &target);
        if target.kind() == Kind::ModelCall {
            log.model_call(uuid::Uuid::new_v4());
            log.field("model_transport", "http");
        }
        crate::request_ids::headers(incoming.headers(), &mut log.fields);
        if let Some(destination) = &destination {
            log.field(
                "client_connect",
                destination.scope.rsplit("://").next().unwrap_or(""),
            );
        }
        // The app is decided once, before any record: routing follows it and
        // every record logs it. A CONNECT takes the app of the first request
        // in its tunnel; requests this listener does not serve have none.
        let service = (valid && !connect).then(|| self.config.app(&target));
        if let Some(Ok(service)) = &service {
            log.field("service", service.name());
        }
        log.event("request_received");
        if !valid {
            return Ok(reject(
                &mut log,
                Error::new(
                    400,
                    if destination.is_some() {
                        "Requests in a decrypted tunnel must target its destination."
                    } else if self.credential_proxy.is_none() {
                        "Absolute-form requests require proxy port credential routing; use origin-form base URLs."
                    } else {
                        "Plain HTTP proxy requests are only served for this listener; use CONNECT for other destinations."
                    },
                ),
            ));
        }
        if connect {
            return Ok(match self.connect(&incoming, &mut log) {
                Ok(destination) => {
                    log.status = 200;
                    log.field("routing", "credential");
                    let upgraded = hyper::upgrade::on(&mut incoming);
                    let _ = relay.send(self.clone().decrypt(upgraded, destination, log));
                    Response::builder().status(200).body(empty()).unwrap()
                }
                Err(error) => reject(&mut log, error),
            });
        }
        let service = match service {
            Some(Ok(service)) => service,
            Some(Err(error)) => return Ok(reject(&mut log, error)),
            None => unreachable!("valid requests other than CONNECT have an app"),
        };
        let relayed = Upstream::relay(&target);
        let ws_key = incoming.headers().get("sec-websocket-key").cloned();
        let upgrade = incoming
            .headers()
            .contains_key("upgrade")
            .then(|| hyper::upgrade::on(&mut incoming));
        let forwarded = self.forward(incoming, &target, service, &mut log).await;
        if let Some(tunnel) = &tunnel {
            record_tunnel(tunnel, &log);
        }
        match forwarded {
            Ok(mut upstream) => {
                if let Some(inspect) = upstream.extensions_mut().remove::<cooldown::Inspect>() {
                    let status = upstream.status().as_u16();
                    let Some(response) = self
                        .location_cooldowns
                        .inspect(inspect, upstream, &mut log)
                        .await
                    else {
                        // The upstream answered. Closing the client connection
                        // does not drop that response from the log.
                        record_upstream_response(&mut log, status, relayed);
                        log.status = 0;
                        log.fields.remove("status");
                        log.fail("antigravity_location_cooldown");
                        // A service error closes HTTP/1 before it sends any response headers.
                        return Err(std::io::Error::other("Antigravity location cooldown"));
                    };
                    upstream = response;
                }
                if upstream.status() == 101 {
                    let Some((upgrade, key)) = upgrade.zip(ws_key) else {
                        record_upstream_response(&mut log, 101, relayed);
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
                        record_upstream_response(&mut log, 101, relayed);
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
                            if target.kind() == Kind::ModelWebSocket {
                                log.field("model_transport", "websocket");
                                log.field(
                                    "websocket_extensions",
                                    headers
                                        .get("sec-websocket-extensions")
                                        .and_then(|v| v.to_str().ok())
                                        .unwrap_or(""),
                                );
                            }
                            record_upstream_response(&mut log, 101, relayed);
                            let _ = relay.send(relay_websocket_or_tunnel(
                                upgrade,
                                Box::new(upstream),
                                target.kind() == Kind::ModelWebSocket,
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
                            record_upstream_response(&mut log, 101, relayed);
                            return Ok(reject(
                                &mut log,
                                Error::config("WebSocket upgrade failed."),
                            ));
                        }
                    }
                }
                let status = upstream.status();
                record_upstream_response(&mut log, status.as_u16(), relayed);
                let headers = filtered_headers(upstream.headers());
                let mut observer = (target.kind() == Kind::ModelCall).then(|| {
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
                let no_body = head || matches!(status.as_u16(), 204 | 304);
                let mut response = Response::builder().status(status);
                *response.headers_mut().unwrap() = headers;
                if no_body {
                    log.finish();
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
                            Err(_)=> { log.fail("transport_error"); yield Err(std::io::Error::other("Upstream stream failed")); return; }
                        }
                    }
                    if let Some(observer) = observer.as_mut() { observer.finish(&mut log); }
                    log.finish();
                    drop(log);
                };
                Ok(response.body(StreamBody::new(body).boxed_unsync()).unwrap())
            }
            Err(e) => {
                log.refuse(&e);
                Ok(response(
                    e.status,
                    &json!({"error":{"message":e.message}}).to_string(),
                ))
            }
        }
    }
    /// CONNECT is served only for destinations decrypted and routed by credential.
    fn connect(
        &self,
        incoming: &Request<Incoming>,
        log: &mut RequestLog,
    ) -> Result<Arc<crate::credential_proxy::Destination>> {
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
        // Logged before routing, so a denied destination is visible too.
        log.field("destination", format!("{host}:{port}"));
        let Some(proxy) = &self.credential_proxy else {
            return Err(Error::new(
                403,
                "CONNECT requires proxy port credential routing.",
            ));
        };
        proxy.destination(&host, port).ok_or_else(|| {
            Error::new(
                403,
                if proxy.is_uncovered(&host, port) {
                    "The proxy CA does not cover this destination; use its base URL or replace the CA."
                } else {
                    "CONNECT is served only for API hosts routed by credential."
                },
            )
        })
    }

    /// Whether an absolute-form target names this listener on a loopback
    /// address or, inside a decrypted CONNECT, that tunnel's destination.
    /// Both apply only while the listener serves as a proxy port.
    fn addressed_here(
        &self,
        uri: &hyper::Uri,
        destination: Option<&crate::credential_proxy::Destination>,
    ) -> bool {
        let (Some(scheme), Some(authority)) = (uri.scheme_str(), uri.authority()) else {
            return false;
        };
        if self.credential_proxy.is_none() {
            return false;
        }
        let default_port = if scheme.eq_ignore_ascii_case("https") {
            443
        } else if scheme.eq_ignore_ascii_case("http") {
            80
        } else {
            return false;
        };
        if authority.as_str().contains('@') {
            return false;
        }
        let host = authority
            .host()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase();
        let port = authority.port_u16().unwrap_or(default_port);
        match destination {
            Some(d) => default_port == 443 && host == d.host && port == d.port,
            None => {
                port == self
                    .local_port
                    .get()
                    .copied()
                    .unwrap_or(self.config.listen_port)
                    && (host == "localhost"
                        || host
                            .parse::<std::net::IpAddr>()
                            .is_ok_and(|ip| ip.is_loopback()))
            }
        }
    }

    /// Terminate the client's TLS inside an accepted CONNECT with the proxy
    /// CA, then serve its requests under the destination's explicit scope.
    fn decrypt(
        self: Arc<Self>,
        upgrade: hyper::upgrade::OnUpgrade,
        destination: Arc<crate::credential_proxy::Destination>,
        mut log: RequestLog,
    ) -> Relay {
        Box::pin(async move {
            let handshake = async {
                let downstream = upgrade.await.map_err(|_| "upgrade_failed")?;
                destination
                    .acceptor
                    .accept(TokioIo::new(downstream))
                    .await
                    .map_err(|error| client_tls_failure(&error))
            };
            match tokio::time::timeout(Duration::from_secs(30), handshake).await {
                Ok(Ok(tls)) => {
                    log.finish();
                    self.serve_http(tls, Some(destination), Some(log)).await;
                }
                Ok(Err(reason)) => log.fail(reason),
                Err(_) => log.fail("client_tls_timeout"),
            }
        })
    }

    /// Relays a successful token response unchanged and, once it is
    /// complete, records which account its new refresh token belongs to and
    /// passes the whole response to `observe`.
    pub(super) fn learn_refresh_route<A: crate::refresh_routes::Account>(
        &self,
        upstream: reqwest::Response,
        routes: Arc<crate::refresh_routes::RefreshRoutes<A>>,
        previous: Option<A>,
        sent: Option<String>,
        observe: Option<ResponseObserver>,
    ) -> reqwest::Response {
        const LIMIT: usize = 65536;
        let mut response = hyper::Response::builder()
            .status(upstream.status())
            .version(upstream.version());
        *response.headers_mut().unwrap() = upstream.headers().clone();
        let encoded = upstream
            .headers()
            .get("content-encoding")
            .is_some_and(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity"));
        let logger = self.logger.clone();
        let mut stream = upstream.bytes_stream();
        let body = async_stream::stream! {
            let mut seen = Vec::new();
            let mut whole = !encoded;
            while let Some(chunk) = stream.next().await {
                let Ok(bytes) = chunk else {
                    yield chunk;
                    return;
                };
                if whole && seen.len() + bytes.len() <= LIMIT {
                    seen.extend_from_slice(&bytes);
                } else {
                    whole = false;
                }
                yield Ok(bytes);
            }
            if whole {
                if let Some(observe) = observe {
                    observe(&seen);
                }
                if let Err(error) =
                    routes.learn_response_for(&seen, sent.as_deref(), previous.as_ref())
                {
                    logger.write(
                        "refresh_route_unsaved",
                        [("reason".to_owned(), error.to_string().into())]
                            .into_iter()
                            .collect(),
                    );
                }
            }
        };
        reqwest::Response::from(response.body(reqwest::Body::wrap_stream(body)).unwrap())
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
        if let Ok(address) = listener.local_addr() {
            let _ = self.local_port.set(address.port());
        }
        let limit = Arc::new(Semaphore::new(128));
        let mut tasks = tokio::task::JoinSet::new();
        let monitor = self.monitor_probes();
        tokio::pin!(monitor);
        let credentials = self.monitor_credentials();
        tokio::pin!(credentials);
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                _=&mut shutdown=>break,
                _=&mut monitor=>{},
                _=&mut credentials=>{},
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

    pub async fn connection<S>(self: Arc<Self>, socket: S)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        self.serve_http(socket, None, None).await
    }

    /// Boxed, since a decrypted CONNECT serves its own connection from a relay.
    fn serve_http<S>(
        self: Arc<Self>,
        mut socket: S,
        destination: Option<Arc<crate::credential_proxy::Destination>>,
        tunnel: Option<RequestLog>,
    ) -> Relay
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        // Dropping the last reference writes a CONNECT no request claimed.
        let tunnel: Option<TunnelLog> = tunnel.map(|log| Arc::new(Mutex::new(Some(log))));
        Box::pin(async move {
            let prefix = match tokio::time::timeout(Duration::from_secs(30), read_head(&mut socket))
                .await
            {
                Ok(Ok(prefix)) => prefix,
                result => {
                    let (status, reason) = match result {
                        Ok(Err(e)) => (e.status, e.message),
                        _ => (408, "Request headers timed out."),
                    };
                    // A decrypted tunnel was opened for its requests, so a
                    // missing or invalid one is recorded rather than dropped.
                    if let Some(destination) = &destination {
                        self.logger.write(
                            "request_rejected",
                            json!({
                                "client_connect": destination.scope.rsplit("://").next().unwrap_or(""),
                                "status": status.to_string(),
                                "reason": reason,
                            })
                            .as_object()
                            .unwrap()
                            .clone(),
                        );
                    }
                    use tokio::io::AsyncWriteExt;
                    let _=socket.write_all(format!("HTTP/1.1 {status} Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n").as_bytes()).await;
                    close_http(&mut socket).await;
                    return;
                }
            };
            let (relay_tx, mut relay_rx) = tokio::sync::mpsc::unbounded_channel::<Relay>();
            // Forwarded responses keep the connection for further requests,
            // which the header read timeout closes once idle; local error
            // responses close it. A stream failing midway aborts the
            // connection, so the client never takes a truncated body as whole.
            let mut connection = http1::Builder::new()
                .max_buf_size(65536)
                .timer(TokioTimer::new())
                .header_read_timeout(Duration::from_secs(30))
                .serve_connection(
                    TokioIo::new(PrefixedSocket { prefix, socket }),
                    service_fn(move |r| {
                        self.clone().handle(
                            r,
                            relay_tx.clone(),
                            destination.clone(),
                            tunnel.clone(),
                        )
                    }),
                )
                .with_upgrades();
            let _ = (&mut connection).await;
            if let Some(parts) = connection.into_parts() {
                close_http(&mut parts.io.into_inner()).await;
            }
            if let Ok(relay) = relay_rx.try_recv() {
                relay.await;
            }
        })
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

/// Why a client's TLS handshake in a decrypted CONNECT failed. A client that
/// does not trust the proxy CA rejects its certificate with an alert; one
/// still trusting a replaced CA of the same name reports a bad signature.
fn client_tls_failure(error: &std::io::Error) -> &'static str {
    use tokio_rustls::rustls::{AlertDescription, Error as TlsError};
    match error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<TlsError>())
    {
        Some(TlsError::AlertReceived(
            AlertDescription::UnknownCA
            | AlertDescription::BadCertificate
            | AlertDescription::CertificateUnknown
            | AlertDescription::UnsupportedCertificate
            | AlertDescription::DecryptError,
        )) => "client_rejected_proxy_ca",
        _ => "client_tls_failed",
    }
}

/// An upstream HTTP response, relayed or not. The client outcome is recorded
/// separately when this request ends. An Auxiliary request's response, made
/// on a Model call's log, is not that Model call's.
fn record_upstream_response(log: &mut RequestLog, status: u16, upstream: Upstream) {
    log.status = status;
    log.field("status", status);
    log.field("headers_ms", log.started.elapsed().as_millis());
    if upstream == Upstream::Auxiliary {
        log.auxiliary_event("upstream_response");
    } else {
        log.event("upstream_response");
    }
}

fn reject(log: &mut RequestLog, error: Error) -> Response<Body> {
    log.refuse(&error);
    response(
        error.status,
        &json!({"error":{"message":error.message}}).to_string(),
    )
}

/// A Responses WebSocket (`model_websocket`) is relayed observing its
/// Model calls; any other upgrade, byte for byte.
fn relay_websocket_or_tunnel(
    upgrade: hyper::upgrade::OnUpgrade,
    upstream: crate::tunnel::Socket,
    model_websocket: bool,
    mut log: RequestLog,
    seconds: f64,
    settings: crate::config::WebSocketTimeouts,
) -> Relay {
    if !model_websocket {
        return relay_stream(upgrade, upstream, log, seconds);
    }
    Box::pin(async move {
        match tokio::time::timeout(Duration::from_secs_f64(seconds), upgrade).await {
            Ok(Ok(downstream)) => {
                websocket_relay::run(TokioIo::new(downstream), upstream, &mut log, &settings).await
            }
            result => {
                log.fail(if result.is_err() {
                    "upgrade_timeout"
                } else {
                    "upgrade_failed"
                });
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
                log.fail(if result.is_err() {
                    "upgrade_timeout"
                } else {
                    "upgrade_failed"
                });
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
            Ok(()) => log.finish(),
            Err(error) => {
                log.fail(
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
/// Sees a whole token response once it has been relayed.
type ResponseObserver = Box<dyn FnOnce(&[u8]) + Send>;
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
mod tests;
