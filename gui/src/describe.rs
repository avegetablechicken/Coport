//! Hover details for API key routing selectors: the upstream base URL each
//! one is forwarded to.

use coport::config::{Config, redacted_endpoint};

/// Userinfo is omitted, as for proxy endpoints.
fn display(url: &str) -> String {
    if url.contains('@') {
        redacted_endpoint(url)
    } else {
        url.to_owned()
    }
}

/// Resolved by the core as for requests: a Codex provider's `base_url` per
/// home (a loopback `http://127.0.0.1:PORT/https://…` wrapper is unwrapped),
/// otherwise the API key base.
pub fn codex_api_key(config: &Config, selector: &str) -> Option<String> {
    let provider = config
        .codex
        .providers
        .iter()
        .find(|p| p.selector == selector)?;
    let mut lines: Vec<String> = Vec::new();
    for upstream in provider.upstreams(&config.codex.base_url.api_key, &config.codex) {
        let line = match upstream {
            Ok(url) => display(&url),
            Err(e) => e.message.to_string(),
        };
        if !lines.contains(&line) {
            lines.push(line);
        }
    }
    Some(lines.join("\n"))
}

pub fn claude_api_key(config: &Config, selector: &str) -> Option<String> {
    match config.claude.api_key_upstream(selector) {
        Ok(upstream) => upstream.map(|url| display(&url)),
        Err(error) => Some(error.message.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_settings_details_use_selected_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("api.json"), r#"{"env":{"ANTHROPIC_BASE_URL":"https://custom.invalid","ANTHROPIC_API_KEY":"secret"}}"#).unwrap();
        let config = Config::parse(&format!("listen_port: 8787\nrequest_timeout_seconds: 3\nclaude:\n  config_dirs: [{}]\n  routing:\n    api_key: {{api: none}}\n", serde_json::to_string(dir.path()).unwrap())).unwrap();
        for name in ["api", "api.json"] {
            assert_eq!(config.claude.api_key_kind(name).unwrap(), "profile");
            assert_eq!(
                claude_api_key(&config, name).as_deref(),
                Some("https://custom.invalid")
            );
        }
        assert_eq!(claude_api_key(&config, "api.example.com"), None);
        assert_eq!(
            config.claude.api_key_kind("api.example.com").unwrap(),
            "gateway"
        );
        assert_eq!(
            config.claude.api_key_kind("ANTHROPIC_API_KEY").unwrap(),
            "api_key"
        );
    }

    #[test]
    fn api_key_selectors_show_the_forwarded_upstream() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("config.toml"),
            r#"[model_providers.custom]
name = "Custom"
base_url = "http://127.0.0.1:7889/https://custom.invalid/v1"
env_key = "CUSTOM_KEY"
experimental_bearer_token = "bearer-secret"
"#,
        )
        .unwrap();
        let config = Config::parse(&format!(
            "listen_port: 8787\nrequest_timeout_seconds: 3\ncodex:\n  homes: [{}]\n  routing:\n    api_key: {{custom: none, CUSTOM_KEY: none, OTHER_KEY: none, OPENAI_API_KEY: none, 'api.invalid/v1': none}}\n",
            serde_json::to_string(home.path()).unwrap()
        ))
        .unwrap();
        assert_eq!(config.codex.api_key_kind("custom").unwrap(), "provider");
        assert_eq!(config.codex.api_key_kind("CUSTOM_KEY").unwrap(), "provider");
        assert_eq!(config.codex.api_key_kind("OTHER_KEY").unwrap(), "api_key");
        assert_eq!(
            config.codex.api_key_kind("OPENAI_API_KEY").unwrap(),
            "api_key"
        );
        assert_eq!(
            config.codex.api_key_kind("api.invalid/v1").unwrap(),
            "gateway"
        );
        let custom = Some("https://custom.invalid/v1".to_owned());
        assert_eq!(codex_api_key(&config, "custom"), custom);
        assert_eq!(codex_api_key(&config, "CUSTOM_KEY"), custom);
        assert_eq!(
            codex_api_key(&config, "OTHER_KEY").as_deref(),
            Some("https://api.openai.com/v1")
        );
        assert_eq!(
            codex_api_key(&config, "OPENAI_API_KEY").as_deref(),
            Some("https://api.openai.com/v1")
        );
        assert_eq!(codex_api_key(&config, "api.invalid/v1"), None);
        assert_eq!(
            claude_api_key(&config, "ANTHROPIC_API_KEY").as_deref(),
            Some("https://api.anthropic.com")
        );
    }
}
