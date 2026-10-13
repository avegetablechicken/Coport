//! Display identity for API configurations whose name or base URL changed.
//! Logs keep what was used at the time; matching happens only when reading.
use crate::logs::Entry;
use coport::config::Config;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

pub const UNIDENTIFIED: &str = "Unidentified";

/// A choice for traffic logged under a configuration name and base URL that
/// no longer exactly match a current configuration. Logs are never rewritten.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrafficAssignment {
    pub service: String,
    pub name: String,
    pub base: Option<String>,
    /// The configuration to count it under; `None` keeps it Unidentified.
    pub target: Option<TrafficTarget>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrafficTarget {
    pub name: String,
    pub base: String,
}

/// The traffic compatibility file: the user's choices, kept apart from the
/// GUI settings and the proxy configuration.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Compatibility {
    pub assignments: Vec<TrafficAssignment>,
}

impl Compatibility {
    /// A missing file holds no choices. An unreadable one is an error, so it
    /// is never replaced by a partial set of choices.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|_| "The traffic compatibility file is invalid".to_owned()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(_) => Err("Cannot read the traffic compatibility file".to_owned()),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        coport_gui::settings::write_private(path, &bytes)
            .map_err(|_| "Cannot save the traffic compatibility file".to_owned())
    }

    /// Replaces the choice for one logged identity; without `choice`, it is
    /// matched automatically again.
    pub fn assign(
        &mut self,
        service: String,
        name: String,
        base: Option<String>,
        choice: Option<Option<TrafficTarget>>,
    ) {
        self.assignments
            .retain(|a| (&a.service, &a.name, &a.base) != (&service, &name, &base));
        if let Some(target) = choice {
            self.assignments.push(TrafficAssignment {
                service,
                name,
                base,
                target,
            });
        }
    }
}

/// Why logged traffic is counted under a configuration it does not exactly
/// match, or why it could not be matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Reason {
    Renamed,
    BaseChanged,
    Legacy,
    Ambiguous,
    Unmatched,
}

/// The configuration name and canonical base URL a request was logged with.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Source {
    pub name: String,
    pub base: Option<String>,
}

pub struct Resolution {
    pub label: String,
    /// Logged identity that is not an exact current match. Its reason is
    /// `None` once the user's choice applies exactly.
    pub source: Option<(Source, Option<Reason>)>,
}

/// A current configuration that historical traffic can be assigned to.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    service: String,
    name: String,
    base: String,
    label: String,
}

#[derive(Default)]
pub struct Identities {
    names: BTreeMap<(String, String), BTreeSet<String>>,
    assignments: BTreeMap<(String, String, Option<String>), Option<TrafficTarget>>,
}

// Match complete bases, including path and non-default port. Never keep URL
// credentials, queries or fragments in an identity or its display label.
fn canonical(base: &str) -> Option<String> {
    let base = coport::config::unwrap_upstream(base).ok()?;
    let url = reqwest::Url::parse(&base).ok()?;
    Some(url.as_str().trim_end_matches('/').to_owned())
}

impl Identities {
    pub fn from_config(config: &Config, assignments: &[TrafficAssignment]) -> Self {
        let mut result = Self::default();
        for provider in &config.codex.providers {
            for base in provider
                .upstreams(&config.codex.base_url.api_key, &config.codex)
                .into_iter()
                .flatten()
            {
                result.insert("Codex", &base, provider.label());
            }
        }
        for name in config.codex.routing.api_key.keys() {
            if config.codex.api_key_kind(name).ok() == Some("gateway") {
                result.insert("Codex", &format_base(name), name);
            }
        }
        for name in config.claude.routing.api_key.keys() {
            match config.claude.api_key_upstream(name) {
                Ok(Some(base)) => result.insert("Claude", &base, name),
                Ok(None) => result.insert("Claude", &format_base(name), name),
                Err(_) => {}
            }
        }
        if config.codex.routing.api_key_fallback.is_some() {
            result.insert("Codex", &config.codex.base_url.api_key, "api_key_fallback");
        }
        if config.claude.routing.api_key_fallback.is_some() {
            result.insert("Claude", &config.claude.base_url, "api_key_fallback");
        }
        result.assignments = assignments
            .iter()
            .map(|a| {
                (
                    (a.service.clone(), a.name.clone(), a.base.clone()),
                    a.target.clone(),
                )
            })
            .collect();
        result
    }

    fn insert(&mut self, service: &str, base: &str, name: &str) {
        if let Some(base) = canonical(base) {
            self.names
                .entry((service.to_owned(), base))
                .or_default()
                .insert(name.to_owned());
        }
    }

    pub fn targets(&self) -> Vec<Target> {
        self.names
            .iter()
            .flat_map(|((service, base), names)| {
                names.iter().map(|name| Target {
                    service: service.clone(),
                    name: name.clone(),
                    base: base.clone(),
                    label: self.label(service, base, name),
                })
            })
            .collect()
    }

    /// `labels` maps logged names to current routing names. Account traffic
    /// and requests without a logged configuration are left to the caller.
    pub fn resolve(
        &self,
        entry: &Entry,
        service: &str,
        labels: &BTreeMap<(String, String), String>,
    ) -> Option<Resolution> {
        // Account identities remain separate even when they use the same API.
        if entry.get("account_id").is_some_and(|id| !id.is_empty()) {
            return None;
        }
        let logged = entry.get("provider").unwrap_or_default();
        let base = match entry.get("upstream_base_url") {
            Some(raw) => match canonical(raw) {
                Some(base) => Some(base),
                // Never show or store an unusable, possibly secret, URL.
                None => {
                    return Some(Resolution {
                        label: UNIDENTIFIED.into(),
                        source: None,
                    });
                }
            },
            None if logged.is_empty() => return None,
            None => None,
        };
        let source = Source {
            name: logged.to_owned(),
            base,
        };
        let key = (service.to_owned(), source.name.clone(), source.base.clone());
        let (label, reason) = match self.assignments.get(&key) {
            Some(None) => (UNIDENTIFIED.into(), None),
            Some(Some(target)) => self.infer(service, &target.name, Some(&target.base), labels),
            None => {
                let name = labels
                    .get(&(service.to_owned(), logged.to_owned()))
                    .map_or(logged, String::as_str);
                match self.infer(service, name, source.base.as_deref(), labels) {
                    (label, None) => {
                        return Some(Resolution {
                            label,
                            source: None,
                        });
                    }
                    inferred => inferred,
                }
            }
        };
        Some(Resolution {
            label,
            source: Some((source, reason)),
        })
    }

    /// An exact name and base match has no reason. A unique base wins over a
    /// name, because the base and its key decide where traffic was sent.
    fn infer(
        &self,
        service: &str,
        name: &str,
        base: Option<&str>,
        labels: &BTreeMap<(String, String), String>,
    ) -> (String, Option<Reason>) {
        if let Some(base) = base
            && let Some(names) = self.names.get(&(service.to_owned(), base.to_owned()))
        {
            return if names.contains(name) {
                (self.label(service, base, name), None)
            } else if let [only] = names.iter().collect::<Vec<_>>()[..] {
                (self.label(service, base, only), Some(Reason::Renamed))
            } else {
                (UNIDENTIFIED.into(), Some(Reason::Ambiguous))
            };
        }
        let reason = if base.is_some() {
            Reason::BaseChanged
        } else {
            Reason::Legacy
        };
        let bases: Vec<_> = self
            .names
            .iter()
            .filter(|((s, _), names)| s == service && names.contains(name))
            .map(|((_, base), _)| base)
            .collect();
        match bases[..] {
            [only] => (self.label(service, only, name), Some(reason)),
            [] if !name.is_empty()
                && labels
                    .iter()
                    .any(|((s, _), label)| s == service && label == name) =>
            {
                (name.to_owned(), Some(reason))
            }
            [] => (UNIDENTIFIED.into(), Some(Reason::Unmatched)),
            _ => (UNIDENTIFIED.into(), Some(Reason::Ambiguous)),
        }
    }

    /// Select proofs from the current configuration after historical name/base
    /// mapping, without making those local names part of the wire format.
    pub(crate) fn provider_reference(
        &self,
        service: &str,
        label: &str,
        references: &[coport::identity::TrafficProviderReference],
    ) -> Option<String> {
        if label == UNIDENTIFIED {
            return None;
        }
        let candidates: BTreeSet<_> = references
            .iter()
            .filter(|r| {
                r.service == service
                    && canonical(&r.upstream)
                        .is_some_and(|base| self.label(service, &base, &r.name) == label)
            })
            .map(|r| r.reference.clone())
            .collect();
        (candidates.len() == 1).then(|| candidates.into_iter().next().unwrap())
    }

    /// Use the same local name and upstream disambiguation for peer proofs as
    /// for this device's own Traffic rows. No peer-supplied labels are used.
    pub(crate) fn provider_label(
        &self,
        provider: &coport::identity::TrafficProviderReference,
    ) -> Option<String> {
        let base = canonical(&provider.upstream)?;
        Some(self.label(&provider.service, &base, &provider.name))
    }

    fn label(&self, service: &str, base: &str, name: &str) -> String {
        // A selector can resolve to different bases in different configured homes.
        // Keep those bases distinct even though their current names coincide.
        let shared_name = self
            .names
            .iter()
            .any(|((other_service, other_base), names)| {
                other_service == service && other_base != base && names.contains(name)
            });
        if shared_name {
            format!("{name} ({base})")
        } else {
            name.to_owned()
        }
    }
}

fn format_base(name: &str) -> String {
    if name.contains("://") {
        name.to_owned()
    } else if name.starts_with("//") {
        format!("https:{name}")
    } else {
        format!("https://{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatibility_file_keeps_one_choice_per_source_and_never_replaces_an_invalid_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("traffic-compatibility.json");
        assert!(Compatibility::load(&path).unwrap().assignments.is_empty());
        let target = || {
            Some(TrafficTarget {
                name: "main".into(),
                base: "https://api.example.com/v1".into(),
            })
        };
        let mut file = Compatibility::default();
        file.assign("Codex".into(), "old".into(), None, Some(target()));
        file.assign("Codex".into(), "old".into(), None, Some(None));
        file.assign("Codex".into(), "gone".into(), None, Some(target()));
        file.save(&path).unwrap();
        let mut loaded = Compatibility::load(&path).unwrap();
        assert_eq!(loaded.assignments.len(), 2);
        assert!(loaded.assignments[0].target.is_none());
        loaded.assign("Codex".into(), "gone".into(), None, None);
        assert_eq!(loaded.assignments.len(), 1);
        for invalid in ["{", "{\r\n  \"assignments\": 3\r\n}\r\n"] {
            std::fs::write(&path, invalid).unwrap();
            assert!(Compatibility::load(&path).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
        }
        // Hand edits may use CRLF line endings.
        std::fs::write(&path, "{\r\n  \"assignments\": []\r\n}\r\n").unwrap();
        assert!(Compatibility::load(&path).unwrap().assignments.is_empty());
    }

    #[test]
    fn normalizes_wrapper_host_port_and_trailing_slash_without_losing_path() {
        assert_eq!(
            canonical("http://127.0.0.1:7889/https://API.example.com:443/v1/"),
            canonical("https://api.example.com/v1")
        );
        for base in [
            "https://api.example.com/v2",
            "https://api.example.com:8443/v1",
        ] {
            assert_ne!(canonical(base), canonical("https://api.example.com/v1"));
        }
        for base in [
            "https://user:secret@api.example.com/v1",
            "https://api.example.com/v1?key=secret",
            "https://api.example.com/v1#secret",
            "broken",
        ] {
            assert_eq!(canonical(base), None);
        }
    }
}
