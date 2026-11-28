//! Codex's dotenvy 0.15.7 + set_filtered semantics in an isolated environment.
//! Parser/quoted-line implementation is vendored under MIT (see LICENSE), with
//! only environment lookup redirected to a map. Updating that map after each
//! accepted entry mirrors Codex's set_var before the iterator's next entry.
mod iter;
mod parse;

use std::{collections::HashMap, path::Path};
pub(crate) type Environment = HashMap<String, String>;

pub(crate) fn inherited() -> Environment {
    std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .collect()
}

pub(crate) fn load(path: &Path, inherited: &Environment) -> Environment {
    let Ok(file) = std::fs::File::open(path) else {
        return inherited.clone();
    };
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return inherited.clone();
    }
    let mut iter = iter::Iter::new(file, inherited.clone());
    // Codex ignores per-entry parse errors, including an invalid first BOM line.
    while iter.next().is_some() {}
    iter.environment
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simulated_loading_matches_codex_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        let inherited = Environment::from([
            ("BASE".into(), "process-base".into()),
            ("KEY".into(), "process-key".into()),
            ("CODEX_FILTERED".into(), "process-protected".into()),
        ]);
        // Expected values were also verified against Codex's dotenvy loading
        // loop. Keep the test deterministic and entirely in process.
        let cases: &[(&str, &[(&str, &str)])] = &[
            (
                "export BASE=file-base\nKEY=first\nKEY=last\nCOPY=${BASE}/${KEY}\nEMPTY=\nQUOTED='literal$BASE#value' # comment\nESCAPED=\"line\\nnext\"\n",
                &[
                    ("BASE", "file-base"),
                    ("KEY", "last"),
                    ("COPY", "file-base/last"),
                    ("EMPTY", ""),
                    ("QUOTED", "literal$BASE#value"),
                    ("ESCAPED", "line\nnext"),
                ],
            ),
            (
                "BASE=first\nBASE=${BASE}-second\nCOPY=${BASE}\nnot valid\nKEY=after-error\nAFTER=ok\n",
                &[
                    ("BASE", "first-second"),
                    ("COPY", "first-second"),
                    ("KEY", "after-error"),
                    ("AFTER", "ok"),
                ],
            ),
            (
                "CODEX_FILTERED=file-protected\ncodex_lower=filtered\nFILTER_COPY=${CODEX_FILTERED}\nCOPY=${codex_lower}\n",
                &[("FILTER_COPY", "process-protected"), ("COPY", "filtered")],
            ),
            (
                "\u{feff}BOM=ignored\r\nKEY=windows-line\r\nQUOTED=\"multi\nline\"\n",
                &[("KEY", "windows-line"), ("QUOTED", "multi\nline")],
            ),
            ("KEY=valid\nAFTER='unterminated\n", &[("KEY", "valid")]),
        ];
        for (fixture, changes) in cases {
            std::fs::write(&path, fixture).unwrap();
            let mut expected = inherited.clone();
            expected.extend(
                changes
                    .iter()
                    .map(|(key, value)| (key.to_string(), value.to_string())),
            );
            assert_eq!(load(&path, &inherited), expected, "fixture: {fixture}");
        }
        assert_eq!(load(&dir.path().join("absent"), &inherited), inherited);
        assert_eq!(load(dir.path(), &inherited), inherited);
    }

    #[tokio::test]
    async fn same_config_observes_dotenv_changes_without_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        let yaml = format!(
            "listen_port: 8787\nrequest_timeout_seconds: 3\ncodex:\n  homes: [{}]\n  routing:\n    api_key: {{CAP_LIVE_KEY: none}}\n",
            serde_json::to_string(dir.path()).unwrap()
        );
        let config = crate::config::Config::parse(&yaml).unwrap();
        std::fs::write(&path, "CAP_LIVE_KEY=first\n").unwrap();
        assert!(config.resolve(Some("Bearer first"), false).await.is_ok());
        std::fs::write(&path, "CAP_LIVE_KEY=second\n").unwrap();
        assert!(config.resolve(Some("Bearer second"), false).await.is_ok());
        assert_eq!(
            config
                .resolve(Some("Bearer first"), false)
                .await
                .err()
                .unwrap()
                .status,
            401
        );
        assert!(std::env::var_os("CAP_LIVE_KEY").is_none());
        std::fs::write(&path, "CAP_LIVE_KEY=\n").unwrap();
        assert!(config.resolve(Some("Bearer second"), false).await.is_err());
    }
}
