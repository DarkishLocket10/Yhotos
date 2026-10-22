#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]
//! End-to-end executable tests over a synthetic MariaDB 16-schema fixture.
use serde_json::Value;
use sqlx::PgPool;
use std::{path::Path, process::Command};
use yhotos_core::access::{Principal, ShareTarget};
use yhotos_db::{
    albums, grants,
    jobs::{self, JobStatus},
    photos, redirects, tags,
};

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
        4
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
        4
    );
    let after = photos::get_by_hash(&mut conn, blake3::hash(jpeg).to_hex().as_ref())
        .await
        .unwrap();
    assert_eq!(before, after.iter().map(|p| p.id).collect::<Vec<_>>());
    // Fixture mutations are test setup, exclusively against the disposable DB.
    sqlx::raw_sql("UPDATE piwigo_images SET name='Changed',author='Updated author',comment='<b>Updated</b>' WHERE id=1; DELETE FROM piwigo_image_tag WHERE image_id=1; UPDATE piwigo_tags SET name='Changed tag' WHERE id=2; UPDATE piwigo_tags SET name='Best &amp; Bright-2' WHERE id=1; UPDATE piwigo_categories SET name='Snow &amp; Ice-2' WHERE id=1; UPDATE piwigo_categories SET id_uppercat=3,global_rank='10.1' WHERE id=2;")
        .execute(&source).await.unwrap();
    invoke(
        &source_url,
        &target,
        &root,
        &store,
        &temp.path().join("third.json"),
        false,
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
