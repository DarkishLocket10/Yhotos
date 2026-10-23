//! STORE-only ZIP writer. Only the copy buffer and central-directory metadata are retained.
use std::io::{Error, ErrorKind, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
pub(super) const COPY_BYTES: usize = 8 * 1024 * 1024;
/// A bounded archive source that reads and checksums into an owned copy buffer.
pub trait Input {
    /// Returns the buffer, initialized byte count and running CRC, without reading past `len`.
    fn read(
        &mut self,
        buffer: Vec<u8>,
        len: usize,
        crc: crc32fast::Hasher,
    ) -> impl Future<Output = Result<(Vec<u8>, usize, crc32fast::Hasher)>> + Send;
}
impl<R: AsyncRead + Unpin + Send> Input for R {
    async fn read(
        &mut self,
        mut buffer: Vec<u8>,
        len: usize,
        mut crc: crc32fast::Hasher,
    ) -> Result<(Vec<u8>, usize, crc32fast::Hasher)> {
        buffer.resize(len, 0);
        let n = AsyncReadExt::read(self, buffer.get_mut(..len).ok_or_else(invalid)?).await?;
        crc.update(buffer.get(..n).ok_or_else(invalid)?);
        Ok((buffer, n, crc))
    }
}
/// A bounded archive destination that transfers or reuses owned copy buffers.
pub trait Output {
    /// Supplies a private copy buffer, which may contain capacity from an earlier archive.
    fn buffer(&mut self) -> Vec<u8> {
        Vec::new()
    }
    /// Reclaims a buffer after its initialized contents are no longer needed.
    fn recycle(&mut self, _buffer: Vec<u8>) {}
    /// Writes archive metadata in order.
    fn write(&mut self, bytes: &[u8]) -> impl Future<Output = Result<()>> + Send;
    /// Transfers the initialized prefix, returning a reusable buffer.
    fn chunk(
        &mut self,
        buffer: Vec<u8>,
        len: usize,
    ) -> impl Future<Output = Result<Vec<u8>>> + Send;
    /// Flushes all archive metadata.
    fn flush(&mut self) -> impl Future<Output = Result<()>> + Send;
}
impl<W: AsyncWrite + Unpin + Send> Output for W {
    async fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.write_all(bytes).await
    }
    async fn chunk(&mut self, buffer: Vec<u8>, len: usize) -> Result<Vec<u8>> {
        self.write_all(buffer.get(..len).ok_or_else(invalid)?)
            .await?;
        Ok(buffer)
    }
    async fn flush(&mut self) -> Result<()> {
        AsyncWriteExt::flush(self).await
    }
}
struct Entry {
    name: String,
    size: u64,
    offset: u64,
    crc: u32,
}
/// Writes ZIP64-capable archives without seeking or buffering file contents.
pub struct ZipWriter<W> {
    out: W,
    offset: u64,
    entries: Vec<Entry>,
    cap: u64,
    buffer: Vec<u8>,
}
fn invalid() -> Error {
    Error::new(ErrorKind::InvalidData, "archive limit or input mismatch")
}
fn u16le(b: &mut Vec<u8>, n: u16) {
    b.extend(n.to_le_bytes());
}
fn u32le(b: &mut Vec<u8>, n: u32) {
    b.extend(n.to_le_bytes());
}
fn u64le(b: &mut Vec<u8>, n: u64) {
    b.extend(n.to_le_bytes());
}
impl<W: Output> ZipWriter<W> {
    /// A byte ceiling includes headers, descriptors and the central directory.
    pub fn new(mut out: W, cap: u64) -> Self {
        let buffer = out.buffer();
        Self {
            out,
            cap,
            offset: 0,
            entries: Vec::new(),
            buffer,
        }
    }
    async fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let next = self
            .offset
            .checked_add(bytes.len() as u64)
            .ok_or_else(invalid)?;
        if next > self.cap {
            return Err(invalid());
        }
        self.out.write(bytes).await?;
        self.offset = next;
        Ok(())
    }
    /// Streams exactly the declared bytes, rejects unsafe names and detects truncated/growing input.
    /// # Errors
    /// Returns IO failures or a violated size/name/byte limit.
    pub async fn entry(&mut self, name: String, size: u64, mut input: impl Input) -> Result<()> {
        if name.is_empty()
            || name.len() > 255
            || name.contains(['/', '\\'])
            || name.contains("..")
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            || self.entries.len() >= 100_000
        {
            return Err(invalid());
        }
        let offset = self.offset;
        let zip64 = size >= u64::from(u32::MAX);
        let mut b = Vec::with_capacity(320);
        u32le(&mut b, 0x0403_4b50);
        u16le(&mut b, if zip64 { 45 } else { 20 });
        u16le(&mut b, 8);
        u16le(&mut b, 0);
        u16le(&mut b, 0);
        u16le(&mut b, 33);
        u32le(&mut b, 0);
        for _ in 0..2 {
            u32le(&mut b, if zip64 { u32::MAX } else { 0 });
        }
        u16le(&mut b, u16::try_from(name.len()).map_err(|_| invalid())?);
        u16le(&mut b, if zip64 { 20 } else { 0 });
        b.extend(name.as_bytes());
        if zip64 {
            u16le(&mut b, 1);
            u16le(&mut b, 16);
            u64le(&mut b, size);
            u64le(&mut b, size);
        }
        self.write(&b).await?;
        let mut crc = crc32fast::Hasher::new();
        let mut buffer = std::mem::take(&mut self.buffer);
        buffer.reserve(COPY_BYTES.saturating_sub(buffer.len()));
        let mut remaining = size;
        while remaining > 0 {
            let len = usize::try_from(remaining.min(COPY_BYTES as u64)).map_err(|_| invalid())?;
            let (read, n, updated_crc) = input.read(buffer, len, crc).await?;
            buffer = read;
            crc = updated_crc;
            if n == 0 {
                return Err(invalid());
            }
            if n > len || buffer.len() < n {
                return Err(invalid());
            }
            let next = self.offset.checked_add(n as u64).ok_or_else(invalid)?;
            if next > self.cap {
                return Err(invalid());
            }
            buffer = self.out.chunk(buffer, n).await?;
            self.offset = next;
            remaining -= n as u64;
        }
        match input.read(vec![0], 1, crc32fast::Hasher::new()).await {
            Ok((_, 0, _)) => {}
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => {}
            Err(e) => return Err(e),
            Ok(_) => return Err(invalid()),
        }
        self.buffer = buffer;
        let crc = crc.finalize();
        b.clear();
        u32le(&mut b, 0x0807_4b50);
        u32le(&mut b, crc);
        if zip64 {
            u64le(&mut b, size);
            u64le(&mut b, size);
        } else {
            for _ in 0..2 {
                u32le(&mut b, u32::try_from(size).map_err(|_| invalid())?);
            }
        }
        self.write(&b).await?;
        self.entries.push(Entry {
            name,
            size,
            offset,
            crc,
        });
        Ok(())
    }
    /// Completes a valid central directory and flushes the stream.
    /// # Errors
    /// Returns IO failures or an exceeded archive byte ceiling.
    pub async fn finish(mut self) -> Result<()> {
        let start = self.offset;
        let count = self.entries.len() as u64;
        for e in std::mem::take(&mut self.entries) {
            // Central entries always use ZIP64 sizes and offsets, even for small files.
            let mut b = Vec::with_capacity(350);
            u32le(&mut b, 0x0201_4b50);
            u16le(&mut b, 45);
            u16le(&mut b, 45);
            u16le(&mut b, 8);
            u16le(&mut b, 0);
            u16le(&mut b, 0);
            u16le(&mut b, 33);
            u32le(&mut b, e.crc);
            u32le(&mut b, u32::MAX);
            u32le(&mut b, u32::MAX);
            u16le(&mut b, u16::try_from(e.name.len()).map_err(|_| invalid())?);
            u16le(&mut b, 28);
            u16le(&mut b, 0);
            u16le(&mut b, 0);
            u16le(&mut b, 0);
            u32le(&mut b, 0);
            u32le(&mut b, u32::MAX);
            b.extend(e.name.as_bytes());
            u16le(&mut b, 1);
            u16le(&mut b, 24);
            u64le(&mut b, e.size);
            u64le(&mut b, e.size);
            u64le(&mut b, e.offset);
            self.write(&b).await?;
        }
        let length = self.offset - start;
        let end = self.offset;
        let mut b = Vec::new();
        u32le(&mut b, 0x0606_4b50);
        u64le(&mut b, 44);
        u16le(&mut b, 45);
        u16le(&mut b, 45);
        u32le(&mut b, 0);
        u32le(&mut b, 0);
        u64le(&mut b, count);
        u64le(&mut b, count);
        u64le(&mut b, length);
        u64le(&mut b, start);
        u32le(&mut b, 0x0706_4b50);
        u32le(&mut b, 0);
        u64le(&mut b, end);
        u32le(&mut b, 1);
        u32le(&mut b, 0x0605_4b50);
        u16le(&mut b, 0);
        u16le(&mut b, 0);
        u16le(&mut b, u16::MAX);
        u16le(&mut b, u16::MAX);
        u32le(&mut b, u32::MAX);
        u32le(&mut b, u32::MAX);
        u16le(&mut b, 0);
        self.write(&b).await?;
        self.out.recycle(std::mem::take(&mut self.buffer));
        self.out.flush().await
    }
}
