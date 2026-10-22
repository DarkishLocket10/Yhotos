//! Fixed-buffer streaming with a per-read idle deadline and no whole-chunk allocation.
use axum::body::{Body, BodyDataStream, Bytes};
use futures_core::Stream;
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::io::StreamReader;
struct Input(BodyDataStream);
impl Stream for Input {
    type Item = Result<Bytes, io::Error>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.0)
            .poll_next(cx)
            .map(|v| v.map(|r| r.map_err(|_| io::Error::other("upload stream failed"))))
    }
}
/// Rewinds uncommitted bytes, streams at most cap bytes and fsyncs before returning an offset.
/// # Errors
/// Returns 413 on overflow, 408 on idle timeout, or generic input/storage failures.
pub async fn write(
    file: &mut tokio::fs::File,
    body: Body,
    offset: u64,
    cap: u64,
) -> Result<u64, axum::http::StatusCode> {
    use axum::http::StatusCode;
    file.set_len(offset)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut input = StreamReader::new(Input(body.into_data_stream()));
    let mut buffer = vec![0; 256 * 1024];
    let mut received = 0;
    loop {
        let n = tokio::time::timeout(Duration::from_secs(30), input.read(&mut buffer))
            .await
            .map_err(|_| StatusCode::REQUEST_TIMEOUT)?
            .map_err(|_| StatusCode::BAD_REQUEST)?;
        if n == 0 {
            break;
        }
        received += n as u64;
        if received > cap {
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }
        file.write_all(buffer.get(..n).ok_or(StatusCode::INTERNAL_SERVER_ERROR)?)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }
    file.flush()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    file.sync_data()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(offset + received)
}
