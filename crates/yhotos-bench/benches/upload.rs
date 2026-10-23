#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]
//! Upload streaming excludes identity validation, which runs in its own process.
use axum::body::Body;
use criterion::{Criterion, criterion_group, criterion_main};
use yhotos_core::{Slug, access::Visibility};
use yhotos_db::{albums, uploads, users};
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
// Compare blocking lookups with PATCH's NOWAIT lookup on the same connection
// and rows. All fixture changes are rolled back after measurement.
fn write_authorization(c: &mut Criterion) {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("Skipping tus write authorization benchmark: DATABASE_URL absent");
        return;
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _context = runtime.enter();
    let pool = runtime.block_on(sqlx::PgPool::connect(&url)).unwrap();
    let mut tx = runtime.block_on(pool.begin()).unwrap();
    let password = yhotos_auth::generate_temp_password();
    let phc = yhotos_auth::hash_password(&password).unwrap();
    let user = runtime
        .block_on(users::create(
            &mut tx,
            &uuid::Uuid::now_v7().simple().to_string(),
            &secrecy::SecretString::from(phc.as_str()),
            false,
        ))
        .unwrap();
    let album = runtime
        .block_on(albums::create(
            &mut tx,
            None,
            &Slug::from_title(&uuid::Uuid::now_v7().to_string()),
            "Upload benchmark",
            Visibility::Restricted,
        ))
        .unwrap();
    let row = runtime
        .block_on(uploads::create(
            &mut tx,
            user.id,
            album.id,
            "original.jpg",
            30_000_000,
            "image/jpeg",
            &uuid::Uuid::new_v4().to_string(),
            "",
        ))
        .unwrap();
    let mut group = c.benchmark_group("tus_write_authorization");
    for (name, lookup) in [
        ("owner_only", 0),
        ("owner_and_live_album", 1),
        ("patch_nowait_and_live_album", 2),
    ] {
        group.bench_function(name, |b| {
            b.iter(|| {
                runtime.block_on(async {
                    let locked = match lookup {
                        0 => uploads::lock(&mut tx, row.id, user.id).await,
                        1 => uploads::lock_for_write(&mut tx, row.id, user.id).await,
                        _ => uploads::lock_for_patch(&mut tx, row.id, user.id).await,
                    }
                    .unwrap();
                    assert!(std::hint::black_box(locked).is_some());
                });
            });
        });
    }
    group.finish();
    runtime.block_on(tx.rollback()).unwrap();
    runtime.block_on(pool.close());
}
criterion_group!(benches, upload, write_authorization);
criterion_main!(benches);
