use crate::error::HttpError;
use axum::body::Bytes;
use futures_core::Stream;
use std::{
    collections::HashMap,
    future::Future,
    net::IpAddr,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::AsyncRead,
    sync::{OwnedSemaphorePermit, Semaphore},
    time::{Instant, Sleep, sleep},
};
use tokio_util::io::ReaderStream;

const MAX_ACTIVE_IPS: usize = 16_384;

pub(crate) struct Streams {
    active: Mutex<HashMap<IpAddr, usize>>,
    cap: usize,
    global: Arc<Semaphore>,
}
impl Streams {
    pub(crate) fn new(cap: usize, global_cap: u16) -> Arc<Self> {
        Arc::new(Self {
            active: Mutex::new(HashMap::new()),
            cap,
            global: Arc::new(Semaphore::new(usize::from(global_cap))),
        })
    }
    pub(crate) fn acquire(self: &Arc<Self>, ip: IpAddr) -> Result<Permit, HttpError> {
        let global = self
            .global
            .clone()
            .try_acquire_owned()
            .map_err(|_| HttpError::RateLimited)?;
        let mut active = self
            .active
            .lock()
            .map_err(|_| HttpError::Internal(anyhow::anyhow!("stream accounting unavailable")))?;
        if active.get(&ip).copied().unwrap_or(0) >= self.cap
            || (!active.contains_key(&ip) && active.len() >= MAX_ACTIVE_IPS)
        {
            return Err(HttpError::RateLimited);
        }
        *active.entry(ip).or_default() += 1;
        Ok(Permit {
            streams: self.clone(),
            ip,
            _global: global,
        })
    }
}
pub(crate) struct Permit {
    streams: Arc<Streams>,
    ip: IpAddr,
    _global: OwnedSemaphorePermit,
}
impl Drop for Permit {
    fn drop(&mut self) {
        if let Ok(mut active) = self.streams.active.lock()
            && let Some(count) = active.get_mut(&self.ip)
        {
            *count -= 1;
            if *count == 0 {
                active.remove(&self.ip);
            }
        }
    }
}
// The response body owns both permits through this stream, including while it
// is unpolled. EOF/errors release them immediately; disconnect/drop uses RAII.
// The fixed deadline is checked before every read and wakes pending reads.
// When socket backpressure stops body polls, the transport write timeout drops
// the body instead; a body timer alone cannot reclaim that connection.
pub(crate) struct GuardedStream<R> {
    stream: ReaderStream<R>,
    permit: Option<Permit>,
    deadline: Pin<Box<Sleep>>,
}
impl<R> GuardedStream<R> {
    pub(crate) fn new(stream: ReaderStream<R>, permit: Permit, lifetime: Duration) -> Self {
        Self {
            stream,
            permit: Some(permit),
            deadline: Box::pin(sleep(lifetime)),
        }
    }
}
impl<R: AsyncRead + Unpin> Stream for GuardedStream<R> {
    type Item = std::io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.permit.is_none() {
            return Poll::Ready(None);
        }
        // Tokio rounds timer wakes to its clock resolution. Compare the actual
        // deadline too, so a ready reader cannot emit bytes after that instant.
        if Instant::now() >= self.deadline.deadline() || self.deadline.as_mut().poll(cx).is_ready()
        {
            self.permit.take();
            return Poll::Ready(Some(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "transfer deadline reached",
            ))));
        }
        let next = Pin::new(&mut self.stream).poll_next(cx);
        if matches!(next, Poll::Ready(None | Some(Err(_)))) {
            self.permit.take();
        }
        next
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::future::poll_fn;
    use tokio::io::ReadBuf;

    #[test]
    fn rejected_per_ip_acquisition_does_not_leak_a_global_permit() {
        let streams = Streams::new(1, 2);
        let ip = "127.0.0.1".parse().unwrap();
        let first = streams.acquire(ip).unwrap();
        assert!(matches!(streams.acquire(ip), Err(HttpError::RateLimited)));
        assert_eq!(streams.global.available_permits(), 1);
        let other = streams.acquire("127.0.0.2".parse().unwrap()).unwrap();
        assert_eq!(streams.global.available_permits(), 0);
        drop((first, other));
        assert_eq!(streams.global.available_permits(), 2);
        assert!(streams.active.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn eof_releases_permits_even_if_the_stream_is_retained() {
        let streams = Streams::new(1, 1);
        let ip = "127.0.0.1".parse().unwrap();
        let mut stream = GuardedStream::new(
            ReaderStream::new(b"bytes".as_slice()),
            streams.acquire(ip).unwrap(),
            Duration::from_secs(30),
        );
        assert_eq!(
            poll_fn(|cx| Pin::new(&mut stream).poll_next(cx))
                .await
                .unwrap()
                .unwrap(),
            "bytes"
        );
        assert!(streams.acquire(ip).is_err());
        assert!(
            poll_fn(|cx| Pin::new(&mut stream).poll_next(cx))
                .await
                .is_none()
        );
        let _next = streams.acquire(ip).unwrap();
        assert!(stream.permit.is_none());
    }

    struct BrokenReader;
    impl AsyncRead for BrokenReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Err(std::io::Error::other("test read failure")))
        }
    }

    #[tokio::test]
    async fn read_error_releases_permits_even_if_the_stream_is_retained() {
        let streams = Streams::new(1, 1);
        let ip = "127.0.0.1".parse().unwrap();
        let mut stream = GuardedStream::new(
            ReaderStream::new(BrokenReader),
            streams.acquire(ip).unwrap(),
            Duration::from_secs(30),
        );
        assert!(
            poll_fn(|cx| Pin::new(&mut stream).poll_next(cx))
                .await
                .unwrap()
                .is_err()
        );
        let _next = streams.acquire(ip).unwrap();
        assert!(stream.permit.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn unpolled_body_expires_before_reading_more_bytes_and_releases_permits() {
        let streams = Streams::new(1, 1);
        let ip = "127.0.0.1".parse().unwrap();
        let mut stream = GuardedStream::new(
            ReaderStream::new(b"bytes".as_slice()),
            streams.acquire(ip).unwrap(),
            Duration::from_secs(5),
        );
        tokio::time::advance(Duration::from_secs(5)).await;
        // Time alone does not release an unpolled body; the deadline is enforced
        // on its next poll. Socket stalls are handled at the transport instead.
        assert!(streams.acquire(ip).is_err());
        let error = poll_fn(|cx| Pin::new(&mut stream).poll_next(cx))
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        let _next = streams.acquire(ip).unwrap();
        assert!(
            poll_fn(|cx| Pin::new(&mut stream).poll_next(cx))
                .await
                .is_none()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn progressing_reads_cannot_extend_the_absolute_deadline() {
        let streams = Streams::new(1, 1);
        let ip = "127.0.0.1".parse().unwrap();
        let mut stream = GuardedStream::new(
            ReaderStream::with_capacity(tokio::io::repeat(b'x'), 1),
            streams.acquire(ip).unwrap(),
            Duration::from_secs(5),
        );
        for _ in 0..4 {
            tokio::time::advance(Duration::from_secs(1)).await;
            assert_eq!(
                poll_fn(|cx| Pin::new(&mut stream).poll_next(cx))
                    .await
                    .unwrap()
                    .unwrap(),
                "x"
            );
            assert!(streams.acquire(ip).is_err());
        }
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(
            poll_fn(|cx| Pin::new(&mut stream).poll_next(cx))
                .await
                .unwrap()
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::TimedOut
        );
        let _next = streams.acquire(ip).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn pending_read_is_woken_at_the_deadline_and_releases_permits() {
        let streams = Streams::new(1, 1);
        let ip = "127.0.0.1".parse().unwrap();
        let (reader, _writer) = tokio::io::duplex(1);
        let mut stream = GuardedStream::new(
            ReaderStream::new(reader),
            streams.acquire(ip).unwrap(),
            Duration::from_secs(5),
        );
        let start = tokio::time::Instant::now();
        // Paused Tokio time advances only once this poll is pending. The
        // deadline must register its waker even though the reader never wakes.
        let error = poll_fn(|cx| Pin::new(&mut stream).poll_next(cx))
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(start.elapsed(), Duration::from_secs(5));
        let _next = streams.acquire(ip).unwrap();
    }
}
