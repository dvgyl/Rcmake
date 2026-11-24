// SPDX-License-Identifier: MIT OR Apache-2.0

use core::ops::Range;

use super::{BinFormat, BinfmtError, Endian, MAX_ENTRIES, OutputBudget, PeDeps, c_string, span};
use crate::BString;

const LE: Endian = Endian::Little;
const DOS_HEADER_SIZE: u64 = 64;
const COFF_HEADER_SIZE: u64 = 20;
const SECTION_HEADER_SIZE: u64 = 40;
const MAX_SECTIONS: u16 = 96;
const DIRECTORY_SIZE: u64 = 8;
const IMPORT_DIRECTORY: u64 = 1;
const DELAY_IMPORT_DIRECTORY: u64 = 13;
const MACHINE_ITANIUM: u16 = 0x0200;

struct Section<'a> {
    addresses: Range<u64>,
    data: &'a [u8],
}

struct Image<'a> {
    base: u64,
    sections: Vec<Section<'a>>,
}

impl<'a> Image<'a> {
    fn read(
        bytes: &'a [u8],
        base: u64,
        headers_size: u64,
        image_size: u64,
        low_alignment: bool,
        table: &[u8],
    ) -> Result<Self, BinfmtError> {
        let headers = span(bytes, 0, headers_size, "PE image headers")?;
        let mut sections = vec![Section {
            addresses: 0..headers_size,
            data: headers,
        }];
        let field = "PE section";
        for row in table.chunks_exact(SECTION_HEADER_SIZE as usize) {
            let virtual_size = u64::from(LE.u32(row, 8, field)?);
            let start = u64::from(LE.u32(row, 12, field)?);
            let raw_size = u64::from(LE.u32(row, 16, field)?);
            let raw_offset = u64::from(LE.u32(row, 20, field)?);
            let virtual_size = if virtual_size == 0 {
                raw_size
            } else {
                virtual_size
            };
            if virtual_size == 0 {
                continue;
            }
            let end = start + virtual_size;
            if end > image_size {
                return Err(BinfmtError::Invalid("PE section address range"));
            }
            let data = if raw_size == 0 {
                &[]
            } else {
                if raw_offset < headers_size || (low_alignment && raw_offset != start) {
                    return Err(BinfmtError::Invalid("PE section file offset"));
                }
                let raw = span(bytes, raw_offset, raw_size, field)?;
                span(raw, 0, raw_size.min(virtual_size), field)?
            };
            sections.push(Section {
                addresses: start..end,
                data,
            });
        }
        sections.sort_unstable_by_key(|section| section.addresses.start);
        let mut previous_end = 0;
        for section in &sections {
            if section.addresses.start < previous_end {
                return Err(BinfmtError::Invalid("PE overlapping section addresses"));
            }
            previous_end = section.addresses.end;
        }
        Ok(Self { base, sections })
    }

    fn at_rva(&self, rva: u32, field: &'static str) -> Result<&'a [u8], BinfmtError> {
        let rva = u64::from(rva);
        let section = self
            .sections
            .iter()
            .find(|section| section.addresses.contains(&rva))
            .ok_or(BinfmtError::Invalid(field))?;
        let offset = usize::try_from(rva - section.addresses.start)
            .map_err(|_| BinfmtError::Invalid(field))?;
        section
            .data
            .get(offset..)
            .ok_or(BinfmtError::Invalid(field))
    }

    fn imports(
        &self,
        directories: &[u8],
        kind: ImportKind,
        budget: &mut OutputBudget,
    ) -> Result<Vec<BString>, BinfmtError> {
        let (index, stride, name_at, field) = match kind {
            ImportKind::Immediate => (IMPORT_DIRECTORY, 20, 12, "PE import table"),
            ImportKind::Delayed => (DELAY_IMPORT_DIRECTORY, 32, 4, "PE delay-import table"),
        };
        let mut imports = Vec::new();
        let Some(directory) = directory(directories, index, field)? else {
            return Ok(imports);
        };
        let tail = self.at_rva(directory.rva, field)?;
        let table = span(tail, 0, u64::from(directory.size), field)?;
        for (index, row) in table.chunks_exact(stride).enumerate() {
            if row.iter().all(|&byte| byte == 0) {
                return Ok(imports);
            }
            if index == MAX_ENTRIES {
                return Err(BinfmtError::LimitExceeded(field));
            }
            let field = "PE DLL name";
            let address = LE.u32(row, name_at, field)?;
            let rva = match kind {
                ImportKind::Immediate => address,
                ImportKind::Delayed => match LE.u32(row, 0, "PE delay-import attributes")? {
                    0 => self.delay_name_rva(address)?,
                    1 => address,
                    _ => return Err(BinfmtError::Invalid("PE delay-import attributes")),
                },
            };
            if rva == 0 {
                return Err(BinfmtError::Invalid(field));
            }
            let name = c_string(self.at_rva(rva, field)?, field)?;
            if name.is_empty() {
                return Err(BinfmtError::Invalid(field));
            }
            imports.push(budget.copy(name)?);
        }
        Err(BinfmtError::Invalid(field))
    }

    fn delay_name_rva(&self, address: u32) -> Result<u32, BinfmtError> {
        u64::from(address)
            .checked_sub(self.base)
            .and_then(|rva| u32::try_from(rva).ok())
            .ok_or(BinfmtError::Invalid("PE delay-import name address"))
    }
}

enum ImportKind {
    Immediate,
    Delayed,
}

struct Directory {
    rva: u32,
    size: u32,
}

fn directory(
    directories: &[u8],
    index: u64,
    field: &'static str,
) -> Result<Option<Directory>, BinfmtError> {
    let offset = index * DIRECTORY_SIZE;
    if offset >= directories.len() as u64 {
        return Ok(None);
    }
    let entry = span(directories, offset, DIRECTORY_SIZE, field)?;
    let rva = LE.u32(entry, 0, field)?;
    let size = LE.u32(entry, 4, field)?;
    match (rva, size) {
        (0, 0) => Ok(None),
        (0, _) | (_, 0) => Err(BinfmtError::Invalid(field)),
        _ => Ok(Some(Directory { rva, size })),
    }
}

pub(super) fn inspect(bytes: &[u8]) -> Result<Option<PeDeps>, BinfmtError> {
    if (bytes.len() as u64) < DOS_HEADER_SIZE {
        return Ok(None);
    }
    let signature = u64::from(LE.u32(bytes, 0x3c, "PE header offset")?);
    if !matches!(span(bytes, signature, 4, "PE signature"), Ok(b"PE\0\0")) {
        return Ok(None);
    }
    if signature < DOS_HEADER_SIZE {
        return Err(BinfmtError::Invalid("PE header offset"));
    }

    let coff = signature + 4;
    let header = span(bytes, coff, COFF_HEADER_SIZE, "PE COFF header")?;
    let machine = LE.u16(header, 0, "PE machine")?;
    let sections = LE.u16(header, 2, "PE section count")?;
    if sections > MAX_SECTIONS {
        return Err(BinfmtError::LimitExceeded("PE section count"));
    }
    let optional_size = u64::from(LE.u16(header, 16, "PE optional header size")?);
    let optional_start = coff + COFF_HEADER_SIZE;
    let field = "PE optional header";
    let optional = span(bytes, optional_start, optional_size, field)?;
    let (format, base, directories_at) = match LE.u16(optional, 0, field)? {
        0x10b => (BinFormat::Pe32, u64::from(LE.u32(optional, 28, field)?), 96),
        0x20b => (BinFormat::Pe32Plus, LE.u64(optional, 24, field)?, 112),
        _ => return Err(BinfmtError::Invalid("PE optional header magic")),
    };
    let directory_count = u64::from(LE.u32(optional, directories_at - 4, field)?);
    let directories = span(
        optional,
        directories_at as u64,
        directory_count * DIRECTORY_SIZE,
        "PE data directories",
    )?;

    let table_start = optional_start + optional_size;
    let table_size = u64::from(sections) * SECTION_HEADER_SIZE;
    let table = span(bytes, table_start, table_size, "PE section table")?;
    let image_size = u64::from(LE.u32(optional, 56, field)?);
    let headers_size = u64::from(LE.u32(optional, 60, field)?);
    if headers_size < table_start + table_size || headers_size > image_size {
        return Err(BinfmtError::Invalid("PE image headers size"));
    }
    let section_alignment = LE.u32(optional, 32, field)?;
    let file_alignment = LE.u32(optional, 36, field)?;
    let page_size = if machine == MACHINE_ITANIUM {
        8192
    } else {
        4096
    };
    let low_alignment = section_alignment < page_size;
    if file_alignment == 0
        || section_alignment < file_alignment
        || (low_alignment && file_alignment != section_alignment)
    {
        return Err(BinfmtError::Invalid("PE image alignment"));
    }
    let image = Image::read(bytes, base, headers_size, image_size, low_alignment, table)?;
    let mut budget = OutputBudget::default();
    Ok(Some(PeDeps {
        format,
        imports: image.imports(directories, ImportKind::Immediate, &mut budget)?,
        delay_imports: image.imports(directories, ImportKind::Delayed, &mut budget)?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binfmt::{BinaryDeps, inspect_binary_deps, tests::write};

    const PE_AT: usize = 0x80;
    const OPTIONAL_AT: usize = PE_AT + 24;
    const RAW_AT: usize = 0x200;
    const RAW_LEN: usize = 0x200;
    const RVA: u32 = 0x1000;
    const BASE: u32 = 0x400000;

    fn half(bytes: &mut [u8], at: usize, value: u16) {
        write(bytes, at, &value.to_le_bytes());
    }

    fn dword(bytes: &mut [u8], at: usize, value: u32) {
        write(bytes, at, &value.to_le_bytes());
    }

    fn directories_at(plus: bool) -> usize {
        OPTIONAL_AT + if plus { 112 } else { 96 }
    }

    fn set_directory(bytes: &mut [u8], plus: bool, index: u64, rva: u32, size: u32) {
        let at = directories_at(plus) + index as usize * DIRECTORY_SIZE as usize;
        dword(bytes, at, rva);
        dword(bytes, at + 4, size);
    }

    fn fixture(plus: bool) -> Vec<u8> {
        let mut bytes = vec![0; RAW_AT + RAW_LEN];
        write(&mut bytes, 0, b"MZ");
        dword(&mut bytes, 0x3c, PE_AT as u32);
        write(&mut bytes, PE_AT, b"PE\0\0");
        half(&mut bytes, PE_AT + 4, if plus { 0x8664 } else { 0x14c });
        half(&mut bytes, PE_AT + 6, 1);
        let section = directories_at(plus) + 16 * DIRECTORY_SIZE as usize;
        half(&mut bytes, PE_AT + 20, (section - OPTIONAL_AT) as u16);
        half(&mut bytes, OPTIONAL_AT, if plus { 0x20b } else { 0x10b });
        if plus {
            write(
                &mut bytes,
                OPTIONAL_AT + 24,
                &0x1_4000_0000_u64.to_le_bytes(),
            );
        } else {
            dword(&mut bytes, OPTIONAL_AT + 28, BASE);
        }
        dword(&mut bytes, OPTIONAL_AT + 32, 0x1000);
        dword(&mut bytes, OPTIONAL_AT + 36, RAW_AT as u32);
        dword(&mut bytes, OPTIONAL_AT + 56, 0x2000);
        dword(&mut bytes, OPTIONAL_AT + 60, RAW_AT as u32);
        dword(&mut bytes, directories_at(plus) - 4, 16);
        set_directory(&mut bytes, plus, IMPORT_DIRECTORY, RVA, 60);

        write(&mut bytes, section, b".rdata\0\0");
        dword(&mut bytes, section + 8, RAW_LEN as u32);
        dword(&mut bytes, section + 12, RVA);
        dword(&mut bytes, section + 16, RAW_LEN as u32);
        dword(&mut bytes, section + 20, RAW_AT as u32);
        dword(&mut bytes, RAW_AT + 12, RVA + 0x80);
        dword(&mut bytes, RAW_AT + 32, RVA + 0x90);
        write(&mut bytes, RAW_AT + 0x80, b"KERNEL32.dll\0");
        write(&mut bytes, RAW_AT + 0x90, b"user32.DLL\0");
        bytes
    }

    fn expected(format: BinFormat, delay_imports: Vec<BString>) -> BinaryDeps {
        BinaryDeps::Pe(PeDeps {
            format,
            imports: vec!["KERNEL32.dll".into(), "user32.DLL".into()],
            delay_imports,
        })
    }

    #[test]
    fn reads_pe32_and_pe32_plus_imports_in_table_order() {
        for (plus, format) in [(false, BinFormat::Pe32), (true, BinFormat::Pe32Plus)] {
            assert_eq!(
                inspect_binary_deps(&fixture(plus)),
                Ok(Some(expected(format, vec![])))
            );
        }
    }

    #[test]
    fn reads_delay_imports_with_relative_and_absolute_names() {
        for (plus, attrs, base, format) in [
            (true, 1, 0, BinFormat::Pe32Plus),
            (false, 0, BASE, BinFormat::Pe32),
        ] {
            let mut bytes = fixture(plus);
            set_directory(&mut bytes, plus, DELAY_IMPORT_DIRECTORY, RVA + 0x40, 64);
            dword(&mut bytes, RAW_AT + 0x40, attrs);
            dword(&mut bytes, RAW_AT + 0x44, base + RVA + 0xa0);
            write(&mut bytes, RAW_AT + 0xa0, b"delay.dll\0");
            assert_eq!(
                inspect_binary_deps(&bytes),
                Ok(Some(expected(format, vec!["delay.dll".into()])))
            );
        }
    }

    #[test]
    fn accepts_absent_and_zeroed_import_directories() {
        let mut bytes = fixture(false);
        let empty = Ok(Some(BinaryDeps::Pe(PeDeps {
            format: BinFormat::Pe32,
            imports: vec![],
            delay_imports: vec![],
        })));
        dword(&mut bytes, directories_at(false) - 4, 1);
        assert_eq!(inspect_binary_deps(&bytes), empty);

        dword(&mut bytes, directories_at(false) - 4, 16);
        set_directory(&mut bytes, false, IMPORT_DIRECTORY, 0, 0);
        assert_eq!(inspect_binary_deps(&bytes), empty);
    }

    #[test]
    fn requires_both_signatures_and_rejects_truncated_pe_data() {
        let mut bytes = fixture(false);
        for end in PE_AT + 4..bytes.len() {
            assert!(
                bytes
                    .get(..end)
                    .is_some_and(|prefix| inspect_binary_deps(prefix).is_err())
            );
        }
        write(&mut bytes, PE_AT, b"NE\0\0");
        assert_eq!(inspect_binary_deps(&bytes), Ok(None));
        dword(&mut bytes, 0x3c, u32::MAX);
        assert_eq!(inspect_binary_deps(&bytes), Ok(None));
    }

    #[test]
    fn rejects_imports_with_invalid_rvas_or_missing_terminators() {
        let mut bytes = fixture(false);
        set_directory(&mut bytes, false, IMPORT_DIRECTORY, RVA, 40);
        assert_eq!(
            inspect_binary_deps(&bytes),
            Err(BinfmtError::Invalid("PE import table"))
        );

        set_directory(
            &mut bytes,
            false,
            IMPORT_DIRECTORY,
            RVA + RAW_LEN as u32 - 1,
            60,
        );
        assert_eq!(
            inspect_binary_deps(&bytes),
            Err(BinfmtError::Truncated("PE import table"))
        );

        set_directory(&mut bytes, false, IMPORT_DIRECTORY, RVA, 60);
        dword(&mut bytes, RAW_AT + 12, u32::MAX);
        assert_eq!(
            inspect_binary_deps(&bytes),
            Err(BinfmtError::Invalid("PE DLL name"))
        );

        dword(&mut bytes, RAW_AT + 12, RVA + RAW_LEN as u32 - 1);
        write(&mut bytes, RAW_AT + RAW_LEN - 1, b"X");
        assert_eq!(
            inspect_binary_deps(&bytes),
            Err(BinfmtError::Truncated("PE DLL name"))
        );
    }

    #[test]
    fn rejects_names_in_padding_or_zero_filled_memory() {
        let mut bytes = fixture(false);
        let section = directories_at(false) + 16 * DIRECTORY_SIZE as usize;
        dword(&mut bytes, section + 8, 0x80);
        assert_eq!(
            inspect_binary_deps(&bytes),
            Err(BinfmtError::Invalid("PE DLL name"))
        );

        dword(&mut bytes, section + 8, RAW_LEN as u32);
        dword(&mut bytes, section + 16, 0x40);
        assert_eq!(
            inspect_binary_deps(&bytes),
            Err(BinfmtError::Invalid("PE DLL name"))
        );
    }

    #[test]
    fn rejects_sections_that_overlap_or_exceed_the_address_space() {
        let mut bytes = fixture(false);
        let section = directories_at(false) + 16 * DIRECTORY_SIZE as usize;
        dword(&mut bytes, section + 12, 0x100);
        assert_eq!(
            inspect_binary_deps(&bytes),
            Err(BinfmtError::Invalid("PE overlapping section addresses"))
        );

        dword(&mut bytes, section + 12, u32::MAX);
        assert_eq!(
            inspect_binary_deps(&bytes),
            Err(BinfmtError::Invalid("PE section address range"))
        );
    }

    #[test]
    fn rejects_mapped_headers_and_sections_beyond_size_of_image() {
        let mut bytes = fixture(false);
        dword(&mut bytes, OPTIONAL_AT + 56, RAW_AT as u32 - 1);
        assert_eq!(
            inspect_binary_deps(&bytes),
            Err(BinfmtError::Invalid("PE image headers size"))
        );

        for virtual_size in [0, RAW_LEN as u32] {
            let section = directories_at(false) + 16 * DIRECTORY_SIZE as usize;
            dword(&mut bytes, section + 8, virtual_size);
            dword(&mut bytes, OPTIONAL_AT + 56, RVA + RAW_LEN as u32 - 1);
            assert_eq!(
                inspect_binary_deps(&bytes),
                Err(BinfmtError::Invalid("PE section address range"))
            );
        }
    }

    #[test]
    fn low_alignment_requires_equal_alignments_and_matching_section_file_offsets() {
        let mut bytes = fixture(false);
        let raw_rva = RAW_AT as u32;
        dword(&mut bytes, OPTIONAL_AT + 32, raw_rva * 2);
        assert_eq!(
            inspect_binary_deps(&bytes),
            Err(BinfmtError::Invalid("PE image alignment"))
        );

        dword(&mut bytes, OPTIONAL_AT + 32, raw_rva);
        assert_eq!(
            inspect_binary_deps(&bytes),
            Err(BinfmtError::Invalid("PE section file offset"))
        );

        let section = directories_at(false) + 16 * DIRECTORY_SIZE as usize;
        dword(&mut bytes, section + 12, raw_rva);
        set_directory(&mut bytes, false, IMPORT_DIRECTORY, raw_rva, 60);
        dword(&mut bytes, RAW_AT + 12, raw_rva + 0x80);
        dword(&mut bytes, RAW_AT + 32, raw_rva + 0x90);
        assert_eq!(
            inspect_binary_deps(&bytes),
            Ok(Some(expected(BinFormat::Pe32, vec![])))
        );
    }

    #[test]
    fn uses_the_itanium_image_page_size() {
        let mut bytes = fixture(true);
        half(&mut bytes, PE_AT + 4, MACHINE_ITANIUM);
        dword(&mut bytes, OPTIONAL_AT + 36, 0x1000);
        assert_eq!(
            inspect_binary_deps(&bytes),
            Err(BinfmtError::Invalid("PE section file offset"))
        );
    }

    #[test]
    fn rejects_invalid_directory_count_and_delay_import_attributes() {
        let mut bytes = fixture(true);
        dword(&mut bytes, directories_at(true) - 4, 17);
        assert_eq!(
            inspect_binary_deps(&bytes),
            Err(BinfmtError::Truncated("PE data directories"))
        );

        dword(&mut bytes, directories_at(true) - 4, 16);
        set_directory(&mut bytes, true, DELAY_IMPORT_DIRECTORY, RVA + 0x40, 64);
        dword(&mut bytes, RAW_AT + 0x40, 2);
        dword(&mut bytes, RAW_AT + 0x44, RVA + 0xa0);
        assert_eq!(
            inspect_binary_deps(&bytes),
            Err(BinfmtError::Invalid("PE delay-import attributes"))
        );
    }

    #[test]
    fn limits_import_descriptor_count() {
        let mut bytes = fixture(false);
        let table_len = (MAX_ENTRIES + 2) * 20;
        let section_len = table_len + 8;
        bytes.resize(RAW_AT + section_len, 0);
        write(&mut bytes, RAW_AT, &vec![0; table_len]);
        let section = directories_at(false) + 16 * DIRECTORY_SIZE as usize;
        dword(&mut bytes, section + 8, section_len as u32);
        dword(&mut bytes, section + 16, section_len as u32);
        dword(&mut bytes, OPTIONAL_AT + 56, 0x20000);
        set_directory(&mut bytes, false, IMPORT_DIRECTORY, RVA, table_len as u32);
        for index in 0..=MAX_ENTRIES {
            dword(&mut bytes, RAW_AT + index * 20 + 12, RVA + table_len as u32);
        }
        write(&mut bytes, RAW_AT + table_len, b"a.dll\0");
        assert_eq!(
            inspect_binary_deps(&bytes),
            Err(BinfmtError::LimitExceeded("PE import table"))
        );
    }
}
