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
    let url = created.headers()["location"].to_str().unwrap().to_owned();
    let head = c.send("HEAD", &url, &[], Body::empty()).await;
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(head.headers()["upload-replace-photo"], photo.to_string());
    assert_eq!(head.headers()["upload-length"], bytes.to_string());
    assert_eq!(head.headers()["cache-control"], "no-store");
    url
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

/// A purge callback reads through another connection, exactly as a public cache refill would.
struct CommittedPlacePurger {
    pool: PgPool,
    photo: PhotoId,
    expected: Value,
    calls: std::sync::Mutex<Vec<Vec<String>>>,
}
impl yhotos_http::admin::cache::CachePurger for CommittedPlacePurger {
    fn purge<'a>(
        &'a self,
        tags: &'a [String],
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<(), yhotos_http::error::HttpError>> + Send + 'a,
        >,
    > {
        Box::pin(async move {
            assert_eq!(
                snapshot(&self.pool, self.photo).await,
                Some(self.expected.clone())
            );
            self.calls.lock().unwrap().push(tags.to_vec());
            Ok(())
        })
    }
}
async fn deliver_place_purges(
    c: &Client,
    id: PhotoId,
    expected: &Value,
    places: &[Uuid],
    count: usize,
) {
    use yhotos_http::admin::cache::CachePurger;
    let mock = CommittedPlacePurger {
        pool: c.pool.clone(),
        photo: id,
        expected: expected.clone(),
        calls: std::sync::Mutex::default(),
    };
    let batches = library::pending_purges(&mut c.pool.acquire().await.unwrap())
        .await
        .unwrap();
    assert_eq!(batches.len(), count);
    for (purge, tags) in batches {
        for tag in library::aggregate_tags()
            .into_iter()
            .chain([format!("photo:{id}"), format!("album:{}", c.album)])
            .chain(places.iter().map(|p| format!("place:{p}")))
        {
            assert!(tags.contains(&tag), "missing {tag} in {tags:?}");
        }
        mock.purge(&tags).await.unwrap();
        library::purge_done(&mut c.pool.acquire().await.unwrap(), purge)
            .await
            .unwrap();
    }
    assert_eq!(mock.calls.lock().unwrap().len(), count);
}
async fn commit_place_command(c: &Client, photo: PhotoId, command: Value) -> Value {
    let before = snapshot(&c.pool, photo).await.unwrap();
    let mut tx = c.pool.begin().await.unwrap();
    library::execute_in(&mut tx, c.user, &serde_json::from_value(command).unwrap())
        .await
        .unwrap();
    // Neither data nor purge may escape a savepoint before the containing transaction commits.
    assert_eq!(snapshot(&c.pool, photo).await, Some(before));
    assert_eq!(
        library::pending_purges(&mut c.pool.acquire().await.unwrap())
            .await
            .unwrap(),
        Vec::new()
    );
    tx.commit().await.unwrap();
    snapshot(&c.pool, photo).await.unwrap()
}
fn assert_place(row: &Value, place: Option<Uuid>, overridden: bool) {
    assert_eq!(row["place_id"], json!(place));
    assert_eq!(row["place_overridden"], json!(overridden));
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
#[allow(clippy::too_many_lines)] // Cross every place state with every place-changing transition.
async fn place_lifecycle_transition_matrix_and_commit_before_every_purge(pool: PgPool) {
    let mut c = Client::new(pool.clone()).await;
    // No background drain: deliver every durable batch explicitly through the observing mock.
    c.app = Router::new();
    c.state = AppState::from(yhotos_http::AppResources {
        config: c.state.config,
        pool: pool.clone(),
        auth: c.state.auth.clone(),
        store: c.state.store.clone(),
        files: yhotos_http::files::FileCache::default(),
        cache_purger: c.state.cache_purger.clone(),
    });
    sqlx::query!("INSERT INTO gazetteer(geonameid,name,admin1,country,lat,lon,population) VALUES(1,'Old city','Ontario','CA',43.7,-79.4,100000),(2,'New city','Region','CA',10,20,100000)").execute(&pool).await.unwrap();
    let old = Uuid::now_v7();
    let new = Uuid::now_v7();
    sqlx::query!("INSERT INTO places(id,geonameid,name,country,lat,lon) VALUES($1,1,'Old city','CA',43.7,-79.4),($2,2,'New city','CA',10,20)",old,new).execute(&pool).await.unwrap();
    let ingest = worker(&c);
    let bytes = fixture("tiny-1x1.jpg"); // This replacement has no GPS.
    let put = c
        .state
        .store
        .put_original(bytes.as_slice(), bytes.len() as u64)
        .await
        .unwrap();
    for state in ["none", "automatic", "manual", "empty"] {
        for manual_gps in [false, true] {
            for op in [
                "edit_keep",
                "edit_gps",
                "edit_clear_gps",
                "edit_automatic_place",
                "edit_automatic_gps",
                "replace",
                "geocode",
                "trash_restore",
            ] {
                let mut source = original(&c, &format!("{state}-{manual_gps}-{op}"));
                if op == "edit_automatic_gps" {
                    source.original_hash = put.hash.to_string();
                    source.original_bytes = i64::try_from(bytes.len()).unwrap();
                }
                let p = seed(&c, &source).await;
                let overridden = matches!(state, "manual" | "empty");
                let selected = matches!(state, "automatic" | "manual").then_some(old);
                sqlx::query!("UPDATE photos SET status='ready',gps_lat=43.7,gps_lon=-79.4,gps_overridden=$2,place_id=$3,place_overridden=$4 WHERE id=$1",p.id.0,manual_gps,selected,overridden).execute(&pool).await.unwrap();
                let mut expected_place = selected;
                let mut expected_override = overridden;
                let mut located = true;
                if op.starts_with("edit_") {
                    let mut edit = json!({"op":"photo_edit","id":p.id,"title":"Edited","description":"","visibility":"inherit","tags":[],"grants":[]});
                    if op == "edit_gps" || op == "edit_clear_gps" {
                        edit["gps_mode"] = json!("manual");
                        if op == "edit_gps" {
                            edit["gps_lat"] = json!(10);
                            edit["gps_lon"] = json!(20);
                        } else {
                            located = false;
                        }
                    }
                    if op == "edit_automatic_gps" {
                        edit["gps_mode"] = json!("automatic");
                        located = false;
                    }
                    if op == "edit_automatic_place" {
                        edit["place_mode"] = json!("automatic");
                        expected_place = None;
                        expected_override = false;
                    }
                    let row = commit_place_command(&c, p.id, edit).await;
                    assert_place(&row, expected_place, expected_override);
                    deliver_place_purges(
                        &c,
                        p.id,
                        &row,
                        &selected.into_iter().collect::<Vec<_>>(),
                        1,
                    )
                    .await;
                } else if op == "replace" {
                    let before = snapshot(&pool, p.id).await.unwrap();
                    let mut tx = pool.begin().await.unwrap();
                    let mut replacement = original(&c, "GPS-free replacement");
                    replacement.original_hash = put.hash.to_string();
                    replacement.original_bytes = i64::try_from(bytes.len()).unwrap();
                    library::replace(&mut tx, c.user, p.id.0, &replacement)
                        .await
                        .unwrap();
                    assert_eq!(snapshot(&pool, p.id).await, Some(before));
                    assert_eq!(
                        library::pending_purges(&mut pool.acquire().await.unwrap())
                            .await
                            .unwrap(),
                        Vec::new()
                    );
                    tx.commit().await.unwrap();
                    if !overridden {
                        expected_place = None;
                    }
                    located = manual_gps;
                    let row = snapshot(&pool, p.id).await.unwrap();
                    assert_place(&row, expected_place, expected_override);
                    deliver_place_purges(
                        &c,
                        p.id,
                        &row,
                        &selected.into_iter().collect::<Vec<_>>(),
                        1,
                    )
                    .await;
                } else if op == "trash_restore" {
                    let row = commit_place_command(
                        &c,
                        p.id,
                        json!({"op":"trash","kind":"photo","id":p.id}),
                    )
                    .await;
                    assert_place(&row, selected, overridden);
                    deliver_place_purges(
                        &c,
                        p.id,
                        &row,
                        &selected.into_iter().collect::<Vec<_>>(),
                        1,
                    )
                    .await;
                    yhotos_db::places::reverse(&mut pool.acquire().await.unwrap(), p.id)
                        .await
                        .unwrap();
                    assert_eq!(snapshot(&pool, p.id).await, Some(row.clone()));
                    deliver_place_purges(&c, p.id, &row, &[], 0).await;
                    let row = commit_place_command(
                        &c,
                        p.id,
                        json!({"op":"restore","kind":"photo","id":p.id}),
                    )
                    .await;
                    assert_place(&row, selected, overridden);
                    deliver_place_purges(
                        &c,
                        p.id,
                        &row,
                        &selected.into_iter().collect::<Vec<_>>(),
                        1,
                    )
                    .await;
                }
                if op == "replace" || op == "edit_automatic_gps" {
                    ingest.ingest(p.id).await.unwrap();
                    let row = snapshot(&pool, p.id).await.unwrap();
                    assert_place(&row, expected_place, expected_override);
                    assert_eq!(
                        row["gps_lat"],
                        if located { json!(43.7) } else { Value::Null }
                    );
                    let geo = sqlx::query_scalar!("SELECT EXISTS(SELECT FROM jobs WHERE kind='geo.reverse' AND payload->>'photo_id'=$1) AS \"queued!\"",p.id.to_string()).fetch_one(&pool).await.unwrap();
                    assert_eq!(
                        geo,
                        (located || expected_place.is_some()) && !expected_override,
                        "{state}/{manual_gps}: effective GPS must schedule geocoding"
                    );
                    deliver_place_purges(
                        &c,
                        p.id,
                        &row,
                        &expected_place.into_iter().collect::<Vec<_>>(),
                        1,
                    )
                    .await;
                }
                // Deliberately complete geocoding only AFTER the edit/replacement purge was delivered.
                let before = snapshot(&pool, p.id).await.unwrap();
                let mut tx = pool.begin().await.unwrap();
                yhotos_db::places::reverse(&mut tx, p.id).await.unwrap();
                assert_eq!(snapshot(&pool, p.id).await, Some(before));
                assert_eq!(
                    library::pending_purges(&mut pool.acquire().await.unwrap())
                        .await
                        .unwrap(),
                    Vec::new()
                );
                tx.commit().await.unwrap();
                let resolved = if expected_override {
                    expected_place
                } else if !located {
                    None
                } else if op == "edit_gps" {
                    Some(new)
                } else {
                    Some(old)
                };
                let row = snapshot(&pool, p.id).await.unwrap();
                assert_place(&row, resolved, expected_override);
                let affected: Vec<_> = expected_place.into_iter().chain(resolved).collect();
                deliver_place_purges(
                    &c,
                    p.id,
                    &row,
                    &affected,
                    usize::from(resolved != expected_place),
                )
                .await;
                // Repeated completion cannot change the place or generate another purge.
                yhotos_db::places::reverse(&mut pool.acquire().await.unwrap(), p.id)
                    .await
                    .unwrap();
                assert_eq!(snapshot(&pool, p.id).await, Some(row.clone()));
                deliver_place_purges(&c, p.id, &row, &[], 0).await;
            }
        }
    }
}

async fn dependent_rows(pool: &PgPool, p: PhotoId) -> Value {
    sqlx::query_scalar!(
        r#"SELECT jsonb_build_object(
      'grants',(SELECT jsonb_agg(id ORDER BY id) FROM photo_grants WHERE photo_id=$1),
      'derivatives',(SELECT jsonb_agg(id ORDER BY id) FROM derivatives WHERE photo_id=$1),
      'video',(SELECT jsonb_agg(id ORDER BY id) FROM video_renditions WHERE photo_id=$1),
      'tags',(SELECT jsonb_agg(id ORDER BY id) FROM photo_tags WHERE photo_id=$1),
      'likes',(SELECT jsonb_agg(id ORDER BY id) FROM likes WHERE photo_id=$1),
      'flags',(SELECT jsonb_agg(id ORDER BY id) FROM flags WHERE photo_id=$1),
      'faces',(SELECT jsonb_agg(id ORDER BY id) FROM faces WHERE photo_id=$1),
      'embeddings',(SELECT jsonb_agg(photo_id) FROM embeddings WHERE photo_id=$1),
      'ocr',(SELECT jsonb_agg(id ORDER BY id) FROM ocr_text WHERE photo_id=$1),
      'covers',(SELECT jsonb_agg(id ORDER BY id) FROM albums WHERE cover_photo_id=$1),
      'events',(SELECT jsonb_agg(id ORDER BY id) FROM events WHERE photo_id=$1)
    ) AS "rows!""#,
        p.0
    )
    .fetch_one(pool)
    .await
    .unwrap()
}
async fn seed_generated_dependents(c: &Client, p: &photos::PhotoRow) {
    sqlx::query!("INSERT INTO derivatives(id,photo_id,variant,format,hash,width,height,bytes) VALUES($1,$2,'thumb','jpeg',$3,1,1,1)",Uuid::now_v7(),p.id.0,p.original_hash).execute(&c.pool).await.unwrap();
    sqlx::query!("INSERT INTO video_renditions(id,photo_id,codec,hash,width,height,bytes,duration_ms) VALUES($1,$2,'h264',$3,1,1,1,1)",Uuid::now_v7(),p.id.0,p.original_hash).execute(&c.pool).await.unwrap();
    sqlx::query!("INSERT INTO faces(id,photo_id,bbox,embedding,quality) VALUES($1,$2,ARRAY[0,0,1,1]::real[],array_fill(1::real,ARRAY[512])::vector,1)",Uuid::now_v7(),p.id.0).execute(&c.pool).await.unwrap();
    sqlx::query!("INSERT INTO embeddings(photo_id,model,vector) VALUES($1,'fixture',array_fill(1::real,ARRAY[768])::vector)",p.id.0).execute(&c.pool).await.unwrap();
    sqlx::query!(
        "INSERT INTO ocr_text(id,photo_id,text) VALUES($1,$2,'Fixture text')",
        Uuid::now_v7(),
        p.id.0
    )
    .execute(&c.pool)
    .await
    .unwrap();
}
async fn seed_dependents(c: &Client, p: &photos::PhotoRow) {
    seed_generated_dependents(c, p).await;
    sqlx::query!(
        "INSERT INTO photo_grants(id,photo_id,principal_kind,principal_id) VALUES($1,$2,'user',$3)",
        Uuid::now_v7(),
        p.id.0,
        c.user.0
    )
    .execute(&c.pool)
    .await
    .unwrap();
    sqlx::query!(
        "INSERT INTO album_grants(id,album_id,principal_kind,principal_id) VALUES($1,$2,'user',$3)",
        Uuid::now_v7(),
        c.album.0,
        c.user.0
    )
    .execute(&c.pool)
    .await
    .unwrap();
    sqlx::query!(
        "UPDATE albums SET cover_photo_id=$2 WHERE id=$1",
        c.album.0,
        p.id.0
    )
    .execute(&c.pool)
    .await
    .unwrap();
    let tag = Uuid::now_v7();
    sqlx::query!(
        "INSERT INTO tags(id,slug,name,source) VALUES($1,$2,'Manual tag','manual')",
        tag,
        format!("tag-{tag}")
    )
    .execute(&c.pool)
    .await
    .unwrap();
    sqlx::query!(
        "INSERT INTO photo_tags(id,photo_id,tag_id,source) VALUES($1,$2,$3,'manual')",
        Uuid::now_v7(),
        p.id.0,
        tag
    )
    .execute(&c.pool)
    .await
    .unwrap();
    yhotos_db::likes::like(
        &mut c.pool.acquire().await.unwrap(),
        p.id,
        yhotos_db::likes::Actor::User(c.user),
    )
    .await
    .unwrap();
    yhotos_db::flags::flag(
        &mut c.pool.acquire().await.unwrap(),
        p.id,
        yhotos_db::likes::Actor::User(c.user),
    )
    .await
    .unwrap();
    let visitor = Token::generate()
        .hash(TokenKind::Session)
        .as_bytes()
        .to_vec();
    sqlx::query!("INSERT INTO events(id,photo_id,album_id,kind,visitor_hash) VALUES($1,$2,$3,'photo_view',$4)",Uuid::now_v7(),p.id.0,c.album.0,visitor).execute(&c.pool).await.unwrap();
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
#[allow(clippy::too_many_lines)] // Exercise the complete FK graph through both purge entry points.
async fn foreign_key_lifecycle_with_pending_replacements_uploads_and_queued_jobs(pool: PgPool) {
    for kind in ["photo", "album"] {
        let mut c = Client::new(pool.clone()).await;
        let root = c.album;
        if kind == "album" {
            c.album = albums::create(
                &mut pool.acquire().await.unwrap(),
                Some(root),
                &Slug::from_title("Nested"),
                "Nested",
                Visibility::Public,
            )
            .await
            .unwrap()
            .id;
        }
        let p = seed(&c, &original(&c, kind)).await;
        let place = Uuid::now_v7();
        sqlx::query!(
            "INSERT INTO places(id,name,country,lat,lon) VALUES($1,'Retained place','CA',43,-79)",
            place
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query!(
            "UPDATE photos SET place_id=$2,place_overridden=true WHERE id=$1",
            p.id.0,
            place
        )
        .execute(&pool)
        .await
        .unwrap();
        seed_dependents(&c, &p).await;
        let outside = albums::create(
            &mut pool.acquire().await.unwrap(),
            None,
            &Slug::from_title(&Uuid::now_v7().to_string()),
            "Outside purge",
            Visibility::Public,
        )
        .await
        .unwrap()
        .id;
        // Keep a pending replacement in ANOTHER album, so album cascades alone cannot fix it.
        let replacement = replacement_url(&c, p.id, 100).await;
        let replacement_id: Uuid = replacement.rsplit('/').next().unwrap().parse().unwrap();
        sqlx::query!(
            "UPDATE uploads SET album_id=$2 WHERE id=$1",
            replacement_id,
            outside.0
        )
        .execute(&pool)
        .await
        .unwrap();
        let ordinary = c.create(100, "ordinary.jpg").await;
        assert_eq!(ordinary.status(), StatusCode::CREATED);
        let ordinary_id: Uuid = ordinary.headers()["location"]
            .to_str()
            .unwrap()
            .rsplit('/')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let temps = sqlx::query!(
            "SELECT id,tmp_path FROM uploads WHERE id=ANY($1)",
            &[replacement_id, ordinary_id]
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let queued_ingest = Queue::new(pool.clone())
            .enqueue(
                Job::Ingest {
                    photo_id: p.id,
                    original_hash: p.original_hash.parse().unwrap(),
                    generation: 0,
                },
                0,
                None,
            )
            .await
            .unwrap();
        Queue::new(pool.clone())
            .enqueue(Job::GeoReverse { photo_id: p.id }, 0, None)
            .await
            .unwrap();
        let before = dependent_rows(&pool, p.id).await;
        assert!(before.as_object().unwrap().values().all(|v| !v.is_null()));
        library::replace(
            &mut pool.acquire().await.unwrap(),
            c.user,
            p.id.0,
            &original(&c, "new FK bytes"),
        )
        .await
        .unwrap();
        let after = dependent_rows(&pool, p.id).await;
        for field in ["grants", "tags", "likes", "flags", "covers", "events"] {
            assert_eq!(after[field], before[field], "replace lost {field}");
        }
        for field in ["derivatives", "video", "faces", "embeddings", "ocr"] {
            assert!(after[field].is_null(), "replace retained stale {field}");
        }
        assert_eq!(
            library::replacement_target(&mut pool.acquire().await.unwrap(), replacement_id)
                .await
                .unwrap(),
            Some(p.id.0)
        );
        // Recreate the regenerated dependents so trash/restore and purge each
        // exercise their FKs with rows present, independently of replacement cleanup.
        seed_generated_dependents(&c, &p).await;
        let after = dependent_rows(&pool, p.id).await;
        let target = if kind == "album" { root.0 } else { p.id.0 };
        command(&c, json!({"op":"trash","kind":kind,"id":target}))
            .await
            .unwrap();
        assert_eq!(dependent_rows(&pool, p.id).await, after);
        command(&c, json!({"op":"restore","kind":kind,"id":target}))
            .await
            .unwrap();
        assert_eq!(dependent_rows(&pool, p.id).await, after);
        command(&c, json!({"op":"trash","kind":kind,"id":target}))
            .await
            .unwrap();
        let mut tx = pool.begin().await.unwrap();
        library::execute_in(
            &mut tx,
            c.user,
            &serde_json::from_value(
                json!({"op":"purge","kind":kind,"id":target,"confirmation":"PURGE"}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        // The cancellation and deletion are one commit, including across album boundaries.
        assert!(snapshot(&pool, p.id).await.is_some());
        assert_eq!(
            library::replacement_target(&mut pool.acquire().await.unwrap(), replacement_id)
                .await
                .unwrap(),
            Some(p.id.0)
        );
        assert_eq!(
            yhotos_db::uploads::cancelled(&mut pool.acquire().await.unwrap())
                .await
                .unwrap(),
            Vec::<String>::new()
        );
        tx.commit().await.unwrap();
        assert!(snapshot(&pool, p.id).await.is_none());
        assert!(
            dependent_rows(&pool, p.id)
                .await
                .as_object()
                .unwrap()
                .values()
                .all(Value::is_null)
        );
        let uploads = sqlx::query!(
            "SELECT id FROM uploads WHERE id=ANY($1)",
            &[replacement_id, ordinary_id]
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            uploads.iter().map(|r| r.id).collect::<Vec<_>>(),
            if kind == "photo" {
                vec![ordinary_id]
            } else {
                vec![]
            }
        );
        // Analytics survives with cleared references; places and old-original GC roots survive too.
        let event = sqlx::query!(
            "SELECT photo_id,album_id FROM events WHERE album_id=$1 OR album_id IS NULL",
            c.album.0
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(event.iter().all(|e| e.photo_id.is_none()));
        if kind == "album" {
            assert!(event.iter().all(|e| e.album_id.is_none()));
        }
        assert!(
            sqlx::query_scalar!("SELECT EXISTS(SELECT FROM places WHERE id=$1)", place)
                .fetch_one(&pool)
                .await
                .unwrap()
                .unwrap()
        );
        assert!(
            library::original_referenced(&mut pool.acquire().await.unwrap(), &p.original_hash)
                .await
                .unwrap()
        );
        let ingest = worker(&c);
        let stale = queued(&pool)
            .await
            .into_iter()
            .find(|r| r.id == queued_ingest)
            .unwrap();
        assert!(matches!(
            ingest.handle(&stale, &context(&pool)).await,
            Err(yhotos_jobs::JobError::Fatal(_))
        ));
        yhotos_db::places::reverse(&mut pool.acquire().await.unwrap(), p.id)
            .await
            .unwrap();
        assert!(snapshot(&pool, p.id).await.is_none());
        // Durable cancellation cleanup survives purge and removes files on the next GC run.
        let mut expected_cleanup: Vec<_> = temps
            .iter()
            .filter(|temp| kind == "album" || temp.id == replacement_id)
            .map(|temp| temp.tmp_path.clone())
            .collect();
        expected_cleanup.sort();
        assert_eq!(
            yhotos_db::uploads::cancelled(&mut pool.acquire().await.unwrap())
                .await
                .unwrap(),
            expected_cleanup
        );
        ingest.sweep_uploads().await.unwrap();
        for temp in temps {
            let temp_id = temp.tmp_path.parse().unwrap();
            assert_eq!(
                c.state.store.upload_len(&temp_id).await.is_ok(),
                kind == "photo" && temp.id == ordinary_id
            );
        }
        assert_eq!(
            yhotos_db::uploads::cancelled(&mut pool.acquire().await.unwrap())
                .await
                .unwrap(),
            Vec::<String>::new()
        );
        ingest.sweep_uploads().await.unwrap();
    }
}

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn purge_waits_for_streaming_upload_without_deadlock_or_late_publication(pool: PgPool) {
    for (kind, replacement) in [("photo", true), ("album", true), ("album", false)] {
        let c = Arc::new(Client::new(pool.clone()).await);
        let p = seed(&c, &original(&c, "Concurrent purge")).await;
        let bytes = fixture("tiny-1x1.jpg");
        let hash: yhotos_storage::ContentHash =
            blake3::hash(&bytes).to_hex().to_string().parse().unwrap();
        let path = if replacement {
            replacement_url(&c, p.id, bytes.len()).await
        } else {
            c.create(bytes.len(), "ordinary.jpg").await.headers()["location"]
                .to_str()
                .unwrap()
                .to_owned()
        };
        let retry_path = path.clone();
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
        let target = if kind == "album" { c.album.0 } else { p.id.0 };
        // Trash does not wait on the upload, and preserves its FK target for resumption/restore.
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            command(&c, json!({"op":"trash","kind":kind,"id":target})),
        )
        .await
        .unwrap()
        .unwrap();
        let purger = c.clone();
        let purge = tokio::spawn(async move {
            command(
                &purger,
                json!({"op":"purge","kind":kind,"id":target,"confirmation":"PURGE"}),
            )
            .await
        });
        // Wait for actual row-lock contention, not a timer-based guess about scheduling.
        tokio::time::timeout(std::time::Duration::from_secs(5),async {
            loop {
                if sqlx::query_scalar!(r#"SELECT EXISTS(SELECT FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND (query LIKE '%DELETE FROM uploads WHERE replace_photo_id%' OR query LIKE '%DELETE FROM albums WHERE id=ANY%')) AS "waiting!""#).fetch_one(&pool).await.unwrap() {break;}
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        release.send(()).unwrap();
        let response = tokio::time::timeout(std::time::Duration::from_secs(10), patch)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        tokio::time::timeout(std::time::Duration::from_secs(5), purge)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(snapshot(&pool, p.id).await.is_none());
        assert_eq!(
            c.send("HEAD", &retry_path, &[], Body::empty())
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            sqlx::query_scalar!("SELECT count(*) FROM photos WHERE album_id=$1", c.album.0)
                .fetch_one(&pool)
                .await
                .unwrap(),
            Some(0)
        );
        assert!(
            library::gc_candidates(&mut pool.acquire().await.unwrap())
                .await
                .unwrap()
                .contains(&hash.to_string())
        );
        let published = c
            .state
            .store
            .open_read(yhotos_storage::Area::Originals, &hash, None)
            .await
            .unwrap();
        published
            .into_std()
            .await
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_hours(48))
            .unwrap();
        worker(&c).sweep_uploads().await.unwrap();
        assert_eq!(c.state.store.list_uploads().await.unwrap(), Vec::new());
        assert!(
            !c.state
                .store
                .exists(yhotos_storage::Area::Originals, &hash, None)
                .await
        );
    }
}
