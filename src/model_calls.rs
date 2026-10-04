//! Content-free model-call observations, separate from HTTP/tunnel lifecycle logs.
use crate::logger::{Logger, RequestLog};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{
    collections::{HashSet, VecDeque},
    sync::Arc,
    time::Instant,
};

pub fn is_model_endpoint(method: &str, raw: &str) -> bool {
    let raw = raw.split('?').next().unwrap_or("");
    let path = raw
        .strip_prefix("/codex/")
        .or_else(|| raw.strip_prefix("/anthropic/"))
        .or_else(|| raw.strip_prefix("/claude/"))
        .unwrap_or(raw)
        .trim_start_matches('/');
    let url = url::Url::parse(path).ok();
    let path = url
        .as_ref()
        .map(|u| u.path())
        .unwrap_or(path)
        .trim_matches('/');
    let ends = |suffix: &str| path == suffix || path.ends_with(&format!("/{suffix}"));
    match method {
        "POST" => [
            "responses",
            "responses/compact",
            "messages",
            "chat/completions",
            "completions",
        ]
        .iter()
        .any(|s| ends(s)),
        "GET" => ends("responses"),
        _ => false,
    }
}

// Unknown fields (including prompts, generated text, and tool arguments) are
// skipped by serde and never copied into log records.
#[derive(Default, Deserialize)]
struct Envelope {
    #[serde(rename = "type", default)]
    kind: String,
    object: Option<String>,
    model: Option<String>,
    id: Option<String>,
    response_id: Option<String>,
    response: Option<Metadata>,
    message: Option<Metadata>,
    #[serde(default, deserialize_with = "deserialize_usage")]
    usage: Option<Usage>,
    error: Option<serde::de::IgnoredAny>,
}
#[derive(Default, Deserialize)]
struct Metadata {
    id: Option<String>,
    model: Option<String>,
    #[serde(default, deserialize_with = "deserialize_usage")]
    usage: Option<Usage>,
    incomplete_details: Option<IncompleteDetails>,
}
#[derive(Default, Deserialize)]
struct IncompleteDetails {
    reason: Option<String>,
}
#[derive(Default, Deserialize)]
struct Usage {
    #[serde(alias = "prompt_tokens")]
    input_tokens: Option<u64>,
    #[serde(alias = "completion_tokens")]
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    #[serde(alias = "prompt_tokens_details")]
    input_tokens_details: Option<TokenDetails>,
}
#[derive(Default, Deserialize)]
struct TokenDetails {
    cached_tokens: Option<u64>,
}

fn deserialize_usage<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Usage>, D::Error> {
    // Bad usage metadata must not hide an otherwise valid terminal event.
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| serde_json::from_value(value).ok()))
}

fn field(fields: &mut Map<String, Value>, key: &str, value: impl ToString) {
    fields.insert(key.into(), json!(value.to_string()));
}
fn safe_label(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}
impl Envelope {
    fn response_id(&self) -> Option<&str> {
        self.response
            .as_ref()
            .and_then(|r| r.id.as_deref())
            .or(self.response_id.as_deref())
            .or_else(|| terminal(&self.kind).and(self.id.as_deref()))
            .filter(|s| safe_label(s))
    }
    fn apply(&self, fields: &mut Map<String, Value>) {
        let meta = self.response.as_ref().or(self.message.as_ref());
        if let Some(model) = meta
            .and_then(|r| r.model.as_deref())
            .or(self.model.as_deref())
            .filter(|s| safe_label(s))
        {
            field(fields, "model", model);
        }
        if let Some(id) = self
            .response_id()
            .or_else(|| meta.and_then(|m| m.id.as_deref()))
            .filter(|s| safe_label(s))
        {
            field(fields, "response_id", id);
        }
        if let Some(reason) = meta
            .and_then(|m| m.incomplete_details.as_ref())
            .and_then(|d| d.reason.as_deref())
            .filter(|s| safe_label(s))
        {
            field(fields, "incomplete_reason", reason);
        }
        if let Some(usage) = meta.and_then(|r| r.usage.as_ref()).or(self.usage.as_ref()) {
            for (key, value) in [
                ("input_tokens", usage.input_tokens),
                ("output_tokens", usage.output_tokens),
                (
                    "cached_input_tokens",
                    usage.cache_read_input_tokens.or_else(|| {
                        usage
                            .input_tokens_details
                            .as_ref()
                            .and_then(|d| d.cached_tokens)
                    }),
                ),
                (
                    "cache_creation_input_tokens",
                    usage.cache_creation_input_tokens,
                ),
            ] {
                // Usage events are snapshots, not deltas; do not sum repeated snapshots.
                if let Some(value) = value {
                    field(fields, key, value);
                }
            }
        }
    }
}
fn terminal(kind: &str) -> Option<&'static str> {
    match kind {
        "response.completed" | "response.done" | "message_stop" => Some("finished"),
        "response.failed" | "error" => Some("failed"),
        // Truncated by the model (for example max_output_tokens), not a transport or API error.
        "response.incomplete" => Some("incomplete"),
        "response.cancelled" | "response.canceled" => Some("cancelled"),
        _ => None,
    }
}

pub(crate) fn http_event(log: &RequestLog, event: &str) {
    if !log.fields.contains_key("model_call_id") {
        return;
    }
    let mapped = match event {
        "request_received" => "model_call_started",
        "route_selected" | "upstream_response" => "model_call_updated",
        "request_finished" | "request_failed" | "request_rejected" | "request_cancelled" => {
            let outcome = log.fields.get("model_outcome").and_then(Value::as_str);
            if log.status >= 400 || outcome == Some("failed") {
                "model_call_failed"
            } else if outcome == Some("finished") {
                "model_call_finished"
            } else if outcome == Some("incomplete") {
                "model_call_incomplete"
            } else if outcome == Some("cancelled") || event == "request_cancelled" {
                "model_call_cancelled"
            } else if event == "request_finished" && outcome == Some("running") {
                "model_call_unknown"
            } else if event == "request_finished" {
                "model_call_finished"
            } else {
                "model_call_failed"
            }
        }
        _ => return,
    };
    log.logger.write(mapped, log.fields.clone());
}

pub(crate) fn observe_request(bytes: &[u8], log: &mut RequestLog) {
    if let Ok(event) = serde_json::from_slice::<Envelope>(bytes) {
        if let Some(model) = event.model.filter(|s| safe_label(s)) {
            log.field("model", model);
        }
    }
}

const MAX_MESSAGE: usize = 32 * 1024 * 1024;

/// Incrementally observes SSE lines or a bounded JSON body without changing it.
pub(crate) struct HttpObserver {
    sse: bool,
    buffer: Vec<u8>,
    data: Vec<u8>,
    disabled: bool,
}
impl HttpObserver {
    pub fn new(sse: bool) -> Self {
        Self {
            sse,
            buffer: Vec::new(),
            data: Vec::new(),
            disabled: false,
        }
    }
    pub fn feed(&mut self, bytes: &[u8], log: &mut RequestLog) {
        if self.disabled {
            return;
        }
        for chunk in bytes.split_inclusive(|b| *b == b'\n') {
            if self.buffer.len() + chunk.len() > MAX_MESSAGE {
                self.disabled = true;
                self.buffer.clear();
                self.data.clear();
                log.field("model_observation", "message_limit");
                return;
            }
            self.buffer.extend_from_slice(chunk);
            if self.sse && self.buffer.last() == Some(&b'\n') {
                let line = self
                    .buffer
                    .strip_suffix(b"\n")
                    .unwrap()
                    .strip_suffix(b"\r")
                    .unwrap_or(self.buffer.strip_suffix(b"\n").unwrap());
                if line.is_empty() {
                    observe_http_json(&self.data, log);
                    self.data.clear();
                } else if let Some(data) = line.strip_prefix(b"data:") {
                    let data = data.strip_prefix(b" ").unwrap_or(data);
                    if self.data.len() + data.len() + 1 > MAX_MESSAGE {
                        self.disabled = true;
                        self.data.clear();
                        log.field("model_observation", "message_limit");
                    } else {
                        if !self.data.is_empty() {
                            self.data.push(b'\n');
                        }
                        self.data.extend_from_slice(data);
                    }
                }
                self.buffer.clear();
                if self.disabled {
                    return;
                }
            }
        }
    }
    pub fn finish(&mut self, log: &mut RequestLog) {
        if self.disabled {
            return;
        }
        if self.sse {
            self.feed(b"\n\n", log);
        } else {
            observe_http_json(&self.buffer, log);
        }
    }
}
fn observe_http_json(bytes: &[u8], log: &mut RequestLog) {
    if bytes == b"[DONE]" {
        if log
            .fields
            .get("model_outcome")
            .and_then(Value::as_str)
            .is_none_or(|s| s == "running")
        {
            log.field("model_outcome", "finished");
        }
        return;
    }
    let Ok(event) = serde_json::from_slice::<Envelope>(bytes) else {
        return;
    };
    event.apply(&mut log.fields);
    if (event.kind.starts_with("response.")
        || event.kind == "message_start"
        || event.object.as_deref() == Some("chat.completion.chunk"))
        && !log.fields.contains_key("model_outcome")
    {
        log.field("model_outcome", "running");
    }
    if (event.kind.ends_with(".delta") || event.kind == "content_block_delta")
        && !log.fields.contains_key("first_token_ms")
    {
        log.field("first_token_ms", log.started.elapsed().as_millis());
    }
    if let Some(outcome) = terminal(&event.kind).or(event.error.as_ref().map(|_| "failed")) {
        log.field("model_outcome", outcome);
        if !event.kind.is_empty() {
            log.field("model_terminal_event", event.kind);
        }
    }
}

struct Call {
    fields: Map<String, Value>,
    started: Instant,
    response_id: Option<String>,
    bytes: usize,
    pending_error: bool,
    deadline_started: tokio::time::Instant,
    output_at: Option<tokio::time::Instant>,
}
/// Per-connection state. It records calls, never assumes one connection is one call.
pub(crate) struct WsCalls {
    logger: Arc<Logger>,
    fields: Map<String, Value>,
    calls: VecDeque<Call>,
    completed: HashSet<String>,
    completed_order: VecDeque<String>,
    client: FrameObserver,
    upstream: FrameObserver,
    observing: bool,
    idle_since: tokio::time::Instant,
    has_started: bool,
}
impl WsCalls {
    pub fn new(log: &RequestLog, extensions: &str) -> Self {
        let compressed = extensions
            .split(',')
            .any(|s| s.trim().starts_with("permessage-deflate"));
        let mut fields = log.fields.clone();
        for key in [
            "status",
            "duration_ms",
            "received_bytes",
            "reason",
            "model_call_id",
        ] {
            fields.remove(key);
        }
        field(&mut fields, "model_transport", "websocket");
        Self {
            logger: log.logger.clone(),
            fields,
            calls: VecDeque::new(),
            completed: HashSet::new(),
            completed_order: VecDeque::new(),
            client: FrameObserver::new(
                compressed,
                extensions.contains("client_no_context_takeover"),
            ),
            upstream: FrameObserver::new(
                compressed,
                extensions.contains("server_no_context_takeover"),
            ),
            observing: true,
            idle_since: tokio::time::Instant::now(),
            has_started: false,
        }
    }
    pub fn feed(&mut self, client: bool, bytes: &[u8]) {
        if !self.observing {
            return;
        }
        // Take one complete frame/message at a time: a read may contain many messages.
        let mut input = bytes;
        while !input.is_empty() {
            let parser = if client {
                &mut self.client
            } else {
                &mut self.upstream
            };
            match parser.next(&mut input) {
                Ok(Some(FrameEvent::Message(message))) => self.message(client, &message),
                Ok(Some(FrameEvent::Close)) => {
                    self.finish(
                        if client { "cancelled" } else { "failed" },
                        if client {
                            "client_closed"
                        } else {
                            "upstream_closed_before_terminal"
                        },
                    );
                    self.observing = false;
                }
                Ok(None) => {}
                Err(reason) => {
                    let mut fields = self.fields.clone();
                    field(&mut fields, "reason", reason);
                    self.logger.write("model_observation_gap", fields);
                    self.finish("unknown", "observation_unavailable");
                    self.observing = false;
                }
            }
            if !self.observing {
                break;
            }
        }
    }
    fn message(&mut self, client: bool, bytes: &[u8]) {
        let Ok(event) = serde_json::from_slice::<Envelope>(bytes) else {
            return;
        };
        if client {
            if event.kind != "response.create" {
                return;
            }
            self.settle_errors();
            if self.calls.len() >= 32 {
                self.finish("unknown", "too_many_pending_calls");
                self.observing = false;
                let mut fields = self.fields.clone();
                field(&mut fields, "reason", "too_many_pending_calls");
                self.logger.write("model_observation_gap", fields);
                return;
            }
            let mut fields = self.fields.clone();
            field(&mut fields, "model_call_id", uuid::Uuid::new_v4());
            field(&mut fields, "wrote_downstream", false);
            event.apply(&mut fields);
            self.logger.write("model_call_started", fields.clone());
            self.calls.push_back(Call {
                fields,
                started: Instant::now(),
                response_id: None,
                bytes: 0,
                pending_error: false,
                deadline_started: tokio::time::Instant::now(),
                output_at: None,
            });
            self.has_started = true;
            return;
        }
        let id = event.response_id();
        if id.is_some_and(|id| self.completed.contains(id)) {
            return;
        }
        let index = id
            .and_then(|id| {
                self.calls
                    .iter()
                    .position(|c| c.response_id.as_deref() == Some(id))
            })
            .or_else(|| {
                if id.is_some() {
                    self.calls.iter().position(|c| c.response_id.is_none())
                } else if self.calls.len() == 1 {
                    Some(0)
                } else {
                    None
                }
            });
        let Some(index) = index else {
            return;
        };
        let call = &mut self.calls[index];
        if call.output_at.is_some() || semantic_output(&event.kind) {
            call.output_at = Some(tokio::time::Instant::now());
        }
        if let Some(id) = id {
            call.response_id = Some(id.to_owned());
        }
        call.bytes += bytes.len();
        field(&mut call.fields, "wrote_downstream", true);
        event.apply(&mut call.fields);
        if event.kind.ends_with(".delta") && !call.fields.contains_key("first_token_ms") {
            field(
                &mut call.fields,
                "first_token_ms",
                call.started.elapsed().as_millis(),
            );
        }
        if event.kind == "error" {
            // A following response.failed is authoritative; do not count both.
            call.pending_error = true;
            self.idle_since = tokio::time::Instant::now();
        } else if let Some(outcome) = terminal(&event.kind) {
            let call = self.calls.remove(index).unwrap();
            self.emit(call, outcome, &event.kind);
        }
    }
    fn emit(&mut self, mut call: Call, outcome: &str, reason: &str) {
        self.idle_since = tokio::time::Instant::now();
        if let Some(id) = call.response_id {
            self.completed.insert(id.clone());
            self.completed_order.push_back(id);
            if self.completed_order.len() > 1024 {
                if let Some(id) = self.completed_order.pop_front() {
                    self.completed.remove(&id);
                }
            }
        }
        field(
            &mut call.fields,
            "duration_ms",
            call.started.elapsed().as_millis(),
        );
        field(&mut call.fields, "received_bytes", call.bytes);
        field(&mut call.fields, "model_terminal_event", reason);
        self.logger
            .write(&format!("model_call_{outcome}"), call.fields);
    }
    fn settle_errors(&mut self) {
        let mut index = 0;
        while index < self.calls.len() {
            if self.calls[index].pending_error {
                let call = self.calls.remove(index).unwrap();
                self.emit(call, "failed", "error");
            } else {
                index += 1;
            }
        }
    }
    pub fn finish(&mut self, outcome: &str, reason: &str) {
        self.settle_errors();
        while let Some(call) = self.calls.pop_front() {
            self.emit(call, outcome, reason);
        }
    }
    pub fn deadline(
        &self,
        settings: &crate::config::WebSocketTimeouts,
    ) -> Option<(tokio::time::Instant, &'static str)> {
        use std::time::Duration;
        // A bare error ends active waiting, but settlement waits for a possible
        // authoritative response.failed, a new turn, or connection close.
        if !self.observing {
            return None;
        }
        if let Some(deadline) = self
            .calls
            .iter()
            .filter(|call| !call.pending_error)
            .map(|call| {
                if let Some(at) = call.output_at {
                    (
                        at + Duration::from_secs_f64(settings.read_seconds),
                        "websocket_read_timeout",
                    )
                } else {
                    (
                        call.deadline_started
                            + Duration::from_secs_f64(settings.first_output_seconds),
                        "websocket_first_output_timeout",
                    )
                }
            })
            .min_by_key(|(at, _)| *at)
        {
            return Some(deadline);
        }
        if !self.has_started {
            Some((
                self.idle_since + Duration::from_secs_f64(settings.first_message_seconds),
                "websocket_first_message_timeout",
            ))
        } else if settings.inter_turn_idle_seconds > 0.0 {
            Some((
                self.idle_since + Duration::from_secs_f64(settings.inter_turn_idle_seconds),
                "websocket_inter_turn_idle_timeout",
            ))
        } else {
            None
        }
    }
    pub fn annotate_pending(&mut self, key: &str, value: &str) {
        for call in &mut self.calls {
            field(&mut call.fields, key, value);
        }
    }
    pub fn is_observing(&self) -> bool {
        self.observing
    }
}
fn semantic_output(kind: &str) -> bool {
    match kind {
        ""
        | "response.created"
        | "response.in_progress"
        | "response.output_item.added"
        | "response.output_item.done" => false,
        _ => kind.contains(".delta") || kind.starts_with("response.output"),
    }
}
impl Drop for WsCalls {
    fn drop(&mut self) {
        self.finish("cancelled", "connection_dropped");
    }
}

// An observer only: it never writes frames, changes masking, or alters flow control.
// Buffers are bounded, including inflated data and fragmented messages.
enum FrameEvent {
    Message(Vec<u8>),
    Close,
}
struct FrameObserver {
    header: Vec<u8>,
    remaining: u64,
    offset: usize,
    mask: Option<[u8; 4]>,
    opcode: u8,
    fin: bool,
    fragmented: bool,
    compressed: bool,
    message: Vec<u8>,
    allow_compression: bool,
    reset_compression: bool,
    inflater: flate2::Decompress,
}
impl FrameObserver {
    fn new(allow_compression: bool, reset_compression: bool) -> Self {
        Self {
            header: Vec::with_capacity(14),
            remaining: 0,
            offset: 0,
            mask: None,
            opcode: 0,
            fin: false,
            fragmented: false,
            compressed: false,
            message: Vec::new(),
            allow_compression,
            reset_compression,
            inflater: flate2::Decompress::new(false),
        }
    }
    fn next(&mut self, input: &mut &[u8]) -> Result<Option<FrameEvent>, &'static str> {
        if self.header.is_empty() || self.remaining == 0 {
            while self.header.len() < 2 && !input.is_empty() {
                self.header.push(input[0]);
                *input = &input[1..];
            }
            if self.header.len() < 2 {
                return Ok(None);
            }
            let len_code = self.header[1] & 127;
            let len_bytes = match len_code {
                126 => 2,
                127 => 8,
                _ => 0,
            };
            let masked = self.header[1] & 128 != 0;
            let header_len = 2 + len_bytes + if masked { 4 } else { 0 };
            let n = (header_len - self.header.len()).min(input.len());
            self.header.extend_from_slice(&input[..n]);
            *input = &input[n..];
            if self.header.len() < header_len {
                return Ok(None);
            }
            self.opcode = self.header[0] & 15;
            self.fin = self.header[0] & 128 != 0;
            let rsv1 = self.header[0] & 64 != 0;
            if self.header[0] & 48 != 0 {
                return Err("unsupported_websocket_extension");
            }
            self.remaining = match len_code {
                126 => u16::from_be_bytes(self.header[2..4].try_into().unwrap()) as u64,
                127 => u64::from_be_bytes(self.header[2..10].try_into().unwrap()),
                n => n as u64,
            };
            self.mask = masked.then(|| self.header[header_len - 4..].try_into().unwrap());
            self.offset = 0;
            match self.opcode {
                1 | 2 => {
                    if self.fragmented {
                        return Err("invalid_websocket_fragment");
                    }
                    self.message.clear();
                    self.compressed = rsv1;
                    if rsv1 && !self.allow_compression {
                        return Err("unexpected_websocket_compression");
                    }
                    self.fragmented = !self.fin;
                }
                0 => {
                    if !self.fragmented || rsv1 {
                        return Err("invalid_websocket_continuation");
                    }
                }
                8..=10 => {
                    if !self.fin || rsv1 || self.remaining > 125 {
                        return Err("invalid_websocket_control");
                    }
                }
                _ => return Err("unsupported_websocket_opcode"),
            }
            if self.opcode < 8
                && self.remaining > MAX_MESSAGE.saturating_sub(self.message.len()) as u64
            {
                return Err("websocket_message_limit");
            }
        }
        let n = self.remaining.min(input.len() as u64) as usize;
        if self.opcode < 8 {
            self.message.extend(
                input[..n]
                    .iter()
                    .enumerate()
                    .map(|(i, b)| b ^ self.mask.map(|m| m[(self.offset + i) % 4]).unwrap_or(0)),
            );
        }
        self.offset += n;
        self.remaining -= n as u64;
        *input = &input[n..];
        if self.remaining > 0 {
            return Ok(None);
        }
        self.header.clear();
        if self.opcode == 8 {
            return Ok(Some(FrameEvent::Close));
        }
        if self.opcode >= 8 || !self.fin {
            return Ok(None);
        }
        self.fragmented = false;
        let message = std::mem::take(&mut self.message);
        if !self.compressed {
            return Ok(Some(FrameEvent::Message(message)));
        }
        let inflated = self.inflate(message)?;
        Ok(Some(FrameEvent::Message(inflated)))
    }
    fn inflate(&mut self, mut data: Vec<u8>) -> Result<Vec<u8>, &'static str> {
        data.extend_from_slice(&[0, 0, 255, 255]);
        let mut input = data.as_slice();
        let mut output = Vec::new();
        loop {
            let mut chunk = [0; 8192];
            let before_in = self.inflater.total_in();
            let before_out = self.inflater.total_out();
            self.inflater
                .decompress(input, &mut chunk, flate2::FlushDecompress::Sync)
                .map_err(|_| "invalid_websocket_deflate")?;
            let consumed = (self.inflater.total_in() - before_in) as usize;
            let written = (self.inflater.total_out() - before_out) as usize;
            if output.len() + written > MAX_MESSAGE {
                return Err("websocket_inflated_limit");
            }
            output.extend_from_slice(&chunk[..written]);
            input = &input[consumed..];
            if written < chunk.len() && input.is_empty() {
                break;
            }
            if consumed == 0 && written == 0 {
                return Err("invalid_websocket_deflate");
            }
        }
        if self.reset_compression {
            self.inflater.reset(false);
        }
        Ok(output)
    }
}

#[cfg(test)]
pub(crate) struct ObservedSocket<T> {
    pub io: T,
    pub calls: std::sync::Arc<std::sync::Mutex<WsCalls>>,
    pub client: bool,
}
#[cfg(test)]
impl<T: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for ObservedSocket<T> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let result = std::pin::Pin::new(&mut self.io).poll_read(cx, buf);
        if let std::task::Poll::Ready(Ok(())) = result {
            self.calls
                .lock()
                .unwrap()
                .feed(self.client, &buf.filled()[before..]);
        }
        result
    }
}
#[cfg(test)]
impl<T: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for ObservedSocket<T> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.io).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn log_at(path: &std::path::Path) -> RequestLog {
        RequestLog { logger: Arc::new(Logger::new(path.to_owned())),
            fields: serde_json::from_value(json!({"request_id":"connection", "method":"GET", "path":"/v1/responses", "provider":"account", "status":"101"})).unwrap(),
            started: Instant::now(), status: 101, bytes: 0, outcome: "request_finished" }
    }
    fn records(path: &std::path::Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect()
    }
    fn frame(op: u8, fin: bool, compressed: bool, mask: bool, payload: &[u8]) -> Vec<u8> {
        let mut frame = vec![op | if fin { 128 } else { 0 } | if compressed { 64 } else { 0 }];
        let m = if mask { 128 } else { 0 };
        if payload.len() < 126 {
            frame.push(m | payload.len() as u8);
        } else if payload.len() <= u16::MAX as usize {
            frame.push(m | 126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        } else {
            frame.push(m | 127);
            frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }
        let key = [1, 9, 8, 4];
        if mask {
            frame.extend_from_slice(&key);
        }
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(i, b)| b ^ if mask { key[i % 4] } else { 0 }),
        );
        frame
    }
    fn send(calls: &mut WsCalls, client: bool, message: Value) {
        let bytes = frame(1, true, false, client, message.to_string().as_bytes());
        // Exercise header, mask and payload splits at every byte.
        for byte in bytes {
            calls.feed(client, &[byte]);
        }
    }
    #[test]
    fn model_category_matches_inference_not_management_endpoints() {
        for (method, path, expected) in [
            ("POST", "/v1/responses", true),
            ("POST", "/backend-api/codex/responses/compact", true),
            (
                "GET",
                "/codex/https://example.invalid/v1/responses?model=x",
                true,
            ),
            ("POST", "/anthropic/v1/messages", true),
            (
                "POST",
                "/claude/https://example.invalid/api/v1/messages",
                true,
            ),
            ("POST", "/v1/chat/completions", true),
            ("POST", "/v1/completions", true),
            ("POST", "/anthropic/v1/messages/count_tokens", false),
            ("GET", "/v1/responses/response-id", false),
            ("GET", "/backend-api/wham/usage", false),
            ("GET", "/anthropic/api/oauth/usage", false),
            ("POST", "/anthropic/v1/oauth/token", false),
            ("POST", "/backend-api/codex/analytics-events/events", false),
            ("POST", "/mcp/openaiDeveloperDocs", false),
            ("GET", "/v1/models", false),
            ("CONNECT", "api.anthropic.com:443", false),
            ("POST", "/v1/not-responses", false),
        ] {
            assert_eq!(is_model_endpoint(method, path), expected, "{method} {path}");
        }
    }

    #[test]
    fn multiple_turns_terminal_dedup_tokens_and_no_content_logging() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let log = log_at(&path);
        let mut calls = WsCalls::new(&log, "");
        for id in ["one", "two"] {
            send(
                &mut calls,
                true,
                json!({"type":"response.create", "model":"test-model", "input":"PRIVATE PROMPT"}),
            );
            send(
                &mut calls,
                false,
                json!({"type":"response.created", "response":{"id":id}}),
            );
            send(
                &mut calls,
                false,
                json!({"type":"response.output_text.delta", "response_id":id,"delta":"PRIVATE ANSWER"}),
            );
            let done = json!({"type":"response.completed", "response":{"id":id,"usage":{"input_tokens":12,"output_tokens":3,"input_tokens_details":{"cached_tokens":4}}}});
            send(&mut calls, false, done.clone());
            send(&mut calls, false, done);
        }
        let rows = records(&path);
        assert_eq!(
            rows.iter()
                .filter(|r| r["event"] == "model_call_started")
                .count(),
            2
        );
        let done: Vec<_> = rows
            .iter()
            .filter(|r| r["event"] == "model_call_finished")
            .collect();
        assert_eq!(done.len(), 2);
        assert_ne!(done[0]["model_call_id"], done[1]["model_call_id"]);
        assert_eq!(done[0]["input_tokens"], "12");
        assert_eq!(done[1]["output_tokens"], "3");
        assert_eq!(done[0]["cached_input_tokens"], "4");
        assert!(done[0].get("status").is_none());
        assert!(!std::fs::read_to_string(path).unwrap().contains("PRIVATE"));
    }
    #[test]
    fn malformed_usage_does_not_hide_a_successful_terminal_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let log = log_at(&path);
        let mut calls = WsCalls::new(&log, "");
        send(&mut calls, true, json!({"type":"response.create"}));
        send(
            &mut calls,
            false,
            json!({"type":"response.completed", "response":{"id":"ok", "usage":{"input_tokens":"bad", "output_tokens":3}}}),
        );
        let rows = records(&path);
        let done = rows
            .iter()
            .find(|r| r["event"] == "model_call_finished")
            .unwrap();
        assert!(done.get("input_tokens").is_none());
        assert!(done.get("output_tokens").is_none());
        assert!(calls.calls.is_empty());
    }

    #[test]
    fn error_then_failed_is_one_turn_and_cancellation_is_separate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let log = log_at(&path);
        let mut calls = WsCalls::new(&log, "");
        send(&mut calls, true, json!({"type":"response.create"}));
        send(
            &mut calls,
            false,
            json!({"type":"error", "error":{"message":"PRIVATE ERROR"}}),
        );
        send(
            &mut calls,
            false,
            json!({"type":"response.failed", "response":{"id":"bad","usage":{"input_tokens":10,"output_tokens":0}}}),
        );
        send(&mut calls, true, json!({"type":"response.create"}));
        calls.feed(true, &frame(8, true, false, true, &[]));
        let rows = records(&path);
        assert_eq!(
            rows.iter()
                .filter(|r| r["event"] == "model_call_failed")
                .count(),
            1
        );
        assert_eq!(
            rows.iter()
                .filter(|r| r["event"] == "model_call_cancelled")
                .count(),
            1
        );
        assert_eq!(
            rows.iter()
                .find(|r| r["event"] == "model_call_failed")
                .unwrap()["input_tokens"],
            "10"
        );
        assert!(!std::fs::read_to_string(path).unwrap().contains("PRIVATE"));
    }
    #[test]
    fn fragmented_messages_ping_extended_lengths_and_deflate_context_takeover() {
        for reset in [true, false] {
            let mut observer = FrameObserver::new(true, reset);
            let mut compressor = flate2::Compress::new(flate2::Compression::fast(), false);
            for _ in 0..2 {
                let payload = format!(
                    "{{\"type\":\"response.create\",\"input\":\"{}\"}}",
                    "context repeated ".repeat(100)
                );
                let mut compressed = vec![0; payload.len() + 100];
                let before = compressor.total_out();
                compressor
                    .compress(
                        payload.as_bytes(),
                        &mut compressed,
                        flate2::FlushCompress::Sync,
                    )
                    .unwrap();
                compressed.truncate((compressor.total_out() - before) as usize);
                assert!(compressed.ends_with(&[0, 0, 255, 255]));
                compressed.truncate(compressed.len() - 4);
                let middle = compressed.len() / 2;
                let wire = [
                    frame(1, false, true, true, &compressed[..middle]),
                    frame(9, true, false, true, b"ping"),
                    frame(0, true, false, true, &compressed[middle..]),
                ]
                .concat();
                let mut messages = Vec::new();
                for byte in wire {
                    if let Some(FrameEvent::Message(data)) =
                        observer.next(&mut &[byte][..]).unwrap()
                    {
                        messages.push(data);
                    }
                }
                assert_eq!(messages, [payload.as_bytes()]);
                if reset {
                    compressor.reset();
                }
            }
        }
        for length in [126, 65536] {
            let payload = vec![b'x'; length];
            let wire = frame(1, true, false, false, &payload);
            let mut parser = FrameObserver::new(false, false);
            let mut input = wire.as_slice();
            let Some(FrameEvent::Message(message)) = parser.next(&mut input).unwrap() else {
                panic!("missing message");
            };
            assert_eq!(message, payload);
            assert!(input.is_empty());
        }
    }
    #[test]
    fn parser_limits_do_not_invent_calls_and_pending_calls_finish_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let log = log_at(&path);
        {
            let mut calls = WsCalls::new(&log, "");
            send(&mut calls, true, json!({"type":"response.create"}));
            // A huge advertised size must fail before allocating its payload.
            let mut wire = vec![0x81, 127];
            wire.extend_from_slice(&(MAX_MESSAGE as u64 + 1).to_be_bytes());
            calls.feed(false, &wire);
            assert!(!calls.observing);
        }
        {
            let mut calls = WsCalls::new(&log, "");
            send(&mut calls, true, json!({"type":"response.create"}));
        }
        let rows = records(&path);
        assert_eq!(
            rows.iter()
                .filter(|r| r["event"] == "model_observation_gap")
                .count(),
            1
        );
        assert_eq!(
            rows.iter()
                .filter(|r| r["event"] == "model_call_cancelled")
                .count(),
            1
        );
    }
    #[test]
    fn http_sse_usage_snapshots_failures_and_json_do_not_duplicate_calls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        {
            let mut log = log_at(&path);
            log.status = 200;
            log.field("model_call_id", "http");
            log.field("model_transport", "http");
            log.event("request_received");
            let mut observer = HttpObserver::new(true);
            let sse = concat!(
                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg\",\"model\":\"claude\",\"usage\":{\"input_tokens\":20,\"output_tokens\":1}}}\r\n\r\n",
                "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":6}}\n\n",
                "data: {\"type\":\"message_stop\"}\n\n"
            );
            for b in sse.bytes() {
                observer.feed(&[b], &mut log);
            }
            observer.finish(&mut log);
            assert_eq!(log.fields["input_tokens"], "20");
            assert_eq!(log.fields["output_tokens"], "6");
            assert_eq!(log.fields["model_outcome"], "finished");
        }
        let rows = records(&path);
        assert_eq!(
            rows.iter()
                .filter(|r| r["event"] == "model_call_finished")
                .count(),
            1
        );
        let mut log = log_at(&path);
        let mut observer = HttpObserver::new(false);
        observer.feed(
            br#"{"model":"gpt","usage":{"prompt_tokens":7,"completion_tokens":2}}"#,
            &mut log,
        );
        observer.finish(&mut log);
        assert_eq!(log.fields["input_tokens"], "7");
        let mut observer = HttpObserver::new(true);
        observer.feed(
            b"data: {\"type\":\"response.failed\",\"response\":{\"id\":\"failed\"}}\n\n",
            &mut log,
        );
        assert_eq!(log.fields["model_outcome"], "failed");
    }
    #[test]
    fn http_incomplete_response_is_not_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        {
            let mut log = log_at(&path);
            log.status = 200;
            log.field("model_call_id", "http");
            let mut observer = HttpObserver::new(true);
            observer.feed(
                b"data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"cut\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n",
                &mut log,
            );
            observer.finish(&mut log);
        }
        let rows = records(&path);
        let done: Vec<_> = rows
            .iter()
            .filter(|r| r["event"].as_str().unwrap().starts_with("model_call_"))
            .collect();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0]["event"], "model_call_incomplete");
        assert_eq!(done[0]["incomplete_reason"], "max_output_tokens");
    }
    #[tokio::test]
    async fn observation_preserves_wire_bytes_and_records_turn_before_connection_closes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.log");
        let log = log_at(&path);
        let calls = Arc::new(std::sync::Mutex::new(WsCalls::new(&log, "")));
        let (mut client, down) = tokio::io::duplex(4096);
        let (up, mut server) = tokio::io::duplex(4096);
        let mut down = ObservedSocket {
            io: down,
            calls: calls.clone(),
            client: true,
        };
        let mut up = ObservedSocket {
            io: up,
            calls: calls.clone(),
            client: false,
        };
        let task =
            tokio::spawn(async move { tokio::io::copy_bidirectional(&mut down, &mut up).await });
        for id in ["a", "b"] {
            let request = frame(
                1,
                true,
                false,
                true,
                br#"{"type":"response.create","model":"gpt"}"#,
            );
            client.write_all(&request).await.unwrap();
            let mut got = vec![0; request.len()];
            server.read_exact(&mut got).await.unwrap();
            assert_eq!(got, request);
            let response = frame(
                1,
                true,
                false,
                false,
                json!({"type":"response.completed","response":{"id":id}})
                    .to_string()
                    .as_bytes(),
            );
            server.write_all(&response).await.unwrap();
            let mut got = vec![0; response.len()];
            client.read_exact(&mut got).await.unwrap();
            assert_eq!(got, response);
            assert!(!task.is_finished());
        }
        assert_eq!(
            records(&path)
                .iter()
                .filter(|r| r["event"] == "model_call_finished")
                .count(),
            2
        );
        client.shutdown().await.unwrap();
        server.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }
}
