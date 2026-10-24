//! Force the public memory-backed path and compare it with the same intact base.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation
)]
use super::*;
use crate::{Encode, EncodeOpts, Limits, ResizeSpec, native};
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}

// Metadata payloads and new meta are appended so the original pixel extents stay
// fixed. Replacing cdsc links makes the test independent of stock fixture EXIF.
fn decorated(seed: &[u8]) -> Vec<u8> {
    let top = boxes(seed, &mut ScanBudget::default()).unwrap();
    let original = one(&top, b"meta").unwrap();
    let meta = boxes(&original[4..], &mut ScanBudget::default()).unwrap();
    let primary = one(&meta, b"pitm").unwrap();
    assert_eq!(primary[0], 0);
    let primary = &primary[4..6];
    let source =
        native::Image::load(&fixture("images/exif-heavy.jpg"), &crate::Kind::Jpeg).unwrap();
    let exif = source.blob("exif-data").unwrap();
    let mut exif = exif.strip_prefix(b"Exif\0\0").unwrap().to_vec();
    // Keep a nontrivial EXIF orientation, even though HEIF display uses irot/imir.
    assert_eq!(&exif[..2], b"II");
    let ifd = u32::from_le_bytes(exif[4..8].try_into().unwrap()) as usize;
    let entries = u16::from_le_bytes(exif[ifd..ifd + 2].try_into().unwrap());
    for at in (ifd + 2..ifd + 2 + usize::from(entries) * 12).step_by(12) {
        if exif[at..at + 2] == 274_u16.to_le_bytes() {
            exif[at + 8..at + 10].copy_from_slice(&6_u16.to_le_bytes());
        }
    }
    let mut payload = vec![0; 4]; // TIFF offset in HEIF Exif item
    payload.extend(exif);
    let xmp = br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmp:Rating="4"><dc:title><rdf:Alt><rdf:li xml:lang="x-default">Fallback metadata</rdf:li></rdf:Alt></dc:title></rdf:Description></rdf:RDF></x:xmpmeta>"#;
    let mut result = seed.to_vec();
    let start = original.as_ptr() as usize - seed.as_ptr() as usize;
    result[start - 4..start].copy_from_slice(b"free");
    let exif_at = result.len() + 8;
    let xmp_at = exif_at + payload.len();
    payload.extend(xmp);
    append(&mut result, b"mdat", &payload).unwrap();
    let mut iinf = one(&meta, b"iinf").unwrap().to_vec();
    assert_eq!(iinf[0], 0);
    let count = u16::from_be_bytes(iinf[4..6].try_into().unwrap()) + 2;
    iinf[4..6].copy_from_slice(&count.to_be_bytes());
    for (item, kind, suffix) in [
        (60000_u16, b"Exif", b"\0".as_slice()),
        (60001, b"mime", b"\0application/rdf+xml\0identity\0"),
    ] {
        let mut info = vec![2, 0, 0, 0];
        info.extend(item.to_be_bytes());
        info.extend([0, 0]);
        info.extend(kind);
        info.extend(suffix);
        append(&mut iinf, b"infe", &info).unwrap();
    }
    let mut locations = super::super::locations(one(&meta, b"iloc").unwrap()).unwrap();
    locations.insert(60000, vec![(0, exif_at as u64, (xmp_at - exif_at) as u64)]);
    locations.insert(60001, vec![(0, xmp_at as u64, xmp.len() as u64)]);
    let mut iloc = vec![0, 0, 0, 0, 0x44, 0];
    iloc.extend((locations.len() as u16).to_be_bytes());
    for (item, extents) in locations {
        iloc.extend((item as u16).to_be_bytes());
        iloc.extend([0, 0]); // data reference index
        iloc.extend((extents.len() as u16).to_be_bytes());
        for (method, offset, len) in extents {
            assert_eq!(method, 0);
            iloc.extend((offset as u32).to_be_bytes());
            iloc.extend((len as u32).to_be_bytes());
        }
    }
    let mut iref = vec![0; 4];
    let mut auxiliary = None;
    if let Some(refs) = meta.iter().find(|b| b.kind == b"iref") {
        assert_eq!(refs.data[0], 0);
        for reference in boxes(&refs.data[4..], &mut ScanBudget::default()).unwrap() {
            if reference.kind == b"auxl" && reference.data.get(4..) == Some(primary) {
                auxiliary = reference.data.get(..2);
            }
            if reference.kind != b"cdsc" {
                append(&mut iref, reference.kind, reference.data).unwrap();
            }
        }
    }
    for item in [60000_u16, 60001] {
        let mut reference = item.to_be_bytes().to_vec();
        reference.extend(if auxiliary.is_some() { 2_u16 } else { 1 }.to_be_bytes());
        reference.extend(primary);
        if let Some(auxiliary) = auxiliary {
            reference.extend(auxiliary);
        }
        append(&mut iref, b"cdsc", &reference).unwrap();
    }
    let mut rebuilt = vec![0; 4];
    for child in meta {
        let replacement = match child.kind {
            b"iinf" => &iinf,
            b"iloc" => &iloc,
            b"iref" => continue,
            _ => child.data,
        };
        append(&mut rebuilt, child.kind, replacement).unwrap();
    }
    append(&mut rebuilt, b"iref", &iref).unwrap();
    append(&mut result, b"meta", &rebuilt).unwrap();
    result
}

fn broken_auxiliary(seed: &[u8]) -> Vec<u8> {
    // A truncated optional reference breaks libheif while primary metadata,
    // image dependencies and alpha remain intact and independently readable.
    with_reference(seed, b"auxl", &[0xea, 0x5f, 0, 1])
}

fn with_reference(seed: &[u8], kind: &[u8], data: &[u8]) -> Vec<u8> {
    let top = boxes(seed, &mut ScanBudget::default()).unwrap();
    let original = one(&top, b"meta").unwrap();
    let meta = boxes(&original[4..], &mut ScanBudget::default()).unwrap();
    let mut result = seed.to_vec();
    let start = original.as_ptr() as usize - seed.as_ptr() as usize;
    result[start - 4..start].copy_from_slice(b"free");
    let mut rebuilt = vec![0; 4];
    for child in meta {
        if child.kind == b"iref" {
            let mut refs = child.data.to_vec();
            append(&mut refs, kind, data).unwrap();
            append(&mut rebuilt, b"iref", &refs).unwrap();
        } else {
            append(&mut rebuilt, child.kind, child.data).unwrap();
        }
    }
    append(&mut result, b"meta", &rebuilt).unwrap();
    result
}

#[test]
#[allow(clippy::too_many_lines)] // One table compares the complete SDR contract before/after fallback.
fn fallback_matches_normal_sdr_metadata_color_dimensions_and_alpha() {
    let limits = Limits::default();
    let normal = tempfile::NamedTempFile::new().unwrap();
    let fallback = tempfile::NamedTempFile::new().unwrap();
    for name in [
        "images/chart.avif",
        "images/chart.heic", // ICC
        "images/chart-p3-nclx.avif",
        "images/chart-p3-nclx.heic", // NCLX
        "images/alpha-10.avif",
        "images/alpha-12.avif",
        "images/alpha-10.heic",
        "images/alpha-12.heic",
        "hdr/orientation/heic-container-6.heic",
        "hdr/orientation/avif-container-5.avif",
    ] {
        let original = decorated(&std::fs::read(fixture(name)).unwrap());
        std::fs::write(normal.path(), &original).unwrap();
        std::fs::write(fallback.path(), broken_auxiliary(&original)).unwrap();
        let id = crate::identify(normal.path(), &limits).unwrap();
        let broken_id = crate::identify(fallback.path(), &limits).unwrap();
        let raw = native::Image::load(normal.path(), &id.kind).unwrap();
        let broken_raw = native::Image::load(fallback.path(), &id.kind).unwrap();
        assert_eq!(
            raw.string("vips-loader").as_deref(),
            Some("heifload"),
            "{name}"
        );
        assert_eq!(
            broken_raw.string("vips-loader").as_deref(),
            Some("heifload_source"),
            "must exercise actual fallback: {name}"
        );
        assert_eq!(raw.size().unwrap(), broken_raw.size().unwrap(), "{name}");
        assert_eq!(raw.orientation(), broken_raw.orientation(), "{name}");
        for field in ["icc-profile-data", "exif-data", "xmp-data"] {
            assert_eq!(raw.blob(field), broken_raw.blob(field), "{name}: {field}");
        }
        let metadata = crate::metadata(normal.path()).unwrap();
        assert_eq!(metadata.make.as_deref(), Some("Yhotos"));
        assert!(metadata.taken_at.is_some() && metadata.lens.is_some() && metadata.gps.is_some());
        assert_eq!(metadata.title.as_deref(), Some("Fallback metadata"));
        assert_eq!(metadata.rating, Some(4));
        assert_eq!(
            serde_json::to_value(metadata).unwrap(),
            serde_json::to_value(crate::metadata(fallback.path()).unwrap()).unwrap(),
            "{name}"
        );
        let normal = crate::decode(normal.path(), &id, &limits).unwrap();
        let fallback = crate::decode(fallback.path(), &broken_id, &limits).unwrap();
        assert!(!fallback.has_gain_map());
        assert!(fallback.hdr_skipped_reason().is_some());
        assert_eq!(
            normal.image.pixels().unwrap(),
            fallback.image.pixels().unwrap(),
            "{name}: XYZ and normalized alpha must match exactly (stronger than fidelity gates)"
        );
        if normal.alpha {
            let alpha = fallback.image.extract(3, 1).unwrap().pixels().unwrap();
            let values: Vec<_> = alpha
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| u16::from_ne_bytes([b[0], b[1]]))
                .collect();
            assert_eq!(values.iter().max(), Some(&65535), "{name}");
            assert_eq!(values.iter().min(), Some(&0), "{name}");
            assert!(values.iter().any(|v| (1..65535).contains(v)));
        }
        for encoder in [
            Encode::Avif {
                quality: 90,
                speed: 8,
                hdr: false,
            },
            Encode::Jxl {
                distance: 0.0,
                effort: 3,
                hdr: false,
            },
            Encode::Jpeg {
                quality: 90,
                progressive: true,
                chroma_subsampling: false,
                hdr: false,
            },
        ] {
            let outputs = [
                tempfile::NamedTempFile::new().unwrap(),
                tempfile::NamedTempFile::new().unwrap(),
            ];
            for (working, output) in [(&normal, &outputs[0]), (&fallback, &outputs[1])] {
                let resized = crate::resize(
                    working,
                    ResizeSpec {
                        long_edge: 24,
                        square: false,
                    },
                )
                .unwrap();
                crate::encode(
                    &resized,
                    encoder,
                    EncodeOpts {
                        keep_icc: true,
                        strip_metadata: true,
                    },
                    output.path(),
                )
                .unwrap();
            }
            let a = crate::identify(outputs[0].path(), &limits).unwrap();
            let b = crate::identify(outputs[1].path(), &limits).unwrap();
            assert_eq!(a, b, "{name}");
            assert_eq!(
                crate::decode(outputs[0].path(), &a, &limits)
                    .unwrap()
                    .image
                    .pixels()
                    .unwrap(),
                crate::decode(outputs[1].path(), &b, &limits)
                    .unwrap()
                    .image
                    .pixels()
                    .unwrap(),
                "{name}"
            );
            let a = native::Image::load(outputs[0].path(), &a.kind).unwrap();
            let b = native::Image::load(outputs[1].path(), &b.kind).unwrap();
            for field in ["icc-profile-data", "exif-data", "xmp-data"] {
                assert_eq!(a.blob(field), b.blob(field), "{name}: derivative {field}");
            }
            assert!(a.blob("xmp-data").is_none() && b.blob("xmp-data").is_none());
            assert!(crate::metadata(outputs[1].path()).unwrap().gps.is_none());
        }
    }
}

#[test]
fn every_container_box_walk_shares_count_and_byte_limits() {
    let many = boxed(b"free", &[]).unwrap().repeat(4097);
    assert!(matches!(
        boxes(&many, &mut ScanBudget::default()),
        Err(CodecError::Limits)
    ));
    assert!(matches!(heif_sdr(&many), Err(CodecError::Limits)));
    let oversized = boxed(b"free", &vec![0; (16 << 20) - 8]).unwrap();
    let mut bytes = boxed(b"free", &[]).unwrap();
    bytes.extend(oversized);
    assert!(matches!(
        boxes(&bytes, &mut ScanBudget::default()),
        Err(CodecError::Limits)
    ));
    let path = tempfile::NamedTempFile::new().unwrap();
    for bytes in [many, bytes] {
        std::fs::write(path.path(), bytes).unwrap();
        assert_eq!(
            crate::gain_container::jxl_present(path.path()),
            Err(CodecError::Limits)
        );
    }
    // A nested walk consumes the parent's budget instead of resetting it.
    let children = boxed(b"free", &[]).unwrap().repeat(4095);
    let nested = boxed(b"meta", &children).unwrap();
    let mut budget = ScanBudget::default();
    let top = boxes(&nested, &mut budget).unwrap();
    let parsed = boxes(top[0].data, &mut budget).unwrap();
    assert_eq!(parsed.len(), 4095);
    assert!(matches!(
        boxes(&boxed(b"free", &[]).unwrap(), &mut budget),
        Err(CodecError::Limits)
    ));
}

#[test]
fn fallback_normalizes_ten_and_twelve_bit_heif_alpha() {
    let input = tempfile::NamedTempFile::new().unwrap();
    for name in [
        "alpha-10.avif",
        "alpha-12.avif",
        "alpha-10.heic",
        "alpha-12.heic",
    ] {
        let bytes = std::fs::read(fixture(&format!("images/{name}"))).unwrap();
        std::fs::write(input.path(), broken_auxiliary(&bytes)).unwrap();
        let id = crate::identify(input.path(), &Limits::default()).unwrap();
        let image = native::Image::load(input.path(), &id.kind).unwrap();
        assert_eq!(
            image.string("vips-loader").as_deref(),
            Some("heifload_source")
        );
        let alpha = crate::decode(input.path(), &id, &Limits::default())
            .unwrap()
            .image
            .extract(3, 1)
            .unwrap()
            .pixels()
            .unwrap();
        let max = alpha
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_ne_bytes([b[0], b[1]]))
            .max();
        assert_eq!(
            max,
            Some(65535),
            "{name}: full opacity must not become transparency"
        );
    }
}

#[test]
fn projection_preserves_premultiplied_alpha_references() {
    let bytes = std::fs::read(fixture("images/alpha-10.avif")).unwrap();
    let top = boxes(&bytes, &mut ScanBudget::default()).unwrap();
    let meta = boxes(
        &one(&top, b"meta").unwrap()[4..],
        &mut ScanBudget::default(),
    )
    .unwrap();
    let refs = boxes(
        &one(&meta, b"iref").unwrap()[4..],
        &mut ScanBudget::default(),
    )
    .unwrap();
    let alpha = refs.iter().find(|r| r.kind == b"auxl").unwrap().data;
    let mut prem = alpha[4..6].to_vec(); // primary -> alpha, opposite of auxl
    prem.extend(1_u16.to_be_bytes());
    prem.extend(&alpha[..2]);
    let original = with_reference(&bytes, b"prem", &prem);
    let projected = heif_sdr(&original).unwrap();
    let top = boxes(&projected, &mut ScanBudget::default()).unwrap();
    let meta = boxes(
        &one(&top, b"meta").unwrap()[4..],
        &mut ScanBudget::default(),
    )
    .unwrap();
    let refs = boxes(
        &one(&meta, b"iref").unwrap()[4..],
        &mut ScanBudget::default(),
    )
    .unwrap();
    assert_eq!(one(&refs, b"prem").unwrap(), prem);
    let path = tempfile::NamedTempFile::new().unwrap();
    let mut pixels = Vec::new();
    for bytes in [original, broken_auxiliary(&projected)] {
        std::fs::write(path.path(), bytes).unwrap();
        let id = crate::identify(path.path(), &Limits::default()).unwrap();
        pixels.push(
            crate::decode(path.path(), &id, &Limits::default())
                .unwrap()
                .image
                .pixels()
                .unwrap(),
        );
    }
    assert_eq!(pixels[0], pixels[1]);
}

#[test]
fn primary_metadata_requires_complete_unprotected_declarations() {
    for (encoding, expected) in [
        (b"".as_slice(), true),
        (b"\0", true),
        (b"identity\0", true),
        (b"gzip\0", true),
        (b"unterminated", false),
        (b"\xff\0", false),
        (b"a\0b\0", false),
    ] {
        let mut data = b"\x02\0\0\0\0\x01\0\0mime\0application/rdf+xml\0".to_vec();
        data.extend(encoding);
        assert_eq!(
            primary_metadata(&Box {
                kind: b"infe",
                data: &data
            }),
            expected
        );
        data[7] = 1; // protected metadata requires an unavailable decryption step
        assert!(!primary_metadata(&Box {
            kind: b"infe",
            data: &data
        }));
    }
}

#[test]
fn jxl_projection_preserves_base_icc_exif_and_xmp() {
    let seed = std::fs::read(fixture("hdr/gain-map.jxl")).unwrap();
    let top = boxes(&seed, &mut ScanBudget::default()).unwrap();
    let source =
        native::Image::load(&fixture("images/exif-heavy.jpg"), &crate::Kind::Jpeg).unwrap();
    let exif = source.blob("exif-data").unwrap();
    let mut payload = vec![0; 4]; // JPEG XL Exif box TIFF offset
    payload.extend(exif.strip_prefix(b"Exif\0\0").unwrap());
    let xmp = source.blob("xmp-data").unwrap();
    let mut normal = Vec::new();
    for item in top.iter().filter(|b| b.kind != b"jhgm") {
        append(&mut normal, item.kind, item.data).unwrap();
    }
    append(&mut normal, b"Exif", &payload).unwrap();
    append(&mut normal, b"xml ", &xmp).unwrap();
    let gain_at = normal.len();
    append(&mut normal, b"jhgm", one(&top, b"jhgm").unwrap()).unwrap();
    let input = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(input.path(), &normal).unwrap();
    let raw = native::Image::load(input.path(), &crate::Kind::Jxl).unwrap();
    let metadata = serde_json::to_value(crate::metadata(input.path()).unwrap()).unwrap();
    let normal_pixels = crate::from_image(raw).unwrap().image.pixels().unwrap();
    for end in [gain_at + 1, gain_at + 7, gain_at + 10, normal.len() - 1] {
        let broken = &normal[..end];
        let projected = crate::gain_container::jxl_sdr(broken).unwrap();
        let kept = boxes(&projected, &mut ScanBudget::default()).unwrap();
        // Older libvips builds expose no JXL EXIF/XMP; verify the complete
        // payloads independently, then compare each native build with itself.
        assert_eq!(one(&kept, b"Exif").unwrap(), payload);
        assert_eq!(one(&kept, b"xml ").unwrap(), xmp);
        let image = native::Image::jxl_sdr_buffer(&projected, &Limits::default()).unwrap();
        assert_eq!(
            crate::from_image(image).unwrap().image.pixels().unwrap(),
            normal_pixels
        );
        std::fs::write(input.path(), broken).unwrap();
        let id = crate::identify(input.path(), &Limits::default()).unwrap();
        let working = crate::decode(input.path(), &id, &Limits::default()).unwrap();
        assert_eq!(working.image.pixels().unwrap(), normal_pixels);
        assert!(!working.has_gain_map() && working.hdr_skipped_reason().is_some());
        assert_eq!(
            serde_json::to_value(crate::metadata(input.path()).unwrap()).unwrap(),
            metadata
        );
        let before = native::Image::jxl_sdr_buffer(&normal[..gain_at], &Limits::default()).unwrap();
        let after = native::Image::jxl_sdr_buffer(&projected, &Limits::default()).unwrap();
        for field in ["icc-profile-data", "exif-data", "xmp-data"] {
            assert_eq!(before.blob(field), after.blob(field), "{field}");
        }
    }
    // The JPEG XL recovery walk enforces the same global count and byte caps.
    let mut many = normal[..12].to_vec();
    many.extend(boxed(b"free", &[]).unwrap().repeat(4096));
    assert_eq!(
        crate::gain_container::jxl_sdr(&many),
        Err(CodecError::Limits)
    );
}

#[test]
fn discarded_auxiliary_extents_cannot_overflow_primary_metadata_accounting() {
    let mut data = vec![1, 0, 0, 0, 0x88, 0, 0, 2]; // v1, 64-bit offsets/lengths
    for (item, extents) in [
        (1_u16, vec![(0_u64, 1_u64)]),
        (2, vec![(u64::MAX, u64::MAX); 2]),
    ] {
        data.extend(item.to_be_bytes());
        data.extend([0; 4]); // construction method and data reference
        data.extend((extents.len() as u16).to_be_bytes());
        for (offset, len) in extents {
            data.extend(offset.to_be_bytes());
            data.extend(len.to_be_bytes());
        }
    }
    let mut required = BTreeSet::from([1]);
    assert!(iloc(&data, &mut required, &BTreeSet::new(), &[0], &[]).is_ok());
    assert_eq!(required, BTreeSet::from([1]));
}
