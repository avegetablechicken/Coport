//! A location rejection is terminal for agy. Close that request before headers
//! and let its next model call wait locally for this login's cooldown. Other
//! upstream errors pass through unchanged, with Google's error envelope logged.
//!
//! A rejection soon after a cooldown ends doubles the next one, up to the last
//! step; a model call the upstream accepts starts the login over.
use super::*;
use std::io::Read;
use tokio::time::{Instant as Tick, sleep_until};

const STEPS: [Duration; 3] = [
    Duration::from_secs(3 * 60),
    Duration::from_secs(6 * 60),
    Duration::from_secs(12 * 60),
];
/// How long after a cooldown ends a fresh rejection still counts as the same
/// episode and moves to the next step.
const EPISODE: Duration = STEPS[STEPS.len() - 1];
const ERROR_LIMIT: usize = 16 * 1024;
const MESSAGE_LIMIT: usize = 300;

#[derive(Clone, Hash, PartialEq, Eq)]
pub(super) struct Scope(String, String);

impl Scope {
    pub(super) fn new(upstream: &str, credential: &str) -> Self {
        Self(upstream.to_owned(), crate::antigravity::digest(credential))
    }

    pub(super) fn model(
        method: &hyper::Method,
        url: &Url,
        scope: impl FnOnce() -> Self,
    ) -> Option<Self> {
        let action = url.path().trim_end_matches('/').rsplit_once(':')?.1;
        (*method == hyper::Method::POST
            && matches!(action, "generateContent" | "streamGenerateContent"))
        .then(scope)
    }
}

/// An upstream error to inspect, with the login it may cool down.
#[derive(Clone)]
pub(super) struct Inspect(Option<Scope>);

/// A login's current or most recent cooldown and its step in `STEPS`.
#[derive(Clone, Copy)]
struct Cooldown {
    until: Tick,
    step: usize,
}

#[derive(Default)]
pub(super) struct LocationCooldowns(Mutex<HashMap<Scope, Cooldown>>);

impl LocationCooldowns {
    pub(super) async fn wait(&self, scope: Option<&Scope>, log: &mut RequestLog) {
        let Some(scope) = scope else { return };
        loop {
            let until = self.0.lock().unwrap().get(scope).map(|c| c.until);
            let Some(until) = until.filter(|until| *until > Tick::now()) else {
                return;
            };
            log.field(
                "location_cooldown_wait_ms",
                until.saturating_duration_since(Tick::now()).as_millis(),
            );
            sleep_until(until).await;
        }
    }

    /// Mark an upstream error for inspection; only a 400 model call can
    /// start a location cooldown. An accepted model call ends its login's
    /// episode, unless it was sent before a cooldown that is still running.
    pub(super) fn observe(
        &self,
        scope: Option<Scope>,
        mut response: reqwest::Response,
    ) -> reqwest::Response {
        let status = response.status();
        if status.is_client_error() || status.is_server_error() {
            let cooldown = scope.filter(|_| status == 400);
            response.extensions_mut().insert(Inspect(cooldown));
        } else if let Some(scope) = scope.filter(|_| status.is_success()) {
            let mut entries = self.0.lock().unwrap();
            if entries.get(&scope).is_some_and(|c| c.until <= Tick::now()) {
                entries.remove(&scope);
            }
        }
        response
    }

    /// Start the login's next cooldown. A call sent before the running
    /// cooldown began is the same rejection and changes nothing.
    fn reject(&self, scope: Scope, log: &mut RequestLog) {
        let now = Tick::now();
        let mut entries = self.0.lock().unwrap();
        entries.retain(|_, c| now.saturating_duration_since(c.until) <= EPISODE);
        let cooldown = match entries.get(&scope).copied() {
            Some(running) if running.until > now => running,
            previous => {
                let step = previous.map_or(0, |c| (c.step + 1).min(STEPS.len() - 1));
                let next = Cooldown {
                    until: now + STEPS[step],
                    step,
                };
                entries.insert(scope, next);
                next
            }
        };
        log.field(
            "location_cooldown_ms",
            cooldown.until.saturating_duration_since(now).as_millis(),
        );
    }

    /// Inspect only a small error envelope. Replay every original byte (and
    /// any body error) for ordinary, oversized or unrecognized errors.
    pub(super) async fn inspect(
        &self,
        Inspect(scope): Inspect,
        response: reqwest::Response,
        log: &mut RequestLog,
    ) -> Option<reqwest::Response> {
        if response
            .content_length()
            .is_some_and(|n| n > ERROR_LIMIT as u64)
        {
            return Some(response);
        }
        let (parts, body) = hyper::Response::<reqwest::Body>::from(response).into_parts();
        let mut stream = body.into_data_stream();
        let mut prefix = Vec::new();
        let overflow = loop {
            match stream.next().await {
                Some(Ok(bytes)) if bytes.len() <= ERROR_LIMIT - prefix.len() => {
                    prefix.extend_from_slice(&bytes)
                }
                other => break other,
            }
        };
        let error = overflow
            .is_none()
            .then(|| error_envelope(&parts.headers, &prefix))
            .flatten();
        if let Some(error) = &error {
            log_error(error, log);
        }
        if let (Some(scope), Some(error)) = (scope, &error) {
            if location_error(error) {
                self.reject(scope, log);
                return None;
            }
        }
        let prefix = futures_util::stream::iter([Ok(Bytes::from(prefix))]);
        let stream = prefix
            .chain(futures_util::stream::iter(overflow))
            .chain(stream);
        Some(hyper::Response::from_parts(parts, reqwest::Body::wrap_stream(stream)).into())
    }
}

/// The `error` object of a complete, small JSON error body.
fn error_envelope(headers: &HeaderMap, body: &[u8]) -> Option<serde_json::Value> {
    // agy's Go transport can request gzip even for JSON errors. Inspect a
    // bounded decoded copy; the original encoding is preserved on pass-through.
    let mut decoded = Vec::new();
    let body = match headers.get("content-encoding").map(|v| v.as_bytes()) {
        None => body,
        Some(v) if v.eq_ignore_ascii_case(b"identity") => body,
        Some(v) if v.eq_ignore_ascii_case(b"gzip") => {
            let decoder = flate2::read::MultiGzDecoder::new(body);
            if decoder
                .take(ERROR_LIMIT as u64 + 1)
                .read_to_end(&mut decoded)
                .is_err()
                || decoded.len() > ERROR_LIMIT
            {
                return None;
            }
            &decoded
        }
        _ => return None,
    };
    let mut envelope = serde_json::from_slice::<serde_json::Value>(body).ok()?;
    let error = envelope.get_mut("error")?.take();
    error.is_object().then_some(error)
}

/// Record why the upstream refused: its status, `ErrorInfo` reason and a
/// single-line, bounded message. Other details are not logged.
fn log_error(error: &serde_json::Value, log: &mut RequestLog) {
    if let Some(status) = error["status"].as_str() {
        log.field("upstream_error_status", status);
    }
    let reason = error["details"].as_array().and_then(|details| {
        details.iter().find_map(|detail| {
            detail["@type"]
                .as_str()
                .is_some_and(|t| t.ends_with("google.rpc.ErrorInfo"))
                .then(|| detail["reason"].as_str())
                .flatten()
        })
    });
    if let Some(reason) = reason {
        log.field("upstream_error_reason", reason);
    }
    if let Some(message) = error["message"].as_str() {
        let mut message = message.split_whitespace().collect::<Vec<_>>().join(" ");
        if let Some((cut, _)) = message.char_indices().nth(MESSAGE_LIMIT) {
            message.truncate(cut);
            message.push('…');
        }
        log.field("upstream_error_message", message);
    }
}

fn location_error(error: &serde_json::Value) -> bool {
    error["status"] == "FAILED_PRECONDITION"
        && error["message"].as_str().is_some_and(|msg| {
            msg.trim()
                .trim_end_matches('.')
                .eq_ignore_ascii_case("User location is not supported for the API use")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;
    const LOCATION: &[u8] = br#"{"error":{"status":"FAILED_PRECONDITION","message":"User location is not supported for the API use."}}"#;

    fn bad(body: Vec<u8>) -> reqwest::Response {
        failed(400, body)
    }

    fn failed(status: u16, body: Vec<u8>) -> reqwest::Response {
        Response::builder()
            .status(status)
            .header("x-test", "retained")
            .body(body)
            .unwrap()
            .into()
    }

    fn log() -> (tempfile::TempDir, RequestLog) {
        let dir = tempfile::tempdir().unwrap();
        let log = RequestLog {
            logger: Arc::new(Logger::new(dir.path().join("log"))),
            fields: Default::default(),
            started: Instant::now(),
            status: 0,
            bytes: 0,
            outcome: "request_finished",
        };
        (dir, log)
    }

    fn cooling(scope: Scope) -> Inspect {
        Inspect(Some(scope))
    }

    async fn reject(cooldowns: &LocationCooldowns, scope: &Scope, log: &mut RequestLog) {
        let response = cooldowns
            .inspect(cooling(scope.clone()), bad(LOCATION.to_vec()), log)
            .await;
        assert!(response.is_none());
    }

    /// `scope` stays cooling for exactly `minutes` from now.
    async fn cools_for(cooldowns: &LocationCooldowns, scope: &Scope, minutes: u64) {
        let (_dir, mut log) = log();
        let mut retry = Box::pin(cooldowns.wait(Some(scope), &mut log));
        assert!(retry.as_mut().now_or_never().is_none());
        tokio::time::advance(Duration::from_secs(minutes * 60 - 1)).await;
        assert!(retry.as_mut().now_or_never().is_none(), "{minutes} min");
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(retry.now_or_never().is_some(), "{minutes} min");
    }

    fn accepted(cooldowns: &LocationCooldowns, scope: &Scope) {
        let response = cooldowns.observe(Some(scope.clone()), failed(200, vec![]));
        assert!(response.extensions().get::<Inspect>().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_rejections_back_off_to_twelve_minutes_for_that_login_only() {
        let cooldowns = LocationCooldowns::default();
        let key = Scope::new("upstream", "first");
        let other = Scope::new("upstream", "second");
        let (_dir, mut log) = log();
        for minutes in [3, 6, 12, 12] {
            reject(&cooldowns, &key, &mut log).await;
            assert_eq!(
                log.fields["location_cooldown_ms"],
                (minutes * 60_000).to_string()
            );
            assert!(
                cooldowns
                    .wait(Some(&other), &mut log)
                    .now_or_never()
                    .is_some()
            );
            cools_for(&cooldowns, &key, minutes).await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn calls_sent_before_a_cooldown_neither_extend_nor_end_it() {
        let cooldowns = LocationCooldowns::default();
        let key = Scope::new("upstream", "first");
        let (_dir, mut log) = log();
        reject(&cooldowns, &key, &mut log).await;
        tokio::time::advance(Duration::from_secs(60)).await;
        // Concurrent calls in flight when the first was rejected.
        reject(&cooldowns, &key, &mut log).await;
        assert_eq!(log.fields["location_cooldown_ms"], "120000");
        accepted(&cooldowns, &key);
        cools_for(&cooldowns, &key, 2).await;
        // The episode continued: the next rejection takes the next step.
        reject(&cooldowns, &key, &mut log).await;
        cools_for(&cooldowns, &key, 6).await;
    }

    #[tokio::test(start_paused = true)]
    async fn an_accepted_call_or_a_quiet_episode_starts_over() {
        let cooldowns = LocationCooldowns::default();
        let key = Scope::new("upstream", "first");
        let (_dir, mut log) = log();
        reject(&cooldowns, &key, &mut log).await;
        cools_for(&cooldowns, &key, 3).await;
        reject(&cooldowns, &key, &mut log).await;
        cools_for(&cooldowns, &key, 6).await;
        // Errors other than a location rejection keep the episode going.
        let response = cooldowns.observe(Some(key.clone()), failed(503, vec![]));
        assert!(response.extensions().get::<Inspect>().is_some());
        reject(&cooldowns, &key, &mut log).await;
        cools_for(&cooldowns, &key, 12).await;
        accepted(&cooldowns, &key);
        reject(&cooldowns, &key, &mut log).await;
        cools_for(&cooldowns, &key, 3).await;

        reject(&cooldowns, &key, &mut log).await;
        cools_for(&cooldowns, &key, 6).await;
        tokio::time::advance(EPISODE).await;
        reject(&cooldowns, &key, &mut log).await;
        cools_for(&cooldowns, &key, 12).await;
        tokio::time::advance(EPISODE + Duration::from_secs(1)).await;
        reject(&cooldowns, &key, &mut log).await;
        cools_for(&cooldowns, &key, 3).await;
    }

    #[tokio::test]
    async fn unrelated_and_oversized_error_bodies_are_forwarded_intact() {
        let cooldowns = LocationCooldowns::default();
        let (_dir, mut log) = log();
        for bytes in [br#"{"error":{"status":"INVALID_ARGUMENT","message":"User location is not supported for the API use."}}"#.to_vec(),
            br#"{"error":{"status":"FAILED_PRECONDITION","message":"Invalid project resource name"}}"#.to_vec(), vec![1; ERROR_LIMIT+1]] {
            let response = cooldowns.inspect(cooling(Scope::new("oauth", "test")), bad(bytes.clone()), &mut log).await.unwrap();
            assert_eq!(response.status(), 400);
            assert_eq!(response.headers()["x-test"], "retained");
            assert_eq!(response.bytes().await.unwrap(), bytes);
        }
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gzip, LOCATION).unwrap();
        let mut response = bad(gzip.finish().unwrap());
        response
            .headers_mut()
            .insert("content-encoding", "gzip".parse().unwrap());
        assert!(
            cooldowns
                .inspect(cooling(Scope::new("oauth", "test")), response, &mut log)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn error_reasons_are_logged_and_bodies_forwarded_intact() {
        let cooldowns = LocationCooldowns::default();
        let long = "x ".repeat(400);
        let body = format!(
            r#"{{"error":{{"code":403,"status":"PERMISSION_DENIED","message":"Denied.\n{long}","details":[{{"@type":"type.googleapis.com/google.rpc.Help"}},{{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"SERVICE_DISABLED","metadata":{{"consumer":"projects/1"}}}}]}}}}"#
        );
        let (_dir, mut log) = log();
        // A 403 cannot cool a login down even when it names the location.
        for (status, bytes) in [(403, body.clone().into_bytes()), (403, LOCATION.to_vec())] {
            let response = cooldowns
                .inspect(Inspect(None), failed(status, bytes.clone()), &mut log)
                .await
                .unwrap();
            assert_eq!(response.status(), status);
            assert_eq!(response.headers()["x-test"], "retained");
            assert_eq!(response.bytes().await.unwrap(), bytes);
            if bytes != LOCATION {
                assert_eq!(log.fields["upstream_error_status"], "PERMISSION_DENIED");
                assert_eq!(log.fields["upstream_error_reason"], "SERVICE_DISABLED");
                let message = log.fields["upstream_error_message"].as_str().unwrap();
                assert!(message.starts_with("Denied. x x"));
                assert!(!message.contains('\n'));
                assert_eq!(message.chars().count(), MESSAGE_LIMIT + 1);
                assert!(!log.fields.contains_key("consumer"));
            }
        }
        assert_eq!(
            log.fields["upstream_error_status"], "FAILED_PRECONDITION",
            "a later error replaces the earlier status"
        );
        assert!(cooldowns.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn only_upstream_errors_are_tagged_and_only_400s_cool_down() {
        let scope = || Some(Scope::new("oauth", "test"));
        let cooldowns = LocationCooldowns::default();
        let ok = cooldowns.observe(scope(), failed(200, vec![]));
        assert!(ok.extensions().get::<Inspect>().is_none());
        for (status, cools) in [(400, true), (403, false), (500, false)] {
            let tagged = cooldowns.observe(scope(), failed(status, vec![]));
            let Inspect(cooldown) = tagged.extensions().get::<Inspect>().unwrap();
            assert_eq!(cooldown.is_some(), cools, "{status}");
        }
        let untagged = cooldowns.observe(None, failed(403, vec![]));
        assert!(untagged.extensions().get::<Inspect>().unwrap().0.is_none());
    }

    #[tokio::test]
    async fn non_json_errors_log_nothing() {
        let cooldowns = LocationCooldowns::default();
        let (_dir, mut log) = log();
        for bytes in [
            b"<html>403</html>".to_vec(),
            br#"{"error":"denied"}"#.to_vec(),
        ] {
            let response = cooldowns
                .inspect(Inspect(None), failed(403, bytes.clone()), &mut log)
                .await
                .unwrap();
            assert_eq!(response.bytes().await.unwrap(), bytes);
        }
        assert!(log.fields.is_empty());
    }
}
