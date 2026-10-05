//! Changes single scalar values of the YAML configuration in place. Only the
//! affected line is rewritten, so comments, anchors and layout survive.

use coport::config::Config;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};

#[derive(Clone, Copy)]
enum Kind {
    Port,
    Seconds,
    Flag,
}

type Getter = fn(&Config) -> Value;

/// Configuration values the Settings page may change, as dotted YAML keys.
const EDITABLE: [(&str, Kind, Getter); 9] = [
    ("listen_port", Kind::Port, |c| json!(c.listen_port)),
    ("request_timeout_seconds", Kind::Seconds, |c| {
        json!(c.request_timeout_seconds)
    }),
    ("websocket.first_message_seconds", Kind::Seconds, |c| {
        json!(c.websocket.first_message_seconds)
    }),
    ("websocket.first_output_seconds", Kind::Seconds, |c| {
        json!(c.websocket.first_output_seconds)
    }),
    ("websocket.read_seconds", Kind::Seconds, |c| {
        json!(c.websocket.read_seconds)
    }),
    ("websocket.write_seconds", Kind::Seconds, |c| {
        json!(c.websocket.write_seconds)
    }),
    ("websocket.inter_turn_idle_seconds", Kind::Seconds, |c| {
        json!(c.websocket.inter_turn_idle_seconds)
    }),
    ("codex.account_auth_file_only", Kind::Flag, |c| {
        json!(c.codex.account_auth_file_only)
    }),
    ("claude.account_auth_file_only", Kind::Flag, |c| {
        json!(c.claude.account_auth_file_only)
    }),
];

/// Effective values of the editable keys, defaults included.
pub fn values(config: &Config) -> BTreeMap<&'static str, Value> {
    EDITABLE
        .iter()
        .map(|(key, _, get)| (*key, get(config)))
        .collect()
}

fn scalar(kind: Kind, value: &Value) -> Option<String> {
    match kind {
        Kind::Port => value
            .as_u64()
            .filter(|n| (1..=65535).contains(n))
            .map(|n| n.to_string()),
        Kind::Seconds => value
            .as_f64()
            .filter(|n| n.is_finite())
            .map(|n| n.to_string()),
        Kind::Flag => value.as_bool().map(|b| b.to_string()),
    }
}

/// Writes one editable value. The file is only replaced when the result is a
/// valid configuration that reads back the requested value.
pub fn set_value(path: &Path, key: &str, value: &Value) -> Result<(), String> {
    let kind = EDITABLE
        .iter()
        .find(|(k, ..)| *k == key)
        .map(|(_, kind, _)| *kind)
        .ok_or_else(|| format!("{key} cannot be changed here."))?;
    let wanted = scalar(kind, value).ok_or_else(|| format!("Invalid value for {key}."))?;
    // Replace the link target, so the proxy reads the edited file.
    let path = path
        .canonicalize()
        .map_err(|e| format!("Cannot open configuration: {e}"))?;
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("Cannot read configuration: {e}"))?;
    let keys: Vec<&str> = key.split('.').collect();
    let edited = set_scalar(&text, &keys, &wanted)?;
    let config = crate::core::parse_config(&edited)?;
    if values(&config).get(key).and_then(|v| scalar(kind, v)) != Some(wanted) {
        return Err(format!(
            "{key} is shared or aliased; edit the file instead."
        ));
    }
    crate::settings::write_private(&path, edited.as_bytes())
        .map_err(|e| format!("Cannot save configuration: {e}"))
}

/// A non-blank, non-comment line of a block mapping or sequence.
struct Content<'a> {
    indent: usize,
    key: Option<&'a str>,
    /// Text after `key:`.
    rest: &'a str,
}

fn content(line: &str) -> Option<Content<'_>> {
    let body = line.trim_start_matches(' ');
    if body.is_empty() || body.starts_with('#') || body == "---" {
        return None;
    }
    let indent = line.len() - body.len();
    let (key, after) = match body.as_bytes()[0] {
        q @ (b'"' | b'\'') => body[1..]
            .find(q as char)
            .map(|end| (&body[1..=end], &body[end + 2..]))
            .unwrap_or(("", "")),
        b'-' | b'?' if body.len() == 1 || body.as_bytes()[1] == b' ' => ("", ""),
        _ => {
            let colon = body
                .match_indices(':')
                .map(|(i, _)| i)
                .find(|&i| body[i + 1..].is_empty() || body[i + 1..].starts_with([' ', '\t']));
            colon.map_or(("", ""), |i| (&body[..i], &body[i..]))
        }
    };
    let rest = after.strip_prefix(':');
    Some(Content {
        indent,
        key: rest.and(Some(key)).filter(|k| !k.is_empty()),
        rest: rest.unwrap_or(""),
    })
}

/// Splits `rest` into leading space, value, the space before a comment, and the comment.
fn value_parts(rest: &str) -> (&str, &str, &str, &str) {
    let value_start = rest.len() - rest.trim_start().len();
    let comment = rest
        .char_indices()
        .find(|&(i, c)| c == '#' && (i == 0 || rest[..i].ends_with([' ', '\t'])))
        .map_or(rest.len(), |(i, _)| i);
    let comment = comment.max(value_start);
    let value = rest[value_start..comment].trim_end();
    let gap = &rest[value_start + value.len()..comment];
    (&rest[..value_start], value, gap, &rest[comment..])
}

fn set_scalar(text: &str, keys: &[&str], value: &str) -> Result<String, String> {
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let by_hand = |key: &str| format!("{key} uses advanced YAML; edit the file instead.");
    // The current mapping: its key line (None at the top level) and line range.
    let mut parent: Option<(usize, usize)> = None;
    let (mut start, mut end) = (0, lines.len());
    for (depth, key) in keys.iter().enumerate() {
        let last = depth + 1 == keys.len();
        let child_indent = (start..end)
            .find_map(|i| content(&lines[i]))
            .map(|c| c.indent);
        let found = (start..end).find(|&i| {
            content(&lines[i]).is_some_and(|c| Some(c.indent) == child_indent && c.key == Some(key))
        });
        let Some(i) = found else {
            let indent = child_indent.unwrap_or(parent.map_or(0, |(_, indent)| indent + 2));
            let at = match parent {
                None => lines.len(),
                Some((line, _)) => {
                    (start..end)
                        .rev()
                        .find(|&i| content(&lines[i]).is_some())
                        .unwrap_or(line)
                        + 1
                }
            };
            let added = keys[depth..].iter().enumerate().map(|(n, k)| {
                let pad = " ".repeat(indent + 2 * n);
                if depth + n + 1 == keys.len() {
                    format!("{pad}{k}: {value}")
                } else {
                    format!("{pad}{k}:")
                }
            });
            lines.splice(at..at, added.collect::<Vec<_>>());
            break;
        };
        let line = content(&lines[i]).unwrap();
        let indent = line.indent;
        let (lead, current, gap, comment) = value_parts(line.rest);
        let block_end = (i + 1..lines.len())
            .find(|&j| content(&lines[j]).is_some_and(|c| c.indent <= indent))
            .unwrap_or(lines.len());
        let nested = (i + 1..block_end).any(|j| content(&lines[j]).is_some());
        if last {
            if nested || current.starts_with(['&', '*', '!', '{', '[', '|', '>', '"', '\'']) {
                return Err(by_hand(key));
            }
            let head = &lines[i][..lines[i].len() - line.rest.len()];
            let lead = if lead.is_empty() { " " } else { lead };
            let tail = if comment.is_empty() {
                String::new()
            } else {
                format!("{gap}{comment}")
            };
            lines[i] = format!("{head}{lead}{value}{tail}");
            break;
        }
        if !current.is_empty() {
            return Err(by_hand(key));
        }
        parent = Some((i, indent));
        (start, end) = (i + 1, block_end);
    }
    let mut out = lines.join(newline);
    if text.is_empty() || text.ends_with('\n') {
        out.push_str(newline);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../../config.example.yaml");
    const BASE: &str = "listen_port: 8787 # local\nrequest_timeout_seconds: 300\n";

    #[test]
    fn replaces_only_the_value_and_keeps_comments() {
        let out = set_scalar(BASE, &["listen_port"], "9000").unwrap();
        assert_eq!(
            out,
            "listen_port: 9000 # local\nrequest_timeout_seconds: 300\n"
        );
        // Git may check the fixture out with CRLF on Windows. Exercise both
        // styles on every platform and require the edit to preserve them.
        let example_lf = EXAMPLE.replace("\r\n", "\n");
        for newline in ["\n", "\r\n"] {
            let example = example_lf.replace('\n', newline);
            let out = set_scalar(&example, &["websocket", "read_seconds"], "1200").unwrap();
            assert_eq!(
                out,
                example.replace(
                    &format!("  read_seconds: 900{newline}"),
                    &format!("  read_seconds: 1200{newline}")
                )
            );
            assert_eq!(Config::parse(&out).unwrap().websocket.read_seconds, 1200.0);
        }
        let out = set_scalar(EXAMPLE, &["claude", "account_auth_file_only"], "false").unwrap();
        let config = Config::parse(&out).unwrap();
        assert!(!config.claude.account_auth_file_only);
        assert!(config.codex.account_auth_file_only);
        assert_eq!(out.lines().count(), EXAMPLE.lines().count());
    }

    #[test]
    fn inserts_missing_keys_into_their_section() {
        let text =
            format!("{BASE}codex:\n    homes: [\"~/.codex\"]\n    # trailing\n\nclaude: {{}}\n");
        let out = set_scalar(&text, &["codex", "account_auth_file_only"], "false").unwrap();
        assert!(out.contains(
            "    homes: [\"~/.codex\"]\n    account_auth_file_only: false\n    # trailing\n"
        ));
        let out = set_scalar(BASE, &["websocket", "write_seconds"], "60").unwrap();
        assert_eq!(out, format!("{BASE}websocket:\n  write_seconds: 60\n"));
        assert_eq!(Config::parse(&out).unwrap().websocket.write_seconds, 60.0);
        let out = set_scalar(
            "listen_port: 1\nwebsocket: # later\n",
            &["websocket", "read_seconds"],
            "5",
        )
        .unwrap();
        assert_eq!(
            out,
            "listen_port: 1\nwebsocket: # later\n  read_seconds: 5\n"
        );
    }

    #[test]
    fn leaves_advanced_yaml_to_the_editor() {
        for text in [
            "listen_port: &port 8787\n",
            "listen_port: \"8787\"\n",
            "websocket: {read_seconds: 5}\n",
            "websocket: &ws\n  read_seconds: 5\n",
        ] {
            let key: &[&str] = if text.starts_with("listen") {
                &["listen_port"]
            } else {
                &["websocket", "read_seconds"]
            };
            assert!(set_scalar(text, key, "9").is_err(), "{text}");
        }
    }

    #[test]
    fn nested_lookalikes_and_line_endings_are_respected() {
        let text = "codex:\r\n  routing:\r\n    account_auth_file_only: x\r\n  account_auth_file_only: true\r\n";
        let out = set_scalar(text, &["codex", "account_auth_file_only"], "false").unwrap();
        assert_eq!(
            out,
            text.replace(
                "  account_auth_file_only: true",
                "  account_auth_file_only: false"
            )
        );
    }

    #[test]
    fn writes_only_valid_configurations_and_keeps_anchors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let text = format!(
            "{BASE}proxies:\n  us: &p http://127.0.0.1:8101\nconnect:\n  \"chatgpt.com:443\": us\n"
        );
        std::fs::write(&path, &text).unwrap();
        set_value(&path, "request_timeout_seconds", &json!(42.5)).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert_eq!(saved, text.replace("seconds: 300", "seconds: 42.5"));
        assert!(set_value(&path, "request_timeout_seconds", &json!(0)).is_err());
        assert!(set_value(&path, "listen_port", &json!(true)).is_err());
        assert!(set_value(&path, "proxies.us", &json!(1)).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), saved);
        set_value(&path, "websocket.inter_turn_idle_seconds", &json!(0)).unwrap();
        let config = Config::read(&path).unwrap();
        assert_eq!(
            values(&config)["websocket.inter_turn_idle_seconds"],
            json!(0.0)
        );
    }
}
