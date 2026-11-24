//! Read external symbols from Windows COFF object files.

use std::collections::BTreeSet;

use rcmake_base::{BString, ByteSlice};

const BIGOBJ_CLASS: &[u8; 16] = b"\xc7\xa1\xba\xd1\xee\xba\xa9\x4b\xaf\x20\xfa\xf6\x6a\xa4\xdc\xb8";
const SECTION_SIZE: usize = 40;
const SECTION_EXECUTE: u32 = 0x2000_0000;
const SECTION_READ: u32 = 0x4000_0000;
const SECTION_WRITE: u32 = 0x8000_0000;

struct Header {
    machine: u16,
    sections: usize,
    section_start: usize,
    symbol_start: usize,
    symbols: usize,
    symbol_size: usize,
}

impl Header {
    fn parse(bytes: &[u8]) -> Option<Self> {
        let first = word(bytes, 0)?;
        let second = word(bytes, 2)?;
        let header = if first == 0 && second == u16::MAX {
            if word(bytes, 4)? < 2 || bytes.get(12..28)? != BIGOBJ_CLASS {
                return None;
            }
            Self {
                machine: word(bytes, 6)?,
                sections: dword(bytes, 44)? as usize,
                section_start: 56,
                symbol_start: dword(bytes, 48)? as usize,
                symbols: dword(bytes, 52)? as usize,
                symbol_size: 20,
            }
        } else {
            // COFF object files have no executable optional header.
            if word(bytes, 16)? != 0 {
                return None;
            }
            Self {
                machine: first,
                sections: second as usize,
                section_start: 20,
                symbol_start: dword(bytes, 8)? as usize,
                symbols: dword(bytes, 12)? as usize,
                symbol_size: 18,
            }
        };
        matches!(
            header.machine,
            0x14c | 0x8664 | 0x1c0 | 0x1c4 | 0xaa64 | 0xa641
        )
        .then_some(header)
    }
}

pub(super) fn exports(
    bytes: &[u8],
    data: &mut BTreeSet<BString>,
    functions: &mut BTreeSet<BString>,
) -> Result<(), &'static str> {
    let header = Header::parse(bytes)
        .ok_or("The file is not a supported COFF object. Use --nm for LLVM bitcode.")?;
    read_symbols(bytes, &header, data, functions).ok_or("The COFF symbol table is invalid.")
}

fn read_symbols(
    bytes: &[u8],
    header: &Header,
    data: &mut BTreeSet<BString>,
    functions: &mut BTreeSet<BString>,
) -> Option<()> {
    let section_bytes = region(bytes, header.section_start, header.sections, SECTION_SIZE)?;
    if header.symbols == 0 {
        return Some(());
    }
    let symbol_bytes = region(
        bytes,
        header.symbol_start,
        header.symbols,
        header.symbol_size,
    )?;
    let strings_start = header.symbol_start.checked_add(symbol_bytes.len())?;
    let strings = region(
        bytes,
        strings_start,
        dword(bytes, strings_start)? as usize,
        1,
    )?;
    let mut entries = symbol_bytes.chunks_exact(header.symbol_size);
    while let Some(entry) = entries.next() {
        let type_offset = header.symbol_size - 4;
        let section = if header.symbol_size == 18 {
            i32::from(word(entry, 12)? as i16)
        } else {
            dword(entry, 12)? as i32
        };
        let kind = word(entry, type_offset)?;
        let external = *entry.get(type_offset + 2)? == 2;
        let aux = usize::from(*entry.get(type_offset + 3)?);
        if aux > entries.len() {
            return None;
        }
        if aux > 0 {
            entries.nth(aux - 1)?;
        }
        if !external || section <= 0 || !matches!(kind, 0 | 0x20) {
            continue;
        }
        let section_header = region(
            section_bytes,
            (section as usize - 1).checked_mul(SECTION_SIZE)?,
            1,
            SECTION_SIZE,
        )?;
        let flags = dword(section_header, 36)?;
        let name = normalize(name(entry, strings)?, header.machine);
        if excluded(name, header.machine) {
            continue;
        }
        let vftable = name.starts_with(b"??_7");
        if (kind == 0 && flags & SECTION_WRITE != 0) || (vftable && header.machine == 0xa641) {
            data.insert(name.into());
        } else if kind != 0 || flags & SECTION_READ == 0 || flags & SECTION_EXECUTE != 0 || vftable
        {
            functions.insert(name.into());
        }
    }
    Some(())
}

fn name<'a>(entry: &'a [u8], strings: &'a [u8]) -> Option<&'a [u8]> {
    if dword(entry, 0)? != 0 {
        let short = entry.get(..8)?;
        return Some(short.split(|byte| *byte == 0).next().unwrap_or_default());
    }
    let offset = dword(entry, 4)? as usize;
    if offset < 4 {
        return None;
    }
    let long = strings.get(offset..)?;
    long.get(..long.iter().position(|&byte| byte == 0)?)
}

fn normalize(name: &[u8], machine: u16) -> &[u8] {
    let mut name = name.trim_ascii_start();
    if name.starts_with(b"_") {
        if let Some(suffix) = name.iter().position(|&byte| byte == b'@') {
            name = name.get(..suffix).unwrap_or_default();
        }
        if machine == 0x14c {
            name = name.strip_prefix(b"_").unwrap_or(name);
        }
    }
    name
}

fn excluded(name: &[u8], machine: u16) -> bool {
    name.is_empty()
        || name.contains(&b'.')
        || name
            .iter()
            .any(|byte| byte.is_ascii_whitespace() || *byte == b';')
        || name.starts_with(b"??_G")
        || name.starts_with(b"??_E")
        || matches!(name, b"__t2m" | b"__m2mep" | b"__mep")
        || name.contains_str(b"$$F")
        || name.contains_str(b"$$J")
        || name
            .iter()
            .all(|byte| b"=<>+-*/%,?|~!^&[]()".contains(byte))
        || (machine == 0xa641
            && [
                b"$entry_thunk".as_slice(),
                b"$ientry_thunk",
                b"$exit_thunk",
                b"$iexit_thunk",
            ]
            .iter()
            .any(|thunk| name.contains_str(thunk)))
}

fn region(bytes: &[u8], base: usize, count: usize, stride: usize) -> Option<&[u8]> {
    bytes.get(base..base.checked_add(count.checked_mul(stride)?)?)
}

fn word(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        region(bytes, offset, 1, 2)?.try_into().ok()?,
    ))
}

fn dword(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        region(bytes, offset, 1, 4)?.try_into().ok()?,
    ))
}
