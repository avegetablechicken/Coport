//! Antigravity's Cloud Code requests: routed by the Google login they carry,
//! forwarded unchanged otherwise; and its Gemini API key requests, routed by
//! their key or upstream URL. Account lookups, their caches, route states and
//! statistics labels follow Claude's.
use super::*;
use crate::{
    antigravity::{Antigravity, AntigravityIdentity, ApiCredential, ApiRoute},
    refresh_routes::RefreshRoutes,
};
use std::collections::BTreeMap;

/// Google identities by access token, with last use for bounded LRU eviction.
/// As `ClaudeProfiles`, one process can share it across rebuilt servers.
pub type AntigravityProfiles = Arc<Mutex<HashMap<String, (Instant, AntigravityIdentity)>>>;
const MAX_PROFILES: usize = 128;

impl Server {
    pub fn with_antigravity_profiles(mut self, profiles: AntigravityProfiles) -> Self {
        self.antigravity_profiles = profiles;
        self
    }
    /// Accounts learned for saved logins, kept across restarts.
    pub fn with_antigravity_refresh_routes(
        mut self,
        routes: RefreshRoutes<AntigravityIdentity>,
    ) -> Self {
        self.antigravity_refresh_routes = Arc::new(routes);
        self
    }

    pub(super) async fn forward_antigravity(
        &self,
        mut incoming: Request<Incoming>,
        target: &str,
        log: &mut RequestLog,
    ) -> Result<reqwest::Response> {
        let settings = &self.config.antigravity;
        // An unprefixed target is Antigravity's only through an API key route.
        let scoped =
            crate::antigravity::target(target).map(crate::antigravity::named_upstream_target);
        let api_target = scoped.as_deref().unwrap_or(target);
        if let Some(route) = settings.api_route(incoming.headers(), api_target)? {
            return self
                .forward_antigravity_api(incoming, api_target, route, log)
                .await;
        }
        // A key in `x-goog-api-key` or `key` is a Gemini API key, routed as
        // Claude's `x-api-key`; a Bearer token stays a Google login.
        if settings.serves_api_keys() {
            if let Some((key, credential)) =
                crate::antigravity::api_credential(incoming.headers(), api_target)?
                    .filter(|(_, credential)| *credential != ApiCredential::Bearer)
            {
                let route = settings
                    .resolve_api_key(key, credential, api_target)
                    .await?;
                return self
                    .forward_antigravity_api(incoming, api_target, route, log)
                    .await;
            }
        }
        if !settings.enabled() {
            return Err(Error::new(404, "Antigravity routing is not configured."));
        }
        let rest = scoped
            .as_deref()
            .ok_or(Error::config("Unroutable Antigravity request target."))?;
        if crate::antigravity::token_host(rest) {
            if !crate::antigravity::token_refresh(rest) {
                return Err(Error::new(
                    404,
                    "Only Antigravity's token refresh is served on oauth2.googleapis.com.",
                ));
            }
            return self.forward_antigravity_refresh(incoming, log).await;
        }
        // As for Codex and Claude: only the saved login's model calls renew it.
        let auto_refresh = self.config.auto_refresh_access_token
            && settings.saved_login
            && crate::model_calls::is_model_endpoint(incoming.method().as_str(), target);
        let rotation_guard = if auto_refresh {
            Some(self.auto_refresh.rotation.read().await)
        } else {
            None
        };
        if auto_refresh {
            if let Some(token) = incoming
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
            {
                if let Some(current) = self.refreshed_alias(Service::Antigravity, token).await {
                    incoming.headers_mut().insert(
                        "authorization",
                        format!("Bearer {current}").parse().unwrap(),
                    );
                }
            }
        }
        let mut route = settings
            .resolve(
                incoming.headers(),
                &self.antigravity_refresh_routes,
                &self.antigravity_renewed,
            )
            .await?;
        // Validate the target before any account lookup.
        let url = settings.url(rest)?;
        if route.needs_profile {
            let identity = self
                .antigravity_identity(&route.token, &route.proxy, log)
                .await?;
            if let Some(refresh) = route.refresh_token.as_deref() {
                self.learn_antigravity_login(refresh, &identity);
            }
            settings.apply_profile(&mut route, identity)?;
        }
        log.field("provider", &route.label);
        log.field(
            "upstream_base_url",
            url.origin().ascii_serialization().as_str(),
        );
        if let Some(identity) = &route.identity {
            log.field("account_id", &identity.email);
        }
        if let Some(label) = self.antigravity_label(route.matched_login, route.identity.as_ref()) {
            log.field("account_label", label);
        }
        log.field("credential_match", credential_match(route.configured));
        let refresh_snapshot = if auto_refresh && route.matched_login {
            self.refresh_snapshot(Service::Antigravity, &route.token)
                .await
        } else {
            None
        };
        drop(rotation_guard);
        let (parts, body) = incoming.into_parts();
        let bytes = read_body(body).await?;
        if log.fields.contains_key("model_call_id") {
            crate::model_calls::observe_request(&bytes, log);
        }
        let mut headers = filtered_headers(&parts.headers);
        headers.insert(
            "authorization",
            format!("Bearer {}", route.token)
                .parse()
                .map_err(|_| Error::new(401, "Invalid credential."))?,
        );
        let mut request = reqwest::Request::new(parts.method, url);
        *request.headers_mut() = headers;
        *request.body_mut() = Some(bytes.into());
        let deadline = Deadline::new(self.config.request_timeout_seconds);
        let renewal = auto_refresh::Renewal::new(refresh_snapshot, &request, None);
        let response = self
            .send_via(&route.proxy, request, false, deadline, log)
            .await?;
        self.retry_after_renewal(response, renewal, &route.proxy, false, deadline, log)
            .await
    }

    /// A Gemini API request, as Codex's and Claude's API key requests: no
    /// account lookup, and its key is relayed where it came, for the upstream
    /// to validate.
    async fn forward_antigravity_api(
        &self,
        incoming: Request<Incoming>,
        target: &str,
        route: ApiRoute,
        log: &mut RequestLog,
    ) -> Result<reqwest::Response> {
        let url = route.url(target)?;
        log.field("provider", &route.selector);
        log.field("upstream_base_url", &route.upstream);
        if let Some(reference) =
            crate::identity::shared_api_reference("Antigravity", &route.upstream, &route.key)
        {
            log.field("credential_ref", reference);
        }
        log.field("credential_match", credential_match(route.configured));
        let (parts, body) = incoming.into_parts();
        let bytes = read_body(body).await?;
        if log.fields.contains_key("model_call_id") {
            crate::model_calls::observe_request(&bytes, log);
        }
        let mut headers = filtered_headers(&parts.headers);
        if route.credential == ApiCredential::Bearer {
            headers.insert(
                "authorization",
                format!("Bearer {}", route.key)
                    .parse()
                    .map_err(|_| Error::new(401, "Invalid credential."))?,
            );
        }
        let mut request = reqwest::Request::new(parts.method, url);
        *request.headers_mut() = headers;
        *request.body_mut() = Some(bytes.into());
        let deadline = Deadline::new(self.config.request_timeout_seconds);
        self.send_via(&route.proxy, request, route.custom_upstream, deadline, log)
            .await
    }

    /// Antigravity's token refresh, as Codex's and Claude's: routed by its
    /// refresh token, relayed unchanged, and its response's ID token teaches
    /// the account of that refresh token.
    async fn forward_antigravity_refresh(
        &self,
        incoming: Request<Incoming>,
        log: &mut RequestLog,
    ) -> Result<reqwest::Response> {
        if incoming.method() != "POST" {
            return Err(Error::new(405, "Token refresh supports POST only."));
        }
        let (parts, body) = incoming.into_parts();
        let bytes = read_body(body).await?;
        // The endpoint serves every Google client; only Antigravity's is
        // routed, so another's credential never leaves through its proxy.
        if !crate::antigravity::form_field(&bytes, "client_id")
            .is_some_and(|id| crate::antigravity::is_client_id(&id))
        {
            return Err(Error::new(
                403,
                "Only Antigravity's token refresh is routed; run other Google clients without this proxy.",
            ));
        }
        // Its client then serves the proxy's own renewals of the saved login.
        crate::antigravity::learn_client(&bytes);
        let token = crate::routing::refresh_token(&bytes)
            .ok_or(Error::new(400, "Token refresh requires a refresh_token."))?;
        let settings = &self.config.antigravity;
        let routes = &self.antigravity_refresh_routes;
        let mut route = settings.resolve_refresh(&token, routes).await?;
        if let Some(access) = route.probe_token.clone() {
            match self.antigravity_identity(&access, &route.proxy, log).await {
                Ok(identity) => {
                    self.learn_antigravity_login(&token, &identity);
                    settings.apply_refresh_profile(&mut route, identity)?;
                }
                // An expired token names no account, and refusing its
                // refresh would leave the login unable to renew at all.
                Err(error) if error.status == 401 => settings.apply_unnamed_refresh(&mut route),
                Err(error) => return Err(error),
            }
        }
        log.field("credential_match", credential_match(route.configured));
        if let Some(identity) = &route.identity {
            log.field("account_id", &identity.email);
        }
        if let Some(label) = self.antigravity_label(route.saved, route.identity.as_ref()) {
            log.field("account_label", label);
        }
        let url = Url::parse(crate::antigravity::TOKEN_REFRESH_UPSTREAM).unwrap();
        let mut request = reqwest::Request::new(parts.method, url);
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
                log,
            )
            .await?;
        if !response.status().is_success() {
            return Ok(response);
        }
        // The new access token is this login's too, though the client may
        // keep it without saving it where the saved login is read.
        let renewed = self.antigravity_renewed.clone();
        let (sent, identity) = (token.clone(), route.identity.clone());
        let observe: ResponseObserver = Box::new(move |body| {
            renewed.learn_response(body, &sent, identity);
        });
        Ok(self.learn_refresh_route(
            response,
            routes.clone(),
            route.identity,
            Some(token),
            Some(observe),
        ))
    }

    /// The route a credential takes, as records name it: the saved login's
    /// email route, else `default`; another account's email route; else the
    /// fallback that carried it.
    fn antigravity_label(
        &self,
        saved: bool,
        identity: Option<&AntigravityIdentity>,
    ) -> Option<String> {
        let settings = &self.config.antigravity;
        if saved {
            settings.login_label(identity)
        } else {
            identity.and_then(|i| settings.mapped_key(i))
        }
        .or_else(|| crate::identity::routing_account_label(&settings.routing, "", &[], ""))
    }

    /// Keeps the account a lookup named for the saved login. A failure to
    /// save it is logged; the request goes on.
    pub(super) fn learn_antigravity_login(
        &self,
        refresh_token: &str,
        identity: &AntigravityIdentity,
    ) {
        if let Err(error) = self
            .antigravity_refresh_routes
            .learn(refresh_token, identity)
        {
            self.logger.write(
                "refresh_route_unsaved",
                [
                    ("service".to_owned(), "antigravity".into()),
                    ("reason".to_owned(), error.to_string().into()),
                ]
                .into_iter()
                .collect(),
            );
        }
    }

    /// The Google account of a token, looked up through `proxy`: never
    /// directly, nor through a proxy an unverified account picks.
    async fn antigravity_identity(
        &self,
        token: &str,
        proxy: &Choice,
        log: &mut RequestLog,
    ) -> Result<AntigravityIdentity> {
        if let Some(identity) = self.cached_antigravity_identity(token, true) {
            return Ok(identity);
        }
        log.field("service", "antigravity");
        let request = self
            .client("none")?
            .get(crate::antigravity::USERINFO_URL)
            .bearer_auth(token)
            .header("accept", "application/json")
            // The proxy reads this response itself and does not decompress it.
            .header("accept-encoding", "identity")
            .build()
            .map_err(|_| Error::config("Invalid Antigravity account lookup."))?;
        let mut response = self
            .send_via(
                proxy,
                request,
                false,
                Deadline::new(self.config.request_timeout_seconds.min(10.0)),
                log,
            )
            .await?;
        for (from, to) in [
            ("proxy", "profile_proxy"),
            ("proxy_endpoint", "profile_proxy_endpoint"),
        ] {
            if let Some(value) = log.fields.get(from).cloned() {
                log.fields.insert(to.into(), value);
            }
        }
        let status = response.status();
        if !status.is_success() {
            return Err(Error::new(
                if status.is_client_error() {
                    status.as_u16()
                } else {
                    502
                },
                "Antigravity account lookup failed.",
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Error::config("Antigravity account lookup read failed."))?
        {
            if bytes.len() + chunk.len() > 65536 {
                return Err(Error::config("Antigravity account lookup exceeds 64 KiB."));
            }
            bytes.extend_from_slice(&chunk);
        }
        let value = serde_json::from_slice(&bytes)
            .map_err(|_| Error::config("Invalid Antigravity account lookup JSON."))?;
        let identity = AntigravityIdentity::userinfo(&value)?;
        let mut cache = self
            .antigravity_profiles
            .lock()
            .map_err(|_| Error::config("Antigravity profile cache unavailable."))?;
        if cache.len() >= MAX_PROFILES && !cache.contains_key(token) {
            if let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, (last_used, _))| *last_used)
                .map(|(key, _)| key.clone())
            {
                cache.remove(&oldest);
            }
        }
        cache.insert(token.to_owned(), (Instant::now(), identity.clone()));
        Ok(identity)
    }

    /// An identity already looked up for `token`; `touch` marks it used.
    fn cached_antigravity_identity(&self, token: &str, touch: bool) -> Option<AntigravityIdentity> {
        let mut cache = self.antigravity_profiles.lock().ok()?;
        let (last_used, identity) = cache.get_mut(token)?;
        if touch {
            *last_used = Instant::now();
        }
        Some(identity.clone())
    }

    /// The saved login and its known account: recorded, learned or looked up.
    async fn antigravity_saved_login(
        &self,
    ) -> Result<Option<(crate::antigravity::Login, Option<AntigravityIdentity>)>> {
        let Some(login) = self.config.antigravity.saved().await? else {
            return Ok(None);
        };
        let identity = Antigravity::login_identity(&login, &self.antigravity_refresh_routes)
            .or_else(|| self.cached_antigravity_identity(&login.access_token, false));
        Ok(Some((login, identity)))
    }

    /// As for Codex and Claude, the states of Antigravity's account routes
    /// with saved logins only: the saved login's route is active, others
    /// inactive, and unknown while an explicit probe has yet to name it.
    /// Never probes.
    pub(super) async fn antigravity_route_states(&self) -> BTreeMap<String, &'static str> {
        let settings = &self.config.antigravity;
        // Without account routes there is nothing to report, and the
        // credential store is not read.
        if !settings.account_auth_file_only || settings.routing.account.is_empty() {
            return BTreeMap::new();
        }
        let mut states: BTreeMap<String, &'static str> = settings
            .routing
            .account
            .keys()
            .map(|name| (name.clone(), "inactive"))
            .collect();
        let Ok(Some((login, identity))) = self.antigravity_saved_login().await else {
            return states;
        };
        if identity.is_none() && settings.routing.account_probe.is_some() {
            let failed = self
                .failed_probes
                .lock()
                .await
                .contains_key(&login.access_token);
            for state in states.values_mut() {
                *state = if failed { "probe_failed" } else { "unknown" };
            }
            return states;
        }
        if let Some(label) = settings.login_label(identity.as_ref()) {
            states.insert(label, "active");
        }
        states
    }

    /// Display labels of Antigravity accounts already identified. Never
    /// probes or reads a login.
    pub(super) fn cached_antigravity_labels(
        &self,
        labels: &mut BTreeMap<(String, String), String>,
    ) {
        let settings = &self.config.antigravity;
        for identity in self.antigravity_refresh_routes.identities() {
            if let Some(label) = settings.mapped_key(&identity) {
                labels
                    .entry(("Antigravity".into(), identity.email))
                    .or_insert(label);
            }
        }
        if let Ok(profiles) = self.antigravity_profiles.lock() {
            for (_, identity) in profiles.values() {
                if let Some(label) = settings.mapped_key(identity).or_else(|| {
                    crate::identity::routing_account_label(&settings.routing, "", &[], "")
                }) {
                    labels
                        .entry(("Antigravity".into(), identity.email.clone()))
                        .or_insert(label);
                }
            }
        }
    }

    /// Background discovery for the saved login, as for Claude's: only with
    /// an explicit `account_probe`, not retried sooner than
    /// `FAILED_PROBE_RETRY` after a failure. A found account is learned for
    /// the login's refresh token.
    pub(super) async fn discover_antigravity_label(
        &self,
        labels: &mut BTreeMap<(String, String), String>,
    ) {
        let settings = &self.config.antigravity;
        let Some(proxy) = settings.routing.account_probe.as_ref() else {
            return;
        };
        if !settings.enabled() {
            return;
        }
        let Ok(Some((login, identity))) = self.antigravity_saved_login().await else {
            return;
        };
        let label = |identity: &AntigravityIdentity| settings.login_label(Some(identity));
        if let Some(identity) = identity {
            if let Some(label) = label(&identity) {
                labels.insert(("Antigravity".into(), identity.email), label);
            }
            return;
        }
        let token = login.access_token.clone();
        if self
            .failed_probes
            .lock()
            .await
            .get(&token)
            .is_some_and(|at| at.elapsed() < FAILED_PROBE_RETRY)
        {
            return;
        }
        let mut log = RequestLog {
            logger: self.logger.clone(),
            fields: json!({"service":"antigravity"})
                .as_object()
                .unwrap()
                .clone(),
            started: Instant::now(),
            status: 0,
            bytes: 0,
            outcome: "account_probe_failed",
        };
        let result = tokio::time::timeout(
            Duration::from_secs_f64(self.config.request_timeout_seconds.min(10.0)),
            self.antigravity_identity(&token, proxy, &mut log),
        )
        .await
        .unwrap_or_else(|_| Err(Error::config("Antigravity account probe timed out.")));
        match result {
            Ok(identity) => {
                log.outcome = "account_probe_finished";
                self.failed_probes.lock().await.remove(&token);
                if let Some(refresh) = login.refresh_token.as_deref() {
                    self.learn_antigravity_login(refresh, &identity);
                }
                if let Some(label) = label(&identity) {
                    labels.insert(("Antigravity".into(), identity.email), label);
                }
            }
            Err(error) => {
                log.field("reason", error.message);
                let mut failed_probes = self.failed_probes.lock().await;
                if failed_probes.len() >= 128 {
                    failed_probes.clear();
                }
                failed_probes.insert(token, Instant::now());
            }
        }
    }
}
