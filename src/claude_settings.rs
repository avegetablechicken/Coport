//! Named Claude settings files, resolved only inside configured directories.
use crate::{Error, Result, config::expand};
use serde_json::Value;

pub(crate) fn is_url(name: &str) -> bool {
    !(name.ends_with(".json") && !name.contains(['/', ':', '\\']))
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

pub(crate) fn load(directories: &[String], name: &str) -> Result<Option<Profile>> {
    validate_name(name)?;
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
                pending.push(entry.path());
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
    let Some(path) = matches.first() else {
        return Ok(None);
    };
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
    Ok(Some(Profile {
        token: token.into(),
        upstream,
    }))
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
