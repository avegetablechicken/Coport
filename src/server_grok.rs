use super::*;
use crate::{
    grok::{ApiRoute, GrokAccount, GrokRoute, GrokTarget},
    refresh_routes::{Account, RefreshRoutes},
};

impl Server {
    pub(super) async fn forward_grok(
        &self,
        incoming: Request<Incoming>,
        target: &str,
        log: &mut RequestLog,
    ) -> Result<reqwest::Response> {
        let settings = &self.config.grok;
        // Proxy tunnels and scoped base URLs share the credential router.
        let scoped = crate::grok::target(target).map(crate::antigravity::named_upstream_target);
        let rest = scoped.as_deref().unwrap_or(target);
        if !settings.configured() {
            return Err(Error::new(404, "Grok routing is not configured."));
        }
        if let Some((url, refresh)) = settings.endpoint(target) {
            return self
                .forward_grok_subscription(incoming, url, refresh, log)
                .await;
        }
        let path = rest.split('?').next().unwrap_or(rest);
        if path.ends_with("/oauth2/token")
            || rest.starts_with("/https://auth.x.ai/")
            || incoming.method() == "GET" && path.ends_with("/chat/completions")
            || target
                .split_once('?')
                .is_some_and(|(path, _)| settings.endpoint(path).is_some())
        {
            return Err(Error::new(404, "Unknown Grok endpoint."));
        }
        match settings
            .resolve(incoming.headers(), rest, &self.grok_routes)
            .await?
        {
            GrokTarget::Account(route) => self.forward_grok_account(incoming, route, log).await,
            GrokTarget::Api(route) => self.forward_grok_api(incoming, route, log).await,
        }
    }

    async fn forward_grok_account(
        &self,
        incoming: Request<Incoming>,
        route: GrokRoute,
        log: &mut RequestLog,
    ) -> Result<reqwest::Response> {
        // As Codex's account requests: the account and its route, no provider.
        log.field(
            "upstream_base_url",
            route.url.origin().ascii_serialization().as_str(),
        );
        if let Some(account) = route.identity.as_ref().and_then(|i| i.account()) {
            log.field("account_id", account);
        }
        if let Some(label) = &route.account_label {
            log.field("account_label", label);
        }
        log.field("credential_match", credential_match(route.configured));
        let headers = self.grok_headers(&incoming, &route.token)?;
        self.send_grok(incoming, headers, route.url, &route.proxy, false, log)
            .await
    }

    /// An xAI API or gateway request, as Codex's and Claude's API key
    /// requests: its key is relayed as sent, for the upstream to validate.
    async fn forward_grok_api(
        &self,
        incoming: Request<Incoming>,
        route: ApiRoute,
        log: &mut RequestLog,
    ) -> Result<reqwest::Response> {
        log.field("provider", &route.selector);
        log.field("upstream_base_url", &route.upstream);
        let key = incoming
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or_default()
            .to_owned();
        if let Some(reference) =
            crate::identity::shared_api_reference("Grok", &route.upstream, &key)
        {
            log.field("credential_ref", reference);
        }
        log.field("credential_match", credential_match(route.configured));
        let headers = self.grok_headers(&incoming, &key)?;
        self.send_grok(
            incoming,
            headers,
            route.url,
            &route.proxy,
            route.custom_upstream,
            log,
        )
        .await
    }

    /// The forwarded headers with the one credential the route was chosen by.
    fn grok_headers(&self, incoming: &Request<Incoming>, token: &str) -> Result<HeaderMap> {
        let mut headers = filtered_headers(incoming.headers());
        headers.insert(
            "authorization",
            format!("Bearer {token}")
                .parse()
                .map_err(|_| Error::new(401, "Invalid credential."))?,
        );
        Ok(headers)
    }

    async fn send_grok(
        &self,
        incoming: Request<Incoming>,
        headers: HeaderMap,
        url: Url,
        proxy: &Choice,
        custom_upstream: bool,
        log: &mut RequestLog,
    ) -> Result<reqwest::Response> {
        let (parts, body) = incoming.into_parts();
        let bytes = read_body(body).await?;
        if log.fields.contains_key("model_call_id") {
            crate::model_calls::observe_request(&bytes, log);
        }
        let mut request = reqwest::Request::new(parts.method, url);
        *request.headers_mut() = headers;
        *request.body_mut() = Some(bytes.into());
        let deadline = Deadline::new(self.config.request_timeout_seconds);
        self.send_via(proxy, request, custom_upstream, deadline, log)
            .await
    }

    pub fn with_grok_routes(mut self, routes: RefreshRoutes<GrokAccount>) -> Self {
        self.grok_routes = Arc::new(routes);
        self
    }

    async fn forward_grok_subscription(
        &self,
        incoming: Request<Incoming>,
        url: Url,
        refresh: bool,
        log: &mut RequestLog,
    ) -> Result<reqwest::Response> {
        let settings = &self.config.grok;
        if !settings.enabled() {
            return Err(Error::new(404, "Grok account routing is not configured."));
        }
        if refresh && incoming.method() != "POST" || !refresh && incoming.method() != "GET" {
            return Err(Error::new(
                405,
                "Unsupported method for this Grok endpoint.",
            ));
        }
        let (parts, body) = incoming.into_parts();
        let bytes = read_body(body).await?;
        let mut headers = filtered_headers(&parts.headers);
        let token = if refresh {
            if !headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.split(';').next())
                .is_some_and(|v| {
                    v.trim()
                        .eq_ignore_ascii_case("application/x-www-form-urlencoded")
                })
            {
                return Err(Error::new(415, "Grok refresh requires a URL-encoded form."));
            }
            // Deserialize once; serde rejects duplicate credential or grant fields.
            #[derive(serde::Deserialize)]
            struct Grant {
                client_id: String,
                grant_type: String,
                refresh_token: String,
            }
            let grant: Grant = serde_urlencoded::from_bytes(&bytes)
                .map_err(|_| Error::new(400, "Invalid Grok refresh form."))?;
            if grant.client_id != crate::grok::CLIENT_ID || grant.grant_type != "refresh_token" {
                return Err(Error::new(403, "Only Grok Build token renewal is routed."));
            }
            if !crate::identity::valid_token(&grant.refresh_token) {
                return Err(Error::new(400, "Invalid Grok refresh token."));
            }
            headers.insert("accept-encoding", "identity".parse().unwrap());
            grant.refresh_token
        } else {
            let auth = parts
                .headers
                .get("authorization")
                .ok_or(Error::new(401, "Grok requires a Bearer token."))?;
            let token = auth
                .to_str()
                .ok()
                .and_then(|v| v.strip_prefix("Bearer "))
                .filter(|s| crate::identity::valid_token(s))
                .ok_or(Error::new(401, "Invalid Grok Bearer token."))?;
            headers.insert("authorization", auth.clone());
            token.to_owned()
        };
        let (choice, identity, selector) =
            settings.route(&token, refresh, &self.grok_routes).await?;
        log.field("credential_match", credential_match(selector.is_some()));
        if let Some(account) = &identity {
            log.field("account_id", account.account_id());
        }
        if let Some(selector) = selector {
            log.field("account_label", selector);
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
                log,
            )
            .await?;
        Ok(
            if refresh && !settings.subscription_file_only() && response.status().is_success() {
                self.learn_refresh_route(
                    response,
                    self.grok_routes.clone(),
                    identity,
                    Some(token),
                    None,
                )
            } else {
                response
            },
        )
    }
}
