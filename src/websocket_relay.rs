//! Responses WebSocket relay with protocol-phase deadlines. CONNECT uses relay.rs.
use crate::{config::WebSocketTimeouts, logger::RequestLog, model_calls::WsCalls};
use std::{
    io,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::Notify,
    time::Instant,
};

const MAX_FRAME: usize = 32 * 1024 * 1024;
#[derive(Debug)]
struct FrameTooLarge;
impl std::fmt::Display for FrameTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WebSocket frame exceeds limit")
    }
}
impl std::error::Error for FrameTooLarge {}
#[derive(Default)]
struct Trace {
    sent: u64,
    received: u64,
    client_frames: u64,
    upstream_frames: u64,
    client_write_partial: bool,
    upstream_write_partial: bool,
}
struct Exit {
    stage: &'static str,
    client: bool,
    code: u16,
    peer_close: bool,
    frame_bytes: usize,
    error_kind: Option<io::ErrorKind>,
}
impl Exit {
    fn failure(stage: &'static str, client: bool, error: io::Error, frame_bytes: usize) -> Self {
        Self {
            stage,
            client,
            code: if error.get_ref().is_some_and(|e| e.is::<FrameTooLarge>()) {
                1009
            } else if error.kind() == io::ErrorKind::InvalidData {
                1002
            } else {
                1011
            },
            peer_close: false,
            frame_bytes,
            error_kind: Some(error.kind()),
        }
    }
}
struct Frame {
    wire: Vec<u8>,
    opcode: u8,
    fin: bool,
    close_code: Option<u16>,
}
async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R, client: bool) -> io::Result<Frame> {
    let mut header = [0; 2];
    reader.read_exact(&mut header).await?;
    let opcode = header[0] & 15;
    let fin = header[0] & 128 != 0;
    let masked = header[1] & 128 != 0;
    if masked != client || header[0] & 48 != 0 || !matches!(opcode, 0 | 1 | 2 | 8 | 9 | 10) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid WebSocket frame",
        ));
    }
    let mut wire = header.to_vec();
    let length = match header[1] & 127 {
        126 => {
            let mut bytes = [0; 2];
            reader.read_exact(&mut bytes).await?;
            wire.extend_from_slice(&bytes);
            let n = u16::from_be_bytes(bytes) as u64;
            if n < 126 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Noncanonical frame length",
                ));
            }
            n
        }
        127 => {
            let mut bytes = [0; 8];
            reader.read_exact(&mut bytes).await?;
            wire.extend_from_slice(&bytes);
            let n = u64::from_be_bytes(bytes);
            if n < 65536 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Noncanonical frame length",
                ));
            }
            n
        }
        n => n as u64,
    };
    if length > MAX_FRAME as u64 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, FrameTooLarge));
    }
    if opcode >= 8 && (!fin || header[0] & 64 != 0 || length > 125) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid control frame",
        ));
    }
    let mut mask = [0; 4];
    if masked {
        reader.read_exact(&mut mask).await?;
        wire.extend_from_slice(&mask);
    }
    let offset = wire.len();
    wire.resize(offset + length as usize, 0);
    reader.read_exact(&mut wire[offset..]).await?;
    let close_code = if opcode == 8 {
        let payload: Vec<u8> = wire[offset..]
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ if masked { mask[i % 4] } else { 0 })
            .collect();
        if payload.len() == 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid close payload",
            ));
        }
        if payload.is_empty() {
            Some(1005) // Local-only status: the peer supplied no close code.
        } else {
            let code = u16::from_be_bytes([payload[0], payload[1]]);
            if !(matches!(code,1000..=1003|1007..=1014) || (3000..=4999).contains(&code))
                || std::str::from_utf8(&payload[2..]).is_err()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Invalid close status",
                ));
            }
            Some(code)
        }
    } else {
        None
    };
    Ok(Frame {
        wire,
        opcode,
        fin,
        close_code,
    })
}
async fn pump<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    client: bool,
    calls: &Arc<Mutex<WsCalls>>,
    changed: &Notify,
    trace: &Mutex<Trace>,
    timeout: Duration,
) -> Exit {
    let mut fragmented = false;
    let mut remaining = timeout;
    loop {
        let frame = match read_frame(reader, client).await {
            Ok(frame) => frame,
            Err(error) => {
                return Exit::failure(
                    if client {
                        "read_client"
                    } else {
                        "read_upstream"
                    },
                    client,
                    error,
                    0,
                );
            }
        };
        if frame.opcode < 8 {
            if (frame.opcode == 0) != fragmented {
                return Exit::failure(
                    "invalid_fragment",
                    client,
                    io::Error::new(io::ErrorKind::InvalidData, "Invalid fragment sequence"),
                    frame.wire.len(),
                );
            }
            if !fragmented {
                remaining = timeout;
            }
            fragmented = !frame.fin;
        }
        // Start a call before forwarding it; observe upstream results only after
        // delivery, so a failed terminal write cannot record a successful call.
        if client && frame.opcode != 8 {
            calls.lock().unwrap().feed(true, &frame.wire);
            changed.notify_one();
        }
        {
            let mut t = trace.lock().unwrap();
            if client {
                t.upstream_write_partial = true;
            } else {
                t.client_write_partial = true;
            }
        }
        let began = Instant::now();
        let budget = if frame.opcode >= 8 {
            timeout
        } else {
            remaining
        };
        let write = async {
            let mut data = frame.wire.as_slice();
            while !data.is_empty() {
                let n = writer.write(data).await?;
                if n == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "WebSocket write stalled",
                    ));
                }
                let mut t = trace.lock().unwrap();
                if client {
                    t.sent += n as u64;
                } else {
                    t.received += n as u64;
                }
                data = &data[n..];
            }
            writer.flush().await
        };
        let result = tokio::time::timeout(budget, write).await;
        if frame.opcode < 8 {
            remaining = remaining.saturating_sub(began.elapsed());
        }
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                return Exit::failure(
                    if client {
                        "write_upstream"
                    } else {
                        "write_client"
                    },
                    !client,
                    error,
                    frame.wire.len(),
                );
            }
            Err(_) => {
                return Exit::failure(
                    if client {
                        "write_upstream_timeout"
                    } else {
                        "write_client_timeout"
                    },
                    !client,
                    io::Error::new(io::ErrorKind::TimedOut, "WebSocket write timeout"),
                    frame.wire.len(),
                );
            }
        }
        {
            let mut t = trace.lock().unwrap();
            if client {
                t.upstream_write_partial = false;
                t.client_frames += 1;
            } else {
                t.client_write_partial = false;
                t.upstream_frames += 1;
            }
        }
        if frame.opcode == 8 {
            calls.lock().unwrap().annotate_pending(
                "websocket_close_code",
                &frame.close_code.unwrap_or(1000).to_string(),
            );
            return Exit {
                stage: if client {
                    "client_close"
                } else {
                    "upstream_close"
                },
                client,
                code: frame.close_code.unwrap_or(1000),
                peer_close: true,
                frame_bytes: frame.wire.len(),
                error_kind: None,
            };
        }
        if !client {
            calls.lock().unwrap().feed(false, &frame.wire);
            // Actual delivered data, rather than the handshake, drives diagnosis.
            if frame.opcode < 8 {
                calls
                    .lock()
                    .unwrap()
                    .annotate_pending("wrote_downstream", "true");
            }
            changed.notify_one();
        }
        if !calls.lock().unwrap().is_observing() {
            return Exit {
                stage: "model_observation_unavailable",
                client,
                code: 1009,
                peer_close: false,
                frame_bytes: frame.wire.len(),
                error_kind: None,
            };
        }
    }
}
async fn watchdog(calls: &Mutex<WsCalls>, changed: &Notify, settings: &WebSocketTimeouts) -> Exit {
    loop {
        let deadline = calls.lock().unwrap().deadline(settings);
        if let Some((at, stage)) = deadline {
            tokio::select! {
                _=changed.notified()=>continue,
                _=tokio::time::sleep_until(at)=>{
                    if calls.lock().unwrap().deadline(settings).is_some_and(|(current,kind)| current<=Instant::now() && kind==stage) {
                        return Exit{stage,client:stage=="websocket_first_message_timeout"||stage=="websocket_inter_turn_idle_timeout",code:if stage=="websocket_inter_turn_idle_timeout" {1000} else {1001},peer_close:false,frame_bytes:0,error_kind:Some(io::ErrorKind::TimedOut)};
                    }
                }
            }
        } else {
            changed.notified().await;
        }
    }
}
fn close_frame(code: u16, reason: &str, masked: bool) -> Vec<u8> {
    let mut payload = Vec::new();
    if code != 1005 {
        payload.extend_from_slice(&code.to_be_bytes());
        payload.extend_from_slice(reason.as_bytes());
    }
    payload.truncate(125);
    let mut wire = vec![0x88, payload.len() as u8 | if masked { 128 } else { 0 }];
    let id = uuid::Uuid::new_v4();
    let mask = &id.as_bytes()[..4];
    if masked {
        wire.extend_from_slice(mask);
    }
    wire.extend(
        payload
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ if masked { mask[i % 4] } else { 0 }),
    );
    wire
}
async fn send_close<W: AsyncWrite + Unpin>(
    writer: &mut W,
    code: u16,
    reason: &str,
    masked: bool,
    timeout: Duration,
) -> bool {
    tokio::time::timeout(timeout, async {
        writer.write_all(&close_frame(code, reason, masked)).await?;
        writer.flush().await
    })
    .await
    .is_ok_and(|r: io::Result<()>| r.is_ok())
}

pub(super) async fn run<A, B>(
    downstream: A,
    upstream: B,
    log: &mut RequestLog,
    settings: &WebSocketTimeouts,
) where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let extensions = log
        .fields
        .get("websocket_extensions")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let calls = Arc::new(Mutex::new(WsCalls::new(log, extensions)));
    let changed = Notify::new();
    let trace = Mutex::new(Trace::default());
    let timeout = Duration::from_secs_f64(settings.write_seconds);
    let (mut cr, mut cw) = tokio::io::split(downstream);
    let (mut ur, mut uw) = tokio::io::split(upstream);
    let exit = tokio::select! {
        result=pump(&mut cr,&mut uw,true,&calls,&changed,&trace,timeout)=>result,
        result=pump(&mut ur,&mut cw,false,&calls,&changed,&trace,timeout)=>result,
        result=watchdog(&calls,&changed,settings)=>result,
    };
    let trace = trace.into_inner().unwrap();
    let normal = exit.peer_close && matches!(exit.code, 1000 | 1001 | 1005)
        || exit.stage == "websocket_inter_turn_idle_timeout";
    log.outcome = if normal {
        "request_finished"
    } else {
        "request_failed"
    };
    log.bytes = trace.received as usize;
    log.field("sent_bytes", trace.sent);
    log.field("client_frames", trace.client_frames);
    log.field("upstream_frames", trace.upstream_frames);
    log.field("reason", exit.stage);
    log.field(
        "error_side",
        if exit.client {
            "downstream"
        } else {
            "upstream"
        },
    );
    log.field("websocket_close_code", exit.code);
    log.field("frame_bytes", exit.frame_bytes);
    log.field("wrote_downstream", trace.received > 0);
    if let Some(kind) = exit.error_kind {
        log.field("transport_error_kind", format!("{kind:?}"));
    }
    {
        let mut calls = calls.lock().unwrap();
        calls.annotate_pending("failure_stage", exit.stage);
        calls.annotate_pending(
            "error_side",
            if exit.client {
                "downstream"
            } else {
                "upstream"
            },
        );
        calls.annotate_pending("websocket_close_code", &exit.code.to_string());
        calls.finish(
            if exit.client
                && (exit.peer_close || matches!(exit.stage, "read_client" | "write_client"))
            {
                "cancelled"
            } else {
                "failed"
            },
            exit.stage,
        );
    }
    // Never inject a close into a partially written frame. The opposite side can
    // still receive a valid close; a blocked/broken socket is dropped promptly.
    let close_timeout = timeout.min(Duration::from_secs(5));
    let client_close = async {
        if !trace.client_write_partial && !(exit.peer_close && !exit.client) {
            send_close(&mut cw, exit.code, exit.stage, false, close_timeout).await
        } else {
            false
        }
    };
    let upstream_close = async {
        if !trace.upstream_write_partial && !(exit.peer_close && exit.client) {
            send_close(&mut uw, exit.code, exit.stage, true, close_timeout).await
        } else {
            false
        }
    };
    let (client_sent, upstream_sent) = tokio::join!(client_close, upstream_close);
    log.field(
        "close_sent_downstream",
        client_sent || exit.peer_close && !exit.client,
    );
    log.field(
        "close_sent_upstream",
        upstream_sent || exit.peer_close && exit.client,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logger::Logger;
    use serde_json::{Value, json};
    use tokio::io::DuplexStream;
    fn data_frame(value: Value, masked: bool) -> Vec<u8> {
        let payload = value.to_string().into_bytes();
        let mut result = vec![0x81];
        let flag = if masked { 128 } else { 0 };
        if payload.len() < 126 {
            result.push(payload.len() as u8 | flag);
        } else {
            result.push(126 | flag);
            result.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        let mask = [1, 2, 3, 4];
        if masked {
            result.extend_from_slice(&mask);
        }
        result.extend(
            payload
                .iter()
                .enumerate()
                .map(|(i, b)| b ^ if masked { mask[i % 4] } else { 0 }),
        );
        result
    }
    fn log(path: std::path::PathBuf) -> RequestLog {
        RequestLog {logger:Arc::new(Logger::new(path)),fields:serde_json::from_value(json!({"request_id":"ws-connection","path":"/v1/responses","method":"GET","provider":"test","status":"101"})).unwrap(),started:std::time::Instant::now(),status:101,bytes:0,outcome:"request_cancelled"}
    }
    fn rows(path: &std::path::Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
    fn start(
        settings: WebSocketTimeouts,
        capacity: usize,
    ) -> (
        tempfile::TempDir,
        DuplexStream,
        DuplexStream,
        tokio::task::JoinHandle<MapResult>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut log = log(dir.path().join("proxy.log"));
        let (client, down) = tokio::io::duplex(capacity);
        let (up, server) = tokio::io::duplex(capacity);
        let task = tokio::spawn(async move {
            run(down, up, &mut log, &settings).await;
            MapResult(log.fields.clone(), log.outcome.to_owned())
        });
        (dir, client, server, task)
    }
    struct MapResult(serde_json::Map<String, Value>, String);
    async fn create(client: &mut DuplexStream, server: &mut DuplexStream) {
        let wire = data_frame(json!({"type":"response.create","model":"test"}), true);
        client.write_all(&wire).await.unwrap();
        assert_eq!(read_frame(server, true).await.unwrap().wire, wire);
    }
    async fn output(server: &mut DuplexStream, client: &mut DuplexStream, value: Value) {
        let wire = data_frame(value, false);
        server.write_all(&wire).await.unwrap();
        assert_eq!(read_frame(client, false).await.unwrap().wire, wire);
    }
    #[tokio::test(start_paused = true)]
    async fn thinking_survives_old_300_second_idle_then_first_output_times_out_despite_heartbeats()
    {
        let (dir, mut client, mut server, task) = start(WebSocketTimeouts::default(), 4096);
        create(&mut client, &mut server).await;
        for _ in 0..4 {
            tokio::time::advance(Duration::from_secs(100)).await;
            output(
                &mut server,
                &mut client,
                json!({"type":"response.in_progress","response":{"id":"r"}}),
            )
            .await;
            assert!(!task.is_finished());
        }
        tokio::time::advance(Duration::from_secs(501)).await;
        let close = read_frame(&mut client, false).await.unwrap();
        assert_eq!(close.close_code, Some(1001));
        let result = task.await.unwrap();
        assert_eq!(result.0["reason"], "websocket_first_output_timeout");
        let records = rows(&dir.path().join("proxy.log"));
        assert_eq!(
            records
                .iter()
                .filter(|r| r["event"] == "model_call_failed")
                .count(),
            1
        );
    }
    #[tokio::test(start_paused = true)]
    async fn semantic_output_arms_read_timeout_and_client_ping_cannot_extend_it() {
        let settings = WebSocketTimeouts {
            read_seconds: 10.0,
            ..Default::default()
        };
        let (_dir, mut client, mut server, task) = start(settings, 4096);
        create(&mut client, &mut server).await;
        output(
            &mut server,
            &mut client,
            json!({"type":"response.output_text.delta","response_id":"r","delta":"x"}),
        )
        .await;
        tokio::time::advance(Duration::from_secs(6)).await;
        output(
            &mut server,
            &mut client,
            json!({"type":"response.output_text.delta","response_id":"r","delta":"y"}),
        )
        .await;
        tokio::time::advance(Duration::from_secs(6)).await;
        assert!(!task.is_finished());
        client.write_all(&[0x89, 0x80, 1, 2, 3, 4]).await.unwrap();
        assert_eq!(read_frame(&mut server, true).await.unwrap().opcode, 9);
        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(
            read_frame(&mut client, false).await.unwrap().close_code,
            Some(1001)
        );
        assert_eq!(task.await.unwrap().0["reason"], "websocket_read_timeout");
    }
    #[tokio::test(start_paused = true)]
    async fn two_turns_complete_before_idle_closes_normally_and_keepalives_do_not_reset_idle() {
        let settings = WebSocketTimeouts {
            inter_turn_idle_seconds: 10.0,
            ..Default::default()
        };
        let (dir, mut client, mut server, task) = start(settings, 4096);
        for id in ["one", "two"] {
            create(&mut client, &mut server).await;
            output(&mut server,&mut client,json!({"type":"response.completed","response":{"id":id,"usage":{"input_tokens":5,"output_tokens":3}}})).await;
        }
        let records = rows(&dir.path().join("proxy.log"));
        assert_eq!(
            records
                .iter()
                .filter(|r| r["event"] == "model_call_finished")
                .count(),
            2
        );
        tokio::time::advance(Duration::from_secs(6)).await;
        client.write_all(&[0x89, 0x80, 1, 2, 3, 4]).await.unwrap();
        read_frame(&mut server, true).await.unwrap();
        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(
            read_frame(&mut client, false).await.unwrap().close_code,
            Some(1000)
        );
        let result = task.await.unwrap();
        assert_eq!(result.1, "request_finished");
        assert_eq!(result.0["reason"], "websocket_inter_turn_idle_timeout");
    }
    #[tokio::test(start_paused = true)]
    async fn normal_upstream_close_does_not_succeed_an_unfinished_call() {
        let (dir, mut client, mut server, task) = start(WebSocketTimeouts::default(), 4096);
        create(&mut client, &mut server).await;
        server
            .write_all(&close_frame(1000, "done", false))
            .await
            .unwrap();
        assert_eq!(
            read_frame(&mut client, false).await.unwrap().close_code,
            Some(1000)
        );
        let result = task.await.unwrap();
        assert_eq!(result.1, "request_finished");
        let records = rows(&dir.path().join("proxy.log"));
        let failed = records
            .iter()
            .find(|r| r["event"] == "model_call_failed")
            .unwrap();
        assert_eq!(failed["model_terminal_event"], "upstream_close");
        assert_eq!(failed["websocket_close_code"], "1000");
    }
    #[tokio::test(start_paused = true)]
    async fn blocked_write_expires_independently_without_injecting_close_into_partial_frame() {
        let settings = WebSocketTimeouts {
            write_seconds: 2.0,
            ..Default::default()
        };
        let (_dir, mut client, mut server, task) = start(settings, 64);
        create(&mut client, &mut server).await;
        let wire = data_frame(
            json!({"type":"response.output_text.delta","response_id":"r","delta":"x".repeat(1024)}),
            false,
        );
        server.write_all(&wire).await.unwrap();
        tokio::time::advance(Duration::from_secs(3)).await;
        // No reads on client: the terminal close must not be injected after the
        // 64-byte prefix of the blocked data frame.
        let result = task.await.unwrap();
        assert_eq!(result.0["reason"], "write_client_timeout");
        assert_eq!(result.0["close_sent_downstream"], "false");
        let mut received = Vec::new();
        client.read_to_end(&mut received).await.unwrap();
        assert_eq!(received, wire[..64]);
    }
    #[tokio::test(start_paused = true)]
    async fn eof_fails_pending_calls_and_empty_close_has_no_wire_status_code() {
        let (dir, mut client, mut server, task) = start(WebSocketTimeouts::default(), 4096);
        create(&mut client, &mut server).await;
        drop(server);
        assert_eq!(
            read_frame(&mut client, false).await.unwrap().close_code,
            Some(1011)
        );
        task.await.unwrap();
        let records = rows(&dir.path().join("proxy.log"));
        assert_eq!(
            records
                .iter()
                .find(|r| r["event"] == "model_call_failed")
                .unwrap()["failure_stage"],
            "read_upstream"
        );
        assert_eq!(close_frame(1005, "ignored", false), [0x88, 0]);
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer.write_all(&[0x88, 0]).await.unwrap();
        assert_eq!(
            read_frame(&mut reader, false).await.unwrap().close_code,
            Some(1005)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn disabling_inter_turn_idle_does_not_disable_active_call_deadlines() {
        let settings = WebSocketTimeouts {
            inter_turn_idle_seconds: 0.0,
            first_output_seconds: 5.0,
            ..Default::default()
        };
        let (_dir, mut client, mut server, task) = start(settings, 4096);
        create(&mut client, &mut server).await;
        output(
            &mut server,
            &mut client,
            json!({"type":"response.completed", "response":{"id":"first"}}),
        )
        .await;
        tokio::time::advance(Duration::from_secs(1000)).await;
        assert!(!task.is_finished());
        create(&mut client, &mut server).await;
        tokio::time::advance(Duration::from_secs(6)).await;
        assert_eq!(
            read_frame(&mut client, false).await.unwrap().close_code,
            Some(1001)
        );
        assert_eq!(
            task.await.unwrap().0["reason"],
            "websocket_first_output_timeout"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn first_message_and_standalone_error_have_distinct_lifecycles() {
        let (dir, mut client, mut server, task) = start(WebSocketTimeouts::default(), 4096);
        create(&mut client, &mut server).await;
        output(
            &mut server,
            &mut client,
            json!({"type":"error","error":{"code":"bad_request"}}),
        )
        .await;
        // The next turn settles a standalone error; an intervening
        // response.failed would instead supply its authoritative usage.
        create(&mut client, &mut server).await;
        assert_eq!(
            rows(&dir.path().join("proxy.log"))
                .iter()
                .filter(|r| r["event"] == "model_call_failed")
                .count(),
            1
        );
        assert!(!task.is_finished());
        client
            .write_all(&close_frame(1000, "done", true))
            .await
            .unwrap();
        task.await.unwrap();
        let (_dir, mut client, _server, task) = start(
            WebSocketTimeouts {
                first_message_seconds: 5.0,
                ..Default::default()
            },
            4096,
        );
        tokio::time::advance(Duration::from_secs(6)).await;
        assert_eq!(
            read_frame(&mut client, false).await.unwrap().close_code,
            Some(1001)
        );
        assert_eq!(
            task.await.unwrap().0["reason"],
            "websocket_first_message_timeout"
        );
    }
}
