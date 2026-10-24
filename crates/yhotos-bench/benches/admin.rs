#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::too_many_lines)]
//! Same transactional action and SSR models as the admin HTTP handler, at 1,000 photos.
use criterion::{Criterion, criterion_group, criterion_main};
use serde_json::json;
use std::{
    hint::black_box,
    time::{Duration, Instant},
};
use yhotos_core::{PhotoId, ShortId, Slug, access::Visibility};
use yhotos_db::{albums, library, library_pages, users};
fn admin(c: &mut Criterion) {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _context = runtime.enter();
    let pool = runtime.block_on(sqlx::PgPool::connect(&url)).unwrap();
    let mut tx = runtime.block_on(pool.begin()).unwrap();
    let password = yhotos_auth::generate_temp_password();
    let hash = yhotos_auth::hash_password(&password).unwrap();
    let user = runtime
        .block_on(users::create(
            &mut tx,
            &uuid::Uuid::now_v7().to_string(),
            &secrecy::SecretString::from(hash.as_str()),
            false,
        ))
        .unwrap();
    let a = runtime
        .block_on(albums::create(
            &mut tx,
            None,
            &Slug::from_title(&uuid::Uuid::now_v7().to_string()),
            "Benchmark album",
            Visibility::Public,
        ))
        .unwrap();
    let b = runtime
        .block_on(albums::create(
            &mut tx,
            None,
            &Slug::from_title(&uuid::Uuid::now_v7().to_string()),
            "Destination",
            Visibility::Public,
        ))
        .unwrap();
    let photos: Vec<_> = (0..1000).map(|_| PhotoId::new()).collect();
    let ids: Vec<_> = photos.iter().map(|p| p.0).collect();
    let shorts: Vec<_> = photos
        .iter()
        .map(|p| ShortId::from_photo_id(*p).to_string())
        .collect();
    let hash = blake3_hash();
    runtime.block_on(sqlx::query!("INSERT INTO photos(id,short_id,album_id,title,original_hash,original_name,original_bytes,mime,kind) SELECT id,short,$3,'Fixture',$4,'fixture.jpeg',1,'image/jpeg','image' FROM unnest($1::uuid[],$2::text[]) p(id,short)",&ids,&shorts,a.id.0,hash).execute(&mut *tx)).unwrap();
    runtime
        .block_on(
            sqlx::query!("UPDATE albums SET manual_order=true WHERE id=$1", a.id.0)
                .execute(&mut *tx),
        )
        .unwrap();
    let csrf = yhotos_http::middleware::csrf::CsrfToken::generate().unwrap();
    let data = runtime
        .block_on(library_pages::page(&mut tx, "album", Some(a.id.0), "", 0))
        .unwrap();
    let choices = runtime.block_on(library_pages::choices(&mut tx)).unwrap();
    let mut group = c.benchmark_group("admin");
    group
        .sample_size(10)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(2));
    group.bench_function("album_1000_render", |b| {
        b.iter(|| {
            black_box(
                yhotos_http::admin::render_album(
                    black_box(&data),
                    black_box(&choices),
                    &csrf,
                    "bench",
                )
                .unwrap(),
            )
        });
    });
    group.bench_function("album_1000_query_and_render", |b| {
        b.iter(|| {
            runtime.block_on(async {
                let data = library_pages::page(&mut tx, "album", Some(a.id.0), "", 0)
                    .await
                    .unwrap();
                let choices = library_pages::choices(&mut tx).await.unwrap();
                black_box(
                    yhotos_http::admin::render_album(&data, &choices, &csrf, "bench").unwrap(),
                )
            });
        });
    });
    for kind in ["move", "visibility", "copy", "reorder"] {
        let command:library::Command=serde_json::from_value(json!({"op":"bulk","ids":ids,"action":match kind{"reorder"=>json!({"kind":"reorder","album_id":a.id}),"move"=>json!({"kind":"move","album_id":b.id}),"copy"=>json!({"kind":"copy","album_id":b.id}),_=>json!({"kind":"visibility","visibility":"restricted","grants":[]})}})).unwrap();
        group.bench_function(format!("bulk_1000_{kind}"), |b| {
            b.iter_custom(|n| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..n {
                    runtime.block_on(async {
                        let mut iteration = sqlx::Connection::begin(&mut *tx).await.unwrap();
                        let started = Instant::now();
                        black_box(
                            library::execute_in(&mut iteration, user.id, &command)
                                .await
                                .unwrap(),
                        );
                        elapsed += started.elapsed();
                        iteration.rollback().await.unwrap();
                    });
                }
                elapsed
            });
        });
    }
    let photo = *photos.first().unwrap();
    runtime.block_on(async {
        sqlx::query!("INSERT INTO gazetteer(geonameid,name,admin1,country,lat,lon,population) VALUES(2147483000,'Benchmark city','Ontario','CA',43.7,-79.4,1000000) ON CONFLICT DO NOTHING").execute(&mut *tx).await.unwrap();
        sqlx::query!("UPDATE photos SET gps_lat=43.7,gps_lon=-79.4 WHERE id=$1",photo.0).execute(&mut *tx).await.unwrap();
    });
    group.bench_function("geocode_and_invalidate", |b| {
        b.iter_custom(|n| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..n {
                runtime.block_on(async {
                    let mut iteration = sqlx::Connection::begin(&mut *tx).await.unwrap();
                    let started = Instant::now();
                    yhotos_db::places::reverse(&mut iteration, photo)
                        .await
                        .unwrap();
                    elapsed += started.elapsed();
                    iteration.rollback().await.unwrap();
                });
            }
            elapsed
        });
    });
    let purge: library::Command = serde_json::from_value(json!({
        "op":"purge","kind":"photo","id":photo,"confirmation":"PURGE"
    }))
    .unwrap();
    let destination = b.id;
    group.bench_function("purge_pending_replacement", |b| {
        b.iter_custom(|n| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..n {
                runtime.block_on(async {
                    let mut iteration = sqlx::Connection::begin(&mut *tx).await.unwrap();
                    sqlx::query!("UPDATE photos SET deleted_at=now() WHERE id=$1",photo.0).execute(&mut *iteration).await.unwrap();
                    sqlx::query!("INSERT INTO uploads(id,user_id,album_id,filename,declared_bytes,mime_hint,tmp_path,replace_photo_id) VALUES($1,$2,$3,'pending.jpeg',100,'image/jpeg',$4,$5)",uuid::Uuid::now_v7(),user.id.0,destination.0,uuid::Uuid::new_v4().to_string(),photo.0).execute(&mut *iteration).await.unwrap();
                    let started = Instant::now();
                    black_box(library::execute_in(&mut iteration,user.id,&purge).await.unwrap());
                    elapsed += started.elapsed();
                    iteration.rollback().await.unwrap();
                });
            }
            elapsed
        });
    });
    group.finish();
    runtime.block_on(tx.rollback()).unwrap();
    runtime.block_on(pool.close());
}
fn blake3_hash() -> String {
    // A valid content-address fixture; no image IO is included.
    "ab".repeat(32)
}
criterion_group!(benches, admin);
criterion_main!(benches);
