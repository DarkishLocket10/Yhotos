#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::manual_midpoint,
    clippy::float_cmp
)]
use super::*;
use std::{path::PathBuf, process::Command, time::Instant};
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/images")
        .join(name)
}
fn load(name: &str) -> Working {
    let p = fixture(name);
    let id = identify(&p, &Limits::default()).unwrap();
    decode(&p, &id, &Limits::default()).unwrap()
}
fn run(args: &[&std::ffi::OsStr]) {
    let status = Command::new("vips").args(args).status().unwrap();
    assert!(status.success());
}
fn lab(path: &Path) -> Vec<[f64; 3]> {
    let tmp = tempfile::tempdir().unwrap();
    let image = tmp.path().join("lab.v");
    let raw = tmp.path().join("lab.raw");
    run(&[
        "icc_import".as_ref(),
        path.as_os_str(),
        image.as_os_str(),
        "--embedded".as_ref(),
        "--pcs=lab".as_ref(),
    ]);
    run(&["rawsave".as_ref(), image.as_os_str(), raw.as_os_str()]);
    let bytes = std::fs::read(raw).unwrap();
    bytes
        .as_chunks::<12>()
        .0
        .iter()
        .map(|c| {
            std::array::from_fn(|i| {
                f64::from(f32::from_ne_bytes(c[i * 4..i * 4 + 4].try_into().unwrap()))
            })
        })
        .collect()
}
#[test]
fn identify_fixture_matrix() {
    let cases = [
        ("exif-heavy.jpg", Kind::Jpeg, 384, 256, 8, false, true),
        ("landscape.jpg", Kind::Jpeg, 768, 512, 8, false, false),
        ("photo.jpg", Kind::Jpeg, 1600, 1236, 8, false, false),
        ("chart-srgb.png", Kind::Png, 384, 256, 8, false, true),
        ("chart-p3.png", Kind::Png, 384, 256, 8, false, true),
        ("chart-adobergb.jpg", Kind::Jpeg, 384, 256, 8, false, true),
        ("alpha.png", Kind::Png, 384, 256, 8, true, false),
        ("16bit.png", Kind::Png, 384, 256, 16, false, true),
        ("16bit.tif", Kind::Tiff, 384, 256, 16, false, true),
        ("gray.jpg", Kind::Jpeg, 384, 256, 8, false, false),
        ("cmyk.jpg", Kind::Jpeg, 384, 256, 8, false, true),
        ("progressive.jpg", Kind::Jpeg, 384, 256, 8, false, true),
        ("tiny-1x1.jpg", Kind::Jpeg, 1, 1, 8, false, false),
        ("wide-8000x100.jpg", Kind::Jpeg, 8000, 100, 8, false, false),
        ("chart.avif", Kind::Avif, 384, 256, 8, false, true),
        ("gain-map.avif", Kind::Avif, 32, 32, 8, false, false),
        ("float.tif", Kind::Tiff, 384, 256, 32, false, true),
        ("chart.jxl", Kind::Jxl, 384, 256, 8, false, true),
    ];
    for (name, kind, width, height, bits, alpha, icc) in cases {
        let id = identify(&fixture(name), &Limits::default()).unwrap();
        assert_eq!(
            (
                id.kind,
                id.width,
                id.height,
                id.bits,
                id.has_alpha,
                id.has_icc,
                id.orientation,
                id.pages
            ),
            (kind, width, height, bits, alpha, icc, 1, 1),
            "{name}"
        );
    }
    #[cfg(feature = "heif")]
    {
        let id = identify(&fixture("chart.heic"), &Limits::default()).unwrap();
        assert_eq!(
            (
                id.kind,
                id.width,
                id.height,
                id.orientation,
                id.bits,
                id.has_icc,
                id.has_alpha
            ),
            (Kind::Heif, 384, 256, 1, 8, true, false)
        );
    }
}
#[test]
fn malicious_inputs_are_bounded_and_typed() {
    for (name, expected) in [
        ("bomb-png.png", CodecError::Limits),
        ("truncated.jpg", CodecError::Mismatch),
        ("polyglot-jpg-html.jpg", CodecError::Mismatch),
        ("zero.jpg", CodecError::BadMagic),
        ("wrong-ext.png", CodecError::Mismatch),
        ("huge-dims-tiff.tif", CodecError::Limits),
    ] {
        let start = Instant::now();
        let path = fixture(&format!("malicious/{name}"));
        let result = identify(&path, &Limits::default())
            .and_then(|id| decode(&path, &id, &Limits::default()).map(|_| ()));
        assert_eq!(result, Err(expected), "{name}");
        assert!(start.elapsed().as_secs_f64() < 2.0, "{name}");
    }
    let limits = Limits {
        max_dim: 4096,
        ..Limits::default()
    };
    assert_eq!(
        identify(&fixture("wide-8000x100.jpg"), &limits),
        Err(CodecError::Limits)
    );
    let limits = Limits {
        max_bytes: 10,
        ..Limits::default()
    };
    assert_eq!(
        identify(&fixture("tiny-1x1.jpg"), &limits),
        Err(CodecError::Limits)
    );
}
#[test]
fn all_exif_orientations_produce_identical_pixels() {
    let reference = load("orientation-1.jpg");
    let pixels = reference.image.pixels().unwrap();
    for orientation in 1..=8 {
        let path = fixture(&format!("orientation-{orientation}.jpg"));
        let id = identify(&path, &Limits::default()).unwrap();
        assert_eq!(id.orientation, orientation);
        assert_eq!(
            (id.kind.clone(), id.bits, id.has_icc, id.has_alpha),
            (Kind::Jpeg, 8, false, false)
        );
        assert_eq!(
            (id.width, id.height),
            if orientation >= 5 {
                (96, 128)
            } else {
                (128, 96)
            }
        );
        let image = decode(&path, &id, &Limits::default()).unwrap();
        assert_eq!(
            image.dimensions().unwrap(),
            reference.dimensions().unwrap(),
            "orientation {orientation}"
        );
        assert_eq!(
            image.image.pixels().unwrap(),
            pixels,
            "orientation {orientation}"
        );
    }
}
fn encoders() -> [(Encode, &'static str); 3] {
    [
        (
            Encode::Avif {
                quality: 100,
                speed: 6,
                hdr: false,
            },
            "avif",
        ),
        (
            Encode::Jxl {
                distance: 0.0,
                effort: 3,
                hdr: false,
            },
            "jxl",
        ),
        (
            Encode::Jpeg {
                quality: 100,
                progressive: true,
                chroma_subsampling: false,
                hdr: false,
            },
            "jpg",
        ),
    ]
}
#[test]
fn charts_preserve_lab_and_profiles_in_every_encoder() {
    let tmp = tempfile::tempdir().unwrap();
    let sidecar: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture("chart.json")).unwrap()).unwrap();
    let reference: Vec<[f64; 3]> = sidecar["lab_d50"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| std::array::from_fn(|i| p[i].as_f64().unwrap()))
        .collect();
    for name in ["chart-srgb.png", "chart-p3.png", "chart-adobergb.jpg"] {
        let w = resize(
            &load(name),
            ResizeSpec {
                long_edge: 192,
                square: false,
            },
        )
        .unwrap();
        for (enc, suffix) in encoders() {
            let out = tmp.path().join(format!("out.{suffix}"));
            encode(
                &w,
                enc,
                EncodeOpts {
                    keep_icc: true,
                    strip_metadata: true,
                },
                &out,
            )
            .unwrap();
            let id = identify(&out, &Limits::default()).unwrap();
            assert!(id.has_icc, "{name} {suffix}");
            assert_eq!((id.width, id.height), (192, 128));
            let actual = lab(&out);
            let mut errors = Vec::new();
            for (i, target) in reference.iter().enumerate() {
                let x = i % 6 * 32 + 16;
                let y = i / 6 * 32 + 16;
                errors.push(delta_e(*target, actual[y * 192 + x]));
            }
            let mean = errors.iter().sum::<f64>() / 24.0;
            let max = errors.iter().copied().fold(0.0, f64::max);
            eprintln!("fidelity {name} {suffix}: mean={mean:.4}, max={max:.4}");
            assert!(
                mean < 1.0 && max < 2.5,
                "{name} {suffix}: mean={mean}, max={max}"
            );
        }
    }
}
#[test]
fn metadata_is_bounded_and_gps_never_survives() {
    let p = fixture("exif-heavy.jpg");
    let meta = metadata(&p).unwrap();
    assert_eq!(meta.make.as_deref(), Some("Yhotos"));
    assert_eq!(meta.model.as_deref(), Some("Fixture Camera"));
    assert_eq!(meta.lens.as_deref(), Some("Synthetic 50mm"));
    assert_eq!(meta.focal_mm, Some(50.0));
    assert_eq!(meta.aperture, Some(2.8));
    assert_eq!(meta.shutter, Some(1.0 / 125.0));
    assert_eq!(meta.iso, Some(200));
    assert_eq!(meta.taken_at.as_deref(), Some("2026:01:02 03:04:05"));
    let (lat, lon) = meta.gps.unwrap();
    assert!((lat - 43.65).abs() < 0.0001);
    assert!((lon + 79.383_333).abs() < 0.0001);
    assert_eq!(meta.rating, Some(4));
    assert_eq!(meta.title.unwrap().len(), 256);
    let w = load("exif-heavy.jpg");
    let tmp = tempfile::tempdir().unwrap();
    for (enc, suffix) in encoders() {
        let out = tmp.path().join(format!("out.{suffix}"));
        encode(
            &w,
            enc,
            EncodeOpts {
                keep_icc: true,
                strip_metadata: true,
            },
            &out,
        )
        .unwrap();
        let m = metadata(&out).unwrap();
        assert!(m.gps.is_none());
        assert!(m.make.is_none());
        assert!(m.title.is_none());
        assert_eq!(m.orientation, 1);
        let id = identify(&out, &Limits::default()).unwrap();
        assert_eq!(id.orientation, 1);
    }
}
#[test]
fn resize_alpha_no_upscale_hdr_and_srgb_fallback() {
    let w = load("alpha.png");
    assert!(w.alpha);
    let (_, _, pixels) = crate::rgba(&w, 100).unwrap();
    assert!(pixels.as_chunks::<4>().0.iter().any(|p| p[3] < 255));
    let original = w.dimensions().unwrap();
    assert_eq!(
        resize(
            &w,
            ResizeSpec {
                long_edge: 1000,
                square: false
            }
        )
        .unwrap()
        .dimensions()
        .unwrap(),
        original
    );
    let square = resize(
        &w,
        ResizeSpec {
            long_edge: 64,
            square: true,
        },
    )
    .unwrap();
    assert_eq!(square.dimensions().unwrap(), (64, 64));
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("out.jxl");
    encode(
        &square,
        Encode::Jxl {
            distance: 0.0,
            effort: 3,
            hdr: false,
        },
        EncodeOpts {
            keep_icc: true,
            strip_metadata: true,
        },
        &out,
    )
    .unwrap();
    assert!(identify(&out, &Limits::default()).unwrap().has_alpha);
    for enc in [
        Encode::Avif {
            quality: 60,
            speed: 6,
            hdr: true,
        },
        Encode::Jxl {
            distance: 1.0,
            effort: 3,
            hdr: true,
        },
        Encode::Jpeg {
            quality: 80,
            progressive: false,
            chroma_subsampling: true,
            hdr: true,
        },
    ] {
        assert!(matches!(
            encode(
                &w,
                enc,
                EncodeOpts {
                    keep_icc: true,
                    strip_metadata: true
                },
                &out
            ),
            Err(CodecError::Unsupported)
        ));
    }
    let out = tmp.path().join("out.jpg");
    encode(
        &load("chart-p3.png"),
        encoders()[2].0,
        EncodeOpts {
            keep_icc: false,
            strip_metadata: true,
        },
        &out,
    )
    .unwrap();
    let id = identify(&out, &Limits::default()).unwrap();
    assert!(
        id.icc_desc
            .is_none_or(|s| s.to_ascii_lowercase().contains("srgb"))
    );
    assert_ne!(thumbhash(&w).unwrap().len(), 0);
    assert_eq!(dominant_colors(&w, 5).unwrap().len(), 5);
}

#[test]
fn jpeg_alpha_is_flattened_against_white_with_icc_retained() {
    let w = load("alpha.png");
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("flattened.jpg");
    let reference = tmp.path().join("reference.png");
    run(&[
        "flatten".as_ref(),
        fixture("alpha.png").as_os_str(),
        reference.as_os_str(),
        "--background=255".as_ref(),
    ]);
    let reference = load_path(&reference);
    for keep_icc in [false, true] {
        encode(
            &w,
            Encode::Jpeg {
                quality: 100,
                progressive: false,
                chroma_subsampling: false,
                hdr: false,
            },
            EncodeOpts {
                keep_icc,
                strip_metadata: true,
            },
            &out,
        )
        .unwrap();
        let id = identify(&out, &Limits::default()).unwrap();
        assert!(!id.has_alpha);
        assert!(id.has_icc);
        let decoded = decode(&out, &id, &Limits::default()).unwrap();
        let rgb = |w: &Working| {
            w.image
                .extract(0, 3)
                .unwrap()
                .linear(1.0 / 256.0)
                .unwrap()
                .xyz()
                .unwrap()
                .export(None)
                .unwrap()
                .linear(1.0 / 257.0)
                .unwrap()
                .cast(native::UCHAR)
                .unwrap()
                .pixels()
                .unwrap()
        };
        let actual = rgb(&decoded);
        let expected = rgb(&reference);
        // The first chart patch has alpha=0.25; all other patches are opaque.
        for (x, y) in [(32, 32), (96, 32), (288, 224)] {
            let p = (y * 384 + x) * 3;
            for channel in 0..3 {
                assert!(
                    actual[p + channel].abs_diff(expected[p + channel]) <= 2,
                    "({x},{y}) channel {channel}: actual {:?}, expected {:?}",
                    &actual[p..p + 3],
                    &expected[p..p + 3]
                );
            }
        }
    }
    // Also exercise an embedded wide-gamut profile through the flatten/cast.
    let profiled = tmp.path().join("alpha-p3.png");
    run(&[
        "icc_transform".as_ref(),
        fixture("alpha.png").as_os_str(),
        profiled.as_os_str(),
        fixture("icc/p3.icc").as_os_str(),
        "--input-profile=srgb".as_ref(),
    ]);
    let w = load_path(&profiled);
    encode(
        &w,
        encoders()[2].0,
        EncodeOpts {
            keep_icc: true,
            strip_metadata: true,
        },
        &out,
    )
    .unwrap();
    let output = native::Image::load(&out, &Kind::Jpeg).unwrap();
    assert_eq!(
        output.blob("icc-profile-data").as_deref(),
        w.profile.as_deref()
    );
}
#[test]
fn resampling_ssim_against_vips_thumbnail() {
    check_ssim(&fixture("landscape.jpg"), 384);
}
#[test]
fn photographic_ssim_against_vips_thumbnail() {
    check_ssim(&fixture("photo.jpg"), 800);
}
#[test]
fn large_photographic_ssim_when_tower_fixture_is_present() {
    let input = fixture("../large/24mp.jpg");
    if !input.is_file() {
        eprintln!("SKIP photographic SSIM: sync fixtures/large/24mp.jpg from tower");
        return;
    }
    check_ssim(&input, 1280);
}
fn check_ssim(input: &Path, edge: u32) {
    let tmp = tempfile::tempdir().unwrap();
    let reference = tmp.path().join("reference.png");
    let output = tmp.path().join("out.jxl");
    run(&[
        "thumbnail".as_ref(),
        input.as_os_str(),
        reference.as_os_str(),
        edge.to_string().as_ref(),
        "--linear".as_ref(),
    ]);
    let resized = resize(
        &load_path(input),
        ResizeSpec {
            long_edge: edge,
            square: false,
        },
    )
    .unwrap();
    encode(
        &resized,
        encoders()[1].0,
        EncodeOpts {
            keep_icc: false,
            strip_metadata: true,
        },
        &output,
    )
    .unwrap();
    let ref_w = load_path(&reference);
    let out_w = load_path(&output);
    let (width, height, a) = crate::rgba(&ref_w, edge).unwrap();
    let (_, _, b) = crate::rgba(&out_w, edge).unwrap();
    // Average local 8x8 SSIM across the full image; no thumbnail hides fine detail.
    let mut scores = Vec::new();
    for y in (0..height as usize - 7).step_by(8) {
        for x in (0..width as usize - 7).step_by(8) {
            let mut pa = Vec::with_capacity(256);
            let mut pb = Vec::with_capacity(256);
            for dy in 0..8 {
                let start = ((y + dy) * width as usize + x) * 4;
                pa.extend_from_slice(&a[start..start + 32]);
                pb.extend_from_slice(&b[start..start + 32]);
            }
            scores.push(ssim(&pa, &pb));
        }
    }
    let score = scores.iter().sum::<f64>() / scores.len() as f64;
    eprintln!("SSIM {score:.6}");
    assert!(score >= 0.98);
}
fn load_path(p: &Path) -> Working {
    decode(
        p,
        &identify(p, &Limits::default()).unwrap(),
        &Limits::default(),
    )
    .unwrap()
}
fn ssim(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let a: Vec<f64> = a
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| 0.2126 * f64::from(p[0]) + 0.7152 * f64::from(p[1]) + 0.0722 * f64::from(p[2]))
        .collect();
    let b: Vec<f64> = b
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| 0.2126 * f64::from(p[0]) + 0.7152 * f64::from(p[1]) + 0.0722 * f64::from(p[2]))
        .collect();
    let n = a.len() as f64;
    let ma = a.iter().sum::<f64>() / n;
    let mb = b.iter().sum::<f64>() / n;
    let va = a.iter().map(|x| (x - ma).powi(2)).sum::<f64>() / (n - 1.0);
    let vb = b.iter().map(|x| (x - mb).powi(2)).sum::<f64>() / (n - 1.0);
    let cov = a
        .iter()
        .zip(b)
        .map(|(x, y)| (x - ma) * (y - mb))
        .sum::<f64>()
        / (n - 1.0);
    ((2.0 * ma * mb + 6.5025) * (2.0 * cov + 58.5225))
        / ((ma * ma + mb * mb + 6.5025) * (va + vb + 58.5225))
}
// CIEDE2000, Sharma/Wu/Dalal. Includes the neutral and hue-wrap branches.
fn delta_e(a: [f64; 3], b: [f64; 3]) -> f64 {
    use std::f64::consts::PI;
    let c1 = a[1].hypot(a[2]);
    let c2 = b[1].hypot(b[2]);
    let cm = (c1 + c2) / 2.0;
    let g = 0.5 * (1.0 - (cm.powi(7) / (cm.powi(7) + 25f64.powi(7))).sqrt());
    let ap1 = (1.0 + g) * a[1];
    let ap2 = (1.0 + g) * b[1];
    let cp1 = ap1.hypot(a[2]);
    let cp2 = ap2.hypot(b[2]);
    let hue = |x: f64, y: f64| {
        let h = y.atan2(x);
        if h < 0.0 { h + 2.0 * PI } else { h }
    };
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
        * (cm.powi(7) / (cm.powi(7) + 25f64.powi(7))).sqrt()
        * (PI / 3.0 * (-((hm.to_degrees() - 275.0) / 25.0).powi(2)).exp()).sin();
    ((dl / sl).powi(2) + (dc / sc).powi(2) + (dh / sh).powi(2) + rt * (dc / sc) * (dh / sh)).sqrt()
}
#[test]
fn delta_e_reference_pair() {
    assert!((delta_e([50.0, 2.6772, -79.7751], [50.0, 0.0, -82.7485]) - 2.0425).abs() < 0.0001);
}
#[cfg(feature = "raw")]
#[test]
fn optional_raw_fixtures() {
    let dir = fixture("../large/raw");
    if !dir.is_dir() {
        eprintln!("SKIP RAW: sync fixtures/large/raw from tower");
        return;
    }
    for entry in std::fs::read_dir(dir).unwrap() {
        let p = entry.unwrap().path();
        if !p.is_file() {
            continue;
        }
        let id = identify(&p, &Limits::default()).unwrap();
        assert!(matches!(id.kind, Kind::Raw(_)));
        decode(&p, &id, &Limits::default()).unwrap();
    }
}

#[test]
fn every_benign_fixture_decodes_including_cmyk_and_gray() {
    for entry in std::fs::read_dir(fixture("")).unwrap() {
        let path = entry.unwrap().path();
        if !path
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|s| matches!(s, "jpg" | "png" | "tif" | "avif" | "jxl" | "heic"))
        {
            continue;
        }
        #[cfg(not(feature = "heif"))]
        if path.extension().is_some_and(|s| s == "heic") {
            continue;
        }
        let id = identify(&path, &Limits::default()).unwrap();
        let working = decode(&path, &id, &Limits::default()).unwrap();
        assert_eq!(working.image.sample_bits(), 16);
        assert!(working.dimensions().unwrap().0 > 0);
    }
}
#[test]
fn gain_map_variants_are_detected_without_decoding_pixels() {
    let jpeg = std::fs::read(fixture("progressive.jpg")).unwrap();
    for signature in [
        b"urn:iso:std:iso:ts:21496:-1".as_slice(),
        b"http://ns.adobe.com/hdr-gain-map/1.0/",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("gain.jpg");
        let mut bytes = jpeg[..2].to_vec();
        bytes.extend([0xff, 0xe1]);
        bytes.extend(u16::try_from(signature.len() + 2).unwrap().to_be_bytes());
        bytes.extend(signature);
        bytes.extend(&jpeg[2..]);
        std::fs::write(&path, bytes).unwrap();
        assert!(identify(&path, &Limits::default()).unwrap().has_gain_map);
    }
}

#[test]
fn truncated_jxl_header_does_not_enter_the_native_eof_loop() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), [255, 10, 0, 0, 0, 0, 0, 46, 0, 0, 10]).unwrap();
    let start = Instant::now();
    assert_eq!(
        identify(tmp.path(), &Limits::default()),
        Err(CodecError::Decode)
    );
    assert!(start.elapsed().as_secs_f64() < 2.0);
}

#[test]
fn short_jxl_never_probes_unrelated_fallback_decoders() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), [255, 10, 65, 10, 1, 0, 0, 57, 65, 10, 2]).unwrap();
    let start = Instant::now();
    let _ = identify(tmp.path(), &Limits::default());
    assert!(start.elapsed().as_secs_f64() < 2.0);
}

#[test]
fn cmyk_is_imported_before_export_to_rgb_only_codecs() {
    let source = fixture("cmyk.jpg");
    let expected = lab(&source);
    let working = load("cmyk.jpg");
    let tmp = tempfile::tempdir().unwrap();
    for (enc, suffix) in encoders() {
        let out = tmp.path().join(format!("cmyk.{suffix}"));
        encode(
            &working,
            enc,
            EncodeOpts {
                keep_icc: true,
                strip_metadata: true,
            },
            &out,
        )
        .unwrap();
        assert!(identify(&out, &Limits::default()).unwrap().has_icc);
        let actual = lab(&out);
        let mut errors = Vec::new();
        for i in 0..24 {
            let idx = (i / 6 * 64 + 32) * 384 + i % 6 * 64 + 32;
            errors.push(delta_e(expected[idx], actual[idx]));
        }
        assert!(
            errors.iter().sum::<f64>() / 24.0 < 1.0,
            "CMYK {suffix} {errors:?}"
        );
    }
}

#[test]
fn tiff_bomb_is_rejected_by_our_header_parser_before_native_loading() {
    let path = fixture("malicious/huge-dims-tiff.tif");
    assert_eq!(
        headers::inspect(&path, &Limits::default()),
        Err(CodecError::Limits)
    );
    assert_eq!(identify(&path, &Limits::default()), Err(CodecError::Limits));
}

// A metadata-only DNG test container. Pixel bytes are existing local JPEG
// fixtures; the sensor strip is deliberately invalid, so a preview test also
// proves identification never attempts a sensor decode.
#[cfg(feature = "raw")]
fn preview_dng(path: &Path, width: u32, height: u32, previews: &[Vec<u8>]) {
    fn entry(out: &mut Vec<u8>, tag: u16, kind: u16, count: u32, value: u32) {
        out.extend(tag.to_le_bytes());
        out.extend(kind.to_le_bytes());
        out.extend(count.to_le_bytes());
        out.extend(value.to_le_bytes());
    }
    const ROOT_ENTRIES: u16 = 10;
    const PREVIEW_ENTRIES: u16 = 8;
    let root_end = 8 + 2 + usize::from(ROOT_ENTRIES) * 12 + 4;
    let sub_size = 2 + usize::from(PREVIEW_ENTRIES) * 12 + 4;
    let sub_offsets: Vec<_> = (0..previews.len())
        .map(|i| (root_end + i * sub_size) as u32)
        .collect();
    let offset_list = root_end + sub_size * previews.len();
    let mut data_offset = offset_list + previews.len() * 4;
    let mut bytes = b"II*\0\x08\0\0\0".to_vec();
    bytes.extend(ROOT_ENTRIES.to_le_bytes());
    entry(&mut bytes, 256, 4, 1, width);
    entry(&mut bytes, 257, 4, 1, height);
    entry(&mut bytes, 258, 3, 1, 16);
    entry(&mut bytes, 259, 3, 1, 1);
    entry(&mut bytes, 262, 3, 1, 32803);
    entry(&mut bytes, 271, 2, 4, u32::from_le_bytes(*b"Test"));
    entry(&mut bytes, 273, 4, 1, u32::MAX);
    entry(&mut bytes, 274, 3, 1, 1);
    entry(
        &mut bytes,
        330,
        4,
        previews.len() as u32,
        if previews.len() == 1 {
            sub_offsets[0]
        } else {
            offset_list as u32
        },
    );
    entry(&mut bytes, 50706, 1, 4, u32::from_le_bytes([1, 4, 0, 0]));
    bytes.extend(0u32.to_le_bytes());
    for preview in previews {
        bytes.extend(PREVIEW_ENTRIES.to_le_bytes());
        // Deliberately small IFD dimensions: JPEG SOF must be checked separately.
        entry(&mut bytes, 254, 4, 1, 1);
        entry(&mut bytes, 256, 4, 1, 1);
        entry(&mut bytes, 257, 4, 1, 1);
        entry(&mut bytes, 259, 3, 1, 7);
        entry(&mut bytes, 262, 3, 1, 2);
        entry(&mut bytes, 273, 4, 1, data_offset as u32);
        entry(&mut bytes, 277, 3, 1, 3);
        entry(&mut bytes, 279, 4, 1, preview.len() as u32);
        bytes.extend(0u32.to_le_bytes());
        data_offset += preview.len();
    }
    for offset in &sub_offsets {
        bytes.extend(offset.to_le_bytes());
    }
    for preview in previews {
        bytes.extend(preview);
    }
    std::fs::write(path, bytes).unwrap();
}
#[cfg(feature = "raw")]
#[test]
fn raw_and_preview_bombs_never_reach_pixel_decoding() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("bomb.dng");
    let valid = std::fs::read(fixture("tiny-1x1.jpg")).unwrap();
    let mut huge = std::fs::read(fixture("chart-adobergb.jpg")).unwrap();
    let sof = huge
        .windows(2)
        .position(|w| matches!(w, [0xff, 0xc0..=0xc2]))
        .unwrap();
    huge[sof + 5..sof + 9].copy_from_slice(&[0xea, 0x60, 0xea, 0x60]);
    // Invalid entropy data ensures a decoder-first implementation cannot return
    // our controlled Limits variant by accident.
    huge.truncate(sof + 19);
    for (w, h, previews) in [(60_000, 60_000, vec![]), (1024, 768, vec![valid, huge])] {
        preview_dng(&path, w, h, &previews);
        let before = raw::PIXEL_DECODES.get();
        assert_eq!(identify(&path, &Limits::default()), Err(CodecError::Limits));
        assert!(matches!(
            raw::decode(&path, &Limits::default()),
            Err(CodecError::Limits)
        ));
        assert_eq!(raw::PIXEL_DECODES.get(), before);
    }
}
#[cfg(feature = "raw")]
#[test]
fn raw_selects_largest_preview_and_preserves_adobe_rgb_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let input = tmp.path().join("previews.dng");
    let small = std::fs::read(fixture("tiny-1x1.jpg")).unwrap();
    let large = std::fs::read(fixture("chart-adobergb.jpg")).unwrap();
    // Both ordering permutations must select the actual JPEG dimensions, not the
    // first reduced SubIFD or the deliberately incorrect 1x1 TIFF dimensions.
    for previews in [vec![small.clone(), large.clone()], vec![large, small]] {
        preview_dng(&input, 1024, 768, &previews);
        let before = raw::PIXEL_DECODES.get();
        let id = identify(&input, &Limits::default()).unwrap();
        assert_eq!(raw::PIXEL_DECODES.get(), before);
        assert_eq!((id.width, id.height), (1024, 768));
        assert!(id.has_icc);
        assert!(id.icc_desc.as_deref().unwrap().contains("Adobe"));
        let working = decode(&input, &id, &Limits::default()).unwrap();
        let reference = load("chart-adobergb.jpg");
        assert_eq!(working.dimensions().unwrap(), (384, 256));
        assert_eq!(working.profile, reference.profile);
        assert_eq!(
            working.image.pixels().unwrap(),
            reference.image.pixels().unwrap()
        );
        let output = tmp.path().join("roundtrip.jxl");
        encode(
            &working,
            Encode::Jxl {
                distance: 0.0,
                effort: 3,
                hdr: false,
            },
            EncodeOpts {
                keep_icc: true,
                strip_metadata: true,
            },
            &output,
        )
        .unwrap();
        let id = identify(&output, &Limits::default()).unwrap();
        assert!(id.has_icc);
        assert!(id.icc_desc.as_deref().unwrap().contains("Adobe"));
        let out = lab(&output);
        let source = lab(&fixture("chart-adobergb.jpg"));
        let differences: Vec<_> = out
            .into_iter()
            .zip(source)
            .map(|(a, b)| delta_e(a, b))
            .collect();
        assert!(differences.iter().sum::<f64>() / (differences.len() as f64) < 1.0);
        assert!(differences.into_iter().fold(0.0, f64::max) < 2.5);
    }
}

#[test]
fn tiff_preflight_checks_both_byte_orders_and_bigtiff_without_pixels() {
    let tmp = tempfile::tempdir().unwrap();
    for big in [false, true] {
        for little in [false, true] {
            let mut bytes = if little {
                b"II".to_vec()
            } else {
                b"MM".to_vec()
            };
            let put = |bytes: &mut Vec<u8>, n: u64, size: usize| {
                let data = if little {
                    n.to_le_bytes()
                } else {
                    n.to_be_bytes()
                };
                bytes.extend(if little {
                    &data[..size]
                } else {
                    &data[8 - size..]
                });
            };
            put(&mut bytes, if big { 43 } else { 42 }, 2);
            if big {
                put(&mut bytes, 8, 2);
                put(&mut bytes, 0, 2);
            }
            put(
                &mut bytes,
                if big { 16 } else { 8 },
                if big { 8 } else { 4 },
            );
            put(&mut bytes, 2, if big { 8 } else { 2 });
            for tag in [256, 257] {
                put(&mut bytes, tag, 2);
                put(&mut bytes, if big { 16 } else { 4 }, 2);
                put(&mut bytes, 1, if big { 8 } else { 4 });
                put(&mut bytes, 60_000, if big { 8 } else { 4 });
            }
            put(&mut bytes, 0, if big { 8 } else { 4 });
            let path = tmp.path().join("header.tif");
            std::fs::write(&path, bytes).unwrap();
            assert_eq!(
                headers::inspect(&path, &Limits::default()),
                Err(CodecError::Limits)
            );
            // Pixel and axis budgets are checked separately before a native call.
            assert_eq!(
                headers::inspect(
                    &path,
                    &Limits {
                        max_pixels: u64::MAX,
                        max_dim: 4096,
                        ..Limits::default()
                    }
                ),
                Err(CodecError::Limits)
            );
        }
    }
}

#[cfg(feature = "raw")]
#[test]
fn raw_identification_without_a_preview_never_decodes_the_sensor() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("invalid-sensor.dng");
    preview_dng(&path, 1024, 768, &[]);
    let before = raw::PIXEL_DECODES.get();
    let id = identify(&path, &Limits::default()).unwrap();
    assert_eq!((id.width, id.height), (1024, 768));
    assert_eq!(id.kind, Kind::Raw(RawMake("Test".to_owned())));
    assert_eq!(raw::PIXEL_DECODES.get(), before);
}

#[cfg(feature = "raw")]
#[test]
fn zero_sized_cr3_file_type_box_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("bad.cr3");
    std::fs::write(&path, b"\0\0\0\0ftypmxl crx \xff").unwrap();
    assert_eq!(
        headers::cr3(&path, &Limits::default()),
        Err(CodecError::Decode)
    );
    assert_eq!(identify(&path, &Limits::default()), Err(CodecError::Decode));
}

fn jpeg_app(jpeg: &[u8], marker: u8, payload: &[u8]) -> Vec<u8> {
    let mut bytes = jpeg[..2].to_vec();
    bytes.extend([0xff, marker]);
    bytes.extend(u16::try_from(payload.len() + 2).unwrap().to_be_bytes());
    bytes.extend(payload);
    bytes.extend(&jpeg[2..]);
    bytes
}
// Complete two-image MPF, with offsets relative to the APP2 TIFF header.
fn apple_mpf(little: bool, xmp: bool) -> (Vec<u8>, usize) {
    fn num(out: &mut Vec<u8>, value: u32, size: usize, little: bool) {
        let bytes = if little {
            value.to_le_bytes()
        } else {
            value.to_be_bytes()
        };
        out.extend(if little {
            &bytes[..size]
        } else {
            &bytes[4 - size..]
        });
    }
    let jpeg = std::fs::read(fixture("progressive.jpg")).unwrap();
    let mut payload = b"http://ns.adobe.com/xap/1.0/\0".to_vec();
    payload.extend(b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><rdf:Description xmlns:apple='http://ns.apple.com/pixeldatainfo/1.0/' apple:AuxiliaryImageType='urn:com:apple:photo:2020:aux:hdrgainmap'/></rdf:RDF></x:xmpmeta>");
    let aux = jpeg_app(&jpeg, if xmp { 0xe1 } else { 0xfe }, &payload);
    let mut mpf = b"MPF\0".to_vec();
    mpf.extend(if little { b"II" } else { b"MM" });
    num(&mut mpf, 42, 2, little);
    num(&mut mpf, 8, 4, little);
    num(&mut mpf, 3, 2, little);
    for (tag, kind, count, value) in [
        (
            0xb000,
            7,
            4,
            if little {
                u32::from_le_bytes(*b"0100")
            } else {
                u32::from_be_bytes(*b"0100")
            },
        ),
        (0xb001, 4, 1, 2),
        (0xb002, 7, 32, 50),
    ] {
        num(&mut mpf, tag, 2, little);
        num(&mut mpf, kind, 2, little);
        num(&mut mpf, count, 4, little);
        num(&mut mpf, value, 4, little);
    }
    num(&mut mpf, 0, 4, little);
    let primary_len = jpeg.len() + 4 + 86;
    let aux_offset = mpf.len() + 16 + 8; // location of auxiliary data offset in APP2 payload
    for (attr, size, offset) in [
        (0x2003_0000, primary_len, 0),
        (0, aux.len(), primary_len - 10), // TIFF byte-order field is at file offset 10.
    ] {
        num(&mut mpf, attr, 4, little);
        num(&mut mpf, size as u32, 4, little);
        num(&mut mpf, offset as u32, 4, little);
        num(&mut mpf, 0, 4, little);
    }
    assert_eq!(mpf.len(), 86);
    let mut bytes = jpeg_app(&jpeg, 0xe2, &mpf);
    assert_eq!(bytes.len(), primary_len);
    bytes.extend(aux);
    (bytes, 6 + aux_offset)
}
#[test]
fn apple_gain_map_is_found_in_mpf_auxiliary_xmp() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    for little in [false, true] {
        for xmp in [false, true] {
            let (bytes, _) = apple_mpf(little, xmp);
            std::fs::write(tmp.path(), bytes).unwrap();
            assert_eq!(
                identify(tmp.path(), &Limits::default())
                    .unwrap()
                    .has_gain_map,
                xmp
            );
        }
    }
    let jpeg = std::fs::read(fixture("progressive.jpg")).unwrap();
    std::fs::write(
        tmp.path(),
        jpeg_app(&jpeg, 0xe1, b"urn:com:apple:photo:2020:aux:hdrgainmap"),
    )
    .unwrap();
    assert!(
        !identify(tmp.path(), &Limits::default())
            .unwrap()
            .has_gain_map
    );
}
#[test]
fn mpf_offsets_counts_and_auxiliary_extents_are_bounded() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let (valid, aux_offset) = apple_mpf(true, true);
    for (pos, value) in [
        (aux_offset, u32::MAX),          // Outside file.
        (aux_offset, 1),                 // Overlaps primary.
        (aux_offset - 4, u32::MAX),      // Auxiliary end outside file.
        (10 + 8 + 2 + 12 + 8, 65),       // Too many images.
        (10 + 8 + 2 + 24 + 8, u32::MAX), // MP entry table outside APP2.
        (10 + 8 + 2 + 24 + 4, 16),       // Incomplete entry table.
    ] {
        let mut bytes = valid.clone();
        bytes[pos..pos + 4].copy_from_slice(&value.to_le_bytes());
        std::fs::write(tmp.path(), bytes).unwrap();
        assert!(headers::gain_map(tmp.path()).is_err(), "field at {pos}");
    }
    let mut bytes = valid;
    let start =
        10 + u32::from_le_bytes(bytes[aux_offset..aux_offset + 4].try_into().unwrap()) as usize;
    bytes[start..start + 2].copy_from_slice(&[0, 0]);
    std::fs::write(tmp.path(), bytes).unwrap();
    assert_eq!(headers::gain_map(tmp.path()), Err(CodecError::Decode));
}

#[cfg(feature = "raw")]
fn sensor_tiff(path: &Path, tiled: bool, canon: bool, w: u16, h: u16, components: u8) {
    let mut jpeg = vec![0xff, 0xd8, 0xff, 0xc3];
    jpeg.extend((8 + u16::from(components) * 3).to_be_bytes());
    jpeg.push(14);
    jpeg.extend(h.to_be_bytes());
    jpeg.extend(w.to_be_bytes());
    jpeg.push(components);
    for n in 0..components {
        jpeg.extend([n + 1, 0x11, 0]);
    }
    let mut entries = vec![
        (256u16, 4u16, 1024u32),
        (257, 4, 768),
        (259, 3, 7),
        (262, 3, 32803),
        (if tiled { 324 } else { 273 }, 4, 0),
        (if tiled { 325 } else { 279 }, 4, jpeg.len() as u32),
        (if canon { 0xc5d8 } else { 50706 }, 4, 1),
    ];
    let offset = if canon { 16 } else { 8 };
    entries[4].2 = offset + 2 + entries.len() as u32 * 12 + 4;
    let mut bytes = b"II*\0".to_vec();
    bytes.extend(offset.to_le_bytes());
    if canon {
        bytes.extend(b"CR\x02\0");
        bytes.extend(offset.to_le_bytes());
    }
    bytes.extend((entries.len() as u16).to_le_bytes());
    for (tag, kind, value) in entries {
        bytes.extend(tag.to_le_bytes());
        bytes.extend(kind.to_le_bytes());
        bytes.extend(1u32.to_le_bytes());
        bytes.extend(value.to_le_bytes());
    }
    bytes.extend(0u32.to_le_bytes());
    bytes.extend(jpeg);
    std::fs::write(path, bytes).unwrap();
}
#[cfg(feature = "raw")]
#[test]
fn conflicting_tiff_and_sensor_sof_without_preview_is_rejected_before_decode() {
    let tmp = tempfile::tempdir().unwrap();
    for (name, tiled, canon) in [
        ("bad.cr2", false, true),
        ("bad.dng", false, false),
        ("bad.arw", true, false),
    ] {
        let path = tmp.path().join(name);
        for (w, h, components) in [(60_000, 60_000, 1), (40_000, 1_000, 2)] {
            sensor_tiff(&path, tiled, canon, w, h, components);
            let before = raw::PIXEL_DECODES.get();
            let start = Instant::now();
            assert_eq!(identify(&path, &Limits::default()), Err(CodecError::Limits));
            assert!(matches!(
                raw::decode(&path, &Limits::default()),
                Err(CodecError::Limits)
            ));
            assert_eq!(raw::PIXEL_DECODES.get(), before);
            assert!(start.elapsed().as_secs_f32() < 2.0);
        }
    }
}

// Canon's old sensor offset lives in MakerNote, independently of TIFF image
// dimensions and strips. Addresses are absolute even in big-endian files.
#[cfg(feature = "raw")]
fn canon_makernote_tiff(entries: &[(u16, u16, u64, u64)], little: bool, exif: bool) -> Vec<u8> {
    let maker = tiff_entries(entries, false, little);
    let maker_entry = (37500, 7, (maker.len() - 8) as u64, 512);
    let mut root = vec![(256, 4, 1, 1024), (257, 4, 1, 768), (271, 2, 6, 256)];
    root.push(if exif {
        (34665, 4, 1, 128)
    } else {
        maker_entry
    });
    let mut bytes = tiff_entries(&root, false, little);
    if exif {
        bytes.resize(128, 0);
        bytes.extend(&tiff_entries(&[maker_entry], false, little)[8..]);
    }
    bytes.resize(256, 0);
    bytes.extend(b"Canon\0");
    bytes.resize(512, 0);
    bytes.extend(&maker[8..]);
    bytes
}

#[cfg(feature = "raw")]
#[test]
fn canon_makernote_sensor_offsets_and_slices_are_checked_before_rawler() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("old.cr2");
    let sensor = tmp.path().join("sensor.cr2");
    for little in [false, true] {
        for exif in [false, true] {
            for slices in [false, true] {
                let entries = if slices {
                    vec![(0x00c5, 4, 1, 2048), (0x00c6, 4, 1, 19)]
                } else {
                    vec![(0x0081, 4, 1, 2048)]
                };
                for (w, h, components, error) in [
                    (1024, 768, 1, None),
                    (60_000, 60_000, 1, Some(CodecError::Limits)),
                    (40_000, 1000, 2, Some(CodecError::Limits)),
                ] {
                    sensor_tiff(&sensor, false, true, w, h, components);
                    let sensor_bytes = std::fs::read(&sensor).unwrap();
                    let sof =
                        &sensor_bytes[sensor_bytes.len() - (12 + usize::from(components) * 3)..];
                    let mut bytes = canon_makernote_tiff(&entries, little, exif);
                    bytes.resize(2048, 0);
                    bytes.extend(sof);
                    // Slice length must include this frame's entire SOF.
                    if slices {
                        let len = sof.len() as u32;
                        bytes[512 + 2 + 12 + 8..512 + 2 + 12 + 12].copy_from_slice(&if little {
                            len.to_le_bytes()
                        } else {
                            len.to_be_bytes()
                        });
                    }
                    std::fs::write(&path, bytes).unwrap();
                    let before = raw::PIXEL_DECODES.get();
                    let start = Instant::now();
                    if let Some(error) = error {
                        rejected_tiff(&path, &error);
                    } else {
                        let id = identify(&path, &Limits::default()).unwrap();
                        assert_eq!((id.width, id.height), (1024, 768));
                        assert_eq!(id.kind, Kind::Raw(RawMake("Canon".to_owned())));
                    }
                    assert_eq!(raw::PIXEL_DECODES.get(), before);
                    assert!(start.elapsed().as_secs_f32() < 2.0);
                }
            }
        }
    }
}

#[cfg(feature = "raw")]
#[test]
fn canon_makernote_entries_and_slice_extents_share_tiff_validation() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("bad.cr2");
    for (entries, error) in [
        (
            vec![(0x0081, 4, 1, u64::from(u32::MAX))],
            CodecError::Decode,
        ),
        (vec![(0x00c5, 4, 1, 2048)], CodecError::Decode),
        (vec![(0x00c6, 4, 1, 16)], CodecError::Decode),
        (
            vec![(0x00c5, 4, 1, 2048), (0x00c6, 4, 1, 100)],
            CodecError::Decode,
        ),
        (
            vec![(65000, 12, u64::from(u32::MAX), 8)],
            CodecError::Limits,
        ),
        (vec![(65000, 0, 1, 0)], CodecError::Decode),
        (
            vec![(65000, 7, 16, u64::from(u32::MAX))],
            CodecError::Decode,
        ),
        (
            vec![(0x0081, 4, 1, 0), (0x0081, 4, 1, 0)],
            CodecError::Decode,
        ),
        (
            (0..4093).map(|tag| (tag, 1, 1, 0)).collect(),
            CodecError::Limits,
        ),
    ] {
        std::fs::write(&path, canon_makernote_tiff(&entries, true, true)).unwrap();
        rejected_tiff(&path, &error);
    }
    // Check every slice, including an oversized second frame after a valid one.
    for little in [false, true] {
        for bomb in [false, true] {
            let mut bytes =
                canon_makernote_tiff(&[(0x00c5, 4, 2, 1024), (0x00c6, 4, 2, 1032)], little, true);
            bytes.resize(1024, 0);
            for value in [2048u32, 2063, 15, 15] {
                bytes.extend(if little {
                    value.to_le_bytes()
                } else {
                    value.to_be_bytes()
                });
            }
            bytes.resize(2048, 0);
            let frame = [0xff, 0xd8, 0xff, 0xc3, 0, 11, 14, 3, 0, 4, 0, 1, 1, 0x11, 0];
            bytes.extend(frame);
            let mut second = frame;
            if bomb {
                second[7..9].copy_from_slice(&60_000u16.to_be_bytes());
                second[9..11].copy_from_slice(&60_000u16.to_be_bytes());
            }
            bytes.extend(second);
            std::fs::write(&path, bytes).unwrap();
            if bomb {
                rejected_tiff(&path, &CodecError::Limits);
            } else {
                assert!(identify(&path, &Limits::default()).is_ok());
            }
        }
    }
    // The MakerNote payload and its entries must not reset the 256 MiB budget.
    let bytes = canon_makernote_tiff(&[(65000, 7, 256 << 20, 4096)], true, true);
    std::fs::write(&path, bytes).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len((256 << 20) + 4096)
        .unwrap();
    rejected_tiff(&path, &CodecError::Limits);
}

fn bmff_box(kind: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut bytes = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
    bytes.extend(kind);
    bytes.extend(payload);
    bytes
}
#[cfg(feature = "raw")]
fn cr3_container(moov: &[u8]) -> Vec<u8> {
    let mut bytes = bmff_box(b"ftyp", b"crx \0\0\0\0crx ");
    bytes.extend(bmff_box(b"moov", moov));
    bytes.extend(bmff_box(b"mdat", &[]));
    bytes
}
#[cfg(feature = "raw")]
fn cr3_preview_record(thumbnail: bool, jpeg: &[u8], width: u16, height: u16) -> Vec<u8> {
    let mut payload = vec![0; 4];
    if !thumbnail {
        payload.extend(1u16.to_be_bytes());
    }
    payload.extend(width.to_be_bytes());
    payload.extend(height.to_be_bytes());
    if !thumbnail {
        payload.extend(1u16.to_be_bytes());
    }
    payload.extend((jpeg.len() as u32).to_be_bytes());
    if thumbnail {
        payload.extend([0; 4]);
    }
    payload.extend(jpeg);
    bmff_box(if thumbnail { b"THMB" } else { b"PRVW" }, &payload)
}
#[cfg(feature = "raw")]
fn cr3_preview_uuid(record: &[u8]) -> Vec<u8> {
    let mut payload = vec![
        0xea, 0xf4, 0x2b, 0x5e, 0x1c, 0x98, 0x4b, 0x88, 0xb9, 0xfb, 0xb7, 0xdc, 0x40, 0x6e, 0x4d,
        0x16,
    ];
    payload.extend([0, 0, 0, 0, 0, 0, 0, 1]);
    payload.extend(record);
    bmff_box(b"uuid", &payload)
}
#[cfg(feature = "raw")]
#[test]
fn cr3_previews_use_only_libvips_including_orientation_icc_and_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    // An extensionless original and a misleading RAW extension still use magic.
    for name in ["original", "preview.cr3", "preview.cr2"] {
        let input = tmp.path().join(name);
        for source in [
            "photo.jpg",
            "chart-adobergb.jpg",
            "orientation-6.jpg",
            "exif-heavy.jpg",
        ] {
            let source = fixture(source);
            let id = identify(&source, &Limits::default()).unwrap();
            let jpeg = std::fs::read(&source).unwrap();
            let record = cr3_preview_record(false, &jpeg, id.width as u16, id.height as u16);
            let small = std::fs::read(fixture("tiny-1x1.jpg")).unwrap();
            let thumbnail = cr3_preview_record(true, &small, 1, 1);
            // Neither this synthetic sensor payload nor moov has enough data
            // for any rawler CR3 decoder. Successful calls prove it is bypassed.
            let mut bytes = cr3_container(&thumbnail);
            bytes.extend(cr3_preview_uuid(&record));
            std::fs::write(&input, &bytes).unwrap();
            let before = raw::PIXEL_DECODES.get();
            let raw_id = identify(&input, &Limits::default()).unwrap();
            assert_eq!(raw_id.kind, Kind::Raw(RawMake("Canon".to_owned())));
            assert_eq!((raw_id.width, raw_id.height), (id.width, id.height));
            assert_eq!(raw_id.orientation, id.orientation);
            assert_eq!(raw_id.icc_desc, id.icc_desc);
            assert_eq!(raw_id.has_icc, id.has_icc);
            let md = metadata(&input).unwrap();
            let reference_md = metadata(&source).unwrap();
            assert_eq!(md.make.as_deref(), Some("Canon"));
            assert_eq!(md.orientation, reference_md.orientation);
            assert_eq!(md.gps, reference_md.gps);
            assert_eq!(md.lens, reference_md.lens);
            assert_eq!(md.icc_desc, reference_md.icc_desc);
            assert_eq!(raw::PIXEL_DECODES.get(), before);
            let working = decode(&input, &raw_id, &Limits::default()).unwrap();
            let reference = load_path(&source);
            assert_eq!(
                working.dimensions().unwrap(),
                reference.dimensions().unwrap()
            );
            assert_eq!(working.profile, reference.profile);
            assert_eq!(
                working.image.pixels().unwrap(),
                reference.image.pixels().unwrap()
            );
            // THMB under Canon's moov descriptor UUID is also a JPEG candidate.
            let mut descriptor = vec![
                0x85, 0xc0, 0xb6, 0x87, 0x82, 0x0f, 0x11, 0xe0, 0x81, 0x11, 0xf4, 0xce, 0x46, 0x2b,
                0x6a, 0x48,
            ];
            descriptor.extend(cr3_preview_record(
                true,
                &jpeg,
                id.width as u16,
                id.height as u16,
            ));
            let mut bytes = cr3_container(&bmff_box(b"uuid", &descriptor));
            bytes.extend(cr3_preview_uuid(&cr3_preview_record(false, &small, 1, 1)));
            std::fs::write(&input, bytes).unwrap();
            assert_eq!(identify(&input, &Limits::default()).unwrap(), raw_id);
        }
    }
}
#[cfg(feature = "raw")]
#[test]
fn cr3_rawler_attack_regressions_return_unsupported_without_a_jpeg() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let mut zero_sample_size = vec![0; 8];
    zero_sample_size.extend(u32::MAX.to_be_bytes());
    let mut fixed_sample_size = vec![0; 4];
    fixed_sample_size.extend(1u32.to_be_bytes());
    fixed_sample_size.extend(u32::MAX.to_be_bytes());
    let mut unknown_uuid = vec![0x42; 16];
    unknown_uuid.extend(b"\0\0\0\0free");
    let mut mvhd_v2 = vec![0; 100];
    mvhd_v2[0] = 2;
    for payload in [
        bmff_box(
            b"trak",
            &bmff_box(
                b"mdia",
                &bmff_box(
                    b"minf",
                    &bmff_box(b"stbl", &bmff_box(b"stsz", &zero_sample_size)),
                ),
            ),
        ),
        bmff_box(b"stsz", &fixed_sample_size),
        bmff_box(b"uuid", &unknown_uuid),
        bmff_box(b"mvhd", &mvhd_v2),
        Vec::new(),
    ] {
        std::fs::write(tmp.path(), cr3_container(&payload)).unwrap();
        let before = raw::PIXEL_DECODES.get();
        let start = Instant::now();
        assert_eq!(
            identify(tmp.path(), &Limits::default()),
            Err(CodecError::Unsupported)
        );
        assert!(matches!(
            raw::decode(tmp.path(), &Limits::default()),
            Err(CodecError::Unsupported)
        ));
        assert_eq!(metadata(tmp.path()).err(), Some(CodecError::Unsupported));
        assert_eq!(raw::PIXEL_DECODES.get(), before);
        assert!(start.elapsed().as_secs_f32() < 2.0);
    }
}
#[cfg(feature = "raw")]
#[test]
fn cr3_jpeg_records_enforce_extents_dimensions_and_candidate_limits_before_decode() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let jpeg = std::fs::read(fixture("tiny-1x1.jpg")).unwrap();
    let valid = cr3_preview_record(false, &jpeg, 1, 1);
    let mut bad_length = valid.clone();
    bad_length[20..24].copy_from_slice(&u32::MAX.to_be_bytes());
    let mut bad_sof = jpeg.clone();
    let sof = bad_sof.windows(2).position(|b| b == b"\xff\xc0").unwrap();
    bad_sof[sof + 5..sof + 9].copy_from_slice(&[0xea, 0x60, 0xea, 0x60]);
    let mut no_eoi = jpeg.clone();
    no_eoi.pop();
    let cases = [
        (
            cr3_preview_record(false, &jpeg, 60_000, 60_000),
            CodecError::Limits,
        ),
        (
            cr3_preview_record(false, &bad_sof, 1, 1),
            CodecError::Limits,
        ),
        (cr3_preview_record(false, &jpeg, 2, 1), CodecError::Mismatch),
        (cr3_preview_record(false, &no_eoi, 1, 1), CodecError::Decode),
        (bad_length, CodecError::Decode),
        (valid.repeat(65), CodecError::Limits),
        (bmff_box(b"PRVW", &[0; 15]), CodecError::Decode),
    ];
    for (record, error) in cases {
        let mut bytes = cr3_container(&[]);
        bytes.extend(cr3_preview_uuid(&record));
        std::fs::write(tmp.path(), bytes).unwrap();
        let before = raw::PIXEL_DECODES.get();
        let start = Instant::now();
        assert_eq!(identify(tmp.path(), &Limits::default()).err(), Some(error));
        assert_eq!(raw::PIXEL_DECODES.get(), before);
        assert!(start.elapsed().as_secs_f32() < 2.0);
    }
}
#[cfg(feature = "raw")]
#[test]
fn cr3_sparse_preview_is_capped_before_buffer_allocation() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let mut record = cr3_preview_record(false, &[], 1, 1);
    let len = (64u32 << 20) + 1;
    record[..4].copy_from_slice(&(24 + len).to_be_bytes());
    record[20..24].copy_from_slice(&len.to_be_bytes());
    let mut uuid = cr3_preview_uuid(&record);
    let uuid_len = uuid.len() as u32 + len;
    uuid[..4].copy_from_slice(&uuid_len.to_be_bytes());
    let mut bytes = cr3_container(&[]);
    bytes.extend(uuid);
    std::fs::write(tmp.path(), &bytes).unwrap();
    tmp.as_file()
        .set_len(bytes.len() as u64 + u64::from(len))
        .unwrap();
    let start = Instant::now();
    let before = raw::PIXEL_DECODES.get();
    assert_eq!(
        identify(tmp.path(), &Limits::default()),
        Err(CodecError::Limits)
    );
    assert!(matches!(
        raw::decode(tmp.path(), &Limits::default()),
        Err(CodecError::Limits)
    ));
    assert_eq!(metadata(tmp.path()).err(), Some(CodecError::Limits));
    assert_eq!(raw::PIXEL_DECODES.get(), before);
    assert!(start.elapsed().as_secs_f32() < 2.0);
}
#[cfg(feature = "raw")]
#[test]
fn nested_cr3_extents_depth_count_and_progress_are_bounded() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let mut too_deep = bmff_box(b"free", &[]);
    for _ in 0..8 {
        too_deep = bmff_box(b"trak", &too_deep);
    }
    let mut wrapped = 1u32.to_be_bytes().to_vec();
    wrapped.extend(b"free");
    wrapped.extend(u64::MAX.to_be_bytes());
    let mut descriptor = vec![
        0x85, 0xc0, 0xb6, 0x87, 0x82, 0x0f, 0x11, 0xe0, 0x81, 0x11, 0xf4, 0xce, 0x46, 0x2b, 0x6a,
        0x48,
    ];
    descriptor.extend(b"\0\0\0\0free");
    let mut stsd = vec![0; 8];
    stsd.extend(b"\0\0\0\0free");
    let mut craw = vec![0; 82];
    craw.extend(b"\0\0\0\0free");
    let cases = [
        (b"\0\0\0\0free".to_vec(), CodecError::Decode),
        (bmff_box(b"trak", b"\0\0\0\0free"), CodecError::Decode),
        (b"\0\0\0\x10free".to_vec(), CodecError::Decode), // Child exceeds parent.
        (vec![0; 7], CodecError::Decode),                 // Truncated child header.
        (b"\0\0\0\x04free".to_vec(), CodecError::Decode), // Size smaller than header.
        (wrapped, CodecError::Decode),
        (bmff_box(b"uuid", &descriptor), CodecError::Decode),
        (bmff_box(b"stsd", &stsd), CodecError::Decode),
        (bmff_box(b"CRAW", &craw), CodecError::Decode),
        (too_deep, CodecError::Limits),
        (bmff_box(b"free", &[]).repeat(4094), CodecError::Limits),
    ];
    for (payload, error) in cases {
        std::fs::write(tmp.path(), cr3_container(&payload)).unwrap();
        let before = raw::PIXEL_DECODES.get();
        let start = Instant::now();
        assert_eq!(
            headers::cr3(tmp.path(), &Limits::default()).err().as_ref(),
            Some(&error)
        );
        assert_eq!(metadata(tmp.path()).err().as_ref(), Some(&error));
        assert_eq!(identify(tmp.path(), &Limits::default()), Err(error));
        assert_eq!(raw::PIXEL_DECODES.get(), before);
        assert!(start.elapsed().as_secs_f32() < 2.0);
    }
    // Opaque mdat bytes are never mistaken for nested boxes. Boundary limits pass.
    let mut at_depth = bmff_box(b"free", &[]);
    for _ in 0..7 {
        at_depth = bmff_box(b"trak", &at_depth);
    }
    for payload in [at_depth, bmff_box(b"free", &[]).repeat(4093)] {
        std::fs::write(tmp.path(), cr3_container(&payload)).unwrap();
        assert_eq!(headers::cr3(tmp.path(), &Limits::default()), Ok(()));
    }
}
#[cfg(feature = "raw")]
#[test]
fn cr3_cmp1_sensor_and_tile_dimensions_remain_bounded() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    for at in [8, 16] {
        let mut cmp1 = vec![0; 36];
        for pos in [8, 12, 16, 20] {
            cmp1[pos..pos + 4].copy_from_slice(&1024u32.to_be_bytes());
        }
        cmp1[at..at + 4].copy_from_slice(&60_000u32.to_be_bytes());
        cmp1[at + 4..at + 8].copy_from_slice(&60_000u32.to_be_bytes());
        std::fs::write(tmp.path(), cr3_container(&bmff_box(b"CMP1", &cmp1))).unwrap();
        assert_eq!(
            identify(tmp.path(), &Limits::default()),
            Err(CodecError::Limits)
        );
    }
}

#[cfg(feature = "raw")]
#[test]
fn raw_sensor_headers_are_validated_without_requiring_entropy_data() {
    let tmp = tempfile::tempdir().unwrap();
    for (name, tiled, canon) in [
        ("ok.cr2", false, true),
        ("ok.dng", false, false),
        ("ok.arw", true, false),
    ] {
        let path = tmp.path().join(name);
        sensor_tiff(&path, tiled, canon, 1024, 768, 1);
        let before = raw::PIXEL_DECODES.get();
        let id = identify(&path, &Limits::default()).unwrap();
        assert_eq!((id.width, id.height), (1024, 768));
        assert_eq!(raw::PIXEL_DECODES.get(), before);
    }
}
#[cfg(feature = "raw")]
#[test]
fn panasonic_sensor_dimensions_override_small_tiff_dimensions_before_decode() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let mut bytes = b"IIU\0\x08\0\0\0".to_vec();
    bytes.extend(4u16.to_le_bytes());
    for (tag, value) in [(2u16, 60_000u32), (3, 60_000), (256, 1024), (257, 768)] {
        bytes.extend(tag.to_le_bytes());
        bytes.extend(4u16.to_le_bytes());
        bytes.extend(1u32.to_le_bytes());
        bytes.extend(value.to_le_bytes());
    }
    bytes.extend(0u32.to_le_bytes());
    std::fs::write(tmp.path(), bytes).unwrap();
    let before = raw::PIXEL_DECODES.get();
    assert_eq!(
        identify(tmp.path(), &Limits::default()),
        Err(CodecError::Limits)
    );
    assert!(matches!(
        raw::decode(tmp.path(), &Limits::default()),
        Err(CodecError::Limits)
    ));
    assert_eq!(raw::PIXEL_DECODES.get(), before);
}
#[cfg(feature = "raw")]
#[test]
fn dng_jxl_sensor_codestream_is_checked_against_custom_limits_before_decode() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("bad.dng");
    sensor_tiff(&path, false, false, 1, 1, 1);
    let mut bytes = std::fs::read(&path).unwrap();
    let jpeg_offset = u32::from_le_bytes(
        bytes[8 + 2 + 4 * 12 + 8..8 + 2 + 4 * 12 + 12]
            .try_into()
            .unwrap(),
    ) as usize;
    let jxl = std::fs::read(fixture("chart.jxl")).unwrap();
    bytes[8 + 2 + 2 * 12 + 8..8 + 2 + 2 * 12 + 12].copy_from_slice(&52546u32.to_le_bytes());
    bytes[8 + 2 + 5 * 12 + 8..8 + 2 + 5 * 12 + 12]
        .copy_from_slice(&(jxl.len() as u32).to_le_bytes());
    bytes.truncate(jpeg_offset);
    bytes.extend(jxl);
    std::fs::write(&path, bytes).unwrap();
    assert!(identify(&path, &Limits::default()).is_ok());
    // TIFF dimensions fit a strict custom cap while the JPEG XL frame does not.
    let mut bytes = std::fs::read(&path).unwrap();
    for n in 0..2 {
        bytes[8 + 2 + n * 12 + 8..8 + 2 + n * 12 + 12].copy_from_slice(&32u32.to_le_bytes());
    }
    std::fs::write(&path, bytes).unwrap();
    let limits = Limits {
        max_dim: 32,
        ..Limits::default()
    };
    let before = raw::PIXEL_DECODES.get();
    assert_eq!(identify(&path, &limits), Err(CodecError::Limits));
    assert!(matches!(
        raw::decode(&path, &limits),
        Err(CodecError::Limits)
    ));
    assert_eq!(raw::PIXEL_DECODES.get(), before);
}

#[cfg(feature = "raw")]
#[test]
fn canon_cfa_sensor_sof_is_checked_even_when_tiff_claims_uncompressed() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("forged.cr2");
    sensor_tiff(&path, false, true, 60_000, 60_000, 1);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[16 + 2 + 2 * 12 + 8..16 + 2 + 2 * 12 + 12].copy_from_slice(&1u32.to_le_bytes());
    bytes[16 + 2 + 6 * 12..16 + 2 + 6 * 12 + 2].copy_from_slice(&33422u16.to_le_bytes());
    // Canon does not require StripByteCounts; the SOF scan must still be capped.
    bytes[16 + 2 + 5 * 12..16 + 2 + 5 * 12 + 2].copy_from_slice(&65000u16.to_le_bytes());
    std::fs::write(&path, bytes).unwrap();
    let before = raw::PIXEL_DECODES.get();
    assert_eq!(identify(&path, &Limits::default()), Err(CodecError::Limits));
    assert!(matches!(
        raw::decode(&path, &Limits::default()),
        Err(CodecError::Limits)
    ));
    assert_eq!(raw::PIXEL_DECODES.get(), before);
}

fn tiff_entries(entries: &[(u16, u16, u64, u64)], big: bool, little: bool) -> Vec<u8> {
    let mut bytes = if little {
        b"II".to_vec()
    } else {
        b"MM".to_vec()
    };
    let put = |bytes: &mut Vec<u8>, value: u64, size: usize| {
        let data = if little {
            value.to_le_bytes()
        } else {
            value.to_be_bytes()
        };
        bytes.extend(if little {
            &data[..size]
        } else {
            &data[8 - size..]
        });
    };
    put(&mut bytes, if big { 43 } else { 42 }, 2);
    if big {
        put(&mut bytes, 8, 2);
        put(&mut bytes, 0, 2);
    }
    put(
        &mut bytes,
        if big { 16 } else { 8 },
        if big { 8 } else { 4 },
    );
    put(&mut bytes, entries.len() as u64, if big { 8 } else { 2 });
    for &(tag, kind, count, value) in entries {
        put(&mut bytes, u64::from(tag), 2);
        put(&mut bytes, u64::from(kind), 2);
        put(&mut bytes, count, if big { 8 } else { 4 });
        put(&mut bytes, value, if big { 8 } else { 4 });
    }
    put(&mut bytes, 0, if big { 8 } else { 4 });
    bytes
}

fn rejected_tiff(path: &Path, expected: &CodecError) {
    let limits = Limits::default();
    let mut id = identify(&fixture("16bit.tif"), &limits).unwrap();
    id.kind = Kind::Raw(RawMake("Test".to_owned()));
    #[cfg(feature = "raw")]
    let before = raw::PIXEL_DECODES.get();
    assert_eq!(identify(path, &limits).as_ref().err(), Some(expected));
    assert_eq!(decode(path, &id, &limits).as_ref().err(), Some(expected));
    assert_eq!(metadata(path).as_ref().err(), Some(expected));
    #[cfg(feature = "raw")]
    {
        assert_eq!(raw::decode(path, &limits).as_ref().err(), Some(expected));
        assert_eq!(raw::metadata(path).as_ref().err(), Some(expected));
        assert_eq!(raw::PIXEL_DECODES.get(), before);
    }
}

#[test]
fn tiff_every_entry_payload_is_checked_before_decode_and_metadata() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    for big in [false, true] {
        for little in [false, true] {
            for (kind, size) in [
                (1, 1),
                (2, 1),
                (3, 2),
                (4, 4),
                (5, 8),
                (6, 1),
                (7, 1),
                (8, 2),
                (9, 4),
                (10, 8),
                (11, 4),
                (12, 8),
                (13, 4),
                (16, 8),
                (17, 8),
                (18, 8),
            ] {
                // Unknown tags still reach rawler's eager entry parser. In
                // particular DOUBLE with count u32::MAX must fail before it.
                for (count, offset, expected) in [
                    (
                        u64::from(u32::MAX),
                        8,
                        if size == 1 {
                            CodecError::Decode
                        } else {
                            CodecError::Limits
                        },
                    ),
                    (16, 4096, CodecError::Decode),
                    (
                        16,
                        if big {
                            u64::MAX - 1
                        } else {
                            u64::from(u32::MAX) - 1
                        },
                        CodecError::Decode,
                    ),
                ] {
                    let bytes = tiff_entries(
                        &[
                            (256, 4, 1, 1),
                            (257, 4, 1, 1),
                            (50706, 1, 4, 0x0000_0401),
                            (65000, kind, count, offset),
                        ],
                        big,
                        little,
                    );
                    std::fs::write(tmp.path(), bytes).unwrap();
                    rejected_tiff(tmp.path(), &expected);
                }
                // A payload ending exactly at EOF is valid for every type.
                let mut bytes = tiff_entries(&[(65000, kind, 16, 4096)], big, little);
                bytes.resize(4096 + 16 * size, 0);
                let len = bytes.len() as u64;
                assert!(
                    tiff::Tiff::from_file(
                        std::io::Cursor::new(bytes.clone()),
                        len,
                        &Limits::default()
                    )
                    .is_ok()
                );
                bytes.pop();
                std::fs::write(tmp.path(), bytes).unwrap();
                rejected_tiff(tmp.path(), &CodecError::Decode);
            }
        }
    }
}

#[test]
fn tiff_entry_counts_and_aggregate_payloads_are_bounded() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    for big in [false, true] {
        let entries: Vec<_> = (1000..5096).map(|tag| (tag, 1, 1, 0)).collect();
        let bytes = tiff_entries(&entries, big, true);
        assert!(
            tiff::Tiff::from_file(
                std::io::Cursor::new(&bytes),
                bytes.len() as u64,
                &Limits::default()
            )
            .is_ok()
        );
        let mut entries = entries;
        entries.push((5096, 1, 1, 0));
        std::fs::write(tmp.path(), tiff_entries(&entries, big, true)).unwrap();
        rejected_tiff(tmp.path(), &CodecError::Limits);

        // Sparse, overlapping payloads exercise the aggregate cap without
        // allocating or reading hundreds of MiB. Exactly 256 MiB is accepted.
        let entries = [(65000, 7, 128 << 20, 4096), (65001, 7, 128 << 20, 4096)];
        std::fs::write(tmp.path(), tiff_entries(&entries, big, true)).unwrap();
        tmp.as_file().set_len((128 << 20) + 4096).unwrap();
        assert!(
            tiff::Tiff::from_file(
                tmp.reopen().unwrap(),
                tmp.as_file().metadata().unwrap().len(),
                &Limits::default()
            )
            .is_ok()
        );
        let bytes = tiff_entries(
            &[
                (65000, 7, 128 << 20, 4096),
                (65001, 7, (128 << 20) + 1, 4096),
            ],
            big,
            true,
        );
        std::fs::write(tmp.path(), bytes).unwrap();
        tmp.as_file().set_len((128 << 20) + 4097).unwrap();
        rejected_tiff(tmp.path(), &CodecError::Limits);
    }
    // A directory link must not reset the aggregate budget. Its inline pointer
    // also counts, so these two directories exceed 256 MiB by exactly one byte.
    let mut bytes = tiff_entries(
        &[(330, 4, 1, 4096), (65000, 7, (128 << 20) - 4, 8192)],
        false,
        true,
    );
    bytes.resize(4096, 0);
    let child = tiff_entries(&[(65001, 7, (128 << 20) + 1, 8192)], false, true);
    bytes.extend(&child[8..]);
    std::fs::write(tmp.path(), bytes).unwrap();
    tmp.as_file().set_len((128 << 20) + 8193).unwrap();
    rejected_tiff(tmp.path(), &CodecError::Limits);
}

#[test]
fn tiff_invalid_entry_types_and_oversized_bigtiff_counts_are_rejected() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    for kind in [0, 14, 15, u16::MAX] {
        std::fs::write(
            tmp.path(),
            tiff_entries(&[(65000, kind, 1, 0)], false, true),
        )
        .unwrap();
        rejected_tiff(tmp.path(), &CodecError::Decode);
    }
    std::fs::write(
        tmp.path(),
        tiff_entries(&[(65000, 1, u64::MAX, 8)], true, true),
    )
    .unwrap();
    rejected_tiff(tmp.path(), &CodecError::Limits);
}

#[test]
fn tiff_linked_directories_validate_unknown_entry_payloads() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    for big in [false, true] {
        for little in [false, true] {
            for (tag, kind) in [
                (330, 4),
                (34665, 4),
                (34853, 4),
                (40965, 4),
                (65000, 13),
                (65000, 18),
            ] {
                // Big endian inline LONG pointers are left aligned in BigTIFF.
                let pointer = if !big && kind == 18 {
                    1024
                } else if big && !little && kind != 18 {
                    4096 << 32
                } else {
                    4096
                };
                let mut bytes = tiff_entries(&[(tag, kind, 1, pointer)], big, little);
                bytes.resize(4096, 0);
                if !big && kind == 18 {
                    bytes[1024..1032].copy_from_slice(&if little {
                        4096u64.to_le_bytes()
                    } else {
                        4096u64.to_be_bytes()
                    });
                }
                let child = tiff_entries(&[(65001, 12, u64::from(u32::MAX), 8)], big, little);
                bytes.extend(&child[if big { 16 } else { 8 }..]);
                std::fs::write(tmp.path(), bytes).unwrap();
                rejected_tiff(tmp.path(), &CodecError::Limits);
            }
        }
    }
}

#[test]
#[allow(clippy::cast_sign_loss, clippy::too_many_lines)] // Bounded generated samples; full round trip.
fn float_tiff_round_trip_preserves_colors_and_normalized_opacity() {
    let tmp = tempfile::tempdir().unwrap();
    for format in ["uchar", "ushort", "float"] {
        let pixels = tmp.path().join("pixels.raw");
        let mut bytes = Vec::new();
        for (blue, alpha) in [(0.75, 0.0f64), (0.75, 0.5), (0.75, 1.0), (4.0, 0.5)] {
            // Extended scRGB luminance is independent of normalized alpha.
            let blue = if format == "float" { blue } else { 0.75 };
            for sample in [0.25, 0.5, blue, alpha] {
                match format {
                    "uchar" => bytes.push((sample * 255.0).round() as u8),
                    "ushort" => bytes.extend(((sample * 65535.0).round() as u16).to_ne_bytes()),
                    _ => bytes.extend((sample as f32).to_ne_bytes()),
                }
            }
        }
        std::fs::write(&pixels, bytes).unwrap();
        let image = tmp.path().join("source.v");
        let interpretation = match format {
            "uchar" => "srgb",
            "ushort" => "rgb16",
            _ => "scrgb",
        };
        run(&[
            "rawload".as_ref(),
            pixels.as_os_str(),
            image.as_os_str(),
            "4".as_ref(),
            "1".as_ref(),
            "4".as_ref(),
            format!("--format={format}").as_ref(),
            format!("--interpretation={interpretation}").as_ref(),
        ]);
        let input = tmp.path().join("source.tif");
        run(&["tiffsave".as_ref(), image.as_os_str(), input.as_os_str()]);
        let id = identify(&input, &Limits::default()).unwrap();
        assert!(id.has_alpha);
        let working = decode(&input, &id, &Limits::default()).unwrap();
        let check = |working: &Working| {
            let bytes = working.image.extract(3, 1).unwrap().pixels().unwrap();
            let alpha: Vec<_> = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| u16::from_ne_bytes(*b))
                .collect();
            assert_eq!(alpha[0], 0, "{format}");
            let middle = if format == "uchar" { 32896u16 } else { 32768 };
            assert!(alpha[1].abs_diff(middle) <= 1, "{format}: {alpha:?}");
            assert_eq!(alpha[2], 65535, "{format}");
            assert!(alpha[3].abs_diff(middle) <= 1, "{format}: {alpha:?}");
        };
        check(&working);
        if format == "float" {
            // Independent linear-sRGB -> D65 XYZ matrix, without the device
            // import under test. Check unclipped color before any encoding.
            let reference_xyz = [
                100.0 * (0.412_456_4 * 0.25 + 0.357_576_1 * 0.5 + 0.180_437_5 * 0.75),
                100.0 * (0.212_672_9 * 0.25 + 0.715_152_2 * 0.5 + 0.072_175_0 * 0.75),
                100.0 * (0.019_333_9 * 0.25 + 0.119_192_0 * 0.5 + 0.950_304_1 * 0.75),
            ];
            let reference_lab = xyz_lab(reference_xyz);
            let xyz = working.image.extract(0, 3).unwrap().pixels().unwrap();
            let errors: Vec<_> = xyz.as_chunks::<6>().0[..3]
                .iter()
                .map(|pixel| {
                    let xyz = std::array::from_fn(|i| {
                        f64::from(u16::from_ne_bytes(
                            pixel[i * 2..i * 2 + 2].try_into().unwrap(),
                        )) / 256.0
                    });
                    delta_e(reference_lab, xyz_lab(xyz))
                })
                .collect();
            assert_fidelity(&errors, &format!("{format} working"));
        }
        let output = tmp.path().join("roundtrip.jxl");
        encode(
            &working,
            Encode::Jxl {
                distance: 0.0,
                effort: 3,
                hdr: false,
            },
            EncodeOpts {
                keep_icc: false,
                strip_metadata: true,
            },
            &output,
        )
        .unwrap();
        let id = identify(&output, &Limits::default()).unwrap();
        check(&decode(&output, &id, &Limits::default()).unwrap());
        if format == "float" {
            // Oracle: convert linear scRGB to 16-bit sRGB independently, then
            // use the same ICC Lab PCS as the other pipeline fidelity gates.
            let rgb = tmp.path().join("rgb.v");
            let reference = tmp.path().join("reference.v");
            run(&[
                "extract_band".as_ref(),
                image.as_os_str(),
                rgb.as_os_str(),
                "0".as_ref(),
                "--n=3".as_ref(),
            ]);
            run(&[
                "colourspace".as_ref(),
                rgb.as_os_str(),
                reference.as_os_str(),
                "rgb16".as_ref(),
            ]);
            let reference = lab(&reference);
            run(&[
                "extract_band".as_ref(),
                output.as_os_str(),
                rgb.as_os_str(),
                "0".as_ref(),
                "--n=3".as_ref(),
            ]);
            let actual = lab(&rgb);
            let errors: Vec<_> = reference[..3]
                .iter()
                .zip(&actual[..3])
                .map(|(a, b)| delta_e(*a, *b))
                .collect();
            assert_fidelity(&errors, &format!("{format} JXL"));
        }
    }
}

fn xyz_lab(xyz: [f64; 3]) -> [f64; 3] {
    let [x, y, z] = std::array::from_fn(|i| {
        let t = xyz[i] / [95.047, 100.0, 108.883][i];
        if t > (6.0f64 / 29.0).powi(3) {
            t.cbrt()
        } else {
            t / (3.0 * (6.0f64 / 29.0).powi(2)) + 4.0 / 29.0
        }
    });
    [116.0 * y - 16.0, 500.0 * (x - y), 200.0 * (y - z)]
}

fn assert_fidelity(errors: &[f64], label: &str) {
    let mean = errors.iter().sum::<f64>() / errors.len() as f64;
    let max = errors.iter().copied().fold(0.0, f64::max);
    eprintln!("fidelity {label}: mean={mean:.4}, max={max:.4}");
    assert!(mean < 1.0 && max < 2.5, "{label}: mean={mean}, max={max}");
}

#[test]
fn iso_gain_map_fixture_uses_item_references_without_text_signatures() {
    let path = fixture("gain-map.avif");
    let bytes = std::fs::read(&path).unwrap();
    assert!(!headers::has_gain_signature(&bytes));
    let id = identify(&path, &Limits::default()).unwrap();
    assert!(id.has_gain_map);
    assert_eq!((id.width, id.height), (32, 32));
    assert_eq!(
        decode(&path, &id, &Limits::default())
            .unwrap()
            .dimensions()
            .unwrap(),
        (32, 32)
    );
    assert!(
        !identify(&fixture("chart.avif"), &Limits::default())
            .unwrap()
            .has_gain_map
    );
    #[cfg(feature = "heif")]
    assert!(
        !identify(&fixture("chart.heic"), &Limits::default())
            .unwrap()
            .has_gain_map
    );
}

fn heif_meta(children: &[u8]) -> Vec<u8> {
    let mut bytes = bmff_box(b"ftyp", b"avif\0\0\0\0avifmif1");
    let mut payload = vec![0; 4];
    payload.extend(children);
    bytes.extend(bmff_box(b"meta", &payload));
    bytes
}

fn heif_items(wide: bool, primary: u32, derived: [u8; 4], inputs: [u32; 2]) -> Vec<u8> {
    let id = |value: u32| {
        if wide {
            value.to_be_bytes().to_vec()
        } else {
            (value as u16).to_be_bytes().to_vec()
        }
    };
    let mut pitm = vec![u8::from(wide), 0, 0, 0];
    pitm.extend(id(primary));
    let mut entries = Vec::new();
    for (item, kind) in [(1, *b"av01"), (2, *b"av01"), (3, derived)] {
        let mut infe = vec![if wide { 3 } else { 2 }, 0, 0, 0];
        infe.extend(id(item));
        infe.extend([0, 0]); // protection index
        infe.extend(kind);
        infe.push(0); // empty item name
        entries.extend(bmff_box(b"infe", &infe));
    }
    let mut iinf = vec![u8::from(wide), 0, 0, 0];
    iinf.extend(id(3));
    iinf.extend(entries);
    let mut dimg = id(3);
    dimg.extend(2u16.to_be_bytes());
    for input in inputs {
        dimg.extend(id(input));
    }
    let mut iref = vec![u8::from(wide), 0, 0, 0];
    iref.extend(bmff_box(b"dimg", &dimg));
    // Intentionally out of order: references may precede item declarations.
    let mut children = bmff_box(b"iref", &iref);
    children.extend(bmff_box(b"iinf", &iinf));
    children.extend(bmff_box(b"pitm", &pitm));
    children
}

#[test]
fn iso_gain_maps_require_tmap_and_primary_first_in_dimg() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    for wide in [false, true] {
        for (primary, kind, inputs, expected) in [
            (1, b"tmap", [1, 2], true),
            (2, b"tmap", [1, 2], false),
            (1, b"iden", [1, 2], false),
            (1, b"tmap", [2, 1], false),
            (1, b"tmap", [1, 99], false),
            (1, b"tmap", [1, 1], false),
            (1, b"tmap", [1, 3], false),
            (3, b"tmap", [3, 2], false),
        ] {
            std::fs::write(
                tmp.path(),
                heif_meta(&heif_items(wide, primary, *kind, inputs)),
            )
            .unwrap();
            assert_eq!(
                headers::gain_map(tmp.path()),
                Ok(expected),
                "wide={wide} {primary} {kind:?} {inputs:?}"
            );
        }
    }
    // tmap/signature-like bytes in pixel payloads and unknown properties are opaque.
    let mut bytes = heif_meta(&bmff_box(
        b"iprp",
        &bmff_box(b"ipco", &bmff_box(b"free", b"HDRGainMap tmap")),
    ));
    bytes.extend(bmff_box(b"mdat", b"HDRGainMap tmap"));
    std::fs::write(tmp.path(), bytes).unwrap();
    assert_eq!(headers::gain_map(tmp.path()), Ok(false));
    let aux = bmff_box(
        b"auxC",
        b"\0\0\0\0urn:com:apple:photo:2020:aux:hdrgainmap\0",
    );
    std::fs::write(
        tmp.path(),
        heif_meta(&bmff_box(b"iprp", &bmff_box(b"ipco", &aux))),
    )
    .unwrap();
    assert_eq!(headers::gain_map(tmp.path()), Ok(true));
}

#[test]
fn heif_gain_map_box_extents_counts_and_scan_budgets_are_bounded() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let cases = [
        (bmff_box(b"iinf", b"\0\0\0\0\xff\xff"), CodecError::Limits),
        (bmff_box(b"iinf", b"\0\0\0\0\0\x01"), CodecError::Decode),
        (
            bmff_box(b"iinf", b"\0\0\0\0\0\x01\0\0\0\0infe"),
            CodecError::Decode,
        ),
        (bmff_box(b"iref", &[0, 0, 0]), CodecError::Decode),
        (bmff_box(b"iref", &[2, 0, 0, 0]), CodecError::Decode),
        (
            bmff_box(
                b"iref",
                &[0, 0, 0, 0]
                    .into_iter()
                    .chain(bmff_box(b"dimg", b"\0\x03\xff\xff"))
                    .collect::<Vec<_>>(),
            ),
            CodecError::Limits,
        ),
        (
            bmff_box(
                b"iref",
                &[0, 0, 0, 0]
                    .into_iter()
                    .chain(bmff_box(b"dimg", b"\0\x03\0\x02\0\x01"))
                    .collect::<Vec<_>>(),
            ),
            CodecError::Decode,
        ),
        (
            bmff_box(b"iprp", &bmff_box(b"ipco", b"\0\0\0\0auxC")),
            CodecError::Decode,
        ),
        (
            bmff_box(b"iprp", &bmff_box(b"ipco", &bmff_box(b"auxC", &[0; 3]))),
            CodecError::Decode,
        ),
        (
            bmff_box(
                b"iprp",
                &bmff_box(b"ipco", &bmff_box(b"free", &[]).repeat(4096)),
            ),
            CodecError::Limits,
        ),
    ];
    for (children, error) in cases {
        std::fs::write(tmp.path(), heif_meta(&children)).unwrap();
        assert_eq!(headers::gain_map(tmp.path()), Err(error));
    }
    // A bounded metadata payload and shared box budget include opaque boxes.
    std::fs::write(tmp.path(), heif_meta(&vec![0; 16 << 20])).unwrap();
    assert_eq!(headers::gain_map(tmp.path()), Err(CodecError::Limits));
    let children = bmff_box(b"free", &vec![0; 8 << 20]);
    let mut bytes = heif_meta(&children);
    bytes.extend(heif_meta(&children).split_off(24)); // second meta, no second ftyp
    std::fs::write(tmp.path(), bytes).unwrap();
    assert_eq!(headers::gain_map(tmp.path()), Err(CodecError::Limits));
    let mut bytes = heif_meta(&[]);
    bytes.extend(bmff_box(b"free", &[]).repeat(4095));
    std::fs::write(tmp.path(), bytes).unwrap();
    assert_eq!(headers::gain_map(tmp.path()), Err(CodecError::Limits));
    // Extended-size boxes use the same checked extent parser.
    let mut bytes = heif_meta(&[]);
    bytes.extend(b"\0\0\0\x01free");
    bytes.extend(u64::MAX.to_be_bytes());
    std::fs::write(tmp.path(), bytes).unwrap();
    assert_eq!(headers::gain_map(tmp.path()), Err(CodecError::Decode));
}
