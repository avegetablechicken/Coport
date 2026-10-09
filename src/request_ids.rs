//! Client correlation metadata only; never copy arbitrary headers or request content.
use hyper::HeaderMap;
use serde::Deserialize;
use serde_json::{Map, Value};

const SESSION_HEADERS: &[&str] = &[
    "session_id",
    "x-session-id",
    "x-codex-session-id",
    "x-claude-code-session-id",
];
const REQUEST_HEADERS: &[&str] = &[
    "x-client-request-id",
    "x-request-id",
    "request-id",
    "request_id",
];

fn valid(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && value.bytes().all(|b| b.is_ascii_graphic())
}

fn record(fields: &mut Map<String, Value>, key: &str, value: &str, source: &str) {
    if valid(value) {
        fields.insert(key.into(), Value::String(value.into()));
        fields.insert(format!("{key}_source"), Value::String(source.into()));
    }
}

pub(crate) fn headers(headers: &HeaderMap, fields: &mut Map<String, Value>) {
    for (key, names) in [
        ("session_id", SESSION_HEADERS),
        ("client_request_id", REQUEST_HEADERS),
    ] {
        fields.insert(key.into(), Value::Null);
        for name in names {
            // Ambiguous duplicate headers are not usable correlation IDs.
            if headers.get_all(*name).iter().count() != 1 {
                continue;
            }
            if let Some(value) = headers
                .get(*name)
                .and_then(|v| v.to_str().ok())
                .filter(|v| valid(v))
            {
                record(fields, key, value, &format!("header:{name}"));
                break;
            }
        }
    }
}

// Value is used only for allowlisted fields so malformed optional metadata does
// not reject an otherwise valid model request. Prompts and tool inputs are skipped.
#[derive(Default, Deserialize)]
struct Ids {
    #[serde(default)]
    session_id: Value,
    #[serde(default)]
    client_request_id: Value,
    #[serde(default)]
    request_id: Value,
    #[serde(default)]
    event_id: Value,
    #[serde(default)]
    metadata: Value,
}

pub(crate) fn body(bytes: &[u8], fields: &mut Map<String, Value>, websocket: bool) {
    let Ok(ids) = serde_json::from_slice::<Ids>(bytes) else {
        return;
    };
    if websocket || !fields.get("session_id").is_some_and(Value::is_string) {
        if let Some(id) = ids.session_id.as_str().filter(|v| valid(v)) {
            record(fields, "session_id", id, "body:session_id");
        } else if let Some(id) = ids
            .metadata
            .get("session_id")
            .and_then(Value::as_str)
            .filter(|v| valid(v))
        {
            record(fields, "session_id", id, "body:metadata.session_id");
        } else if let Some(user) = ids.metadata.get("user_id").and_then(Value::as_str) {
            // Claude metadata may encode session_id in a JSON string, or in the
            // legacy user_<hash>_account_<id>_session_<uuid> form. Never log user_id.
            let parsed = serde_json::from_str::<Ids>(user).ok();
            if let Some(id) = parsed
                .as_ref()
                .and_then(|p| p.session_id.as_str())
                .filter(|v| valid(v))
            {
                record(fields, "session_id", id, "body:metadata.user_id.session_id");
            } else if let Some((prefix, id)) = user.rsplit_once("_session_") {
                if prefix.starts_with("user_")
                    && prefix.contains("_account_")
                    && uuid::Uuid::parse_str(id).is_ok()
                {
                    record(fields, "session_id", id, "body:metadata.user_id.session_id");
                }
            }
        }
    }
    if !fields
        .get("client_request_id")
        .is_some_and(Value::is_string)
    {
        for (value, source) in [
            (&ids.client_request_id, "body:client_request_id"),
            (&ids.request_id, "body:request_id"),
        ]
        .into_iter()
        .chain(websocket.then_some((&ids.event_id, "body:event_id")))
        {
            if let Some(id) = value.as_str().filter(|v| valid(v)) {
                record(fields, "client_request_id", id, source);
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn headers_preserve_client_ids_without_replacing_proxy_id() {
        for session in SESSION_HEADERS {
            for request in REQUEST_HEADERS {
                let mut headers = HeaderMap::new();
                headers.insert(*session, "session-123".parse().unwrap());
                headers.insert(*request, "client-456".parse().unwrap());
                let mut fields = Map::new();
                fields.insert("request_id".into(), json!("proxy-789"));
                super::headers(&headers, &mut fields);
                body(
                    br#"{"session_id":"body-session","request_id":"body-request"}"#,
                    &mut fields,
                    false,
                );
                assert_eq!(fields["session_id"], "session-123");
                assert_eq!(fields["client_request_id"], "client-456");
                assert_eq!(fields["request_id"], "proxy-789");
                assert_eq!(fields["session_id_source"], format!("header:{session}"));
                assert_eq!(
                    fields["client_request_id_source"],
                    format!("header:{request}")
                );
            }
        }
    }

    #[test]
    fn missing_invalid_and_duplicate_ids_are_not_fabricated() {
        let mut headers = HeaderMap::new();
        headers.append("session_id", "one".parse().unwrap());
        headers.append("session_id", "two".parse().unwrap());
        headers.insert("x-request-id", " ".parse().unwrap());
        let mut fields = Map::new();
        super::headers(&headers, &mut fields);
        for request in [
            json!({}),
            json!({"session_id": 42, "request_id": {"secret":"PRIVATE"}}),
            json!({"session_id":"bad\nline", "request_id":"x".repeat(257)}),
            json!({"metadata":{"user_id":"PRIVATE"}}),
        ] {
            body(request.to_string().as_bytes(), &mut fields, false);
            assert_eq!(fields["session_id"], Value::Null);
            assert_eq!(fields["client_request_id"], Value::Null);
            assert!(!fields.contains_key("session_id_source"));
        }
        body(b"not json", &mut fields, false);
        assert_eq!(fields["session_id"], Value::Null);
    }

    #[test]
    fn body_ids_and_claude_metadata_exclude_user_and_account_information() {
        let session = "550e8400-e29b-41d4-a716-446655440000";
        for payload in [
            json!({"session_id":session}),
            json!({"metadata":{"session_id":session}}),
            json!({"metadata":{"user_id":json!({"session_id":session,"account_uuid":"PRIVATE","device_id":"PRIVATE"}).to_string()}}),
            json!({"metadata":{"user_id":format!("user_PRIVATE_account_PRIVATE_session_{session}")}}),
        ] {
            let mut payload = payload;
            payload["request_id"] = json!("client-request");
            payload["input"] = json!("PRIVATE PROMPT");
            let mut fields = Map::new();
            super::headers(&HeaderMap::new(), &mut fields);
            body(payload.to_string().as_bytes(), &mut fields, false);
            assert_eq!(fields["session_id"], session);
            assert_eq!(fields["client_request_id"], "client-request");
            assert!(!serde_json::to_string(&fields).unwrap().contains("PRIVATE"));
        }
    }
}
