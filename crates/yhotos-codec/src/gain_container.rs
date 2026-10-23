//! Bounded ISO metadata and HEIF item locations. No native decoder enters this parser.
#![allow(
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)] // Cursor reads and fixed channel indices are bounded.
use crate::{CodecError, Limits, hdr::Metadata};
use std::collections::BTreeMap;

pub(crate) fn jxl_present(path: &std::path::Path) -> Result<bool, CodecError> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).map_err(|_| CodecError::Io)?;
    let len = file.metadata().map_err(|_| CodecError::Io)?.len();
    let mut prefix = [0; 2];
    file.read_exact(&mut prefix)
        .map_err(|_| CodecError::Decode)?;
    if prefix == [255, 10] {
        return Ok(false);
    }
    let mut at = 0_u64;
    for _ in 0..4096 {
        if at == len {
            return Ok(false);
        }
        file.seek(SeekFrom::Start(at)).map_err(|_| CodecError::Io)?;
        let mut header = [0; 8];
        file.read_exact(&mut header)
            .map_err(|_| CodecError::Decode)?;
        let size = u32::from_be_bytes(header[..4].try_into().map_err(|_| CodecError::Decode)?);
        let (size, minimum) = if size == 1 {
            let mut large = [0; 8];
            file.read_exact(&mut large)
                .map_err(|_| CodecError::Decode)?;
            (u64::from_be_bytes(large), 16)
        } else if size == 0 {
            (len - at, 8)
        } else {
            (u64::from(size), 8)
        };
        let end = at
            .checked_add(size)
            .filter(|n| *n <= len && size >= minimum)
            .ok_or(CodecError::Decode)?;
        if &header[4..] == b"jhgm" {
            return Ok(true);
        }
        if &header[4..] == b"brob" {
            // The inner type is uncompressed. Stay inside this box and never
            // decompress untrusted metadata just to detect a gain map.
            if size - minimum < 4 {
                return Err(CodecError::Decode);
            }
            let mut inner = [0; 4];
            file.read_exact(&mut inner)
                .map_err(|_| CodecError::Decode)?;
            if &inner == b"jhgm" {
                return Ok(true);
            }
        }
        at = end;
    }
    Err(CodecError::Limits)
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], CodecError> {
        let end = self.at.checked_add(n).ok_or(CodecError::Decode)?;
        let bytes = self.bytes.get(self.at..end).ok_or(CodecError::Decode)?;
        self.at = end;
        Ok(bytes)
    }
    fn num(&mut self, n: usize) -> Result<u64, CodecError> {
        if n > 8 {
            return Err(CodecError::Decode);
        }
        Ok(self.take(n)?.iter().fold(0, |v, b| v << 8 | u64::from(*b)))
    }
    fn fraction(&mut self, denominator: Option<u64>, signed: bool) -> Result<f32, CodecError> {
        let n = self.num(4)?;
        let n = if signed {
            i64::from(i32::from_be_bytes(
                u32::try_from(n)
                    .map_err(|_| CodecError::Decode)?
                    .to_be_bytes(),
            )) as f64
        } else {
            n as f64
        };
        let d = match denominator {
            Some(d) => d,
            None => self.num(4)?,
        };
        if d == 0 {
            return Err(CodecError::Decode);
        }
        Ok((n / d as f64) as f32)
    }
}
pub(crate) fn iso(bytes: &[u8]) -> Result<Metadata, CodecError> {
    if bytes.len() > 256 {
        return Err(CodecError::Limits);
    }
    let mut c = Cursor::new(bytes);
    if c.num(2)? != 0 || c.num(2)? != 0 {
        return Err(CodecError::Unsupported);
    }
    let flags = c.num(1)?;
    if flags & 0x37 != 0 {
        return Err(CodecError::Unsupported);
    }
    let denominator = if flags & 8 != 0 {
        Some(c.num(4)?)
    } else {
        None
    };
    let mut metadata = Metadata {
        min: [0.0; 3],
        max: [0.0; 3],
        gamma: [0.0; 3],
        base_offset: [0.0; 3],
        alternate_offset: [0.0; 3],
        base_headroom: c.fraction(denominator, false)?,
        alternate_headroom: c.fraction(denominator, false)?,
        use_base: flags & 64 != 0,
    };
    let channels = if flags & 128 != 0 { 3 } else { 1 };
    for channel in 0..channels {
        metadata.min[channel] = c.fraction(denominator, true)?;
        metadata.max[channel] = c.fraction(denominator, true)?;
        metadata.gamma[channel] = c.fraction(denominator, false)?;
        metadata.base_offset[channel] = c.fraction(denominator, true)?;
        metadata.alternate_offset[channel] = c.fraction(denominator, true)?;
    }
    if c.at != bytes.len() {
        return Err(CodecError::Decode);
    }
    if channels == 1 {
        metadata.min = [metadata.min[0]; 3];
        metadata.max = [metadata.max[0]; 3];
        metadata.gamma = [metadata.gamma[0]; 3];
        metadata.base_offset = [metadata.base_offset[0]; 3];
        metadata.alternate_offset = [metadata.alternate_offset[0]; 3];
    }
    metadata.validate()?;
    Ok(metadata)
}

pub(crate) fn iso_write(metadata: &Metadata) -> Result<Vec<u8>, CodecError> {
    metadata.validate()?;
    let single = [
        metadata.min,
        metadata.max,
        metadata.gamma,
        metadata.base_offset,
        metadata.alternate_offset,
    ]
    .iter()
    .all(|values| {
        values[0].to_bits() == values[1].to_bits() && values[1].to_bits() == values[2].to_bits()
    });
    let channels = if single { 1 } else { 3 };
    let mut bytes = vec![
        0,
        0,
        0,
        0,
        8 | if single { 0 } else { 128 } | if metadata.use_base { 64 } else { 0 },
    ];
    bytes.extend_from_slice(&1_000_000_u32.to_be_bytes());
    let mut value = |v: f32| {
        bytes.extend_from_slice(&((f64::from(v) * 1_000_000.0).round() as i32).to_be_bytes());
    };
    value(metadata.base_headroom);
    value(metadata.alternate_headroom);
    for c in 0..channels {
        value(metadata.min[c]);
        value(metadata.max[c]);
        value(metadata.gamma[c]);
        value(metadata.base_offset[c]);
        value(metadata.alternate_offset[c]);
    }
    Ok(bytes)
}

pub(crate) fn jxl(
    bytes: &[u8],
    limits: &Limits,
    base: &crate::Identity,
) -> Result<crate::hdr::GainMap, CodecError> {
    let top = parse_boxes(bytes, &mut 4096, true)?;
    if top
        .iter()
        .any(|b| b.kind == b"brob" && b.data.starts_with(b"jhgm"))
    {
        return Err(CodecError::CompressedJxlGainMap);
    }
    let data = one(&top, b"jhgm")?;
    if data.len() > 16 << 20 {
        return Err(CodecError::Limits);
    }
    let mut c = Cursor::new(data);
    if c.num(1)? != 0 {
        return Err(CodecError::Unsupported);
    }
    let size = usize::try_from(c.num(2)?).map_err(|_| CodecError::Limits)?;
    let metadata = iso(c.take(size)?)?;
    // Alternate profiles require a second color pipeline. Unknown profiles are SDR-only.
    if c.num(1)? != 0 || c.num(4)? != 0 || !metadata.use_base {
        return Err(CodecError::Unsupported);
    }
    let image = crate::native::Image::gain_jxl(&data[c.at..], limits, base)?;
    Ok(crate::hdr::GainMap { image, metadata })
}
pub(crate) fn jxl_mux(
    base: &[u8],
    gain: &[u8],
    metadata: &Metadata,
) -> Result<Vec<u8>, CodecError> {
    if !metadata.use_base {
        return Err(CodecError::Unsupported);
    }
    let metadata = iso_write(metadata)?;
    let gain = if gain.starts_with(b"\xff\x0a") {
        gain
    } else {
        one(&boxes(gain, &mut 4096)?, b"jxlc")?
    };
    if gain.len() > 16 << 20 {
        return Err(CodecError::Limits);
    }
    let mut encoded = vec![0];
    encoded.extend_from_slice(
        &u16::try_from(metadata.len())
            .map_err(|_| CodecError::Limits)?
            .to_be_bytes(),
    );
    encoded.extend_from_slice(&metadata);
    encoded.extend_from_slice(&[0; 5]); // absent alternative color encoding and compressed ICC
    encoded.extend_from_slice(gain);
    let mut result = Vec::new();
    let mut append = |kind: &[u8], data: &[u8]| -> Result<(), CodecError> {
        let size = u32::try_from(data.len() + 8).map_err(|_| CodecError::Limits)?;
        result.extend_from_slice(&size.to_be_bytes());
        result.extend_from_slice(kind);
        result.extend_from_slice(data);
        Ok(())
    };
    if base.starts_with(b"\xff\x0a") {
        append(b"JXL ", b"\r\n\x87\n")?;
        append(b"ftyp", b"jxl \0\0\0\0jxl ")?;
        append(b"jxlc", base)?;
    } else {
        boxes(base, &mut 4096)?;
        result.extend_from_slice(base);
    }
    let size = u32::try_from(encoded.len() + 8).map_err(|_| CodecError::Limits)?;
    result.extend_from_slice(&size.to_be_bytes());
    result.extend_from_slice(b"jhgm");
    result.extend_from_slice(&encoded);
    Ok(result)
}

struct Box<'a> {
    kind: &'a [u8],
    data: &'a [u8],
}
fn boxes<'a>(data: &'a [u8], budget: &mut usize) -> Result<Vec<Box<'a>>, CodecError> {
    parse_boxes(data, budget, false)
}
fn parse_boxes<'a>(
    mut data: &'a [u8],
    budget: &mut usize,
    allow_to_eof: bool,
) -> Result<Vec<Box<'a>>, CodecError> {
    let mut result = Vec::new();
    while !data.is_empty() {
        *budget = budget.checked_sub(1).ok_or(CodecError::Limits)?;
        let mut c = Cursor::new(data);
        let n = c.num(4)?;
        let kind = c.take(4)?;
        let n = if n == 0 && allow_to_eof {
            // JPEG XL permits a final box extending to EOF. HEIF keeps finite extents.
            data.len()
        } else {
            usize::try_from(if n == 1 { c.num(8)? } else { n }).map_err(|_| CodecError::Limits)?
        };
        if n < c.at {
            return Err(CodecError::Decode);
        }
        let payload = data.get(c.at..n).ok_or(CodecError::Decode)?;
        result.push(Box {
            kind,
            data: payload,
        });
        data = data.get(n..).ok_or(CodecError::Decode)?;
    }
    Ok(result)
}
fn one<'a>(items: &[Box<'a>], kind: &[u8]) -> Result<&'a [u8], CodecError> {
    let mut matching = items.iter().filter(|b| b.kind == kind);
    let data = matching.next().ok_or(CodecError::Decode)?.data;
    if matching.next().is_some() {
        return Err(CodecError::Decode);
    }
    Ok(data)
}
type Location = (u64, u64, u64); // construction method, absolute/idat offset, length
fn locations(data: &[u8]) -> Result<BTreeMap<u32, Vec<Location>>, CodecError> {
    let mut c = Cursor::new(data);
    let version = c.num(1)?;
    if version > 2 || c.num(3)? != 0 {
        return Err(CodecError::Unsupported);
    }
    let sizes = c.num(2)?;
    let offset_size = (sizes >> 12) as usize;
    let length_size = ((sizes >> 8) & 15) as usize;
    let base_size = ((sizes >> 4) & 15) as usize;
    let index_size = if version > 0 {
        (sizes & 15) as usize
    } else {
        0
    };
    if [offset_size, length_size, base_size, index_size]
        .iter()
        .any(|n| !matches!(n, 0 | 4 | 8))
    {
        return Err(CodecError::Unsupported);
    }
    let count = c.num(if version == 2 { 4 } else { 2 })?;
    if count > 4096 {
        return Err(CodecError::Limits);
    }
    let mut result = BTreeMap::new();
    let mut budget = 4096_usize;
    for _ in 0..count {
        let id = u32::try_from(c.num(if version == 2 { 4 } else { 2 })?)
            .map_err(|_| CodecError::Decode)?;
        let method = if version > 0 { c.num(2)? } else { 0 };
        if c.num(2)? != 0 || method > 1 {
            return Err(CodecError::Unsupported);
        }
        let base = c.num(base_size)?;
        let count = usize::try_from(c.num(2)?).map_err(|_| CodecError::Limits)?;
        budget = budget.checked_sub(count).ok_or(CodecError::Limits)?;
        let mut extents = Vec::new();
        for _ in 0..count {
            if index_size != 0 && c.num(index_size)? != 0 {
                return Err(CodecError::Unsupported);
            }
            let offset = base
                .checked_add(c.num(offset_size)?)
                .ok_or(CodecError::Decode)?;
            extents.push((method, offset, c.num(length_size)?));
        }
        if id == 0 || result.insert(id, extents).is_some() {
            return Err(CodecError::Decode);
        }
    }
    if c.at != data.len() {
        return Err(CodecError::Decode);
    }
    Ok(result)
}
fn item(
    bytes: &[u8],
    idat: &[u8],
    locations: &BTreeMap<u32, Vec<Location>>,
    id: u32,
) -> Result<Vec<u8>, CodecError> {
    let mut result = Vec::new();
    for &(method, offset, len) in locations.get(&id).ok_or(CodecError::Decode)? {
        let offset = usize::try_from(offset).map_err(|_| CodecError::Limits)?;
        let len = usize::try_from(len).map_err(|_| CodecError::Limits)?;
        if result.len().checked_add(len).is_none_or(|n| n > 1 << 20) {
            return Err(CodecError::Limits);
        }
        let source = if method == 1 { idat } else { bytes };
        result.extend_from_slice(
            source
                .get(offset..offset.checked_add(len).ok_or(CodecError::Decode)?)
                .ok_or(CodecError::Decode)?,
        );
    }
    Ok(result)
}
pub(crate) struct HeifGain {
    pub id: u32,
    pub metadata: Metadata,
    pub apple: bool,
}
#[allow(clippy::too_many_lines)] // Item tables share one bounded namespace and box budget.
pub(crate) fn heif(bytes: &[u8], limits: &Limits) -> Result<HeifGain, CodecError> {
    let mut budget = 4096;
    let top = boxes(bytes, &mut budget)?;
    let meta = one(&top, b"meta")?;
    if meta.len() > 16 << 20 {
        return Err(CodecError::Limits);
    }
    if meta.get(..4) != Some(&[0, 0, 0, 0]) {
        return Err(CodecError::Unsupported);
    }
    let meta = boxes(&meta[4..], &mut budget)?;
    let primary = one(&meta, b"pitm")?;
    let mut c = Cursor::new(primary);
    let version = c.num(1)?;
    if version > 1 || c.num(3)? != 0 {
        return Err(CodecError::Unsupported);
    }
    let primary = c.num(if version == 0 { 2 } else { 4 })? as u32;
    let iinf = one(&meta, b"iinf")?;
    let mut c = Cursor::new(iinf);
    let version = c.num(1)?;
    if version > 1 || c.num(3)? != 0 {
        return Err(CodecError::Unsupported);
    }
    let count = c.num(if version == 0 { 2 } else { 4 })?;
    let infos = boxes(&iinf[c.at..], &mut budget)?;
    if count as usize != infos.len() {
        return Err(CodecError::Decode);
    }
    let mut types = BTreeMap::new();
    for info in infos {
        if info.kind != b"infe" {
            return Err(CodecError::Decode);
        }
        let mut c = Cursor::new(info.data);
        let version = c.num(1)?;
        c.num(3)?;
        if !matches!(version, 2 | 3) {
            return Err(CodecError::Unsupported);
        }
        let id = c.num(if version == 2 { 2 } else { 4 })? as u32;
        if c.num(2)? != 0 {
            return Err(CodecError::Unsupported);
        }
        if types.insert(id, c.take(4)?).is_some() {
            return Err(CodecError::Decode);
        }
    }
    let refs = one(&meta, b"iref")?;
    let mut c = Cursor::new(refs);
    let version = c.num(1)?;
    if version > 1 || c.num(3)? != 0 {
        return Err(CodecError::Unsupported);
    }
    let width = if version == 0 { 2 } else { 4 };
    let mut derived = None;
    let mut aux = Vec::new();
    let mut metadata_owners = BTreeMap::new();
    for reference in boxes(&refs[4..], &mut budget)? {
        let mut c = Cursor::new(reference.data);
        let from = c.num(width)? as u32;
        let count = c.num(2)?;
        if count > 4096 {
            return Err(CodecError::Limits);
        }
        if reference.kind == b"dimg" && types.get(&from) == Some(&b"tmap".as_slice()) && count == 2
        {
            let base = c.num(width)? as u32;
            let gain = c.num(width)? as u32;
            if base != primary
                || gain == base
                || !types.contains_key(&gain)
                || derived.replace((from, gain)).is_some()
            {
                return Err(CodecError::Decode);
            }
        } else if reference.kind == b"auxl" && count == 1 && c.num(width)? == u64::from(primary) {
            aux.push(from);
        } else if reference.kind == b"cdsc"
            && count == 1
            && metadata_owners.insert(from, c.num(width)? as u32).is_some()
        {
            return Err(CodecError::Decode);
        }
    }
    let locations = locations(one(&meta, b"iloc")?)?;
    let idat = meta
        .iter()
        .find(|b| b.kind == b"idat")
        .map_or(&[][..], |b| b.data);
    if let Some((tmap, id)) = derived {
        let data = item(bytes, idat, &locations, tmap)?;
        // ToneMapImage carries a one-byte version before ISO gain-map metadata.
        if data.first() != Some(&0) {
            return Err(CodecError::Unsupported);
        }
        return Ok(HeifGain {
            id,
            metadata: iso(&data[1..])?,
            apple: false,
        });
    }
    let iprp = boxes(one(&meta, b"iprp")?, &mut budget)?;
    let properties = boxes(one(&iprp, b"ipco")?, &mut budget)?;
    let ipma = one(&iprp, b"ipma")?;
    let mut c = Cursor::new(ipma);
    let version = c.num(1)?;
    let flags = c.num(3)?;
    if version > 1 || flags > 1 {
        return Err(CodecError::Unsupported);
    }
    let count = c.num(4)?;
    if count > 4096 {
        return Err(CodecError::Limits);
    }
    let mut apple = None;
    for _ in 0..count {
        let id = c.num(if version == 0 { 2 } else { 4 })? as u32;
        let count = c.num(1)?;
        for _ in 0..count {
            let index = c.num(if flags == 0 { 1 } else { 2 })? as usize
                & if flags == 0 { 127 } else { 32767 };
            if index == 0 {
                continue;
            }
            let p = properties.get(index - 1).ok_or(CodecError::Decode)?;
            if aux.contains(&id)
                && p.kind == b"auxC"
                && p.data.get(4..) == Some(b"urn:com:apple:photo:2020:aux:hdrgainmap\0")
                && apple.replace(id).is_some()
            {
                return Err(CodecError::Decode);
            }
        }
    }
    let gain_id = apple.ok_or(CodecError::Unsupported)?;
    let mut exif = None;
    let mut version = false;
    for (&id, &kind) in &types {
        if kind == b"Exif"
            && metadata_owners.get(&id) == Some(&primary)
            && exif.replace(item(bytes, idat, &locations, id)?).is_some()
        {
            return Err(CodecError::Decode);
        }
        if kind == b"mime"
            && metadata_owners
                .get(&id)
                .is_some_and(|owner| *owner == primary || *owner == gain_id)
        {
            version |= apple_version(&item(bytes, idat, &locations, id)?)?;
        }
    }
    if !version {
        return Err(CodecError::Decode);
    }
    let exif = exif.ok_or(CodecError::Decode)?;
    let mut c = Cursor::new(&exif);
    let at = usize::try_from(c.num(4)?)
        .map_err(|_| CodecError::Limits)?
        .checked_add(4)
        .ok_or(CodecError::Decode)?;
    let tiff = exif.get(at..).ok_or(CodecError::Decode)?;
    let mut tiff =
        crate::tiff::Tiff::from_file(std::io::Cursor::new(tiff), tiff.len() as u64, limits)?;
    let mut note = None;
    for index in 0..tiff.count() {
        if let Some(value) = tiff.blob(index, 37500, 64 << 10)?
            && note.replace(value).is_some()
        {
            return Err(CodecError::Decode);
        }
    }
    let headroom = apple_headroom(&note.ok_or(CodecError::Decode)?)?;
    Ok(HeifGain {
        id: gain_id,
        apple: true,
        metadata: Metadata {
            min: [0.0; 3],
            max: [headroom; 3],
            gamma: [1.0; 3],
            base_offset: [0.0; 3],
            alternate_offset: [0.0; 3],
            base_headroom: 0.0,
            alternate_headroom: headroom,
            use_base: true,
        },
    })
}

fn apple_version(bytes: &[u8]) -> Result<bool, CodecError> {
    use quick_xml::{
        NsReader,
        events::Event,
        name::{Namespace, ResolveResult},
    };
    if bytes.len() > 64 << 10 {
        return Err(CodecError::Limits);
    }
    let mut reader = NsReader::from_reader(bytes);
    let namespace = ResolveResult::Bound(Namespace("http://ns.apple.com/HDRGainMap/1.0/"));
    let mut found = false;
    let mut value: Option<String> = None;
    let mut depth = 0_usize;
    for _ in 0..4096 {
        let event = reader.read_event().map_err(|_| CodecError::Decode)?;
        let start = matches!(&event, Event::Start(_));
        match event {
            Event::Start(e) | Event::Empty(e) => {
                // The version is a scalar RDF property; nested resources are not versions.
                if value.is_some() {
                    return Err(CodecError::Unsupported);
                }
                let (ns, name) = reader.resolver().resolve_element(e.name());
                if ns == namespace && name.as_ref() == "HDRGainMapVersion" {
                    if found || !start {
                        return Err(CodecError::Unsupported);
                    }
                    found = true;
                    value = Some(String::new());
                }
                for attribute in e.attributes() {
                    let attribute = attribute.map_err(|_| CodecError::Decode)?;
                    let (ns, name) = reader.resolver().resolve_attribute(attribute.key);
                    if ns == namespace && name.as_ref() == "HDRGainMapVersion" {
                        if found || attribute.value.as_ref() != "65536" {
                            return Err(CodecError::Unsupported);
                        }
                        found = true;
                    }
                }
                // Empty events do not contribute to nesting; byte and event caps still apply.
                if start {
                    depth += 1;
                }
                if depth > 32 {
                    return Err(CodecError::Limits);
                }
            }
            Event::Text(text) => {
                if let Some(value) = &mut value {
                    if value.len() + text.len() > 256 {
                        return Err(CodecError::Limits);
                    }
                    value.push_str(text.as_ref());
                }
            }
            Event::CData(text) => {
                if let Some(value) = &mut value {
                    if value.len() + text.len() > 256 {
                        return Err(CodecError::Limits);
                    }
                    value.push_str(text.as_ref());
                }
            }
            Event::End(_) => {
                if let Some(value) = value.take()
                    && value.trim() != "65536"
                {
                    return Err(CodecError::Unsupported);
                }
                depth = depth.checked_sub(1).ok_or(CodecError::Decode)?;
            }
            Event::DocType(_) | Event::GeneralRef(_) => {
                return Err(CodecError::Decode);
            }
            Event::Eof => {
                return if depth == 0 {
                    Ok(found)
                } else {
                    Err(CodecError::Decode)
                };
            }
            _ => {}
        }
    }
    Err(CodecError::Limits)
}
fn apple_headroom(note: &[u8]) -> Result<f32, CodecError> {
    if note.get(..10) != Some(b"Apple iOS\0") || note.len() > 64 << 10 {
        return Err(CodecError::Unsupported);
    }
    let little = match note.get(12..14) {
        Some(b"II") => true,
        Some(b"MM") => false,
        _ => return Err(CodecError::Decode),
    };
    let n = |at: usize, size: usize| -> Result<u32, CodecError> {
        let b = note
            .get(at..at.checked_add(size).ok_or(CodecError::Decode)?)
            .ok_or(CodecError::Decode)?;
        Ok(if little {
            b.iter().rev().fold(0, |v, b| v << 8 | u32::from(*b))
        } else {
            b.iter().fold(0, |v, b| v << 8 | u32::from(*b))
        })
    };
    let count = n(14, 2)? as usize;
    if count > 256 {
        return Err(CodecError::Limits);
    }
    let mut values = [None, None];
    for i in 0..count {
        let at = 16 + i * 12;
        let tag = n(at, 2)?;
        if !matches!(tag, 33 | 48) {
            continue;
        }
        if n(at + 4, 4)? != 1 {
            return Err(CodecError::Decode);
        }
        let value = match n(at + 2, 2)? {
            11 => f32::from_bits(n(at + 8, 4)?),
            10 => {
                let offset = n(at + 8, 4)? as usize;
                let a = i32::from_ne_bytes(n(offset, 4)?.to_ne_bytes());
                let b = i32::from_ne_bytes(n(offset + 4, 4)?.to_ne_bytes());
                if b == 0 {
                    return Err(CodecError::Decode);
                }
                a as f32 / b as f32
            }
            _ => return Err(CodecError::Decode),
        };
        if !value.is_finite() || values[usize::from(tag == 48)].replace(value).is_some() {
            return Err(CodecError::Decode);
        }
    }
    let [Some(a), Some(b)] = values else {
        return Err(CodecError::Decode);
    };
    // Apple's documented headroom calculation. Values below zero stops are SDR.
    let stops = if a < 1.0 {
        if b <= 0.01 {
            -20.0 * b + 1.8
        } else {
            -0.101 * b + 1.601
        }
    } else if b <= 0.01 {
        -70.0 * b + 3.0
    } else {
        -0.303 * b + 2.303
    };
    if !(0.01..=16.0).contains(&stops) {
        return Err(CodecError::Unsupported);
    }
    Ok(stops)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn jxl_brob_detection_checks_inner_type_and_box_extent() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/hdr/gain-map.jxl");
        let base = crate::native::identify_jxl(&path, &Limits::default()).unwrap();
        for extended in [false, true] {
            for (payload, present) in [
                (b"jhgm\0".as_slice(), true),
                (b"Exifjhgm".as_slice(), false),
            ] {
                let size = if extended { 16 } else { 8 } + payload.len();
                let mut bytes = if extended {
                    let mut bytes = b"\0\0\0\x01brob".to_vec();
                    bytes.extend_from_slice(&(size as u64).to_be_bytes());
                    bytes
                } else {
                    let mut bytes = (size as u32).to_be_bytes().to_vec();
                    bytes.extend_from_slice(b"brob");
                    bytes
                };
                bytes.extend_from_slice(payload);
                std::fs::write(file.path(), &bytes).unwrap();
                assert_eq!(jxl_present(file.path()), Ok(present));
                if present {
                    assert!(matches!(
                        jxl(&bytes, &Limits::default(), &base),
                        Err(CodecError::CompressedJxlGainMap)
                    ));
                }
            }
        }
        let bytes = b"\0\0\0\0brobjhgm\0";
        std::fs::write(file.path(), bytes).unwrap();
        assert_eq!(jxl_present(file.path()), Ok(true));
        assert!(matches!(
            jxl(bytes, &Limits::default(), &base),
            Err(CodecError::CompressedJxlGainMap)
        ));
        assert!(matches!(boxes(bytes, &mut 4096), Err(CodecError::Decode)));
        for length in 0..4 {
            let mut bytes = (8 + length as u32).to_be_bytes().to_vec();
            bytes.extend_from_slice(b"brob");
            bytes.extend_from_slice(&b"jhgm"[..length]);
            // A following valid box must not supply the missing inner-type bytes.
            bytes.extend_from_slice(b"\0\0\0\x08free");
            std::fs::write(file.path(), &bytes).unwrap();
            assert_eq!(jxl_present(file.path()), Err(CodecError::Decode));
        }
    }
    fn metadata() -> Metadata {
        Metadata {
            min: [0.0; 3],
            max: [2.0; 3],
            gamma: [1.0; 3],
            base_offset: [0.0; 3],
            alternate_offset: [0.0; 3],
            base_headroom: 0.0,
            alternate_headroom: 2.0,
            use_base: true,
        }
    }
    #[test]
    fn iso_rejects_every_truncation_zero_denominator_flags_and_invalid_coefficients() {
        let bytes = iso_write(&metadata()).unwrap();
        assert!(iso(&bytes).is_ok());
        for length in 0..bytes.len() {
            assert!(iso(&bytes[..length]).is_err());
        }
        let mut bad = bytes.clone();
        bad[5..9].fill(0);
        assert!(iso(&bad).is_err());
        for flag in [1, 2, 4, 16, 32] {
            let mut bad = bytes.clone();
            bad[4] |= flag;
            assert!(iso(&bad).is_err());
        }
        let mut bad = bytes.clone();
        bad[1] = 1;
        assert!(iso(&bad).is_err());
        let mut bad = bytes.clone();
        bad.extend([0]);
        assert!(iso(&bad).is_err());
        for mutate in 0..6 {
            let mut m = metadata();
            match mutate {
                0 => m.gamma[1] = 0.0,
                1 => m.gamma[2] = f32::NAN,
                2 => m.max[0] = -1.0,
                3 => m.base_offset[0] = f32::INFINITY,
                4 => m.alternate_headroom = 0.0,
                _ => m.min[0] = 20.0,
            }
            assert!(m.validate().is_err());
        }
    }
    #[test]
    fn iso_keeps_single_and_per_channel_coefficients_and_headroom() {
        let mut m = metadata();
        m.min = [-1.0, 0.0, 1.0];
        m.max = [1.0, 2.0, 3.0];
        m.gamma = [0.5, 1.0, 2.0];
        m.base_offset = [0.01, 0.02, 0.03];
        m.alternate_offset = [0.04, 0.05, 0.06];
        m.base_headroom = 0.5;
        m.alternate_headroom = 4.5;
        let decoded = iso(&iso_write(&m).unwrap()).unwrap();
        for c in 0..3 {
            assert!((decoded.min[c] - m.min[c]).abs() < 0.00001);
            assert!((decoded.max[c] - m.max[c]).abs() < 0.00001);
            assert!((decoded.gamma[c] - m.gamma[c]).abs() < 0.00001);
            assert!((decoded.base_offset[c] - m.base_offset[c]).abs() < 0.00001);
            assert!((decoded.alternate_offset[c] - m.alternate_offset[c]).abs() < 0.00001);
        }
        assert!((decoded.base_headroom - 0.5).abs() < 0.00001);
        assert!((decoded.alternate_headroom - 4.5).abs() < 0.00001);
    }
    #[test]
    fn apple_xmp_accepts_attribute_and_element_packets() {
        for packet in [
            include_bytes!("../../../fixtures/hdr/apple-attribute.xmp").as_slice(),
            include_bytes!("../../../fixtures/hdr/apple-element.xmp").as_slice(),
            b"<root xmlns:h='http://ns.apple.com/HDRGainMap/1.0/'><h:HDRGainMapVersion> \n65536\t </h:HDRGainMapVersion></root>",
            b"<HDRGainMapVersion xmlns='http://ns.apple.com/HDRGainMap/1.0/'>65536</HDRGainMapVersion>",
            b"<root xmlns:h='http://ns.apple.com/HDRGainMap/1.0/'><h:HDRGainMapVersion>655<!-- XMP comment -->36</h:HDRGainMapVersion></root>",
            b"<root xmlns:h='http://ns.apple.com/HDRGainMap/1.0/'><h:HDRGainMapVersion><![CDATA[65536]]></h:HDRGainMapVersion></root>",
        ] {
            assert!(apple_version(packet).unwrap());
        }
    }
    #[test]
    fn apple_xmp_rejects_malformed_and_duplicate_element_properties() {
        for property in [
            "<h:HDRGainMapVersion>2</h:HDRGainMapVersion>",
            "<h:HDRGainMapVersion/>",
            "<h:HDRGainMapVersion></h:HDRGainMapVersion>",
            "<h:HDRGainMapVersion>65536 65536</h:HDRGainMapVersion>",
            "<h:HDRGainMapVersion>65536<value/></h:HDRGainMapVersion>",
            "<h:HDRGainMapVersion><value>65536</value></h:HDRGainMapVersion>",
            "<h:HDRGainMapVersion>6553&six;</h:HDRGainMapVersion>",
            "<h:HDRGainMapVersion><![CDATA[2]]></h:HDRGainMapVersion>",
            "<h:HDRGainMapVersion>65536</h:HDRGainMapVersion><h:HDRGainMapVersion>65536</h:HDRGainMapVersion>",
            "<a h:HDRGainMapVersion='65536'/><h:HDRGainMapVersion>65536</h:HDRGainMapVersion>",
            "<h:HDRGainMapVersion>65536</h:HDRGainMapVersion><a h:HDRGainMapVersion='65536'/>",
            "<h:HDRGainMapVersion h:HDRGainMapVersion='65536'>65536</h:HDRGainMapVersion>",
            "<a h:HDRGainMapVersion='65536'/><a h:HDRGainMapVersion='65536'/>",
            "<h:HDRGainMapVersion>65536</h:WrongEnd>",
            "<h:HDRGainMapVersion>65536",
        ] {
            let packet =
                format!("<root xmlns:h='http://ns.apple.com/HDRGainMap/1.0/'>{property}</root>");
            assert!(apple_version(packet.as_bytes()).is_err(), "{property}");
        }
        let oversized = format!(
            "<h:HDRGainMapVersion xmlns:h='http://ns.apple.com/HDRGainMap/1.0/'>{}</h:HDRGainMapVersion>",
            " ".repeat(257)
        );
        assert_eq!(apple_version(oversized.as_bytes()), Err(CodecError::Limits));
    }
    #[test]
    fn apple_xmp_rejects_spoofed_namespace_unknown_version_and_entities() {
        let valid =
            b"<root xmlns:h='http://ns.apple.com/HDRGainMap/1.0/' h:HDRGainMapVersion='65536'/>";
        assert!(apple_version(valid).unwrap());
        assert!(
            !apple_version(b"<root xmlns:h='unrelated' h:HDRGainMapVersion='65536'/>").unwrap()
        );
        for packet in [
            b"<root xmlns:h='unrelated'><h:HDRGainMapVersion>65536</h:HDRGainMapVersion></root>".as_slice(),
            b"<root xmlns:h='http://ns.apple.com/HDRGainMap/1.0/'><HDRGainMapVersion>65536</HDRGainMapVersion></root>",
            b"<root xmlns:h='http://ns.apple.com/HDRGainMap/1.0/'><a xmlns:h='unrelated'><h:HDRGainMapVersion>65536</h:HDRGainMapVersion></a></root>",
        ] {
            assert!(!apple_version(packet).unwrap());
        }
        for bad in [
            b"<root xmlns:h='http://ns.apple.com/HDRGainMap/1.0/' h:HDRGainMapVersion='2'/>"
                .as_slice(),
            b"<!DOCTYPE root [<!ENTITY x 'test'>]><root/>",
            b"<root>",
        ] {
            assert!(apple_version(bad).is_err());
        }
        let mut deep = b"<a>".repeat(33);
        deep.extend(b"</a>".repeat(33));
        assert!(apple_version(&deep).is_err());
        assert!(apple_version(&vec![0; 65537]).is_err());
        assert!(apple_version(&b"<a/>".repeat(4097)).is_err());
    }
    #[test]
    fn apple_note_rejects_missing_values_nonfinite_and_truncated_fields() {
        let mut note = b"Apple iOS\0\0\x01MM\0\x02".to_vec();
        for (tag, value) in [(33_u16, 1.0_f32), (48, 0.0)] {
            note.extend(tag.to_be_bytes());
            note.extend(11_u16.to_be_bytes());
            note.extend(1_u32.to_be_bytes());
            note.extend(value.to_bits().to_be_bytes());
        }
        note.extend([0; 4]);
        assert!((apple_headroom(&note).unwrap() - 3.0).abs() < 0.0001);
        for end in 0..40 {
            assert!(apple_headroom(&note[..end]).is_err());
        }
        let mut bad = note.clone();
        bad[24..28].copy_from_slice(&f32::NAN.to_bits().to_be_bytes());
        assert!(apple_headroom(&bad).is_err());
        let mut bad = note;
        bad[14..16].copy_from_slice(&1_u16.to_be_bytes());
        assert!(apple_headroom(&bad).is_err());
    }
}
