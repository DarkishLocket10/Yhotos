//! Read and CRC in one blocking task, directly into the buffer transferred to HTTP.
use super::writer::Input;
use std::io::{self, Read};

pub(super) struct File(pub Option<std::fs::File>);
// The archive producer runs on the blocking pool. Keep disk reads and CRC on
// that same thread, without another task handoff or intermediate file buffer.
impl Input for File {
    fn read(
        &mut self,
        mut buffer: Vec<u8>,
        len: usize,
        mut crc: crc32fast::Hasher,
    ) -> impl Future<Output = io::Result<(Vec<u8>, usize, crc32fast::Hasher)>> + Send {
        std::future::ready((|| {
            let file = self
                .0
                .as_mut()
                .ok_or_else(|| io::Error::other("archive file unavailable"))?;
            buffer.clear();
            let n = file.take(len as u64).read_to_end(&mut buffer)?;
            crc.update(&buffer);
            Ok((buffer, n, crc))
        })())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::zip::writer::{COPY_BYTES, ZipWriter};
    use std::io::{Seek, Write};

    #[tokio::test]
    async fn reserved_buffer_is_reused_and_reads_never_exceed_the_requested_limit() {
        let file = tempfile::tempfile().unwrap();
        file.set_len(COPY_BYTES as u64 + 137).unwrap();
        let mut input = File(Some(file));
        let buffer = Vec::with_capacity(COPY_BYTES);
        let address = buffer.as_ptr();
        let (buffer, n, crc) = input
            .read(buffer, COPY_BYTES, crc32fast::Hasher::new())
            .await
            .unwrap();
        assert_eq!(n, COPY_BYTES);
        assert_eq!(buffer.capacity(), COPY_BYTES);
        assert_eq!(buffer.as_ptr(), address);
        let (buffer, n, _) = input.read(buffer, 1, crc).await.unwrap();
        assert_eq!(n, 1);
        assert_eq!(buffer.len(), 1);
        assert_eq!(
            input.0.as_mut().unwrap().stream_position().unwrap(),
            COPY_BYTES as u64 + 1
        );
    }

    #[tokio::test]
    async fn owned_disk_reads_match_reference_crc_across_buffers_and_entries() {
        let data: Vec<_> = (0..COPY_BYTES + 137).map(|n| n.to_le_bytes()[0]).collect();
        let mut actual = Vec::new();
        let mut expected = Vec::new();
        let cap = data.len() as u64 * 2 + 1024;
        let mut disk = ZipWriter::new(&mut actual, cap);
        let mut reference = ZipWriter::new(&mut expected, cap);
        for (name, bytes) in [
            ("001-first.bin", data.as_slice()),
            ("002-second.bin", data.get(..137).unwrap()),
        ] {
            let mut file = tempfile::tempfile().unwrap();
            file.write_all(bytes).unwrap();
            file.rewind().unwrap();
            disk.entry(name.into(), bytes.len() as u64, File(Some(file)))
                .await
                .unwrap();
            reference
                .entry(name.into(), bytes.len() as u64, bytes)
                .await
                .unwrap();
        }
        disk.finish().await.unwrap();
        reference.finish().await.unwrap();
        assert_eq!(actual, expected);
    }
}
