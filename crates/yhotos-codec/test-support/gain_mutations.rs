//! Deterministic optional-metadata mutations shared by codec, host and fuzz tests.
#![allow(
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::unwrap_used,
    clippy::too_many_lines,
    missing_docs
)]

pub struct Case {
    pub name: String,
    pub bytes: Vec<u8>,
    pub invalid_geometry: bool,
}

fn push(cases: &mut Vec<Case>, name: impl Into<String>, bytes: Vec<u8>) {
    cases.push(Case {
        name: name.into(),
        bytes,
        invalid_geometry: false,
    });
}

fn replace(cases: &mut Vec<Case>, seed: &[u8], name: &str, at: usize, value: &[u8]) {
    let mut bytes = seed.to_vec();
    bytes[at..at + value.len()].copy_from_slice(value);
    push(cases, name, bytes);
}

fn find(bytes: &[u8], pattern: &[u8]) -> usize {
    bytes
        .windows(pattern.len())
        .position(|b| b == pattern)
        .unwrap()
}

fn u32_at(bytes: &[u8], at: usize) -> usize {
    u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize
}

// Resize one optional leaf, repairing ancestor extents and absolute iloc offsets
// so the primary codestream remains intact. Only the fixture's v0 iloc is used.
fn edit_box(seed: &[u8], at: usize, replacement: &[u8]) -> Vec<u8> {
    fn ancestors(
        seed: &[u8],
        start: usize,
        end: usize,
        target: usize,
        path: &mut Vec<usize>,
    ) -> bool {
        let mut at = start;
        while at < end {
            let size = u32_at(seed, at);
            if at == target {
                return true;
            }
            let skip = match &seed[at + 4..at + 8] {
                b"meta" | b"iref" => Some(4),
                b"iinf" => Some(6),
                b"iprp" | b"ipco" => Some(0),
                _ => None,
            };
            if let Some(skip) = skip {
                path.push(at);
                if ancestors(seed, at + 8 + skip, at + size, target, path) {
                    return true;
                }
                path.pop();
            }
            at += size;
        }
        false
    }
    let mut path = Vec::new();
    assert!(ancestors(seed, 0, seed.len(), at, &mut path));
    let size = u32_at(seed, at);
    let delta = isize::try_from(replacement.len()).unwrap() - isize::try_from(size).unwrap();
    let mut bytes = seed.to_vec();
    bytes.splice(at..at + size, replacement.iter().copied());
    for parent in path {
        let size = u32::try_from(isize::try_from(u32_at(seed, parent)).unwrap() + delta).unwrap();
        bytes[parent..parent + 4].copy_from_slice(&size.to_be_bytes());
    }
    let iloc = find(&bytes, b"iloc");
    assert_eq!(&bytes[iloc + 4..iloc + 10], b"\0\0\0\0\x44\0");
    let count = usize::from(u16::from_be_bytes([bytes[iloc + 10], bytes[iloc + 11]]));
    for entry in (iloc + 12..iloc + 12 + count * 14).step_by(14) {
        let offset = u32_at(&bytes, entry + 6);
        if offset >= at + size {
            bytes[entry + 6..entry + 10].copy_from_slice(
                &u32::try_from(isize::try_from(offset).unwrap() + delta)
                    .unwrap()
                    .to_be_bytes(),
            );
        }
    }
    bytes
}

fn corrupt_box(cases: &mut Vec<Case>, form: &str, seed: &[u8], kind_at: usize) {
    let at = kind_at - 4;
    let size = u32_at(seed, at);
    let kind = &seed[kind_at..kind_at + 4];
    // Every field boundary plus a partial field, not just one sample truncation.
    for length in (0..size - 8).step_by(2) {
        let mut leaf = ((length + 8) as u32).to_be_bytes().to_vec();
        leaf.extend_from_slice(kind);
        leaf.extend_from_slice(&seed[at + 8..at + 8 + length]);
        push(
            cases,
            format!("{form}/box-{kind_at}/truncate-{length}"),
            edit_box(seed, at, &leaf),
        );
    }
    // colr/pixi on a gain image are optional: raw gain samples do not enter
    // color management, and channel depth can be inferred from its codestream.
    if !matches!(kind, b"colr" | b"pixi") {
        let mut missing = seed[at..at + size].to_vec();
        missing[4..8].copy_from_slice(b"free");
        push(
            cases,
            format!("{form}/box-{kind_at}/missing"),
            edit_box(seed, at, &missing),
        );
    }
    if kind == b"dimg" || kind == b"auxl" || kind == b"cdsc" {
        let mut duplicate = seed[at..at + size].to_vec();
        duplicate.extend_from_slice(&seed[at..at + size]);
        push(
            cases,
            format!("{form}/box-{kind_at}/duplicate"),
            edit_box(seed, at, &duplicate),
        );
    }
}

// Every version/flags byte, denominator and coefficient, including RGB channels.
fn iso_mutations(seed: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut result = Vec::new();
    for end in 0..seed.len() {
        result.push((format!("truncate-{end}"), seed[..end].to_vec()));
    }
    for at in 0..5 {
        let mut bytes = seed.to_vec();
        bytes[at] = 255;
        result.push((format!("version-flags-{at}"), bytes));
    }
    let common = seed[4] & 8 != 0;
    let step = if common { 4 } else { 8 };
    let start = if common { 9 } else { 5 };
    if common {
        let mut bytes = seed.to_vec();
        bytes[5..9].fill(0);
        result.push(("common-zero-denominator".into(), bytes));
    }
    for at in (start..seed.len()).step_by(step) {
        let mut bytes = seed.to_vec();
        bytes[at..at + 4].copy_from_slice(&i32::MAX.to_be_bytes());
        result.push((format!("range-{at}"), bytes));
        if !common {
            let mut bytes = seed.to_vec();
            bytes[at + 4..at + 8].fill(0);
            result.push((format!("zero-denominator-{at}"), bytes));
        }
    }
    let mut bytes = seed.to_vec();
    bytes.push(0);
    result.push(("trailing-byte".into(), bytes));
    result
}

fn jpeg_aux(seed: &[u8]) -> (usize, usize) {
    let tiff = find(seed, b"MPF\0") + 4;
    (tiff, tiff + u32_at(seed, tiff + 74))
}

fn with_aux(seed: &[u8], aux: &[u8]) -> Vec<u8> {
    let (tiff, at) = jpeg_aux(seed);
    let mut bytes = seed[..at].to_vec();
    bytes[tiff + 70..tiff + 74].copy_from_slice(&(aux.len() as u32).to_be_bytes());
    bytes.extend_from_slice(aux);
    bytes
}

fn jpeg(cases: &mut Vec<Case>) {
    let iso = include_bytes!("../../../fixtures/hdr/iso.jpg").as_slice();
    let adobe = include_bytes!("../../../fixtures/hdr/adobe.jpg").as_slice();
    let ns = b"urn:iso:std:iso:ts:21496:-1\0";
    let (tiff, aux) = jpeg_aux(iso);
    let at = aux + find(&iso[aux..], ns) + ns.len();
    let segment = at - ns.len() - 4;
    let end = segment + 2 + usize::from(u16::from_be_bytes([iso[segment + 2], iso[segment + 3]]));
    for rgb in [false, true] {
        let mut metadata = iso[at..end].to_vec();
        if rgb {
            metadata[4] |= 128;
            let channel = metadata[17..].to_vec();
            metadata.extend_from_slice(&channel);
            metadata.extend_from_slice(&channel);
        }
        for (name, mutated) in iso_mutations(&metadata) {
            let mut gain = iso[aux..segment].to_vec();
            gain.extend_from_slice(&[255, 226]);
            gain.extend_from_slice(&((ns.len() + mutated.len() + 2) as u16).to_be_bytes());
            gain.extend_from_slice(ns);
            gain.extend(mutated);
            gain.extend_from_slice(&iso[end..]);
            push(
                cases,
                format!("jpeg-iso/rgb-{rgb}/{name}"),
                with_aux(iso, &gain),
            );
        }
    }
    // Each MPF header, directory tag/type/count/value, and image-entry field.
    for (field, width) in [(0, 2), (2, 2), (4, 4), (8, 2), (46, 4)]
        .into_iter()
        .chain(
            (10..46)
                .step_by(12)
                .flat_map(|p| [(p, 2), (p + 2, 2), (p + 4, 4), (p + 8, 4)]),
        )
        .chain(
            [50, 66]
                .into_iter()
                .flat_map(|p| [(p, 4), (p + 4, 4), (p + 8, 4)]),
        )
    {
        replace(
            cases,
            iso,
            &format!("jpeg-mpf/field-{field}"),
            tiff + field,
            &vec![255; width],
        );
    }
    // Truncate the MPF payload at each field boundary, keeping primary marker
    // framing intact with an ordinary COM segment of the remaining bytes.
    for size in [0, 2, 4, 8, 10, 22, 34, 46, 50, 66, 82] {
        let start = tiff - 8;
        let old_end = start + 2 + usize::from(u16::from_be_bytes([iso[start + 2], iso[start + 3]]));
        let cut = tiff + size;
        if old_end - cut < 4 {
            continue;
        }
        let mut bytes = iso.to_vec();
        bytes[start + 2..start + 4].copy_from_slice(&((cut - start - 2) as u16).to_be_bytes());
        bytes[cut..cut + 2].copy_from_slice(&[255, 254]);
        bytes[cut + 2..cut + 4].copy_from_slice(&((old_end - cut - 2) as u16).to_be_bytes());
        push(cases, format!("jpeg-mpf/truncate-{size}"), bytes);
    }
    for (name, field, value) in [
        ("missing", tiff - 7, 254),
        ("aux-soi", aux, 0),
        ("aux-eoi", iso.len() - 1, 0),
    ] {
        replace(cases, iso, &format!("jpeg-mpf/{name}"), field, &[value]);
    }
    for end in [aux, aux + 1, at, end - 1, end] {
        push(
            cases,
            format!("jpeg-mpf/truncated-aux-{end}"),
            iso[..end].to_vec(),
        );
    }
    let (_, start) = jpeg_aux(adobe);
    let xmp = start + find(&adobe[start..], b"http://ns.adobe.com/xap/1.0/\0");
    let header = xmp - 4;
    let end = header + 2 + usize::from(u16::from_be_bytes([adobe[header + 2], adobe[header + 3]]));
    for field in [
        "Version",
        "BaseRenditionIsHDR",
        "GainMapMin",
        "GainMapMax",
        "Gamma",
        "OffsetSDR",
        "OffsetHDR",
        "HDRCapacityMin",
        "HDRCapacityMax",
    ] {
        let key = format!("hdrgm:{field}=\"");
        let at = start + find(&adobe[start..], key.as_bytes()) + key.len();
        let len = adobe[at..].iter().position(|b| *b == b'"').unwrap();
        for value in [b"NaN".as_slice(), b"-999", b"", b"1 2", b"inf"] {
            let mut gain = adobe[start..at].to_vec();
            gain.extend_from_slice(value);
            gain.extend_from_slice(&adobe[at + len..]);
            let new_len = (end - header - 2 + value.len() - len) as u16;
            gain[header - start + 2..header - start + 4].copy_from_slice(&new_len.to_be_bytes());
            push(
                cases,
                format!("jpeg-xmp/{field}/{value:?}"),
                with_aux(adobe, &gain),
            );
        }
        // A duplicate property is inconsistent, even if both values are identical.
        let mut gain = adobe[start..=at + len].to_vec();
        gain.push(b' ');
        gain.extend_from_slice(&adobe[at - key.len()..=at + len]);
        gain.extend_from_slice(&adobe[at + len + 1..]);
        let length = end - header - 2 + key.len() + len + 2;
        gain[header - start + 2..header - start + 4]
            .copy_from_slice(&(length as u16).to_be_bytes());
        push(
            cases,
            format!("jpeg-xmp/{field}/duplicate"),
            with_aux(adobe, &gain),
        );
        push(
            cases,
            format!("jpeg-xmp/{field}/truncate"),
            adobe[..at].to_vec(),
        );
    }
    for (name, seed) in [("iso", iso), ("xmp", adobe)] {
        let (_, start) = jpeg_aux(seed);
        for marker in [
            b"urn:iso:std:iso:ts:21496:-1\0".as_slice(),
            b"http://ns.adobe.com/xap/1.0/\0",
        ] {
            if let Some(offset) = seed[start..]
                .windows(marker.len())
                .position(|b| b == marker)
            {
                let at = start + offset - 4;
                for length in [0_u16, 1, u16::MAX] {
                    replace(
                        cases,
                        seed,
                        &format!("jpeg-{name}/marker-length-{length}"),
                        at + 2,
                        &length.to_be_bytes(),
                    );
                }
                replace(
                    cases,
                    seed,
                    &format!("jpeg-{name}/missing-marker"),
                    at + 1,
                    &[254],
                );
            }
        }
    }
    let frame = aux
        + iso[aux..]
            .windows(2)
            .position(|b| matches!(b, [255, 0xc0..=0xc2]))
            .unwrap();
    for (w, h) in [
        (192_u16, 128_u16),
        (48, 16),
        (0, 32),
        (48, 0),
        (u16::MAX, u16::MAX),
    ] {
        let mut bytes = iso.to_vec();
        bytes[frame + 5..frame + 7].copy_from_slice(&h.to_be_bytes());
        bytes[frame + 7..frame + 9].copy_from_slice(&w.to_be_bytes());
        push(cases, format!("jpeg/geometry-{w}x{h}"), bytes);
        cases.last_mut().unwrap().invalid_geometry = true;
    }
}

fn jxl(cases: &mut Vec<Case>) {
    let seed = include_bytes!("../../../fixtures/hdr/gain-map.jxl").as_slice();
    let at = find(seed, b"jhgm") - 4;
    let metadata = at + 11;
    let size = usize::from(u16::from_be_bytes([seed[at + 9], seed[at + 10]]));
    for (name, value) in iso_mutations(&seed[metadata..metadata + size]) {
        let mut bytes = seed[..metadata].to_vec();
        bytes[at + 9..at + 11].copy_from_slice(&(value.len() as u16).to_be_bytes());
        bytes.extend(value);
        bytes.extend_from_slice(&seed[metadata + size..]);
        let len = bytes.len() - at;
        bytes[at..at + 4].copy_from_slice(&(len as u32).to_be_bytes());
        push(cases, format!("jxl/iso/{name}"), bytes);
    }
    for end in at + 1..seed.len() {
        push(cases, format!("jxl/truncate-{end}"), seed[..end].to_vec());
    }
    for field in [
        at + 8,
        at + 9,
        at + 10,
        metadata + size,
        metadata + size + 1,
        metadata + size + 4,
    ] {
        replace(cases, seed, &format!("jxl/field-{field}"), field, &[255]);
    }
    for length in [1, 7, 8, 15, u32::MAX] {
        replace(
            cases,
            seed,
            &format!("jxl/length-{length}"),
            at,
            &length.to_be_bytes(),
        );
    }
    let mut duplicate = seed.to_vec();
    duplicate.extend_from_slice(&seed[at..]);
    push(cases, "jxl/duplicate-jhgm", duplicate);
    // Keep a broken jhgm signature when removing its payload: HDR intent survives.
    let mut empty = seed[..at + 8].to_vec();
    empty[at..at + 4].copy_from_slice(&8_u32.to_be_bytes());
    push(cases, "jxl/missing-payload", empty);
    let chart = include_bytes!("../../../fixtures/images/chart.jxl");
    let mut codestream = Vec::new();
    let mut offset = 0;
    while offset < chart.len() {
        let size = u32_at(chart, offset);
        match &chart[offset + 4..offset + 8] {
            b"jxlc" => codestream.extend_from_slice(&chart[offset + 8..offset + size]),
            b"jxlp" => codestream.extend_from_slice(&chart[offset + 12..offset + size]),
            _ => {}
        }
        offset += size;
    }
    let mut oversized = seed[..metadata + size + 5].to_vec();
    oversized.extend(codestream);
    let length = oversized.len() - at;
    oversized[at..at + 4].copy_from_slice(&(length as u32).to_be_bytes());
    push(cases, "jxl/invalid-geometry", oversized);
    cases.last_mut().unwrap().invalid_geometry = true;
}

fn bmff(cases: &mut Vec<Case>, form: &str, seed: &[u8]) {
    let gain = if form == "avif" { 3_u16 } else { 2 };
    let iref = find(seed, b"iref");
    for field in iref + 4..iref + 8 {
        replace(
            cases,
            seed,
            &format!("{form}/iref/header-{field}"),
            field,
            &[255],
        );
    }
    for length in 0..4 {
        let mut leaf = ((length + 8) as u32).to_be_bytes().to_vec();
        leaf.extend_from_slice(b"iref");
        leaf.extend_from_slice(&seed[iref + 4..iref + 4 + length]);
        push(
            cases,
            format!("{form}/iref/truncate-{length}"),
            edit_box(seed, iref - 4, &leaf),
        );
    }
    for kind in [b"dimg".as_slice(), b"auxl", b"cdsc"] {
        for at in seed
            .windows(4)
            .enumerate()
            .filter_map(|(at, b)| (b == kind).then_some(at))
        {
            corrupt_box(cases, form, seed, at);
            let end = at - 4 + u32_at(seed, at - 4);
            for field in (at + 4..end).step_by(2) {
                for value in [0_u16, 1, gain, 99] {
                    if seed[field..field + 2] == value.to_be_bytes()
                        || kind == b"cdsc" && field == end - 2 && value == 1
                    {
                        continue;
                    }
                    // Primary targets are required; mutate optional owners and
                    // the gain target, and malformed reference counts.
                    replace(
                        cases,
                        seed,
                        &format!("{form}/{kind:?}/field-{field}/{value}"),
                        field,
                        &value.to_be_bytes(),
                    );
                }
            }
            // A duplicate reference box occupies an existing opaque metadata box
            // only in AVIF; insertion is tested by the metadata parser below.
        }
    }
    let infe = seed
        .windows(4)
        .enumerate()
        .filter_map(|(at, b)| (b == b"infe").then_some(at))
        .collect::<Vec<_>>();
    for &at in &infe[1..] {
        // Item counts stay consistent for a short/missing optional infe.
        // Replacing its type with free tests a missing declaration separately.
        for length in [8, 10, 12] {
            let mut leaf = ((length + 8) as u32).to_be_bytes().to_vec();
            leaf.extend_from_slice(b"infe");
            leaf.extend_from_slice(&seed[at + 4..at + 4 + length]);
            push(
                cases,
                format!("{form}/infe-{at}/truncate-{length}"),
                edit_box(seed, at - 4, &leaf),
            );
        }
        for kind in [b"mime".as_slice(), b"zzzz", b"tmap"] {
            if &seed[at + 12..at + 16] != kind {
                replace(
                    cases,
                    seed,
                    &format!("{form}/item-{at}/type-{kind:?}"),
                    at + 12,
                    kind,
                );
            }
        }
        for field in [at + 4, at + 8, at + 10] {
            replace(
                cases,
                seed,
                &format!("{form}/item-{at}/field-{field}"),
                field,
                &[255, 255],
            );
        }
    }
    let first_aux_property = if form == "avif" {
        find(seed, b"colr") + 19
    } else {
        seed.windows(4)
            .enumerate()
            .filter_map(|(at, b)| (b == b"hvcC").then_some(at))
            .nth(1)
            .unwrap()
    };
    for kind in [
        b"ispe".as_slice(),
        b"pixi",
        b"av1C",
        b"hvcC",
        b"colr",
        b"clap",
        b"auxC",
    ] {
        for at in seed
            .windows(4)
            .enumerate()
            .filter_map(|(at, b)| (b == kind && at >= first_aux_property).then_some(at))
        {
            corrupt_box(cases, form, seed, at);
        }
    }
    let iloc = find(seed, b"iloc");
    let count = usize::from(u16::from_be_bytes([seed[iloc + 10], seed[iloc + 11]]));
    for entry in (iloc + 26..iloc + 12 + count * 14).step_by(14) {
        for (field, size) in [(0, 2), (2, 2), (4, 2), (6, 4), (10, 4)] {
            // A malformed declared extent count would consume the next item's
            // header; its bounded parser is tested separately as a hard failure.
            if field == 4 {
                continue;
            }
            for value in [vec![0; size], vec![255; size]] {
                if seed[entry + field..entry + field + size] == value {
                    continue;
                }
                replace(
                    cases,
                    seed,
                    &format!("{form}/iloc-{entry}/field-{field}/{value:?}"),
                    entry + field,
                    &value,
                );
            }
        }
    }
    // Corrupt every auxiliary association independently, and duplicate one
    // referenced property while preserving all primary property associations.
    let ipma = find(seed, b"ipma");
    let count = u32_at(seed, ipma + 8);
    let mut entry = ipma + 12;
    for _ in 0..count {
        let id = u16::from_be_bytes([seed[entry], seed[entry + 1]]);
        let count = usize::from(seed[entry + 2]);
        if id != 1 {
            for field in entry + 3..entry + 3 + count {
                replace(
                    cases,
                    seed,
                    &format!("{form}/ipma/index-{field}"),
                    field,
                    &[127],
                );
            }
            let end = ipma - 4 + u32_at(seed, ipma - 4);
            let mut leaf = seed[ipma - 4..end].to_vec();
            leaf[entry + 2 - (ipma - 4)] += 1;
            leaf.insert(entry + 3 + count - (ipma - 4), seed[entry + 3]);
            let len = leaf.len() as u32;
            leaf[..4].copy_from_slice(&len.to_be_bytes());
            push(
                cases,
                format!("{form}/ipma/duplicate-index-{entry}"),
                edit_box(seed, ipma - 4, &leaf),
            );
        }
        entry += 3 + count;
    }
    if form == "avif" {
        // iloc v0, 4-byte offsets and lengths; item 2 is tmap metadata.
        let iloc = find(seed, b"iloc");
        let entry = iloc + 12 + 14;
        let offset = u32_at(seed, entry + 6);
        let len = u32_at(seed, entry + 10);
        for (name, value) in iso_mutations(&seed[offset + 1..offset + len]) {
            // Truncate via the item's length; other metadata mutations retain
            // the original mdat extent and do not touch either pixel codestream.
            if value.len() > len - 1 {
                continue;
            }
            let mut bytes = seed.to_vec();
            bytes[offset + 1..offset + 1 + value.len()].copy_from_slice(&value);
            bytes[entry + 10..entry + 14]
                .copy_from_slice(&((value.len() + 1) as u32).to_be_bytes());
            push(cases, format!("avif/tmap/{name}"), bytes);
        }
        replace(cases, seed, "avif/tmap/version", offset, &[255]);
        for field in [entry + 6, entry + 10] {
            replace(
                cases,
                seed,
                &format!("avif/tmap/location-{field}"),
                field,
                &u32::MAX.to_be_bytes(),
            );
        }
    } else {
        let note = find(seed, b"Apple iOS\0");
        for field in [0, 12, 14, 16, 18, 20, 24, 28, 30, 32, 36] {
            replace(
                cases,
                seed,
                &format!("heic/makernote/field-{field}"),
                note + field,
                &[255, 255],
            );
        }
        for field in [note + 24, note + 36] {
            for value in [f32::NAN, f32::INFINITY, -1000.0, 1000.0] {
                // Apple's first scalar selects a branch at 1; all finite values
                // on that branch are defined, so extreme values are not corrupt.
                if field == note + 24 && value.is_finite() {
                    continue;
                }
                replace(
                    cases,
                    seed,
                    &format!("heic/makernote/value-{field}-{value}"),
                    field,
                    &value.to_bits().to_be_bytes(),
                );
            }
        }
        let auxc = find(seed, b"auxC");
        for field in [auxc + 4, auxc + 8, auxc + 40, auxc + 47] {
            replace(
                cases,
                seed,
                &format!("heic/auxC/field-{field}"),
                field,
                &[255],
            );
        }
        let version = find(seed, b"65536");
        for field in version..version + 5 {
            replace(cases, seed, &format!("heic/xmp/field-{field}"), field, b"x");
        }
    }
    for kind in [b"iref".as_slice(), b"auxC"] {
        if let Some(at) = seed.windows(4).position(|b| b == kind) {
            replace(
                cases,
                seed,
                &format!("{form}/missing-{kind:?}"),
                at,
                b"free",
            );
        }
    }
    let ispe = seed
        .windows(4)
        .enumerate()
        .filter_map(|(at, b)| (b == b"ispe").then_some(at))
        .next_back()
        .unwrap();
    for (w, h) in [(192_u32, 128_u32), (48, 16)] {
        let mut bytes = seed.to_vec();
        bytes[ispe + 8..ispe + 12].copy_from_slice(&w.to_be_bytes());
        bytes[ispe + 12..ispe + 16].copy_from_slice(&h.to_be_bytes());
        if form == "heic" {
            let crop = find(seed, b"clap");
            bytes[crop + 4..crop + 8].copy_from_slice(&w.to_be_bytes());
            bytes[crop + 12..crop + 16].copy_from_slice(&h.to_be_bytes());
            bytes[crop + 20..crop + 24].fill(0);
            bytes[crop + 28..crop + 32].fill(0);
        }
        push(cases, format!("{form}/geometry-{w}x{h}"), bytes);
        cases.last_mut().unwrap().invalid_geometry = true;
    }
}

pub fn cases() -> Vec<Case> {
    let mut cases = Vec::new();
    jpeg(&mut cases);
    jxl(&mut cases);
    bmff(
        &mut cases,
        "heic",
        include_bytes!("../../../fixtures/hdr/apple.heic"),
    );
    bmff(
        &mut cases,
        "avif",
        include_bytes!("../../../fixtures/images/gain-map.avif"),
    );
    cases
}
