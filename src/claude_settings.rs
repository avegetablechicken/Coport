//! Named Claude settings files, resolved only inside configured directories.
use crate::{Error, Result, config::expand};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

pub(crate) fn is_url(name: &str) -> bool {
    (!name.ends_with(".json") || name.contains(['/', ':', '\\']))
        && crate::url_routing::is_url_selector(name)
}

pub(crate) fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.trim() != name
        || name.contains(['/', '\\', ':'])
        || name.chars().any(char::is_control)
    {
        return Err(Error::config(
            "Claude settings selectors must be file names, not paths.",
        ));
    }
    Ok(())
}

pub(crate) struct Profile {
    pub token: String,
    pub upstream: String,
}

/// How long a search result is reused. Requests resolve selectors on every
/// call and the default directory holds all of Claude Code's data, so it is
/// not walked each time; the selected file itself is still read every time.
#[cfg(not(test))]
const SEARCH_TTL: Duration = Duration::from_secs(10);
#[cfg(test)]
const SEARCH_TTL: Duration = Duration::ZERO;

/// Search results by directories and selector, with their expiry.
type Searches = BTreeMap<(Vec<String>, String), (Instant, Option<PathBuf>)>;

pub(crate) fn load(directories: &[String], name: &str) -> Result<Option<Profile>> {
    load_within(directories, name, SEARCH_TTL)
}

fn load_within(directories: &[String], name: &str, ttl: Duration) -> Result<Option<Profile>> {
    static SEARCHES: Mutex<Searches> = Mutex::new(BTreeMap::new());
    validate_name(name)?;
    let key = (directories.to_vec(), name.to_owned());
    let cached = {
        let mut searches = SEARCHES.lock().unwrap();
        let now = Instant::now();
        searches.retain(|_, (expires, _)| *expires > now);
        searches.get(&key).map(|(_, path)| path.clone())
    };
    // A selected file that is gone is searched for again at once.
    let path = match cached {
        Some(path) if path.as_ref().is_none_or(|p| p.is_file()) => path,
        _ => {
            let path = find(directories, name)?;
            if !ttl.is_zero() {
                SEARCHES
                    .lock()
                    .unwrap()
                    .insert(key, (Instant::now() + ttl, path.clone()));
            }
            path
        }
    };
    let Some(path) = path else {
        return Ok(None);
    };
    read(&path).map(Some)
}

fn find(directories: &[String], name: &str) -> Result<Option<PathBuf>> {
    let mut settings = std::collections::BTreeSet::new();
    let mut exact = std::collections::BTreeSet::new();
    let mut pending: Vec<_> = directories.iter().map(|d| expand(d)).collect();
    let mut visited = std::collections::HashSet::new();
    while let Some(directory) = pending.pop() {
        let canonical = match std::fs::canonicalize(&directory) {
            Ok(path) => path,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(Error::config("Cannot read Claude settings directory.")),
        };
        if !visited.insert(canonical) {
            continue;
        }
        // Claude Code creates and removes files here while it runs; an entry
        // that disappears during the walk is skipped rather than failing.
        let gone = |e: &std::io::Error| e.kind() == std::io::ErrorKind::NotFound;
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(e) if gone(&e) => continue,
            Err(_) => return Err(Error::config("Cannot read Claude settings directory.")),
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) if gone(&e) => continue,
                Err(_) => return Err(Error::config("Cannot read Claude settings directory.")),
            };
            let kind = match entry.file_type() {
                Ok(kind) => kind,
                Err(e) if gone(&e) => continue,
                Err(_) => return Err(Error::config("Cannot read Claude settings entry.")),
            };
            if kind.is_dir() {
                // Repository internals never hold settings, and plugin
                // marketplaces under the default directory are clones.
                if entry.file_name() != ".git" {
                    pending.push(entry.path());
                }
                continue;
            }
            // Never follow symlinks out of the configured tree or into cycles.
            if !kind.is_file() {
                continue;
            }
            let filename = entry.file_name();
            let Some(filename) = filename.to_str() else {
                continue;
            };
            let Some(stem) = filename.strip_suffix(".json") else {
                continue;
            };
            let settings_match = filename.contains("settings") && filename.contains(name);
            let exact_match = stem == name || filename == name;
            if !settings_match && !exact_match {
                continue;
            }
            let path = match std::fs::canonicalize(entry.path()) {
                Ok(path) => path,
                Err(e) if gone(&e) => continue,
                Err(_) => return Err(Error::config("Cannot read Claude settings file.")),
            };
            if settings_match {
                settings.insert(path.clone());
            }
            if exact_match {
                exact.insert(path);
            }
        }
    }
    let matches = if settings.is_empty() { exact } else { settings };
    if matches.len() > 1 {
        return Err(Error::config(
            "Multiple Claude settings files match the API selector; use a more specific filename.",
        ));
    }
    Ok(matches.into_iter().next())
}

fn read(path: &Path) -> Result<Profile> {
    let text = std::fs::read_to_string(path)
        .map_err(|_| Error::config("Cannot read Claude settings file."))?;
    let document: Value =
        serde_json::from_str(&text).map_err(|_| Error::config("Invalid Claude settings JSON."))?;
    let field = |name: &str| {
        document
            .get("env")
            .and_then(|v| v.get(name))
            .or_else(|| document.get(name))
    };
    let token = field("ANTHROPIC_AUTH_TOKEN")
        .or_else(|| field("ANTHROPIC_API_KEY"))
        .and_then(Value::as_str)
        .filter(|s| crate::identity::valid_token(s))
        .ok_or(Error::config(
            "Claude settings require ANTHROPIC_AUTH_TOKEN or ANTHROPIC_API_KEY.",
        ))?;
    let upstream = field("ANTHROPIC_BASE_URL")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(Error::config("Claude settings require ANTHROPIC_BASE_URL."))?;
    let upstream = crate::config::unwrap_upstream(upstream)?;
    Ok(Profile {
        token: token.into(),
        upstream,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn write(path: &std::path::Path, host: &str) {
        std::fs::write(path, format!(r#"{{"env":{{"ANTHROPIC_BASE_URL":"https://{host}","ANTHROPIC_API_KEY":"test-key"}}}}"#)).unwrap();
    }
    #[test]
    fn recursive_settings_priority_ambiguity_and_invalid_files() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = vec![dir.path().to_string_lossy().into_owned()];
        std::fs::create_dir(dir.path().join("profiles")).unwrap();
        write(&dir.path().join("profiles/api.json"), "exact.invalid");
        for name in ["api", "api.json"] {
            assert_eq!(
                load(&dirs, name).unwrap().unwrap().upstream,
                "https://exact.invalid"
            );
        }
        assert!(load(&dirs, "missing").unwrap().is_none());
        assert!(load(&dirs, "../api").is_err());
        write(&dir.path().join("settings-api.json"), "settings.invalid");
        assert_eq!(
            load(&dirs, "api").unwrap().unwrap().upstream,
            "https://settings.invalid"
        );
        write(
            &dir.path().join("profiles/api-settings.json"),
            "duplicate.invalid",
        );
        assert!(load(&dirs, "api").is_err());
        std::fs::remove_file(dir.path().join("profiles/api-settings.json")).unwrap();
        for invalid in [
            "broken",
            "{}",
            r#"{"env":{"ANTHROPIC_BASE_URL":"http://127.0.0.1:8787","ANTHROPIC_API_KEY":"key"}}"#,
        ] {
            std::fs::write(dir.path().join("settings-api.json"), invalid).unwrap();
            assert!(load(&dirs, "api").is_err());
        }
    }
    #[test]
    fn fields_precedence_root_format_and_url_classification() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = vec![dir.path().to_string_lossy().into_owned()];
        let path = dir.path().join("api.json");
        std::fs::write(
            &path,
            r#"{"ANTHROPIC_BASE_URL":"https://root.invalid","ANTHROPIC_API_KEY":"root-key"}"#,
        )
        .unwrap();
        assert_eq!(load(&dirs, "api").unwrap().unwrap().token, "root-key");
        std::fs::write(&path, r#"{"ANTHROPIC_API_KEY":"root-key","env":{"ANTHROPIC_AUTH_TOKEN":"auth-token","ANTHROPIC_API_KEY":"api-key","ANTHROPIC_BASE_URL":"https://env.invalid"}}"#).unwrap();
        let profile = load(&dirs, "api").unwrap().unwrap();
        assert_eq!(profile.token, "auth-token");
        assert_eq!(profile.upstream, "https://env.invalid");
        assert!(!is_url("api.json"));
        assert!(!is_url("work.settings.json"));
        assert!(is_url("https://provider.invalid/api.json"));
        assert!(is_url("api.example.com"));
    }

    #[test]
    fn searches_are_reused_but_files_are_read_each_time() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = vec![dir.path().to_string_lossy().into_owned()];
        let ttl = Duration::from_secs(60);
        write(&dir.path().join("cached.json"), "first.invalid");
        assert_eq!(
            load_within(&dirs, "cached", ttl).unwrap().unwrap().upstream,
            "https://first.invalid"
        );
        // Edits to the selected file apply at once; a new rival file is only
        // found by a later search.
        write(&dir.path().join("cached.json"), "edited.invalid");
        write(&dir.path().join("settings-cached.json"), "rival.invalid");
        assert_eq!(
            load_within(&dirs, "cached", ttl).unwrap().unwrap().upstream,
            "https://edited.invalid"
        );
        // A removed selection is searched for again immediately.
        std::fs::remove_file(dir.path().join("cached.json")).unwrap();
        assert_eq!(
            load_within(&dirs, "cached", ttl).unwrap().unwrap().upstream,
            "https://rival.invalid"
        );
    }

    #[test]
    fn repositories_are_not_searched() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = vec![dir.path().to_string_lossy().into_owned()];
        let objects = dir.path().join("plugins/marketplace/.git/refs");
        std::fs::create_dir_all(&objects).unwrap();
        write(&objects.join("settings-api.json"), "git.invalid");
        assert!(load(&dirs, "api").unwrap().is_none());
        write(
            &dir.path().join("plugins/marketplace/api.json"),
            "plugin.invalid",
        );
        assert_eq!(
            load(&dirs, "api").unwrap().unwrap().upstream,
            "https://plugin.invalid"
        );
    }

    #[test]
    fn multiple_directories_and_overlapping_roots() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        write(&a.path().join("api.json"), "a.invalid");
        write(&b.path().join("api.json"), "b.invalid");
        let a = a.path().to_string_lossy().into_owned();
        let b = b.path().to_string_lossy().into_owned();
        assert!(load(&[a.clone(), b], "api").is_err());
        assert!(load(&[a.clone(), a], "api").unwrap().is_some());
    }
}
