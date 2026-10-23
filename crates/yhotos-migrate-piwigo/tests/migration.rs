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
use sqlx::PgPool;
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

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn fixture_import_dry_run_rerun_and_metadata_updates(pool: PgPool) {
    let Ok(source_url) = std::env::var("YHOTOS_TEST_MARIADB_URL") else {
        assert!(
            std::env::var_os("CI").is_none(),
            "MariaDB fixture service required in CI"
        );
        return;
    };
    let fixture_url = url::Url::parse(&source_url).unwrap();
    assert_eq!(
        fixture_url.path(),
        "/piwigo_fixture",
        "fixture reset requires the reserved disposable database"
    );
    // This is the only MariaDB the test may touch. The fixed trusted dump is
    // test DDL, not SQL assembled from source data or request input.
    let source = sqlx::MySqlPool::connect(&source_url).await.unwrap();
    sqlx::raw_sql(include_str!("fixtures/piwigo.sql"))
        .execute(&source)
        .await
        .unwrap();
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
