//! API Key Routes, as every App routes them: `routing.api_key` names an
//! upstream URL, whose requests forward any key for the upstream to check,
//! or a selector whose key a request must carry, read where the App keeps
//! it (an environment variable, a Claude settings file, a Codex provider).
//! Each App reads its keys and builds its Route's own part; the matching,
//! its errors and the checks the keys share live here.
use super::by_url;
use crate::{
    Error, Result,
    config::{Choice, Config},
};
use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use url::Url;

/// A Route by API key, with what its App adds (`ext`).
#[derive(Clone, Debug)]
pub struct ApiRoute<E = ()> {
    /// The `routing.api_key` selector the key matched, the URL route that
    /// carried it, or the App's fallback name: records name it as `provider`.
    pub selector: String,
    pub proxy: Choice,
    /// The base URL requests go to.
    pub upstream: String,
    /// The key sent upstream.
    pub key: String,
    /// The configuration names the key, not a URL route or `fallback`.
    pub configured: bool,
    /// An upstream other than the App's own, reached with the platform's TLS.
    pub custom_upstream: bool,
    pub ext: E,
}

/// What an App's errors about its API keys say.
pub(crate) struct Texts {
    /// A key two selectors name.
    pub ambiguous: &'static str,
    /// Two selectors with the same key, found while checking credentials.
    pub duplicate: &'static str,
}

/// What reading one selector's key found about a request's key.
pub(crate) enum Read<T> {
    /// It is this selector's key, with what the App learned reading it.
    Match(T),
    /// It is not.
    Other,
    /// The selector's key could not be read.
    Unavailable,
}

impl Read<()> {
    /// A selector's key as read, compared with the request's.
    pub(crate) fn compare(read: Result<String>, key: &str) -> Self {
        match read {
            Ok(value) if value == key => Self::Match(()),
            Ok(_) => Self::Other,
            Err(_) => Self::Unavailable,
        }
    }
}

/// The one selector a key matched, if any.
pub(crate) struct Found<'a, T> {
    pub named: Option<(&'a String, &'a Choice, T)>,
    /// Some selector's key could not be read, so an unmatched key may still
    /// be one.
    pub unavailable: bool,
}

/// An App's `routing.api_key`, seen as API Key Routes.
pub(crate) struct ApiKeys<'a> {
    routes: &'a BTreeMap<String, Choice>,
    /// Which selectors are upstream URLs.
    is_url: fn(&str) -> bool,
    texts: Texts,
}

impl<'a> ApiKeys<'a> {
    pub(crate) fn new(routes: &'a BTreeMap<String, Choice>, texts: Texts) -> Self {
        Self::with_urls(routes, by_url::is_url_selector, texts)
    }

    /// As `new`, for an App that spells URL selectors its own way.
    pub(crate) fn with_urls(
        routes: &'a BTreeMap<String, Choice>,
        is_url: fn(&str) -> bool,
        texts: Texts,
    ) -> Self {
        Self {
            routes,
            is_url,
            texts,
        }
    }

    pub(crate) fn is_url(&self, selector: &str) -> bool {
        (self.is_url)(selector)
    }

    /// Checks each selector: a URL with `upstream`, any other with `named`;
    /// then its proxy, and that no two URL routes overlap.
    pub(crate) fn validate(
        &self,
        config: &Config,
        upstream: impl Fn(&str) -> Result<()>,
        named: impl Fn(&str) -> Result<()>,
    ) -> Result<()> {
        for (selector, choice) in self.routes {
            if self.is_url(selector) {
                upstream(selector)?;
            } else {
                named(selector)?;
            }
            config.validate_choice(choice)?;
        }
        by_url::validate_routes(&self.url_routes())
    }

    /// The selectors that name a key, not an upstream URL.
    pub(crate) fn named(&self) -> impl Iterator<Item = (&'a String, &'a Choice)> + use<'a> {
        let is_url = self.is_url;
        self.routes.iter().filter(move |(s, _)| !is_url(s))
    }

    fn url_routes(&self) -> BTreeMap<String, Choice> {
        self.routes
            .iter()
            .filter(|(s, _)| self.is_url(s))
            .map(|(s, c)| (s.clone(), c.clone()))
            .collect()
    }

    /// The URL route serving an explicit target: its selector, proxy and
    /// upstream.
    pub(crate) fn url_route(&self, target: &str) -> Result<Option<(String, Choice, Url)>> {
        let routes = self.url_routes();
        let Some((selector, choice)) = by_url::match_route(&routes, target)? else {
            return Ok(None);
        };
        let upstream = by_url::validate_upstream(selector)?;
        Ok(Some((selector.to_owned(), choice.clone(), upstream)))
    }

    /// Display classification: a URL route is a gateway.
    pub(crate) fn kind(&self, selector: &str) -> &'static str {
        if self.is_url(selector) {
            "gateway"
        } else {
            "api_key"
        }
    }

    /// The selector whose key a request carries, by reading each one's.
    /// Two are ambiguous.
    pub(crate) async fn find<T, F, Fut>(&self, mut read: F) -> Result<Found<'a, T>>
    where
        F: FnMut(&'a str) -> Fut,
        Fut: Future<Output = Result<Read<T>>>,
    {
        let mut named = None;
        let mut unavailable = false;
        for (selector, choice) in self.named() {
            match read(selector).await? {
                Read::Match(value) => {
                    if named.is_some() {
                        return Err(Error::new(409, self.texts.ambiguous));
                    }
                    named = Some((selector, choice, value));
                }
                Read::Other => {}
                Read::Unavailable => unavailable = true,
            }
        }
        Ok(Found { named, unavailable })
    }

    /// Each named selector's keys can be read and none is another route's;
    /// `seen` holds the keys other routes of the App carry.
    pub(crate) async fn check_distinct<F, Fut>(
        &self,
        seen: &mut HashSet<String>,
        mut keys: F,
    ) -> Result<()>
    where
        F: FnMut(&'a str) -> Fut,
        Fut: Future<Output = Result<Vec<String>>>,
    {
        for (selector, _) in self.named() {
            for key in keys(selector).await? {
                if !seen.insert(key) {
                    return Err(Error::config(self.texts.duplicate));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXTS: Texts = Texts {
        ambiguous: "ambiguous",
        duplicate: "duplicate",
    };

    fn routes(names: &[&str]) -> BTreeMap<String, Choice> {
        names
            .iter()
            .map(|n| ((*n).to_owned(), Choice::direct()))
            .collect()
    }

    #[tokio::test]
    async fn a_key_matches_one_named_selector_and_url_routes_are_not_read() {
        let routes = routes(&["ONE", "TWO", "https://gateway.example"]);
        let keys = ApiKeys::new(&routes, TEXTS);
        let mut read = Vec::new();
        let found = keys
            .find(|selector| {
                read.push(selector.to_owned());
                async move {
                    Ok(match selector {
                        "TWO" => Read::Match(2),
                        _ => Read::Other,
                    })
                }
            })
            .await
            .unwrap();
        assert_eq!(
            found.named.map(|(s, _, v)| (s.as_str(), v)),
            Some(("TWO", 2))
        );
        assert!(!found.unavailable);
        assert_eq!(read, ["ONE", "TWO"]);
    }

    #[tokio::test]
    async fn two_matches_are_ambiguous_and_unreadable_keys_are_reported() {
        let routes = routes(&["ONE", "TWO", "THREE"]);
        let keys = ApiKeys::new(&routes, TEXTS);
        let both = keys
            .find(|s| async move {
                Ok(if s == "THREE" {
                    Read::Unavailable
                } else {
                    Read::Match(())
                })
            })
            .await;
        assert!(both.is_err_and(|e| e.status == 409));
        let none = keys
            .find(|s| async move {
                Ok(if s == "THREE" {
                    Read::Unavailable
                } else {
                    Read::<()>::Other
                })
            })
            .await
            .unwrap();
        assert!(none.named.is_none() && none.unavailable);
    }

    #[tokio::test]
    async fn keys_must_be_distinct_from_each_other_and_from_other_routes() {
        let routes = routes(&["ONE", "TWO"]);
        let keys = ApiKeys::new(&routes, TEXTS);
        let mut seen = HashSet::new();
        keys.check_distinct(&mut seen, |s| async move { Ok(vec![s.to_owned()]) })
            .await
            .unwrap();
        let mut seen = HashSet::from(["account".to_owned()]);
        let clash = keys
            .check_distinct(&mut seen, |s| async move {
                Ok(vec![if s == "TWO" { "account" } else { s }.to_owned()])
            })
            .await;
        assert!(clash.is_err());
    }

    #[test]
    fn an_app_may_spell_url_selectors_its_own_way() {
        let routes = routes(&["profile.json", "https://gateway.example"]);
        let keys = ApiKeys::with_urls(&routes, |s| !s.ends_with(".json"), TEXTS);
        assert_eq!(
            keys.named().map(|(s, _)| s.as_str()).collect::<Vec<_>>(),
            ["profile.json"]
        );
        assert_eq!(keys.kind("profile.json"), "api_key");
        assert_eq!(keys.kind("https://gateway.example"), "gateway");
        let (selector, _, upstream) = keys
            .url_route("/https://gateway.example/v1/messages")
            .unwrap()
            .unwrap();
        assert_eq!(selector, "https://gateway.example");
        assert_eq!(upstream.as_str(), "https://gateway.example/");
    }
}
