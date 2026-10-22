#![forbid(unsafe_code)]
//! Complete grid/template render and optional warm PostgreSQL page query.
use criterion::Criterion;
use serde_json::json;
use std::{error::Error, hint::black_box, time::Instant};
use yhotos_core::{AlbumId, PhotoId, ShortId, access::Principal};
use yhotos_db::{
    albums::{self, AlbumPage},
    photos::PageSort,
};
use yhotos_http::pages::{PageContext, render_album};

fn fixture() -> Result<AlbumPage, Box<dyn Error>> {
    let album = AlbumId::new();
    let photos=(0..60).map(|index| {
        let id=PhotoId::new();
        json!({"id":id,"short_id":ShortId::from_photo_id(id),"album_id":album,"title":format!("Photo {index}"),"description":"Fixture photograph","original_hash":"a".repeat(64),"original_name":"photo.jpeg","kind":"image","width":1500,"height":1000,"dominant_colors":[2_443_131],"thumbhash":"AAgCBwAAAAAAAAAAAAAAAAAAAAAAAAAA","derivatives":[
          {"variant":"small","format":"jpeg","hdr":false,"hash":"b".repeat(64),"w":640,"h":427},
          {"variant":"medium","format":"jpeg","hdr":false,"hash":"c".repeat(64),"w":1280,"h":853},
          {"variant":"large","format":"jpeg","hdr":false,"hash":"d".repeat(64),"w":2048,"h":1365},
          {"variant":"small","format":"avif","hdr":false,"hash":"e".repeat(64),"w":640,"h":427},
          {"variant":"small","format":"jxl","hdr":false,"hash":"f".repeat(64),"w":640,"h":427}
        ],"camera_make":null,"camera_model":null,"lens":null,"focal_mm":null,"aperture":null,"shutter":null,"iso":null,"gps_lat":null,"gps_lon":null})
    }).collect::<Vec<_>>();
    Ok(serde_json::from_value(
        json!({"album":{"id":album,"title":"Fixture gallery","description":"Sixty photos","path":"gallery-fixture","count":60,"cover":photos.first()},"chain":[{"id":album,"title":"Fixture gallery","path":"gallery-fixture"}],"children":[],"photos":photos,"total":60}),
    )?)
}
fn main() -> Result<(), Box<dyn Error>> {
    let mut criterion = Criterion::default().configure_from_args();
    let context = PageContext {
        nonce: "benchmark-nonce",
        stylesheet: "/static/app.fixture.css",
        script: "/static/thumbhash.fixture.js",
        public_base_url: "https://gallery.example",
    };
    let page = fixture()?;
    render_album(&page, 1, PageSort::Taken, &context)?;
    criterion.bench_function("pages/render_album/60", |b| {
        b.iter(|| black_box(render_album(black_box(&page), 1, PageSort::Taken, &context)));
    });
    if let Ok(url) = std::env::var("DATABASE_URL") {
        let runtime = tokio::runtime::Runtime::new()?;
        let _runtime_context = runtime.enter();
        let db = runtime.block_on(yhotos_db::Db::connect(
            &url.into(),
            yhotos_db::Role::Public,
            2,
            yhotos_db::DatabaseTls::Disable,
        ))?;
        let path = vec![yhotos_core::Slug::new("gallery-fixture")?];
        let mut conn = runtime.block_on(db.pool().acquire())?;
        let read = |conn: &mut sqlx::PgConnection| {
            runtime.block_on(albums::page(
                conn,
                &Principal::Anonymous,
                &path,
                1,
                PageSort::Taken,
            ))
        };
        if read(&mut conn)?.is_some() {
            let mut samples = Vec::with_capacity(1000);
            for _ in 0..1000 {
                let start = Instant::now();
                if let Some(page) = read(&mut conn)? {
                    black_box(render_album(&page, 1, PageSort::Taken, &context)?);
                }
                samples.push(start.elapsed());
            }
            samples.sort_unstable();
            eprintln!(
                "pages/warm_album/60 p50={:?} p99={:?} (1000 samples)",
                samples.get(500),
                samples.get(990)
            );
            criterion.bench_function("pages/warm_album/60", |b| {
                b.iter(|| {
                    black_box(
                        read(&mut conn)
                            .and_then(|page| page.ok_or(yhotos_db::DbError::InvalidInput))
                            .map(|page| render_album(&page, 1, PageSort::Taken, &context)),
                    )
                });
            });
        } else {
            eprintln!(
                "Warm DB benchmark needs the gallery-fixture album; run the pages_fixture example."
            );
        }
    }
    criterion.final_summary();
    Ok(())
}
