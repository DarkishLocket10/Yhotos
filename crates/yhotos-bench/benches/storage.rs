#![forbid(unsafe_code)]

//! Measures complete original writes against a fresh store per iteration.

use std::hint::black_box;
use std::time::Instant;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use yhotos_storage::Store;

fn storage(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new()
        .unwrap_or_else(|e| std::process::exit(e.raw_os_error().unwrap_or(1)));
    let input = vec![0x5a; 30_000_000];
    let mut group = c.benchmark_group("storage");
    group.throughput(Throughput::Bytes(30_000_000));
    group.sample_size(20);
    group.bench_function("put_original_30mb", |b| {
        b.iter_custom(|iterations| {
            (0..iterations)
                .map(|_| {
                    let Ok(root) = tempfile::tempdir() else {
                        std::process::exit(1)
                    };
                    let Ok(store) = Store::open(root.path()) else {
                        std::process::exit(1)
                    };
                    // Exclude store setup and root cleanup, as in the baseline.
                    let start = Instant::now();
                    let result = runtime
                        .block_on(store.put_original(black_box(input.as_slice()), 30_000_000));
                    let elapsed = start.elapsed();
                    if !matches!(result, Ok(put) if !put.existed && put.bytes == 30_000_000) {
                        std::process::exit(1);
                    }
                    elapsed
                })
                .sum()
        });
    });
    group.finish();
}

criterion_group!(benches, storage);
criterion_main!(benches);
