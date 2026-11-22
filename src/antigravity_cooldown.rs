//! A location rejection is terminal for agy. Close that request before headers
//! and let its next model call wait locally for this login's cooldown.
use super::*;
use std::io::Read;
use tokio::time::{Instant as Tick, sleep_until};

const COOLDOWN: Duration = Duration::from_secs(180);
const ERROR_LIMIT: usize = 16 * 1024;

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

    pub(super) fn tag_error(
        scope: Option<Self>,
        mut response: reqwest::Response,
    ) -> reqwest::Response {
        if response.status() == 400 {
            if let Some(scope) = scope {
                response.extensions_mut().insert(scope);
            }
        }
        response
    }
}

#[derive(Default)]
pub(super) struct LocationCooldowns(Mutex<HashMap<Scope, Tick>>);

impl LocationCooldowns {
    pub(super) async fn wait(&self, scope: Option<&Scope>, log: &mut RequestLog) {
        let Some(scope) = scope else { return };
        loop {
            let until = self.0.lock().unwrap().get(scope).copied();
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

    /// Inspect only a small error envelope. Replay every original byte (and
    /// any body error) for ordinary, oversized or unrecognized 400s.
    pub(super) async fn inspect(
        &self,
        scope: Scope,
        response: reqwest::Response,
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
        if overflow.is_none() && location_error(&parts.headers, &prefix) {
            let now = Tick::now();
            let mut entries = self.0.lock().unwrap();
            entries.retain(|_, until| *until > now);
            entries.insert(scope, now + COOLDOWN);
            return None;
        }
        let prefix = futures_util::stream::iter([Ok(Bytes::from(prefix))]);
        let stream = prefix
            .chain(futures_util::stream::iter(overflow))
            .chain(stream);
        Some(hyper::Response::from_parts(parts, reqwest::Body::wrap_stream(stream)).into())
    }
}

fn location_error(headers: &HeaderMap, body: &[u8]) -> bool {
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
                return false;
            }
            &decoded
        }
        _ => return false,
    };
    let Ok(envelope) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    let error = &envelope["error"];
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
        Response::builder()
            .status(400)
            .header("x-test", "retained")
            .body(body)
            .unwrap()
            .into()
    }

    #[tokio::test(start_paused = true)]
    async fn cooldown_lasts_three_minutes_and_restarts_only_on_a_fresh_rejection() {
        let cooldowns = LocationCooldowns::default();
        let key = Scope::new("upstream", "first");
        let other = Scope::new("upstream", "second");
        let dir = tempfile::tempdir().unwrap();
        let mut log = RequestLog {
            logger: Arc::new(Logger::new(dir.path().join("log"))),
            fields: Default::default(),
            started: Instant::now(),
            status: 0,
            bytes: 0,
            outcome: "request_finished",
        };
        for _ in 0..2 {
            assert!(
                cooldowns
                    .inspect(key.clone(), bad(LOCATION.to_vec()))
                    .await
                    .is_none()
            );
            assert!(
                cooldowns
                    .wait(Some(&other), &mut log)
                    .now_or_never()
                    .is_some()
            );
            assert!(
                cooldowns
                    .wait(Some(&key), &mut log)
                    .now_or_never()
                    .is_none()
            );
            tokio::time::advance(Duration::from_secs(179)).await;
            let mut retry = Box::pin(cooldowns.wait(Some(&key), &mut log));
            assert!(retry.as_mut().now_or_never().is_none());
            tokio::time::advance(Duration::from_secs(1)).await;
            assert!(retry.now_or_never().is_some());
        }
    }

    #[tokio::test]
    async fn unrelated_and_oversized_error_bodies_are_forwarded_intact() {
        let cooldowns = LocationCooldowns::default();
        for bytes in [br#"{"error":{"status":"INVALID_ARGUMENT","message":"User location is not supported for the API use."}}"#.to_vec(),
            br#"{"error":{"status":"FAILED_PRECONDITION","message":"Invalid project resource name"}}"#.to_vec(), vec![1; ERROR_LIMIT+1]] {
            let response = cooldowns.inspect(Scope::new("oauth", "test"), bad(bytes.clone())).await.unwrap();
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
                .inspect(Scope::new("oauth", "test"), response)
                .await
                .is_none()
        );
    }
}
