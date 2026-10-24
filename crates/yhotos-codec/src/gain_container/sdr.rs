//! Rebuild only the required SDR item graph after an optional item breaks libheif.
//! Absolute pixel offsets and primary properties are retained; no pixels are copied
//! from auxiliary items into the graph handed to the native loader.
use super::{Box, Cursor, ScanBudget, boxes, one};
use crate::CodecError;
use std::collections::{BTreeMap, BTreeSet};

fn boxed(kind: &[u8], payload: &[u8]) -> Result<Vec<u8>, CodecError> {
    if payload.len() > (16 << 20) - 8 {
        return Err(CodecError::Limits);
    }
    let mut result = u32::try_from(payload.len() + 8)
        .map_err(|_| CodecError::Limits)?
        .to_be_bytes()
        .to_vec();
    result.extend_from_slice(kind);
    result.extend_from_slice(payload);
    Ok(result)
}

fn append(out: &mut Vec<u8>, kind: &[u8], payload: &[u8]) -> Result<(), CodecError> {
    if out
        .len()
        .checked_add(payload.len() + 8)
        .is_none_or(|len| len > 16 << 20)
    {
        return Err(CodecError::Limits);
    }
    out.extend(boxed(kind, payload)?);
    Ok(())
}

type Reference<'a> = (u64, u32, Vec<u32>, Box<'a>);

fn required_items(
    item: u32,
    depth: u8,
    infos: &BTreeMap<u32, Box<'_>>,
    references: &[Reference<'_>],
    visiting: &mut BTreeSet<u32>,
    checked: &mut BTreeSet<u32>,
) -> Result<(), CodecError> {
    if depth > 8 {
        return Err(CodecError::Limits);
    }
    if checked.contains(&item) {
        return Ok(());
    }
    if !visiting.insert(item) {
        return Err(CodecError::Decode);
    }
    let kind = item_type(infos.get(&item).ok_or(CodecError::Decode)?)?;
    if matches!(kind, b"grid" | b"iden") {
        let mut matching = references
            .iter()
            .filter(|(_, from, _, r)| *from == item && r.kind == b"dimg");
        let (_, _, targets, _) = matching.next().ok_or(CodecError::Decode)?;
        if matching.next().is_some()
            || targets.is_empty()
            || targets.iter().collect::<BTreeSet<_>>().len() != targets.len()
        {
            return Err(CodecError::Decode);
        }
        for target in targets {
            required_items(*target, depth + 1, infos, references, visiting, checked)?;
        }
    } else if !matches!(kind, b"av01" | b"hvc1") {
        return Err(CodecError::Unsupported);
    }
    visiting.remove(&item);
    checked.insert(item);
    Ok(())
}

fn id(c: &mut Cursor<'_>, wide: bool) -> Result<u32, CodecError> {
    u32::try_from(c.num(if wide { 4 } else { 2 })?).map_err(|_| CodecError::Decode)
}

fn infos<'a>(
    data: &'a [u8],
    budget: &mut ScanBudget,
) -> Result<BTreeMap<u32, Box<'a>>, CodecError> {
    let mut c = Cursor::new(data);
    let version = c.num(1)?;
    if version > 1 || c.num(3)? != 0 || c.num(if version == 0 { 2 } else { 4 })? > 4096 {
        return Err(CodecError::Limits);
    }
    let mut result = BTreeMap::new();
    for info in boxes(&data[c.at..], budget)? {
        if info.kind != b"infe" {
            continue;
        }
        let mut c = Cursor::new(info.data);
        let version = c.num(1)?;
        if !matches!(version, 2 | 3) {
            continue;
        }
        c.num(3)?;
        let item = id(&mut c, version == 3)?;
        if item == 0 || result.insert(item, info).is_some() {
            return Err(CodecError::Decode);
        }
    }
    Ok(result)
}

fn item_type<'a>(info: &Box<'a>) -> Result<&'a [u8], CodecError> {
    let mut c = Cursor::new(info.data);
    let wide = c.num(1)? == 3;
    c.num(3)?;
    id(&mut c, wide)?;
    c.num(2)?;
    c.take(4)
}

// Validate the complete declaration before exposing a metadata item to libheif.
fn primary_metadata(info: &Box<'_>) -> bool {
    let parse = || -> Result<bool, CodecError> {
        let mut c = Cursor::new(info.data);
        let version = c.num(1)?;
        if !matches!(version, 2 | 3) || c.num(3)? != 0 {
            return Ok(false);
        }
        id(&mut c, version == 3)?;
        if c.num(2)? != 0 {
            return Ok(false);
        }
        let kind = c.take(4)?;
        let rest = &info.data[c.at..];
        let Some(name_end) = rest.iter().position(|b| *b == 0) else {
            return Ok(false);
        };
        let content = &rest[name_end + 1..];
        Ok(match kind {
            b"Exif" => content.is_empty(),
            b"mime" => content
                .strip_prefix(b"application/rdf+xml\0")
                .is_some_and(|encoding| {
                    // content_encoding is optional, but when present it is one
                    // complete UTF-8 string. Retain it exactly as the normal loader does.
                    encoding.is_empty()
                        || encoding.strip_suffix(&[0]).is_some_and(|text| {
                            !text.contains(&0) && std::str::from_utf8(text).is_ok()
                        })
                }),
            _ => false,
        })
    };
    parse() == Ok(true)
}

// Copy only checked locations. Invalid optional metadata is dropped independently
// of required pixels; a bad auxiliary extent is never followed.
fn iloc(
    data: &[u8],
    required: &mut BTreeSet<u32>,
    metadata: &BTreeSet<u32>,
    bytes: &[u8],
    idat: &[u8],
) -> Result<Vec<u8>, CodecError> {
    let mut c = Cursor::new(data);
    let version = c.num(1)?;
    if version > 2 || c.num(3)? != 0 {
        return Err(CodecError::Unsupported);
    }
    let sizes = c.num(2)?;
    let offset = (sizes >> 12) as usize;
    let length = ((sizes >> 8) & 15) as usize;
    let base = ((sizes >> 4) & 15) as usize;
    let index = if version > 0 {
        (sizes & 15) as usize
    } else {
        0
    };
    if [offset, length, base, index]
        .iter()
        .any(|n| !matches!(n, 0 | 4 | 8))
    {
        return Err(CodecError::Unsupported);
    }
    let count = c.num(if version == 2 { 4 } else { 2 })?;
    if count > 4096 {
        return Err(CodecError::Limits);
    }
    let mut entries = BTreeMap::new();
    let mut extents = 4096_usize;
    let mut metadata_remaining = 16_u64 << 20;
    for _ in 0..count {
        let start = c.at;
        let item = id(&mut c, version == 2)?;
        let method = if version > 0 { c.num(2)? } else { 0 };
        let reference = c.num(2)?;
        let base = c.num(base)?;
        let count = c.num(2)? as usize;
        extents = extents.checked_sub(count).ok_or(CodecError::Limits)?;
        let mut valid = count > 0 && method <= 1 && reference == 0;
        let mut item_bytes = 0_u64;
        for _ in 0..count {
            let index = c.num(index)?;
            let at = c.num(offset)?;
            let len = c.num(length)?;
            let end = base.checked_add(at).and_then(|at| at.checked_add(len));
            valid &= index == 0
                && len > 0
                && end.is_some_and(|end| {
                    end <= if method == 0 { bytes.len() } else { idat.len() } as u64
                });
            // Invalid optional extents are discarded, so their untrusted
            // lengths must not overflow accounting for retained metadata.
            item_bytes = item_bytes.saturating_add(len);
        }
        if required.contains(&item) && !valid {
            return Err(CodecError::Decode);
        }
        let keep_metadata = metadata.contains(&item) && valid;
        if keep_metadata {
            metadata_remaining = metadata_remaining
                .checked_sub(item_bytes)
                .ok_or(CodecError::Limits)?;
        }
        if (required.contains(&item) || keep_metadata)
            && entries.insert(item, &data[start..c.at]).is_some()
        {
            return Err(CodecError::Decode);
        }
    }
    required.extend(metadata.iter().filter(|item| entries.contains_key(item)));
    if c.at != data.len() || entries.len() != required.len() {
        return Err(CodecError::Decode);
    }
    let mut result = data[..6].to_vec();
    if version == 2 {
        result.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    } else {
        result.extend_from_slice(&(entries.len() as u16).to_be_bytes());
    }
    for entry in entries.values() {
        result.extend_from_slice(entry);
    }
    boxed(b"iloc", &result)
}

#[allow(clippy::too_many_lines)] // One bounded primary graph rebuild with a shared box budget.
pub(crate) fn heif_sdr(bytes: &[u8]) -> Result<Vec<u8>, CodecError> {
    let mut budget = ScanBudget::default();
    let top = boxes(bytes, &mut budget)?;
    let ftyp = one(&top, b"ftyp")?;
    if !(8..=4088).contains(&ftyp.len()) || !ftyp.len().is_multiple_of(4) {
        return Err(CodecError::Decode);
    }
    let original = one(&top, b"meta")?;
    if original.len() > 16 << 20 || original.get(..4) != Some(&[0; 4]) {
        return Err(CodecError::Limits);
    }
    let meta = boxes(&original[4..], &mut budget)?;
    let pitm = one(&meta, b"pitm")?;
    let mut c = Cursor::new(pitm);
    let version = c.num(1)?;
    if version > 1 || c.num(3)? != 0 {
        return Err(CodecError::Unsupported);
    }
    let primary = id(&mut c, version == 1)?;
    let infos = infos(one(&meta, b"iinf")?, &mut budget)?;
    let iprp = boxes(one(&meta, b"iprp")?, &mut budget)?;
    let properties = boxes(one(&iprp, b"ipco")?, &mut budget)?;
    let associations = one(&iprp, b"ipma")?;
    let mut c = Cursor::new(associations);
    let version = c.num(1)?;
    let flags = c.num(3)?;
    let count = c.num(4)?;
    if version > 1 || flags > 1 || count > 4096 {
        return Err(CodecError::Limits);
    }
    let mut items = BTreeMap::new();
    let mut alpha = BTreeSet::new();
    let mut remaining = 4096_usize;
    for _ in 0..count {
        let item = id(&mut c, version == 1)?;
        let count = c.num(1)? as usize;
        remaining = remaining.checked_sub(count).ok_or(CodecError::Limits)?;
        let mut values = Vec::new();
        for _ in 0..count {
            let value = c.num(if flags == 0 { 1 } else { 2 })? as u16;
            let index = value & if flags == 0 { 127 } else { 32767 };
            if let Some(property) = index
                .checked_sub(1)
                .and_then(|i| properties.get(usize::from(i)))
                && property.kind == b"auxC"
                && matches!(
                    property.data.get(4..),
                    Some(
                        b"urn:mpeg:hevc:2015:auxid:1\0"
                            | b"urn:mpeg:mpegB:cicp:systems:auxiliary:alpha\0"
                    )
                )
            {
                alpha.insert(item);
            }
            values.push(value);
        }
        if items.insert(item, values).is_some() {
            return Err(CodecError::Decode);
        }
    }
    if c.at != associations.len() {
        return Err(CodecError::Decode);
    }
    let mut required = BTreeSet::from([primary]);
    let mut references = Vec::new();
    let mut metadata = BTreeSet::new();
    if let Some(iref) = meta.iter().find(|b| b.kind == b"iref") {
        let header = iref.data.get(..4);
        let known = header.is_some_and(|h| h[0] <= 1 && h[1..] == [0; 3]);
        if !known
            && (!alpha.is_empty()
                || matches!(
                    item_type(infos.get(&primary).ok_or(CodecError::Decode)?)?,
                    b"grid" | b"iden"
                ))
        {
            return Err(CodecError::Unsupported);
        }
        let version = u64::from(header.map_or(0, |h| h[0]));
        let mut remaining = 4096_usize;
        let raw_references = if iref.data.len() >= 4 {
            boxes(&iref.data[4..], &mut budget)?
        } else {
            Vec::new()
        };
        for reference in raw_references.into_iter().filter(|_| known) {
            if !matches!(reference.kind, b"dimg" | b"auxl" | b"cdsc" | b"prem") {
                continue;
            }
            if reference.data.len() < if version == 1 { 6 } else { 4 } {
                // No complete source id/count; it cannot name a usable optional
                // item. Required primary dependencies still fail in the loader.
                continue;
            }
            let mut c = Cursor::new(reference.data);
            let from = id(&mut c, version == 1)?;
            let count = c.num(2)? as usize;
            remaining = remaining.checked_sub(count).ok_or(CodecError::Limits)?;
            let derived = infos
                .get(&from)
                .and_then(|info| item_type(info).ok())
                .is_some_and(|t| matches!(t, b"grid" | b"iden"));
            if reference.kind == b"cdsc" {
                // Descriptive metadata points from the metadata item to the
                // primary. Ignore damaged/foreign auxiliary descriptions.
                let parsed = (|| -> Result<bool, CodecError> {
                    let mut targets = Vec::new();
                    for _ in 0..count {
                        targets.push(id(&mut c, version == 1)?);
                    }
                    Ok(c.at == reference.data.len()
                        && targets.contains(&primary)
                        && targets.iter().collect::<BTreeSet<_>>().len() == targets.len()
                        && infos.get(&from).is_some_and(primary_metadata))
                })();
                if parsed == Ok(true) && metadata.insert(from) {
                    references.push((version, from, vec![primary], reference));
                }
                continue;
            }
            if reference.kind == b"dimg" && derived
                || reference.kind == b"auxl" && alpha.contains(&from)
                || reference.kind == b"prem" && from == primary
            {
                let mut targets = Vec::new();
                for _ in 0..count {
                    targets.push(id(&mut c, version == 1)?);
                }
                if c.at != reference.data.len() {
                    return Err(CodecError::Decode);
                }
                if reference.kind == b"auxl" && targets.as_slice() == [primary] {
                    required.insert(from);
                }
                references.push((version, from, targets, reference));
            }
        }
    }
    let mut checked = BTreeSet::new();
    for item in required {
        required_items(
            item,
            0,
            &infos,
            &references,
            &mut BTreeSet::new(),
            &mut checked,
        )?;
    }
    let mut required = checked;
    let idat = meta
        .iter()
        .find(|b| b.kind == b"idat")
        .map_or(&[][..], |b| b.data);
    let locations = iloc(one(&meta, b"iloc")?, &mut required, &metadata, bytes, idat)?;
    let mut new_meta = vec![0; 4];
    append(&mut new_meta, b"hdlr", one(&meta, b"hdlr")?)?;
    append(&mut new_meta, b"pitm", pitm)?;
    let mut iinf = vec![0; 4];
    iinf.extend_from_slice(&(required.len() as u16).to_be_bytes());
    for item in &required {
        append(
            &mut iinf,
            b"infe",
            infos.get(item).ok_or(CodecError::Decode)?.data,
        )?;
    }
    append(&mut new_meta, b"iinf", &iinf)?;
    if new_meta.len() + locations.len() > 16 << 20 {
        return Err(CodecError::Limits);
    }
    new_meta.extend(locations);
    if !idat.is_empty() {
        append(&mut new_meta, b"idat", idat)?;
    }
    let mut ipco = Vec::new();
    let mut ipma = vec![1, 0, 0, 1]; // wide ids and indices, preserving essential bits
    ipma.extend_from_slice(&(required.len() as u32).to_be_bytes());
    let mut index = 0_u16;
    let mut property_indices = BTreeMap::new();
    for item in &required {
        let values = match items.get(item) {
            Some(values) => values.as_slice(),
            None if metadata.contains(item) => &[],
            None => return Err(CodecError::Decode),
        };
        ipma.extend_from_slice(&item.to_be_bytes());
        ipma.push(values.len() as u8);
        for value in values {
            let property = value & if flags == 0 { 127 } else { 32767 };
            if property == 0 {
                ipma.extend_from_slice(&[0, 0]);
                continue;
            }
            let mapped = if let Some(mapped) = property_indices.get(&property) {
                *mapped
            } else {
                let value = properties
                    .get(usize::from(property - 1))
                    .ok_or(CodecError::Decode)?;
                append(&mut ipco, value.kind, value.data)?;
                index = index
                    .checked_add(1)
                    .filter(|i| *i < 32768)
                    .ok_or(CodecError::Limits)?;
                property_indices.insert(property, index);
                index
            };
            let essential = value & if flags == 0 { 128 } else { 32768 } != 0;
            ipma.extend_from_slice(&(mapped | if essential { 32768 } else { 0 }).to_be_bytes());
        }
    }
    let mut iprp = boxed(b"ipco", &ipco)?;
    append(&mut iprp, b"ipma", &ipma)?;
    append(&mut new_meta, b"iprp", &iprp)?;
    if !references.is_empty() {
        let version = references[0].0;
        let mut iref = vec![version as u8, 0, 0, 0];
        for (_, from, targets, reference) in references {
            if !required.contains(&from) || !targets.iter().all(|item| required.contains(item)) {
                continue;
            }
            if reference.kind == b"cdsc" {
                // A metadata item can describe both primary and auxiliary items.
                // Keep the primary link without references to stripped items.
                let mut data = if version == 0 {
                    (from as u16).to_be_bytes().to_vec()
                } else {
                    from.to_be_bytes().to_vec()
                };
                data.extend_from_slice(&1_u16.to_be_bytes());
                if version == 0 {
                    data.extend_from_slice(&(primary as u16).to_be_bytes());
                } else {
                    data.extend_from_slice(&primary.to_be_bytes());
                }
                append(&mut iref, b"cdsc", &data)?;
            } else {
                append(&mut iref, reference.kind, reference.data)?;
            }
        }
        append(&mut new_meta, b"iref", &iref)?;
    }
    let start = original.as_ptr() as usize - bytes.as_ptr() as usize;
    let mut result = bytes.to_vec();
    if let Some(padding) = original.len().checked_sub(new_meta.len())
        && (padding == 0 || padding >= 8)
    {
        if padding != 0 {
            new_meta.extend(boxed(b"free", &vec![0; padding - 8])?);
        }
        result[start..start + original.len()].copy_from_slice(&new_meta);
    } else {
        // Widening ipma entries can outgrow the old meta. Move it to EOF,
        // keeping mdat at its absolute offsets and idat relative to the copy.
        let kind_at = start.checked_sub(4).ok_or(CodecError::Decode)?;
        let kind_at = if bytes.get(kind_at..start) == Some(b"meta") {
            kind_at
        } else {
            start.checked_sub(12).ok_or(CodecError::Decode)?
        };
        result
            .get_mut(kind_at..kind_at + 4)
            .ok_or(CodecError::Decode)?
            .copy_from_slice(b"free");
        result.extend(boxed(b"meta", &new_meta)?);
    }
    Ok(result)
}

#[cfg(test)]
#[path = "sdr_tests.rs"]
mod tests;
