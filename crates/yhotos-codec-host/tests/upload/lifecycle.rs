//! A single transition table covers every lifecycle state and destructive action.
use super::*;
use serde_json::{Value, json};
use yhotos_core::{PhotoId, ShortId};
use yhotos_db::{jobs, library, photos};
use yhotos_jobs::{Job, JobCtx, JobHandler, Queue};

async fn command(c: &Client, value: Value) -> Result<library::Change, yhotos_db::DbError> {
    library::execute(&c.pool, c.user, &serde_json::from_value(value).unwrap()).await
}
async fn snapshot(pool: &PgPool, id: PhotoId) -> Option<Value> {
    sqlx::query_scalar!(
        r#"SELECT to_jsonb(p) AS "row!" FROM photos p WHERE id=$1"#,
        id.0
    )
    .fetch_optional(pool)
    .await
    .unwrap()
}
async fn queued(pool: &PgPool) -> Vec<yhotos_jobs::JobRow> {
    let mut rows = Vec::new();
    for status in [
        jobs::JobStatus::Pending,
        jobs::JobStatus::Running,
        jobs::JobStatus::Complete,
        jobs::JobStatus::Failed,
    ] {
        let batch = jobs::list(&mut pool.acquire().await.unwrap(), status, 0)
            .await
            .unwrap();
        for row in &batch {
            Job::from_row(row).unwrap();
        }
        rows.extend(batch);
    }
    rows
}
fn context(pool: &PgPool) -> JobCtx {
    JobCtx {
        queue: Queue::new(pool.clone()),
        shutdown: tokio_util::sync::CancellationToken::new(),
    }
}
fn worker(c: &Client) -> yhotos_pipeline::Ingest {
    let mut ingest = yhotos_pipeline::Ingest::new(
        c.state.store.clone(),
        c.pool.clone(),
        env!("CARGO_BIN_EXE_yhotos-codec-host").into(),
    );
    ingest.plan = yhotos_pipeline::VariantPlan {
        variants: vec![yhotos_pipeline::Variant {
            name: "thumb".into(),
            long_edge: 320,
            square: true,
            formats: vec![yhotos_pipeline::Format::Jpeg],
            scale2x: false,
        }],
    };
    ingest
}
async fn seed(c: &Client, original: &photos::NewPhoto) -> photos::PhotoRow {
    photos::create(&mut c.pool.acquire().await.unwrap(), original)
        .await
        .unwrap()
}
fn original(c: &Client, label: &str) -> photos::NewPhoto {
    photos::NewPhoto {
        album_id: c.album,
        title: label.into(),
        original_hash: blake3::hash(label.as_bytes()).to_hex().to_string(),
        original_name: "fixture.jpg".into(),
        original_bytes: 12,
        mime: "image/jpeg".into(),
        kind: photos::MediaKind::Image,
    }
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
#[allow(clippy::too_many_lines)] // One table checks identical invariants for every transition.
async fn photo_lifecycle_transition_matrix(pool: PgPool) {
    let mut c = Client::new(pool.clone()).await;
    // Inspect the durable outbox before delivery. Drop the HTTP router's worker
    // and use fresh resources without starting a background cache drain.
    let state = AppState::from(yhotos_http::AppResources {
        config: c.state.config,
        pool: pool.clone(),
        auth: c.state.auth.clone(),
        store: c.state.store.clone(),
        files: yhotos_http::files::FileCache::default(),
        cache_purger: c.state.cache_purger.clone(),
    });
    c.app = Router::new();
    c.state = state;
    let place = Uuid::now_v7();
    sqlx::query!(
        "INSERT INTO places(id,name,country,lat,lon) VALUES($1,'Fixture place','CA',43,-79)",
        place
    )
    .execute(&pool)
    .await
    .unwrap();
    let date = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
    for start in [
        "pending",
        "ingesting",
        "published",
        "failed",
        "trashed",
        "replaced",
    ] {
        for op in ["trash", "restore", "purge", "replace"] {
            // Test automatic metadata and explicit overrides (including an explicit empty place).
            for metadata in ["automatic", "manual", "empty"] {
                let p = seed(&c, &original(&c, &format!("{start}-{op}-{metadata}"))).await;
                if start == "replaced" {
                    library::replace(
                        &mut pool.acquire().await.unwrap(),
                        c.user,
                        p.id.0,
                        &original(&c, "previous replacement"),
                    )
                    .await
                    .unwrap();
                }
                let status = match start {
                    "ingesting" => "processing",
                    "published" | "trashed" => "ready",
                    "failed" => "failed",
                    _ => "pending",
                };
                let manual = metadata != "automatic";
                let selected = if metadata == "empty" {
                    None
                } else {
                    Some(place)
                };
                sqlx::query!("UPDATE photos SET status=$2::text::photo_status, failure_reason=CASE WHEN $2='failed' THEN 'fixture failure' END, taken_at=$3,taken_at_local=$3 AT TIME ZONE 'UTC',taken_at_override=CASE WHEN $4 THEN $3 END, gps_lat=43,gps_lon=-79,gps_overridden=$4,place_id=$5,place_overridden=$4,deleted_at=CASE WHEN $6 THEN now() END WHERE id=$1",p.id.0,status,date,manual,selected,start=="trashed").execute(&pool).await.unwrap();
                let has_derivative = matches!(start, "published" | "trashed");
                if has_derivative {
                    sqlx::query!("INSERT INTO derivatives(id,photo_id,variant,format,hdr,hash,width,height,bytes) VALUES($1,$2,'thumb','jpeg',false,$3,1,1,1)",Uuid::now_v7(),p.id.0,p.original_hash).execute(&pool).await.unwrap();
                }
                // The queue at the start reflects the live pending/running generation.
                sqlx::query!("DELETE FROM jobs")
                    .execute(&pool)
                    .await
                    .unwrap();
                let before = snapshot(&pool, p.id).await.unwrap();
                if matches!(start, "pending" | "ingesting" | "replaced") {
                    let id = jobs::enqueue_unique(&mut pool.acquire().await.unwrap(), "ingest", &json!({"photo_id":p.id,"original_hash":before["original_hash"],"generation":before["generation"]}),0,None,3).await.unwrap();
                    if start == "ingesting" {
                        sqlx::query!("UPDATE jobs SET locked_by='matrix-worker',locked_at=now(),attempts=1 WHERE id=$1",id.0).execute(&pool).await.unwrap();
                    }
                }
                let old_jobs = queued(&pool).await;
                let old_purges = library::pending_purges(&mut pool.acquire().await.unwrap())
                    .await
                    .unwrap();
                let result = if op == "replace" {
                    library::replace(
                        &mut pool.acquire().await.unwrap(),
                        c.user,
                        p.id.0,
                        &original(&c, "new bytes"),
                    )
                    .await
                } else {
                    command(
                        &c,
                        if op == "purge" {
                            json!({"op":op,"kind":"photo","id":p.id,"confirmation":"PURGE"})
                        } else {
                            json!({"op":op,"kind":"photo","id":p.id})
                        },
                    )
                    .await
                };
                let valid = match op {
                    "restore" | "purge" => start == "trashed",
                    "replace" => start != "trashed",
                    _ => true,
                };
                assert_eq!(result.is_ok(), valid, "{start} / {op} / {metadata}");
                let after = snapshot(&pool, p.id).await;
                let new_jobs = queued(&pool).await;
                let added: Vec<_> = new_jobs
                    .iter()
                    .filter(|r| !old_jobs.iter().any(|old| old.id == r.id))
                    .map(|r| Job::from_row(r).unwrap())
                    .collect();
                for old in &old_jobs {
                    assert!(
                        new_jobs.contains(old),
                        "{start}/{op}: changed an existing job"
                    );
                }
                assert_eq!(new_jobs.len(), old_jobs.len() + added.len());
                let expected_jobs = if valid && op == "replace" {
                    vec![Job::Ingest {
                        photo_id: p.id,
                        original_hash: original(&c, "new bytes").original_hash.parse().unwrap(),
                        generation: before["generation"].as_i64().unwrap() + 1,
                    }]
                } else if valid && op == "purge" {
                    vec![Job::Gc]
                } else {
                    vec![]
                };
                assert_eq!(added, expected_jobs, "{start} / {op}");
                if valid {
                    let change = result.unwrap();
                    for tag in [
                        "visibility".into(),
                        "map".into(),
                        "calendar".into(),
                        "places".into(),
                        format!("photo:{}", p.id),
                        format!("album:{}", c.album),
                    ] {
                        assert!(change.tags.contains(&tag), "{start}/{op}: missing {tag}");
                    }
                    if selected.is_some() {
                        assert!(change.tags.contains(&format!("place:{place}")));
                    }
                    let persisted = sqlx::query_scalar!(
                        "SELECT tags FROM cache_purges WHERE id=$1",
                        change.purge_id
                    )
                    .fetch_one(&pool)
                    .await
                    .unwrap();
                    assert_eq!(persisted, change.tags);
                    if op == "purge" {
                        assert!(after.is_none());
                    } else {
                        let after = after.unwrap();
                        assert_eq!(after["id"], before["id"]);
                        assert_eq!(after["short_id"], ShortId::from_photo_id(p.id).to_string());
                        assert_eq!(after["deleted_at"].is_null(), op != "trash");
                        assert_eq!(
                            after["status"],
                            if op == "replace" { "pending" } else { status }
                        );
                        for field in [
                            "generation",
                            "original_hash",
                            "place_id",
                            "taken_at",
                            "taken_at_local",
                            "gps_lat",
                            "gps_lon",
                            "failure_reason",
                            "taken_at_override",
                            "gps_overridden",
                            "place_overridden",
                        ] {
                            let expected = if op == "replace" {
                                match field {
                                    "generation" => json!(before[field].as_i64().unwrap() + 1),
                                    "original_hash" => {
                                        json!(original(&c, "new bytes").original_hash)
                                    }
                                    "failure_reason" => Value::Null,
                                    "place_id" | "taken_at" | "taken_at_local" | "gps_lat"
                                    | "gps_lon"
                                        if !manual =>
                                    {
                                        Value::Null
                                    }
                                    _ => before[field].clone(),
                                }
                            } else {
                                before[field].clone()
                            };
                            assert_eq!(after[field], expected, "{start}/{op}/{metadata}: {field}");
                        }
                    }
                } else {
                    assert_eq!(after, Some(before));
                    assert_eq!(old_jobs, new_jobs);
                    assert_eq!(
                        old_purges,
                        library::pending_purges(&mut pool.acquire().await.unwrap())
                            .await
                            .unwrap()
                    );
                }
                let derivatives =
                    sqlx::query_scalar!("SELECT hash FROM derivatives WHERE photo_id=$1", p.id.0)
                        .fetch_all(&pool)
                        .await
                        .unwrap();
                let expected_derivatives =
                    if has_derivative && !(valid && matches!(op, "replace" | "purge")) {
                        vec![p.original_hash.clone()]
                    } else {
                        vec![]
                    };
                assert_eq!(
                    derivatives, expected_derivatives,
                    "{start}/{op}: derivative content"
                );
                // Keep the matrix independent of the outbox page size.
                sqlx::query!("DELETE FROM cache_purges")
                    .execute(&pool)
                    .await
                    .unwrap();
            }
        }
    }
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn trash_worker_restore_requeues_fenced_ingest_for_photo_and_album(pool: PgPool) {
    let c = Client::new(pool.clone()).await;
    let ingest = worker(&c);
    for kind in ["photo", "album"] {
        let bytes = fixture("tiny-1x1.jpg");
        let put = c
            .state
            .store
            .put_original(bytes.as_slice(), bytes.len() as u64)
            .await
            .unwrap();
        let mut source = original(&c, kind);
        source.original_hash = put.hash.to_string();
        source.original_bytes = i64::try_from(bytes.len()).unwrap();
        let p = seed(&c, &source).await;
        let old_job = jobs::enqueue_unique(
            &mut pool.acquire().await.unwrap(),
            "ingest",
            &json!({"photo_id":p.id,"original_hash":put.hash.to_string(),"generation":0}),
            0,
            None,
            3,
        )
        .await
        .unwrap();
        let target = if kind == "album" { c.album.0 } else { p.id.0 };
        command(&c, json!({"op":"trash","kind":kind,"id":target}))
            .await
            .unwrap();
        let (old, claim) = jobs::claim(
            &mut pool.acquire().await.unwrap(),
            "trashed-worker",
            &["ingest"],
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(old.id, old_job);
        assert!(matches!(
            ingest.handle(&old, &context(&pool)).await,
            Err(yhotos_jobs::JobError::Fatal(_))
        ));
        // Model the queue's terminal handling of an unavailable photo.
        sqlx::query!("UPDATE jobs SET finished_at=now(),locked_at=NULL,locked_by=NULL,error='photo unavailable' WHERE id=$1",claim.job_id.0).execute(&pool).await.unwrap();
        command(&c, json!({"op":"restore","kind":kind,"id":target}))
            .await
            .unwrap();
        let restored = snapshot(&pool, p.id).await.unwrap();
        assert_eq!(restored["status"], "pending");
        assert_eq!(restored["generation"], 1);
        // A late old attempt cannot start or fail the restored generation.
        ingest.handle(&old, &context(&pool)).await.unwrap();
        let (new, claim) = jobs::claim(
            &mut pool.acquire().await.unwrap(),
            "restored-worker",
            &["ingest"],
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            Job::from_row(&new).unwrap(),
            Job::Ingest {
                photo_id: p.id,
                original_hash: put.hash,
                generation: 1
            }
        );
        ingest.handle(&new, &context(&pool)).await.unwrap();
        assert!(
            jobs::complete(&mut pool.acquire().await.unwrap(), &claim)
                .await
                .unwrap()
        );
        assert_eq!(snapshot(&pool, p.id).await.unwrap()["status"], "ready");
        assert_eq!(
            sqlx::query_scalar!("SELECT count(*) FROM derivatives WHERE photo_id=$1", p.id.0)
                .fetch_one(&pool)
                .await
                .unwrap(),
            Some(1)
        );
    }
}

async fn replacement_url(c: &Client, photo: PhotoId, bytes: usize) -> String {
    let created = c
        .send(
            "POST",
            "/admin/upload",
            &[
                ("upload-length", bytes.to_string()),
                ("upload-replace-photo", photo.to_string()),
                (
                    "upload-metadata",
                    format!(
                        "filename {},album {},filetype {}",
                        STANDARD.encode("replacement.jpg"),
                        STANDARD.encode(c.album.to_string()),
                        STANDARD.encode("image/jpeg")
                    ),
                ),
            ],
            Body::empty(),
        )
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    created.headers()["location"].to_str().unwrap().to_owned()
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn stalled_replacement_body_allows_library_edits_and_revalidates_target(pool: PgPool) {
    let c = Arc::new(Client::new(pool.clone()).await);
    for delete_target in [false, true] {
        let p = seed(&c, &original(&c, "stalled upload")).await;
        let bytes = fixture("tiny-1x1.jpg");
        let path = replacement_url(&c, p.id, bytes.len()).await;
        let (started, reading) = tokio::sync::oneshot::channel();
        let (release, waiting) = tokio::sync::oneshot::channel();
        let uploader = c.clone();
        let patch = tokio::spawn(async move {
            uploader
                .send(
                    "PATCH",
                    &path,
                    &[
                        ("content-type", "application/offset+octet-stream".into()),
                        ("upload-offset", "0".into()),
                    ],
                    Body::from_stream(GatedChunk {
                        bytes: Some(bytes.into()),
                        started: Some(started),
                        release: waiting,
                    }),
                )
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), reading)
            .await
            .unwrap()
            .unwrap();
        // Even an edit to the destination album and a photo in it must finish
        // inside the bulk-operation budget while the upload body is stalled.
        let edits = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            command(&c,json!({"op":"album_edit","id":c.album,"title":"Edited during streaming","description":"","visibility":"public","manual_order":false,"grants":[]})).await.unwrap();
            command(&c,json!({"op":"bulk","ids":[p.id],"action":{"kind":"pattern","title":"Concurrent edit","description":""}})).await.unwrap();
            if delete_target { command(&c,json!({"op":"trash","kind":"photo","id":p.id})).await.unwrap(); }
        }).await;
        release.send(()).unwrap();
        edits.expect("library edits blocked on the stalled replacement body");
        let response = tokio::time::timeout(std::time::Duration::from_secs(10), patch)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            response.status(),
            if delete_target {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::NO_CONTENT
            }
        );
        let row = snapshot(&pool, p.id).await.unwrap();
        assert_eq!(row["title"], "Concurrent edit");
        assert_eq!(row["generation"], i64::from(!delete_target));
        assert_eq!(row["deleted_at"].is_null(), !delete_target);
    }
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn replacing_geotagged_original_with_undated_no_gps_file_clears_automatic_metadata(
    pool: PgPool,
) {
    let c = Client::new(pool.clone()).await;
    let p = seed(&c, &original(&c, "geotagged")).await;
    let place = Uuid::now_v7();
    sqlx::query!(
        "INSERT INTO places(id,name,country,lat,lon) VALUES($1,'Old place','CA',43,-79)",
        place
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query!("UPDATE photos SET gps_lat=43,gps_lon=-79,place_id=$2,taken_at=now(),taken_at_local=localtimestamp WHERE id=$1",p.id.0,place).execute(&pool).await.unwrap();
    let bytes = fixture("tiny-1x1.jpg");
    let path = replacement_url(&c, p.id, bytes.len()).await;
    assert_eq!(
        c.patch(&path, 0, &bytes).await.status(),
        StatusCode::NO_CONTENT
    );
    let ingest = worker(&c);
    let (job, claim) = jobs::claim(
        &mut pool.acquire().await.unwrap(),
        "new-original",
        &["ingest"],
    )
    .await
    .unwrap()
    .unwrap();
    ingest.handle(&job, &context(&pool)).await.unwrap();
    jobs::complete(&mut pool.acquire().await.unwrap(), &claim)
        .await
        .unwrap();
    yhotos_db::places::reverse(&mut pool.acquire().await.unwrap(), p.id)
        .await
        .unwrap();
    let row = snapshot(&pool, p.id).await.unwrap();
    assert_eq!(row["status"], "ready");
    for field in [
        "gps_lat",
        "gps_lon",
        "place_id",
        "taken_at",
        "taken_at_local",
    ] {
        assert!(row[field].is_null(), "retained {field}: {}", row[field]);
    }
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn restore_restarts_each_unfinished_state_without_reviving_independent_trash(pool: PgPool) {
    let c = Client::new(pool.clone()).await;
    for status in ["pending", "processing", "failed"] {
        for kind in ["photo", "album"] {
            let p = seed(&c, &original(&c, status)).await;
            let independent = seed(&c, &original(&c, "independent trash")).await;
            sqlx::query!(
                "UPDATE photos SET deleted_at=now()-interval '1 day' WHERE id=$1",
                independent.id.0
            )
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query!("UPDATE photos SET status=$2::text::photo_status,failure_reason=CASE WHEN $2='failed' THEN 'fixture failure' END WHERE id=$1",p.id.0,status).execute(&pool).await.unwrap();
            let target = if kind == "photo" { p.id.0 } else { c.album.0 };
            command(&c, json!({"op":"trash","kind":kind,"id":target}))
                .await
                .unwrap();
            command(&c, json!({"op":"restore","kind":kind,"id":target}))
                .await
                .unwrap();
            let row = snapshot(&pool, p.id).await.unwrap();
            assert_eq!(row["status"], "pending");
            assert_eq!(row["generation"], 1);
            assert!(row["deleted_at"].is_null());
            assert!(row["failure_reason"].is_null());
            let batch = queued(&pool).await;
            assert_eq!(
                batch
                    .iter()
                    .filter(|job| Job::from_row(job).unwrap()
                        == Job::Ingest {
                            photo_id: p.id,
                            original_hash: p.original_hash.parse().unwrap(),
                            generation: 1,
                        })
                    .count(),
                1
            );
            assert!(!snapshot(&pool, independent.id).await.unwrap()["deleted_at"].is_null());
            assert!(
                !batch
                    .iter()
                    .any(|job| job.payload["photo_id"] == json!(independent.id))
            );
        }
    }
}
