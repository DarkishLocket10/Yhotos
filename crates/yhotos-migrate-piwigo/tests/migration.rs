#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]
//! End-to-end executable tests over a synthetic MariaDB 16-schema fixture.
use axum::{
    Router,
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::Value;
use sqlx::{Connection, MySqlConnection, MySqlPool, PgPool, Row};
use std::{path::Path, process::Command};
use tower::ServiceExt;
use yhotos_core::access::{Principal, ShareTarget};
use yhotos_db::{
    albums, grants,
    jobs::{self, JobStatus},
    photos, redirects, tags,
};
use yhotos_http::{AppState, public_router};
use yhotos_storage::{DerivativeKey, Store};

fn router(pool: &PgPool, store: &Path) -> Router {
    let config = yhotos_config::Config::from_figment(
        figment::Figment::new()
            .merge(("database_url_public", "postgres://127.0.0.1/test"))
            .merge((
                "session_key",
                STANDARD.encode(uuid::Uuid::now_v7().as_bytes().repeat(2)),
            ))
            .merge(("data_dir", store.to_string_lossy().as_ref()))
            .merge(("public_base_url", "https://example.test"))
            .merge(("admin_listen", "127.0.0.1:8081"))
            .merge(("admin_base_url", "https://admin.example.test"))
            .merge(("database_url_admin", "postgres://127.0.0.1/test")),
    )
    .unwrap();
    public_router(AppState::new(
        Box::leak(Box::new(config)),
        pool.clone(),
        Store::open(store).unwrap(),
        None,
    ))
}

async fn get(app: &Router, path: &str) -> axum::response::Response {
    let mut request = Request::builder().uri(path).body(Body::empty()).unwrap();
    request.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
    ));
    app.clone().oneshot(request).await.unwrap()
}

#[test]
fn snapshot_runbook_requires_stopped_writers_for_both_snapshots() {
    let runbook = include_str!("../../../docs/migration.md");
    let snapshot = runbook
        .split("2. For every rehearsal")
        .nth(1)
        .unwrap()
        .split("3. On the snapshot")
        .next()
        .unwrap();
    for requirement in [
        "all other",
        "database and upload writers",
        "Keep every writer stopped until both",
        "database dump and upload copy/snapshot finish",
        "MyISAM",
        "does not make those tables consistent",
    ] {
        assert!(
            snapshot.contains(requirement),
            "missing snapshot safeguard: {requirement}"
        );
    }
}

fn invoke(
    source: &str,
    destination: &str,
    root: &Path,
    store: &Path,
    report: &Path,
    dry: bool,
) -> Value {
    let mut command = Command::new(env!("CARGO_BIN_EXE_yhotos-migrate-piwigo"));
    command
        .env("PIWIGO_DATABASE_URL", source)
        .env("DATABASE_URL", destination)
        .env("YHOTOS_MIGRATION_PG_TLS", "disable")
        .args(["--source-root"])
        .arg(root)
        .arg("--storage-root")
        .arg(store)
        .arg("--report")
        .arg(report)
        .args(["--concurrency", "2"]);
    if dry {
        command.arg("--dry-run");
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&std::fs::read(report).unwrap()).unwrap()
}

// MariaDB's fixture is shared by these isolated PostgreSQL test databases.
// Keep a dedicated session lock alive until each test's source mutations end.
async fn source_fixture() -> Option<(String, MySqlPool, MySqlConnection)> {
    let Ok(source_url) = std::env::var("YHOTOS_TEST_MARIADB_URL") else {
        assert!(
            std::env::var_os("CI").is_none(),
            "MariaDB fixture service required in CI"
        );
        return None;
    };
    let fixture_url = url::Url::parse(&source_url).unwrap();
    assert_eq!(
        fixture_url.path(),
        "/piwigo_fixture",
        "fixture reset requires the reserved disposable database"
    );
    // This is the only MariaDB the test may touch. The fixed trusted dump is
    // test DDL, not SQL assembled from source data or request input.
    let mut lock = MySqlConnection::connect(&source_url).await.unwrap();
    let acquired = sqlx::raw_sql("SELECT GET_LOCK('yhotos_piwigo_fixture',120)")
        .fetch_one(&mut lock)
        .await
        .unwrap();
    assert_eq!(acquired.get::<Option<i64>, _>(0), Some(1));
    let source = MySqlPool::connect(&source_url).await.unwrap();
    sqlx::raw_sql(include_str!("fixtures/piwigo.sql"))
        .execute(&source)
        .await
        .unwrap();
    Some((source_url, source, lock))
}

struct Fixture {
    source_url: String,
    source: MySqlPool,
    _lock: MySqlConnection,
    temp: tempfile::TempDir,
    root: std::path::PathBuf,
    store: std::path::PathBuf,
    target: String,
}
impl Fixture {
    async fn new(pool: &PgPool) -> Option<Self> {
        let (source_url, source, lock) = source_fixture().await?;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("upload");
        let store = temp.path().join("store");
        std::fs::create_dir_all(root.join("2024")).unwrap();
        std::fs::create_dir(&store).unwrap();
        for name in ["one.jpg", "two.jpg"] {
            std::fs::write(
                root.join("2024").join(name),
                b"\xff\xd8\xffsynthetic original",
            )
            .unwrap();
        }
        std::fs::write(root.join("2024/five.png"), b"\x89PNG\r\n\x1a\nfixture").unwrap();
        let mut target = url::Url::parse(&std::env::var("DATABASE_URL").unwrap()).unwrap();
        target.set_path(pool.connect_options().get_database().unwrap());
        Some(Self {
            source_url,
            source,
            _lock: lock,
            temp,
            root,
            store,
            target: target.to_string(),
        })
    }
    fn import(&self, report: &str) -> Value {
        invoke(
            &self.source_url,
            &self.target,
            &self.root,
            &self.store,
            &self.temp.path().join(report),
            false,
        )
    }
    async fn change(&self, sql: &'static str) {
        sqlx::raw_sql(sql).execute(&self.source).await.unwrap();
    }
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn fixture_import_dry_run_rerun_and_metadata_updates(pool: PgPool) {
    let Some((source_url, source, _lock)) = source_fixture().await else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("upload");
    let store = temp.path().join("store");
    std::fs::create_dir_all(root.join("2024")).unwrap();
    std::fs::create_dir(&store).unwrap();
    let jpeg = b"\xff\xd8\xffsynthetic original";
    std::fs::write(root.join("2024/one.jpg"), jpeg).unwrap();
    // Different source identities with identical originals must still coexist.
    std::fs::write(root.join("2024/two.jpg"), jpeg).unwrap();
    std::fs::write(root.join("2024/five.png"), b"\x89PNG\r\n\x1a\nfixture").unwrap();
    std::fs::write(temp.path().join("outside.jpg"), jpeg).unwrap();
    let mut target = url::Url::parse(&std::env::var("DATABASE_URL").unwrap()).unwrap();
    target.set_path(pool.connect_options().get_database().unwrap());
    let target = target.to_string();
    let dry = invoke(
        &source_url,
        "invalid destination is never connected",
        &root,
        &store,
        &temp.path().join("dry.json"),
        true,
    );
    assert_eq!(dry.get("albums").unwrap(), 3);
    assert_eq!(dry.get("photos").unwrap(), 6);
    assert_eq!(dry.get("photo_rows").unwrap(), 4);
    assert_eq!(dry.get("skipped").unwrap().as_array().unwrap().len(), 3);
    assert!(dry.get("collisions").unwrap().as_array().unwrap().len() >= 3);
    assert_eq!(std::fs::read_dir(&store).unwrap().count(), 0);
    let mut conn = pool.acquire().await.unwrap();
    assert!(albums::list_tree(&mut conn).await.unwrap().is_empty());
    let first = invoke(
        &source_url,
        &target,
        &root,
        &store,
        &temp.path().join("first.json"),
        false,
    );
    assert_eq!(first.get("photo_rows").unwrap(), 4);
    assert_eq!(
        jobs::list(&mut conn, JobStatus::Pending, 0)
            .await
            .unwrap()
            .len(),
        6
    );
    let tree = albums::list_tree(&mut conn).await.unwrap();
    assert_eq!(tree.len(), 3);
    let public = tree.iter().find(|a| a.slug.as_str() == "snow-ice").unwrap();
    let private = tree
        .iter()
        .find(|a| a.slug.as_str() == "snow-ice-2")
        .unwrap();
    let nested = tree.iter().find(|a| a.slug.as_str() == "nested").unwrap();
    assert_eq!(nested.parent_id, Some(public.id));
    assert_eq!(public.title, "Snow & Ice");
    assert_eq!(public.description, "A trip");
    assert!(public.sort_order < private.sort_order);
    assert!(tree.iter().all(|a| a.cover_photo_id.is_some()));
    let by_hash = photos::get_by_hash(&mut conn, blake3::hash(jpeg).to_hex().as_ref())
        .await
        .unwrap();
    assert_eq!(by_hash.len(), 3);
    let source_photo = by_hash.iter().find(|p| p.album_id == public.id).unwrap();
    let original_id = source_photo.id;
    assert_eq!(source_photo.title, "Ice & snow");
    assert_eq!(source_photo.description, "A photo");
    assert_eq!(source_photo.taken_at.unwrap().year(), 2024);
    assert_eq!(source_photo.posted_at.month(), time::Month::February);
    assert_eq!(source_photo.gps_lat, Some(45.5));
    assert_eq!(
        tags::tags_of(&mut conn, source_photo.id)
            .await
            .unwrap()
            .first()
            .unwrap()
            .name,
        "Best & Bright"
    );
    let restricted = by_hash.iter().find(|p| p.album_id == private.id).unwrap();
    let original_tag_id = tags::tags_of(&mut conn, restricted.id)
        .await
        .unwrap()
        .first()
        .unwrap()
        .tag_id;
    assert_eq!(restricted.title, "two");
    assert!(
        grants::grants_for_photo(&mut conn, restricted.id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        grants::grants_for_album(&mut conn, private.id)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        restricted.visibility,
        yhotos_core::access::Visibility::Restricted
    );
    for path in [
        "/index.php?/category/1",
        "/index.php?/category/1-snow_ice",
        "/snow_ice",
        "/legacy/snow_ice",
    ] {
        let target: ShareTarget = redirects::lookup(&mut conn, path)
            .await
            .unwrap()
            .unwrap()
            .into();
        assert!(matches!(target,ShareTarget::Album(id) if id==public.id));
    }
    for path in ["/picture.php?/1", "/picture.php?/1/category/1"] {
        let target: ShareTarget = redirects::lookup(&mut conn, path)
            .await
            .unwrap()
            .unwrap()
            .into();
        assert!(matches!(target,ShareTarget::Photo(id) if id==original_id));
    }
    assert!(
        redirects::destination(
            &mut conn,
            &Principal::Anonymous,
            &["/picture.php?/2".into()]
        )
        .await
        .unwrap()
        .is_none()
    );
    // Imported rows must satisfy the HTTP handler's numeric fallbacks, while
    // the current album name must still win over a reset/stale numeric id.
    let app = router(&pool, &store);
    for path in [
        "/index.php?/category/1-obsolete-name",
        "/index.php?/category/3-snow_ice",
    ] {
        let response = get(&app, path).await;
        assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY, "{path}");
        assert_eq!(response.headers().get("location").unwrap(), "/a/snow-ice");
    }
    let response = get(&app, "/picture.php?/1/category/999").await;
    assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
    assert_eq!(
        response.headers().get("location").unwrap(),
        format!("/p/{}", source_photo.short_id).as_str()
    );

    // The fixture bytes have no EXIF GPS. Piwigo-only coordinates still queue
    // geocoding, and metadata-only reruns must clear and refresh old places.
    sqlx::query!("INSERT INTO gazetteer(geonameid,name,admin1,country,lat,lon,population) VALUES (1,'First','Region','CA',45.5,-73.6,1000),(2,'Second','Region','CA',48.5,-70.6,1000)").execute(&mut *conn).await.unwrap();
    yhotos_db::places::reverse(&mut conn, original_id)
        .await
        .unwrap();
    let initial_place = photos::get_by_short_id(&mut conn, &source_photo.short_id)
        .await
        .unwrap()
        .unwrap()
        .place_id
        .unwrap();
    let before: Vec<_> = by_hash.iter().map(|p| p.id).collect();
    let second = invoke(
        &source_url,
        &target,
        &root,
        &store,
        &temp.path().join("second.json"),
        false,
    );
    assert_eq!(second.get("reused_originals").unwrap(), 3);
    assert_eq!(
        jobs::list(&mut conn, JobStatus::Pending, 0)
            .await
            .unwrap()
            .len(),
        6
    );
    let after = photos::get_by_hash(&mut conn, blake3::hash(jpeg).to_hex().as_ref())
        .await
        .unwrap();
    assert_eq!(before, after.iter().map(|p| p.id).collect::<Vec<_>>());
    sqlx::query!("UPDATE jobs SET finished_at=now() WHERE kind='geo.reverse'")
        .execute(&mut *conn)
        .await
        .unwrap();
    // Fixture mutations are test setup, exclusively against the disposable DB.
    sqlx::raw_sql("UPDATE piwigo_images SET latitude=48.5,longitude=-70.6,name='Changed',author='Updated author',comment='<b>Updated</b>' WHERE id=1; DELETE FROM piwigo_image_tag WHERE image_id=1; UPDATE piwigo_tags SET name='Changed tag' WHERE id=2; UPDATE piwigo_tags SET name='Best &amp; Bright-2' WHERE id=1; UPDATE piwigo_categories SET name='Snow &amp; Ice-2' WHERE id=1; UPDATE piwigo_categories SET id_uppercat=3,global_rank='10.1' WHERE id=2;")
        .execute(&source).await.unwrap();
    invoke(
        &source_url,
        &target,
        &root,
        &store,
        &temp.path().join("third.json"),
        false,
    );
    let moved_gps = photos::get_by_short_id(&mut conn, &source_photo.short_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(moved_gps.gps_lat, Some(48.5));
    assert!(
        moved_gps.place_id.is_none(),
        "stale place must clear before geocoding"
    );
    let queued = jobs::list(&mut conn, JobStatus::Pending, 0).await.unwrap();
    assert!(queued.iter().any(|job| job.kind == "geo.reverse"
        && job.payload == serde_json::json!({"photo_id":original_id})));
    yhotos_db::places::reverse(&mut conn, original_id)
        .await
        .unwrap();
    assert_ne!(
        photos::get_by_short_id(&mut conn, &source_photo.short_id)
            .await
            .unwrap()
            .unwrap()
            .place_id
            .unwrap(),
        initial_place
    );
    let renamed_albums = albums::list_tree(&mut conn).await.unwrap();
    assert_eq!(
        renamed_albums
            .iter()
            .find(|a| a.id == public.id)
            .unwrap()
            .slug
            .as_str(),
        "snow-ice-2"
    );
    assert_eq!(
        renamed_albums
            .iter()
            .find(|a| a.id == private.id)
            .unwrap()
            .slug
            .as_str(),
        "snow-ice"
    );
    assert_eq!(
        renamed_albums
            .iter()
            .find(|a| a.id == nested.id)
            .unwrap()
            .parent_id,
        Some(private.id)
    );
    let changed = photos::get_by_short_id(&mut conn, &source_photo.short_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(changed.id, original_id);
    assert_eq!(changed.title, "Changed");
    assert_eq!(changed.description, "Updated");
    let renamed_tags = tags::tags_of(&mut conn, restricted.id).await.unwrap();
    let renamed_tag = renamed_tags.first().unwrap();
    assert_eq!(renamed_tag.tag_id, original_tag_id);
    assert_eq!(renamed_tag.name, "Changed tag");
    assert!(
        tags::tags_of(&mut conn, changed.id)
            .await
            .unwrap()
            .is_empty()
    );
    // Publish a derivative so all three routes are demonstrably reachable
    // before removing both public memberships of image 1 on the next rerun.
    let hash = blake3::hash(jpeg).to_hex().to_string();
    let content_hash = hash.parse().unwrap();
    let storage = Store::open(&store).unwrap();
    let staging = storage.tmp_dir().unwrap();
    let derivative = staging.path().join("derivative.jpg");
    std::fs::write(&derivative, jpeg).unwrap();
    storage
        .put_derivative(
            &DerivativeKey {
                hash: &content_hash,
                variant: "small",
                format: "jpeg",
                hdr: false,
            },
            &derivative,
        )
        .await
        .unwrap();
    sqlx::query!("INSERT INTO derivatives(id,photo_id,variant,format,hdr,hash,width,height,bytes) VALUES ($1,$2,'small','jpeg',false,$3,100,80,$4)",uuid::Uuid::now_v7(),original_id.0,hash,i64::try_from(jpeg.len()).unwrap()).execute(&mut *conn).await.unwrap();
    let paths = [
        format!("/p/{}", source_photo.short_id),
        format!("/i/{hash}/small.jpeg"),
        format!("/o/{hash}/one.jpg"),
    ];
    let app = router(&pool, &store);
    for path in &paths {
        assert_eq!(
            get(&app, path).await.status(),
            StatusCode::OK,
            "{path} before rerun"
        );
    }
    sqlx::raw_sql("DELETE FROM piwigo_image_category WHERE image_id=1; INSERT INTO piwigo_image_category VALUES (1,3,1); UPDATE piwigo_images SET level=1 WHERE id=1;").execute(&source).await.unwrap();
    invoke(
        &source_url,
        &target,
        &root,
        &store,
        &temp.path().join("restricted.json"),
        false,
    );
    // Use a fresh serving process/cache, as at migration cutover.
    let app = router(&pool, &store);
    for path in &paths {
        assert_eq!(
            get(&app, path).await.status(),
            StatusCode::NOT_FOUND,
            "{path} after rerun"
        );
    }
    let copies = photos::get_by_hash(&mut conn, &hash).await.unwrap();
    assert_eq!(copies.len(), 4);
    assert_eq!(copies.iter().filter(|p| p.deleted_at.is_some()).count(), 2);
    for copy in &copies {
        assert_eq!(copy.visibility, yhotos_core::access::Visibility::Restricted);
        assert!(
            grants::grants_for_photo(&mut conn, copy.id)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            get(&app, &format!("/p/{}", copy.short_id)).await.status(),
            StatusCode::NOT_FOUND
        );
    }
    // Existing report names must fail before changing destination state.
    let failed = Command::new(env!("CARGO_BIN_EXE_yhotos-migrate-piwigo"))
        .env("PIWIGO_DATABASE_URL", &source_url)
        .args(["--dry-run", "--source-root"])
        .arg(&root)
        .arg("--report")
        .arg(temp.path().join("dry.json"))
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert!(!String::from_utf8_lossy(&failed.stderr).contains(&source_url));
    assert_eq!(std::fs::read(root.join("2024/one.jpg")).unwrap(), jpeg);
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn deleted_collision_owners_do_not_break_reruns(pool: PgPool) {
    let Some(f) = Fixture::new(&pool).await else {
        return;
    };
    f.import("before.json");
    let mut conn = pool.acquire().await.unwrap();
    let albums_before = albums::list_tree(&mut conn).await.unwrap();
    let tags_before =
        sqlx::query!("SELECT source_id,tag_id FROM migration_tags ORDER BY source_id")
            .fetch_all(&mut *conn)
            .await
            .unwrap();
    let photos_before = sqlx::query!("SELECT photo_id FROM migration_sources WHERE image_id=1")
        .fetch_all(&mut *conn)
        .await
        .unwrap();
    // Delete the first owner of each collision, plus its nested category and
    // image. Their destination identities must remain reserved, not reused.
    f.change("DELETE FROM piwigo_categories WHERE id IN (1,2); DELETE FROM piwigo_images WHERE id=1; DELETE FROM piwigo_image_category WHERE image_id=1 OR category_id IN (1,2); DELETE FROM piwigo_tags WHERE id=1; DELETE FROM piwigo_image_tag WHERE tag_id=1;").await;
    f.import("after.json");
    let albums_after = albums::list_tree(&mut conn).await.unwrap();
    for before in &albums_before {
        let after = albums_after.iter().find(|a| a.id == before.id).unwrap();
        assert_eq!(before.slug, after.slug);
    }
    let tags_after = sqlx::query!("SELECT m.source_id,m.tag_id,t.slug::text AS \"slug!\" FROM migration_tags m JOIN tags t ON t.id=m.tag_id ORDER BY m.source_id")
        .fetch_all(&mut *conn).await.unwrap();
    for (before, after) in tags_before.iter().zip(&tags_after) {
        assert_eq!(
            (before.source_id, before.tag_id),
            (after.source_id, after.tag_id)
        );
    }
    assert_eq!(tags_after.first().unwrap().slug, "best-bright");
    assert_eq!(tags_after.get(1).unwrap().slug, "best-bright-2");
    for photo in photos_before {
        let retired = sqlx::query!("SELECT deleted_at FROM photos WHERE id=$1", photo.photo_id)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert!(retired.deleted_at.is_some());
    }
    let app = router(&pool, &f.store);
    assert_eq!(
        get(&app, "/picture.php?/1").await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(get(&app, "/snow_ice").await.status(), StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn removed_source_overrides_clear_publication_and_reextract_exif(pool: PgPool) {
    let Some(f) = Fixture::new(&pool).await else {
        return;
    };
    f.import("before.json");
    let mut conn = pool.acquire().await.unwrap();
    // Simulate completed ingest and geocoding with source-only dates/GPS.
    sqlx::query!("INSERT INTO gazetteer(geonameid,name,admin1,country,lat,lon,population) VALUES (1,'First','Region','CA',45.5,-73.6,1000)")
        .execute(&mut *conn).await.unwrap();
    let ids = sqlx::query!(
        "SELECT photo_id FROM migration_sources WHERE image_id=1 ORDER BY category_id"
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    for id in &ids {
        sqlx::query!("UPDATE photos SET status='ready',derivative_plan='fixture',taken_at=NULL,taken_at_local=NULL,gps_lat=NULL,gps_lon=NULL WHERE id=$1",id.photo_id)
            .execute(&mut *conn).await.unwrap();
        yhotos_db::places::reverse(&mut conn, yhotos_core::PhotoId(id.photo_id))
            .await
            .unwrap();
        assert!(
            sqlx::query!("SELECT place_id FROM photos WHERE id=$1", id.photo_id)
                .fetch_one(&mut *conn)
                .await
                .unwrap()
                .place_id
                .is_some()
        );
    }
    // An EXIF-only photo must retain its fallback on this same rerun.
    sqlx::query!("UPDATE photos SET gps_lat=10,gps_lon=20,taken_at=now(),taken_at_local=localtimestamp WHERE id IN (SELECT photo_id FROM migration_sources WHERE image_id=2)")
        .execute(&mut *conn).await.unwrap();
    f.change(
        "UPDATE piwigo_images SET date_creation=NULL,latitude=NULL,longitude=NULL WHERE id=1;",
    )
    .await;
    f.import("after.json");
    for id in &ids {
        let photo = sqlx::query!("SELECT taken_at,taken_at_local,gps_lat,gps_lon,place_id,status::text AS \"status!\",generation FROM photos WHERE id=$1",id.photo_id).fetch_one(&mut *conn).await.unwrap();
        assert!(photo.taken_at.is_none() && photo.taken_at_local.is_none());
        assert!(photo.gps_lat.is_none() && photo.gps_lon.is_none() && photo.place_id.is_none());
        assert_eq!(photo.status, "pending");
        assert_eq!(photo.generation, 1);
        let payload = serde_json::json!({"photo_id":id.photo_id,"original_hash":blake3::hash(b"\xff\xd8\xffsynthetic original").to_hex().to_string(),"generation":1});
        let queued = sqlx::query_scalar!(
            "SELECT count(*) FROM jobs WHERE kind='ingest' AND payload=$1 AND finished_at IS NULL",
            payload
        )
        .fetch_one(&mut *conn)
        .await
        .unwrap();
        assert_eq!(queued, Some(1));
        // Ingest publication with no EXIF must keep removed values absent.
        sqlx::query!("UPDATE photos SET status='ready',derivative_plan='no-exif',taken_at=NULL,taken_at_local=NULL,gps_lat=NULL,gps_lon=NULL WHERE id=$1",id.photo_id).execute(&mut *conn).await.unwrap();
        let published = sqlx::query!(
            "SELECT taken_at,gps_lat,gps_lon FROM photos WHERE id=$1",
            id.photo_id
        )
        .fetch_one(&mut *conn)
        .await
        .unwrap();
        assert!(
            published.taken_at.is_none()
                && published.gps_lat.is_none()
                && published.gps_lon.is_none()
        );
        // Real EXIF is accepted after the override is removed, through the same
        // publication trigger used by the pipeline.
        sqlx::query!("UPDATE photos SET derivative_plan='exif',taken_at=now(),taken_at_local=localtimestamp,gps_lat=30,gps_lon=40 WHERE id=$1",id.photo_id).execute(&mut *conn).await.unwrap();
        let published = sqlx::query!(
            "SELECT taken_at,gps_lat FROM photos WHERE id=$1",
            id.photo_id
        )
        .fetch_one(&mut *conn)
        .await
        .unwrap();
        assert!(published.taken_at.is_some());
        assert_eq!(published.gps_lat, Some(30.0));
    }
    let exif = sqlx::query!("SELECT p.taken_at,p.gps_lat,p.gps_lon,p.generation FROM photos p JOIN migration_sources m ON m.photo_id=p.id WHERE m.image_id=2").fetch_one(&mut *conn).await.unwrap();
    assert!(exif.taken_at.is_some());
    assert_eq!(
        (exif.gps_lat, exif.gps_lon, exif.generation),
        (Some(10.0), Some(20.0), 0)
    );
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn retired_unfinished_photos_get_new_ingest_generations_on_restoration(pool: PgPool) {
    let Some(f) = Fixture::new(&pool).await else {
        return;
    };
    f.import("before.json");
    let mut conn = pool.acquire().await.unwrap();
    let before = sqlx::query!("SELECT m.image_id,m.category_id,p.id,p.short_id,p.original_hash FROM migration_sources m JOIN photos p ON p.id=m.photo_id ORDER BY m.image_id,m.category_id").fetch_all(&mut *conn).await.unwrap();
    sqlx::query!(
        "UPDATE photos SET status='processing' WHERE id=$1",
        before.get(1).unwrap().id
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query!(
        "UPDATE photos SET status='ready',derivative_plan='fixture' WHERE id=$1",
        before.get(2).unwrap().id
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query!("INSERT INTO derivatives(id,photo_id,variant,format,hdr,hash,width,height,bytes) VALUES ($1,$2,'small','jpeg',false,$3,100,80,1)",uuid::Uuid::now_v7(),before.get(2).unwrap().id,before.get(2).unwrap().original_hash).execute(&mut *conn).await.unwrap();
    sqlx::query!(
        "UPDATE photos SET status='failed',failure_reason='fixture failure' WHERE id=$1",
        before.get(3).unwrap().id
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    f.change("DELETE FROM piwigo_image_category;").await;
    f.import("retired.json");
    // Terminate each worker envelope while its photo is unavailable. A new
    // generation must not deduplicate against these old finished jobs.
    sqlx::query!("UPDATE jobs SET max_attempts=1 WHERE kind='ingest'")
        .execute(&mut *conn)
        .await
        .unwrap();
    while let Some((job, token)) = jobs::claim(&mut conn, "fixture-worker", &["ingest"])
        .await
        .unwrap()
    {
        let id: uuid::Uuid =
            serde_json::from_value(job.payload.get("photo_id").unwrap().clone()).unwrap();
        let unavailable = sqlx::query!(
            "SELECT id FROM photos WHERE id=$1 AND deleted_at IS NULL",
            id
        )
        .fetch_optional(&mut *conn)
        .await
        .unwrap();
        assert!(unavailable.is_none());
        assert!(
            jobs::fail(
                &mut conn,
                &token,
                "photo unavailable",
                std::time::Duration::ZERO
            )
            .await
            .unwrap()
        );
    }
    assert_eq!(
        jobs::list(&mut conn, JobStatus::Failed, 0)
            .await
            .unwrap()
            .len(),
        4
    );
    f.change("INSERT INTO piwigo_image_category VALUES (1,1,8),(1,2,3),(2,3,1),(5,2,4);")
        .await;
    f.import("restored.json");
    for (index, old) in before.iter().enumerate() {
        let restored = sqlx::query!("SELECT id,short_id,deleted_at,status::text AS \"status!\",generation,failure_reason,derivative_plan FROM photos WHERE id=$1",old.id).fetch_one(&mut *conn).await.unwrap();
        assert_eq!(
            (restored.id, restored.short_id),
            (old.id, old.short_id.clone())
        );
        assert!(restored.deleted_at.is_none());
        if index == 2 {
            assert_eq!(restored.status, "ready");
            assert_eq!(restored.generation, 0);
            assert_eq!(restored.derivative_plan.as_deref(), Some("fixture"));
        } else {
            assert_eq!(restored.status, "pending");
            assert_eq!(restored.generation, 1);
            assert!(restored.failure_reason.is_none());
            let payload = serde_json::json!({"photo_id":old.id,"original_hash":old.original_hash,"generation":1});
            assert_eq!(sqlx::query_scalar!("SELECT count(*) FROM jobs WHERE kind='ingest' AND payload=$1 AND finished_at IS NULL",payload).fetch_one(&mut *conn).await.unwrap(),Some(1));
        }
    }
}

// Every pattern the importer persists must be routed to authorized lookup,
// including internal numeric fallbacks and encoded bare names.
async fn assert_stored_redirects_reachable(pool: &PgPool, store: &Path) {
    let mut conn = pool.acquire().await.unwrap();
    let paths = sqlx::query!("SELECT pattern FROM migration_redirects ORDER BY pattern")
        .fetch_all(&mut *conn)
        .await
        .unwrap();
    assert!(!paths.is_empty());
    let app = router(pool, store);
    for row in paths {
        let destination = redirects::destination(
            &mut conn,
            &Principal::Anonymous,
            std::slice::from_ref(&row.pattern),
        )
        .await
        .unwrap();
        let response = get(&app, &row.pattern).await;
        if let Some(destination) = destination {
            assert_eq!(
                response.status(),
                StatusCode::MOVED_PERMANENTLY,
                "{}",
                row.pattern
            );
            assert_eq!(
                response.headers()["location"],
                destination,
                "{}",
                row.pattern
            );
        } else {
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{}", row.pattern);
            assert!(response.headers().get("location").is_none());
        }
    }
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn bare_album_aliases_redirect_over_http_after_source_changes(pool: PgPool) {
    let Some(f) = Fixture::new(&pool).await else {
        return;
    };
    f.change("UPDATE piwigo_categories SET permalink='private_snow' WHERE id=3;")
        .await;
    f.import("before.json");
    let app = router(&pool, &f.store);
    assert_eq!(
        get(&app, "/snow_ice").await.status(),
        StatusCode::MOVED_PERMANENTLY
    );
    assert_eq!(
        get(&app, "/private_snow").await.status(),
        StatusCode::NOT_FOUND
    );
    assert_stored_redirects_reachable(&pool, &f.store).await;
    f.change("UPDATE piwigo_categories SET permalink='thawed_ice',name='Thawed &amp; Ice' WHERE id=1; UPDATE piwigo_categories SET status='public' WHERE id=3; UPDATE piwigo_categories SET status='private' WHERE id=2; INSERT INTO piwigo_categories VALUES (4,NULL,'Map',NULL,'20',NULL,'public','map','true'),(5,NULL,'0',NULL,'21',NULL,'public',NULL,'true');").await;
    f.import("after.json");
    let app = router(&pool, &f.store);
    let response = get(&app, "/thawed_ice").await;
    assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
    assert_eq!(response.headers()["location"], "/a/thawed-ice");
    assert_eq!(
        get(&app, "/private_snow").await.status(),
        StatusCode::MOVED_PERMANENTLY
    );
    assert_eq!(get(&app, "/nested").await.status(), StatusCode::NOT_FOUND);
    for invalid in ["/unknown-alias", "/snow_ice?query=1", "/two/segments"] {
        assert_eq!(get(&app, invalid).await.status(), StatusCode::NOT_FOUND);
    }
    let mut conn = pool.acquire().await.unwrap();
    assert!(
        redirects::lookup(&mut conn, "/map")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        redirects::lookup(&mut conn, "/index.php?/category/0")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        get(&app, "/legacy/map").await.status(),
        StatusCode::MOVED_PERMANENTLY
    );
    assert_stored_redirects_reachable(&pool, &f.store).await;
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn newly_ambiguous_aliases_remove_previous_http_redirects(pool: PgPool) {
    let Some(f) = Fixture::new(&pool).await else {
        return;
    };
    f.change("UPDATE piwigo_categories SET name='Trip',permalink=NULL WHERE id=1; UPDATE piwigo_categories SET name='Other',status='public' WHERE id=3;").await;
    f.import("before.json");
    let paths = ["/trip", "/legacy/trip", "/index.php?/category/trip"];
    let app = router(&pool, &f.store);
    for path in paths {
        assert_eq!(
            get(&app, path).await.status(),
            StatusCode::MOVED_PERMANENTLY,
            "{path}"
        );
    }
    let mut conn = pool.acquire().await.unwrap();
    let target = redirects::lookup(&mut conn, "/trip")
        .await
        .unwrap()
        .unwrap();
    redirects::insert(&mut conn, "/owner-kept", target.clone().into())
        .await
        .unwrap();
    // A mapping manually retargeted after an earlier import is independent.
    let nested = albums::list_tree(&mut conn)
        .await
        .unwrap()
        .into_iter()
        .find(|a| a.slug.as_str() == "nested")
        .unwrap();
    redirects::insert(&mut conn, "/Trip", ShareTarget::Album(nested.id))
        .await
        .unwrap();
    f.change("UPDATE piwigo_categories SET name='Trip' WHERE id=3;")
        .await;
    let report = f.import("after.json");
    assert!(
        report
            .get("collisions")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .any(|p| {
                p.get("reason")
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .contains("trip collides; alias omitted")
            })
    );
    let app = router(&pool, &f.store);
    for path in paths {
        assert!(redirects::lookup(&mut conn, path).await.unwrap().is_none());
        assert_eq!(
            get(&app, path).await.status(),
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    assert_eq!(
        get(&app, "/owner-kept").await.status(),
        StatusCode::MOVED_PERMANENTLY
    );
    assert!(
        matches!(redirects::lookup(&mut conn,"/Trip").await.unwrap().unwrap().into(),ShareTarget::Album(id) if id==nested.id)
    );
    for path in ["/index.php?/category/1", "/index.php?/category/3"] {
        assert_eq!(
            get(&app, path).await.status(),
            StatusCode::MOVED_PERMANENTLY
        );
    }
    assert_stored_redirects_reachable(&pool, &f.store).await;
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn piwigo_access_flags_remain_hidden_over_http_on_import_and_rerun(pool: PgPool) {
    let Some(f) = Fixture::new(&pool).await else {
        return;
    };
    f.change(include_str!("fixtures/privacy.sql")).await;
    for id in 10_u8..=17 {
        std::fs::write(
            f.root.join(format!("2024/{id}.jpg")),
            [0xff, 0xd8, 0xff, id],
        )
        .unwrap();
    }
    for report in ["private-first.json", "private-rerun.json"] {
        f.import(report);
        let mut conn = pool.acquire().await.unwrap();
        let rows = sqlx::query!("SELECT m.image_id,p.id,p.short_id,p.original_hash,p.original_name,a.id AS album_id,a.visibility::text AS \"visibility!\" FROM migration_sources m JOIN photos p ON p.id=m.photo_id JOIN albums a ON a.id=p.album_id WHERE m.image_id>=10 ORDER BY m.image_id")
            .fetch_all(&mut *conn).await.unwrap();
        assert_eq!(rows.len(), 8);
        let storage = Store::open(&f.store).unwrap();
        for row in &rows {
            assert!(
                grants::grants_for_photo(&mut conn, yhotos_core::PhotoId(row.id))
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert!(
                grants::grants_for_album(&mut conn, yhotos_core::AlbumId(row.album_id))
                    .await
                    .unwrap()
                    .is_empty()
            );
            if !matches!(row.image_id, 13 | 14) {
                assert_eq!(row.visibility, "restricted");
            }
            // Real stored bytes and derivative rows ensure file rejection is
            // authorization, rather than an absent original or derivative.
            let stage = storage.tmp_dir().unwrap();
            let path = stage.path().join("small.jpg");
            std::fs::write(&path, b"\xff\xd8\xffderivative").unwrap();
            let hash = row.original_hash.parse().unwrap();
            storage
                .put_derivative(
                    &DerivativeKey {
                        hash: &hash,
                        variant: "small",
                        format: "jpeg",
                        hdr: false,
                    },
                    &path,
                )
                .await
                .unwrap();
            sqlx::query!("INSERT INTO derivatives(id,photo_id,variant,format,hdr,hash,width,height,bytes) VALUES ($1,$2,'small','jpeg',false,$3,1,1,13) ON CONFLICT(photo_id,variant,format,hdr) DO NOTHING",uuid::Uuid::now_v7(),row.id,row.original_hash).execute(&mut *conn).await.unwrap();
        }
        let app = router(&pool, &f.store);
        let missing = get(&app, "/p/00000000").await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        let standard = axum::body::to_bytes(missing.into_body(), usize::MAX)
            .await
            .unwrap();
        let albums = [
            "locked",
            "user-private",
            "group-private",
            "level-public",
            "locked/locked-child",
            "user-access",
            "group-access",
            "locked-private",
        ];
        for (row, album) in rows.iter().zip(albums) {
            let mut paths = vec![
                format!("/p/{}", row.short_id),
                format!("/i/{}/small.jpeg", row.original_hash),
                format!("/o/{}/{}", row.original_hash, row.original_name),
                format!("/picture.php?/{}", row.image_id),
                format!("/picture.php?/{}/category/{}", row.image_id, row.image_id),
            ];
            if row.image_id != 13 {
                paths.extend([
                    format!("/a/{album}"),
                    format!("/index.php?/category/{}", row.image_id),
                    format!("/index.php?/category/{}-obsolete", row.image_id),
                ]);
            }
            for path in paths {
                let response = get(&app, &path).await;
                assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path} {report}");
                assert!(response.headers().get("location").is_none(), "{path}");
                assert_eq!(
                    axum::body::to_bytes(response.into_body(), usize::MAX)
                        .await
                        .unwrap(),
                    standard,
                    "{path}"
                );
            }
        }
        // Validate every imported name/permalink redirect as well.
        assert_stored_redirects_reachable(&pool, &f.store).await;
    }
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn numeric_name_aliases_cannot_replace_canonical_album_targets(pool: PgPool) {
    let Some(f) = Fixture::new(&pool).await else {
        return;
    };
    f.change("UPDATE piwigo_categories SET name='1',permalink=NULL WHERE id=2; UPDATE piwigo_categories SET name='1-snow_ice',permalink=NULL,status='public' WHERE id=3;").await;
    for report in ["canonical-first.json", "canonical-rerun.json"] {
        let report = f.import(report);
        assert!(
            report
                .get("collisions")
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .any(|p| {
                    p.get("reason")
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .contains("canonical numeric redirect")
                })
        );
        let mut conn = pool.acquire().await.unwrap();
        let album = sqlx::query!("SELECT album_id FROM migration_albums WHERE source_id=1")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        for path in [
            "/index.php?/category/1",
            "/index.php?/category/1-snow_ice",
            "/category/1",
        ] {
            let target: ShareTarget = redirects::lookup(&mut conn, path)
                .await
                .unwrap()
                .unwrap()
                .into();
            assert_eq!(
                target,
                ShareTarget::Album(yhotos_core::AlbumId(album.album_id)),
                "{path}"
            );
        }
        let app = router(&pool, &f.store);
        for path in ["/index.php?/category/1", "/index.php?/category/1-snow_ice"] {
            let response = get(&app, path).await;
            assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
            assert_eq!(response.headers()["location"], "/a/snow-ice");
        }
    }
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn unchanged_source_reruns_preserve_manual_redirect_targets_and_ownership(pool: PgPool) {
    let Some(f) = Fixture::new(&pool).await else {
        return;
    };
    f.import("manual-before.json");
    let mut conn = pool.acquire().await.unwrap();
    let nested = albums::list_tree(&mut conn)
        .await
        .unwrap()
        .into_iter()
        .find(|a| a.slug.as_str() == "nested")
        .unwrap();
    let paths = [
        "/snow_ice",
        "/index.php?/category/1",
        "/index.php?/category/1-snow_ice",
        "/picture.php?/1",
    ];
    for path in paths {
        redirects::insert(&mut conn, path, ShareTarget::Album(nested.id))
            .await
            .unwrap();
    }
    for report in ["manual-rerun.json", "manual-rerun-again.json"] {
        f.import(report);
        let app = router(&pool, &f.store);
        for path in paths {
            let target: ShareTarget = redirects::lookup(&mut conn, path)
                .await
                .unwrap()
                .unwrap()
                .into();
            assert_eq!(target, ShareTarget::Album(nested.id), "{path}");
            assert_eq!(
                sqlx::query_scalar!(
                    "SELECT count(*) FROM migration_redirects WHERE pattern=$1",
                    path
                )
                .fetch_one(&mut *conn)
                .await
                .unwrap(),
                Some(0)
            );
            let response = get(&app, path).await;
            assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY, "{path}");
            assert_eq!(response.headers()["location"], "/a/snow-ice/nested");
        }
    }
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn global_piwigo_access_flags_fail_closed_on_rerun(pool: PgPool) {
    let Some(f) = Fixture::new(&pool).await else {
        return;
    };
    for (index,sql) in [
        "INSERT INTO piwigo_config VALUES ('gallery_locked','true');",
        "UPDATE piwigo_config SET value='false' WHERE param='gallery_locked'; INSERT INTO piwigo_config VALUES ('guest_access','false');",
    ].into_iter().enumerate() {
        f.change(sql).await;
        for run in 0..2 {
            f.import(&format!("global-{index}-{run}.json"));
            let mut conn = pool.acquire().await.unwrap();
            assert!(albums::list_tree(&mut conn).await.unwrap().iter().all(|a|a.visibility==yhotos_core::access::Visibility::Restricted));
            let app = router(&pool,&f.store);
            for path in ["/a/snow-ice","/index.php?/category/1","/picture.php?/1"] {
                assert_eq!(get(&app,path).await.status(),StatusCode::NOT_FOUND,"{path}");
            }
        }
    }
}
