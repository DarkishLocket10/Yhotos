//! Deterministic dual-metadata MPF layout; compressed base scan bytes are copied unchanged.
#![allow(
    clippy::indexing_slicing,
    clippy::float_cmp,
    clippy::cast_possible_truncation
)] // Marker extents and fixed MP entry offsets are checked.
use crate::{CodecError, hdr::Metadata};
use std::fmt::Write;
const ISO: &[u8] = b"urn:iso:std:iso:ts:21496:-1\0";
const XMP: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
fn segments(bytes: &[u8]) -> Result<Vec<(usize, usize, u8)>, CodecError> {
    if !bytes.starts_with(b"\xff\xd8") {
        return Err(CodecError::Decode);
    }
    let mut at = 2;
    let mut result = Vec::new();
    while at < bytes.len().min(16 << 20) {
        while bytes.get(at..at + 2) == Some(&[255, 255]) {
            at += 1;
            if at >= 16 << 20 {
                return Err(CodecError::Limits);
            }
        }
        let h = bytes.get(at..at + 2).ok_or(CodecError::Decode)?;
        if h[0] != 255 {
            return Err(CodecError::Decode);
        }
        if h[1] == 0xda || h[1] == 0xd9 {
            return Ok(result);
        }
        if h[1] == 1 || (0xd0..=0xd7).contains(&h[1]) {
            at += 2;
            continue;
        }
        let n = bytes.get(at + 2..at + 4).ok_or(CodecError::Decode)?;
        let n = usize::from(u16::from_be_bytes([n[0], n[1]]));
        let end = at
            .checked_add(n + 2)
            .filter(|end| *end <= bytes.len() && n >= 2)
            .ok_or(CodecError::Decode)?;
        if result.len() >= 4096 {
            return Err(CodecError::Limits);
        }
        result.push((at, end, h[1]));
        at = end;
    }
    Err(CodecError::Limits)
}
fn strip(bytes: &[u8]) -> Result<Vec<u8>, CodecError> {
    let mut result = Vec::new();
    let mut from = 0;
    for (start, end, marker) in segments(bytes)? {
        let payload = &bytes[start + 4..end];
        if marker == 0xe1 && payload.starts_with(XMP)
            || marker == 0xe2 && (payload.starts_with(b"MPF\0") || payload.starts_with(ISO))
        {
            result.extend_from_slice(&bytes[from..start]);
            from = end;
        }
    }
    result.extend_from_slice(&bytes[from..]);
    Ok(result)
}
fn jfif_end(bytes: &[u8]) -> Result<usize, CodecError> {
    Ok(segments(bytes)?
        .first()
        .filter(|&&(start, end, marker)| {
            marker == 0xe0 && bytes[start + 4..end].starts_with(b"JFIF\0")
        })
        .map_or(2, |&(_, end, _)| end))
}
fn segment(marker: u8, prefix: &[u8], data: &[u8]) -> Result<Vec<u8>, CodecError> {
    let size = u16::try_from(prefix.len() + data.len() + 2).map_err(|_| CodecError::Limits)?;
    let mut result = vec![255, marker];
    result.extend_from_slice(&size.to_be_bytes());
    result.extend_from_slice(prefix);
    result.extend_from_slice(data);
    Ok(result)
}
fn xml(body: &str) -> String {
    format!(
        "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:Description xmlns:hdrgm=\"http://ns.adobe.com/hdr-gain-map/1.0/\" hdrgm:Version=\"1.0\" {body}</rdf:Description></rdf:RDF></x:xmpmeta>"
    )
}
fn property(name: &str, values: [f32; 3], attributes: &mut String, properties: &mut String) {
    if values[0] == values[1] && values[1] == values[2] {
        // The reference XMP reader recognizes scalar coefficients only as Description attributes.
        let _ = write!(attributes, "hdrgm:{name}=\"{}\" ", values[0]);
    } else {
        let _ = write!(
            properties,
            "<hdrgm:{name}><rdf:Seq>{}</rdf:Seq></hdrgm:{name}>",
            format_args!(
                "<rdf:li>{}</rdf:li><rdf:li>{}</rdf:li><rdf:li>{}</rdf:li>",
                values[0], values[1], values[2]
            )
        );
    }
}

#[allow(clippy::too_many_lines)] // Bounded XML state and typed field conversion stay together.
fn adobe(bytes: &[u8]) -> Result<Option<Metadata>, CodecError> {
    use quick_xml::{
        NsReader,
        events::Event,
        name::{Namespace, ResolveResult},
    };
    use std::collections::BTreeMap;
    if bytes.len() > 64 << 10 {
        return Err(CodecError::Limits);
    }
    let mut reader = NsReader::from_reader(bytes);
    let mut fields: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut active: Option<(String, usize)> = None;
    let mut depth = 0_usize;
    let namespace = ResolveResult::Bound(Namespace("http://ns.adobe.com/hdr-gain-map/1.0/"));
    for _ in 0..4096 {
        let event = reader.read_event().map_err(|_| CodecError::Decode)?;
        let start = matches!(&event, Event::Start(_));
        match event {
            Event::Start(e) | Event::Empty(e) => {
                if start {
                    depth += 1;
                }
                if depth > 32 {
                    return Err(CodecError::Limits);
                }
                let (ns, name) = reader.resolver().resolve_element(e.name());
                if active.is_some()
                    && ns != namespace
                    && !(ns
                        == ResolveResult::Bound(Namespace(
                            "http://www.w3.org/1999/02/22-rdf-syntax-ns#",
                        ))
                        && ["Seq", "li"].contains(&name.as_ref())
                        && start)
                {
                    return Err(CodecError::Unsupported);
                }
                if ns == namespace {
                    if active.is_some()
                        || fields
                            .insert(name.as_ref().to_owned(), Vec::new())
                            .is_some()
                    {
                        return Err(CodecError::Decode);
                    }
                    active = Some((name.as_ref().to_owned(), depth));
                }
                for attribute in e.attributes() {
                    let a = attribute.map_err(|_| CodecError::Decode)?;
                    let (ns, name) = reader.resolver().resolve_attribute(a.key);
                    if ns == namespace {
                        if a.value.len() > 256 {
                            return Err(CodecError::Limits);
                        }
                        let values = a
                            .value
                            .split_ascii_whitespace()
                            .map(str::to_owned)
                            .collect::<Vec<_>>();
                        if fields.insert(name.as_ref().to_owned(), values).is_some() {
                            return Err(CodecError::Decode);
                        }
                    }
                }
                if !start {
                    active = None;
                }
            }
            Event::Text(text) => {
                if let Some((name, _)) = &active {
                    let value: &str = text.as_ref();
                    let values = fields.get_mut(name).ok_or(CodecError::Decode)?;
                    values.extend(value.split_ascii_whitespace().map(str::to_owned));
                    if values.len() > 3 || values.iter().any(|v| v.len() > 64) {
                        return Err(CodecError::Limits);
                    }
                }
            }
            Event::End(_) => {
                if active.as_ref().is_some_and(|(_, d)| *d == depth) {
                    active = None;
                }
                depth = depth.checked_sub(1).ok_or(CodecError::Decode)?;
            }
            Event::DocType(_) | Event::GeneralRef(_) | Event::CData(_) => {
                return Err(CodecError::Decode);
            }
            Event::Eof => {
                if depth != 0 {
                    return Err(CodecError::Decode);
                }
                if fields.iter().any(|(name, values)| {
                    ![
                        "Version",
                        "BaseRenditionIsHDR",
                        "GainMapMin",
                        "GainMapMax",
                        "Gamma",
                        "OffsetSDR",
                        "OffsetHDR",
                        "HDRCapacityMin",
                        "HDRCapacityMax",
                    ]
                    .contains(&name.as_str())
                        || ([
                            "Version",
                            "BaseRenditionIsHDR",
                            "HDRCapacityMin",
                            "HDRCapacityMax",
                        ]
                        .contains(&name.as_str())
                            && values.len() != 1)
                }) {
                    return Err(CodecError::Unsupported);
                }
                if fields
                    .get("Version")
                    .is_some_and(|v| v.as_slice() != ["1.0"])
                {
                    return Err(CodecError::Unsupported);
                }
                if !fields.contains_key("GainMapMax") {
                    return Ok(None);
                }
                if fields.get("Version").map(Vec::as_slice) != Some(&["1.0".to_owned()]) {
                    return Err(CodecError::Unsupported);
                }
                if fields
                    .get("BaseRenditionIsHDR")
                    .is_some_and(|v| v.as_slice() != ["False"])
                {
                    return Err(CodecError::Unsupported);
                }
                let number = |name: &str, default: Option<f32>| -> Result<[f32; 3], CodecError> {
                    let Some(values) = fields.get(name) else {
                        return default.map(|v| [v; 3]).ok_or(CodecError::Decode);
                    };
                    let values = values
                        .iter()
                        .map(|v| v.parse::<f32>().map_err(|_| CodecError::Decode))
                        .collect::<Result<Vec<_>, _>>()?;
                    match values.as_slice() {
                        [v] => Ok([*v; 3]),
                        [a, b, c] => Ok([*a, *b, *c]),
                        _ => Err(CodecError::Decode),
                    }
                };
                let m = Metadata {
                    min: number("GainMapMin", Some(0.0))?,
                    max: number("GainMapMax", None)?,
                    gamma: number("Gamma", Some(1.0))?,
                    base_offset: number("OffsetSDR", Some(1.0 / 64.0))?,
                    alternate_offset: number("OffsetHDR", Some(1.0 / 64.0))?,
                    base_headroom: number("HDRCapacityMin", Some(0.0))?[0],
                    alternate_headroom: number("HDRCapacityMax", None)?[0],
                    use_base: true,
                };
                m.validate()?;
                return Ok(Some(m));
            }
            _ => {}
        }
    }
    Err(CodecError::Limits)
}
fn consistent(a: &Metadata, b: &Metadata) -> bool {
    let fields = |m: &Metadata| {
        [
            m.min,
            m.max,
            m.gamma,
            m.base_offset,
            m.alternate_offset,
            [m.base_headroom, m.alternate_headroom, 0.0],
        ]
    };
    a.use_base == b.use_base
        && fields(a)
            .iter()
            .flatten()
            .zip(fields(b).iter().flatten())
            .all(|(a, b)| (a - b).abs() < 0.0001)
}
pub(crate) struct JpegGain<'a> {
    pub image: &'a [u8],
    pub metadata: Metadata,
}
pub(crate) fn extract(stream: &[u8]) -> Result<JpegGain<'_>, CodecError> {
    let mut images = None;
    for (start, end, marker) in segments(stream)? {
        let data = &stream[start + 4..end];
        if marker == 0xe2 && data.starts_with(b"MPF\0") {
            if images.is_some() {
                return Err(CodecError::Decode);
            }
            images = Some(crate::mpf::images(
                &data[4..],
                (start + 8) as u64,
                stream.len() as u64,
            )?);
        }
        if marker == 0xe2
            && let Some(version) = data.strip_prefix(ISO)
            && version != [0, 0, 0, 0]
        {
            return Err(CodecError::Unsupported);
        }
    }
    let mut found = None;
    for (start, end) in images.ok_or(CodecError::Decode)?.into_iter().skip(1) {
        let image = stream
            .get(start as usize..end as usize)
            .ok_or(CodecError::Decode)?;
        let mut iso = None;
        let mut xmp = None;
        for (start, end, marker) in segments(image)? {
            let data = &image[start + 4..end];
            if marker == 0xe2
                && let Some(data) = data.strip_prefix(ISO)
                && iso.replace(crate::gain_container::iso(data)?).is_some()
            {
                return Err(CodecError::Decode);
            }
            if marker == 0xe1
                && let Some(data) = data.strip_prefix(XMP)
                && let Some(m) = adobe(data)?
                && xmp.replace(m).is_some()
            {
                return Err(CodecError::Decode);
            }
        }
        if let (Some(a), Some(b)) = (&iso, &xmp)
            && !consistent(a, b)
        {
            return Err(CodecError::Decode);
        }
        if let Some(metadata) = iso.or(xmp)
            && found.replace(JpegGain { image, metadata }).is_some()
        {
            return Err(CodecError::Decode);
        }
    }
    found.ok_or(CodecError::Unsupported)
}
pub(crate) fn dual(stream: &[u8], metadata: &Metadata) -> Result<Vec<u8>, CodecError> {
    metadata.validate()?;
    let mut images = None;
    for (start, end, marker) in segments(stream)? {
        let payload = &stream[start + 4..end];
        if marker == 0xe2 && payload.starts_with(b"MPF\0") {
            if images.is_some() {
                return Err(CodecError::Decode);
            }
            images = Some(crate::mpf::images(
                &payload[4..],
                (start + 8) as u64,
                stream.len() as u64,
            )?);
        }
    }
    let images = images.ok_or(CodecError::Decode)?;
    if images.len() != 2 || images[0].0 != 0 || images[0].1 != images[1].0 {
        return Err(CodecError::Decode);
    }
    let base = strip(
        stream
            .get(..images[0].1 as usize)
            .ok_or(CodecError::Decode)?,
    )?;
    let gain = strip(
        stream
            .get(images[1].0 as usize..images[1].1 as usize)
            .ok_or(CodecError::Decode)?,
    )?;
    let mut attributes = String::new();
    let mut properties = String::new();
    for (name, values) in [
        ("GainMapMin", metadata.min),
        ("GainMapMax", metadata.max),
        ("Gamma", metadata.gamma),
        ("OffsetSDR", metadata.base_offset),
        ("OffsetHDR", metadata.alternate_offset),
    ] {
        property(name, values, &mut attributes, &mut properties);
    }
    let aux_document = xml(&format!(
        "{attributes}hdrgm:HDRCapacityMin=\"{}\" hdrgm:HDRCapacityMax=\"{}\" hdrgm:BaseRenditionIsHDR=\"False\">{properties}",
        metadata.base_headroom, metadata.alternate_headroom
    ));
    let aux_xmp = segment(0xe1, XMP, aux_document.as_bytes())?;
    let aux_iso = segment(0xe2, ISO, &crate::gain_container::iso_write(metadata)?)?;
    let gain_size = gain.len() + aux_xmp.len() + aux_iso.len();
    let primary_document = xml(&format!(
        "xmlns:Container=\"http://ns.google.com/photos/1.0/container/\" xmlns:Item=\"http://ns.google.com/photos/1.0/container/item/\"><Container:Directory><rdf:Seq><rdf:li rdf:parseType=\"Resource\"><Container:Item Item:Semantic=\"Primary\" Item:Mime=\"image/jpeg\"/></rdf:li><rdf:li rdf:parseType=\"Resource\"><Container:Item Item:Semantic=\"GainMap\" Item:Mime=\"image/jpeg\" Item:Length=\"{gain_size}\"/></rdf:li></rdf:Seq></Container:Directory>"
    ));
    let primary_xmp = segment(0xe1, XMP, primary_document.as_bytes())?;
    let primary_iso = segment(0xe2, ISO, &[0, 0, 0, 0])?;
    let primary_prefix = jfif_end(&base)?;
    let auxiliary_prefix = jfif_end(&gain)?;
    let tiff_position = primary_prefix + primary_xmp.len() + primary_iso.len() + 8;
    let primary_size = base.len() + primary_xmp.len() + primary_iso.len() + 90;
    let mut tiff = b"MM\0*\0\0\0\x08\0\x03".to_vec();
    // MPFVersion, NumberOfImages, MPEntry; entry array starts at TIFF offset 50.
    tiff.extend_from_slice(&[0xb0, 0, 0, 7, 0, 0, 0, 4, b'0', b'1', b'0', b'0']);
    tiff.extend_from_slice(&[0xb0, 1, 0, 4, 0, 0, 0, 1, 0, 0, 0, 2]);
    tiff.extend_from_slice(&[0xb0, 2, 0, 7, 0, 0, 0, 32, 0, 0, 0, 50, 0, 0, 0, 0]);
    for (attributes, size, offset) in [
        (0x2003_0000_u32, primary_size, 0),
        (0, gain_size, primary_size - tiff_position),
    ] {
        tiff.extend_from_slice(&attributes.to_be_bytes());
        tiff.extend_from_slice(
            &u32::try_from(size)
                .map_err(|_| CodecError::Limits)?
                .to_be_bytes(),
        );
        tiff.extend_from_slice(
            &u32::try_from(offset)
                .map_err(|_| CodecError::Limits)?
                .to_be_bytes(),
        );
        tiff.extend_from_slice(&[0; 4]);
    }
    let mpf = segment(0xe2, b"MPF\0", &tiff)?;
    let mut result = Vec::with_capacity(primary_size + gain_size);
    for bytes in [
        &base[..primary_prefix],
        &primary_xmp,
        &primary_iso,
        &mpf,
        &base[primary_prefix..],
        &gain[..auxiliary_prefix],
        &aux_xmp,
        &aux_iso,
        &gain[auxiliary_prefix..],
    ] {
        result.extend_from_slice(bytes);
    }
    Ok(result)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn jpeg_metadata_scan_accepts_fill_and_standalone_markers() {
        assert_eq!(
            segments(b"\xff\xd8\xff\xff\xe1\0\x04xx\xff\xda").unwrap(),
            vec![(3, 9, 0xe1)]
        );
        assert_eq!(
            segments(b"\xff\xd8\xff\x01\xff\xe1\0\x04xx\xff\xda").unwrap(),
            vec![(4, 10, 0xe1)]
        );
        assert_eq!(
            jfif_end(b"\xff\xd8\xff\xe0\0\x07JFIF\0\xff\xda").unwrap(),
            11
        );
        assert_eq!(jfif_end(b"\xff\xd8\xff\xe1\0\x04xx\xff\xda").unwrap(), 2);
    }
    #[test]
    fn adobe_attributes_and_rgb_sequences_are_typed_and_bounded() {
        let mut attributes = String::new();
        let mut properties = String::new();
        for (name, values) in [
            ("GainMapMin", [-1.0, 0.0, 1.0]),
            ("GainMapMax", [1.0, 2.0, 3.0]),
            ("Gamma", [0.5, 1.0, 2.0]),
            ("OffsetSDR", [0.01; 3]),
            ("OffsetHDR", [0.02; 3]),
        ] {
            property(name, values, &mut attributes, &mut properties);
        }
        assert_eq!(
            attributes,
            "hdrgm:OffsetSDR=\"0.01\" hdrgm:OffsetHDR=\"0.02\" "
        );
        let document = xml(&format!(
            "{attributes}hdrgm:HDRCapacityMin=\"0\" hdrgm:HDRCapacityMax=\"3\">{properties}"
        ));
        let metadata = adobe(document.as_bytes()).unwrap().unwrap();
        assert!(
            metadata
                .min
                .iter()
                .zip([-1.0, 0.0, 1.0])
                .all(|(a, b)| (a - b).abs() < 0.0001)
        );
        assert!(
            metadata
                .gamma
                .iter()
                .zip([0.5, 1.0, 2.0])
                .all(|(a, b)| (a - b).abs() < 0.0001)
        );
        assert!(
            metadata
                .base_offset
                .iter()
                .all(|v| (v - 0.01).abs() < 0.0001)
        );
        assert!(
            metadata
                .alternate_offset
                .iter()
                .all(|v| (v - 0.02).abs() < 0.0001)
        );
        assert!((metadata.alternate_headroom - 3.0).abs() < 0.0001);
        for (from, to) in [
            ("<rdf:li>0.5</rdf:li>", "<rdf:li>NaN</rdf:li>"),
            ("hdrgm:HDRCapacityMax=\"3\"", "hdrgm:HDRCapacityMax=\"0\""),
            ("hdrgm:Version=\"1.0\"", "hdrgm:Version=\"2.0\""),
            ("<rdf:Seq>", "<unknown>"),
            (
                "hdrgm:HDRCapacityMax=\"3\"",
                "hdrgm:HDRCapacityMax=\"3 4 5\"",
            ),
        ] {
            assert!(
                adobe(document.replace(from, to).as_bytes()).is_err(),
                "{from}"
            );
        }
        assert!(adobe(b"<!DOCTYPE root><root/>").is_err());
        assert!(adobe(&vec![0; 65537]).is_err());
    }
    #[test]
    fn jpeg_iso_and_adobe_models_must_agree() {
        let fixture =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/hdr/iso.jpg");
        let original = std::fs::read(fixture).unwrap();
        let gain = extract(&original).unwrap();
        let mut base = &original[..];
        for (start, end, marker) in segments(&original).unwrap() {
            if marker == 0xe2 && original[start + 4..end].starts_with(b"MPF\0") {
                let images = crate::mpf::images(
                    &original[start + 8..end],
                    (start + 8) as u64,
                    original.len() as u64,
                )
                .unwrap();
                base = &original[..usize::try_from(images[0].1).unwrap()];
            }
        }
        let encoded = crate::hdr::mux(base, gain.image, &gain.metadata).unwrap();
        assert!(extract(&encoded).is_ok());
        let mut bad = encoded;
        let text = b"hdrgm:GainMapMax=\"2\"";
        let at = bad.windows(text.len()).position(|b| b == text).unwrap();
        bad[at + b"hdrgm:GainMapMax=\"".len()] = b'3';
        assert!(extract(&bad).is_err());
    }
    #[test]
    fn jpeg_gain_map_follows_every_base_exif_orientation() {
        let fixture =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/hdr/iso.jpg");
        let original = std::fs::read(fixture).unwrap();
        let gain = extract(&original).unwrap();
        let images = segments(&original)
            .unwrap()
            .into_iter()
            .find_map(|(start, end, marker)| {
                (marker == 0xe2 && original[start + 4..end].starts_with(b"MPF\0")).then(|| {
                    crate::mpf::images(
                        &original[start + 8..end],
                        (start + 8) as u64,
                        original.len() as u64,
                    )
                    .unwrap()
                })
            })
            .unwrap();
        let stream = crate::hdr::mux(
            &original[..images[0].1 as usize],
            gain.image,
            &gain.metadata,
        )
        .unwrap();
        let tiff = segments(&stream)
            .unwrap()
            .into_iter()
            .find_map(|(start, end, marker)| {
                (marker == 0xe2 && stream[start + 4..end].starts_with(b"MPF\0"))
                    .then_some(start + 8)
            })
            .unwrap();
        // Our dual writer has a big-endian TIFF and MP entries at offset 50.
        assert_eq!(&stream[tiff..tiff + 4], b"MM\0*");
        for orientation in 1_u8..=8 {
            let mut exif = b"Exif\0\0II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0".to_vec();
            exif.extend([orientation, 0, 0, 0, 0, 0, 0, 0]);
            let exif = segment(0xe1, &[], &exif).unwrap();
            let mut input = stream.clone();
            let size = u32::from_be_bytes(input[tiff + 54..tiff + 58].try_into().unwrap());
            input[tiff + 54..tiff + 58].copy_from_slice(&(size + exif.len() as u32).to_be_bytes());
            input.splice(2..2, exif);
            let file = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(file.path(), input).unwrap();
            let id = crate::identify(file.path(), &crate::Limits::default()).unwrap();
            assert_eq!(id.orientation, orientation);
            let working = crate::decode(file.path(), &id, &crate::Limits::default()).unwrap();
            assert!(
                working.has_gain_map(),
                "orientation {orientation}: {:?}",
                working.hdr_skipped_reason()
            );
            let resized = crate::resize(
                &working,
                crate::ResizeSpec {
                    long_edge: 48,
                    square: false,
                },
            )
            .unwrap();
            let (w, h) = resized.dimensions().unwrap();
            let (gw, gh) = resized.gain_map.as_ref().unwrap().image.size().unwrap();
            assert_eq!((gw * 2, gh * 2), (w, h), "orientation {orientation}");
            assert_eq!((w, h), if orientation >= 5 { (32, 48) } else { (48, 32) });
        }
    }
}
