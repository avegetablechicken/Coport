use super::*;
use crate::{
    grok::GrokAccount,
    refresh_routes::{Account, RefreshRoutes},
};

impl Server {
    pub fn with_grok_routes(mut self, routes: RefreshRoutes<GrokAccount>) -> Self {
        self.grok_routes = Arc::new(routes);
        self
    }

    pub(super) async fn forward_grok(
        &self,
        incoming: Request<Incoming>,
        target: &str,
        log: &mut RequestLog,
    ) -> Result<reqwest::Response> {
        let settings = &self.config.grok;
        if !settings.enabled() {
            return Err(Error::new(404, "Grok account routing is not configured."));
        }
        let (url, refresh) = settings
            .endpoint(target)
            .ok_or(Error::new(404, "Unknown Grok subscription endpoint."))?;
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
            if refresh
                && settings.account_auth_file_only != Some(true)
                && response.status().is_success()
            {
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
