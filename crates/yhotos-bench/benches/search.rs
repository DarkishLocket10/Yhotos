#![forbid(unsafe_code)]
//! Parser, RRF and fused real-Postgres search over ten thousand synthetic photos.
use criterion::Criterion;
use std::{
    error::Error,
    hint::black_box,
    time::{Duration, Instant},
};
use yhotos_core::{AlbumId, PhotoId, ShortId, access::Principal};
use yhotos_db::{
    access_sql,
    search::{self, List},
};
use yhotos_http::search::{fuse, query};

async fn fused(
    pool: &sqlx::PgPool,
    vector: &str,
    filters: &[search::Filter],
) -> Result<usize, yhotos_db::DbError> {
    let p = access_sql::visible_photos(&Principal::Anonymous);
    let (a, b, c) = tokio::try_join!(
        search::candidates(
            pool,
            &p,
            filters,
            "sunset",
            Some(vector),
            None,
            List::Semantic
        ),
        search::candidates(pool, &p, filters, "sunset", Some(vector), None, List::Text),
        search::candidates(
            pool,
            &p,
            filters,
            "sunset",
            Some(vector),
            None,
            List::Structured
        )
    )?;
    Ok(fuse([a, b, c]).len())
}
fn bench_people(
    criterion: &mut Criterion,
    runtime: &tokio::runtime::Runtime,
    pool: &sqlx::PgPool,
    person: uuid::Uuid,
    cluster_count: usize,
    photo_count: usize,
) -> Result<(), Box<dyn Error>> {
    let mut conn = runtime.block_on(pool.acquire())?;
    let last = runtime.block_on(yhotos_db::faces::people(&mut conn, 4, Some(person)))?;
    if last.total != cluster_count || last.people.len() != 25 {
        return Err("people benchmark returned incomplete merge targets".into());
    }
    criterion.bench_function("search/people/206/first_page", |b| {
        b.iter(|| black_box(runtime.block_on(yhotos_db::faces::people(&mut conn, 1, None))));
    });
    criterion.bench_function("search/people/205/merge_page_4", |b| {
        b.iter(|| {
            black_box(runtime.block_on(yhotos_db::faces::people(&mut conn, 4, Some(person))))
        });
    });
    let first = runtime.block_on(yhotos_db::faces::photos(&mut conn, person, 1))?;
    if first.photos.len() != 60 || first.total != photo_count {
        return Err("person benchmark returned incomplete results".into());
    }
    criterion.bench_function("search/person/10k/first_page", |b| {
        b.iter(|| black_box(runtime.block_on(yhotos_db::faces::photos(&mut conn, person, 1))));
    });
    criterion.bench_function("search/person/10k/page_100", |b| {
        b.iter(|| black_box(runtime.block_on(yhotos_db::faces::photos(&mut conn, person, 100))));
    });
    runtime.block_on(async { drop(conn) });
    Ok(())
}
fn bench_fused(
    criterion: &mut Criterion,
    runtime: &tokio::runtime::Runtime,
    pool: &sqlx::PgPool,
    vector: &str,
) -> Result<(), Box<dyn Error>> {
    for _ in 0..10 {
        runtime.block_on(fused(pool, vector, &[]))?;
    }
    let mut times = Vec::with_capacity(1000);
    for _ in 0..1000 {
        let start = Instant::now();
        runtime.block_on(fused(pool, vector, &[]))?;
        times.push(start.elapsed());
    }
    times.sort_unstable();
    let p99 = times.get(989).copied().unwrap_or(Duration::MAX);
    eprintln!("search/10k/fused p99: {p99:?} (1000 warm samples)");
    criterion.bench_function("search/10k/fused", |b| {
        b.iter(|| black_box(runtime.block_on(fused(pool, vector, &[]))));
    });
    let filters = [
        search::Filter {
            key: "after".into(),
            value: "2026-01-01".into(),
        },
        search::Filter {
            key: "before".into(),
            value: "2027-01-01".into(),
        },
    ];
    criterion.bench_function("search/10k/utc_date_filters", |b| {
        b.iter(|| black_box(runtime.block_on(fused(pool, vector, &filters))));
    });
    Ok(())
}
fn main() -> Result<(), Box<dyn Error>> {
    let mut criterion = Criterion::default().configure_from_args();
    criterion.bench_function("search/parser", |b| {
        b.iter(|| {
            black_box(query::parse(black_box(
                "sunset camera:\"Sony A7\" year:2025 tag:beach",
            )))
        });
    });
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL absent; 10k database benchmark not measured");
        criterion.final_summary();
        return Ok(());
    };
    let runtime = tokio::runtime::Runtime::new()?;
    let options = url.parse::<sqlx::postgres::PgConnectOptions>()?.options([
        ("jit", "off"),
        ("hnsw.ef_search", "200"),
        ("hnsw.iterative_scan", "strict_order"),
        ("hnsw.max_scan_tuples", "20000"),
    ]);
    let pool = runtime.block_on(
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(8)
            .connect_with(options),
    )?;
    let album = AlbumId::new();
    let person = uuid::Uuid::now_v7();
    let photo_ids: Vec<_> = (0..10_000).map(|_| PhotoId::new()).collect();
    let short_ids: Vec<_> = photo_ids
        .iter()
        .map(|id| ShortId::from_photo_id(*id).to_string())
        .collect();
    let photo_ids: Vec<_> = photo_ids.into_iter().map(|id| id.0).collect();
    let clusters: Vec<_> = (0..205).map(|_| uuid::Uuid::now_v7()).collect();
    let names: Vec<_> = (0..205)
        .map(|i| {
            if i < 200 {
                format!("Person {i:03}")
            } else {
                String::new()
            }
        })
        .collect();
    let sample = photo_ids.first().ok_or("missing benchmark photo")?;
    runtime.block_on(async {
        sqlx::query!("INSERT INTO albums(id,slug,title) VALUES($1,$2,'Synthetic search benchmark')",album.0,format!("search-bench-{}",album.0)).execute(&pool).await?;
        sqlx::query!("INSERT INTO photos(id,short_id,album_id,title,original_hash,original_name,original_bytes,mime,kind,status) SELECT id,short_id,$3,'sunset photo '||n,repeat('a',64),'fixture.jpg',1,'image/jpeg','image','ready' FROM unnest($1::uuid[],$2::text[]) WITH ORDINALITY AS fixture(id,short_id,n)",&photo_ids,&short_ids,album.0).execute(&pool).await?;
        sqlx::query!("INSERT INTO embeddings(photo_id,model,vector) SELECT p.id,$2,ARRAY(SELECT random()::real FROM generate_series(1,768) g WHERE p.id IS NOT NULL)::vector FROM photos p WHERE p.album_id=$1",album.0,search::EMBED_MODEL).execute(&pool).await?;
        sqlx::query!("INSERT INTO persons(id,name) VALUES($1,'Search benchmark person')",person).execute(&pool).await?;
        sqlx::query!("INSERT INTO faces(id,photo_id,bbox,quality,person_id,model) SELECT id,id,ARRAY[0.1,0.1,0.5,0.5],0.9,$2,'fixture' FROM unnest($1::uuid[]) id",&photo_ids,person).execute(&pool).await?;
        sqlx::query!("INSERT INTO persons(id,name) SELECT id,nullif(name,'') FROM unnest($1::uuid[],$2::text[]) f(id,name)",&clusters,&names).execute(&pool).await?;
        sqlx::query!("INSERT INTO faces(id,photo_id,bbox,quality,person_id,model) SELECT id,$2,ARRAY[0.1,0.1,0.5,0.5],0.9,id,'fixture' FROM unnest($1::uuid[]) id",&clusters,sample).execute(&pool).await?;
        sqlx::query!("ANALYZE photos,embeddings,albums,ocr_text,tags,photo_tags,faces,persons").execute(&pool).await?;
        Ok::<_,sqlx::Error>(())
    })?;
    let vector = format!("[1,{}]", vec!["0"; 767].join(","));
    bench_fused(&mut criterion, &runtime, &pool, &vector)?;
    bench_people(
        &mut criterion,
        &runtime,
        &pool,
        person,
        clusters.len(),
        photo_ids.len(),
    )?;
    let plan=runtime.block_on(async {
        sqlx::query_scalar!(r#"EXPLAIN (ANALYZE,BUFFERS,FORMAT JSON) SELECT photo_id FROM embeddings ORDER BY vector <=> $1::text::vector LIMIT 200"#,vector).fetch_one(&pool).await
    })?;
    eprintln!("HNSW distance scan EXPLAIN: {}", plan.unwrap_or_default());
    runtime.block_on(async {
        sqlx::query!("DELETE FROM photos WHERE album_id=$1", album.0)
            .execute(&pool)
            .await?;
        sqlx::query!("DELETE FROM albums WHERE id=$1", album.0)
            .execute(&pool)
            .await?;
        sqlx::query!("DELETE FROM persons WHERE id=$1", person)
            .execute(&pool)
            .await?;
        sqlx::query!("DELETE FROM persons WHERE id=ANY($1)", &clusters)
            .execute(&pool)
            .await?;
        Ok::<_, sqlx::Error>(())
    })?;
    criterion.final_summary();
    Ok(())
}
