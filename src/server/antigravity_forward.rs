//! Antigravity's Cloud Code requests: routed by the Google login they carry,
//! forwarded unchanged otherwise; and its Gemini API key requests, routed by
//! their key or upstream URL. Account lookups, their caches, route states and
//! statistics labels follow Claude's.
use super::identity_directory::{Directory, Lookup, State, Texts};
use super::pipeline::{Adapter, Credential, Plan, Relay, RouteRecord, TokenRoute};
use super::*;
use crate::antigravity::cooldown::Scope;
use crate::{
    antigravity::{Antigravity, AntigravityIdentity, AntigravityRoute, ApiCredential, ApiRoute},
    refresh_routes::RefreshRoutes,
};
use hyper::http::request::Parts;
use std::collections::BTreeMap;

/// Google accounts by access token, which servers rebuilt in one process
/// share.
pub type AntigravityProfiles = Profiles<AntigravityIdentity>;

/// Antigravity's Account lookup: the Google account's userinfo.
impl Lookup for super::pipeline::Antigravity {
    type Identity = AntigravityIdentity;
    const SERVICE: &'static str = "antigravity";
    const HEADERS: &'static [(&'static str, &'static str)] = &[];
    const TEXTS: Texts = Texts {
        request: "Invalid Antigravity account lookup.",
        failed: "Antigravity account lookup failed.",
        read: "Antigravity account lookup read failed.",
        too_large: "Antigravity account lookup exceeds 64 KiB.",
        json: "Invalid Antigravity account lookup JSON.",
        timeout: "Antigravity account probe timed out.",
    };
    fn directory(server: &Server) -> &Directory<AntigravityIdentity> {
        &server.antigravity_identities
    }
    fn url(_: &Server) -> Result<Url> {
        Ok(Url::parse(crate::antigravity::USERINFO_URL).unwrap())
    }
    fn identity(answer: &serde_json::Value) -> Result<AntigravityIdentity> {
        AntigravityIdentity::userinfo(answer)
    }
}

impl Server {
    pub fn with_antigravity_profiles(mut self, profiles: AntigravityProfiles) -> Self {
        self.antigravity_identities.share(profiles);
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

    /// Antigravity's token refresh, as Codex's and Claude's: routed by its
    /// refresh token, relayed unchanged, and its response's ID token teaches
    /// the account of that refresh token.
    async fn forward_antigravity_refresh(
        &self,
        incoming: Request<Incoming>,
        log: &mut RequestLog,
    ) -> Result<reqwest::Response> {
        let (parts, bytes) = Server::token_refresh_body(incoming).await?;
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
            match self
                .lookup_account::<super::pipeline::Antigravity>(&access, &route.proxy, log)
                .await
            {
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
        RouteRecord {
            account_id: route.identity.as_ref().map(|i| i.email.clone()),
            account_label: settings.account_label(route.saved, route.identity.as_ref()),
            configured: route.configured,
            ..Default::default()
        }
        .stamp(log);
        // The new access token is this login's too, though the client may
        // keep it without saving it where the saved login is read.
        let renewed = self.antigravity_renewed.clone();
        let (sent, identity) = (token.clone(), route.identity.clone());
        let observe: ResponseObserver = Box::new(move |body| {
            renewed.learn_response(body, &sent, identity);
        });
        let token_route = TokenRoute {
            url: Url::parse(crate::antigravity::TOKEN_REFRESH_UPSTREAM).unwrap(),
            proxy: route.proxy,
            routes: routes.clone(),
            previous: route.identity,
            sent: Some(token),
            observe: Some(observe),
        };
        self.relay_token_refresh(parts, bytes, token_route, log)
            .await
    }

    /// The route a credential takes, as records name it: the saved login's
    /// email route, else `default`; another account's email route; else the
    /// fallback that carried it.
    /// Keeps the account a lookup named for the saved login. A failure to
    /// save it is logged; the request goes on.
    pub(crate) fn learn_antigravity_login(
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

    /// The saved login and its known account: recorded, learned or looked up.
    async fn antigravity_saved_login(
        &self,
    ) -> Result<Option<(crate::antigravity::Login, Option<AntigravityIdentity>)>> {
        let Some(login) = self.config.antigravity.saved().await? else {
            return Ok(None);
        };
        let identity = Antigravity::login_identity(&login, &self.antigravity_refresh_routes)
            .or_else(|| {
                self.antigravity_identities
                    .known(&login.access_token, false)
            });
        Ok(Some((login, identity)))
    }

    /// Antigravity's login for Route activation: its saved login only, whose
    /// account is recorded, learned or looked up. Never probes.
    pub(super) async fn antigravity_logins(&self) -> Vec<crate::route_activation::Login> {
        use crate::route_activation::Login;
        let settings = &self.config.antigravity;
        let Ok(Some((login, identity))) = self.antigravity_saved_login().await else {
            return Vec::new();
        };
        if identity.is_none() && settings.routing.account_probe.is_some() {
            return vec![
                match self.antigravity_identities.state(&login.access_token).await {
                    State::Failed => Login::ProbeFailed,
                    _ => Login::Unidentified,
                },
            ];
        }
        settings
            .login_label(identity.as_ref())
            .map(Login::Active)
            .into_iter()
            .collect()
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
        for identity in self.antigravity_identities.accounts() {
            if let Some(label) = settings
                .mapped_key(&identity)
                .or_else(|| crate::identity::routing_account_label(&settings.routing, "", &[], ""))
            {
                labels
                    .entry(("Antigravity".into(), identity.email))
                    .or_insert(label);
            }
        }
    }

    /// Account discovery for the saved login, as for Claude's: only with an
    /// explicit `account_probe`. A found account is learned for the login's
    /// refresh token.
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
        let Some(identity) = self
            .discover_account::<super::pipeline::Antigravity>(&login.access_token, proxy)
            .await
        else {
            return;
        };
        if let Some(refresh) = login.refresh_token.as_deref() {
            self.learn_antigravity_login(refresh, &identity);
        }
        if let Some(label) = label(&identity) {
            labels.insert(("Antigravity".into(), identity.email), label);
        }
    }
}

/// An Antigravity Route and the Cloud Code or Gemini API URL it sends to.
pub(super) enum Route {
    /// A Gemini API key or URL route.
    Api(ApiRoute, Url),
    /// The Google login the request carries.
    Account(AntigravityRoute, Url),
}

impl Adapter for super::pipeline::Antigravity {
    type Own = ();
    type Pending = (AntigravityRoute, Url);
    type Route = Route;

    async fn plan(server: &Server, parts: &mut Parts, target: &Target) -> Result<Plan<(), Route>> {
        let settings = &server.config.antigravity;
        let api_target = target.rest();
        if let Some(route) = settings.api_route(&parts.headers, api_target)? {
            let url = route.url(api_target)?;
            return Ok(Plan::Route(Route::Api(route, url)));
        }
        // A key in `x-goog-api-key` or `key` is a Gemini API key, routed as
        // Claude's `x-api-key`; a Bearer token stays a Google login.
        if settings.serves_api_keys() {
            if let Some((key, credential)) =
                crate::antigravity::api_credential(&parts.headers, api_target)?
                    .filter(|(_, credential)| *credential != ApiCredential::Bearer)
            {
                let route = settings
                    .resolve_api_key(key, credential, api_target)
                    .await?;
                let url = route.url(api_target)?;
                return Ok(Plan::Route(Route::Api(route, url)));
            }
        }
        if !settings.enabled() {
            return Err(Error::new(404, "Antigravity routing is not configured."));
        }
        // An unprefixed target is Antigravity's only through an API key route.
        if !target.scoped() {
            return Err(Error::config("Unroutable Antigravity request target."));
        }
        let rest = target.rest();
        if crate::antigravity::token_host(rest) {
            if target.kind() != Kind::TokenRefresh {
                return Err(Error::new(
                    404,
                    "Only Antigravity's token refresh is served on oauth2.googleapis.com.",
                ));
            }
            return Ok(Plan::Own(()));
        }
        // agy's scope check names its token in the query: route by it as by
        // a Bearer header, which must name the same token if both are sent.
        if let Some(token) = crate::antigravity::query_token(rest)? {
            let bearer = parts
                .headers
                .get("authorization")
                .map(|v| v.to_str().ok().and_then(|v| v.strip_prefix("Bearer ")));
            match bearer {
                None => {
                    let value = format!("Bearer {token}")
                        .parse()
                        .map_err(|_| Error::new(401, "Invalid credential."))?;
                    parts.headers.insert("authorization", value);
                }
                Some(Some(bearer)) if bearer == token => {}
                Some(_) => {
                    return Err(Error::new(
                        400,
                        "Supply one access token: the Bearer header and access_token differ.",
                    ));
                }
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
        server.forward_antigravity_refresh(incoming, log).await
    }

    /// As for Codex and Claude: only the saved login's Model calls renew it.
    fn renews(server: &Server) -> bool {
        server.config.antigravity.saved_login
    }

    async fn account(
        server: &Server,
        parts: &Parts,
        target: &Target,
    ) -> Result<(AntigravityRoute, Url)> {
        let settings = &server.config.antigravity;
        let route = settings
            .resolve(
                &parts.headers,
                &server.antigravity_refresh_routes,
                &server.antigravity_renewed,
            )
            .await?;
        // Validate the target before any account lookup.
        let url = settings.url(target.rest())?;
        Ok((route, url))
    }

    fn saved_login((route, _): &(AntigravityRoute, Url)) -> Option<&str> {
        route.saved_login()
    }

    async fn complete(
        server: &Server,
        (mut route, url): (AntigravityRoute, Url),
        log: &mut RequestLog,
    ) -> Result<Route> {
        if route.needs_profile {
            let identity = server
                .lookup_account::<super::pipeline::Antigravity>(&route.token, &route.proxy, log)
                .await?;
            if let Some(refresh) = route.ext.as_deref() {
                server.learn_antigravity_login(refresh, &identity);
            }
            server
                .config
                .antigravity
                .apply_profile(&mut route, identity)?;
        }
        Ok(Route::Account(route, url))
    }

    fn record(_: &Server, route: &Route) -> RouteRecord {
        match route {
            Route::Api(route, _) => RouteRecord::api("Antigravity", route),
            Route::Account(route, url) => {
                RouteRecord::account(route, url.origin().ascii_serialization())
            }
        }
    }

    fn build(
        _: &Server,
        route: Route,
        parts: &Parts,
        _: &Target,
        _: &mut RequestLog,
    ) -> Result<Relay> {
        Ok(match route {
            Route::Api(route, url) => Relay {
                cooldown: Scope::model(&parts.method, &url, || {
                    Scope::new(&route.upstream, &route.key)
                }),
                url,
                proxy: route.proxy,
                native_tls: route.custom_upstream,
                credential: if route.ext == ApiCredential::Bearer {
                    Credential::Bearer(route.key)
                } else {
                    Credential::AsSent
                },
                renews: false,
                claude_identity: None,
            },
            Route::Account(route, url) => Relay {
                cooldown: Scope::model(&parts.method, &url, || {
                    let login = route
                        .identity
                        .as_ref()
                        .map(|id| id.email.to_ascii_lowercase());
                    let credential = login
                        .as_deref()
                        .or(route.ext.as_deref())
                        .unwrap_or(&route.token);
                    Scope::new("oauth", credential)
                }),
                url,
                proxy: route.proxy,
                native_tls: false,
                credential: Credential::Bearer(route.token),
                renews: true,
                claude_identity: None,
            },
        })
    }
}
