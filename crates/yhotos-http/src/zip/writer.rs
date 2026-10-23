//! STORE-only ZIP writer. Only the copy buffer and central-directory metadata are retained.
use std::io::{Error, ErrorKind, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
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
impl<W: AsyncWrite + Unpin> ZipWriter<W> {
    /// A byte ceiling includes headers, descriptors and the central directory.
    pub fn new(out: W, cap: u64) -> Self {
        Self {
            out,
            cap,
            offset: 0,
            entries: Vec::new(),
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
        self.out.write_all(bytes).await?;
        self.offset = next;
        Ok(())
    }
    /// Streams exactly the declared bytes, rejects unsafe names and detects truncated/growing input.
    /// # Errors
    /// Returns IO failures or a violated size/name/byte limit.
    pub async fn entry(
        &mut self,
        name: String,
        size: u64,
        mut input: impl AsyncRead + Unpin,
    ) -> Result<()> {
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
        let mut buffer = vec![0; 1024 * 1024];
        let mut remaining = size;
        while remaining > 0 {
            let len = usize::try_from(remaining.min(buffer.len() as u64)).map_err(|_| invalid())?;
            let chunk = buffer.get_mut(..len).ok_or_else(invalid)?;
            let n = input.read(chunk).await?;
            if n == 0 {
                return Err(invalid());
            }
            let chunk = chunk.get(..n).ok_or_else(invalid)?;
            crc.update(chunk);
            self.write(chunk).await?;
            remaining -= n as u64;
        }
        match input.read_u8().await {
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => {}
            Err(e) => return Err(e),
            Ok(_) => return Err(invalid()),
        }
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
        self.out.flush().await
    }
}
