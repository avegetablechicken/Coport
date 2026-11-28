//! Request framing rules, held for every request on a connection.
//!
//! Hyper normalizes framing it is lenient about: it drops a Content-Length
//! that a Transfer-Encoding overrides, so the conflict is gone from what it
//! parses. The rules are therefore applied to the raw head. The first head is
//! read whole before Hyper starts; later ones are seen by a [`Tracker`] as
//! Hyper reads them, which follows each body to find where the next head
//! begins. It never alters the bytes: it only records each request's verdict
//! for the request handler to take, in the order Hyper hands requests over.

use crate::{Error, Result};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// The largest request head, including its terminating blank line.
pub(super) const MAX_HEAD: usize = 65536;
const MAX_BODY: u64 = 32 * 1024 * 1024;
/// The longest chunk-size or trailer line followed.
const MAX_LINE: usize = 4096;

/// How a request's body ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Body {
    Length(u64),
    Chunked,
}

/// Checks a request head, without its terminating blank line.
pub(super) fn check_head(head: &[u8]) -> Result<Body> {
    let head =
        std::str::from_utf8(head).map_err(|_| Error::new(400, "Invalid request headers."))?;
    let mut names = std::collections::HashSet::new();
    let mut length = 0;
    for line in head.split("\r\n").skip(1) {
        let (name, value) = line
            .split_once(':')
            .ok_or(Error::new(400, "Invalid request header."))?;
        let name = name.to_ascii_lowercase();
        if !names.insert(name.clone()) && !super::repeatable_request_header(&name) {
            return Err(Error::new(400, "Duplicate request header."));
        }
        if name == "transfer-encoding" && !value.trim().eq_ignore_ascii_case("chunked") {
            return Err(Error::new(400, "Unsupported body framing."));
        }
        if name == "content-length" {
            let v = value.trim();
            if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
                return Err(Error::new(400, "Invalid Content-Length."));
            }
            length = v
                .parse::<u64>()
                .ok()
                .filter(|n| *n <= MAX_BODY)
                .ok_or(Error::new(413, "Body exceeds 32 MiB."))?;
        }
    }
    if names.contains("transfer-encoding") && names.contains("content-length") {
        return Err(Error::new(400, "Ambiguous body framing."));
    }
    if names.contains("expect") {
        return Err(Error::new(417, "Expect is unsupported."));
    }
    Ok(if names.contains("transfer-encoding") {
        Body::Chunked
    } else {
        Body::Length(length)
    })
}

/// Each request's verdict, in the order Hyper hands the requests over.
#[derive(Clone, Default)]
pub(super) struct Verdicts(Arc<Mutex<VecDeque<Result<()>>>>);

impl Verdicts {
    /// The next request's verdict. A request the tracker lost sight of
    /// fails closed.
    pub(super) fn pop(&self) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Err(Error::new(400, "Invalid request framing.")))
    }

    fn push(&self, verdict: Result<()>) {
        self.0.lock().unwrap().push_back(verdict);
    }
}

enum State {
    Head(Vec<u8>),
    /// Content-Length bytes left.
    Body(u64),
    ChunkSize(Vec<u8>),
    /// Chunk bytes left, with the CRLF after them.
    ChunkData(u64),
    Trailer(Vec<u8>),
    /// Nothing more to follow: framing was rejected (Hyper closes the
    /// connection after its response), or the bytes are not HTTP Hyper would
    /// accept, such as an upgraded stream.
    Done,
}

/// Follows the requests in the bytes Hyper reads from a connection.
pub(super) struct Tracker {
    state: State,
    verdicts: Verdicts,
}

impl Tracker {
    pub(super) fn new(verdicts: Verdicts) -> Self {
        Self {
            state: State::Head(Vec::new()),
            verdicts,
        }
    }

    /// Bytes read from the connection, in order.
    pub(super) fn observe(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            self.state = match std::mem::replace(&mut self.state, State::Done) {
                State::Done => return,
                State::Head(head) => self.head(head, &mut bytes),
                State::Body(left) => match skip(left, &mut bytes) {
                    0 => State::Head(Vec::new()),
                    left => State::Body(left),
                },
                State::ChunkData(left) => match skip(left, &mut bytes) {
                    0 => State::ChunkSize(Vec::new()),
                    left => State::ChunkData(left),
                },
                State::ChunkSize(mut line) => match take_line(&mut line, &mut bytes) {
                    None if line.len() > MAX_LINE => State::Done,
                    None => State::ChunkSize(line),
                    Some(()) => match std::str::from_utf8(&line).ok().and_then(chunk_size) {
                        Some(0) => State::Trailer(Vec::new()),
                        Some(n) => State::ChunkData(n.saturating_add(2)),
                        None => State::Done,
                    },
                },
                State::Trailer(mut line) => match take_line(&mut line, &mut bytes) {
                    None if line.len() > MAX_LINE => State::Done,
                    None => State::Trailer(line),
                    // A blank line ends the trailer section, and the body.
                    Some(()) if line.is_empty() || line == b"\r" => State::Head(Vec::new()),
                    Some(()) => State::Trailer(Vec::new()),
                },
            };
        }
    }

    fn head(&mut self, mut head: Vec<u8>, bytes: &mut &[u8]) -> State {
        if head.is_empty() {
            // Hyper skips blank lines before a request line.
            let blank = bytes
                .iter()
                .take_while(|b| matches!(**b, b'\r' | b'\n'))
                .count();
            *bytes = &bytes[blank..];
        }
        let old = head.len();
        let take = bytes.len().min(MAX_HEAD + 1 - old);
        head.extend_from_slice(&bytes[..take]);
        let from = old.saturating_sub(3);
        let Some(end) = head[from..]
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|p| from + p)
        else {
            *bytes = &bytes[take..];
            if head.len() > MAX_HEAD {
                self.verdicts
                    .push(Err(Error::new(431, "Headers exceed 64 KiB.")));
                return State::Done;
            }
            return State::Head(head);
        };
        *bytes = &bytes[end + 4 - old..];
        let verdict = if end + 4 > MAX_HEAD {
            Err(Error::new(431, "Headers exceed 64 KiB."))
        } else {
            check_head(&head[..end])
        };
        match verdict {
            Ok(body) => {
                self.verdicts.push(Ok(()));
                match body {
                    Body::Length(0) => State::Head(Vec::new()),
                    Body::Length(n) => State::Body(n),
                    Body::Chunked => State::ChunkSize(Vec::new()),
                }
            }
            Err(error) => {
                self.verdicts.push(Err(error));
                State::Done
            }
        }
    }
}

/// A chunk-size line's size, ignoring chunk extensions.
fn chunk_size(line: &str) -> Option<u64> {
    let size = line.trim_end_matches('\r').split(';').next()?;
    u64::from_str_radix(size.trim(), 16).ok()
}

/// Skips up to `left` bytes, returning how many are still to come.
fn skip(left: u64, bytes: &mut &[u8]) -> u64 {
    let n = left.min(bytes.len() as u64);
    *bytes = &bytes[n as usize..];
    left - n
}

/// Moves bytes into `line` up to its LF, which is consumed but not kept.
/// `Some` once the line is complete.
fn take_line(line: &mut Vec<u8>, bytes: &mut &[u8]) -> Option<()> {
    match bytes.iter().position(|b| *b == b'\n') {
        Some(p) => {
            line.extend_from_slice(&bytes[..p]);
            *bytes = &bytes[p + 1..];
            Some(())
        }
        None => {
            line.extend_from_slice(bytes);
            *bytes = &[];
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdicts(input: &[u8], step: usize) -> Vec<std::result::Result<(), &'static str>> {
        let verdicts = Verdicts::default();
        let mut tracker = Tracker::new(verdicts.clone());
        for chunk in input.chunks(step) {
            tracker.observe(chunk);
        }
        let out = verdicts.0.lock().unwrap().drain(..).collect::<Vec<_>>();
        out.into_iter().map(|v| v.map_err(|e| e.message)).collect()
    }

    #[test]
    fn follows_bodies_to_each_head() {
        // Bodies hold what would be a conflicting head if read as one.
        let trap = "POST / HTTP/1.1\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n";
        let input = format!(
            "POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n{trap}\
             \r\nPOST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n{:x};ext=1\r\n{trap}\r\n0\r\nX-Trailer: 1\r\n\r\n\
             GET / HTTP/1.1\r\nHost: a\r\n\r\n\
             POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\nContent-Length: 1\r\n\r\n0\r\n\r\n\
             GET / HTTP/1.1\r\n\r\n",
            trap.len(),
            trap.len(),
        );
        for step in [1, 2, 3, 7, 64, input.len()] {
            assert_eq!(
                verdicts(input.as_bytes(), step),
                [Ok(()), Ok(()), Ok(()), Err("Ambiguous body framing.")],
                "step {step}"
            );
        }
    }

    #[test]
    fn unfollowable_bytes_fail_closed() {
        let verdicts = Verdicts::default();
        let mut tracker = Tracker::new(verdicts.clone());
        tracker.observe(b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n");
        tracker.observe(b"GET / HTTP/1.1\r\n\r\n");
        assert!(verdicts.pop().is_ok());
        assert_eq!(
            verdicts.pop().unwrap_err().message,
            "Invalid request framing."
        );
    }

    #[test]
    fn oversized_head_is_rejected() {
        let big = "a".repeat(MAX_HEAD);
        let input = format!("GET / HTTP/1.1\r\nX-Big: {big}\r\n\r\n");
        assert_eq!(
            verdicts(input.as_bytes(), 4096),
            [Err::<(), _>("Headers exceed 64 KiB.")]
        );
    }
}
