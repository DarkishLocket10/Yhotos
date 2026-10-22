use sqlx::PgPool;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use yhotos_core::{PhotoId, Slug, access::Visibility};
use yhotos_db::{albums, photos};
use yhotos_http::pages::{PageContext, render_album, render_photo};
use yhotos_jobs::{Job, JobCtx, JobHandler, JobKind, JobRow, Queue, QueueConfig};
use yhotos_pipeline::{Ingest, JobError, TestPause, VariantPlan};
use yhotos_storage::{ContentHash, DerivativeKey, Store};

mod fidelity;

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
        let (root, mut ingest, id) = setup(&pool, &name).await;
        ingest.default_timezone = jiff::tz::TimeZone::get("America/Toronto").unwrap();
        ingest.ingest(id).await.unwrap();
        assert_ready(&pool, &ingest, id).await;
        if name.starts_with("orientation-") {
            assert_oriented_pages(&pool, id, &name).await;
        }
        if name == "exif-heavy.jpg" {
            let row=sqlx::query!("SELECT camera_make,camera_model,lens,focal_mm,aperture,shutter,iso,gps_lat,gps_lon,taken_at,taken_at_local FROM photos WHERE id=$1",id.0).fetch_one(&pool).await.unwrap();
            assert_eq!(
                row.taken_at,
                Some(time::macros::datetime!(2026-01-02 08:04:05 UTC))
            );
            assert_eq!(
                row.taken_at_local,
                Some(time::macros::datetime!(2026-01-02 03:04:05))
            );
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

async fn assert_oriented_pages(pool: &PgPool, id: PhotoId, name: &str) {
    let source = yhotos_codec::identify(&fixture(name), &yhotos_codec::Limits::default()).unwrap();
    let expected = if source.orientation >= 5 {
        (source.height.unwrap(), source.width.unwrap())
    } else {
        (source.width.unwrap(), source.height.unwrap())
    };
    assert_ne!(
        expected.0, expected.1,
        "orientation fixture must be non-square"
    );
    let row = sqlx::query!(
        "SELECT short_id,width,height,orientation,original_orientation FROM photos WHERE id=$1",
        id.0
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        (row.width, row.height),
        (
            Some(i32::try_from(expected.0).unwrap()),
            Some(i32::try_from(expected.1).unwrap())
        )
    );
    assert_eq!(row.orientation, Some(1));
    assert_eq!(
        row.original_orientation,
        Some(i32::from(source.orientation))
    );
    let mut conn = pool.acquire().await.unwrap();
    let principal = yhotos_core::access::Principal::Anonymous;
    let sort = photos::PageSort::Taken;
    let page = photos::page(&mut conn, &principal, &row.short_id.parse().unwrap(), sort)
        .await
        .unwrap()
        .unwrap();
    let path = page
        .chain
        .last()
        .unwrap()
        .path
        .split('/')
        .map(|s| Slug::new(s).unwrap())
        .collect::<Vec<_>>();
    let album = albums::page(&mut conn, &principal, &path, 1, sort)
        .await
        .unwrap()
        .unwrap();
    let context = PageContext {
        nonce: "test-nonce",
        stylesheet: "/static/test.css",
        script: "/static/test.js",
        public_base_url: "https://gallery.example",
    };
    for html in [
        render_photo(&page, sort, &context).unwrap(),
        render_album(&album, 1, sort, &context).unwrap(),
    ] {
        let picture = html
            .split("<picture>")
            .nth(1)
            .unwrap()
            .split("</picture>")
            .next()
            .unwrap();
        let img = picture
            .split("<img ")
            .nth(1)
            .unwrap()
            .split('>')
            .next()
            .unwrap();
        assert!(
            img.contains(&format!("width=\"{}\"", expected.0)),
            "{name}: {img}"
        );
        assert!(
            img.contains(&format!("height=\"{}\"", expected.1)),
            "{name}: {img}"
        );
        assert!(img.contains("src=\"/i/"), "{name}: {img}");
        assert!(
            picture.contains("<source ") && picture.contains("srcset=\"/i/"),
            "{name}: {picture}"
        );
    }
}
#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn packed_png_depths_complete_the_production_ingest(pool: PgPool) {
    for bits in [1, 2, 4] {
        let name = format!("{bits}bit.png");
        let identity =
            yhotos_codec::identify(&fixture(&name), &yhotos_codec::Limits::default()).unwrap();
        assert_eq!(identity.bits, bits);
        let (root, ingest, id) = setup(&pool, &name).await;
        ingest.ingest(id).await.unwrap();
        assert_ready(&pool, &ingest, id).await;
        empty_tmp(root.path()).await;
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

async fn ingest_job(pool: &PgPool, photo_id: PhotoId) -> Job {
    let row = sqlx::query!(
        "SELECT original_hash,generation FROM photos WHERE id=$1",
        photo_id.0
    )
    .fetch_one(pool)
    .await
    .unwrap();
    Job::Ingest {
        photo_id,
        original_hash: row.original_hash.parse().unwrap(),
        generation: row.generation,
    }
}

async fn replace(pool: &PgPool, ingest: &Ingest, id: PhotoId, name: &str) -> Job {
    let input = tokio::fs::File::open(fixture(name)).await.unwrap();
    let original = ingest.store.put_original(input, 2 << 30).await.unwrap();
    let album = sqlx::query!("SELECT album_id FROM photos WHERE id=$1", id.0)
        .fetch_one(pool)
        .await
        .unwrap();
    photos::replace_original(
        &mut pool.acquire().await.unwrap(),
        id,
        &photos::NewPhoto {
            album_id: yhotos_core::AlbumId(album.album_id),
            title: "Replacement".into(),
            original_hash: original.hash.to_string(),
            original_name: "replacement.jpg".into(),
            original_bytes: i64::try_from(original.bytes).unwrap(),
            mime: "image/jpeg".into(),
            kind: photos::MediaKind::Image,
        },
        &photos::PhotoMetadata {
            title: "Replacement".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    ingest_job(pool, id).await
}

async fn assert_replacement_pending(pool: &PgPool, id: PhotoId, job: &Job) {
    let photo = sqlx::query!("SELECT status::text AS \"status!\",failure_reason,derivative_plan,(SELECT count(*) FROM derivatives WHERE photo_id=$1) AS \"count!\" FROM photos WHERE id=$1",id.0).fetch_one(pool).await.unwrap();
    assert_eq!(photo.status, "pending");
    assert!(photo.failure_reason.is_none() && photo.derivative_plan.is_none());
    assert_eq!(photo.count, 0);
    let value = serde_json::to_value(job).unwrap();
    let rows = sqlx::query!(
        "SELECT attempts,locked_by,finished_at FROM jobs WHERE kind='ingest' AND payload=$1",
        value["payload"]
    )
    .fetch_all(pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].attempts, 0);
    assert!(rows[0].locked_by.is_none() && rows[0].finished_at.is_none());
}

struct AfterPublication {
    ingest: Ingest,
    published: tokio::sync::mpsc::Sender<()>,
    resume: Arc<tokio::sync::Notify>,
    fail: bool,
}
#[async_trait::async_trait]
impl JobHandler for AfterPublication {
    async fn handle(&self, row: &JobRow, ctx: &JobCtx) -> Result<(), JobError> {
        self.ingest.handle(row, ctx).await?;
        self.published.send(()).await.unwrap();
        self.resume.notified().await;
        if self.fail {
            Err(JobError::Fatal("completion failed".into()))
        } else {
            Ok(())
        }
    }
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn replacement_between_publication_and_completion_survives_old_success_and_failure(
    pool: PgPool,
) {
    for (fail, name) in [false, true]
        .into_iter()
        .flat_map(|fail| ["chart-srgb.png", "orientation-6.jpg"].map(|name| (fail, name)))
    {
        let (root, ingest, id) = setup(&pool, "chart-srgb.png").await;
        let queue = Queue::with_config(
            pool.clone(),
            QueueConfig {
                concurrency: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let old_job = ingest_job(&pool, id).await;
        let old = queue.enqueue(old_job.clone(), 0, None).await.unwrap();
        let (published, mut ready) = tokio::sync::mpsc::channel(1);
        let resume = Arc::new(tokio::sync::Notify::new());
        let handler = AfterPublication {
            ingest: ingest.clone(),
            published,
            resume: resume.clone(),
            fail,
        };
        let shutdown = CancellationToken::new();
        let cancel = shutdown.clone();
        let worker = tokio::spawn(async move {
            queue
                .run_worker("replacement", &[JobKind::Ingest], handler, cancel)
                .await
        });
        tokio::time::timeout(Duration::from_secs(30), ready.recv())
            .await
            .unwrap()
            .unwrap();
        assert_ready(&pool, &ingest, id).await;
        let new_job = replace(&pool, &ingest, id, name).await;
        let new = Queue::new(pool.clone())
            .enqueue(new_job.clone(), 0, None)
            .await
            .unwrap();
        assert_ne!(old, new);
        let running = sqlx::query!("SELECT locked_by,finished_at FROM jobs WHERE id=$1", old.0)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(running.locked_by.is_some() && running.finished_at.is_none());
        assert_replacement_pending(&pool, id, &new_job).await;
        let before = sqlx::query!("SELECT updated_at FROM photos WHERE id=$1", id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
        shutdown.cancel();
        resume.notify_one();
        worker.await.unwrap().unwrap();
        assert_replacement_pending(&pool, id, &new_job).await;
        let after = sqlx::query!("SELECT updated_at FROM photos WHERE id=$1", id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(before.updated_at, after.updated_at);
        let old_row = sqlx::query!("SELECT finished_at,error FROM jobs WHERE id=$1", old.0)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(old_row.finished_at.is_some());
        assert_eq!(old_row.error.is_some(), fail);
        // Even an explicitly delayed old envelope is a no-op for the replacement.
        let (row, token) = yhotos_db::jobs::claim(
            &mut pool.acquire().await.unwrap(),
            "new original",
            &["ingest"],
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(row.id, new);
        let mut stale = row.clone();
        stale.payload = serde_json::to_value(old_job).unwrap()["payload"].clone();
        let ctx = JobCtx {
            queue: Queue::new(pool.clone()),
            shutdown: CancellationToken::new(),
        };
        ingest.handle(&stale, &ctx).await.unwrap();
        assert_replacement_status_pending(&pool, id).await;
        ingest.handle(&row, &ctx).await.unwrap();
        yhotos_db::jobs::complete(&mut pool.acquire().await.unwrap(), &token)
            .await
            .unwrap();
        assert_ready(&pool, &ingest, id).await;
        empty_tmp(root.path()).await;
    }
}

async fn assert_replacement_status_pending(pool: &PgPool, id: PhotoId) {
    let row = sqlx::query!(
        "SELECT status::text AS \"status!\",failure_reason FROM photos WHERE id=$1",
        id.0
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(row.status, "pending");
    assert!(row.failure_reason.is_none());
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn dated_to_undated_replacement_clears_capture_metadata_and_increments_every_generation(
    pool: PgPool,
) {
    let (root, ingest, id) = setup(&pool, "exif-heavy.jpg").await;
    ingest.ingest(id).await.unwrap();
    let before = sqlx::query!(
        "SELECT taken_at,taken_at_local FROM photos WHERE id=$1",
        id.0
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(before.taken_at.is_some() && before.taken_at_local.is_some());
    for generation in 1..=2 {
        let job = replace(&pool, &ingest, id, "orientation-6.jpg").await;
        assert!(matches!(job, Job::Ingest { generation: actual, .. } if actual == generation));
        assert_replacement_pending(&pool, id, &job).await;
        let row = sqlx::query!("SELECT taken_at,taken_at_local,original_orientation,camera_make,camera_model,lens,focal_mm,aperture,shutter,iso,gps_lat,gps_lon,place_id FROM photos WHERE id=$1", id.0).fetch_one(&pool).await.unwrap();
        assert!(row.taken_at.is_none() && row.taken_at_local.is_none());
        assert!(row.original_orientation.is_none());
        assert!(row.camera_make.is_none() && row.camera_model.is_none() && row.lens.is_none());
        assert!(
            row.focal_mm.is_none()
                && row.aperture.is_none()
                && row.shutter.is_none()
                && row.iso.is_none()
        );
        assert!(row.gps_lat.is_none() && row.gps_lon.is_none() && row.place_id.is_none());
        ingest.ingest(id).await.unwrap();
        let after = sqlx::query!(
            "SELECT status::text AS \"status!\",taken_at,taken_at_local FROM photos WHERE id=$1",
            id.0
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(after.status, "ready");
        assert!(after.taken_at.is_none() && after.taken_at_local.is_none());
    }
    empty_tmp(root.path()).await;
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn replacement_before_publication_survives_old_publication_and_verification_failure(
    pool: PgPool,
) {
    for (corrupt, name) in [false, true]
        .into_iter()
        .flat_map(|corrupt| ["chart-srgb.png", "orientation-6.jpg"].map(|name| (corrupt, name)))
    {
        let (root, mut ingest, id) = setup(&pool, "chart-srgb.png").await;
        let (exited, mut rx) = tokio::sync::mpsc::channel(1);
        let pause = Arc::new(TestPause {
            exited,
            resume: tokio::sync::Notify::new(),
        });
        ingest.test_pause = Some(pause.clone());
        let old_job = ingest_job(&pool, id).await;
        let queue = Queue::with_config(
            pool.clone(),
            QueueConfig {
                concurrency: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let old = queue.enqueue(old_job, 0, None).await.unwrap();
        let handler = ingest.clone();
        let shutdown = CancellationToken::new();
        let cancel = shutdown.clone();
        let worker = tokio::spawn(async move {
            queue
                .run_worker("before publication", &[JobKind::Ingest], handler, cancel)
                .await
        });
        let dir = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let new_job = replace(&pool, &ingest, id, name).await;
        if corrupt {
            let path = dir.join("thumb.avif");
            let mut bytes = tokio::fs::read(&path).await.unwrap();
            bytes[0] ^= 1;
            tokio::fs::write(path, bytes).await.unwrap();
        }
        shutdown.cancel();
        pause.resume.notify_one();
        worker.await.unwrap().unwrap();
        assert_replacement_pending(&pool, id, &new_job).await;
        let row = sqlx::query!("SELECT finished_at,error FROM jobs WHERE id=$1", old.0)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(row.finished_at.is_some());
        assert_eq!(row.error.is_some(), corrupt);
        empty_tmp(root.path()).await;
        // Leave no earlier replacement job for the next loop's worker to claim.
        sqlx::query!(
            "DELETE FROM jobs WHERE payload->>'photo_id'=$1",
            id.to_string()
        )
        .execute(&pool)
        .await
        .unwrap();
    }
}

// Rebuild only the EXIF APP1 segment of a committed JPEG. This deliberately
// small TIFF contains DateTimeOriginal and OffsetTimeOriginal in its EXIF IFD.
fn jpeg_with_capture_offset() -> Vec<u8> {
    let source = std::fs::read(fixture("orientation-1.jpg")).unwrap();
    let mut exif = b"Exif\0\0II\x2a\0\x08\0\0\0".to_vec();
    exif.extend(1_u16.to_le_bytes());
    for n in [0x8769_u16, 4] {
        exif.extend(n.to_le_bytes());
    }
    for n in [1_u32, 26, 0] {
        exif.extend(n.to_le_bytes());
    }
    exif.extend(2_u16.to_le_bytes());
    for (tag, count, offset) in [(0x9003_u16, 20_u32, 56_u32), (0x9011, 7, 76)] {
        exif.extend(tag.to_le_bytes());
        exif.extend(2_u16.to_le_bytes());
        exif.extend(count.to_le_bytes());
        exif.extend(offset.to_le_bytes());
    }
    exif.extend(0_u32.to_le_bytes());
    exif.extend(b"2026:01:02 03:04:05\0+05:30\0");
    let mut jpeg = vec![0xff, 0xd8, 0xff, 0xe1];
    jpeg.extend(u16::try_from(exif.len() + 2).unwrap().to_be_bytes());
    jpeg.extend(exif);
    let mut at = 2;
    while at < source.len() {
        assert_eq!(source[at], 0xff);
        if source[at + 1] == 0xda {
            jpeg.extend(&source[at..]);
            break;
        }
        let length = usize::from(u16::from_be_bytes([source[at + 2], source[at + 3]]));
        let end = at + 2 + length;
        if source[at + 1] != 0xe1 || !source[at + 4..end].starts_with(b"Exif\0\0") {
            jpeg.extend(&source[at..end]);
        }
        at = end;
    }
    jpeg
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn explicit_exif_capture_offset_is_stored_and_overrides_default_timezone(pool: PgPool) {
    let (root, mut ingest, id) = setup(&pool, "orientation-1.jpg").await;
    let bytes = jpeg_with_capture_offset();
    let original = ingest
        .store
        .put_original(&bytes[..], 2 << 30)
        .await
        .unwrap();
    sqlx::query!(
        "UPDATE photos SET original_hash=$2,original_bytes=$3 WHERE id=$1",
        id.0,
        original.hash.to_string(),
        i64::try_from(original.bytes).unwrap()
    )
    .execute(&pool)
    .await
    .unwrap();
    ingest.default_timezone = jiff::tz::TimeZone::get("America/Toronto").unwrap();
    ingest.ingest(id).await.unwrap();
    let row = sqlx::query!(
        "SELECT taken_at,taken_at_local FROM photos WHERE id=$1",
        id.0
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        row.taken_at,
        Some(time::macros::datetime!(2026-01-01 21:34:05 UTC))
    );
    assert_eq!(
        row.taken_at_local,
        Some(time::macros::datetime!(2026-01-02 03:04:05))
    );
    empty_tmp(root.path()).await;
}
