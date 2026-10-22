#![no_main]
#![forbid(unsafe_code)]
use libfuzzer_sys::fuzz_target;
use std::io::Write;
// Run the CR3 allocation, progress and version regressions with RAW enabled.
static REGRESSION: std::sync::Once = std::sync::Once::new();
fuzz_target!(|data: &[u8]| {
    REGRESSION.call_once(|| {
        let Ok(mut file) = tempfile::NamedTempFile::new() else {
            return;
        };
        let bytes = b"\0\0\0\x14ftypcrx \0\0\0\0crx \0\0\0\x10moov\0\0\0\0free\0\0\0\x08mdat";
        if file.write_all(bytes).is_err() {
            return;
        }
        assert_eq!(
            yhotos_codec::identify(file.path(), &yhotos_codec::Limits::default()),
            Err(yhotos_codec::CodecError::Decode)
        );
        let mut stsz = vec![0; 8];
        stsz.extend(u32::MAX.to_be_bytes());
        let mut uuid = vec![0x42; 16];
        uuid.extend(b"\0\0\0\0free");
        let mut mvhd = vec![0; 100];
        mvhd[0] = 2;
        for payload in [
            box_bytes(b"stsz", &stsz),
            box_bytes(b"uuid", &uuid),
            box_bytes(b"mvhd", &mvhd),
        ] {
            if std::fs::write(file.path(), cr3(&payload)).is_err() {
                return;
            }
            assert_eq!(
                yhotos_codec::identify(file.path(), &yhotos_codec::Limits::default()),
                Err(yhotos_codec::CodecError::Unsupported)
            );
        }
    });
    let limits = yhotos_codec::Limits {
        max_pixels: 1_000_000,
        max_bytes: 1 << 20,
        max_dim: 4096,
    };
    if data.len() > limits.max_bytes as usize {
        return;
    }
    let Ok(mut file) = tempfile::NamedTempFile::new() else {
        return;
    };
    if file.write_all(data).is_err() {
        return;
    }
    check(file.path(), &limits);
    // Mutate vendor MakerNotes behind valid TIFF dimensions and Make, so the
    // shared entry, extent and payload budgets cannot hide behind bad magic.
    let nested = makernote(data);
    if nested.len() <= limits.max_bytes as usize && std::fs::write(file.path(), nested).is_ok() {
        check(file.path(), &limits);
    }
    // Exercise HEIF metadata boxes before the native header loader, including
    // iinf/infe, iref/dimg and iprp/ipco property extents and counts.
    let mut nested = box_bytes(b"ftyp", b"avif\0\0\0\0avifmif1");
    let mut meta = vec![0; 4];
    meta.extend(data);
    nested.extend(box_bytes(b"meta", &meta));
    if nested.len() <= limits.max_bytes as usize && std::fs::write(file.path(), nested).is_ok() {
        check(file.path(), &limits);
    }
    // Mutate nested box payloads inside a valid CR3 envelope as well.
    let nested = cr3(data);
    if nested.len() <= limits.max_bytes as usize && std::fs::write(file.path(), nested).is_ok() {
        check(file.path(), &limits);
    }
    // Exercise the JPEG prefix reader inside Canon's preview UUID directly.
    let mut record = vec![0; 16];
    record[6..8].copy_from_slice(&1u16.to_be_bytes());
    record[8..10].copy_from_slice(&1u16.to_be_bytes());
    record[12..16].copy_from_slice(&(data.len() as u32).to_be_bytes());
    record.extend(data);
    let mut uuid = vec![
        0xea, 0xf4, 0x2b, 0x5e, 0x1c, 0x98, 0x4b, 0x88, 0xb9, 0xfb, 0xb7, 0xdc, 0x40, 0x6e, 0x4d,
        0x16,
    ];
    uuid.extend([0, 0, 0, 0, 0, 0, 0, 1]);
    uuid.extend(box_bytes(b"PRVW", &record));
    let mut nested = cr3(&[]);
    nested.extend(box_bytes(b"uuid", &uuid));
    if nested.len() <= limits.max_bytes as usize && std::fs::write(file.path(), nested).is_ok() {
        check(file.path(), &limits);
    }
});

fn box_bytes(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut bytes = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
    bytes.extend(kind);
    bytes.extend(payload);
    bytes
}
fn cr3(moov: &[u8]) -> Vec<u8> {
    let mut bytes = box_bytes(b"ftyp", b"crx \0\0\0\0crx ");
    bytes.extend(box_bytes(b"moov", moov));
    bytes.extend(box_bytes(b"mdat", &[]));
    bytes
}

fn makernote(data: &[u8]) -> Vec<u8> {
    let (make, header): (&[u8], &[u8]) = match data.first().copied().unwrap_or(0) % 7 {
        1 => (b"SONY\0", b""),
        2 => (b"NIKON CORPORATION\0", b"Nikon\0\x02\0\0\0II*\0\x08\0\0\0"),
        3 => (b"Panasonic\0", b"Panasonic\0\0\0"),
        4 => (b"OLYMPUS\0", b"OLYMPUS\0II\x03\0"),
        5 => (b"OLYMPUS\0", b"OLYMP\0\x01\0"),
        6 => (b"FUJIFILM\0", b"FUJIFILM\x0c\0\0\0"),
        _ => (b"Canon\0", b""),
    };
    let mut bytes = b"II*\0\x10\0\0\0CR\x02\0\x10\0\0\0".to_vec();
    bytes.extend(4u16.to_le_bytes());
    for (tag, kind, count, value) in [
        (256u16, 4u16, 1u32, 1u32),
        (257, 4, 1, 1),
        (271, 2, make.len() as u32, 96),
        (37500, 7, (header.len() + data.len()) as u32, 128),
    ] {
        bytes.extend(tag.to_le_bytes());
        bytes.extend(kind.to_le_bytes());
        bytes.extend(count.to_le_bytes());
        bytes.extend(value.to_le_bytes());
    }
    bytes.extend(0u32.to_le_bytes());
    bytes.resize(96, 0);
    bytes.extend(make);
    bytes.resize(128, 0);
    bytes.extend(header);
    bytes.extend(data);
    bytes
}

fn check(path: &std::path::Path, limits: &yhotos_codec::Limits) {
    if let Ok(id) = yhotos_codec::identify(path, limits) {
        if matches!(id.kind, yhotos_codec::Kind::Video { .. }) {
            return;
        }
        assert!(
            id.width.unwrap_or(0) > 0
                && id.height.unwrap_or(0) > 0
                && id.width.unwrap_or(0) <= limits.max_dim
                && id.height.unwrap_or(0) <= limits.max_dim
        );
        assert!(
            u64::from(id.width.unwrap_or(0))
                * u64::from(id.height.unwrap_or(0))
                * u64::from(id.pages)
                <= limits.max_pixels
        );
        assert!((1..=8).contains(&id.orientation));
    }
}
