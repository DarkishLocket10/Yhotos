#![forbid(unsafe_code)]

//! Content-addressed, bounded streaming storage. Filesystem errors contain only
//! operation names, paths relative to the store, and error kinds.

mod filesystem;
mod gc;
mod hash;
mod paths;
mod temp;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

pub use gc::GcReport;
pub use hash::ContentHash;
pub use temp::TempDir;

use paths::{Component, safe_join};

const BUFFER_BYTES: usize = 256 * 1024;

/// Filesystem areas with distinct validated filename layouts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Area {
    /// Immutable uploaded bytes.
    Originals,
    /// Encoded image variants.
    Derivatives,
    /// MP4 renditions.
    Video,
}

/// A derivative selector. Variant and format must each be 1 to 16 lowercase
/// ASCII letters or digits; storage validates them before constructing a path.
#[derive(Clone, Copy, Debug)]
pub struct DerivativeKey<'a> {
    /// Digest used to shard and name the file.
    pub hash: &'a ContentHash,
    /// Encoding variant, or codec when selecting a video rendition.
    pub variant: &'a str,
    /// Encoding format; must be `mp4` for video reads.
    pub format: &'a str,
    /// Whether the image contains HDR data; must be false for video.
    pub hdr: bool,
}

/// Result of a completed original write; deduplication never replaces bytes.
#[derive(Debug)]
pub struct Put {
    /// BLAKE3 digest computed in the single streaming pass.
    pub hash: ContentHash,
    /// Number of input bytes accepted.
    pub bytes: u64,
    /// True if another write already placed this digest.
    pub existed: bool,
}

/// Storage failures with no absolute paths or underlying OS error messages.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// A digest was not exactly 64 lowercase hex characters.
    #[error("invalid content hash")]
    InvalidHash,
    /// A component, selector, or filesystem entry was unsafe.
    #[error("invalid storage name")]
    InvalidName,
    /// Input exceeded its declared streaming limit.
    #[error("input exceeds {max} bytes")]
    TooLarge {
        /// Maximum permitted byte count.
        max: u64,
    },
    /// The requested entry does not exist.
    #[error("storage entry not found")]
    NotFound,
    /// An operation failed, with a redacted root-relative context.
    #[error("{0}")]
    Io(#[source] io::Error),
}

/// A canonical existing data root. The tree must be owned by the application;
/// other processes must not replace directories or the `.publish.lock` file
/// while operations are in flight.
#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Opens an existing directory, resolving its root without creating entries.
    /// Readers can open a read-only root even before its first publication.
    ///
    /// # Errors
    /// Returns `NotFound` for a missing root, `InvalidName` for a non-directory,
    /// or a redacted `Io` error on root access.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, StorageError> {
        let root = fs::canonicalize(root).map_err(|e| io_error("open root", ".", e))?;
        let metadata = fs::metadata(&root).map_err(|e| io_error("inspect root", ".", e))?;
        if !metadata.is_dir() {
            return Err(StorageError::InvalidName);
        }
        Ok(Self { root })
    }

    /// Returns the sharded path using only the validated digest.
    #[must_use]
    pub fn path_original(&self, h: &ContentHash) -> PathBuf {
        self.sharded(Area::Originals, h, Component::hash(h))
    }

    /// Returns a sharded derivative path after validating both name tokens.
    ///
    /// # Errors
    /// Returns `InvalidName` for an invalid variant or format.
    pub fn path_derivative(&self, k: &DerivativeKey<'_>) -> Result<PathBuf, StorageError> {
        paths::validate_token(k.variant)?;
        paths::validate_token(k.format)?;
        let filename = format!(
            "{}.{}.{}{}",
            k.hash,
            k.variant,
            k.format,
            if k.hdr { ".hdr" } else { "" }
        );
        Ok(self.sharded(Area::Derivatives, k.hash, Component::parse(&filename)?))
    }

    /// Returns a sharded MP4 path after validating the codec token.
    ///
    /// # Errors
    /// Returns `InvalidName` for an invalid codec.
    pub fn path_video(&self, h: &ContentHash, codec: &str) -> Result<PathBuf, StorageError> {
        paths::validate_token(codec)?;
        Ok(self.sharded(
            Area::Video,
            h,
            Component::parse(&format!("{h}.{codec}.mp4"))?,
        ))
    }

    /// Streams at most `max_bytes` with a 256 KiB buffer, hashing off the async
    /// executor. Syncs data and atomically publishes without overwriting a hash.
    /// Deduplication refreshes the existing file's modification time.
    /// Errors and cancelled futures discard their temporary file.
    ///
    /// # Errors
    /// Returns `TooLarge` on a limit violation, `InvalidName` for unsafe tree
    /// entries, or a redacted `Io` error on input, write, sync, or placement failure.
    pub async fn put_original<R: AsyncRead + Unpin>(
        &self,
        r: R,
        max_bytes: u64,
    ) -> Result<Put, StorageError> {
        let store = self.clone();
        let (file, temp) = blocking(move || store.new_temp()).await?;
        self.put_original_into(r, max_bytes, file, temp).await
    }

    async fn put_original_into<R: AsyncRead + Unpin>(
        &self,
        mut r: R,
        max_bytes: u64,
        file: fs::File,
        temp: tempfile::TempPath,
    ) -> Result<Put, StorageError> {
        let mut file = tokio::fs::File::from_std(file);
        let mut buffer = vec![0; BUFFER_BYTES];
        let mut hasher = blake3::Hasher::new();
        let mut bytes = 0_u64;
        loop {
            let n = r
                .read(&mut buffer)
                .await
                .map_err(|e| io_error("read input", "<stream>", e))?;
            if n == 0 {
                break;
            }
            let n64 = u64::try_from(n).map_err(|_| StorageError::TooLarge { max: max_bytes })?;
            if n64 > max_bytes.saturating_sub(bytes) {
                return Err(StorageError::TooLarge { max: max_bytes });
            }
            (buffer, hasher) = blocking(move || {
                if let Some(chunk) = buffer.get(..n) {
                    hasher.update(chunk);
                }
                Ok((buffer, hasher))
            })
            .await?;
            if let Some(chunk) = buffer.get(..n) {
                file.write_all(chunk)
                    .await
                    .map_err(|e| self.error("write temporary", &temp, e))?;
            }
            bytes += n64;
        }
        // Tokio buffers the final write. Flush surfaces its error; sync_data
        // alone can retain that error internally and still report success.
        file.flush()
            .await
            .map_err(|e| self.error("flush temporary", &temp, e))?;
        file.sync_data()
            .await
            .map_err(|e| self.error("sync temporary", &temp, e))?;
        drop(file);
        let hash = ContentHash(*hasher.finalize().as_bytes());
        let store = self.clone();
        let existed =
            blocking(move || store.publish(temp, &store.path_original(&hash), false)).await?;
        Ok(Put {
            hash,
            bytes,
            existed,
        })
    }

    /// Moves a regular codec output into place, rejecting symlinks. Across
    /// devices, streams a copy into a destination-side temporary file first.
    /// The source must be an absolute path beneath this store's canonical `tmp`
    /// directory without symlink components, produced inside a `tmp_dir`.
    /// Refreshes the destination's modification time, including on deduplication,
    /// so GC's grace period starts at publication.
    ///
    /// # Errors
    /// Returns `InvalidName` for invalid keys or unsafe sources, including files
    /// outside `tmp`, `NotFound` for missing source files, or a redacted `Io`
    /// error for filesystem failures.
    pub async fn put_derivative(
        &self,
        k: &DerivativeKey<'_>,
        src: &Path,
    ) -> Result<(), StorageError> {
        let dest = self.path_derivative(k)?;
        let src = src.to_path_buf();
        let store = self.clone();
        blocking(move || store.move_derivative(&src, &dest)).await
    }

    /// Opens a regular file after rejecting symlinks in every component. The
    /// file supports streaming and `AsyncSeek` for HTTP range handling. Videos
    /// use a key with `variant = codec`, `format = "mp4"`, `hdr = false`.
    ///
    /// # Errors
    /// Returns `InvalidName` for invalid selectors, symlinks, or non-files,
    /// `NotFound` for missing entries, or a redacted `Io` error.
    pub async fn open_read(
        &self,
        area: Area,
        h: &ContentHash,
        name: Option<&DerivativeKey<'_>>,
    ) -> Result<tokio::fs::File, StorageError> {
        let path = self.lookup(area, h, name)?;
        let store = self.clone();
        let file = blocking(move || {
            store.check_path(&path, true)?;
            fs::File::open(&path).map_err(|e| store.error("open read", &path, e))
        })
        .await?;
        Ok(tokio::fs::File::from_std(file))
    }

    /// Returns true only for a valid, existing regular file with no symlinks.
    pub async fn exists(
        &self,
        area: Area,
        h: &ContentHash,
        name: Option<&DerivativeKey<'_>>,
    ) -> bool {
        self.open_read(area, h, name).await.is_ok()
    }

    /// Removes a typed regular file and syncs its containing directory on Linux.
    ///
    /// # Errors
    /// Returns `InvalidName` for invalid selectors or unsafe entries,
    /// `NotFound` for missing files, or a redacted `Io` error.
    pub async fn remove(
        &self,
        area: Area,
        h: &ContentHash,
        name: Option<&DerivativeKey<'_>>,
    ) -> Result<(), StorageError> {
        let path = self.lookup(area, h, name)?;
        let store = self.clone();
        blocking(move || {
            store.check_path(&path, true)?;
            fs::remove_file(&path).map_err(|e| store.error("remove", &path, e))?;
            store.sync_parent(&path)
        })
        .await
    }

    /// Creates a UUID directory under `tmp` with drop cleanup, unless kept.
    /// Creation is fallible rather than panicking on filesystem failures.
    ///
    /// # Errors
    /// Returns `InvalidName` for unsafe tree entries or a redacted `Io`
    /// error if the temporary directory cannot be created.
    pub fn tmp_dir(&self) -> Result<TempDir, StorageError> {
        self.create_tmp_dir()
    }

    /// Removes unreferenced derivatives and expired UUID temporary entries,
    /// including abandoned destination-side derivative staging files.
    /// The callback receives the digest and complete derivative filename.
    /// Derivatives and staging files are retained for at least `min_age` after
    /// their modification time, regardless of the callback. Callers must commit
    /// derivative rows within `min_age` of publication; this is how the ingest
    /// pipeline coordinates its database references with GC (it passes one hour).
    /// Publication and GC's final age/reference check and deletion are serialized
    /// across processes by a shared/exclusive file lock, taken per collected entry.
    /// The callback runs on a blocking worker under the exclusive lock and must
    /// not publish files or wait for a publisher. It must own its captured data.
    /// Temporary entries still observe `tmp_older_than`; temporary files also
    /// receive the `min_age` protection.
    ///
    /// # Errors
    /// Returns `InvalidName` for unsafe entries or a redacted `Io` error
    /// when enumeration, metadata inspection, or removal fails.
    pub async fn gc(
        &self,
        keep: impl Fn(&ContentHash, &str) -> bool + Send + Sync + 'static,
        tmp_older_than: Duration,
        min_age: Duration,
    ) -> Result<GcReport, StorageError> {
        self.collect(keep, tmp_older_than, min_age).await
    }

    fn sharded(&self, area: Area, hash: &ContentHash, filename: Component) -> PathBuf {
        let [first, second, ..] = hash.0;
        safe_join(
            &self.root,
            [
                Component::area(area),
                Component::byte(first),
                Component::byte(second),
                filename,
            ],
        )
    }

    fn lookup(
        &self,
        area: Area,
        h: &ContentHash,
        name: Option<&DerivativeKey<'_>>,
    ) -> Result<PathBuf, StorageError> {
        match (area, name) {
            (Area::Originals, None) => Ok(self.path_original(h)),
            (Area::Derivatives, Some(k)) if k.hash == h => self.path_derivative(k),
            (Area::Video, Some(k)) if k.hash == h && k.format == "mp4" && !k.hdr => {
                self.path_video(h, k.variant)
            }
            _ => Err(StorageError::InvalidName),
        }
    }

    fn error(&self, op: &str, path: &Path, error: io::Error) -> StorageError {
        let relative = path
            .strip_prefix(&self.root)
            .unwrap_or_else(|_| Path::new("<outside>"));
        io_error(op, &relative.to_string_lossy(), error)
    }
}

fn io_error(op: &str, path: &str, error: io::Error) -> StorageError {
    let kind = error.kind();
    drop(error); // Discard OS messages, which can contain absolute paths.
    if kind == io::ErrorKind::NotFound {
        return StorageError::NotFound;
    }
    StorageError::Io(io::Error::new(kind, format!("{op} at {path}: {kind:?}")))
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, StorageError> + Send + 'static,
) -> Result<T, StorageError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| io_error("storage worker", ".", io::Error::other("worker failed")))?
}

#[cfg(test)]
mod tests;
