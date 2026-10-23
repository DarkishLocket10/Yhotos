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
fn input_failures_and_invalid_outputs_have_classified_exits() {
    let tmp = tempfile::tempdir().unwrap();
    for (input, code) in [
        (fixture("malicious/bomb-png.png"), 2),
        (tmp.path().join("missing"), 3),
    ] {
        let result = invoke(&job(&input, tmp.path()), &[]);
        assert_eq!(result.status.code(), Some(code));
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

#[cfg(feature = "test-hooks")]
#[test]
fn identity_only_reports_without_any_decode_call() {
    let tmp = tempfile::tempdir().unwrap();
    // Retain JPEG marker headers and EOI but remove the entropy-coded pixels.
    let bytes = std::fs::read(fixture("photo.jpg")).unwrap();
    let scan = bytes.windows(2).position(|b| b == [0xff, 0xda]).unwrap();
    let scan_len = u16::from_be_bytes([bytes[scan + 2], bytes[scan + 3]]) as usize;
    let mut truncated = bytes[..scan + 2 + scan_len].to_vec();
    truncated.extend([0xff, 0xd9]);
    let input = tmp.path().join("data");
    std::fs::write(&input, truncated).unwrap();
    let out = tmp.path().join("out");
    std::fs::create_dir(&out).unwrap();
    let identity = json!({"input":input,"out_dir":out,"outputs":[],"want":["identity"]});
    let output = invoke(&identity, &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["identity"]["kind"], "Jpeg");
    assert_eq!(report["timings"]["decode_calls"], 0);
    let mut decode = job(&input, &out);
    decode["outputs"] = json!([]);
    let decoded = invoke(&decode, &[]);
    assert!(!decoded.status.success());
}

#[cfg(feature = "test-hooks")]
#[test]
fn video_identity_and_metadata_do_not_decode() {
    for name in ["header-only.mp4", "header-only.mov"] {
        let tmp = tempfile::tempdir().unwrap();
        let request = json!({"input":fixture(name),"out_dir":tmp.path(),"outputs":[],"want":["identity","metadata"]});
        let output = invoke(&request, &[]);
        assert!(output.status.success());
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["timings"]["decode_calls"], 0);
        assert_eq!(report["identity"]["width"], 320);
        assert_eq!(report["metadata"]["orientation"], 1);
    }
}

fn hdr_job(input: &Path, out: &Path) -> Value {
    let mut outputs = Vec::new();
    for (variant, edge, quality) in [
        ("medium", 1280, 90),
        ("large", 2048, 99),
        ("xlarge", 3200, 99),
    ] {
        for format in ["Jpeg", "Jxl"] {
            for hdr in [false, true] {
                let encode = if format == "Jpeg" {
                    json!({"Jpeg":{"quality":quality,"progressive":true,"chroma_subsampling":false,"hdr":hdr}})
                } else {
                    json!({"Jxl":{"distance":0.5,"effort":3,"hdr":hdr}})
                };
                outputs.push(json!({"name":format!("{variant}.{}{}",format.to_lowercase(),if hdr {".hdr"} else {""}),"resize":{"long_edge":edge,"square":false},"encode":encode,"keep_icc":true}));
            }
        }
    }
    outputs.push(json!({"name":"medium.avif.hdr","resize":{"long_edge":1280,"square":false},"encode":{"Avif":{"quality":90,"speed":6,"hdr":true}}}));
    json!({"input":input,"out_dir":out,"outputs":outputs,"want":["identity","metadata","thumbhash","dominant_colors"]})
}
fn hdr_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/hdr")
        .join(name)
        .canonicalize()
        .unwrap()
}
#[test]
fn required_hdr_container_fixtures_produce_planned_jpegs() {
    for name in [
        "iso.jpg",
        "adobe.jpg",
        "apple.heic",
        "apple-element.heic",
        "gain-map.jxl",
        "../images/gain-map.avif",
    ] {
        let out = tempfile::tempdir().unwrap();
        let result = invoke(&hdr_job(&hdr_fixture(name), out.path()), &[]);
        assert!(
            result.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let report: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert!(report["hdr_skipped_reason"].is_null(), "{name}: {report}");
        assert_eq!(report["timings"]["hdr_base_reuses"], 6);
        assert_eq!(report["outputs"].as_array().unwrap().len(), 12);
        assert_eq!(
            report["skipped_outputs"],
            json!([{"name":"medium.avif.hdr","reason":"encoder_unavailable"}])
        );
        for variant in ["medium", "large", "xlarge"] {
            let output = std::fs::read(out.path().join(format!("{variant}.jpeg.hdr"))).unwrap();
            assert!(output.windows(6).any(|b| b == b"hdrgm:"));
            assert!(
                output
                    .windows(b"urn:iso:std:iso:ts:21496".len())
                    .any(|b| b == b"urn:iso:std:iso:ts:21496")
            );
        }
    }
}
#[test]
fn ordinary_sources_have_no_hdr_outputs_or_photo_skip_reason() {
    let out = tempfile::tempdir().unwrap();
    let result = invoke(&hdr_job(&fixture("chart-srgb.png"), out.path()), &[]);
    assert!(result.status.success());
    let report: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert!(report["hdr_skipped_reason"].is_null());
    assert_eq!(report["outputs"].as_array().unwrap().len(), 6);
    assert!(
        report["skipped_outputs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|o| o["reason"] == "no_gain_map")
    );
    assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 6);
}
#[test]
fn compressed_jxl_gain_map_preserves_sdr_and_reports_unsupported_reason() {
    let out = tempfile::tempdir().unwrap();
    let result = invoke(&hdr_job(&hdr_fixture("gain-map-brob.jxl"), out.path()), &[]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report: Value = serde_json::from_slice(&result.stdout).unwrap();
    let reason = "compressed JPEG XL gain map not supported";
    assert_eq!(report["identity"]["has_gain_map"], true);
    assert_eq!(report["hdr_skipped_reason"], reason);
    let outputs = report["outputs"].as_array().unwrap();
    assert_eq!(outputs.len(), 6);
    for output in outputs {
        let name = output["name"].as_str().unwrap();
        let path = out.path().join(name);
        assert_ne!(path.extension(), Some("hdr".as_ref()));
        assert!(std::fs::metadata(&path).unwrap().len() > 0);
        assert!(
            !yhotos_codec::identify(&path, &yhotos_codec::Limits::default())
                .unwrap()
                .has_gain_map
        );
    }
    let skipped = report["skipped_outputs"].as_array().unwrap();
    assert_eq!(skipped.len(), 7);
    assert!(skipped.iter().all(|o| o["reason"] == reason));
    assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 6);
}
#[test]
#[allow(clippy::too_many_lines)] // Keep malformed inputs beside their shared SDR/report contract.
fn malformed_gain_metadata_never_fails_an_sdr_ingest() {
    let iso = std::fs::read(hdr_fixture("iso.jpg")).unwrap();
    let adobe = std::fs::read(hdr_fixture("adobe.jpg")).unwrap();
    let apple = std::fs::read(hdr_fixture("apple.heic")).unwrap();
    let avif = std::fs::read(fixture("gain-map.avif")).unwrap();
    let mut cases = Vec::new();
    let ns = b"urn:iso:std:iso:ts:21496:-1\0";
    let at = iso.windows(ns.len()).rposition(|b| b == ns).unwrap() + ns.len();
    for (name, offset, value) in [
        ("iso_version", 1, 1),
        ("iso_headroom_denominator", 8, 0),
        ("iso_gamma", 28, 0),
    ] {
        let mut bytes = iso.clone();
        bytes[at + offset] = value;
        cases.push((name, bytes));
    }
    // This fixture's big-endian MPF has its two-entry table at TIFF offset 50.
    let tiff = iso.windows(4).position(|b| b == b"MPF\0").unwrap() + 4;
    assert_eq!(&iso[tiff..tiff + 2], b"MM");
    let entry = tiff + 50 + 16;
    let auxiliary =
        tiff + u32::from_be_bytes(iso[entry + 8..entry + 12].try_into().unwrap()) as usize;
    for (name, field, value) in [
        ("mpf_zero_offset", entry + 8, 0),
        ("mpf_overlapping_offset", entry + 8, 1),
        ("mpf_outside_offset", entry + 8, u32::MAX),
        ("mpf_outside_size", entry + 4, u32::MAX),
        ("mpf_table_offset", tiff + 8 + 2 + 24 + 8, u32::MAX),
        ("mpf_image_count", tiff + 8 + 2 + 12 + 8, 65),
    ] {
        let mut bytes = iso.clone();
        bytes[field..field + 4].copy_from_slice(&value.to_be_bytes());
        cases.push((name, bytes));
    }
    for (name, offset, value) in [
        ("auxiliary_soi", auxiliary, 0),
        ("auxiliary_marker", auxiliary + 2, 0),
        ("auxiliary_segment_length", auxiliary + 4, 255),
        ("auxiliary_eoi", iso.len() - 1, 0),
    ] {
        let mut bytes = iso.clone();
        bytes[offset] = value;
        cases.push((name, bytes));
    }
    let mut bytes = iso.clone();
    bytes[entry + 8..entry + 12].copy_from_slice(&0_u32.to_be_bytes());
    *bytes.last_mut().unwrap() = 0;
    cases.push(("mpf_zero_offset_and_auxiliary_eoi", bytes));
    for (name, pattern, last) in [
        ("adobe_version", b"hdrgm:Version=\"1.0\"".as_slice(), b'2'),
        ("adobe_gamma", b"hdrgm:Gamma=\"1\"".as_slice(), b'0'),
        ("adobe_max", b"hdrgm:GainMapMax=\"2\"".as_slice(), b'x'),
    ] {
        let mut bytes = adobe.clone();
        let at = bytes
            .windows(pattern.len())
            .rposition(|b| b == pattern)
            .unwrap();
        let value = at + pattern.iter().position(|b| *b == b'"').unwrap() + 1;
        bytes[value] = last;
        cases.push((name, bytes));
    }
    for (name, mut bytes) in [
        ("apple_version", apple.clone()),
        (
            "apple_element_version",
            std::fs::read(hdr_fixture("apple-element.heic")).unwrap(),
        ),
    ] {
        let at = bytes.windows(5).position(|b| b == b"65536").unwrap();
        bytes[at] = b'9';
        cases.push((name, bytes));
    }
    let mut bytes = apple;
    let at = bytes.windows(10).position(|b| b == b"Apple iOS\0").unwrap();
    bytes[at + 24..at + 28].copy_from_slice(&f32::NAN.to_bits().to_be_bytes());
    cases.push(("apple_headroom", bytes));
    let pattern = b"\0\0\0\0\0\x40\0\0\0\0\0\0\0\x01\0\0\0\x02";
    let at = avif
        .windows(pattern.len())
        .position(|b| b == pattern)
        .unwrap();
    let mut bytes = avif;
    bytes[at + 13] = 0;
    cases.push(("tmap_denominator", bytes));
    let mut bytes = std::fs::read(hdr_fixture("gain-map.jxl")).unwrap();
    let at = bytes.windows(4).position(|b| b == b"jhgm").unwrap();
    bytes[at + 4] = 1;
    cases.push(("jhgm_version", bytes));
    for (name, bytes) in cases {
        let input = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(input.path(), bytes).unwrap();
        let out = tempfile::tempdir().unwrap();
        let result = invoke(&hdr_job(input.path(), out.path()), &[]);
        assert!(
            result.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let report: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert!(
            report["identity"]["has_gain_map"].as_bool().unwrap(),
            "{name}"
        );
        assert!(!report["hdr_skipped_reason"].is_null(), "{name}: {report}");
        assert_eq!(report["outputs"].as_array().unwrap().len(), 6, "{name}");
        assert!(
            report["outputs"]
                .as_array()
                .unwrap()
                .iter()
                .all(|o| Path::new(o["name"].as_str().unwrap()).extension() != Some("hdr".as_ref())),
            "{name}: {report}"
        );
        assert_eq!(
            report["skipped_outputs"].as_array().unwrap().len(),
            7,
            "{name}"
        );
        assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 6, "{name}");
    }
}
