use sqlx::PgPool;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use yhotos_core::{PhotoId, Slug, access::Visibility};
use yhotos_db::{albums, photos};
use yhotos_pipeline::{Ingest, JobError, TestPause, VariantPlan};
use yhotos_storage::{ContentHash, DerivativeKey, Store};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/images")
        .join(name)
}
async fn setup(pool: &PgPool, name: &str) -> (tempfile::TempDir, Ingest, PhotoId) {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(root.path()).unwrap();
    let input = tokio::fs::File::open(fixture(name)).await.unwrap();
    let original = store.put_original(input, 2 << 30).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let album = albums::create(
        &mut conn,
        None,
        &Slug::new(&format!("album-{}", uuid::Uuid::now_v7())).unwrap(),
        "Pipeline test",
        Visibility::Public,
    )
    .await
    .unwrap();
    let photo = photos::create(
        &mut conn,
        &photos::NewPhoto {
            album_id: album.id,
            title: "Fixture".into(),
            original_hash: original.hash.to_string(),
            original_name: name.into(),
            original_bytes: i64::try_from(original.bytes).unwrap(),
            mime: "image/jpeg".into(),
            kind: photos::MediaKind::Image,
        },
    )
    .await
    .unwrap();
    let host = PathBuf::from(env!("CARGO_BIN_EXE_yhotos-codec-host"));
    (root, Ingest::new(store, pool.clone(), host), photo.id)
}
async fn empty_tmp(root: &Path) {
    assert_eq!(
        tokio::fs::read_dir(root.join("tmp"))
            .await
            .unwrap()
            .next_entry()
            .await
            .unwrap()
            .map(|e| e.file_name()),
        None
    );
}
fn count_files(path: &Path) -> usize {
    std::fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            if path.is_dir() { count_files(&path) } else { 1 }
        })
        .sum()
}
async fn assert_ready(pool: &PgPool, ingest: &Ingest, id: PhotoId) {
    let photo=sqlx::query!("SELECT status::text AS \"status!\",width,height,orientation,placeholder,dominant_colors,camera_make,gps_lat,gps_lon FROM photos WHERE id=$1",id.0).fetch_one(pool).await.unwrap();
    assert_eq!(photo.status, "ready");
    assert!(photo.width.unwrap() > 0);
    assert!(photo.height.unwrap() > 0);
    assert!(photo.placeholder.unwrap().len() >= 5);
    assert_ne!(photo.dominant_colors, [] as [i32; 0]);
    let rows=sqlx::query!("SELECT variant,format,hash,width,height,bytes FROM derivatives WHERE photo_id=$1 ORDER BY variant,format",id.0).fetch_all(pool).await.unwrap();
    let expected = VariantPlan::default()
        .variants
        .iter()
        .map(|v| v.formats.len() * if v.scale2x { 2 } else { 1 })
        .sum::<usize>();
    assert_eq!(rows.len(), expected);
    for row in rows {
        let hash: ContentHash = row.hash.parse().unwrap();
        let path = ingest
            .store
            .path_derivative(&DerivativeKey {
                hash: &hash,
                variant: &row.variant,
                format: &row.format,
                hdr: false,
            })
            .unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            u64::try_from(row.bytes).unwrap()
        );
        assert_eq!(
            blake3::hash(&std::fs::read(path).unwrap())
                .to_hex()
                .as_str(),
            row.hash
        );
        assert!(row.width > 0 && row.height > 0);
    }
    let jobs = sqlx::query!(
        "SELECT kind,priority FROM jobs WHERE payload=$1 ORDER BY kind",
        serde_json::json!({"photo_id":id})
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let mut expected = vec!["ml.embed", "ml.faces", "ml.ocr", "ml.tags"];
    if photo.gps_lat.is_some() && photo.gps_lon.is_some() {
        expected.insert(0, "geo.reverse");
    }
    assert_eq!(
        jobs.iter().map(|j| j.kind.as_str()).collect::<Vec<_>>(),
        expected
    );
    assert!(jobs.iter().all(|j| j.priority < 0));
}
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn real_host_chart_and_orientations_publish_and_rerun_without_changes(pool: PgPool) {
    for name in std::iter::once("chart-srgb.png".to_owned())
        .chain((1..=8).map(|n| format!("orientation-{n}.jpg")))
        .chain(std::iter::once("exif-heavy.jpg".to_owned()))
    {
        let (root, ingest, id) = setup(&pool, &name).await;
        ingest.ingest(id).await.unwrap();
        assert_ready(&pool, &ingest, id).await;
        if name == "exif-heavy.jpg" {
            let row=sqlx::query!("SELECT camera_make,camera_model,lens,focal_mm,aperture,shutter,iso,gps_lat,gps_lon FROM photos WHERE id=$1",id.0).fetch_one(&pool).await.unwrap();
            assert_eq!(row.camera_make.as_deref(), Some("Yhotos"));
            assert_eq!(row.camera_model.as_deref(), Some("Fixture Camera"));
            assert_eq!(row.lens.as_deref(), Some("Synthetic 50mm"));
            assert_eq!(row.focal_mm, Some(50.0));
            assert_eq!(row.aperture, Some(2.8));
            assert_eq!(row.iso, Some(200));
            assert!(row.shutter.is_some());
            assert!(row.gps_lat.is_some() && row.gps_lon.is_some());
        }
        empty_tmp(root.path()).await;
        let before = count_files(root.path());
        let first = sqlx::query!(
            "SELECT updated_at,derivative_plan FROM photos WHERE id=$1",
            id.0
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        ingest.ingest(id).await.unwrap();
        let second = sqlx::query!(
            "SELECT updated_at,derivative_plan FROM photos WHERE id=$1",
            id.0
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(first.updated_at, second.updated_at);
        assert_eq!(first.derivative_plan, second.derivative_plan);
        assert_eq!(before, count_files(root.path()));
        assert_ready(&pool, &ingest, id).await;
    }
}
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn malicious_input_fails_without_outputs_and_cleans_tmp(pool: PgPool) {
    let (root, ingest, id) = setup(&pool, "malicious/bomb-png.png").await;
    assert!(matches!(ingest.ingest(id).await, Err(JobError::Fatal(_))));
    let row=sqlx::query!("SELECT status::text AS \"status!\",failure_reason,(SELECT count(*) FROM derivatives WHERE photo_id=$1) AS \"count!\" FROM photos WHERE id=$1",id.0).fetch_one(&pool).await.unwrap();
    assert_eq!(row.status, "failed");
    let reason = row.failure_reason.unwrap();
    assert_ne!(reason, "");
    assert!(reason.len() <= 200);
    assert!(!reason.contains('/') && !reason.contains('\\') && !reason.contains('\n'));
    assert_eq!(row.count, 0);
    empty_tmp(root.path()).await;
}
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn video_never_spawns_codec(pool: PgPool) {
    let (root, mut ingest, id) = setup(&pool, "chart-srgb.png").await;
    sqlx::query!("UPDATE photos SET kind='video' WHERE id=$1", id.0)
        .execute(&pool)
        .await
        .unwrap();
    ingest.codec_host = PathBuf::from("/missing/codec");
    ingest.ingest(id).await.unwrap();
    let jobs = sqlx::query!(
        "SELECT kind,priority FROM jobs WHERE payload=$1 ORDER BY kind",
        serde_json::json!({"photo_id":id})
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].kind, "video.transcode");
    empty_tmp(root.path()).await;
}
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn timeout_retries_and_cleans_tmp(pool: PgPool) {
    let (root, mut ingest, id) = setup(&pool, "chart-srgb.png").await;
    ingest.test_delay = Some(Duration::from_secs(10));
    ingest.codec_timeout = Duration::from_millis(100);
    let start = std::time::Instant::now();
    assert!(matches!(
        ingest.ingest(id).await,
        Err(JobError::Retry { .. })
    ));
    assert!(start.elapsed() < Duration::from_secs(2));
    empty_tmp(root.path()).await;
}
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn output_replaced_after_host_exit_is_never_trusted(pool: PgPool) {
    let (root, mut ingest, id) = setup(&pool, "chart-srgb.png").await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let pause = Arc::new(TestPause {
        exited: tx,
        resume: tokio::sync::Notify::new(),
    });
    ingest.test_pause = Some(pause.clone());
    let h = tokio::spawn(async move { ingest.ingest(id).await });
    let dir = rx.recv().await.unwrap();
    let path = dir.join("thumb.avif");
    let mut bytes = tokio::fs::read(&path).await.unwrap();
    bytes[0] ^= 1;
    tokio::fs::write(path, bytes).await.unwrap();
    pause.resume.notify_one();
    assert!(matches!(h.await.unwrap(), Err(JobError::Fatal(_))));
    let count = sqlx::query!(
        "SELECT count(*) AS \"count!\" FROM derivatives WHERE photo_id=$1",
        id.0
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count.count, 0);
    empty_tmp(root.path()).await;
}
