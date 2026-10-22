#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! The binary is exercised as a subprocess so rlimits never affect the test runner.
use serde_json::{Value, json};
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/images")
        .join(name)
        .canonicalize()
        .unwrap()
}
fn job(input: &Path, out: &Path) -> Value {
    json!({"input":input,"out_dir":out,"outputs":[{"name":"medium.avif","resize":{"long_edge":192,"square":false},"encode":{"Avif":{"quality":90,"speed":6}},"keep_icc":true},{"name":"medium.jxl","resize":{"long_edge":192,"square":false},"encode":{"Jxl":{"distance":0.5,"effort":3}},"keep_icc":true},{"name":"medium.jpg","resize":{"long_edge":192,"square":false},"encode":{"Jpeg":{"quality":95,"progressive":true,"chroma_subsampling":false}},"keep_icc":false}],"want":["identity","metadata","thumbhash","dominant_colors"]})
}
fn invoke(job: &Value, args: &[&str]) -> std::process::Output {
    invoke_with_concurrency(job, args, "999")
}
fn invoke_with_concurrency(job: &Value, args: &[&str], concurrency: &str) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_yhotos-codec-host"))
        .args(args)
        .env("VIPS_WARNING", "injected")
        .env("VIPS_CONCURRENCY", concurrency)
        .env("VIPS_DISC_THRESHOLD", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(job).unwrap())
        .unwrap();
    child.wait_with_output().unwrap()
}
#[cfg(feature = "test-hooks")]
#[test]
fn validated_concurrency_reaches_native_initialization_and_survives_decode() {
    for (setting, expected) in [
        ("3", "3\n"),
        ("0", "2\n"),
        ("999", "2\n"),
        ("invalid", "2\n"),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let output = invoke_with_concurrency(
            &job(&fixture("chart-srgb.png"), tmp.path()),
            &["--self-test-concurrency"],
            setting,
        );
        assert!(output.status.success(), "{setting}: {:?}", output.status);
        assert_eq!(output.stderr, b"");
        assert_eq!(output.stdout, expected.as_bytes(), "{setting}");
    }
}
#[test]
fn three_output_report_and_hashes() {
    let tmp = tempfile::tempdir().unwrap();
    let output = invoke(&job(&fixture("chart-srgb.png"), tmp.path()), &[]);
    assert!(
        output.status.success(),
        "status {:?}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["identity"]["width"], 384);
    assert!(report["thumbhash"].is_string());
    assert_eq!(report["dominant_colors"].as_array().unwrap().len(), 5);
    for output in report["outputs"].as_array().unwrap() {
        let path = tmp.path().join(output["name"].as_str().unwrap());
        let bytes = std::fs::read(path).unwrap();
        assert_eq!(output["bytes"], bytes.len());
        assert_eq!(output["width"], 192);
        assert_eq!(output["height"], 128);
        assert_eq!(output["blake3"], blake3::hash(&bytes).to_hex().as_str());
    }
}
#[test]
fn rejected_inputs_and_outputs_exit_two_without_report() {
    let tmp = tempfile::tempdir().unwrap();
    for input in [
        fixture("malicious/bomb-png.png"),
        tmp.path().join("missing"),
    ] {
        let result = invoke(&job(&input, tmp.path()), &[]);
        assert_eq!(result.status.code(), Some(2));
        assert_eq!(result.stdout, b"");
        assert_eq!(
            result
                .stderr
                .split(|b| *b == b'\n')
                .filter(|s| !s.is_empty())
                .count(),
            1
        );
    }
    for name in [
        "../outside.jpg",
        "/tmp/outside.jpg",
        "a/../../outside.jpg",
        ".",
        "a/b.jpg",
        "",
    ] {
        let mut job = job(&fixture("chart-srgb.png"), tmp.path());
        job["outputs"][0]["name"] = json!(name);
        assert_eq!(invoke(&job, &[]).status.code(), Some(2), "{name}");
    }
    std::os::unix::fs::symlink(tmp.path().join("missing"), tmp.path().join("medium.avif")).unwrap();
    assert_eq!(
        invoke(&job(&fixture("chart-srgb.png"), tmp.path()), &[])
            .status
            .code(),
        Some(2)
    );
}
#[test]
fn malformed_and_oversize_jobs_are_rejected() {
    for body in [vec![b'{'], vec![b' '; (1 << 20) + 1]] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_yhotos-codec-host"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let _ = child.stdin.take().unwrap().write_all(&body);
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(output.stdout, b"");
    }
}
#[cfg(all(target_os = "linux", debug_assertions))]
#[test]
fn seccomp_blocks_network() {
    let result = Command::new(env!("CARGO_BIN_EXE_yhotos-codec-host"))
        .arg("--self-test-network")
        .output()
        .unwrap();
    assert!(result.status.success());
    assert_eq!(result.stdout, b"");
}
#[cfg(all(target_os = "linux", debug_assertions))]
#[test]
fn cpu_timeout_exits_four() {
    let tmp = tempfile::tempdir().unwrap();
    let mut j = job(&fixture("chart-srgb.png"), tmp.path());
    j["limits"] = json!({"cpu_seconds":1});
    let result = invoke(&j, &["--self-test-timeout"]);
    assert_eq!(result.status.code(), Some(4));
    assert_eq!(result.stdout, b"");
    assert_eq!(result.stderr, b"codec timeout\n");
}

#[cfg(all(target_os = "linux", debug_assertions))]
#[test]
fn address_space_allocation_abort_is_a_failed_decode() {
    use std::os::unix::process::ExitStatusExt;
    let tmp = tempfile::tempdir().unwrap();
    let mut j = job(&fixture("chart-srgb.png"), tmp.path());
    // Without the tightened address-space limit the allocation succeeds and
    // the debug probe returns its explicit "limit not enforced" rejection.
    assert_eq!(invoke(&j, &["--self-test-memory"]).status.code(), Some(2));
    j["limits"] = json!({"max_memory":64 << 20});
    let parent = std::process::id().to_string();
    let direct = invoke(&j, &["--sandboxed-job", &parent, "--self-test-memory"]);
    assert_eq!(direct.status.signal(), Some(nix::libc::SIGABRT));
    assert_eq!(direct.stdout, b"");
    assert!(
        direct
            .stderr
            .starts_with(b"memory allocation of 134217728 bytes failed")
    );
    let result = invoke(&j, &["--self-test-memory"]);
    assert_eq!(result.status.code(), Some(3));
    assert_eq!(result.stdout, b"");
    assert_eq!(result.stderr, b"image processing failed\n");
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
}

#[test]
fn corrupt_pixel_stream_exits_three_without_decoder_diagnostics() {
    let tmp = tempfile::tempdir().unwrap();
    let source = std::fs::read(fixture("progressive.jpg")).unwrap();
    let scan = source.windows(2).position(|w| w == [0xff, 0xda]).unwrap();
    let length = usize::from(u16::from_be_bytes([source[scan + 2], source[scan + 3]]));
    let mut bytes = source[..scan + 2 + length].to_vec();
    bytes.extend([0xff, 0xd9]);
    let input = tmp.path().join("corrupt.jpg");
    std::fs::write(&input, bytes).unwrap();
    let out = tmp.path().join("out");
    std::fs::create_dir(&out).unwrap();
    let output = invoke(&job(&input, &out), &[]);
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"");
    assert_eq!(output.stderr, b"image processing failed\n");
}

#[cfg(all(target_os = "linux", debug_assertions))]
#[test]
fn seccomp_blocks_cross_process_signals_and_limits() {
    // The test runner is live, same-UID, and its thread-group leader exists.
    // Unfiltered signal 0 and read-only prlimit are permitted for this target.
    let result = Command::new(env!("CARGO_BIN_EXE_yhotos-codec-host"))
        .arg(format!("--self-test-process={}", std::process::id()))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"");
    assert_eq!(result.stderr, b"");
}

#[cfg(all(target_os = "linux", debug_assertions))]
#[test]
fn filesystem_confines_reads_writes_truncation_and_symlink_escape() {
    let root = tempfile::tempdir().unwrap();
    let out = root.path().join("out");
    std::fs::create_dir(&out).unwrap();
    let unrelated = root.path().join("unrelated");
    std::fs::write(&unrelated, b"unrelated original").unwrap();
    std::os::unix::fs::symlink(&unrelated, out.join("escape")).unwrap();
    let result = invoke(
        &job(&fixture("chart-srgb.png"), &out),
        &["--self-test-filesystem"],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(std::fs::read(&unrelated).unwrap(), b"unrelated original");
    assert!(!root.path().join("created").exists());
    assert_eq!(std::fs::metadata(out.join("allowed")).unwrap().len(), 5);
}

#[cfg(target_os = "linux")]
#[test]
fn filesystem_rejects_input_inside_writable_output() {
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("original.png");
    std::fs::copy(fixture("chart-srgb.png"), &input).unwrap();
    let result = invoke(&job(&input, root.path()), &[]);
    assert_eq!(result.status.code(), Some(2));
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}
