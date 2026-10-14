# Logs

Logs are written to `logs/proxy.log` beside the application configuration file. Override the file location with `--log-file /path/to/proxy.log`.
Records are also mirrored to stderr when it is a terminal, so a foreground run
shows them as before. Background runners (launchd, systemd, the desktop daemon)
and redirected stderr receive records only while the log file cannot be opened
or written; startup errors and log maintenance failures always go to stderr.
File writes, rotation and pruning run on a dedicated logging thread. Producers do
not wait for disk I/O. Queues hold at most 1024 records per logger, with an 8 MiB
process-wide buffer budget and a 256 KiB limit per record. Saturation emits a
`log_records_dropped` count instead of blocking requests. Normal shutdown drains
accepted records; forcibly killing a process can lose records still queued.

```sh
tail -F logs/proxy.log
```

Each line is JSON with a UTC timestamp. Log files rotate at 5 MiB, keeping the
latest backup as `proxy.log.1` and moving older backups into `logs/history/` as
timestamped JSONL archives. Archives are kept for at least 30 days after creation;
cleanup runs at startup and rotation, and retains files modified within the last
30 days. Retention is based on age, with no file-count cap that could discard busy
days early. Traffic reads the active file, latest backup, and historical archives,
deduplicating imported request records. History from before this policy can only
be shown if the source logs still exist. Unix files use mode `0600`; Windows files
inherit directory ACLs, so keep them in your private user directory. If archiving
fails, logging continues in the current file; failures are also reported on stderr.

Traffic groups API configurations by service and the logged `upstream_base_url`,
matching the complete base (including its path and port) against the current
configuration; account identities remain separate. A logged name and base that
match one configuration count under it. Records from a renamed configuration
(same base), one whose base URL changed (same name), or records logged before
base URLs were recorded (name only) are merged into the current configuration
and marked for review in the panel. Records whose base is used by several
configurations, or whose name and base match none, count as `Unidentified` and
are also marked. For each marked name and base, the panel can assign the
records to a current configuration or to `Unidentified`. These choices are
stored in `traffic-compatibility.json` beside the GUI settings (Settings →
Files → Traffic Compatibility); deleting it restores automatic matching. This
reconciliation is read-only and never rewrites retained logs.

| Event | Meaning |
| --- | --- |
| `current_route` | Account ID and proxy configured at startup; not a connectivity test. |
| `route_unavailable` | Startup credentials or mapping could not be read; later requests retry credential loading using the startup configuration. |
| `request_received` | Request method, path without query, unique request ID, and `service`. |
| `route_selected` | Account and proxy actually selected for this request. |
| `upstream_response` | Upstream HTTP status and time to response headers (`headers_ms`). |
| `route_health` | Effective route eligibility, consecutive failures and feedback source (`probe` or `request`). |
| `upstream_retry` | A bodyless GET transport failure before response headers; includes `upstream_attempts`, redacted `transport_error` category, and `retry_delay_ms`. |
| `request_finished` | Transfer completed, including status, duration, and received bytes; check status for upstream errors. |
| `request_cancelled` | Request processing was dropped (for example, client disconnect or shutdown). No status is logged if no response was established; the UI shows CANCEL. |
| `request_rejected` / `request_failed` | Authentication, configuration, connection, or streaming failure with diagnostic context. |
| `model_call_started` / `model_call_updated` | One HTTP model request or WebSocket `response.create`, identified by `model_call_id` separately from its connection's `request_id`. |
| `model_call_finished` / `model_call_failed` / `model_call_cancelled` | Per-call outcome, model, response ID, duration, and reported input/output/cache token usage when available. WebSocket success requires a model terminal event delivered to the client. |
| `model_call_incomplete` | The model ended the response early with `response.incomplete` (for example `max_output_tokens`, recorded as `incomplete_reason`). Not counted as an error. |
| `model_call_unknown` / `model_observation_gap` | A call outcome could not be observed reliably, or observation exceeded a bounded parser limit; never counted as a successful model response. |

Every record of a proxied request carries `service`, `codex` or `claude`: the app
the proxy routes the request to, decided from its target before the first record.
Token refreshes belong to their app. Routed requests also carry
`credential_match`: `configured` when the credential is one the configuration
holds or maps (a saved login, a named API Key, provider or settings, or an account
`routing.account` names), and `unmatched` when only a fallback or an explicit URL
route, which does not check it, carries the request. Any local program may open a
CONNECT, so its end record is written once the first request in the tunnel is
routed and takes that request's `service` and account only when it is
`configured`. A CONNECT whose TLS handshake fails, that closes before a request,
whose first request is unmatched, or that is refused has no `service`, whatever
its host. A CONNECT with one counts as a request of that app, but never toward
latency. Traffic statistics count requests of the two apps only.

Request logs keep the proxy-generated `request_id` separate from the client's
`client_request_id` and `session_id`. Session headers are checked in this order:
`session_id`, `x-session-id`, `x-codex-session-id`, `x-claude-code-session-id`.
Client request headers are checked in this order: `x-client-request-id`,
`x-request-id`, `request-id`, `request_id`. The corresponding `*_source` fields
identify the selected header or body field. IDs must be nonempty printable ASCII,
without spaces, and at most 256 bytes; ambiguous duplicate headers are skipped.
Missing or invalid IDs are recorded as `null`, never generated as client IDs.

For uncompressed JSON model requests, absent header IDs can be filled from
`session_id` or `metadata.session_id`, and `client_request_id` or `request_id`.
Claude's `metadata.user_id` JSON-string session field and legacy
`user_<hash>_account_<id>_session_<uuid>` form are also recognized; the user/account
portion is never logged. Body IDs appear in subsequent lifecycle records, since
`request_received` and HTTP `model_call_started` precede body parsing. Compressed
request bodies are not decoded, so their IDs must be supplied in headers.
WebSocket calls inherit the handshake session ID unless the individual
`response.create` supplies one. Each call reads its own request ID from
`client_request_id`, `request_id`, or `event_id`, in that order; the handshake ID
is retained separately as `connection_client_request_id` and is never reused as
a per-call client request ID. Upstream response IDs remain `response_id`.

The GUI's **Model Calls** scope counts each HTTP generation/compaction request and
each WebSocket generation, including active calls. **All** counts network requests
and connections instead. Model calls are deduplicated by call ID across log rotation.
New model calls are read from explicit `model_call_*` events with a `model_call_id`.
Historical HTTP generation/compaction requests also count as one call each;
requests already carrying a call ID are excluded from this fallback to avoid double
counting. WebSocket connection records are never used to infer calls.
The client's `Accept-Encoding` is forwarded unchanged, so responses arrive compressed
as the client negotiated and `received_bytes` counts the bytes actually transferred.
HTTP model calls also record `response_content_type` and `response_content_encoding`
when present. Compressed responses (`gzip`, `deflate`, `br`, `zstd`) are decoded
incrementally as they stream through, keeping only the fields above; forwarded bytes
are unchanged and no body is stored. Compressed request bodies are forwarded without
decoding, so the model is taken from the response. Event streams are recognized by
their content even without a `text/event-stream` content type. When a response
cannot be observed, `model_observation` records why: `unsupported_encoding`,
`decode_error`, `message_limit` or `memory_budget`.
Token totals include only reported usage; missing usage displays as unknown, not zero.
These totals are not provider quota or billing measurements. No prompts, generated
text, or tool arguments are written to these logs.

Responses WebSocket sessions have independent `websocket` timeout settings:
`first_message_seconds` (30), `first_output_seconds` (900), `read_seconds` (900),
`write_seconds` (120), and `inter_turn_idle_seconds` (300). Values are seconds;
only inter-turn idle accepts 0 to disable its timer. The first-output deadline
waits for semantic output, not a handshake, `response.created`, or heartbeats.
After output begins, upstream model messages refresh the read deadline. Successful
terminal delivery starts the between-turn idle timer; new calls start fresh timers.
Writes have a separate timeout budget, including across fragments. Idle sessions
close with code 1000; first-message/output/read timeouts use 1001 and write failures
use 1011. Close frames are sent where the transport permits, never inside a partial
data frame. Connection logs record the stage, direction, close code, frame size and
whether downstream bytes were written. An unfinished call remains failed even when
the upstream closes normally. Other upgraded connections retain the shared
byte-activity timeout from `request_timeout_seconds`.

The WebSocket observer handles masking, fragmentation and negotiated
`permessage-deflate` (including context takeover). Frames and reconstructed or
inflated messages are limited to 32 MiB. Exceeding observation limits records a gap
and closes the model WebSocket; it does not silently fabricate successful calls.
HTTP observation buffers, WebSocket frames and reconstructed/inflated messages
share a process-wide 128 MiB allocation budget. HTTP observation stops and releases
its buffers when that budget is exhausted; forwarded bytes are unchanged.
A model WebSocket closes in a controlled way instead of disabling its phase timers:
frame allocation exhaustion uses close code 1013, and observation exhaustion records
a gap before closing. This budget covers these buffers, not total process RSS.

The GUI's exit-IP lookup and proxy-port diagnostic are display-only checks.
They do not feed the daemon's per-destination route-health cache; a reachable
proxy or successful IP lookup does not prove that a specific API origin is reachable.

Logs contain **full account IDs and proxy endpoints**. They do not record tokens, authentication headers, query parameters, or request/response bodies. Keep logs private. `/health` requests are excluded from request logs.
