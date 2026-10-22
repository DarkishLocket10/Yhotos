#![forbid(unsafe_code)]
//! Full verified ingest, including the publication transaction and follow-up jobs.
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

fn fixture(dir: &Path) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = fixture(&path) {
                return Some(found);
            }
        } else if let Ok(id) = yhotos_codec::identify(&path, &yhotos_codec::Limits::default())
            && u64::from(id.width) * u64::from(id.height) >= 24_000_000
        {
            return Some(path);
        }
    }
    None
}
fn main() -> Result<(), Box<dyn Error>> {
    let Some(input) = fixture(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/large"))
    else {
        eprintln!("pipeline/ingest skipped: no 24 MP fixture in fixtures/large");
        return Ok(());
    };
    let runtime = tokio::runtime::Runtime::new()?;
    let _context = runtime.enter();
    let url = std::env::var("DATABASE_URL")?.into();
    let db = runtime.block_on(Db::connect(&url, Role::Admin, 16, DatabaseTls::Disable))?;
    runtime.block_on(db.migrate())?;
    let root = tempfile::tempdir()?;
    let store = Store::open(root.path())?;
    let original = runtime.block_on(async {
        let file = tokio::fs::File::open(input).await?;
        store
            .put_original(file, 2 << 30)
            .await
            .map_err(std::io::Error::other)
    })?;
    let host = std::env::var_os("YHOTOS_WORKER__CODEC_HOST_PATH")
        .map_or_else(
            || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/release/yhotos-codec-host"),
            PathBuf::from,
        )
        .canonicalize()?;
    let ingest = Ingest::new(store, db.pool().clone(), host);
    let mut conn = runtime.block_on(db.pool().acquire())?;
    let album = runtime.block_on(albums::create(
        &mut conn,
        None,
        &Slug::new(&format!("bench-{}", uuid::Uuid::now_v7()))?,
        "Pipeline benchmark",
        Visibility::Public,
    ))?;
    let mut ids = Vec::new();
    for _ in 0..8 {
        let photo = runtime.block_on(photos::create(
            &mut conn,
            &photos::NewPhoto {
                album_id: album.id,
                title: "Benchmark".into(),
                original_hash: original.hash.to_string(),
                original_name: "benchmark".into(),
                original_bytes: i64::try_from(original.bytes)?,
                mime: "image/jpeg".into(),
                kind: photos::MediaKind::Image,
            },
        ))?;
        ids.push(photo.id);
    }
    drop(conn);
    let mut c = Criterion::default()
        .sample_size(10)
        .measurement_time(Duration::from_secs(20))
        .configure_from_args();
    let mut failure = None;
    for parallelism in [1, 8] {
        let name = if parallelism == 1 {
            "pipeline/ingest_single"
        } else {
            "pipeline/ingest_8_parallel"
        };
        c.bench_function(name,|b|b.iter_custom(|iterations| {
            let mut total=Duration::ZERO;
            for _ in 0..iterations {
                let result=runtime.block_on(async {
                    // Reset publication outside timing so the benchmark never measures a no-op.
                    for id in ids.iter().take(parallelism) {sqlx::query!("UPDATE photos SET status='pending',derivative_plan=NULL WHERE id=$1",id.0).execute(db.pool()).await?;}
                    let start=Instant::now();
                    let mut tasks=tokio::task::JoinSet::new();
                    for id in ids.iter().take(parallelism).copied() {let ingest=ingest.clone();tasks.spawn(async move{ingest.ingest(id).await});}
                    while let Some(result)=tasks.join_next().await {result??;}
                    Ok::<Duration,Box<dyn Error>>(start.elapsed())
                });
                match result {Ok(elapsed)=>total+=elapsed,Err(error)=>{failure=Some(error);break;}}
            }
            total
        }));
    }
    c.final_summary();
    // Delete benchmark rows before the temporary store disappears.
    sqlx_cleanup(&runtime, db.pool(), album.id.0)?;
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(())
}
fn sqlx_cleanup(
    runtime: &tokio::runtime::Runtime,
    pool: &sqlx::PgPool,
    album: uuid::Uuid,
) -> Result<(), sqlx::Error> {
    runtime.block_on(async {sqlx::query!("DELETE FROM jobs WHERE payload->>'photo_id' IN (SELECT id::text FROM photos WHERE album_id=$1)",album).execute(pool).await?;sqlx::query!("DELETE FROM photos WHERE album_id=$1",album).execute(pool).await?;sqlx::query!("DELETE FROM albums WHERE id=$1",album).execute(pool).await?;Ok(())})
}
