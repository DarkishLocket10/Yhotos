//! Rebuild only the required SDR item graph after an optional item breaks libheif.
//! Absolute pixel offsets and primary properties are retained; no pixels are copied
//! from auxiliary items into the graph handed to the native loader.
use super::{Box, Cursor, boxes, one};
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

fn infos<'a>(data: &'a [u8], budget: &mut usize) -> Result<BTreeMap<u32, Box<'a>>, CodecError> {
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

// Copy only checked locations of required items. Optional extents are never followed.
fn iloc(
    data: &[u8],
    required: &BTreeSet<u32>,
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
    for _ in 0..count {
        let start = c.at;
        let item = id(&mut c, version == 2)?;
        let method = if version > 0 { c.num(2)? } else { 0 };
        let reference = c.num(2)?;
        let base = c.num(base)?;
        let count = c.num(2)? as usize;
        extents = extents.checked_sub(count).ok_or(CodecError::Limits)?;
        for _ in 0..count {
            let index = c.num(index)?;
            let at = c.num(offset)?;
            let len = c.num(length)?;
            if required.contains(&item) {
                if method > 1 || reference != 0 || index != 0 {
                    return Err(CodecError::Unsupported);
                }
                let end = base
                    .checked_add(at)
                    .and_then(|at| at.checked_add(len))
                    .ok_or(CodecError::Decode)?;
                if len == 0 || end > if method == 0 { bytes.len() } else { idat.len() } as u64 {
                    return Err(CodecError::Decode);
                }
            }
        }
        if required.contains(&item) && entries.insert(item, &data[start..c.at]).is_some() {
            return Err(CodecError::Decode);
        }
    }
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
    let mut budget = 4096;
    let top = boxes(bytes, &mut budget)?;
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
            if !matches!(reference.kind, b"dimg" | b"auxl") {
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
            if reference.kind == b"dimg" && derived
                || reference.kind == b"auxl" && alpha.contains(&from)
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
    let required = checked;
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
    let idat = meta
        .iter()
        .find(|b| b.kind == b"idat")
        .map_or(&[][..], |b| b.data);
    let locations = iloc(one(&meta, b"iloc")?, &required, bytes, idat)?;
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
        let values = items.get(item).ok_or(CodecError::Decode)?;
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
        for (_, from, _, reference) in references {
            if required.contains(&from) {
                append(&mut iref, reference.kind, reference.data)?;
            }
        }
        append(&mut new_meta, b"iref", &iref)?;
    }
    let padding = original
        .len()
        .checked_sub(new_meta.len())
        .filter(|n| *n >= 8)
        .ok_or(CodecError::Decode)?;
    new_meta.extend(boxed(b"free", &vec![0; padding - 8])?);
    let start = original.as_ptr() as usize - bytes.as_ptr() as usize;
    let mut result = bytes.to_vec();
    result[start..start + original.len()].copy_from_slice(&new_meta);
    Ok(result)
}
