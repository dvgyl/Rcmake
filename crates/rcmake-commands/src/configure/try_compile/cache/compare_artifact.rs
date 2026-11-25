// SPDX-License-Identifier: MIT OR Apache-2.0

use super::Artifact;

pub(super) fn matches(left: Option<&Artifact>, right: Option<&Artifact>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => {
            left.relative_path == right.relative_path
                && left.mode == right.mode
                && (left.bytes == right.bytes
                    || normalized(&left.bytes)
                        .zip(normalized(&right.bytes))
                        .is_some_and(|(left, right)| left == right))
        }
        (None, None) => true,
        _ => false,
    }
}

fn normalized(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.starts_with(b"!<arch>\n") {
        normalized_archive(bytes)
    } else if bytes.starts_with(b"MZ") {
        normalized_pe(bytes)
    } else {
        None
    }
}

fn normalized_archive(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut result = bytes.to_vec();
    let mut offset = 8_usize;
    while offset < result.len() {
        let header = result.get_mut(offset..offset.checked_add(60)?)?;
        if header.get(58..)? != b"`\n" {
            return None;
        }
        let size = std::str::from_utf8(header.get(48..58)?)
            .ok()?
            .trim()
            .parse::<usize>()
            .ok()?;
        header.get_mut(16..48)?.fill(0);
        offset = offset
            .checked_add(60)?
            .checked_add(size)?
            .checked_add(size % 2)?;
    }
    (offset == result.len()).then_some(result)
}

fn normalized_pe(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut result = bytes.to_vec();
    let pe = usize::try_from(u32_at(bytes, 0x3c)?).ok()?;
    if bytes.get(pe..pe.checked_add(4)?)? != b"PE\0\0" {
        return None;
    }
    let coff = pe.checked_add(4)?;
    let optional = coff.checked_add(20)?;
    let section_count = usize::from(u16_at(bytes, coff.checked_add(2)?)?);
    let optional_size = usize::from(u16_at(bytes, coff.checked_add(16)?)?);
    let (directories, count_offset) = match u16_at(bytes, optional)? {
        0x10b => (96, 92),
        0x20b => (112, 108),
        _ => return None,
    };
    if optional_size < directories {
        return None;
    }
    result
        .get_mut(coff.checked_add(4)?..coff.checked_add(8)?)?
        .fill(0);
    let count = u32_at(bytes, optional.checked_add(count_offset)?)?;
    if count <= 6 {
        return Some(result);
    }
    let debug_offset = directories + 6 * 8;
    if optional_size < debug_offset + 8 {
        return None;
    }
    let debug = optional.checked_add(debug_offset)?;
    let address = u32_at(bytes, debug)?;
    let size = usize::try_from(u32_at(bytes, debug.checked_add(4)?)?).ok()?;
    if size == 0 {
        return Some(result);
    }
    let table = optional.checked_add(optional_size)?;
    let sections = bytes.get(table..table.checked_add(section_count.checked_mul(40)?)?)?;
    let (sections, _) = sections.as_chunks::<40>();
    let offset = sections.iter().find_map(|section| {
        let virtual_address = u32_at(section, 12)?;
        let raw_size = u32_at(section, 16)?;
        let delta = address.checked_sub(virtual_address)?;
        let position = u32_at(section, 20)?.checked_add(delta)?;
        (usize::try_from(raw_size.checked_sub(delta)?).ok()? >= size)
            .then(|| usize::try_from(position).ok())
            .flatten()
    })?;
    let (records, rest) = bytes
        .get(offset..offset.checked_add(size)?)?
        .as_chunks::<28>();
    if !rest.is_empty() {
        return None;
    }
    for record in records {
        if u32_at(record, 12)? != 2 {
            continue;
        }
        let size = usize::try_from(u32_at(record, 16)?).ok()?;
        let offset = usize::try_from(u32_at(record, 24)?).ok()?;
        let debug = result.get_mut(offset..offset.checked_add(size)?)?;
        if debug.starts_with(b"RSDS") {
            debug.get_mut(4..20)?.fill(0);
        }
    }
    Some(result)
}

fn u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}
