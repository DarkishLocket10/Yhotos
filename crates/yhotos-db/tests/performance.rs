#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used)]
//! Query count and realistic-data plan checks for the two database hot paths.

use sqlx::{ConnectOptions, Connection, PgConnection, PgPool};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tracing::{Event, Subscriber, instrument::WithSubscriber};
use tracing_subscriber::{Layer, layer::Context, prelude::*};
use uuid::Uuid;
use yhotos_core::{PhotoId, ShortId, Slug, access::Visibility};
use yhotos_db::{albums, photos};

struct QueryCount(Arc<AtomicUsize>);
impl<S: Subscriber> Layer<S> for QueryCount {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        if event.metadata().target() == "sqlx::query" {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[sqlx::test]
async fn album_photos_and_derivatives_are_one_query(pool: PgPool) {
    let mut conn = PgConnection::connect_with(
        &pool
            .connect_options()
            .as_ref()
            .clone()
            .log_statements(log::LevelFilter::Debug),
    )
    .await
    .unwrap();
    let album = albums::create(
        &mut conn,
        None,
        &Slug::new("count").unwrap(),
        "Count",
        Visibility::Public,
    )
    .await
    .unwrap();
    let p = photos::create(
        &mut conn,
        &photos::NewPhoto {
            album_id: album.id,
            title: "Photo".into(),
            original_hash: format!("{}{}", Uuid::now_v7().simple(), Uuid::now_v7().simple()),
            original_name: "photo.jpg".into(),
            original_bytes: 1,
            mime: "image/jpeg".into(),
            kind: photos::MediaKind::Image,
        },
    )
    .await
    .unwrap();
    let hash = format!("{}{}", Uuid::now_v7().simple(), Uuid::now_v7().simple());
    sqlx::query!("INSERT INTO derivatives(id,photo_id,variant,format,hdr,hash,width,height,bytes) VALUES ($1,$2,'small','jpeg',false,$3,640,480,123)",Uuid::now_v7(),p.id.0,hash).execute(&mut conn).await.unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    let subscriber = tracing_subscriber::registry().with(QueryCount(counter.clone()));
    // Drop the query future/stream while its subscriber is still entered. Attaching
    // the subscriber directly to that future can miss the SQLx logger's Drop event.
    let rows = async {
        photos::list_in_album(&mut conn, album.id, photos::PhotoOrder::Manual, 0, 50)
            .await
            .unwrap()
    }
    .with_subscriber(subscriber)
    .await;
    assert_eq!(counter.load(Ordering::Relaxed), 1);
    assert_eq!(rows.len(), 1);
    let derivative = rows.first().unwrap().derivatives.get(0).unwrap();
    assert_eq!(derivative.get("hash").unwrap(), &hash);
    assert_eq!(derivative.get("width").unwrap(), 640);
}

#[sqlx::test]
async fn album_and_claim_plans_use_indexes(pool: PgPool) {
    assert_eq!(
        include_str!("../queries/list_in_album_explain.sql")
            .strip_prefix("EXPLAIN (ANALYZE, FORMAT TEXT) "),
        Some(include_str!("../queries/list_in_album.sql"))
    );
    assert_eq!(
        include_str!("../queries/claim_explain.sql")
            .strip_prefix("EXPLAIN (ANALYZE, FORMAT TEXT) "),
        Some(include_str!("../queries/claim.sql"))
    );
    let mut conn = pool.acquire().await.unwrap();
    let album = albums::create(
        &mut conn,
        None,
        &Slug::new("indexed").unwrap(),
        "Indexed",
        Visibility::Public,
    )
    .await
    .unwrap();
    let other = albums::create(
        &mut conn,
        None,
        &Slug::new("other").unwrap(),
        "Other",
        Visibility::Public,
    )
    .await
    .unwrap();
    let ids: Vec<_> = (0..5000).map(|_| PhotoId::new().0).collect();
    let short_ids: Vec<_> = ids
        .iter()
        .map(|id| ShortId::from_photo_id(PhotoId(*id)).to_string())
        .collect();
    let hash = format!("{}{}", Uuid::now_v7().simple(), Uuid::now_v7().simple());
    sqlx::query!("INSERT INTO photos(id,short_id,album_id,title,original_hash,original_name,original_bytes,mime,kind,sort_order) SELECT ids.id,shorts.short_id,CASE WHEN ids.position<=50 THEN $3::uuid ELSE $4::uuid END,'Photo',$5,'photo.jpg',1,'image/jpeg','image',ids.position::integer FROM unnest($1::uuid[]) WITH ORDINALITY AS ids(id,position) JOIN unnest($2::text[]) WITH ORDINALITY AS shorts(short_id,position) USING(position)",&ids,&short_ids,album.id.0,other.id.0,hash).execute(&mut *conn).await.unwrap();
    let job_ids: Vec<_> = (0..5000).map(|_| Uuid::now_v7()).collect();
    sqlx::query!("INSERT INTO jobs(id,kind,payload,run_after) SELECT id,CASE WHEN position<=10 THEN 'ingest' ELSE 'other' END,'{}',CASE WHEN position<=10 THEN now() ELSE now()+interval '1 day' END FROM unnest($1::uuid[]) WITH ORDINALITY AS j(id,position)",&job_ids).execute(&mut *conn).await.unwrap();
    sqlx::query!("ANALYZE photos")
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query!("ANALYZE jobs")
        .execute(&mut *conn)
        .await
        .unwrap();
    let photo_plan = sqlx::query_file_scalar!(
        "queries/list_in_album_explain.sql",
        album.id.0,
        "manual",
        50_i64,
        0_i64
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    let photo_plan = photo_plan
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n");
    assert!(photo_plan.contains("Index"), "{photo_plan}");
    assert!(!photo_plan.contains("Seq Scan on photos"), "{photo_plan}");
    let kinds = ["ingest"];
    // EXPLAIN ANALYZE executes the claim inside this disposable test database.
    let job_plan = sqlx::query_file_scalar!(
        "queries/claim_explain.sql",
        "plan-worker",
        &kinds as &[&str]
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    let job_plan = job_plan
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n");
    assert!(job_plan.contains("Index"), "{job_plan}");
    assert!(!job_plan.contains("Seq Scan on jobs"), "{job_plan}");
}
