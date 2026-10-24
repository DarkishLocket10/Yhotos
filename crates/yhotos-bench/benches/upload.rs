#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]
//! Upload streaming excludes identity validation, which runs in its own process.
use axum::body::Body;
use criterion::{Criterion, criterion_group, criterion_main};
use yhotos_core::{Slug, access::Visibility};
use yhotos_db::{albums, photos, uploads, users};
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
    let photo = runtime
        .block_on(photos::create(
            &mut tx,
            &photos::NewPhoto {
                album_id: album.id,
                title: "Replacement target".into(),
                original_hash: "ab".repeat(32),
                original_name: "original.jpg".into(),
                original_bytes: 30_000_000,
                mime: "image/jpeg".into(),
                kind: photos::MediaKind::Image,
            },
        ))
        .unwrap();
    runtime
        .block_on(yhotos_db::library::bind_replacement(
            &mut tx, row.id, photo.id.0,
        ))
        .unwrap();
    let mut group = c.benchmark_group("tus_write_authorization");
    group
        .sample_size(20)
        .warm_up_time(std::time::Duration::from_secs(1))
        .measurement_time(std::time::Duration::from_secs(2));
    for (name, lookup) in [
        ("owner_only", 0),
        ("owner_with_expiry", 1),
        ("replacement_patch_after", 2),
        ("replacement_patch_before", 3),
    ] {
        group.bench_function(name, |b| {
            b.iter(|| {
                runtime.block_on(async {
                    let locked = match lookup {
                        0 => uploads::lock(&mut tx, row.id, user.id).await,
                        1 => uploads::lock_for_write(&mut tx, row.id, user.id).await,
                        2 => uploads::lock_for_patch(&mut tx, row.id, user.id).await,
                        _ => legacy_replacement_lock(&mut tx, row.id, user.id).await,
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
// The pre-review PATCH lookup, retained to measure the change on the same connection.
async fn legacy_replacement_lock(
    conn: &mut sqlx::PgConnection,
    id: uuid::Uuid,
    user: yhotos_core::UserId,
) -> Result<Option<uploads::Upload>, yhotos_db::DbError> {
    let row = sqlx::query_as!(uploads::Upload,r#"WITH tree_lock AS MATERIALIZED (
        SELECT pg_advisory_xact_lock(20) FROM uploads
        WHERE id=$1 AND user_id=$2 AND expires_at>now() AND replace_photo_id IS NOT NULL
    ) SELECT id,user_id AS "user_id: _",album_id AS "album_id: _",filename,declared_bytes,received_bytes,mime_hint,tmp_path,metadata,expires_at,created_at
    FROM uploads WHERE id=$1 AND user_id=$2 AND expires_at>now()
        AND (SELECT count(*) FROM tree_lock)>=0 FOR UPDATE NOWAIT"#,id,user.0).fetch_optional(&mut *conn).await?;
    if let Some(ref row) = row {
        sqlx::query!(
            "SELECT id FROM albums WHERE id=$1 AND deleted_at IS NULL FOR SHARE",
            row.album_id.0
        )
        .fetch_one(conn)
        .await?;
    }
    Ok(row)
}
criterion_group!(benches, upload, write_authorization);
criterion_main!(benches);
