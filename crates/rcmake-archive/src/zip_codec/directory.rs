use std::io::SeekFrom;

use rcmake_host::ReadSeek;

use super::{
    CENTRAL, CENTRAL_HEADER_SIZE, Directory, END, END_RECORD_SIZE, ZIP64_END, ZIP64_LOCATOR, dword,
    entry_limit, invalid_header, qword, word,
};
use crate::{ArchiveError, MAX_ENTRIES, archive_error};

pub(crate) fn open(
    input: &mut dyn ReadSeek,
    len: u64,
    limit: u64,
) -> Result<Directory, ArchiveError> {
    let start = len.saturating_sub(END_RECORD_SIZE as u64 + u64::from(u16::MAX));
    input.seek(SeekFrom::Start(start)).map_err(archive_error)?;
    let mut tail = vec![0; (len - start) as usize];
    input.read_exact(&mut tail).map_err(archive_error)?;
    let mut selected = None;
    let mut error = None;
    for (offset, record) in tail.windows(END_RECORD_SIZE).enumerate().rev() {
        let position = start + offset as u64;
        if selected.as_ref().is_some_and(|(selected, _)| {
            position + END_RECORD_SIZE as u64 + u64::from(u16::MAX) <= *selected
        }) {
            break;
        }
        let Some(fields) = record.strip_prefix(END) else {
            continue;
        };
        let comment_start = position + END_RECORD_SIZE as u64;
        let comment_end = comment_start + u64::from(word(fields, 16)?);
        if comment_end > len
            || selected
                .as_ref()
                .is_some_and(|(selected, _)| !(comment_start..comment_end).contains(selected))
        {
            continue;
        }
        match directory(input, position, fields, limit) {
            Ok(directory) => selected = Some((position, directory)),
            Err(failure) => {
                error.get_or_insert(failure);
            }
        }
    }
    selected
        .map(|(_, directory)| directory)
        .ok_or_else(|| error.unwrap_or_else(invalid_header))
}

fn directory(
    input: &mut dyn ReadSeek,
    position: u64,
    fields: &[u8],
    limit: u64,
) -> Result<Directory, ArchiveError> {
    let locator = if (word(fields, 6)? == u16::MAX || dword(fields, 12)? == u32::MAX)
        && let Some(offset) = position.checked_sub(20)
        && let Ok(bytes) = read_at::<20>(input, offset)
        && bytes.starts_with(ZIP64_LOCATOR)
    {
        Some((offset, bytes))
    } else {
        None
    };
    let layout = if let Some((offset, bytes)) = locator {
        zip64(input, offset, bytes.get(4..).ok_or_else(invalid_header)?)?
    } else {
        zip32(input, position, fields)?
    };
    let end = layout
        .start
        .checked_add(layout.size)
        .filter(|&end| end <= layout.footer)
        .ok_or_else(invalid_header)?;
    if layout.count > MAX_ENTRIES as u64
        || layout.size > crate::extract::stage::METADATA_BYTES as u64
    {
        return Err(entry_limit());
    }
    if end + 4 <= layout.footer && &read_at::<4>(input, end)? == CENTRAL {
        return Err(invalid_header());
    }
    input
        .seek(SeekFrom::Start(layout.start))
        .map_err(archive_error)?;
    let mut records = vec![0; layout.size as usize];
    for chunk in records.chunks_mut(64 * 1024) {
        input.read_exact(chunk).map_err(archive_error)?;
    }
    let mut directory = Directory {
        records,
        bytes_left: limit,
        count: layout.count as usize,
        offset: layout.offset,
        local_end: layout.start,
    };
    directory.bytes_left = directory.preflight(input, limit)?;
    Ok(directory)
}

struct Layout {
    start: u64,
    size: u64,
    offset: u64,
    count: u64,
    footer: u64,
}

fn zip32(input: &mut dyn ReadSeek, footer: u64, fields: &[u8]) -> Result<Layout, ArchiveError> {
    let count = word(fields, 6)?;
    if word(fields, 0)? != 0 || word(fields, 2)? != 0 || word(fields, 4)? != count {
        return Err(invalid_header());
    }
    let relative = u64::from(dword(fields, 12)?);
    let size = u64::from(dword(fields, 8)?);
    let end = footer
        .checked_sub(size)
        .filter(|&end| end >= relative)
        .ok_or_else(invalid_header)?;
    if size < u64::from(count) * CENTRAL_HEADER_SIZE as u64 {
        return Err(invalid_header());
    }
    let start = if count == 0 && size == 0 {
        footer
    } else {
        magic(input, relative, end + 4, CENTRAL)?.ok_or_else(invalid_header)?
    };
    Ok(Layout {
        start,
        size,
        offset: start - relative,
        count: u64::from(count),
        footer,
    })
}

fn zip64(input: &mut dyn ReadSeek, locator: u64, fields: &[u8]) -> Result<Layout, ArchiveError> {
    let relative = qword(fields, 4)?;
    if dword(fields, 0)? != 0 || dword(fields, 12)? > 1 {
        return Err(invalid_header());
    }
    let mut start = relative;
    while let Some(position) = magic(input, start, locator, ZIP64_END)? {
        start = position + 1;
        if position + 56 > locator {
            break;
        }
        let tail = read_at::<52>(input, position + 4)?;
        let size = qword(&tail, 0)?;
        if size < 44
            || size.saturating_add(12) > locator - position
            || dword(&tail, 12)? != 0
            || dword(&tail, 16)? != 0
            || word(&tail, 10)? > word(&tail, 8)?
        {
            continue;
        }
        let count = qword(&tail, 28)?;
        if qword(&tail, 20)? != count {
            continue;
        }
        let offset = position - relative;
        return Ok(Layout {
            start: qword(&tail, 44)?
                .checked_add(offset)
                .ok_or_else(invalid_header)?,
            size: qword(&tail, 36)?,
            offset,
            count,
            footer: position,
        });
    }
    Err(invalid_header())
}

pub(super) fn read_at<const N: usize>(
    input: &mut dyn ReadSeek,
    position: u64,
) -> Result<[u8; N], ArchiveError> {
    input
        .seek(SeekFrom::Start(position))
        .map_err(archive_error)?;
    let mut bytes = [0; N];
    input.read_exact(&mut bytes).map_err(archive_error)?;
    Ok(bytes)
}

fn magic(
    input: &mut dyn ReadSeek,
    mut start: u64,
    end: u64,
    signature: &[u8; 4],
) -> Result<Option<u64>, ArchiveError> {
    let mut bytes = vec![0; 64 * 1024];
    while end.saturating_sub(start) >= 4 {
        let len = (end - start).min(bytes.len() as u64) as usize;
        input.seek(SeekFrom::Start(start)).map_err(archive_error)?;
        let chunk = bytes.get_mut(..len).ok_or_else(invalid_header)?;
        input.read_exact(chunk).map_err(archive_error)?;
        if let Some(hit) = chunk.windows(4).position(|window| window == signature) {
            return Ok(Some(start + hit as u64));
        }
        start += len as u64 - 3;
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn end_record_search_stays_within_the_maximum_comment_length() {
        let mut zip = END.to_vec();
        zip.resize(END_RECORD_SIZE - 2, 0);
        zip.extend(u16::MAX.to_le_bytes());
        zip.resize(END_RECORD_SIZE + usize::from(u16::MAX), 0);
        assert!(open(&mut Cursor::new(&zip), zip.len() as u64, 0).is_ok());
        zip.push(0);
        let mut input = Cursor::new(&zip);
        assert!(open(&mut input, zip.len() as u64, 0).is_err());
    }
}
