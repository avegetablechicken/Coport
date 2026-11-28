//! Explicit, configured third-party Claude API upstreams.

/// A target, without its scope, naming an explicit upstream.
pub(crate) fn explicit_target(target: &str) -> Option<&str> {
    target.starts_with("/https://").then_some(target)
}

pub(crate) fn oauth_path(url: &url::Url) -> bool {
    let path = percent_encoding::percent_decode_str(url.path()).decode_utf8_lossy();
    path.ends_with("/api/oauth") || path.contains("/api/oauth/")
}

pub(crate) fn check_api_base(url: &url::Url) -> crate::Result<()> {
    let token = url::Url::parse(crate::claude::TOKEN_REFRESH_UPSTREAM).unwrap();
    if oauth_path(url) || crate::routing::by_url::overlaps(url, &token) {
        return Err(crate::Error::config(
            "Claude API upstream overlaps an OAuth-only endpoint.",
        ));
    }
    Ok(())
}

pub(crate) fn validate_api_upstream(value: &str) -> crate::Result<url::Url> {
    let url = crate::routing::by_url::validate_upstream(value)?;
    check_api_base(&url)?;
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_ip_routes_reject_private_and_malformed_destinations() {
        assert!(validate_api_upstream("https://182.92.106.196:6060").is_ok());
        assert!(validate_api_upstream("https://gateway.invalid/anthropic").is_ok());
        for base in [
            "http://182.92.106.196:6060",
            "https://127.0.0.1",
            "https://2130706433",
            "https://10.0.0.1",
            "https://169.254.169.254",
            "https://100.64.0.1",
            "https://198.18.0.1",
            "https://224.0.0.1",
            "https://192.0.2.1",
            "https://user:secret@182.92.106.196:6060",
            "https://182.92.106.196:6060?q=1",
        ] {
            assert!(validate_api_upstream(base).is_err(), "{base}");
        }
    }
}
