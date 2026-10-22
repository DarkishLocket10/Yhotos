#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
//! This harness belongs to the binary package so Cargo supplies its real host path.
//! The pipeline test implementation remains with the pipeline crate.
#[cfg(feature = "test-hooks")]
#[path = "../../yhotos-pipeline/tests/support/e2e.rs"]
mod pipeline;
