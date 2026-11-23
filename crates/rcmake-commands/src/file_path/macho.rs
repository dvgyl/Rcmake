use std::io::ErrorKind;
use std::path::Path;

use rcmake_base::{BString, list};
use rcmake_eval::{Cx, Flow};
use rcmake_host::{FileKind, Follow, Host, ReadRange};

use super::{Result, error, set};

pub(super) fn run(cx: &mut Cx<'_>, args: &[&[u8]]) -> Result {
    let [file, rest @ ..] = args else {
        return Err(error(
            "READ_MACHO must be called with at least three additional arguments.",
        ));
    };
    if rest.len() < 2 {
        return Err(error(
            "READ_MACHO must be called with at least three additional arguments.",
        ));
    }
    let mut output = None;
    let mut capture = None;
    let mut args = rest.iter().copied().peekable();
    while let Some(option) = args.next() {
        let slot = match option {
            b"ARCHITECTURES" => &mut output,
            b"CAPTURE_ERROR" => &mut capture,
            _ => continue,
        };
        *slot = args.next_if(|arg| !matches!(*arg, b"ARCHITECTURES" | b"CAPTURE_ERROR"));
    }
    let path = cx.resolve_path(file);
    let host = cx.host();
    let limit = cx.limits().max_file_read;
    let result = match host.metadata(&path, Follow::Yes) {
        Ok(meta) if meta.kind == FileKind::File => architectures(host, &path, meta.len, limit)
            .ok_or(b" is not a valid Macho-O file".as_slice()),
        Err(err) if err.kind == ErrorKind::NotFound => Err(b" does not exist".as_slice()),
        _ => Err(b" is not a valid Macho-O file".as_slice()),
    };
    match result {
        Ok(names) => {
            if let Some(name) = output.filter(|name| !name.is_empty()) {
                set(cx, name, names)?;
            }
        }
        Err(reason) => {
            let message = [*file, reason].concat();
            if let Some(name) = capture.filter(|name| !name.is_empty()) {
                set(cx, name, message)?;
                if let Some(name) = output.filter(|name| !name.is_empty()) {
                    set(cx, name, "unknown")?;
                }
            } else {
                return Err(error(message));
            }
        }
    }
    Ok(Flow::Normal)
}

#[derive(Clone, Copy)]
pub(super) enum Order {
    Little,
    Big,
}

impl Order {
    pub(super) fn number<const N: usize>(self, bytes: &[u8], offset: usize) -> Option<u64> {
        let bytes = bytes.get(offset..)?.get(..N)?;
        let mut word = [0; 8];
        match self {
            Self::Little => {
                word.get_mut(..N)?.copy_from_slice(bytes);
                Some(u64::from_le_bytes(word))
            }
            Self::Big => {
                word.get_mut(8usize.checked_sub(N)?..)?
                    .copy_from_slice(bytes);
                Some(u64::from_be_bytes(word))
            }
        }
    }
}

fn thin(header: &[u8]) -> Option<&'static str> {
    let (order, size) = match header.get(..4)? {
        [0xfe, 0xed, 0xfa, 0xce] => (Order::Big, 28),
        [0xce, 0xfa, 0xed, 0xfe] => (Order::Little, 28),
        [0xfe, 0xed, 0xfa, 0xcf] => (Order::Big, 32),
        [0xcf, 0xfa, 0xed, 0xfe] => (Order::Little, 32),
        _ => return None,
    };
    header.get(..size)?;
    let cpu = order.number::<4>(header, 4)?;
    let subtype = order.number::<4>(header, 8)? & 0x00ff_ffff;
    Some(architecture(cpu, subtype))
}

fn architectures(host: &dyn Host, file: &Path, size: u64, limit: u64) -> Option<BString> {
    let read = |offset, len| {
        host.read(
            file,
            ReadRange::Slice {
                offset,
                len: Some(len),
            },
        )
        .ok()
    };
    let header = read(0, size.min(32).min(limit))?;
    let (order, width) = match header.get(..4)? {
        [0xca, 0xfe, 0xba, 0xbe] => (Order::Big, 20),
        [0xbe, 0xba, 0xfe, 0xca] => (Order::Little, 20),
        [0xca, 0xfe, 0xba, 0xbf] => (Order::Big, 32),
        [0xbf, 0xba, 0xfe, 0xca] => (Order::Little, 32),
        _ => return thin(&header).map(BString::from),
    };
    let count = order.number::<4>(&header, 4)?;
    let table_size = count * width;
    let end = 8 + table_size;
    if count == 0 || end > size || end.checked_add(count * 32)? > limit || count > 4096 {
        return None;
    }
    let table = read(8, table_size)?;
    if u64::try_from(table.len()).ok()? != table_size {
        return None;
    }
    let names = table
        .chunks_exact(usize::try_from(width).ok()?)
        .map(|entry| {
            let (offset, length) = if width == 20 {
                (order.number::<4>(entry, 8)?, order.number::<4>(entry, 12)?)
            } else {
                (order.number::<8>(entry, 8)?, order.number::<8>(entry, 16)?)
            };
            if offset < end || offset.checked_add(length)? > size {
                return None;
            }
            thin(&read(offset, length.min(32))?)
        })
        .collect::<Option<Vec<_>>>()?;
    Some(list::join(names))
}

fn architecture(cpu: u64, subtype: u64) -> &'static str {
    match (cpu, subtype) {
        (6, 1) => "m68k",
        (6, 2) => "m68040",
        (6, 3) => "m68030",
        (7, 4) => "i486",
        (7, 132) => "i486SX",
        (7, 8) | (0x0100_0007, 8) => "x86_64h",
        (7, _) => "i386",
        (0x0100_0007, 3) => "x86_64",
        (12, 0) => "arm",
        (12, 5..=18) => [
            "armv4t",
            "armv6",
            "armv5",
            "xscale",
            "armv7",
            "armv7f",
            "armv7s",
            "armv7k",
            "armv8",
            "armv6m",
            "armv7m",
            "armv7em",
            "armv8m.main",
            "armv8m.base",
        ]
        .get((subtype - 5) as usize)
        .copied()
        .unwrap_or("unknown"),
        (0x0100_000c, 0 | 1) => "arm64",
        (0x0100_000c, 2) => "arm64e",
        (0x0100_000c, 12) => "arm64e.x1",
        (0x0200_000c, 1) => "arm64_32",
        (14, _) => "sparc",
        (18, _) => "ppc",
        (0x0100_0012, _) => "ppc64",
        _ => "unknown",
    }
}
