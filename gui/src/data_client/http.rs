//! Summaries read over HTTP(S) from a Device's data endpoint. A plain HTTP
//! endpoint must resolve only to private LAN or loopback addresses, and the
//! checked resolution is the one connected to.
use super::{Source, Summary, Transport};
use coport::external_access::DataConfig;
use std::{net::IpAddr, time::Duration};

impl Source {
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty()
            || self.name.len() > 128
            || self.name.chars().any(char::is_control)
        {
            return Err("Provide a data source name (up to 128 bytes).".into());
        }
        if self.transport == Transport::Ssh {
            return self
                .ssh_connection
                .as_ref()
                .ok_or_else(|| "Configure the SSH data connection.".to_owned())?
                .validate();
        }
        if self.url.len() > 2048 {
            return Err("Data source URL is too long.".into());
        }
        let url = reqwest::Url::parse(&self.url).map_err(|_| "Invalid data source URL")?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !matches!(url.path(), "" | "/")
        {
            return Err("Use an HTTP(S) origin without credentials, path or query.".into());
        }
        self.credentials().validate(0).map_err(|e| e.to_string())?;
        if self
            .ca_certificate
            .as_ref()
            .is_some_and(|path| !coport::config::expand(path).is_absolute())
        {
            return Err("CA certificate path must be absolute.".into());
        }
        Ok(())
    }
    fn credentials(&self) -> DataConfig {
        DataConfig {
            port: 1,
            token_env: self.token_env.clone(),
            token_file: self.token_file.clone(),
            trusted_lan: vec!["10.0.0.0/8".parse().unwrap()],
            tls: None,
        }
    }
}
/// `source`'s Summary, or with `selection` its requested range first:
/// a Device that sends the rest later answers with that range only.
pub(super) async fn read(
    source: &Source,
    selection: Option<super::Selection>,
) -> Result<Summary, String> {
    let token = source
        .credentials()
        .load_token()
        .map_err(|_| "Cannot load this data source's private access key")?;
    let mut url = reqwest::Url::parse(&source.url).map_err(|_| "Invalid source URL")?;
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(8));
    if url.scheme() == "http" {
        let host = url.host_str().ok_or("Missing data source host")?.to_owned();
        let port = url.port_or_known_default().ok_or("Missing source port")?;
        let addresses = tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|_| "Cannot resolve LAN data source")?
            .take(17)
            .collect::<Vec<_>>();
        if addresses.is_empty()
            || addresses.len() > 16
            || addresses.iter().any(|addr| !lan(addr.ip()))
        {
            return Err("HTTP data sources must resolve exclusively to private LAN or loopback addresses; use HTTPS for public sources.".into());
        }
        // Pin the validated resolution so a later DNS answer cannot redirect the key.
        builder = builder.resolve_to_addrs(&host, &addresses);
    }
    if let Some(path) = &source.ca_certificate {
        let bytes = coport::external_access::read_limited(
            &coport::config::expand(path),
            false,
            1024 * 1024,
        )
        .map_err(|_| "Cannot read data source CA certificate")?;
        let cert =
            reqwest::Certificate::from_pem(&bytes).map_err(|_| "Invalid source CA certificate")?;
        builder = builder.add_root_certificate(cert);
    }
    url.set_path("/v1/summary");
    let client = builder.build().map_err(|_| "Cannot create data client")?;
    let mut request = client
        .get(url)
        .bearer_auth(token)
        .header(crate::data_api::SERVICES_HEADER, crate::data_api::SERVICES);
    if let Some((minutes, scope)) = selection {
        let scope = if scope == crate::traffic::TrafficScope::Model {
            "model"
        } else {
            "all"
        };
        request = request.header("x-coport-window", format!("{minutes}:{scope}"));
    }
    let mut response = request
        .send()
        .await
        .map_err(|_| "Data connection failed; check connectivity and HTTPS certificate")?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(
            "Data endpoint rejected the request; check its credentials and configuration.".into(),
        );
    }
    if response
        .headers()
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .is_none_or(|h| !h.starts_with("application/json"))
    {
        return Err("Unexpected data response type.".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Cannot read data summary")?
    {
        if chunk.len() > (1024usize * 1024).saturating_sub(bytes.len()) {
            return Err("Data response exceeds size limit.".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    let summary: Summary =
        serde_json::from_slice(&bytes).map_err(|_| "Invalid processed summary schema")?;
    summary.validate()?;
    Ok(summary)
}
fn lan(ip: IpAddr) -> bool {
    ip.is_loopback() || matches!(ip,IpAddr::V4(ip) if ip.is_private())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn url_configuration_cannot_embed_credentials_or_arbitrary_paths() {
        let make = |url: &str| Source {
            name: "test".into(),
            transport: Transport::Http,
            ssh_connection: None,
            url: url.into(),
            token_env: Some("TEST_KEY".into()),
            token_file: None,
            ca_certificate: None,
            device_id: None,
            ssh_device: None,
        };
        for url in [
            "file:///etc/passwd",
            "http://user:password@localhost",
            "https://example.com/logs",
            "https://example.com/?key=secret",
        ] {
            assert!(make(url).validate().is_err());
        }
        assert!(make("http://10.42.0.196:8788").validate().is_ok());
        assert!(make("https://example.com:8788").validate().is_ok());
        assert!(!lan("8.8.8.8".parse().unwrap()));
        assert!(!lan("169.254.169.254".parse().unwrap()));
    }
}
