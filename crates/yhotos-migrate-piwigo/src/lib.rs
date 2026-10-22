#![forbid(unsafe_code)]
//! Read-only Piwigo source migration with bounded streaming and resumable imports.
mod import;
mod plan;
mod source;

use serde::Serialize;
use std::{io::Write, path::PathBuf};
use time::UtcOffset;
use yhotos_storage::{SourceTree, Store};

/// Redacted failures, safe to print without leaking credentials or source paths.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Invalid options or environment values.
    #[error("invalid migration configuration")]
    Configuration,
    /// Incompatible source schema version.
    #[error("Piwigo schema version 16 is required")]
    SourceSchema,
    /// Corrupt tree, references or metadata.
    #[error("invalid Piwigo data; inspect the dry-run report")]
    SourceData,
    /// Connection or SQL failed; underlying details must never be printed.
    #[error("database operation failed")]
    Sql(#[from] sqlx::Error),
    /// Destination repository failure.
    #[error("destination operation failed")]
    Db(#[from] yhotos_db::DbError),
    /// Storage rejected a file or failed.
    #[error("storage operation failed")]
    Storage(#[from] yhotos_storage::StorageError),
    /// A background worker did not finish.
    #[error("migration worker failed")]
    Worker,
    /// Report publication failed.
    #[error("report could not be written")]
    Report,
}
/// Result with a credential-safe display error.
pub type Result<T> = std::result::Result<T, Error>;

/// Operator-provided local roots and bounded import options; no credentials.
pub struct Options {
    /// Mounted read-only Piwigo upload root.
    pub source_root: PathBuf,
    /// Existing Yhotos storage root; unused by dry-run.
    pub storage_root: PathBuf,
    /// JSON report output outside the source tree.
    pub report: PathBuf,
    /// Read and validate everything without changing PostgreSQL or storage.
    pub dry_run: bool,
    /// Concurrent file hashes/copies, 1 through 32; default CLI value is four.
    pub concurrency: usize,
    /// Interpretation of Piwigo's timezone-less dates; default is UTC.
    pub source_offset: UtcOffset,
}

/// A source identity and a fixed reason, without untrusted paths or metadata.
#[derive(Debug, Serialize)]
pub struct Problem {
    /// Piwigo image or album id, when applicable.
    pub id: Option<i64>,
    /// Fixed diagnostic suitable for logs and JSON reports.
    pub reason: String,
}
/// Rehearsal and import counts. Photos count source images; rows count memberships.
#[derive(Debug, Default, Serialize)]
pub struct Report {
    /// Whether destination writes were disabled.
    pub dry_run: bool,
    /// Source category count.
    pub albums: usize,
    /// Source image count, including skipped files.
    pub photos: usize,
    /// Imported/planned category membership rows.
    pub photo_rows: usize,
    /// Bytes read from available source originals, counted once per source image.
    pub bytes: u64,
    /// Originals already present in content-addressed storage.
    pub reused_originals: usize,
    /// Source images not imported with reasons.
    pub skipped: Vec<Problem>,
    /// Slug and ambiguous legacy name collisions.
    pub collisions: Vec<Problem>,
    /// Invalid metadata or fields that need operator review.
    pub unsupported: Vec<Problem>,
    /// Album counts printed as progress and preserved in the JSON report.
    pub album_progress: Vec<AlbumProgress>,
}
/// One source album's resulting photo membership count.
#[derive(Debug, Serialize)]
pub struct AlbumProgress {
    /// Piwigo category id, safe to print without terminal control characters.
    pub id: i64,
    /// Successfully planned/imported direct members.
    pub photos: usize,
}

/// Migrates from credentials supplied only by the environment. Dry-run does not
/// connect to PostgreSQL, open destination storage, or create source entries.
/// Writes a JSON report even when individual files cannot be imported.
///
/// # Errors
/// Returns redacted configuration, source, destination or report failures.
pub async fn run(options: &Options) -> Result<Report> {
    use secrecy::SecretString;
    use yhotos_db::{DatabaseTls, Db, Role};
    if !(1..=32).contains(&options.concurrency) {
        return Err(Error::Configuration);
    }
    // A report may never be placed beneath the read-only source root, including
    // via a symlinked output directory. The output basename is operator input.
    let root = std::fs::canonicalize(&options.source_root).map_err(|_| Error::Configuration)?;
    let report_parent = options
        .report
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let parent = std::fs::canonicalize(report_parent).map_err(|_| Error::Configuration)?;
    if parent.starts_with(&root) || options.report.is_symlink() {
        return Err(Error::Configuration);
    }
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&options.report)
        .map_err(|_| Error::Report)?;
    let tree = std::sync::Arc::new(SourceTree::open(&root)?);
    let url = std::env::var("PIWIGO_DATABASE_URL").map_err(|_| Error::Configuration)?;
    let pool = source::connect(&url).await?;
    let snapshot = source::snapshot(&pool).await?;
    pool.close().await;
    let mut report = Report {
        dry_run: options.dry_run,
        albums: snapshot.albums.len(),
        photos: snapshot.images.len(),
        ..Report::default()
    };
    let plan = plan::build(snapshot, &mut report)?;
    let destination = if options.dry_run {
        None
    } else {
        let store_root =
            std::fs::canonicalize(&options.storage_root).map_err(|_| Error::Configuration)?;
        if store_root.starts_with(&root) || root.starts_with(&store_root) {
            return Err(Error::Configuration);
        }
        let url: SecretString = std::env::var("DATABASE_URL")
            .map_err(|_| Error::Configuration)?
            .into();
        let tls = match std::env::var("YHOTOS_MIGRATION_PG_TLS").as_deref() {
            Ok("disable") => DatabaseTls::Disable,
            Err(std::env::VarError::NotPresent) | Ok("require") => DatabaseTls::Require,
            _ => return Err(Error::Configuration),
        };
        Some((
            Db::connect(&url, Role::Admin, 2, tls).await?,
            Store::open(store_root)?,
        ))
    };
    import::execute(options, tree, plan, destination.as_ref(), &mut report).await?;
    let json = serde_json::to_vec_pretty(&report).map_err(|_| Error::Report)?;
    // create_new prevents following an existing report symlink or overwriting a
    // source hard link. Use a fresh output name for every rehearsal.
    output.write_all(&json).map_err(|_| Error::Report)?;
    output.sync_all().map_err(|_| Error::Report)?;
    Ok(report)
}
