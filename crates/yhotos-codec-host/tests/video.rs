#![forbid(unsafe_code)]
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
//! Actual ffmpeg/ffprobe processes and Linux sandbox controls, without internet fixtures.
use serde_json::{Value, json};
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/video")
        .join(name)
        .canonicalize()
        .unwrap()
}
fn video_binary(name: &str) -> PathBuf {
    std::env::var_os("YHOTOS_TEST_VIDEO_BIN_DIR")
        .map_or_else(
            || {
                let local = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/video-tools");
                if local.join(name).is_file() {
                    local
                } else {
                    PathBuf::from("/usr/bin")
                }
            },
            PathBuf::from,
        )
        .join(name)
        .canonicalize()
        .unwrap()
}
fn invoke(request: &Value, args: &[&str]) -> std::process::Output {
    let mut request = request.clone();
    let job = if args.first() == Some(&"--video-exec") {
        &mut request["job"]
    } else {
        &mut request
    };
    if job["settings"].is_null() {
        job["settings"] = json!({});
    }
    job["settings"]["ffmpeg"] = json!(video_binary("ffmpeg"));
    job["settings"]["ffprobe"] = json!(video_binary("ffprobe"));
    let mut child = Command::new(env!("CARGO_BIN_EXE_yhotos-codec-host"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(request.to_string().as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}
#[test]
fn containers_rotation_renditions_and_poster() {
    for (name, width, height) in [
        ("h264.mp4", 128, 96),
        ("hevc.mov", 128, 96),
        ("rotated.mov", 96, 128),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let output = invoke(
            &json!({"input":fixture(name),"out_dir":dir.path()}),
            &["--video"],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["info"]["width"], width);
        assert_eq!(report["info"]["height"], height);
        for (codec, profile) in [("h264", "High"), ("av1", "Main")] {
            let output = Command::new("ffprobe")
                .args(["-v", "error", "-show_streams", "-of", "json"])
                .arg(dir.path().join(format!("{codec}.mp4")))
                .output()
                .unwrap();
            assert!(output.status.success());
            let probe: Value = serde_json::from_slice(&output.stdout).unwrap();
            let streams = probe["streams"].as_array().unwrap();
            assert_eq!(streams[0]["codec_name"], codec);
            assert_eq!(streams[0]["profile"], profile);
            assert_eq!(streams[0]["width"], width);
            assert_eq!(streams[0]["height"], height);
            assert_eq!(streams[1]["codec_name"], "aac");
            let bytes = std::fs::read(dir.path().join(format!("{codec}.mp4"))).unwrap();
            assert!(
                bytes.windows(4).position(|b| b == b"moov").unwrap()
                    < bytes.windows(4).position(|b| b == b"mdat").unwrap()
            );
        }
        assert!(
            std::fs::metadata(dir.path().join("poster.jpeg"))
                .unwrap()
                .len()
                > 0
        );
        assert_eq!(report["outputs"][3]["width"], 1600);
        let poster = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=width,height",
                "-of",
                "json",
            ])
            .arg(dir.path().join("poster.jpeg"))
            .output()
            .unwrap();
        assert!(poster.status.success());
    }
}
#[test]
fn malformed_and_over_duration_inputs_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("bad.mp4");
    let out = dir.path().join("out");
    std::fs::create_dir(&out).unwrap();
    std::fs::write(&input, b"not a video").unwrap();
    assert_eq!(
        invoke(&json!({"input":input,"out_dir":out}), &["--video"])
            .status
            .code(),
        Some(2)
    );
    assert_eq!(
        invoke(
            &json!({"input":fixture("h264.mp4"),"out_dir":out,"settings":{"max_duration":1}}),
            &["--video"]
        )
        .status
        .code(),
        Some(2)
    );
}
#[test]
fn linux_execute_fork_socket_and_write_policy() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input");
    std::fs::write(&input, b"unparsed").unwrap();
    let out = dir.path().join("out");
    std::fs::create_dir(&out).unwrap();
    std::fs::copy("/bin/true", out.join("other")).unwrap();
    let output = invoke(
        &json!({"job":{"input":input,"out_dir":out,"settings":{"ffmpeg":"/usr/bin/ffmpeg","ffprobe":"/usr/bin/ffprobe","timeout":60,"max_duration":1800,"threads":2},"probe_only":true},"stage":"Probe","info":null}),
        &[
            "--video-exec",
            &std::process::id().to_string(),
            "--video-test",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(out.join("permitted")).unwrap(), b"ok");
    assert!(!dir.path().join("outside-denied").exists());
}
#[cfg(feature = "test-hooks")]
#[test]
fn timeout_kills_the_real_ffmpeg_process_group() {
    for mut signal in [
        None,
        Some(nix::sys::signal::Signal::SIGXCPU),
        Some(nix::sys::signal::Signal::SIGKILL),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_yhotos-codec-host"))
            .args(["--video", "--video-realtime"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(
                json!({"input":fixture("h264.mp4"),"out_dir":dir.path(),"settings":{"timeout":1,"ffmpeg":video_binary("ffmpeg"),"ffprobe":video_binary("ffprobe")}})
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
        let start = std::time::Instant::now();
        let mut parser = None;
        while start.elapsed() < Duration::from_secs(5) {
            if let Ok(children) =
                std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
            {
                for pid in children.split_whitespace() {
                    if std::fs::read_to_string(format!("/proc/{pid}/comm"))
                        .is_ok_and(|s| s.trim() == "ffmpeg")
                    {
                        parser = Some(pid.to_owned());
                        if let Some(signal) = signal.take() {
                            nix::sys::signal::kill(
                                nix::unistd::Pid::from_raw(pid.parse().unwrap()),
                                signal,
                            )
                            .unwrap();
                        }
                    }
                }
            }
            if let Some(status) = child.try_wait().unwrap() {
                assert_eq!(status.code(), Some(4));
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if child.try_wait().unwrap().is_none() {
            let _ = child.kill();
        }
        let _ = child.wait().unwrap();
        assert!(start.elapsed() < Duration::from_secs(5));
        let parser = parser.expect("actual ffmpeg was observed");
        assert!(!Path::new(&format!("/proc/{parser}")).exists());
    }
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn video_job_publishes_sources_poster_sprite_ready_and_inline_playback(pool: sqlx::PgPool) {
    use yhotos_core::{Slug, access::Visibility};
    use yhotos_db::{albums, photos};
    use yhotos_storage::Store;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let original = store
        .put_original(
            tokio::fs::File::open(fixture("rotated.mov")).await.unwrap(),
            300_000,
        )
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let album = albums::create(
        &mut conn,
        None,
        &Slug::new("video").unwrap(),
        "Video",
        Visibility::Public,
    )
    .await
    .unwrap();
    let photo = photos::create(
        &mut conn,
        &photos::NewPhoto {
            album_id: album.id,
            title: "Video fixture".into(),
            original_hash: original.hash.to_string(),
            original_name: "rotated.mov".into(),
            original_bytes: i64::try_from(original.bytes).unwrap(),
            mime: "video/quicktime".into(),
            kind: photos::MediaKind::Video,
        },
    )
    .await
    .unwrap();
    drop(conn);
    let mut ingest = yhotos_pipeline::Ingest::new(
        store,
        pool.clone(),
        env!("CARGO_BIN_EXE_yhotos-codec-host").into(),
    );
    ingest.video.ffmpeg = video_binary("ffmpeg");
    ingest.video.ffprobe = video_binary("ffprobe");
    ingest.ingest(photo.id).await.unwrap();
    let row = sqlx::query!("SELECT priority FROM jobs WHERE kind='video.transcode'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(row.priority < 0);
    ingest.transcode_video(photo.id, 1).await.unwrap();
    let row = sqlx::query!(
        "SELECT status::text AS \"status!\",width,height FROM photos WHERE id=$1",
        photo.id.0
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.status, "ready");
    assert_eq!((row.width, row.height), (Some(96), Some(128)));
    let rows = sqlx::query!(
        "SELECT hash,codec,bytes,duration_ms FROM video_renditions WHERE photo_id=$1",
        photo.id.0
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    for row in rows {
        let hash = row.hash.parse().unwrap();
        let file = ingest
            .store
            .open_read(
                yhotos_storage::Area::Video,
                &hash,
                Some(&yhotos_storage::DerivativeKey {
                    hash: &hash,
                    variant: &row.codec,
                    format: "mp4",
                    hdr: false,
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            file.metadata().await.unwrap().len(),
            u64::try_from(row.bytes).unwrap()
        );
        assert_eq!(row.duration_ms, 1200);
    }
    let mut conn = pool.acquire().await.unwrap();
    let page = photos::page(
        &mut conn,
        &yhotos_core::access::Principal::Anonymous,
        &photo.short_id,
        photos::PageSort::Taken,
    )
    .await
    .unwrap()
    .unwrap();
    drop(conn);
    let html = yhotos_http::pages::render_photo(
        &page,
        photos::PageSort::Taken,
        &yhotos_http::pages::PageContext {
            nonce: "fixture",
            stylesheet: "/static/app.css",
            script: "/static/thumbhash.js",
            public_base_url: "https://gallery.example",
        },
    )
    .unwrap();
    assert!(html.contains("<video controls preload=\"metadata\" playsinline poster="));
    assert!(html.find("/av1.mp4").unwrap() < html.find("/h264.mp4").unwrap());
    assert!(html.contains("/scrub.vtt"));
    assert!(page.photo.derivatives.iter().any(|d| d.variant == "scrub"));
    assert!(page.photo.derivatives.iter().any(|d| d.format == "avif"));
    assert!(page.photo.derivatives.iter().any(|d| d.format == "jxl"));
    ingest.transcode_video(photo.id, 2).await.unwrap();
    let count = sqlx::query!(
        "SELECT count(*) AS \"count!\" FROM video_renditions WHERE photo_id=$1",
        photo.id.0
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count.count, 2);
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn video_timeout_is_retryable_and_never_publishes_partial_outputs(pool: sqlx::PgPool) {
    use std::os::unix::fs::PermissionsExt;
    use yhotos_core::{Slug, access::Visibility};
    use yhotos_db::{albums, photos};
    let dir = tempfile::tempdir().unwrap();
    let store = yhotos_storage::Store::open(dir.path()).unwrap();
    let original = store
        .put_original(
            tokio::fs::File::open(fixture("h264.mp4")).await.unwrap(),
            300_000,
        )
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let album = albums::create(
        &mut conn,
        None,
        &Slug::new("retry-video").unwrap(),
        "Retry",
        Visibility::Public,
    )
    .await
    .unwrap();
    let photo = photos::create(
        &mut conn,
        &photos::NewPhoto {
            album_id: album.id,
            title: "Retry fixture".into(),
            original_hash: original.hash.to_string(),
            original_name: "video.mp4".into(),
            original_bytes: i64::try_from(original.bytes).unwrap(),
            mime: "video/mp4".into(),
            kind: photos::MediaKind::Video,
        },
    )
    .await
    .unwrap();
    drop(conn);
    let path = dir.path().join("fake-host");
    std::fs::write(&path, b"#!/bin/sh\ncat >/dev/null\nexit 4\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let ingest = yhotos_pipeline::Ingest::new(store, pool.clone(), path);
    let result = ingest.transcode_video(photo.id, 1).await;
    assert!(matches!(
        result,
        Err(yhotos_pipeline::JobError::Retry { .. })
    ));
    let row = sqlx::query!(
        "SELECT status::text AS \"status!\" FROM photos WHERE id=$1",
        photo.id.0
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.status, "pending");
    let rows = sqlx::query!(
        "SELECT count(*) AS \"count!\" FROM video_renditions WHERE photo_id=$1",
        photo.id.0
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rows.count, 0);
}

#[test]
fn dynamic_executables_fail_closed_before_parsing() {
    let dir = tempfile::tempdir().unwrap();
    // This Rust host is dynamically linked even in the codec image, unlike the
    // explicitly shipped static ffmpeg/ffprobe parsers.
    let request = json!({"input":fixture("h264.mp4"),"out_dir":dir.path(),"settings":{"ffmpeg":env!("CARGO_BIN_EXE_yhotos-codec-host"),"ffprobe":video_binary("ffprobe")}});
    let mut child = Command::new(env!("CARGO_BIN_EXE_yhotos-codec-host"))
        .arg("--video")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(request.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(output.stdout, [] as [u8; 0]);
    assert_eq!(
        String::from_utf8(output.stderr).unwrap().trim(),
        "video executable requires static linking"
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[sqlx::test(migrator = "yhotos_db::MIGRATOR")]
async fn replacement_queues_a_distinct_video_generation_and_delayed_jobs_skip_it(
    pool: sqlx::PgPool,
) {
    use yhotos_core::{Slug, access::Visibility};
    use yhotos_db::{albums, jobs, photos};
    use yhotos_jobs::{Job, JobCtx, JobHandler, Queue};
    let dir = tempfile::tempdir().unwrap();
    let store = yhotos_storage::Store::open(dir.path()).unwrap();
    let put = store
        .put_original(
            tokio::fs::File::open(fixture("h264.mp4")).await.unwrap(),
            300_000,
        )
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let album = albums::create(
        &mut conn,
        None,
        &Slug::new("video-replacement").unwrap(),
        "Video replacement",
        Visibility::Public,
    )
    .await
    .unwrap();
    let original = photos::NewPhoto {
        album_id: album.id,
        title: "Replacement".into(),
        original_hash: put.hash.to_string(),
        original_name: "video.mp4".into(),
        original_bytes: i64::try_from(put.bytes).unwrap(),
        mime: "video/mp4".into(),
        kind: photos::MediaKind::Video,
    };
    let photo = photos::create(&mut conn, &original).await.unwrap();
    drop(conn);
    let ingest =
        yhotos_pipeline::Ingest::new(store, pool.clone(), dir.path().join("host-must-never-run"));
    let mut conn = pool.acquire().await.unwrap();
    jobs::enqueue_unique(
        &mut conn,
        "video.transcode",
        &json!({"photo_id":photo.id}),
        -1,
        None,
        3,
    )
    .await
    .unwrap();
    drop(conn);
    sqlx::query_file!("../yhotos-db/migrations/20261022192026_video_job_generations.sql")
        .execute(&pool)
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    photos::replace_original(
        &mut conn,
        photo.id,
        &original,
        &photos::PhotoMetadata::default(),
    )
    .await
    .unwrap();
    drop(conn);
    ingest.ingest(photo.id).await.unwrap();
    let (old, _) = jobs::claim(
        &mut pool.acquire().await.unwrap(),
        "old-video",
        &["video.transcode"],
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(
        Job::from_row(&old).unwrap(),
        Job::VideoTranscode { generation: 0, .. }
    ));
    ingest
        .handle(
            &old,
            &JobCtx {
                queue: Queue::new(pool.clone()),
                shutdown: tokio_util::sync::CancellationToken::new(),
            },
        )
        .await
        .unwrap();
    let (new, _) = jobs::claim(
        &mut pool.acquire().await.unwrap(),
        "new-video",
        &["video.transcode"],
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(
        Job::from_row(&new).unwrap(),
        Job::VideoTranscode { generation: 1, .. }
    ));
}
