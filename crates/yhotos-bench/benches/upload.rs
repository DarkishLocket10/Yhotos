#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]
//! Upload streaming excludes identity validation, which runs in its own process.
use axum::body::Body;
use criterion::{Criterion, criterion_group, criterion_main};
fn upload(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = yhotos_storage::Store::open(dir.path()).unwrap();
    let chunk = axum::body::Bytes::from(vec![42; 8 * 1024 * 1024]);
    c.bench_function("tus_stream_30mb_8mib_chunks", |b| {
        b.iter(|| {
            runtime.block_on(async {
                let (id, mut file) = store.create_upload().await.unwrap();
                let mut offset = 0;
                while offset < 30_000_000 {
                    let len =
                        usize::try_from((30_000_000 - offset).min(chunk.len() as u64)).unwrap();
                    offset = yhotos_http::admin::upload::stream::write(
                        &mut file,
                        Body::from(chunk.slice(..len)),
                        offset,
                        8 * 1024 * 1024,
                    )
                    .await
                    .unwrap();
                }
                drop(file);
                store.remove_upload(&id).await.unwrap();
            });
        });
    });
    c.bench_function("tus_metadata_parse",|b|b.iter(||yhotos_http::admin::upload::headers::metadata(std::hint::black_box("filename cGhvdG8uanBn,album MDE5YTFiMzQtNGM1Ni03Nzg5LThiOTAtMTIzNDU2Nzg5YWJj,filetype aW1hZ2UvanBlZw=="))));
}
criterion_group!(benches, upload);
criterion_main!(benches);
