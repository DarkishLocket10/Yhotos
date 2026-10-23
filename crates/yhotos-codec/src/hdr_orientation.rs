//! Independent corner markers catch rotations and reflections even when dimensions agree.
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::cast_precision_loss
)]
use crate::{Encode, EncodeOpts, Limits, ResizeSpec, Working, native};
use std::path::Path;

const INPUTS: [(&str, &str); 7] = [
    ("iso", "jpg"),
    ("adobe", "jpg"),
    ("heic-exif", "heic"),
    ("heic-container", "heic"),
    ("avif-exif", "avif"),
    ("avif-container", "avif"),
    ("jxl", "jxl"),
];

fn input(form: &str, extension: &str, orientation: u8) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/hdr/orientation")
        .join(format!("{form}-{orientation}.{extension}"))
}

fn displayed_orientation(form: &str, orientation: u8) -> u8 {
    // The libvips HEIF loader discards EXIF Orientation: irot/imir control display.
    // These additional fixtures prove that ignored EXIF never rotates only the gain map.
    if form.ends_with("-exif") {
        1
    } else {
        orientation
    }
}

fn corners(image: &native::Image, base: bool) -> [f64; 4] {
    let (w, h) = image.size().unwrap();
    let pixels = image.pixels().unwrap();
    let bands = usize::try_from(image.bands()).unwrap();
    [(1, 1), (3, 1), (1, 3), (3, 3)].map(|(x, y)| {
        let at = ((h as usize * y / 4) * w as usize + w as usize * x / 4) * bands;
        if base {
            // Linear XYZ ushort: the Y band is luminance.
            f64::from(u16::from_ne_bytes([
                pixels[(at + 1) * 2],
                pixels[(at + 1) * 2 + 1],
            ]))
        } else {
            f64::from(pixels[at])
        }
    })
}

fn assert_markers(working: &Working, orientation: u8) {
    // Expected source-quadrant positions, independently specified from the native transforms.
    let order = [
        [0, 1, 2, 3],
        [1, 0, 3, 2],
        [3, 2, 1, 0],
        [2, 3, 0, 1],
        [0, 2, 1, 3],
        [2, 0, 3, 1],
        [3, 1, 2, 0],
        [1, 3, 0, 2],
    ][usize::from(orientation - 1)];
    let expected = order.map(|i| [3, 0, 1, 2][i]);
    assert!(working.has_gain_map(), "{:?}", working.hdr_skipped_reason());
    let gain = &working.gain_map.as_ref().unwrap().image;
    let (w, h) = working.dimensions().unwrap();
    let (gw, gh) = gain.size().unwrap();
    assert_eq!((gw * 2, gh * 2), (w, h));
    for (image, base) in [(&working.image, true), (gain, false)] {
        let values = corners(image, base);
        let ranks = values.map(|v| values.iter().filter(|other| **other < v).count());
        assert_eq!(
            ranks, expected,
            "orientation {orientation}, base={base}, samples={values:?}"
        );
        // Distinct levels ensure quantization or a uniform map cannot make this test pass.
        let mut sorted = values;
        sorted.sort_by(f64::total_cmp);
        assert!(
            sorted
                .windows(2)
                .all(|pair| pair[1] - pair[0] > if base { 200.0 } else { 10.0 })
        );
    }
}

#[test]
fn jxl_retains_the_stored_base_transform_separately_from_display_identity() {
    for orientation in 1..=8 {
        let path = input("jxl", "jxl", orientation);
        let bytes = std::fs::read(&path).unwrap();
        let stored = native::jxl_stored_identity(&bytes[..], &Limits::default()).unwrap();
        assert_eq!(
            (stored.width, stored.height, stored.orientation),
            (Some(96), Some(64), orientation)
        );
        let displayed = crate::identify(&path, &Limits::default()).unwrap();
        assert_eq!(displayed.orientation, 1);
        assert_eq!(
            displayed.width.zip(displayed.height).unwrap(),
            if orientation >= 5 { (64, 96) } else { (96, 64) }
        );
    }
}

#[test]
fn malformed_heif_orientation_properties_are_rejected_before_gain_decode() {
    for (form, extension) in [("heic-container", "heic"), ("avif-container", "avif")] {
        let bytes = std::fs::read(input(form, extension, 5)).unwrap();
        for property in [b"irot", b"imir"] {
            for (offset, kind) in bytes
                .windows(4)
                .enumerate()
                .filter(|(_, kind)| *kind == property)
            {
                assert_eq!(kind, property);
                let mut bad = bytes.clone();
                // Reserved bits are forbidden; do not silently mask them into a transform.
                bad[offset + 4] |= 0x80;
                assert!(crate::gain_container::heif(&bad, &Limits::default()).is_err());
            }
        }
        let mut duplicate = bytes.clone();
        let mirror = bytes.windows(4).position(|kind| kind == b"imir").unwrap();
        // Orientation 5 already has a rotation followed by this mirror.
        duplicate[mirror..mirror + 4].copy_from_slice(b"irot");
        assert!(crate::gain_container::heif(&duplicate, &Limits::default()).is_err());
    }
}

#[test]
fn every_hdr_input_keeps_markers_aligned_for_all_eight_orientations() {
    for (form, extension) in INPUTS {
        for orientation in 1..=8 {
            let path = input(form, extension, orientation);
            let orientation = displayed_orientation(form, orientation);
            let id = crate::identify(&path, &Limits::default()).unwrap();
            let working = crate::decode(&path, &id, &Limits::default()).unwrap();
            eprintln!("input {form}, orientation {orientation}");
            assert_eq!(
                working.dimensions().unwrap(),
                if orientation >= 5 { (64, 96) } else { (96, 64) }
            );
            assert_markers(&working, orientation);
            for square in [false, true] {
                let resized = crate::resize(
                    &working,
                    ResizeSpec {
                        long_edge: 48,
                        square,
                    },
                )
                .unwrap();
                assert_markers(&resized, orientation);
            }
        }
    }
}

#[test]
fn hdr_encoders_preserve_oriented_markers_with_and_without_sdr_reuse() {
    let opts = EncodeOpts {
        keep_icc: true,
        strip_metadata: true,
    };
    let output = tempfile::NamedTempFile::new().unwrap();
    let sdr = tempfile::NamedTempFile::new().unwrap();
    for (form, extension) in INPUTS {
        for orientation in 1..=8 {
            let path = input(form, extension, orientation);
            let orientation = displayed_orientation(form, orientation);
            let id = crate::identify(&path, &Limits::default()).unwrap();
            let working = crate::decode(&path, &id, &Limits::default()).unwrap();
            for square in [false, true] {
                let working = crate::resize(
                    &working,
                    ResizeSpec {
                        long_edge: 48,
                        square,
                    },
                )
                .unwrap();
                for encoder in [
                    Encode::Jpeg {
                        quality: 100,
                        progressive: true,
                        chroma_subsampling: false,
                        hdr: true,
                    },
                    Encode::Jxl {
                        distance: 0.0,
                        effort: 3,
                        hdr: true,
                    },
                ] {
                    let (_, cache) = crate::encode_sdr_cached(
                        &working,
                        encoder.sdr(),
                        opts,
                        sdr.path(),
                        16 << 20,
                    )
                    .unwrap();
                    let cache = cache.unwrap();
                    for reuse in [false, true] {
                        eprintln!(
                            "{form}, orientation {orientation}, {encoder:?}, square={square}, reuse={reuse}"
                        );
                        if reuse {
                            crate::encode_reusing_sdr(
                                &working,
                                encoder,
                                opts,
                                output.path(),
                                &cache,
                            )
                            .unwrap();
                        } else {
                            crate::encode(&working, encoder, opts, output.path()).unwrap();
                        }
                        let id = crate::identify(output.path(), &Limits::default()).unwrap();
                        assert_eq!(id.orientation, 1);
                        let decoded =
                            crate::decode(output.path(), &id, &Limits::default()).unwrap();
                        assert_markers(&decoded, orientation);
                        let sdr_id = crate::identify(sdr.path(), &Limits::default()).unwrap();
                        let sdr = crate::decode(sdr.path(), &sdr_id, &Limits::default()).unwrap();
                        assert_eq!(decoded.image.pixels().unwrap(), sdr.image.pixels().unwrap());
                    }
                }
            }
        }
    }
}
