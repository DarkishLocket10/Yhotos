#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Header latency and one independently resized medium derivative per encoder.
use criterion::{Criterion, criterion_group, criterion_main};
use std::{
    hint::black_box,
    path::{Path, PathBuf},
    time::Duration,
};
use yhotos_codec::{Encode, EncodeOpts, Limits, ResizeSpec};
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}
fn codec(c: &mut Criterion) {
    let input = root().join("images/chart-srgb.png");
    let limits = Limits::default();
    c.bench_function("codec/identify", |b| {
        b.iter(|| yhotos_codec::identify(black_box(&input), &limits).unwrap());
    });
    let tiff = root().join("images/16bit.tif");
    c.bench_function("codec/identify_tiff", |b| {
        b.iter(|| yhotos_codec::identify(black_box(&tiff), &limits).unwrap());
    });
    let alpha = root().join("images/alpha.png");
    let alpha_id = yhotos_codec::identify(&alpha, &limits).unwrap();
    c.bench_function("codec/decode_alpha", |b| {
        b.iter(|| yhotos_codec::decode(black_box(&alpha), &alpha_id, &limits).unwrap());
    });
    let alpha_working = yhotos_codec::decode(&alpha, &alpha_id, &limits).unwrap();
    let output = tempfile::NamedTempFile::new().unwrap();
    c.bench_function("codec/encode_alpha_jpeg", |b| {
        b.iter(|| {
            yhotos_codec::encode(
                black_box(&alpha_working),
                Encode::Jpeg {
                    quality: 90,
                    progressive: true,
                    chroma_subsampling: false,
                    hdr: false,
                },
                EncodeOpts {
                    keep_icc: true,
                    strip_metadata: true,
                },
                output.path(),
            )
            .unwrap()
        });
    });
    let jpeg = root().join("images/progressive.jpg");
    c.bench_function("codec/identify_jpeg", |b| {
        b.iter(|| yhotos_codec::identify(black_box(&jpeg), &limits).unwrap());
    });
    for (name, file) in [
        ("identify_avif", "chart.avif"),
        ("identify_gain_map", "gain-map.avif"),
    ] {
        let input = root().join("images").join(file);
        c.bench_function(&format!("codec/{name}"), |b| {
            b.iter(|| yhotos_codec::identify(black_box(&input), &limits).unwrap());
        });
    }
    let float = root().join("images/float.tif");
    let float_id = yhotos_codec::identify(&float, &limits).unwrap();
    c.bench_function("codec/decode_float", |b| {
        b.iter(|| yhotos_codec::decode(black_box(&float), &float_id, &limits).unwrap());
    });
    #[cfg(feature = "raw")]
    raw_benchmarks(c);
    let large = root().join("large/24mp.jpg");
    if large.is_file() {
        medium(c, &large, "24mp");
    } else {
        eprintln!("SKIP medium/24mp: sync fixtures/large/24mp.jpg from tower");
    }
    medium(c, &input, "chart");
}
#[cfg(feature = "raw")]
fn raw_benchmarks(c: &mut Criterion) {
    let limits = Limits::default();
    for (name, make, header) in [
        ("cr2_makernote", b"Canon\0".as_slice(), b"".as_slice()),
        ("sony_makernote", b"SONY\0", b""),
        (
            "nikon_makernote",
            b"NIKON CORPORATION\0",
            b"Nikon\0\x02\0\0\0II*\0\x08\0\0\0",
        ),
    ] {
        let file = tempfile::Builder::new().suffix(".cr2").tempfile().unwrap();
        std::fs::write(file.path(), tiff_makernote(make, header)).unwrap();
        c.bench_function(&format!("codec/identify_{name}"), |b| {
            b.iter(|| yhotos_codec::identify(black_box(file.path()), &limits).unwrap());
        });
    }
    let preview = tempfile::Builder::new().suffix(".dng").tempfile().unwrap();
    let jpeg = std::fs::read(root().join("images/photo.jpg")).unwrap();
    let mut bytes = b"II*\0\x08\0\0\0".to_vec();
    classic_ifd(
        &mut bytes,
        &[
            (256, 4, 1, 1024),
            (257, 4, 1, 768),
            (50706, 1, 4, 0),
            (513, 4, 1, 128),
            (514, 4, 1, u32::try_from(jpeg.len()).unwrap()),
        ],
    );
    bytes.resize(128, 0);
    bytes.extend(jpeg);
    std::fs::write(preview.path(), bytes).unwrap();
    c.bench_function("codec/identify_tiff_preview", |b| {
        b.iter(|| yhotos_codec::identify(black_box(preview.path()), &limits).unwrap());
    });
    // A synthetic envelope around the owner's real JPEG measures the CR3 path
    // without requiring the optional tower RAW collection.
    let cr3 = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(cr3.path(), cr3_preview()).unwrap();
    c.bench_function("codec/identify_cr3_preview", |b| {
        b.iter(|| yhotos_codec::identify(black_box(cr3.path()), &limits).unwrap());
    });
    medium(c, cr3.path(), "cr3_preview");
}
#[cfg(feature = "raw")]
fn classic_ifd(bytes: &mut Vec<u8>, entries: &[(u16, u16, u32, u32)]) {
    bytes.extend(u16::try_from(entries.len()).unwrap().to_le_bytes());
    for &(tag, kind, count, value) in entries {
        bytes.extend(tag.to_le_bytes());
        bytes.extend(kind.to_le_bytes());
        bytes.extend(count.to_le_bytes());
        bytes.extend(value.to_le_bytes());
    }
    bytes.extend(0u32.to_le_bytes());
}
#[cfg(feature = "raw")]
fn tiff_makernote(make: &[u8], header: &[u8]) -> Vec<u8> {
    let mut bytes = b"II*\0\x08\0\0\0".to_vec();
    classic_ifd(
        &mut bytes,
        &[
            (256, 4, 1, 1024),
            (257, 4, 1, 768),
            (271, 2, u32::try_from(make.len()).unwrap(), 128),
            (37500, 7, u32::try_from(header.len()).unwrap() + 18, 256),
        ],
    );
    bytes.resize(128, 0);
    bytes.extend(make);
    bytes.resize(256, 0);
    bytes.extend(header);
    classic_ifd(&mut bytes, &[(0x0081, 4, 1, 512)]);
    bytes.resize(512, 0);
    bytes.extend([0xff, 0xd8, 0xff, 0xc3, 0, 11, 14, 3, 0, 4, 0, 1, 1, 0x11, 0]);
    bytes
}
#[cfg(feature = "raw")]
fn cr3_preview() -> Vec<u8> {
    let path = root().join("images/photo.jpg");
    let id = yhotos_codec::identify(&path, &Limits::default()).unwrap();
    let jpeg = std::fs::read(path).unwrap();
    let mut record = vec![0; 4];
    record.extend(1u16.to_be_bytes());
    record.extend(u16::try_from(id.width).unwrap().to_be_bytes());
    record.extend(u16::try_from(id.height).unwrap().to_be_bytes());
    record.extend(1u16.to_be_bytes());
    record.extend(u32::try_from(jpeg.len()).unwrap().to_be_bytes());
    record.extend(jpeg);
    let mut uuid = vec![
        0xea, 0xf4, 0x2b, 0x5e, 0x1c, 0x98, 0x4b, 0x88, 0xb9, 0xfb, 0xb7, 0xdc, 0x40, 0x6e, 0x4d,
        0x16,
    ];
    uuid.extend([0, 0, 0, 0, 0, 0, 0, 1]);
    uuid.extend(box_bytes(b"PRVW", &record));
    let mut bytes = box_bytes(b"ftyp", b"crx \0\0\0\0crx ");
    bytes.extend(box_bytes(b"moov", &[]));
    bytes.extend(box_bytes(b"mdat", &[]));
    bytes.extend(box_bytes(b"uuid", &uuid));
    bytes
}
#[cfg(feature = "raw")]
fn box_bytes(kind: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut bytes = u32::try_from(payload.len() + 8)
        .unwrap()
        .to_be_bytes()
        .to_vec();
    bytes.extend(kind);
    bytes.extend(payload);
    bytes
}
fn medium(c: &mut Criterion, input: &Path, label: &str) {
    let mut group = c.benchmark_group(format!("codec/medium/{label}"));
    group
        .sample_size(10)
        .measurement_time(Duration::from_secs(3));
    let tmp = tempfile::tempdir().unwrap();
    let output = tmp.path().join("output");
    let limits = Limits::default();
    let id = yhotos_codec::identify(input, &limits).unwrap();
    for (name, encode) in [
        (
            "avif",
            Encode::Avif {
                quality: 60,
                speed: 6,
                hdr: false,
            },
        ),
        (
            "jxl",
            Encode::Jxl {
                distance: 1.0,
                effort: 5,
                hdr: false,
            },
        ),
        (
            "jpeg",
            Encode::Jpeg {
                quality: 90,
                progressive: true,
                chroma_subsampling: false,
                hdr: false,
            },
        ),
    ] {
        group.bench_function(name, |b| {
            b.iter(|| {
                let working = yhotos_codec::decode(input, &id, &limits).unwrap();
                let resized = yhotos_codec::resize(
                    &working,
                    ResizeSpec {
                        long_edge: 1280,
                        square: false,
                    },
                )
                .unwrap();
                black_box(
                    yhotos_codec::encode(
                        &resized,
                        encode,
                        EncodeOpts {
                            keep_icc: true,
                            strip_metadata: true,
                        },
                        &output,
                    )
                    .unwrap(),
                )
            });
        });
    }
    group.finish();
}
criterion_group!(benches, codec);
criterion_main!(benches);
