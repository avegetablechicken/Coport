//! Routing by explicitly configured API upstream URL.
use crate::{Error, Result};
use std::collections::HashSet;
use url::Url;

pub(crate) fn is_url_selector(value: &str) -> bool {
    // Plain provider/environment names stay credential selectors. Hostnames,
    // IPs and address/path forms are HTTPS upstreams even without a scheme.
    value.contains(['.', ':', '/'])
}

/// Two HTTPS route bases overlap if either covers the other's path on the
/// same origin, as a server that decodes the path and merges repeated
/// slashes sees it. A dot segment that only decoding reveals could mean
/// anything upstream, so it overlaps everything.
pub(crate) fn overlaps(a: &Url, b: &Url) -> bool {
    fn segments(url: &Url) -> Option<Vec<String>> {
        let path = percent_encoding::percent_decode_str(url.path()).decode_utf8_lossy();
        let segments: Vec<String> = path
            .split('/')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        (!segments.iter().any(|s| s == "." || s == "..")).then_some(segments)
    }
    if a.origin() != b.origin() {
        return false;
    }
    let (Some(a), Some(b)) = (segments(a), segments(b)) else {
        return true;
    };
    a.starts_with(&b) || b.starts_with(&a)
}

/// Rejects two URL selectors that normalize to the same base.
pub(crate) fn validate_routes<'a>(bases: impl IntoIterator<Item = &'a str>) -> Result<()> {
    let mut seen = HashSet::new();
    for base in bases {
        let url = validate_upstream(base)?;
        if !seen.insert(url.as_str().trim_end_matches('/').to_owned()) {
            return Err(Error::config("Duplicate API upstream URL routes."));
        }
    }
    Ok(())
}

/// The longest URL route base covering an explicit `/https://` target, with
/// its validated upstream.
pub(crate) fn match_route<'a, C>(
    routes: impl IntoIterator<Item = (&'a str, C)>,
    target: &str,
) -> Result<Option<(&'a str, C, Url)>> {
    if !target.starts_with("/https://") {
        return Ok(None);
    }
    let dest =
        Url::parse(&target[1..]).map_err(|_| Error::new(400, "Invalid explicit API target."))?;
    let mut selected: Option<(&str, C, Url, usize)> = None;
    for (base, choice) in routes {
        let url = validate_upstream(base)?;
        let root = url.path().trim_end_matches('/');
        let length = root.len();
        let covers = dest.scheme() == url.scheme()
            && dest.host_str() == url.host_str()
            && dest.port_or_known_default() == url.port_or_known_default()
            && (dest.path() == root || dest.path().starts_with(&format!("{root}/")));
        if covers && selected.as_ref().is_none_or(|s| length > s.3) {
            selected = Some((base, choice, url, length));
        }
    }
    let Some((base, choice, url, _)) = selected else {
        return Ok(None);
    };
    crate::routing::upstream_url(url.as_str(), target, false)?;
    Ok(Some((base, choice, url)))
}

// A public IP is allowed only as a configured API route destination. This
// does not enable arbitrary URL forwarding or weaken the other service bases.
pub(crate) fn validate_upstream(value: &str) -> Result<Url> {
    let normalized = if value.contains("://") {
        value.to_owned()
    } else if value.starts_with("//") {
        format!("https:{value}")
    } else {
        format!("https://{value}")
    };
    let u = Url::parse(&normalized).map_err(|_| Error::config("Invalid API upstream."))?;
    if let Some(url::Host::Ipv4(ip)) = u.host() {
        let [a, b, c, _] = ip.octets();
        if u.scheme() == "https"
            && u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none()
            && u.port() != Some(0)
            && !ip.is_private()
            && !ip.is_loopback()
            && !ip.is_link_local()
            && !ip.is_documentation()
            && a != 0
            && a < 224
            && !(a == 100 && (64..=127).contains(&b))
            && !(a == 192 && b == 0 && c == 0)
            && !(a == 198 && (18..=19).contains(&b))
        {
            return Ok(u);
        }
        return Err(Error::config(
            "API IP upstream must be a public HTTPS address.",
        ));
    }
    crate::config::validate_upstream(&normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Choice;
    use std::collections::BTreeMap;

    fn urls(routes: &BTreeMap<String, Choice>) -> impl Iterator<Item = (&str, &Choice)> {
        routes.iter().map(|(s, c)| (s.as_str(), c))
    }
    fn bases<'a>(bases: &[&'a str]) -> impl Iterator<Item = &'a str> {
        bases.iter().copied()
    }
    #[test]
    fn schemeless_routes_default_to_https_and_detect_equivalent_duplicates() {
        for (input, normalized) in [
            ("api.example.com", "https://api.example.com/"),
            ("api.example.com:8443/v1", "https://api.example.com:8443/v1"),
            ("182.92.106.196:6060", "https://182.92.106.196:6060/"),
            ("//api.example.com/v1", "https://api.example.com/v1"),
        ] {
            assert!(is_url_selector(input));
            assert_eq!(validate_upstream(input).unwrap().as_str(), normalized);
        }
        for name in [
            "ShareCoder",
            "openai",
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
        ] {
            assert!(!is_url_selector(name));
        }
        let routes = BTreeMap::from([("api.example.com/v1".into(), Choice::direct())]);
        assert!(
            match_route(urls(&routes), "/https://api.example.com/v1/responses")
                .unwrap()
                .is_some()
        );
        assert!(
            match_route(urls(&routes), "/https://api.example.com/v1-evil/responses")
                .unwrap()
                .is_none()
        );
        assert!(
            validate_routes(bases(&[
                "api.example.com/v1",
                "https://api.example.com:443/v1/"
            ]))
            .is_err()
        );
        for value in [
            "127.0.0.1:8787",
            "localhost:8787",
            "10.0.0.1/v1",
            "api.example.com/v1?key=secret",
            "api.example.com/v1#fragment",
        ] {
            assert!(validate_upstream(value).is_err(), "{value}");
        }
    }
    #[test]
    fn host_only_routes_are_less_specific_than_explicit_api_paths() {
        let routes = BTreeMap::from([
            ("api.example.com".into(), Choice::One("jp".into())),
            ("api.example.com/v1".into(), Choice::One("jp_lab".into())),
            ("api.example.com/v1/models".into(), Choice::direct()),
        ]);
        for (path, proxy) in [
            ("/responses", "jp"),
            ("/v1/responses", "jp_lab"),
            ("/v1/models", "none"),
            ("/v1/models/example", "none"),
            ("/v1-other/responses", "jp"),
        ] {
            assert_eq!(
                match_route(urls(&routes), &format!("/https://api.example.com{path}"))
                    .unwrap()
                    .unwrap()
                    .1
                    .label(),
                proxy
            );
        }
        assert!(
            match_route(urls(&routes), "/https://api.example.com:8443/v1/responses")
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn matching_requires_origin_and_path_boundaries_and_prefers_the_longest_base() {
        let routes = BTreeMap::from([
            ("https://api.invalid/v1".into(), Choice::One("jp".into())),
            ("https://api.invalid/v1/special".into(), Choice::direct()),
        ]);
        assert_eq!(
            match_route(
                urls(&routes),
                "/https://api.invalid/v1/responses?stream=true"
            )
            .unwrap()
            .unwrap()
            .1
            .label(),
            "jp"
        );
        assert_eq!(
            match_route(urls(&routes), "/https://api.invalid/v1/special/responses")
                .unwrap()
                .unwrap()
                .1
                .label(),
            "none"
        );
        for target in [
            "/https://api.invalid:444/v1/responses",
            "/https://api.invalid/v1-evil/responses",
            "/https://evil.invalid/v1/responses",
            "/v1/responses",
        ] {
            assert!(match_route(urls(&routes), target).unwrap().is_none());
        }
        assert!(match_route(urls(&routes), "/https://api.invalid/v1/%2e%2e/v1/responses").is_err());
        assert!(match_route(urls(&routes), "/https://secret@api.invalid/v1/responses").is_err());
    }
    #[test]
    fn explicit_routes_accept_public_ips_and_reject_invalid_or_duplicate_bases() {
        assert!(validate_upstream("https://182.92.106.196:6060").is_ok());
        for base in [
            "http://api.invalid/v1",
            "https://127.0.0.1/v1",
            "https://2130706433/v1",
            "https://10.0.0.1",
            "https://169.254.169.254",
            "https://100.64.0.1",
            "https://224.0.0.1",
            "https://api.invalid/v1?q=secret",
        ] {
            assert!(validate_upstream(base).is_err(), "{base}");
        }
        assert!(
            validate_routes(bases(&[
                "https://api.invalid/v1/",
                "https://api.invalid:443/v1"
            ]))
            .is_err()
        );
    }
    #[test]
    fn overlap_survives_doubled_slashes_and_encoded_segments() {
        let reserved = Url::parse("https://chatgpt.com/backend-api").unwrap();
        for base in [
            "https://chatgpt.com//backend-api",
            "https://chatgpt.com/backend-api//codex",
            "https://chatgpt.com/%2Fbackend-api",
            "https://chatgpt.com/%2fbackend-api/codex",
            "https://chatgpt.com/v1/%2e%2e/backend-api",
            "https://chatgpt.com/",
        ] {
            let url = validate_upstream(base).unwrap();
            assert!(overlaps(&url, &reserved), "{base} -> {url}");
        }
        for base in [
            "https://chatgpt.com/backend-apix",
            "https://chatgpt.com/v1",
            "https://api.openai.com/backend-api",
        ] {
            assert!(
                !overlaps(&validate_upstream(base).unwrap(), &reserved),
                "{base}"
            );
        }
    }
}
