use std::io::{Read, SeekFrom};

use rcmake_host::ReadSeek;

use super::{Data, Entry, Scan, Stage, StagedEntry, listing_line, stage};
use crate::{ArchiveError, MAX_NAME_BYTES, archive_error};

pub(super) const MAGIC: &[u8] = b"!<arch>\n";

impl Scan<'_> {
    pub(super) fn ar(
        &mut self,
        source: &mut dyn ReadSeek,
        length: u64,
        stage: &mut Stage<'_>,
    ) -> Result<Vec<StagedEntry>, ArchiveError> {
        const HEADER_SIZE: u64 = 60;
        let mut names = Vec::new();
        let mut entries = Vec::new();
        let mut offset = MAGIC.len() as u64;
        source
            .seek(SeekFrom::Start(offset))
            .map_err(archive_error)?;
        while offset < length {
            let mut header = [0; HEADER_SIZE as usize];
            source.read_exact(&mut header).map_err(archive_error)?;
            if header.get(58..).unwrap_or_default() != b"`\n" {
                return Err(invalid("member header"));
            }
            let mut name = header[..16].trim_ascii_end();
            let mut size = number(header.get(48..58).unwrap_or_default(), 10)?;
            offset = offset
                .checked_add(HEADER_SIZE + size + size % 2)
                .filter(|&end| end <= length)
                .ok_or_else(|| invalid("member size"))?;
            let mut bsd_name;
            if let Some(count) = name.strip_prefix(b"#1/") {
                let count = number(count, 10)?;
                if count > MAX_NAME_BYTES as u64 || count > size {
                    return Err(invalid("BSD name length"));
                }
                self.reserve_metadata(count as usize)?;
                bsd_name = vec![0; count as usize];
                source.read_exact(&mut bsd_name).map_err(archive_error)?;
                let end = bsd_name
                    .iter()
                    .rposition(|&byte| byte != 0)
                    .map_or(0, |i| i + 1);
                name = bsd_name.get(..end).unwrap_or_default();
                size -= count;
            } else if !matches!(name, b"/" | b"//" | b"/SYM64/") {
                name = if let Some(index) = name.strip_prefix(b"/") {
                    table_name(&names, number(index, 10)?)?
                } else {
                    name.strip_suffix(b"/").unwrap_or(name)
                };
            }

            self.count_entry(name.len())?;
            self.reserve_payload(size)?;
            let metadata = matches!(
                name,
                b"/" | b"//"
                    | b"/SYM64/"
                    | b"__.SYMDEF"
                    | b"__.SYMDEF SORTED"
                    | b"__.SYMDEF_64"
                    | b"__.SYMDEF_64 SORTED"
            );
            if name == b"//" {
                let count = usize::try_from(size).map_err(|_| invalid("GNU name table size"))?;
                self.reserve_metadata(count)?;
                names.resize(count, 0);
                source.read_exact(&mut names).map_err(archive_error)?;
                self.list_ar_metadata(b"//", &header, 0)?;
            } else if metadata {
                self.list_ar_metadata(name, &header, size)?;
            } else {
                let path = self.path(name)?;
                if self.select(name, &path)? {
                    self.list_ar_member(name, &header, size)?;
                    if !self.list_only {
                        let payload = stage.payload(size, |out| {
                            stage::copy((&mut *source).take(size), out, size)
                        })?;
                        entries.push((
                            Entry {
                                path,
                                data: Data::File(size),
                                mode: Some(
                                    number(header.get(40..48).unwrap_or_default(), 8)? as u32
                                        & 0o7777,
                                ),
                                mtime: if self.touch {
                                    None
                                } else {
                                    Some(time(&header)?)
                                },
                            },
                            Some(payload),
                        ));
                    }
                }
            }
            source
                .seek(SeekFrom::Start(offset))
                .map_err(archive_error)?;
        }
        Ok(entries)
    }

    fn list_ar_metadata(
        &mut self,
        name: &[u8],
        header: &[u8; 60],
        size: u64,
    ) -> Result<(), ArchiveError> {
        // The symbol and string tables have special archive names. They have no
        // destination in the extracted filesystem.
        if self.list_only && self.select(name, &rcmake_host::path::to_native(name))? {
            self.list_ar_member(name, header, size)?;
        }
        Ok(())
    }

    fn list_ar_member(
        &mut self,
        name: &[u8],
        header: &[u8; 60],
        size: u64,
    ) -> Result<(), ArchiveError> {
        if let Some(format_time) = self.listing.time_formatter() {
            let mode =
                0o100000 | (number(header.get(40..48).unwrap_or_default(), 8)? as u32 & 0o7777);
            let owner = format!("{} ", number(header.get(28..34).unwrap_or_default(), 10)?);
            let group = number(header.get(34..40).unwrap_or_default(), 10)?.to_string();
            let line = listing_line(
                mode,
                owner.as_bytes(),
                group.as_bytes(),
                &size.to_string(),
                &format_time(time(header)?),
                name,
            );
            self.reserve_metadata(line.len().saturating_sub(name.len()))?;
            self.names.push(line);
        } else {
            self.names.push(name.into());
        }
        Ok(())
    }
}

fn table_name(table: &[u8], offset: u64) -> Result<&[u8], ArchiveError> {
    let offset = usize::try_from(offset).map_err(|_| invalid("GNU name offset"))?;
    let name = table
        .get(offset..)
        .ok_or_else(|| invalid("GNU name offset"))?;
    if offset != 0 && table.get(offset - 1) != Some(&b'\n') {
        return Err(invalid("GNU name offset"));
    }
    let end = name
        .iter()
        .take(MAX_NAME_BYTES + 2)
        .position(|&byte| byte == b'\n')
        .ok_or_else(|| invalid("GNU name"))?;
    let name = name.get(..end).ok_or_else(|| invalid("GNU name"))?;
    Ok(name.strip_suffix(b"/").unwrap_or(name))
}

fn number(field: &[u8], radix: u32) -> Result<u64, ArchiveError> {
    let field = field.trim_ascii();
    if field.is_empty() {
        return Ok(0);
    }
    std::str::from_utf8(field)
        .ok()
        .and_then(|text| u64::from_str_radix(text, radix).ok())
        .ok_or_else(|| invalid("numeric field"))
}

fn time(header: &[u8; 60]) -> Result<rcmake_host::FileTime, ArchiveError> {
    let secs = i64::try_from(number(header.get(16..28).unwrap_or_default(), 10)?)
        .map_err(|_| invalid("timestamp"))?;
    Ok(rcmake_host::FileTime { secs, nanos: 0 })
}

fn invalid(field: &str) -> ArchiveError {
    ArchiveError::new(format!("Invalid ar archive {field}"))
}
