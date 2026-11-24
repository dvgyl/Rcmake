//! Check the dependency tables of executable and shared library images.

use rcmake_base::{
    BString,
    binfmt::{BinFormat, BinaryDeps, BinfmtError, inspect_binary_deps},
};

use super::{Check, mutate, pick, preview, random_bytes, require};
use crate::Rng;

const MAX_ENTRIES: usize = 4_096;
const MAX_NAME: usize = 64 * 1024;
const MAX_OUTPUT: usize = 1024 * 1024;

pub(super) fn check(rng: &mut Rng) -> Check {
    let mode = rng.below(48);
    if mode == 0 {
        let mut input = pick(rng, &[b"".as_slice(), b"\x7fELF\x02\x01\x01", b"MZ"]).to_vec();
        input.extend(random_bytes(rng));
        return inspect(&input, None, false);
    }

    let format = *pick(
        rng,
        &[
            BinFormat::Elf32,
            BinFormat::Elf64,
            BinFormat::Pe32,
            BinFormat::Pe32Plus,
        ],
    );
    // ELF uses three entries for the string table address, size, and terminator.
    let capacity = if matches!(format, BinFormat::Elf32 | BinFormat::Elf64) {
        MAX_ENTRIES - 3
    } else {
        MAX_ENTRIES
    };
    let (count, name_len) = match mode {
        1 => (capacity, 1),
        2 => (capacity + 1, 1),
        3 => (1, MAX_NAME),
        4 => (1, MAX_NAME + 1),
        5 => (MAX_OUTPUT / MAX_NAME, MAX_NAME),
        6 => (MAX_OUTPUT / MAX_NAME + 1, MAX_NAME),
        _ => (1 + rng.below(8) as usize, 1 + rng.below(32) as usize),
    };
    let mut name = rng.bytes(name_len);
    for byte in &mut name {
        if *byte == 0 {
            *byte = b'x';
        }
    }
    let mut input = match format {
        BinFormat::Elf32 | BinFormat::Elf64 => elf_image(
            format == BinFormat::Elf64,
            rng.below(2) == 0,
            rng.below(2) == 0,
            count,
            &name,
        ),
        _ => pe_image(format == BinFormat::Pe32Plus, rng.below(3), count, &name),
    };
    match mode {
        2 | 4 | 6 => inspect(&input, None, true),
        7..=14 => {
            input.truncate(rng.below(input.len() as u64) as usize);
            inspect(&input, None, false)
        }
        15..=33 => {
            mutate(rng, &mut input, b"\xff\xff\xff\xff\0\0\0\0");
            inspect(&input, None, false)
        }
        _ => inspect(&input, Some((format, count, &name)), false),
    }
}

fn inspect(input: &[u8], expected: Option<(BinFormat, usize, &[u8])>, over_limit: bool) -> Check {
    let result = inspect_binary_deps(input);
    if over_limit {
        require(matches!(result, Err(BinfmtError::LimitExceeded(_))), || {
            format!("Expected a binary format limit error. Got {result:?}.")
        })?;
    }
    let Ok(Some(deps)) = result else {
        require(expected.is_none(), || {
            format!("Parser rejected a valid image. Got {result:?}.")
        })?;
        return Ok(false);
    };
    let names: Vec<&BString> = match &deps {
        BinaryDeps::Elf(elf) => elf
            .needed
            .iter()
            .chain(&elf.rpaths)
            .chain(&elf.runpaths)
            .chain(&elf.soname)
            .collect(),
        BinaryDeps::Pe(pe) => pe.imports.iter().chain(&pe.delay_imports).collect(),
    };
    require(
        names.len() <= MAX_ENTRIES
            && names
                .iter()
                .all(|name| name.len() <= MAX_NAME && !name.contains(&0))
            && names.iter().map(|name| name.len()).sum::<usize>() <= MAX_OUTPUT,
        || {
            format!(
                "Dependency metadata exceeds its limits. Input {}.",
                preview(input)
            )
        },
    )?;
    if let Some((format, count, name)) = expected {
        require(
            deps.format() == format
                && names.len() == count
                && names.iter().all(|actual| actual.as_slice() == name),
            || "Parser changed the format or dependency names of a valid image.".into(),
        )?;
    }
    Ok(true)
}

fn elf_image(wide: bool, big: bool, sections: bool, count: usize, name: &[u8]) -> Vec<u8> {
    const BASE: usize = 0x400000;

    let (header, word, header_size_at) = if wide { (64, 8, 52) } else { (52, 4, 40) };
    let (rows, row_size) = match (sections, wide) {
        (true, true) => (3, 64),
        (true, false) => (3, 40),
        (false, true) => (2, 56),
        (false, false) => (2, 32),
    };
    let dynamic = header + rows * row_size;
    let strings = dynamic + (count + 3) * 2 * word;
    let strings_size = name.len() + 2;
    let size = strings + strings_size;
    let mut image = vec![0; strings];
    image.push(0);
    image.extend_from_slice(name);
    image.push(0);
    put(&mut image, 0, b"\x7fELF");
    put(
        &mut image,
        4,
        &[if wide { 2 } else { 1 }, if big { 2 } else { 1 }, 1],
    );
    let number =
        |image: &mut [u8], offset, value, width| put_number(image, offset, value, width, big);
    number(&mut image, 16, 3, 2); // Shared object.
    number(&mut image, 20, 1, 4); // ELF version.
    number(&mut image, header_size_at, header, 2);
    if sections {
        number(&mut image, if wide { 40 } else { 32 }, header, word);
        number(&mut image, header_size_at + 6, row_size, 2);
        number(&mut image, header_size_at + 8, rows, 2);
        let dynamic_row = header + row_size;
        let string_row = dynamic_row + row_size;
        let (address_at, file_at, size_at, link_at, entry_at) = if wide {
            (16, 24, 32, 40, 56)
        } else {
            (12, 16, 20, 24, 36)
        };
        number(&mut image, dynamic_row + 4, 6, 4); // SHT_DYNAMIC.
        number(&mut image, dynamic_row + file_at, dynamic, word);
        number(&mut image, dynamic_row + size_at, strings - dynamic, word);
        number(&mut image, dynamic_row + link_at, 2, 4);
        number(&mut image, dynamic_row + entry_at, 2 * word, word);
        number(&mut image, string_row + 4, 3, 4); // SHT_STRTAB.
        number(&mut image, string_row + address_at, BASE + strings, word);
        number(&mut image, string_row + file_at, strings, word);
        number(&mut image, string_row + size_at, strings_size, word);
    } else {
        number(&mut image, if wide { 32 } else { 28 }, header, word);
        number(&mut image, header_size_at + 2, row_size, 2);
        number(&mut image, header_size_at + 4, rows, 2);
        let (file_at, address_at, size_at) = if wide { (8, 16, 32) } else { (4, 8, 16) };
        number(&mut image, header, 1, 4); // PT_LOAD.
        number(&mut image, header + address_at, BASE, word);
        number(&mut image, header + size_at, size, word);
        number(&mut image, header + size_at + word, size, word);
        let dynamic_row = header + row_size;
        number(&mut image, dynamic_row, 2, 4); // PT_DYNAMIC.
        number(&mut image, dynamic_row + file_at, dynamic, word);
        number(&mut image, dynamic_row + size_at, strings - dynamic, word);
    }
    let tags = [(5, BASE + strings), (10, strings_size)]
        .into_iter()
        .chain(std::iter::repeat_n((1, 1), count));
    for (index, (tag, value)) in tags.enumerate() {
        let offset = dynamic + index * 2 * word;
        number(&mut image, offset, tag, word);
        number(&mut image, offset + word, value, word);
    }
    image
}

fn pe_image(plus: bool, import_kind: u64, count: usize, name: &[u8]) -> Vec<u8> {
    const PE: usize = 64;
    const OPTIONAL: usize = PE + 24;
    const RAW: usize = 512;
    const RVA: usize = 0x1000;
    const BASE: usize = 0x400000;

    let (directory, stride, name_at) = if import_kind == 0 {
        (1, 20, 12)
    } else {
        (13, 32, 4)
    };
    let table_size = (count + 1) * stride;
    let raw_size = table_size + name.len() + 1;
    let mut image = vec![0; RAW + table_size];
    image.extend_from_slice(name);
    image.push(0);
    let dir_at = OPTIONAL + if plus { 112 } else { 96 };
    let section = dir_at + (directory + 1) * 8;
    let number =
        |image: &mut [u8], offset, value, width| put_number(image, offset, value, width, false);
    put(&mut image, 0, b"MZ");
    number(&mut image, 0x3c, PE, 4);
    put(&mut image, PE, b"PE\0\0");
    number(&mut image, PE + 6, 1, 2);
    number(&mut image, PE + 20, section - OPTIONAL, 2);
    number(&mut image, OPTIONAL, if plus { 0x20b } else { 0x10b }, 2);
    number(
        &mut image,
        OPTIONAL + if plus { 24 } else { 28 },
        BASE,
        if plus { 8 } else { 4 },
    );
    number(&mut image, OPTIONAL + 60, RAW, 4);
    number(&mut image, dir_at - 4, directory + 1, 4);
    number(&mut image, dir_at + directory * 8, RVA, 4);
    number(&mut image, dir_at + directory * 8 + 4, table_size, 4);
    number(&mut image, section + 8, raw_size, 4);
    number(&mut image, section + 12, RVA, 4);
    number(&mut image, section + 16, raw_size, 4);
    number(&mut image, section + 20, RAW, 4);
    for index in 0..count {
        let row = RAW + index * stride;
        if import_kind == 1 {
            number(&mut image, row, 1, 4); // Delay-import name uses an RVA.
        }
        let address = RVA + table_size + if import_kind == 2 { BASE } else { 0 };
        number(&mut image, row + name_at, address, 4);
    }
    image
}

fn put_number(image: &mut [u8], at: usize, value: usize, width: usize, big: bool) {
    let mut bytes = (value as u64).to_le_bytes();
    if let Some(field) = bytes.get_mut(..width) {
        if big {
            field.reverse();
        }
        put(image, at, field);
    }
}

fn put(image: &mut [u8], at: usize, data: &[u8]) {
    if let Some(field) = image.get_mut(at..at + data.len()) {
        field.copy_from_slice(data);
    }
}
