#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::cast_precision_loss)] // Benchmark arithmetic only.
//! Disk streaming baseline and STORE ZIP64 throughput.
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use tokio::io::AsyncSeekExt;
use yhotos_http::zip::writer::ZipWriter;
fn bench(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("original");
    let bytes = 64 * 1024 * 1024_u64;
    std::fs::write(&path, vec![123; usize::try_from(bytes).unwrap()]).unwrap();
    let mut group = c.benchmark_group("zip_disk_stream");
    group.sample_size(10);
    group.throughput(Throughput::Bytes(bytes));
    let mut file = rt.block_on(tokio::fs::File::open(&path)).unwrap();
    group.bench_function("plain_copy", |b| {
        b.iter(|| {
            rt.block_on(async {
                file.rewind().await.unwrap();
                tokio::io::copy_buf(
                    &mut tokio::io::BufReader::with_capacity(1024 * 1024, &mut file),
                    &mut tokio::io::sink(),
                )
                .await
                .unwrap();
            });
        });
    });
    group.bench_function("store_zip64", |b| {
        b.iter(|| {
            rt.block_on(async {
                file.rewind().await.unwrap();
                let mut zip = ZipWriter::new(tokio::io::sink(), bytes + 4096);
                zip.entry("001-original.jpeg".into(), bytes, &mut file)
                    .await
                    .unwrap();
                zip.finish().await.unwrap();
            });
        });
    });
    group.finish();
    #[cfg(unix)]
    {
        use nix::sys::{
            resource::{UsageWho, getrusage},
            time::TimeValLike,
        };
        let cpu = || {
            let usage = getrusage(UsageWho::RUSAGE_SELF).unwrap();
            usage.user_time().num_microseconds() + usage.system_time().num_microseconds()
        };
        let before = cpu();
        for _ in 0..32 {
            rt.block_on(async {
                file.rewind().await.unwrap();
                let mut zip = ZipWriter::new(tokio::io::sink(), bytes + 4096);
                zip.entry("001-original.jpeg".into(), bytes, &mut file)
                    .await
                    .unwrap();
                zip.finish().await.unwrap();
            });
        }
        let used = cpu() - before;
        // Microseconds / 2 GiB, scaled to a 200 MB/s sustained transfer.
        eprintln!(
            "zip CPU at 200 MB/s: {:.3}% of one core",
            used as f64 / 1_000_000.0 / (bytes * 32) as f64 * 200_000_000.0 * 100.0
        );
    }
}
criterion_group!(benches, bench);
criterion_main!(benches);
