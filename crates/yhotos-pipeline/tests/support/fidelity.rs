//! Independent CLI oracle over the files published by the actual default ingest.
#![allow(
    clippy::indexing_slicing,
    clippy::cast_precision_loss,
    clippy::float_cmp,
    clippy::manual_midpoint
)]
use super::{assert_ready, empty_tmp, fixture, setup};
use base64::{Engine, engine::general_purpose::STANDARD};
use sqlx::PgPool;
use std::{ffi::OsStr, path::Path, process::Command};
use yhotos_codec::{Limits, identify};
use yhotos_pipeline::VariantPlan;
use yhotos_storage::{ContentHash, DerivativeKey};

#[sqlx::test(migrations = "../yhotos-db/migrations")]
async fn production_plan_meets_photograph_and_p3_fidelity_gates(pool: PgPool) {
    let mut failures = Vec::new();
    for name in ["photo.jpg", "photo-large.jpg", "chart-p3.png"] {
        let (root, ingest, id) = setup(&pool, name).await;
        let start = std::time::Instant::now();
        ingest.ingest(id).await.unwrap();
        eprintln!("default ingest {name}: {:?}", start.elapsed());
        assert_ready(&pool, &ingest, id).await;
        let rows = sqlx::query!("SELECT variant,format,hash,width,height,bytes FROM derivatives WHERE photo_id=$1 ORDER BY variant,format", id.0)
            .fetch_all(&pool).await.unwrap();
        let source = fixture(name);
        let source_id = identify(&source, &Limits::default()).unwrap();
        let source_colorimetry = source_id.has_icc.then(|| profile_colorimetry(&source));
        for row in rows {
            let plan = VariantPlan::default();
            let variant = plan
                .variants
                .iter()
                .find(|v| row.variant == v.name || row.variant == format!("{}2x", v.name))
                .unwrap();
            let edge = variant.long_edge * if row.variant.ends_with("2x") { 2 } else { 1 };
            // Keep the existing photograph for smaller variants, but exercise
            // medium2x/large/xlarge at their requested sizes, without upscaling.
            if (name == "photo.jpg" && edge > 1600) || (name == "photo-large.jpg" && edge <= 1600) {
                continue;
            }
            let hash: ContentHash = row.hash.parse().unwrap();
            let output = ingest
                .store
                .path_derivative(&DerivativeKey {
                    hash: &hash,
                    variant: &row.variant,
                    format: &row.format,
                    hdr: false,
                })
                .unwrap();
            // Re-identify and decode each measured file at full resolution.
            let output_id = identify(&output, &Limits::default()).unwrap();
            if name != "chart-p3.png" {
                assert_eq!(
                    output_id.width.unwrap().max(output_id.height.unwrap()),
                    edge,
                    "{name} {} {} was not measured at its true size",
                    row.variant,
                    row.format
                );
            }
            assert_eq!(
                (output_id.width.unwrap(), output_id.height.unwrap()),
                (
                    u32::try_from(row.width).unwrap(),
                    u32::try_from(row.height).unwrap()
                )
            );
            if row.format != "jpeg" {
                assert!(
                    output_id.has_icc,
                    "{name} {} {} lost ICC",
                    row.variant, row.format
                );
                if let Some(expected) = source_colorimetry {
                    // JXL synthesizes equivalent ICC profiles with platform-dependent
                    // descriptions. Compare D50 PCS primaries and white point instead.
                    let actual = profile_colorimetry(&output);
                    for (actual, expected) in actual
                        .into_iter()
                        .flatten()
                        .zip(expected.into_iter().flatten())
                    {
                        assert!(
                            (actual - expected).abs() < 0.001,
                            "{name} {} {} changed ICC colorimetry: {actual} vs {expected}",
                            row.variant,
                            row.format
                        );
                    }
                }
            }
            let oracle = Oracle::new(&source, edge, variant.square);
            let score = oracle.ssim(&output);
            let (mean, max) = if name == "chart-p3.png" {
                oracle.chart_delta_e(&output)
            } else {
                (0.0, 0.0)
            };
            eprintln!(
                "fidelity | {name} | {} | {} | {}x{} | {} | {score:.6} | {mean:.4} | {max:.4} |",
                row.variant, row.format, row.width, row.height, row.bytes
            );
            if !(score.is_finite()
                && score
                    >= threshold(edge)
                        + if cfg!(target_os = "macos") {
                            0.002
                        } else {
                            0.0
                        }
                && mean.is_finite()
                && mean < 1.0
                && max.is_finite()
                && max < 2.5)
            {
                failures.push(format!(
                    "{name} {} {}: SSIM={score:.6}, deltaE mean={mean:.4}, max={max:.4}",
                    row.variant, row.format
                ));
            }
        }
        empty_tmp(root.path()).await;
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn profile_colorimetry(image: &Path) -> [[f64; 3]; 4] {
    let output = Command::new("vipsheader")
        .args(["-f", "icc-profile-data"])
        .arg(image)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "ICC extraction failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let profile = STANDARD
        .decode(String::from_utf8(output.stdout).unwrap().trim())
        .unwrap();
    icc_colorimetry(&profile)
}

fn icc_colorimetry(profile: &[u8]) -> [[f64; 3]; 4] {
    let u32_at = |bytes: &[u8], at| u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
    let count = usize::try_from(u32_at(profile, 128)).unwrap();
    [b"rXYZ", b"gXYZ", b"bXYZ", b"wtpt"].map(|signature| {
        let tag = profile[132..132 + count * 12]
            .as_chunks::<12>()
            .0
            .iter()
            .find(|tag| tag[..4] == *signature)
            .unwrap();
        let offset = usize::try_from(u32_at(tag, 4)).unwrap();
        assert!(u32_at(tag, 8) >= 20);
        let xyz = &profile[offset..offset + 20];
        assert_eq!(&xyz[..4], b"XYZ ");
        std::array::from_fn(|i| {
            f64::from(i32::from_be_bytes(
                xyz[8 + i * 4..12 + i * 4].try_into().unwrap(),
            )) / 65536.0
        })
    })
}

#[test]
fn colorimetry_comparison_detects_a_p3_to_srgb_profile_change() {
    let p3 = icc_colorimetry(&std::fs::read(fixture("icc/p3.icc")).unwrap());
    let srgb = icc_colorimetry(&std::fs::read(fixture("icc/srgb.icc")).unwrap());
    assert!(
        p3.into_iter()
            .flatten()
            .zip(srgb.into_iter().flatten())
            .any(|(a, b)| (a - b).abs() > 0.001)
    );
}

fn threshold(edge: u32) -> f64 {
    if edge >= 2048 { 0.98 } else { 0.97 }
}

fn vips(args: &[&OsStr]) {
    let result = Command::new("vips")
        .env("VIPS_CONCURRENCY", "2")
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "vips {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

struct Oracle {
    dir: tempfile::TempDir,
    width: usize,
    height: usize,
    rgb: Vec<u8>,
    square: bool,
}
impl Oracle {
    fn new(source: &Path, edge: u32, square: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cropped = dir.path().join("crop.v");
        let id = identify(source, &Limits::default()).unwrap();
        let source = if square {
            let side = id.width.unwrap().min(id.height.unwrap());
            vips(&[
                "extract_area".as_ref(),
                source.as_os_str(),
                cropped.as_os_str(),
                ((id.width.unwrap() - side) / 2).to_string().as_ref(),
                ((id.height.unwrap() - side) / 2).to_string().as_ref(),
                side.to_string().as_ref(),
                side.to_string().as_ref(),
            ]);
            cropped.as_path()
        } else {
            source
        };
        let reference = dir.path().join("reference.png");
        // A separate high-quality linear-light thumbnail of the original. PNG
        // is lossless; no production codec/resize implementation builds this.
        vips(&[
            "thumbnail".as_ref(),
            source.as_os_str(),
            reference.as_os_str(),
            edge.to_string().as_ref(),
            "--height".as_ref(),
            edge.to_string().as_ref(),
            "--size=down".as_ref(),
            "--linear".as_ref(),
        ]);
        let id = identify(&reference, &Limits::default()).unwrap();
        let mut oracle = Self {
            dir,
            width: id.width.unwrap() as usize,
            height: id.height.unwrap() as usize,
            rgb: Vec::new(),
            square,
        };
        oracle.rgb = oracle.rgb(&reference);
        oracle
    }
    fn rgb(&self, image: &Path) -> Vec<u8> {
        let decoded = self.dir.path().join("rgb.v");
        let raw = self.dir.path().join("rgb.raw");
        vips(&[
            "icc_transform".as_ref(),
            image.as_os_str(),
            decoded.as_os_str(),
            fixture("icc/srgb.icc").as_os_str(),
            "--embedded".as_ref(),
            "--input-profile".as_ref(),
            fixture("icc/srgb.icc").as_os_str(),
            "--depth=8".as_ref(),
        ]);
        vips(&["rawsave".as_ref(), decoded.as_os_str(), raw.as_os_str()]);
        let pixels = std::fs::read(raw).unwrap();
        assert_eq!(pixels.len(), self.width * self.height * 3);
        pixels
    }
    fn ssim(&self, image: &Path) -> f64 {
        let actual = self.rgb(image);
        let mut total = 0.0;
        let mut windows = 0;
        // Full-resolution local 8x8 luminance SSIM, including partial border
        // windows. Neither a global average nor a downsample can hide detail.
        for y in (0..self.height).step_by(8) {
            for x in (0..self.width).step_by(8) {
                let mut a = Vec::new();
                let mut b = Vec::new();
                for dy in y..(y + 8).min(self.height) {
                    for dx in x..(x + 8).min(self.width) {
                        let at = (dy * self.width + dx) * 3;
                        let luma = |pixels: &[u8]| {
                            0.2126 * f64::from(pixels[at])
                                + 0.7152 * f64::from(pixels[at + 1])
                                + 0.0722 * f64::from(pixels[at + 2])
                        };
                        a.push(luma(&self.rgb));
                        b.push(luma(&actual));
                    }
                }
                total += ssim(&a, &b);
                windows += 1;
            }
        }
        total / f64::from(windows)
    }
    fn lab(&self, image: &Path) -> Vec<[f64; 3]> {
        let decoded = self.dir.path().join("lab.v");
        let raw = self.dir.path().join("lab.raw");
        vips(&[
            "icc_import".as_ref(),
            image.as_os_str(),
            decoded.as_os_str(),
            "--embedded".as_ref(),
            "--input-profile".as_ref(),
            fixture("icc/srgb.icc").as_os_str(),
            "--pcs=lab".as_ref(),
        ]);
        vips(&["rawsave".as_ref(), decoded.as_os_str(), raw.as_os_str()]);
        let bytes = std::fs::read(raw).unwrap();
        assert_eq!(bytes.len(), self.width * self.height * 12);
        bytes
            .as_chunks::<12>()
            .0
            .iter()
            .map(|p| {
                std::array::from_fn(|i| {
                    f64::from(f32::from_ne_bytes(p[i * 4..i * 4 + 4].try_into().unwrap()))
                })
            })
            .collect()
    }
    fn chart_delta_e(&self, image: &Path) -> (f64, f64) {
        // Use the chart's known D50 Lab values, so a color error introduced by
        // the reference thumbnail cannot mask the same error in a derivative.
        let sidecar: serde_json::Value =
            serde_json::from_slice(&std::fs::read(fixture("chart.json")).unwrap()).unwrap();
        let expected: Vec<[f64; 3]> = sidecar["lab_d50"]
            .as_array()
            .unwrap()
            .iter()
            .map(|lab| std::array::from_fn(|i| lab[i].as_f64().unwrap()))
            .collect();
        assert_eq!(expected.len(), 24);
        let actual = self.lab(image);
        let mut errors = Vec::new();
        // Sample the central 9x9 of each retained chart patch. Square variants
        // center-crop columns 0 and 5; all other variants retain all 24 patches.
        let columns = if self.square { 1..5 } else { 0..6 };
        let source_width = if self.square { 256 } else { 384 };
        for row in 0..4 {
            for col in columns.clone() {
                let x = ((col * 64 + 32 - if self.square { 64 } else { 0 }) * self.width)
                    / source_width;
                let y = ((row * 64 + 32) * self.height) / 256;
                let mut b = [0.0; 3];
                for yy in y - 4..=y + 4 {
                    for xx in x - 4..=x + 4 {
                        for channel in 0..3 {
                            b[channel] += actual[yy * self.width + xx][channel] / 81.0;
                        }
                    }
                }
                errors.push(delta_e(expected[row * 6 + col], b));
            }
        }
        (
            errors.iter().sum::<f64>() / errors.len() as f64,
            errors.iter().copied().fold(0.0, f64::max),
        )
    }
}

fn ssim(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len() as f64;
    let ma = a.iter().sum::<f64>() / n;
    let mb = b.iter().sum::<f64>() / n;
    let denominator = (n - 1.0).max(1.0);
    let va = a.iter().map(|v| (v - ma).powi(2)).sum::<f64>() / denominator;
    let vb = b.iter().map(|v| (v - mb).powi(2)).sum::<f64>() / denominator;
    let cov = a
        .iter()
        .zip(b)
        .map(|(x, y)| (x - ma) * (y - mb))
        .sum::<f64>()
        / denominator;
    ((2.0 * ma * mb + 6.5025) * (2.0 * cov + 58.5225))
        / ((ma * ma + mb * mb + 6.5025) * (va + vb + 58.5225))
}

// CIEDE2000 (Sharma/Wu/Dalal), including neutral and hue-wrap branches.
fn delta_e(a: [f64; 3], b: [f64; 3]) -> f64 {
    use std::f64::consts::PI;
    let cm = (a[1].hypot(a[2]) + b[1].hypot(b[2])) / 2.0;
    let g = 0.5 * (1.0 - (cm.powi(7) / (cm.powi(7) + 25_f64.powi(7))).sqrt());
    let ap1 = (1.0 + g) * a[1];
    let ap2 = (1.0 + g) * b[1];
    let cp1 = ap1.hypot(a[2]);
    let cp2 = ap2.hypot(b[2]);
    let hue = |x: f64, y: f64| y.atan2(x).rem_euclid(2.0 * PI);
    let h1 = hue(ap1, a[2]);
    let h2 = hue(ap2, b[2]);
    let dl = b[0] - a[0];
    let dc = cp2 - cp1;
    let mut dh = h2 - h1;
    if cp1 * cp2 == 0.0 {
        dh = 0.0;
    } else if dh > PI {
        dh -= 2.0 * PI;
    } else if dh < -PI {
        dh += 2.0 * PI;
    }
    let dh = 2.0 * (cp1 * cp2).sqrt() * (dh / 2.0).sin();
    let lm = (a[0] + b[0]) / 2.0;
    let cm = (cp1 + cp2) / 2.0;
    let hm = if cp1 * cp2 == 0.0 {
        h1 + h2
    } else if (h1 - h2).abs() <= PI {
        (h1 + h2) / 2.0
    } else if h1 + h2 < 2.0 * PI {
        (h1 + h2 + 2.0 * PI) / 2.0
    } else {
        (h1 + h2 - 2.0 * PI) / 2.0
    };
    let t = 1.0 - 0.17 * (hm - PI / 6.0).cos()
        + 0.24 * (2.0 * hm).cos()
        + 0.32 * (3.0 * hm + PI / 30.0).cos()
        - 0.20 * (4.0 * hm - 63.0 * PI / 180.0).cos();
    let sl = 1.0 + 0.015 * (lm - 50.0).powi(2) / (20.0 + (lm - 50.0).powi(2)).sqrt();
    let sc = 1.0 + 0.045 * cm;
    let sh = 1.0 + 0.015 * cm * t;
    let rt = -2.0
        * (cm.powi(7) / (cm.powi(7) + 25_f64.powi(7))).sqrt()
        * (PI / 3.0 * (-((hm.to_degrees() - 275.0) / 25.0).powi(2)).exp()).sin();
    ((dl / sl).powi(2) + (dc / sc).powi(2) + (dh / sh).powi(2) + rt * (dc / sc) * (dh / sh)).sqrt()
}

#[test]
fn oracle_metrics_have_known_reference_values() {
    assert!((delta_e([50.0, 2.6772, -79.7751], [50.0, 0.0, -82.7485]) - 2.0425).abs() < 0.0001);
    assert_eq!(
        ssim(&[0.0, 64.0, 128.0, 255.0], &[0.0, 64.0, 128.0, 255.0]),
        1.0
    );
    assert!(ssim(&[0.0, 255.0, 0.0, 255.0], &[255.0, 0.0, 255.0, 0.0]) < 0.0);
}
