#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]
//! Compare ingest publication before and after generation/administrative guards.
//! Requires a disposable migrated `DATABASE_URL`; all rows and DDL roll back.
use axum::http::Uri;
use criterion::{Criterion, criterion_group, criterion_main};
use sqlx::Connection;
use yhotos_core::Slug;
use yhotos_db::{
    migration::{self, ImportPhoto},
    photos::{MediaKind, NewPhoto},
};

// Frozen parser from the revision before bare album routing.
fn positive_id(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|b| b.is_ascii_digit())
        && value.parse::<u64>().is_ok_and(|id| id > 0)
}
fn legacy_before(uri: &Uri) -> Option<Vec<String>> {
    if let Some(rest) = uri.path().strip_prefix("/legacy/") {
        if rest.len() > 4096
            || rest.is_empty()
            || rest.contains(['\\', '\0'])
            || uri.query().is_some()
        {
            return None;
        }
        return Some(vec![uri.path().to_owned(), format!("/{rest}")]);
    }
    let query = uri.query()?;
    match uri.path() {
        "/index.php" => {
            let category = query.strip_prefix("/category/")?;
            let (id, slug) = category
                .split_once('-')
                .map_or((category, None), |(id, slug)| (id, Some(slug)));
            if !positive_id(id) {
                if category
                    .split('-')
                    .next()
                    .is_some_and(|part| part.bytes().all(|b| b.is_ascii_digit()))
                    || category.is_empty()
                    || category.len() > 1024
                    || category.contains(['/', '\\', '?', '#', '&'])
                    || category.chars().any(char::is_control)
                {
                    return None;
                }
                return Some(vec![format!("/index.php?{query}"), format!("/{category}")]);
            }
            if slug.is_some_and(|slug| {
                slug.is_empty()
                    || slug.len() > 1024
                    || slug.contains(['/', '\\', '?', '#', '&'])
                    || slug.chars().any(char::is_control)
            }) {
                return None;
            }
            let mut patterns = vec![
                format!("/index.php?{query}"),
                format!("/category/{category}"),
            ];
            if let Some(slug) = slug {
                patterns.push(format!("/{slug}"));
            }
            patterns.push(format!("/category/{id}"));
            Some(patterns)
        }
        "/picture.php" => {
            let rest = query.strip_prefix('/')?;
            let Some((id, category)) = rest.split_once("/category/") else {
                return positive_id(rest)
                    .then(|| vec![format!("/picture.php?/{rest}"), format!("/picture/{rest}")]);
            };
            if !positive_id(id) || !positive_id(category) {
                return None;
            }
            Some(vec![
                format!("/picture.php?{query}"),
                format!("/picture/{id}/category/{category}"),
                format!("/picture/{id}"),
            ])
        }
        _ => None,
    }
}

fn benchmark(c: &mut Criterion) {
    let uri: Uri = "/index.php?/category/12-snow_ice".parse().unwrap();
    c.bench_function("legacy_patterns/before", |b| {
        b.iter(|| std::hint::black_box(legacy_before(std::hint::black_box(&uri))));
    });
    c.bench_function("legacy_patterns/after", |b| {
        b.iter(|| {
            std::hint::black_box(yhotos_http::pages::legacy_patterns(std::hint::black_box(
                &uri,
            )))
        });
    });
    let album: Uri = "/snow_ice".parse().unwrap();
    c.bench_function("legacy_patterns/bare_album_before", |b| {
        b.iter(|| std::hint::black_box(legacy_before(std::hint::black_box(&album))));
    });
    c.bench_function("legacy_patterns/bare_album_after", |b| {
        b.iter(|| {
            std::hint::black_box(yhotos_http::pages::legacy_patterns(std::hint::black_box(
                &album,
            )))
        });
    });
    let bare: Uri = "/picture.php?/12".parse().unwrap();
    c.bench_function("legacy_patterns/bare_photo", |b| {
        b.iter(|| {
            std::hint::black_box(yhotos_http::pages::legacy_patterns(std::hint::black_box(
                &bare,
            )))
        });
    });

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let Some(url) = std::env::var("DATABASE_URL").ok() else {
        return;
    };
    let mut conn = runtime.block_on(sqlx::PgConnection::connect(&url)).unwrap();
    let mut tx = runtime.block_on(conn.begin()).unwrap();
    let album = runtime
        .block_on(migration::upsert_album(
            &mut tx,
            i64::MAX,
            None,
            &Slug::from_title(&uuid::Uuid::now_v7().to_string()),
            "Benchmark",
            "",
            0,
            false,
        ))
        .unwrap();
    let hash = format!(
        "{}{}",
        uuid::Uuid::now_v7().simple(),
        uuid::Uuid::now_v7().simple()
    );
    let row = ImportPhoto {
        image_id: i64::MAX,
        category_id: i64::MAX,
        original: NewPhoto {
            album_id: album,
            title: "Benchmark".into(),
            original_hash: hash,
            original_name: "one.jpg".into(),
            original_bytes: 1,
            mime: "image/jpeg".into(),
            kind: MediaKind::Image,
        },
        description: String::new(),
        author: None,
        taken_at: Some(time::OffsetDateTime::now_utc()),
        taken_at_local: None,
        posted_at: time::OffsetDateTime::now_utc(),
        sort_order: 0,
        restricted: false,
        gps_lat: Some(45.0),
        gps_lon: Some(-73.0),
    };
    let ids = runtime
        .block_on(migration::upsert_photos(&mut tx, &[row]))
        .unwrap();
    let id = *ids.first().unwrap();
    // Frozen pre-review trigger; benchmark DDL is private to this transaction
    // and rolls back. Both cases return effective coordinates, isolating the
    // generation/administrative guard's cost in the pipeline publication path.
    runtime
        .block_on(
            sqlx::raw_sql(
                r"
CREATE OR REPLACE FUNCTION preserve_piwigo_metadata() RETURNS trigger LANGUAGE plpgsql
SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
DECLARE imported record;
BEGIN
 SELECT * INTO imported FROM public.migration_sources
 WHERE photo_id=NEW.id AND original_hash=NEW.original_hash;
 IF FOUND THEN
  NEW.taken_at := coalesce(imported.taken_at, NEW.taken_at);
  NEW.taken_at_local := coalesce(imported.taken_at_local, NEW.taken_at_local);
  NEW.gps_lat := coalesce(imported.gps_lat, NEW.gps_lat);
  NEW.gps_lon := coalesce(imported.gps_lon, NEW.gps_lon);
 END IF;
 RETURN NEW;
END $$;
",
            )
            .execute(&mut *tx),
        )
        .unwrap();
    // Isolate each sample: retaining thousands of successful row versions in
    // this outer transaction biases the later case with a longer MVCC history.
    // Savepoint setup and rollback are excluded from publication timings.
    c.bench_function("migration_metadata/before", |b| {
        b.iter_custom(|iterations| {
            let mut elapsed = std::time::Duration::ZERO;
            for _ in 0..iterations {
                let mut sample = runtime.block_on(tx.begin()).unwrap();
                let start = std::time::Instant::now();
                runtime.block_on(sqlx::query!("UPDATE photos SET taken_at=NULL,taken_at_local=NULL,gps_lat=NULL,gps_lon=NULL,status='ready',derivative_plan='benchmark' WHERE id=$1 RETURNING gps_lat,gps_lon",id.0).fetch_one(&mut *sample)).unwrap();
                elapsed += start.elapsed();
                runtime.block_on(sample.rollback()).unwrap();
            }
            elapsed
        });
    });
    let migration =
        include_str!("../../yhotos-db/migrations/20261022002204_piwigo_metadata_generation.sql");
    let start = migration.find("CREATE OR REPLACE FUNCTION").unwrap();
    runtime
        .block_on(sqlx::raw_sql(migration.get(start..).unwrap()).execute(&mut *tx))
        .unwrap();
    c.bench_function("migration_metadata/after", |b| {
        b.iter_custom(|iterations| {
            let mut elapsed = std::time::Duration::ZERO;
            for _ in 0..iterations {
                let mut sample = runtime.block_on(tx.begin()).unwrap();
                let start = std::time::Instant::now();
                runtime.block_on(sqlx::query!("UPDATE photos SET taken_at=NULL,taken_at_local=NULL,gps_lat=NULL,gps_lon=NULL,status='ready',derivative_plan='benchmark' WHERE id=$1 RETURNING gps_lat,gps_lon",id.0).fetch_one(&mut *sample)).unwrap();
                elapsed += start.elapsed();
                runtime.block_on(sample.rollback()).unwrap();
            }
            elapsed
        });
    });
    runtime.block_on(tx.rollback()).unwrap();
}
criterion_group! { name=benches; config=Criterion::default().sample_size(10).warm_up_time(std::time::Duration::from_secs(1)).measurement_time(std::time::Duration::from_secs(2)); targets=benchmark }
criterion_main!(benches);
