---
id: 004
slug: storage
model: gpt-6.1-sol
effort: xhigh
review_effort: xhigh
depends_on: [001]
crates: [yhotos-storage]
---

# 004 Content-addressed storage

## Goal

`yhotos-storage` is the only code that touches the data tree. It provides a `Store` rooted at `data_dir` with streaming writes that hash as they go, atomic placement into the content-addressed layout from `docs/architecture.md` section 5, safe path construction that cannot escape the root, range-capable reads, temp space for uploads and zips, and a garbage-collection helper. It is fuzzed and benchmarked.

## Read first

- `AGENTS.md`
- `docs/architecture.md` section 5
- `docs/threat-model.md` sections 4.1 (path traversal), 4.4 (DoS: streaming), 4.5
- `docs/performance.md` sections 1 (hash + copy budget) and 3 (streaming everywhere)

## Scope

`crates/yhotos-storage/`, `fuzz/fuzz_targets/safe_join.rs`, `crates/yhotos-bench/benches/storage.rs`. Workspace `Cargo.toml` for new dependencies only.

## Design

```rust
pub struct Store { root: PathBuf }           // root is canonicalized at construction and must exist
pub struct ContentHash([u8; 32]);            // BLAKE3; Display/FromStr as 64 lowercase hex; FromStr rejects anything else
pub enum Area { Originals, Derivatives, Video }
pub struct DerivativeKey<'a> { pub hash: &'a ContentHash, pub variant: &'a str, pub format: &'a str, pub hdr: bool }
    // variant and format are validated against ^[a-z0-9]{1,16}$; file name is "<hash>.<variant>.<format>[.hdr]"

impl Store {
  pub fn open(root: impl AsRef<Path>) -> Result<Store, StorageError>;
  pub fn path_original(&self, h: &ContentHash) -> PathBuf;          // infallible once inputs are typed
  pub fn path_derivative(&self, k: &DerivativeKey) -> Result<PathBuf, StorageError>;
  pub fn path_video(&self, h: &ContentHash, codec: &str) -> Result<PathBuf, StorageError>;
  pub async fn put_original<R: AsyncRead + Unpin>(&self, r: R, max_bytes: u64) -> Result<Put, StorageError>;
      // streams to tmp, hashes with BLAKE3 while writing, fsyncs, renames into place; if a file with that hash
      // already exists, discards the temp file and returns Put { hash, bytes, existed: true }
  pub async fn put_derivative(&self, k: &DerivativeKey, src: &Path) -> Result<(), StorageError>;
      // moves (rename, or copy+fsync+rename across devices) a file produced by the codec host into place, atomically
  pub async fn open_read(&self, area: Area, h: &ContentHash, name: Option<&DerivativeKey>) -> Result<tokio::fs::File, StorageError>;
  pub async fn exists(&self, ...) -> bool;
  pub async fn remove(&self, ...) -> Result<(), StorageError>;
  pub fn tmp_dir(&self) -> TempDir;            // /data/tmp/<uuid>, removed on drop unless kept
  pub async fn gc(&self, keep: impl Fn(&ContentHash, &str) -> bool, tmp_older_than: Duration) -> Result<GcReport, StorageError>;
}
```

- `safe_join(root, components)` is a private function used by every path builder: each component must match `^[A-Za-z0-9._-]{1,128}$`, must not be `.` or `..`, and the joined path must start with `root` after lexical normalization. After the file exists, `open_read` also verifies with `symlink_metadata` that no component is a symlink. These are the properties the fuzz target checks.
- Sharding: `<h0h1>/<h2h3>/<hash...>` where `h0h1` are the first two hex chars, as in the architecture doc. Directories created with `0750`.
- Writes: `O_EXCL` temp file in `tmp/`, `fdatasync` before rename, directory fsync after rename on Linux. Size cap enforced during streaming; exceeding it aborts and removes the temp file.
- `put_original` must not buffer more than 1 MiB at a time; use a fixed 256 KiB buffer.
- Errors (`StorageError`): `InvalidHash`, `InvalidName`, `TooLarge { max }`, `NotFound`, `Io(std::io::Error)` with the operation and a redacted path (relative to root) in the message.
- No path from outside the crate is ever accepted as a `Path` for lookup; only typed keys.

### Fuzz
`fuzz/fuzz_targets/safe_join.rs` feeds arbitrary byte strings as components and asserts the result either errors or is inside a temp root; also asserts `ContentHash::from_str` never panics.

### Bench
`storage.rs`: `put_original` of a 30 MB in-memory buffer (hash + write + rename) against a `tempfile` root. Budget: under 150 ms on tower-class hardware; record the local number.

## Acceptance

- [ ] Unit tests for every rejection: `..`, absolute components, empty, too long, bad characters, uppercase hash, 63/65-char hash, symlinked component (create one in a temp root and assert `open_read` refuses).
- [ ] `put_original` dedupes: writing the same bytes twice returns `existed: true` the second time and leaves one file.
- [ ] `put_original` enforces `max_bytes` and leaves no temp file behind (assert `tmp/` is empty after).
- [ ] Concurrent `put_original` of the same bytes from two tasks yields one file and both succeed.
- [ ] `gc` removes unreferenced derivatives and old temp dirs, keeps referenced ones; report counts match.
- [ ] Fuzz target builds (`cargo fuzz build`) and runs 60 s locally without a crash (record in PR; CI will run a smoke of 30 s once the fuzz workflow exists, not in this task).
- [ ] Benchmark exists and number is in the PR.
- [ ] `scripts/check.sh` passes.

## Security notes

- Threat model 4.1 path traversal: this crate is the control. The PR must point to the test for each rejection case.
- Threat model 4.4: no whole-file reads; `open_read` returns a `File` for streaming; range handling is the HTTP layer's job via `AsyncSeek`.
- File permissions: files `0640`, directories `0750`; never world-readable.
- Allowed new dependencies: `blake3`, `tokio` (fs, io-util), `tempfile`, `uuid`, `cargo-fuzz`/`libfuzzer-sys` (fuzz crate only), `arbitrary`.

## Performance

- Hash while writing, single pass. No second read of the file to hash it.
- Benchmark as above.

## Not in this task

- HTTP serving of files, ETags, range requests.
- Upload protocol (tus) and chunk assembly.
- Zip streaming.
