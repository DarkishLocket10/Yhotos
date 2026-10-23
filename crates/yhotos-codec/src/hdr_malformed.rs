//! Every deterministic mutation must preserve SDR through the public codec API.
#![allow(clippy::unwrap_used, clippy::panic)]
use crate::{Encode, EncodeOpts, Limits, ResizeSpec, gain_mutations, native};
use std::{collections::BTreeMap, path::Path};

fn base_pixels(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name);
    let id = crate::identify(&path, &Limits::default()).unwrap();
    crate::from_image(native::Image::load(&path, &id.kind).unwrap())
        .unwrap()
        .image
        .pixels()
        .unwrap()
}

#[test]
fn all_optional_gain_map_corruptions_preserve_sdr() {
    let limits = Limits::default();
    let input = tempfile::NamedTempFile::new().unwrap();
    let output = tempfile::NamedTempFile::new().unwrap();
    let mut failures = Vec::new();
    let cases = gain_mutations::cases();
    let references: BTreeMap<_, _> = [
        ("jpeg", "hdr/iso.jpg"),
        ("jpeg-xmp", "hdr/adobe.jpg"),
        ("heic", "hdr/apple.heic"),
        ("avif", "images/gain-map.avif"),
        ("jxl", "hdr/gain-map.jxl"),
    ]
    .map(|(key, path)| (key, base_pixels(path)))
    .into();
    for case in &cases {
        std::fs::write(input.path(), &case.bytes).unwrap();
        let before = native::GAIN_PIXEL_DECODES.get();
        let result = (|| -> Result<(), String> {
            let id =
                crate::identify(input.path(), &limits).map_err(|e| format!("identify: {e}"))?;
            let working =
                crate::decode(input.path(), &id, &limits).map_err(|e| format!("decode: {e}"))?;
            if case.invalid_geometry && native::GAIN_PIXEL_DECODES.get() != before {
                return Err("auxiliary pixel decoder called for invalid geometry".into());
            }
            if working.has_gain_map() || working.hdr_skipped_reason().is_none() {
                return Err(format!(
                    "HDR retained or reason missing: {:?}",
                    working.hdr_skipped_reason()
                ));
            }
            let key = ["jpeg-xmp", "jpeg", "heic", "avif", "jxl"]
                .into_iter()
                .find(|key| case.name.starts_with(key))
                .unwrap();
            if working
                .image
                .pixels()
                .map_err(|e| e.to_string())?
                .as_slice()
                != references.get(key).unwrap().as_slice()
            {
                return Err("SDR pixels changed".into());
            }
            let resized = crate::resize(
                &working,
                ResizeSpec {
                    long_edge: 24,
                    square: false,
                },
            )
            .map_err(|e| format!("resize: {e}"))?;
            if resized.hdr_skipped_reason() != working.hdr_skipped_reason() {
                return Err("resize lost the fallback reason".into());
            }
            crate::encode(
                &resized,
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
            .map_err(|e| format!("SDR encode: {e}"))?;
            let sdr = crate::identify(output.path(), &limits)
                .map_err(|e| format!("SDR identify: {e}"))?;
            if sdr.has_gain_map {
                return Err("SDR output carries gain metadata".into());
            }
            Ok(())
        })();
        if let Err(error) = result {
            failures.push(format!("{}: {error}", case.name));
        }
    }
    assert!(
        failures.is_empty(),
        "{} failures / {} mutations:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
    eprintln!("verified {} gain-map mutations", cases.len());
}

#[test]
fn primary_heif_projection_preserves_color_and_alpha_and_rejects_required_damage() {
    let output = tempfile::NamedTempFile::new().unwrap();
    for name in ["chart.avif", "chart.heic", "alpha-10.avif", "alpha-12.heic"] {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/images")
            .join(name);
        let bytes = std::fs::read(&path).unwrap();
        let projected =
            crate::gain_container::heif_sdr(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        std::fs::write(output.path(), &projected).unwrap();
        let id = crate::identify(output.path(), &Limits::default()).unwrap();
        let working = crate::decode(output.path(), &id, &Limits::default()).unwrap();
        assert_eq!(
            working.image.pixels().unwrap(),
            base_pixels(&format!("images/{name}")),
            "{name}"
        );
        for kind in [b"pitm", b"iloc", b"iprp"] {
            let mut missing = bytes.clone();
            let at = missing.windows(4).position(|b| b == kind).unwrap();
            missing
                .get_mut(at..at + 4)
                .unwrap()
                .copy_from_slice(b"free");
            assert!(
                crate::gain_container::heif_sdr(&missing).is_err(),
                "{name}: {kind:?}"
            );
        }
    }
}
