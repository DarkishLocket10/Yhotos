#![forbid(unsafe_code)]
//! Full video transcode, verified rendition publication and normal poster ingest.
use criterion::Criterion;
use std::{
    error::Error,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use yhotos_core::{Slug, access::Visibility};
use yhotos_db::{DatabaseTls, Db, Role, albums, photos};
use yhotos_pipeline::Ingest;
use yhotos_storage::Store;
fn main() -> Result<(), Box<dyn Error>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    let host = std::env::var_os("YHOTOS_WORKER__CODEC_HOST_PATH")
        .map_or_else(
            || root.join("target/debug/yhotos-codec-host"),
            PathBuf::from,
        )
        .canonicalize()?;
    let tools = std::env::var_os("YHOTOS_TEST_VIDEO_BIN_DIR")
        .map_or_else(|| root.join("target/video-tools"), PathBuf::from)
        .canonicalize()?;
    let runtime = tokio::runtime::Runtime::new()?;
    let _entered = runtime.enter();
    let db = runtime.block_on(Db::connect(
        &std::env::var("DATABASE_URL")?.into(),
        Role::Admin,
        4,
        DatabaseTls::Disable,
    ))?;
    let dir = tempfile::tempdir()?;
    let store = Store::open(dir.path())?;
    let put = runtime.block_on(store.put_original(
        runtime.block_on(tokio::fs::File::open(root.join("fixtures/video/h264.mp4")))?,
        300_000,
    ))?;
    let mut ingest = Ingest::new(store, db.pool().clone(), host);
    ingest.video.ffmpeg = tools.join("ffmpeg");
    ingest.video.ffprobe = tools.join("ffprobe");
    let mut conn = runtime.block_on(db.pool().acquire())?;
    let album = runtime.block_on(albums::create(
        &mut conn,
        None,
        &Slug::new(&format!("video-bench-{}", uuid::Uuid::now_v7()))?,
        "Video benchmark",
        Visibility::Public,
    ))?;
    drop(conn);
    let mut failure = None;
    let mut c = Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(5))
        .configure_from_args();
    c.bench_function("video_pipeline/1_2s_128x96", |b| {
        b.iter_custom(|iterations| {
            let started = Instant::now();
            for _ in 0..iterations {
                let run = runtime.block_on(async {
                    let mut conn = db.pool().acquire().await?;
                    let photo = photos::create(
                        &mut conn,
                        &photos::NewPhoto {
                            album_id: album.id,
                            title: "Video benchmark".into(),
                            original_hash: put.hash.to_string(),
                            original_name: "video.mp4".into(),
                            original_bytes: i64::try_from(put.bytes)?,
                            mime: "video/mp4".into(),
                            kind: photos::MediaKind::Video,
                        },
                    )
                    .await?;
                    drop(conn);
                    ingest.transcode_video(photo.id, 1).await?;
                    Ok::<(), Box<dyn Error>>(())
                });
                if let Err(error) = run {
                    failure = Some(error);
                    break;
                }
            }
            started.elapsed()
        });
    });
    c.final_summary();
    runtime.block_on(async {
        sqlx::query!("DELETE FROM photos WHERE album_id=$1", album.id.0)
            .execute(db.pool())
            .await?;
        sqlx::query!("DELETE FROM albums WHERE id=$1", album.id.0)
            .execute(db.pool())
            .await?;
        Ok::<(), sqlx::Error>(())
    })?;
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(())
}
