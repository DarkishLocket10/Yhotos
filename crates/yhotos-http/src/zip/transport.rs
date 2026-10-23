//! Bounded recycled copy buffers flow from the producer to HTTP with no duplex copies.
use super::writer::{COPY_BYTES, Output};
use axum::body::Bytes;
use futures_core::Stream;
use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{Mutex, OnceLock},
    task::{Context, Poll},
};
use tokio::sync::{mpsc, oneshot};

// Retain at most 24 MiB across archives. Only capacity is reused: Input sets the
// initialized length afresh, and no buffer is reclaimed while Bytes references exist.
const POOL_BUFFERS: usize = 3;
static BUFFERS: OnceLock<Mutex<Vec<Vec<u8>>>> = OnceLock::new();
fn take_buffer() -> Vec<u8> {
    BUFFERS
        .get_or_init(Mutex::default)
        .lock()
        .ok()
        .and_then(|mut buffers| buffers.pop())
        .unwrap_or_else(|| Vec::with_capacity(COPY_BYTES))
}
fn recycle_buffer(mut buffer: Vec<u8>) {
    if buffer.capacity() != COPY_BYTES {
        return;
    }
    buffer.clear();
    if let Ok(mut buffers) = BUFFERS.get_or_init(Mutex::default).lock()
        && buffers.len() < POOL_BUFFERS
    {
        buffers.push(buffer);
    }
}

pub(super) struct Destination {
    sender: mpsc::Sender<Bytes>,
    recycle: mpsc::Sender<Vec<u8>>,
    free: mpsc::Receiver<Vec<u8>>,
}
pub(super) struct ArchiveStream {
    receiver: mpsc::Receiver<Bytes>,
    result: oneshot::Receiver<bool>,
}
pub(super) fn channel() -> (Destination, ArchiveStream, oneshot::Sender<bool>) {
    let (sender, receiver) = mpsc::channel(1);
    let (recycle, free) = mpsc::channel(2);
    let (finished, result) = oneshot::channel();
    (
        Destination {
            sender,
            recycle,
            free,
        },
        ArchiveStream { receiver, result },
        finished,
    )
}
fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "archive stream closed")
}
struct Chunk {
    buffer: Vec<u8>,
    len: usize,
    recycle: mpsc::Sender<Vec<u8>>,
}
impl AsRef<[u8]> for Chunk {
    fn as_ref(&self) -> &[u8] {
        self.buffer.get(..self.len).unwrap_or_default()
    }
}
impl Drop for Chunk {
    fn drop(&mut self) {
        if let Err(error) = self.recycle.try_send(std::mem::take(&mut self.buffer)) {
            recycle_buffer(error.into_inner());
        }
    }
}
impl Drop for Destination {
    fn drop(&mut self) {
        self.free.close();
        while let Ok(buffer) = self.free.try_recv() {
            recycle_buffer(buffer);
        }
    }
}
impl Output for Destination {
    fn buffer(&mut self) -> Vec<u8> {
        self.free.try_recv().unwrap_or_else(|_| take_buffer())
    }
    fn recycle(&mut self, buffer: Vec<u8>) {
        if let Err(error) = self.recycle.try_send(buffer) {
            recycle_buffer(error.into_inner());
        }
    }
    async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.sender
            .send(Bytes::copy_from_slice(bytes))
            .await
            .map_err(|_| closed())
    }
    async fn chunk(&mut self, buffer: Vec<u8>, len: usize) -> io::Result<Vec<u8>> {
        if len > buffer.len() {
            return Err(io::Error::other("archive input mismatch"));
        }
        let chunk = Bytes::from_owner(Chunk {
            buffer,
            len,
            recycle: self.recycle.clone(),
        });
        self.sender.send(chunk).await.map_err(|_| closed())?;
        // Reuse only after Hyper drops its last Bytes reference. A consumer is
        // allowed to retain frames (e.g. BodyExt::collect); never await recycling.
        Ok(self.buffer())
    }
    fn flush(&mut self) -> impl Future<Output = io::Result<()>> + Send {
        std::future::ready(Ok(()))
    }
}
// EOF is successful only after the central directory was written. IO errors,
// deadlines and producer panics/cancellation must abort a truncated HTTP body.
impl Stream for ArchiveStream {
    type Item = io::Result<Bytes>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.receiver.poll_recv(cx) {
            Poll::Ready(None) => match Pin::new(&mut self.result).poll(cx) {
                Poll::Ready(Ok(true)) => Poll::Ready(None),
                Poll::Ready(_) => Poll::Ready(Some(Err(io::Error::other("archive incomplete")))),
                Poll::Pending => Poll::Pending,
            },
            next => next.map(|item| item.map(Ok)),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::future::poll_fn;

    #[tokio::test]
    async fn chunk_is_transferred_without_copy_and_recycled_after_last_reference() {
        let (mut output, mut stream, _finished) = channel();
        let buffer = vec![7; COPY_BYTES];
        let address = buffer.as_ptr();
        let mut spare = output.chunk(buffer, 128).await.unwrap();
        let chunk = poll_fn(|cx| Pin::new(&mut stream).poll_next(cx))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(chunk.as_ptr(), address);
        spare.resize(COPY_BYTES, 9);
        assert!(chunk.iter().all(|b| *b == 7));
        let retained = chunk.clone();
        drop(chunk);
        assert!(output.free.try_recv().is_err());
        drop(retained);
        let reused = output.chunk(spare, 128).await.unwrap();
        assert_eq!(reused.as_ptr(), address);
    }

    #[tokio::test(start_paused = true)]
    async fn queue_backpressure_and_disconnect_bound_the_producer() {
        let (mut output, stream, _finished) = channel();
        output.write(b"first").await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), output.write(b"second"))
                .await
                .is_err()
        );
        drop(stream);
        assert_eq!(
            output.write(b"disconnected").await.unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[tokio::test]
    async fn cancelled_producer_aborts_the_body_instead_of_completing_eof() {
        let (output, mut stream, finished) = channel();
        drop((output, finished));
        assert!(
            poll_fn(|cx| Pin::new(&mut stream).poll_next(cx))
                .await
                .unwrap()
                .is_err()
        );
    }
}
