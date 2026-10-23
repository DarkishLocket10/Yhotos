#![forbid(unsafe_code)]
//! Paired SDR/HDR JPEG/JXL costs with the default plan's encoded-base reuse.
use criterion::Criterion;
use std::{
    error::Error,
    hint::black_box,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use yhotos_codec::{Encode, EncodeOpts, Limits, ResizeSpec};
#[allow(clippy::too_many_lines)] // Keep paired benchmark setup and timing policy together.
fn main() -> Result<(), Box<dyn Error>> {
    let input = std::env::var_os("YHOTOS_HDR_BENCH_FIXTURE")
        .map_or_else(
            || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/hdr/iso.jpg"),
            PathBuf::from,
        )
        .canonicalize()?;
    let id = yhotos_codec::identify(&input, &Limits::default())?;
    eprintln!(
        "HDR benchmark source: {:?}x{:?}; tower 24 MP budget requires YHOTOS_HDR_BENCH_FIXTURE",
        id.width, id.height
    );
    let working = yhotos_codec::decode(&input, &id, &Limits::default())?;
    if !working.has_gain_map() {
        return Err("benchmark requires a supported gain map".into());
    }
    let output = tempfile::NamedTempFile::new()?;
    let mut criterion = Criterion::default()
        .sample_size(20)
        .warm_up_time(Duration::from_secs(2))
        .measurement_time(Duration::from_secs(5))
        .configure_from_args();
    let mut failure = None;
    criterion.bench_function("hdr/identify_jpeg_gain_map", |b| {
        b.iter(|| yhotos_codec::identify(black_box(&input), &Limits::default()));
    });
    criterion.bench_function("hdr/decode_jpeg_gain_map", |b| {
        b.iter(|| yhotos_codec::decode(black_box(&input), &id, &Limits::default()));
    });
    let avif = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/images/gain-map.avif");
    let mut broken_avif = std::fs::read(&avif)?;
    let reference = broken_avif
        .windows(4)
        .position(|bytes| bytes == b"dimg")
        .ok_or("missing tmap reference")?;
    broken_avif
        .get_mut(reference + 10..reference + 12)
        .ok_or("missing gain target")?
        .fill(0);
    let malformed = tempfile::NamedTempFile::new()?;
    std::fs::write(malformed.path(), broken_avif)?;
    for (form, path) in [("valid", avif.as_path()), ("dangling", malformed.path())] {
        criterion.bench_function(&format!("hdr/identify_avif_tmap/{form}"), |b| {
            b.iter(|| yhotos_codec::identify(black_box(path), &Limits::default()));
        });
        if let Ok(identity) = yhotos_codec::identify(path, &Limits::default()) {
            criterion.bench_function(&format!("hdr/decode_avif_tmap/{form}"), |b| {
                b.iter(|| yhotos_codec::decode(black_box(path), &identity, &Limits::default()));
            });
        }
    }
    for (form, name) in [
        ("plain", "gain-map.jxl"),
        ("compressed", "gain-map-brob.jxl"),
        ("rotated", "orientation/jxl-6.jxl"),
        ("mirrored", "orientation/jxl-2.jxl"),
    ] {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/hdr")
            .join(name);
        let identity = yhotos_codec::identify(&path, &Limits::default())?;
        if form != "compressed"
            && !yhotos_codec::decode(&path, &identity, &Limits::default())?.has_gain_map()
        {
            return Err("JXL benchmark lost its gain map".into());
        }
        criterion.bench_function(&format!("hdr/identify_jxl_gain_map/{form}"), |b| {
            b.iter(|| yhotos_codec::identify(black_box(&path), &Limits::default()));
        });
        criterion.bench_function(&format!("hdr/decode_jxl_gain_map/{form}"), |b| {
            b.iter(|| yhotos_codec::decode(black_box(&path), &identity, &Limits::default()));
        });
    }
    for (form, name) in [
        ("attribute", "apple.heic"),
        ("element", "apple-element.heic"),
        ("rotated", "orientation/heic-container-6.heic"),
        ("mirrored", "orientation/heic-container-2.heic"),
    ] {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/hdr")
            .join(name);
        let identity = yhotos_codec::identify(&path, &Limits::default())?;
        criterion.bench_function(&format!("hdr/decode_apple_heic/{form}"), |b| {
            b.iter(|| yhotos_codec::decode(black_box(&path), &identity, &Limits::default()));
        });
    }
    for (form, name) in [
        ("rotated", "orientation/avif-container-6.avif"),
        ("mirrored", "orientation/avif-container-2.avif"),
    ] {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/hdr")
            .join(name);
        let identity = yhotos_codec::identify(&path, &Limits::default())?;
        criterion.bench_function(&format!("hdr/decode_avif_tmap/{form}"), |b| {
            b.iter(|| yhotos_codec::decode(black_box(&path), &identity, &Limits::default()));
        });
    }
    for (variant, edge, quality) in [
        ("medium", 1280, 90),
        ("large", 2048, 99),
        ("xlarge", 3200, 99),
    ] {
        let resized = yhotos_codec::resize(
            &working,
            ResizeSpec {
                long_edge: edge,
                square: false,
            },
        )?;
        for (format, distance) in [
            ("jpeg", None),
            (
                "jxl",
                Some(if edge <= 1600 {
                    1.0
                } else if edge <= 2048 {
                    0.55
                } else {
                    0.78
                }),
            ),
        ] {
            for hdr in [false, true] {
                let name = format!(
                    "hdr/{}x{}/{variant}/{format}/{}",
                    id.width.unwrap_or(0),
                    id.height.unwrap_or(0),
                    if hdr { "gain_map" } else { "sdr" }
                );
                let encoder = distance.map_or(
                    Encode::Jpeg {
                        quality,
                        progressive: true,
                        chroma_subsampling: false,
                        hdr,
                    },
                    |distance| Encode::Jxl {
                        distance,
                        effort: 7,
                        hdr,
                    },
                );
                let options = EncodeOpts {
                    keep_icc: true,
                    strip_metadata: true,
                };
                let base = if hdr && (format == "jxl" || variant != "xlarge") {
                    yhotos_codec::encode_sdr_cached(
                        &resized,
                        encoder.sdr(),
                        options,
                        output.path(),
                        16 << 20,
                    )?
                    .1
                } else {
                    None
                };
                criterion.bench_function(&name, |b| {
                    b.iter_custom(|iterations| {
                        let start = Instant::now();
                        for _ in 0..iterations {
                            let result = if let Some(base) = &base {
                                yhotos_codec::encode_reusing_sdr(
                                    black_box(&resized),
                                    encoder,
                                    options,
                                    output.path(),
                                    base,
                                )
                            } else {
                                yhotos_codec::encode(
                                    black_box(&resized),
                                    encoder,
                                    options,
                                    output.path(),
                                )
                            };
                            match result {
                                Ok(result) => {
                                    black_box(result);
                                }
                                Err(error) => {
                                    failure = Some(error);
                                    break;
                                }
                            }
                        }
                        start.elapsed()
                    });
                });
            }
        }
    }
    criterion.final_summary();
    if let Some(error) = failure {
        return Err(error.into());
    }
    Ok(())
}
