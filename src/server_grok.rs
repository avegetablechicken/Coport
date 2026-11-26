use super::pipeline::{Adapter, Credential, Plan, Relay, RouteRecord};
use super::*;
use crate::{
    grok::{ApiRoute, Endpoint, GrokAccount, GrokRoute, GrokTarget},
    refresh_routes::RefreshRoutes,
};

impl Server {
    pub fn with_grok_routes(mut self, routes: RefreshRoutes<GrokAccount>) -> Self {
        self.grok_routes = Arc::new(routes);
        self
    }

    async fn forward_grok_subscription(
        &self,
        incoming: Request<Incoming>,
        url: Url,
        endpoint: Endpoint,
        log: &mut RequestLog,
    ) -> Result<reqwest::Response> {
        let settings = &self.config.grok;
        if !settings.enabled() {
            return Err(Error::new(404, "Grok account routing is not configured."));
        }
        let post = matches!(endpoint, Endpoint::Token | Endpoint::DeviceCode);
        if incoming.method() != if post { "POST" } else { "GET" } {
            return Err(Error::new(
                405,
                "Unsupported method for this Grok endpoint.",
            ));
        }
        // Discovery is answered for the listener the request reached.
        let listener = (endpoint == Endpoint::Discovery)
            .then(|| local_base(incoming.headers()))
            .flatten();
        let (parts, body) = incoming.into_parts();
        let bytes = read_body(body).await?;
        let mut headers = filtered_headers(&parts.headers);
        if post && !form_body(&headers) {
            return Err(Error::new(
                415,
                "Grok OAuth requests require a URL-encoded form.",
            ));
        }
        // What the request carries: a renewal's refresh token, a usage
        // read's access token, or nothing a sign-in can be routed by.
        let mut token = None;
        let mut sign_in = false;
        match endpoint {
            Endpoint::Token => {
                // Deserialize once; serde rejects duplicate credential or grant fields.
                #[derive(serde::Deserialize)]
                struct Grant {
                    client_id: String,
                    grant_type: String,
                    refresh_token: Option<String>,
                    device_code: Option<String>,
                }
                let grant: Grant = serde_urlencoded::from_bytes(&bytes)
                    .map_err(|_| Error::new(400, "Invalid Grok token form."))?;
                if grant.client_id != crate::grok::CLIENT_ID {
                    return Err(Error::new(
                        403,
                        "Only Grok Build token requests are routed.",
                    ));
                }
                match (
                    grant.grant_type.as_str(),
                    grant.refresh_token,
                    grant.device_code,
                ) {
                    ("refresh_token", Some(refresh), None) => {
                        if !crate::identity::valid_token(&refresh) {
                            return Err(Error::new(400, "Invalid Grok refresh token."));
                        }
                        token = Some(refresh);
                    }
                    (DEVICE_GRANT, None, Some(code)) if crate::identity::valid_token(&code) => {
                        sign_in = true;
                    }
                    _ => {
                        return Err(Error::new(
                            403,
                            "Only Grok Build token renewal and device sign-in are routed.",
                        ));
                    }
                }
                headers.insert("accept-encoding", "identity".parse().unwrap());
            }
            Endpoint::DeviceCode => {
                #[derive(serde::Deserialize)]
                struct Request {
                    client_id: String,
                }
                let request: Request = serde_urlencoded::from_bytes(&bytes)
                    .map_err(|_| Error::new(400, "Invalid Grok device code form."))?;
                if request.client_id != crate::grok::CLIENT_ID {
                    return Err(Error::new(403, "Only Grok Build sign-in is routed."));
                }
            }
            Endpoint::Discovery => {
                headers.insert("accept-encoding", "identity".parse().unwrap());
            }
            Endpoint::Usage => {
                let auth = parts
                    .headers
                    .get("authorization")
                    .ok_or(Error::new(401, "Grok requires a Bearer token."))?;
                let access = auth
                    .to_str()
                    .ok()
                    .and_then(|v| v.strip_prefix("Bearer "))
                    .filter(|s| crate::identity::valid_token(s))
                    .ok_or(Error::new(401, "Invalid Grok Bearer token."))?;
                headers.insert("authorization", auth.clone());
                token = Some(access.to_owned());
            }
        }
        let refresh = endpoint == Endpoint::Token && token.is_some();
        let (choice, identity) = match &token {
            Some(token) => {
                let (choice, identity, selector) =
                    settings.route(token, refresh, &self.grok_routes).await?;
                log.field("credential_match", credential_match(selector.is_some()));
                if let Some(selector) = selector {
                    log.field("account_label", selector);
                }
                (choice, identity)
            }
            None => {
                let (choice, label) = settings.issuer_route()?;
                log.field("account_label", label);
                (choice, None)
            }
        };
        if let Some(account) = &identity {
            log.field("account_id", account.user_id());
        }
        log.field("upstream_base_url", url.origin().ascii_serialization());
        let mut request = reqwest::Request::new(parts.method, url);
        *request.headers_mut() = headers;
        *request.body_mut() = Some(bytes.into());
        let response = self
            .send_via(
                choice,
                request,
                false,
                Deadline::new(self.config.request_timeout_seconds),
                Upstream::Relay,
                log,
            )
            .await?;
        if endpoint == Endpoint::Discovery {
            return discovery_response(response, listener.as_deref()).await;
        }
        Ok(
            // Learned whatever the policy, which only decides whether learned
            // tokens are used, so turning it off later finds these renewals.
            // A device sign-in's tokens name their account the same way.
            if (refresh || sign_in) && response.status().is_success() {
                self.learn_refresh_route(
                    response,
                    self.grok_routes.clone(),
                    identity,
                    token.filter(|_| refresh),
                    None,
                )
            } else {
                response
            },
        )
    }
}

/// The OAuth grant type of a device sign-in's token request.
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

fn form_body(headers: &HeaderMap) -> bool {
    headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|v| {
            v.trim()
                .eq_ignore_ascii_case("application/x-www-form-urlencoded")
        })
}

/// This listener as a loopback client addressed it, from its `Host`: the
/// base its token endpoint is given at. None for any other host, such as
/// xAI's own inside a decrypted CONNECT, whose client reaches the token
/// endpoint through the proxy port already.
fn local_base(headers: &HeaderMap) -> Option<String> {
    let host = headers.get("host")?.to_str().ok()?;
    let authority: hyper::http::uri::Authority = host.parse().ok()?;
    let name = authority
        .host()
        .trim_start_matches('[')
        .trim_end_matches(']');
    let loopback = name.eq_ignore_ascii_case("localhost")
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    (loopback && !host.contains('@')).then(|| format!("http://{authority}"))
}

/// xAI's discovery document, its token and device endpoints moved to this
/// listener when a loopback client asked; any other response as it came.
async fn discovery_response(
    response: reqwest::Response,
    listener: Option<&str>,
) -> Result<reqwest::Response> {
    const LIMIT: usize = 65536;
    let Some(listener) = listener.filter(|_| response.status().is_success()) else {
        return Ok(response);
    };
    if response.content_length().is_some_and(|n| n > LIMIT as u64) {
        return Err(Error::config("Grok discovery document is too large."));
    }
    let status = response.status();
    let mut headers = response.headers().clone();
    let bytes = response
        .bytes()
        .await
        .map_err(|_| Error::config("Grok discovery document could not be read."))?;
    if bytes.len() > LIMIT {
        return Err(Error::config("Grok discovery document is too large."));
    }
    let mut document: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|_| Error::config("Invalid Grok discovery document."))?;
    crate::grok::discovery_through(&mut document, listener);
    let body = serde_json::to_vec(&document).unwrap();
    for name in [
        "content-length",
        "content-encoding",
        "transfer-encoding",
        "etag",
    ] {
        headers.remove(name);
    }
    headers.insert("content-type", "application/json".parse().unwrap());
    let mut built = hyper::Response::builder().status(status);
    *built.headers_mut().unwrap() = headers;
    Ok(reqwest::Response::from(
        built.body(reqwest::Body::from(body)).unwrap(),
    ))
}

/// A Grok Route: every one is chosen from the request's credential, with no
/// account lookup or Saved login renewal.
pub(super) enum Route {
    /// As Codex's account requests: the account and its route, no provider.
    Account(GrokRoute),
    /// An xAI API or gateway request, as Codex's and Claude's API key
    /// requests: its key is relayed as sent, for the upstream to validate.
    Api(ApiRoute, String),
}

impl Adapter for super::pipeline::Grok {
    type Own = (Url, Endpoint);
    type Pending = std::convert::Infallible;
    type Route = Route;

    async fn plan(
        server: &Server,
        incoming: &mut Request<Incoming>,
        target: &Target,
    ) -> Result<Plan<(Url, Endpoint), Route>> {
        let settings = &server.config.grok;
        // Proxy tunnels and scoped base URLs share the credential router.
        let rest = target.rest();
        let raw = target.raw();
        if !settings.configured() {
            return Err(Error::new(404, "Grok routing is not configured."));
        }
        if let Some(own) = settings.endpoint(raw) {
            return Ok(Plan::Own(own));
        }
        let path = rest.split('?').next().unwrap_or(rest);
        if path.ends_with("/oauth2/token")
            || rest.starts_with("/https://auth.x.ai/")
            || incoming.method() == "GET" && path.ends_with("/chat/completions")
            || raw
                .split_once('?')
                .is_some_and(|(path, _)| settings.endpoint(path).is_some())
        {
            return Err(Error::new(404, "Unknown Grok endpoint."));
        }
        Ok(Plan::Route(
            match settings
                .resolve(incoming.headers(), rest, &server.grok_routes)
                .await?
            {
                GrokTarget::Account(route) => Route::Account(route),
                GrokTarget::Api(route) => {
                    let key = incoming
                        .headers()
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.strip_prefix("Bearer "))
                        .unwrap_or_default()
                        .to_owned();
                    Route::Api(route, key)
                }
            },
        ))
    }

    async fn own(
        server: &Server,
        incoming: Request<Incoming>,
        (url, endpoint): (Url, Endpoint),
        log: &mut RequestLog,
    ) -> Result<reqwest::Response> {
        server
            .forward_grok_subscription(incoming, url, endpoint, log)
            .await
    }

    fn renews(_: &Server) -> bool {
        false
    }

    async fn account(
        _: &Server,
        _: &Request<Incoming>,
        _: &Target,
    ) -> Result<std::convert::Infallible> {
        unreachable!("every Grok Route is planned")
    }

    fn saved_login(pending: &std::convert::Infallible) -> Option<&str> {
        match *pending {}
    }

    async fn complete(
        _: &Server,
        pending: std::convert::Infallible,
        _: &mut RequestLog,
    ) -> Result<Route> {
        match pending {}
    }

    fn record(_: &Server, route: &Route) -> RouteRecord {
        match route {
            Route::Account(route) => RouteRecord {
                upstream_base_url: Some(route.url.origin().ascii_serialization()),
                account_id: route
                    .identity
                    .as_ref()
                    .and_then(|i| i.account())
                    .map(|a| a.to_string()),
                account_label: route.account_label.clone(),
                configured: route.configured,
                ..Default::default()
            },
            Route::Api(route, key) => RouteRecord {
                provider: Some(route.selector.clone()),
                upstream_base_url: Some(route.upstream.clone()),
                credential_ref: crate::identity::shared_api_reference("Grok", &route.upstream, key),
                configured: route.configured,
                ..Default::default()
            },
        }
    }

    fn build(
        _: &Server,
        route: Route,
        _: &Request<Incoming>,
        _: &Target,
        _: &mut RequestLog,
    ) -> Result<Relay> {
        let (url, proxy, native_tls, token) = match route {
            Route::Account(route) => (route.url, route.proxy, false, route.token),
            Route::Api(route, key) => (route.url, route.proxy, route.custom_upstream, key),
        };
        Ok(Relay {
            url,
            proxy,
            native_tls,
            credential: Credential::Bearer(token),
            cooldown: None,
            renews: false,
            claude_identity: None,
        })
    }
}
