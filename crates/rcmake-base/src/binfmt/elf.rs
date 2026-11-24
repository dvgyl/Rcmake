// SPDX-License-Identifier: MIT OR Apache-2.0

use core::ops::Range;

use super::{BinFormat, BinfmtError, ElfDeps, Endian, MAX_ENTRIES, OutputBudget, c_string, span};

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const SHT_STRTAB: u32 = 3;
const SHT_DYNAMIC: u32 = 6;

const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;
const DT_STRTAB: u64 = 5;
const DT_STRSZ: u64 = 10;
const DT_SONAME: u64 = 14;
const DT_RPATH: u64 = 15;
const DT_RUNPATH: u64 = 29;

const MAX_SECTIONS: u64 = 65_536;

#[derive(Clone, Copy)]
enum Class {
    Elf32,
    Elf64,
}

#[derive(Clone, Copy)]
struct Elf {
    class: Class,
    endian: Endian,
}

impl Elf {
    fn word_size(self) -> usize {
        match self.class {
            Class::Elf32 => 4,
            Class::Elf64 => 8,
        }
    }

    fn word(self, data: &[u8], offset: usize, field: &'static str) -> Result<u64, BinfmtError> {
        match self.class {
            Class::Elf32 => self.endian.u32(data, offset, field).map(u64::from),
            Class::Elf64 => self.endian.u64(data, offset, field),
        }
    }

    fn address_end(self, start: u64, size: u64, field: &'static str) -> Result<u64, BinfmtError> {
        start
            .checked_add(size)
            .filter(|&end| matches!(self.class, Class::Elf64) || end <= 1_u64 << 32)
            .ok_or(BinfmtError::Invalid(field))
    }

    fn program_headers(self, bytes: &[u8]) -> Result<ProgramHeaders<'_>, BinfmtError> {
        let field = "ELF program header table";
        let (offset, size, minimum_size) = match self.class {
            Class::Elf32 => (28, 42, 32),
            Class::Elf64 => (32, 54, 56),
        };
        let offset = self.word(bytes, offset, field)?;
        let entry_size = self.endian.u16(bytes, size, field)?;
        let count = u64::from(self.endian.u16(bytes, size + 2, field)?);
        let mut headers = ProgramHeaders::default();
        if count == 0 && offset == 0 {
            return Ok(headers);
        }
        if count > MAX_ENTRIES as u64 {
            return Err(BinfmtError::LimitExceeded(field));
        }
        let table = header_table(bytes, offset, entry_size, count, minimum_size, field)?;

        for entry in table.chunks_exact(usize::from(entry_size)) {
            let kind = self.endian.u32(entry, 0, field)?;
            if !matches!(kind, PT_LOAD | PT_DYNAMIC) {
                continue;
            }
            let (file_at, address_at, size_at) = match self.class {
                Class::Elf32 => (4, 8, 16),
                Class::Elf64 => (8, 16, 32),
            };
            let offset = self.word(entry, file_at, field)?;
            let size = self.word(entry, size_at, field)?;
            let address = self.word(entry, address_at, field)?;
            let memory_size = self.word(entry, size_at + self.word_size(), field)?;
            if kind == PT_DYNAMIC {
                let field = "ELF dynamic segment";
                if size != memory_size {
                    return Err(BinfmtError::Invalid(field));
                }
                set_once(&mut headers.dynamic, (address, offset, size), field)?;
                continue;
            }

            let field = "ELF load segment";
            let end = self.address_end(address, memory_size, field)?;
            if size > memory_size {
                return Err(BinfmtError::Invalid(field));
            }
            if memory_size != 0 {
                let data = if size == 0 {
                    &[]
                } else {
                    span(bytes, offset, size, field)?
                };
                headers.loads.push(Load {
                    addresses: address..end,
                    offset,
                    data,
                });
            }
        }
        Ok(headers)
    }

    fn section_dynamic(self, bytes: &[u8]) -> Result<Option<DynamicSection<'_>>, BinfmtError> {
        let field = "ELF section header table";
        let (offset, size, minimum_size, address_at, file_at, size_at, link_at, entry_at) =
            match self.class {
                Class::Elf32 => (32, 46, 40, 12, 16, 20, 24, 36),
                Class::Elf64 => (40, 58, 64, 16, 24, 32, 40, 56),
            };
        let offset = self.word(bytes, offset, field)?;
        let entry_size = self.endian.u16(bytes, size, field)?;
        let mut count = u64::from(self.endian.u16(bytes, size + 2, field)?);
        if count == 0 && offset == 0 {
            return Ok(None);
        }
        if count == 0 {
            let first = header_table(bytes, offset, entry_size, 1, minimum_size, field)?;
            if self.endian.u32(first, 4, field)? != 0 {
                return Err(BinfmtError::Invalid(field));
            }
            count = self.word(first, size_at, field)?;
        }
        if count > MAX_SECTIONS {
            return Err(BinfmtError::LimitExceeded(field));
        }
        let table = header_table(bytes, offset, entry_size, count, minimum_size, field)?;
        let mut dynamic = None;
        for entry in table.chunks_exact(usize::from(entry_size)) {
            if self.endian.u32(entry, 4, field)? == SHT_DYNAMIC {
                set_once(&mut dynamic, entry, "ELF dynamic section")?;
            }
        }
        let Some(entry) = dynamic else {
            return Ok(None);
        };

        let field = "ELF dynamic section";
        if self.word(entry, entry_at, field)? != (self.word_size() * 2) as u64 {
            return Err(BinfmtError::Invalid(field));
        }
        let entries = span(
            bytes,
            self.word(entry, file_at, field)?,
            self.word(entry, size_at, field)?,
            field,
        )?;

        let field = "ELF linked string table";
        let link = u64::from(self.endian.u32(entry, link_at, field)?);
        if link >= count {
            return Err(BinfmtError::Invalid(field));
        }
        let entry_size = u64::from(entry_size);
        let strings = span(table, link * entry_size, entry_size, field)?;
        if self.endian.u32(strings, 4, field)? != SHT_STRTAB {
            return Err(BinfmtError::Invalid(field));
        }
        let string_address = self.word(strings, address_at, field)?;
        let strings = span(
            bytes,
            self.word(strings, file_at, field)?,
            self.word(strings, size_at, field)?,
            field,
        )?;
        Ok(Some(DynamicSection {
            entries,
            string_address,
            strings,
        }))
    }
}

#[derive(Default)]
struct ProgramHeaders<'a> {
    loads: Vec<Load<'a>>,
    dynamic: Option<(u64, u64, u64)>,
}

impl<'a> ProgramHeaders<'a> {
    fn map(
        &self,
        elf: Elf,
        address: u64,
        size: u64,
        field: &'static str,
    ) -> Result<(u64, &'a [u8]), BinfmtError> {
        let invalid = BinfmtError::Invalid(field);
        let end = elf.address_end(address, size, field)?;
        let mut mapped = None;
        for load in &self.loads {
            let range = &load.addresses;
            let overlaps = range.contains(&address) || (address < range.start && range.start < end);
            if !overlaps {
                continue;
            }
            if address < range.start || end > range.end || mapped.is_some() {
                return Err(invalid);
            }
            let displacement = address - range.start;
            let data = span(load.data, displacement, size, field)?;
            let offset = load.offset.checked_add(displacement).ok_or(invalid)?;
            mapped = Some((offset, data));
        }
        mapped.ok_or(invalid)
    }
}

struct Load<'a> {
    addresses: Range<u64>,
    offset: u64,
    data: &'a [u8],
}

struct DynamicSection<'a> {
    entries: &'a [u8],
    string_address: u64,
    strings: &'a [u8],
}

#[derive(Default)]
struct Dynamic {
    strtab: Option<u64>,
    strsz: Option<u64>,
    needed: Vec<u64>,
    rpath: Option<u64>,
    runpath: Option<u64>,
    soname: Option<u64>,
}

impl Dynamic {
    fn read(elf: Elf, entries: &[u8]) -> Result<Self, BinfmtError> {
        let field = "ELF dynamic table";
        let word_size = elf.word_size();
        let entries = entries.chunks_exact(word_size * 2);
        if !entries.remainder().is_empty() {
            return Err(BinfmtError::Truncated(field));
        }
        if entries.len() > MAX_ENTRIES {
            return Err(BinfmtError::LimitExceeded(field));
        }
        let mut dynamic = Self::default();
        for entry in entries {
            let tag = elf.word(entry, 0, field)?;
            let value = elf.word(entry, word_size, field)?;
            match tag {
                DT_NULL => return Ok(dynamic),
                DT_NEEDED => dynamic.needed.push(value),
                DT_STRTAB => set_once(&mut dynamic.strtab, value, "ELF DT_STRTAB")?,
                DT_STRSZ => set_once(&mut dynamic.strsz, value, "ELF DT_STRSZ")?,
                DT_RPATH => set_once(&mut dynamic.rpath, value, "ELF DT_RPATH")?,
                DT_RUNPATH => set_once(&mut dynamic.runpath, value, "ELF DT_RUNPATH")?,
                DT_SONAME => set_once(&mut dynamic.soname, value, "ELF DT_SONAME")?,
                _ => {}
            }
        }
        Err(BinfmtError::Invalid("ELF dynamic table terminator"))
    }

    fn string_table(&self) -> Result<(u64, u64), BinfmtError> {
        let field = "ELF dynamic string table";
        match (self.strtab, self.strsz) {
            (Some(address), Some(size)) => Ok((address, size)),
            _ => Err(BinfmtError::Invalid(field)),
        }
    }

    fn strings_from_section<'a>(
        &self,
        section: DynamicSection<'a>,
    ) -> Result<&'a [u8], BinfmtError> {
        let field = "ELF linked string table";
        let (address, size) = self.string_table()?;
        if address != section.string_address {
            return Err(BinfmtError::Invalid(field));
        }
        span(section.strings, 0, size, field)
    }

    fn collect(self, strings: &[u8], deps: &mut ElfDeps) -> Result<(), BinfmtError> {
        if !strings.is_empty() && (strings.first() != Some(&0) || strings.last() != Some(&0)) {
            return Err(BinfmtError::Invalid("ELF dynamic string table boundaries"));
        }
        let mut budget = OutputBudget::default();
        for offset in self.needed {
            deps.needed.push(budget.copy(string_at(strings, offset)?)?);
        }
        for (offset, output) in [
            (self.rpath, &mut deps.rpaths),
            (self.runpath, &mut deps.runpaths),
        ] {
            if let Some(offset) = offset {
                for path in string_at(strings, offset)?.split(|&byte| byte == b':') {
                    output.push(budget.copy(path)?);
                }
            }
        }
        if let Some(offset) = self.soname {
            deps.soname = Some(budget.copy(string_at(strings, offset)?)?);
        }
        Ok(())
    }
}

fn header_table<'a>(
    bytes: &'a [u8],
    offset: u64,
    entry_size: u16,
    count: u64,
    minimum_size: u16,
    field: &'static str,
) -> Result<&'a [u8], BinfmtError> {
    if offset == 0 || count == 0 || entry_size < minimum_size {
        return Err(BinfmtError::Invalid(field));
    }
    let size = count
        .checked_mul(u64::from(entry_size))
        .ok_or(BinfmtError::Invalid(field))?;
    span(bytes, offset, size, field)
}

fn set_once<T>(slot: &mut Option<T>, value: T, field: &'static str) -> Result<(), BinfmtError> {
    if slot.is_some() {
        return Err(BinfmtError::Invalid(field));
    }
    *slot = Some(value);
    Ok(())
}

fn string_at(strings: &[u8], offset: u64) -> Result<&[u8], BinfmtError> {
    let field = "ELF dynamic string offset";
    let offset = usize::try_from(offset).map_err(|_| BinfmtError::Invalid(field))?;
    let tail = strings
        .get(offset..)
        .filter(|tail| !tail.is_empty())
        .ok_or(BinfmtError::Invalid(field))?;
    c_string(tail, "ELF dynamic string")
}

pub(super) fn inspect(bytes: &[u8]) -> Result<ElfDeps, BinfmtError> {
    let field = "ELF header";
    let (class, format, header_size, size_at) = match bytes.get(4) {
        Some(1) => (Class::Elf32, BinFormat::Elf32, 52, 40),
        Some(2) => (Class::Elf64, BinFormat::Elf64, 64, 52),
        Some(_) => return Err(BinfmtError::Invalid("ELF class")),
        None => return Err(BinfmtError::Truncated(field)),
    };
    let endian = match bytes.get(5) {
        Some(1) => Endian::Little,
        Some(2) => Endian::Big,
        Some(_) => return Err(BinfmtError::Invalid("ELF byte order")),
        None => return Err(BinfmtError::Truncated(field)),
    };
    span(bytes, 0, header_size, field)?;
    if bytes.get(6) != Some(&1) || endian.u32(bytes, 20, field)? != 1 {
        return Err(BinfmtError::Invalid("ELF version"));
    }
    let actual_size = u64::from(endian.u16(bytes, size_at, field)?);
    if actual_size < header_size {
        return Err(BinfmtError::Invalid(field));
    }
    span(bytes, 0, actual_size, field)?;

    let elf = Elf { class, endian };
    let program = elf.program_headers(bytes)?;
    let mut deps = ElfDeps {
        format,
        needed: Vec::new(),
        rpaths: Vec::new(),
        runpaths: Vec::new(),
        soname: None,
    };
    if let Some((address, offset, size)) = program.dynamic {
        let field = "ELF dynamic segment mapping";
        let (mapped_offset, entries) = program.map(elf, address, size, field)?;
        if offset != mapped_offset {
            return Err(BinfmtError::Invalid(field));
        }
        let dynamic = Dynamic::read(elf, entries)?;
        let (address, size) = dynamic.string_table()?;
        let (_, strings) = program.map(elf, address, size, "ELF dynamic string table mapping")?;
        dynamic.collect(strings, &mut deps)?;
    } else if let Some(section) = elf.section_dynamic(bytes)? {
        let dynamic = Dynamic::read(elf, section.entries)?;
        let strings = dynamic.strings_from_section(section)?;
        dynamic.collect(strings, &mut deps)?;
    }
    Ok(deps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binfmt::{BinaryDeps, inspect_binary_deps, tests::write};

    const IMAGE_LEN: usize = 0x400;
    const DYNAMIC_AT: usize = 0x200;
    const STRINGS_AT: usize = 0x300;
    const BASE: u64 = 0x400000;
    const STRINGS: &[u8] =
        b"\0liba.so\0lib-\xff.so\0$ORIGIN/lib::/opt/lib:\0${ORIGIN}/../lib\0fixture.so\0";
    const DYNAMIC: [(u64, u64); 8] = [
        (DT_STRTAB, BASE + STRINGS_AT as u64),
        (DT_STRSZ, STRINGS.len() as u64),
        (DT_NEEDED, 1),
        (DT_NEEDED, 9),
        (DT_RPATH, 18),
        (DT_RUNPATH, 41),
        (DT_SONAME, 58),
        (DT_NULL, 0),
    ];
    const ELF64_LE: Elf = Elf {
        class: Class::Elf64,
        endian: Endian::Little,
    };

    fn half(elf: Elf, bytes: &mut [u8], at: usize, value: u16) {
        let encoded = match elf.endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        };
        write(bytes, at, &encoded);
    }

    fn dword(elf: Elf, bytes: &mut [u8], at: usize, value: u32) {
        let encoded = match elf.endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        };
        write(bytes, at, &encoded);
    }

    fn word(elf: Elf, bytes: &mut [u8], at: usize, value: u64) {
        match (elf.class, elf.endian) {
            (Class::Elf32, _) => dword(elf, bytes, at, value as u32),
            (Class::Elf64, Endian::Little) => write(bytes, at, &value.to_le_bytes()),
            (Class::Elf64, Endian::Big) => write(bytes, at, &value.to_be_bytes()),
        }
    }

    fn fixture(elf: Elf) -> Vec<u8> {
        let mut bytes = vec![0; IMAGE_LEN];
        let (class, header, entry, phoff, ehsize, file_at, address_at, size_at) = match elf.class {
            Class::Elf32 => (1, 52, 32, 28, 40, 4, 8, 16),
            Class::Elf64 => (2, 64, 56, 32, 52, 8, 16, 32),
        };
        let data = match elf.endian {
            Endian::Little => 1,
            Endian::Big => 2,
        };
        write(&mut bytes, 0, b"\x7fELF");
        write(&mut bytes, 4, &[class, data, 1]);
        half(elf, &mut bytes, 16, 3);
        dword(elf, &mut bytes, 20, 1);
        word(elf, &mut bytes, phoff, u64::from(header));
        half(elf, &mut bytes, ehsize, header);
        half(elf, &mut bytes, ehsize + 2, entry);
        half(elf, &mut bytes, ehsize + 4, 2);

        let load = usize::from(header);
        dword(elf, &mut bytes, load, PT_LOAD);
        word(elf, &mut bytes, load + address_at, BASE);
        word(elf, &mut bytes, load + size_at, IMAGE_LEN as u64);
        word(
            elf,
            &mut bytes,
            load + size_at + elf.word_size(),
            IMAGE_LEN as u64,
        );

        let dynamic = load + usize::from(entry);
        let dynamic_size = (DYNAMIC.len() * elf.word_size() * 2) as u64;
        dword(elf, &mut bytes, dynamic, PT_DYNAMIC);
        word(elf, &mut bytes, dynamic + file_at, DYNAMIC_AT as u64);
        word(
            elf,
            &mut bytes,
            dynamic + address_at,
            BASE + DYNAMIC_AT as u64,
        );
        word(elf, &mut bytes, dynamic + size_at, dynamic_size);
        word(
            elf,
            &mut bytes,
            dynamic + size_at + elf.word_size(),
            dynamic_size,
        );

        for (index, (tag, value)) in DYNAMIC.into_iter().enumerate() {
            let start = DYNAMIC_AT + index * elf.word_size() * 2;
            word(elf, &mut bytes, start, tag);
            word(elf, &mut bytes, start + elf.word_size(), value);
        }
        write(&mut bytes, STRINGS_AT, STRINGS);
        bytes
    }

    fn expected(format: BinFormat) -> BinaryDeps {
        BinaryDeps::Elf(ElfDeps {
            format,
            needed: vec![
                b"liba.so".as_slice().into(),
                b"lib-\xff.so".as_slice().into(),
            ],
            rpaths: vec![
                "$ORIGIN/lib".into(),
                "".into(),
                "/opt/lib".into(),
                "".into(),
            ],
            runpaths: vec!["${ORIGIN}/../lib".into()],
            soname: Some("fixture.so".into()),
        })
    }

    #[test]
    fn reads_all_elf_widths_and_byte_orders() {
        for endian in [Endian::Little, Endian::Big] {
            for (class, format) in [
                (Class::Elf32, BinFormat::Elf32),
                (Class::Elf64, BinFormat::Elf64),
            ] {
                let bytes = fixture(Elf { class, endian });
                assert_eq!(inspect_binary_deps(&bytes), Ok(Some(expected(format))));
            }
        }
    }

    #[test]
    fn maps_string_table_through_a_nonzero_segment_file_offset() {
        let elf = ELF64_LE;
        let mut bytes = fixture(elf);
        let load = 64;
        let offset = DYNAMIC_AT as u64 - 16;
        let length = STRINGS_AT as u64 + STRINGS.len() as u64 - offset;
        word(elf, &mut bytes, load + 8, offset);
        word(elf, &mut bytes, load + 16, BASE + offset);
        word(elf, &mut bytes, load + 32, length);
        word(elf, &mut bytes, load + 40, length);
        assert_eq!(
            inspect_binary_deps(&bytes),
            Ok(Some(expected(BinFormat::Elf64)))
        );
    }

    #[test]
    fn reads_dynamic_section_and_linked_strings_without_program_headers() {
        let elf = ELF64_LE;
        let mut bytes = fixture(elf);
        let sections = IMAGE_LEN;
        bytes.resize(sections + 3 * 64, 0);
        word(elf, &mut bytes, 32, 0);
        half(elf, &mut bytes, 56, 0);
        word(elf, &mut bytes, 40, sections as u64);
        half(elf, &mut bytes, 58, 64);
        half(elf, &mut bytes, 60, 3);

        let dynamic = sections + 64;
        dword(elf, &mut bytes, dynamic + 4, SHT_DYNAMIC);
        word(elf, &mut bytes, dynamic + 24, DYNAMIC_AT as u64);
        word(elf, &mut bytes, dynamic + 32, (DYNAMIC.len() * 16) as u64);
        dword(elf, &mut bytes, dynamic + 40, 2);
        word(elf, &mut bytes, dynamic + 56, 16);

        let strings = sections + 128;
        dword(elf, &mut bytes, strings + 4, SHT_STRTAB);
        word(elf, &mut bytes, strings + 16, BASE + STRINGS_AT as u64);
        word(elf, &mut bytes, strings + 24, STRINGS_AT as u64);
        word(elf, &mut bytes, strings + 32, STRINGS.len() as u64);
        assert_eq!(
            inspect_binary_deps(&bytes),
            Ok(Some(expected(BinFormat::Elf64)))
        );

        dword(elf, &mut bytes, dynamic + 40, 3);
        assert_eq!(
            inspect_binary_deps(&bytes),
            Err(BinfmtError::Invalid("ELF linked string table"))
        );
    }

    #[test]
    fn returns_empty_metadata_for_static_image() {
        let mut bytes = fixture(ELF64_LE);
        half(ELF64_LE, &mut bytes, 56, 1);
        assert_eq!(
            inspect_binary_deps(&bytes),
            Ok(Some(BinaryDeps::Elf(ElfDeps {
                format: BinFormat::Elf64,
                needed: vec![],
                rpaths: vec![],
                runpaths: vec![],
                soname: None,
            })))
        );
    }

    #[test]
    fn rejects_truncated_images_and_non_nul_string_table_boundaries() {
        let elf = ELF64_LE;
        let mut bytes = fixture(elf);
        for end in 4..bytes.len() {
            assert!(
                bytes
                    .get(..end)
                    .is_some_and(|prefix| inspect_binary_deps(prefix).is_err())
            );
        }
        word(elf, &mut bytes, DYNAMIC_AT + 24, 8);
        assert_eq!(
            inspect_binary_deps(&bytes),
            Err(BinfmtError::Invalid("ELF dynamic string table boundaries"))
        );
        let mut bytes = fixture(elf);
        word(elf, &mut bytes, DYNAMIC_AT + 24, STRINGS.len() as u64 + 1);
        write(&mut bytes, STRINGS_AT + STRINGS.len(), b"x");
        assert_eq!(
            inspect_binary_deps(&bytes),
            Err(BinfmtError::Invalid("ELF dynamic string table boundaries"))
        );
        let mut bytes = fixture(elf);
        write(&mut bytes, STRINGS_AT, b"x");
        assert_eq!(
            inspect_binary_deps(&bytes),
            Err(BinfmtError::Invalid("ELF dynamic string table boundaries"))
        );
    }

    #[test]
    fn requires_both_dynamic_string_table_tags_without_string_references() {
        let elf = ELF64_LE;
        for (first, second) in [(0, 0), (DT_STRTAB, 0), (4, DT_STRSZ)] {
            let mut bytes = fixture(elf);
            word(elf, &mut bytes, DYNAMIC_AT, first);
            word(elf, &mut bytes, DYNAMIC_AT + 16, second);
            word(elf, &mut bytes, DYNAMIC_AT + 32, DT_NULL);
            assert_eq!(
                inspect_binary_deps(&bytes),
                Err(BinfmtError::Invalid("ELF dynamic string table"))
            );
        }
    }

    #[test]
    fn rejects_dynamic_segments_that_disagree_with_the_load_mapping() {
        let elf = ELF64_LE;
        let dynamic = 64 + 56;
        let size = (DYNAMIC.len() * 16) as u64;
        for (field, value, error) in [
            (8, DYNAMIC_AT as u64 + 16, "ELF dynamic segment mapping"),
            (16, BASE + IMAGE_LEN as u64, "ELF dynamic segment mapping"),
            (40, size - 16, "ELF dynamic segment"),
            (40, size + 16, "ELF dynamic segment"),
        ] {
            let mut bytes = fixture(elf);
            word(elf, &mut bytes, dynamic + field, value);
            assert_eq!(
                inspect_binary_deps(&bytes),
                Err(BinfmtError::Invalid(error))
            );
        }

        let mut bytes = fixture(elf);
        word(elf, &mut bytes, 64 + 32, DYNAMIC_AT as u64 + size - 1);
        assert_eq!(
            inspect_binary_deps(&bytes),
            Err(BinfmtError::Truncated("ELF dynamic segment mapping"))
        );
    }

    #[test]
    fn rejects_string_tables_that_overlap_or_extend_beyond_load_segments() {
        let elf = ELF64_LE;
        let mut bytes = fixture(elf);
        half(elf, &mut bytes, 56, 3);
        let load = 64 + 56 * 2;
        dword(elf, &mut bytes, load, PT_LOAD);
        word(elf, &mut bytes, load + 8, STRINGS_AT as u64 + 1);
        word(elf, &mut bytes, load + 16, BASE + STRINGS_AT as u64 + 1);
        word(elf, &mut bytes, load + 32, 3);
        word(elf, &mut bytes, load + 40, 3);
        let error = Err(BinfmtError::Invalid("ELF dynamic string table mapping"));
        assert_eq!(inspect_binary_deps(&bytes), error);

        half(elf, &mut bytes, 56, 2);
        word(elf, &mut bytes, 64 + 32, STRINGS_AT as u64 + 4);
        assert!(inspect_binary_deps(&bytes).is_err());
    }

    #[test]
    fn rejects_invalid_dynamic_tables_and_excess_entries() {
        let elf = ELF64_LE;
        let mut bytes = fixture(elf);
        word(elf, &mut bytes, DYNAMIC_AT + (DYNAMIC.len() - 1) * 16, 4);
        assert_eq!(
            inspect_binary_deps(&bytes),
            Err(BinfmtError::Invalid("ELF dynamic table terminator"))
        );

        let length = (MAX_ENTRIES + 1) * 16;
        let image_size = DYNAMIC_AT + length;
        bytes.resize(image_size, 0);
        for offset in [32, 40] {
            word(elf, &mut bytes, 64 + offset, image_size as u64);
            word(elf, &mut bytes, 64 + 56 + offset, length as u64);
        }
        assert_eq!(
            inspect_binary_deps(&bytes),
            Err(BinfmtError::LimitExceeded("ELF dynamic table"))
        );
    }

    #[test]
    fn rejects_load_addresses_that_overflow() {
        let mut bytes = fixture(ELF64_LE);
        word(ELF64_LE, &mut bytes, 64 + 16, u64::MAX);
        assert_eq!(
            inspect_binary_deps(&bytes),
            Err(BinfmtError::Invalid("ELF load segment"))
        );
    }
}
