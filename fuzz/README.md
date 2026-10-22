# Storage fuzz harness

The safe_join target compiles the private production component validation,
safe-join, and hash parsing modules directly. It uses only libfuzzer-sys and
arbitrary; the storage API stays private and production storage dependencies stay
out of the harness.
Symlink refusal is covered by the storage unit tests.

Run from the repository root with a sanitizer-capable toolchain and cargo-fuzz:

```sh
cargo +nightly fuzz build
cargo +nightly fuzz run safe_join -- -max_total_time=60 -max_len=1024
cargo deny --manifest-path fuzz/Cargo.toml --config deny.toml check
```

When nightly is unavailable, installed Rust 1.99.1 also supports the sanitizer
flags with `RUSTC_BOOTSTRAP=1`. Cargo deny loads the fuzz-only NCSA exception from
`fuzz/deny.exceptions.toml` for libfuzzer-sys 0.4.13. The root deny policy is
maintainer-only and remains unchanged.
