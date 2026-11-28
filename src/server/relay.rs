//! Transparent tunnels expire only when neither direction makes progress.
use std::{
    io,
    pin::Pin,
    sync::Mutex,
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::Instant;

pub(super) struct Progress {
    pub sent: u64,
    pub received: u64,
    last_activity: Instant,
    pub failure: Option<(&'static str, &'static str, io::ErrorKind)>,
}
impl Progress {
    pub fn new() -> Self {
        Self {
            sent: 0,
            received: 0,
            last_activity: Instant::now(),
            failure: None,
        }
    }
}
struct Observed<'a, T> {
    io: T,
    progress: &'a Mutex<Progress>,
    side: &'static str,
}
impl<T> Observed<'_, T> {
    fn error(&self, operation: &'static str, error: &io::Error) {
        self.progress.lock().unwrap().failure = Some((self.side, operation, error.kind()));
    }
}
impl<T: AsyncRead + Unpin> AsyncRead for Observed<'_, T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.io).poll_read(cx, buf);
        match &result {
            Poll::Ready(Ok(())) if buf.filled().len() > before => {
                self.progress.lock().unwrap().last_activity = Instant::now()
            }
            Poll::Ready(Err(error)) => self.error("read", error),
            _ => {}
        }
        result
    }
}
impl<T: AsyncWrite + Unpin> AsyncWrite for Observed<'_, T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.io).poll_write(cx, buf);
        match &result {
            Poll::Ready(Ok(n)) if *n > 0 => {
                let mut p = self.progress.lock().unwrap();
                p.last_activity = Instant::now();
                if self.side == "upstream" {
                    p.sent += *n as u64;
                } else {
                    p.received += *n as u64;
                }
            }
            Poll::Ready(Err(error)) => self.error("write", error),
            _ => {}
        }
        result
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.io).poll_flush(cx);
        if let Poll::Ready(Err(error)) = &result {
            self.error("flush", error);
        }
        result
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.io).poll_shutdown(cx);
        if let Poll::Ready(Err(error)) = &result {
            self.error("shutdown", error);
        }
        result
    }
}
pub(super) async fn copy_idle<A, B>(
    downstream: A,
    upstream: B,
    idle: Duration,
    progress: &Mutex<Progress>,
) -> io::Result<()>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let mut downstream = Observed {
        io: downstream,
        progress,
        side: "downstream",
    };
    let mut upstream = Observed {
        io: upstream,
        progress,
        side: "upstream",
    };
    let copy = tokio::io::copy_bidirectional(&mut downstream, &mut upstream);
    tokio::pin!(copy);
    loop {
        let deadline = progress.lock().unwrap().last_activity + idle;
        tokio::select! {
            result = &mut copy => return result.map(|_| ()),
            _ = tokio::time::sleep_until(deadline) => {
                if Instant::now() >= progress.lock().unwrap().last_activity + idle {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "Tunnel idle timeout"));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test(start_paused = true)]
    async fn one_way_activity_survives_multiple_idle_periods_then_expires() {
        for from_client in [true, false] {
            let (mut client, downstream) = tokio::io::duplex(64);
            let (upstream, mut server) = tokio::io::duplex(64);
            let progress = std::sync::Arc::new(Mutex::new(Progress::new()));
            let shared = progress.clone();
            let task = tokio::spawn(async move {
                copy_idle(downstream, upstream, Duration::from_secs(10), &shared).await
            });
            for _ in 0..6 {
                tokio::time::advance(Duration::from_secs(4)).await;
                if from_client {
                    client.write_all(b"hello").await.unwrap();
                    let mut bytes = [0; 5];
                    server.read_exact(&mut bytes).await.unwrap();
                    assert_eq!(&bytes, b"hello");
                } else {
                    server.write_all(b"hello").await.unwrap();
                    let mut bytes = [0; 5];
                    client.read_exact(&mut bytes).await.unwrap();
                    assert_eq!(&bytes, b"hello");
                }
                assert!(!task.is_finished());
            }
            tokio::time::advance(Duration::from_secs(11)).await;
            assert_eq!(
                task.await.unwrap().unwrap_err().kind(),
                io::ErrorKind::TimedOut
            );
            let p = progress.lock().unwrap();
            assert_eq!(
                (p.sent, p.received),
                if from_client { (30, 0) } else { (0, 30) }
            );
            assert!(p.failure.is_none());
        }
    }

    #[tokio::test]
    async fn half_close_preserves_response_and_byte_counts() {
        let (mut client, downstream) = tokio::io::duplex(64);
        let (upstream, mut server) = tokio::io::duplex(64);
        let p = Mutex::new(Progress::new());
        let transfer = copy_idle(downstream, upstream, Duration::from_secs(5), &p);
        let peers = async {
            client.write_all(b"hello").await.unwrap();
            client.shutdown().await.unwrap();
            let mut input = Vec::new();
            server.read_to_end(&mut input).await.unwrap();
            assert_eq!(input, b"hello");
            server.write_all(b"reply").await.unwrap();
            server.shutdown().await.unwrap();
            let mut output = Vec::new();
            client.read_to_end(&mut output).await.unwrap();
            assert_eq!(output, b"reply");
        };
        let (result, ()) = tokio::join!(transfer, peers);
        result.unwrap();
        let p = p.into_inner().unwrap();
        assert_eq!((p.sent, p.received), (5, 5));
    }

    #[tokio::test]
    async fn broken_peer_preserves_prior_bytes_and_error_side() {
        let (mut client, downstream) = tokio::io::duplex(64);
        let (upstream, mut server) = tokio::io::duplex(64);
        let p = Mutex::new(Progress::new());
        let transfer = copy_idle(downstream, upstream, Duration::from_secs(5), &p);
        let peers = async {
            client.write_all(b"first").await.unwrap();
            let mut bytes = [0; 5];
            server.read_exact(&mut bytes).await.unwrap();
            drop(server);
            client.write_all(b"second").await.unwrap();
            client
        };
        let (result, _client) = tokio::join!(transfer, peers);
        assert!(result.is_err());
        let p = p.into_inner().unwrap();
        assert_eq!(p.sent, 5);
        assert_eq!(
            p.failure,
            Some(("upstream", "write", io::ErrorKind::BrokenPipe))
        );
    }
}
