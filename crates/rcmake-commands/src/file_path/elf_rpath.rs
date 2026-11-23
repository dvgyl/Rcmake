use std::ops::Range;

use rcmake_eval::{Cx, Flow};
use rcmake_host::{FileKind, Follow, ReadRange, WriteMode};

use super::macho::Order;
use super::{Result, error, status};

const RPATH: u64 = 15;
const RUNPATH: u64 = 29;

#[derive(Clone, Copy)]
struct Layout {
    order: Order,
    wide: bool,
}

impl Layout {
    fn word(self, data: &[u8], offset: usize) -> Option<u64> {
        if self.wide {
            self.order.number::<8>(data, offset)
        } else {
            self.order.number::<4>(data, offset)
        }
    }

    fn size(self) -> usize {
        if self.wide { 8 } else { 4 }
    }
}

struct Paths {
    dynamic: Range<usize>,
    stride: usize,
    entries: Vec<(usize, Range<usize>)>,
}

fn parse(data: &[u8]) -> Option<Paths> {
    let [0x7f, b'E', b'L', b'F', class, endian, 1] = data.get(..7)? else {
        return None;
    };
    let wide = match class {
        1 => false,
        2 => true,
        _ => return None,
    };
    let order = match endian {
        1 => Order::Little,
        2 => Order::Big,
        _ => return None,
    };
    let layout = Layout { order, wide };
    let stride = 2 * layout.size();
    let (offset, size_at, min_size) = if wide { (32, 54, 56) } else { (28, 42, 32) };
    let start = usize::try_from(layout.word(data, offset)?).ok()?;
    let width = usize::try_from(order.number::<2>(data, size_at)?).ok()?;
    let count = usize::try_from(order.number::<2>(data, size_at + 2)?).ok()?;
    let empty = || Paths {
        dynamic: 0..0,
        stride,
        entries: Vec::new(),
    };
    if count == 0 {
        return Some(empty());
    }
    if width < min_size {
        return None;
    }
    let headers = data.get(start..start.checked_add(width.checked_mul(count)?)?)?;
    let segment = |header: &[u8]| -> Option<(u64, u64, u64)> {
        let (offset, address, size) = if wide { (8, 16, 32) } else { (4, 8, 16) };
        Some((
            layout.word(header, offset)?,
            layout.word(header, address)?,
            layout.word(header, size)?,
        ))
    };
    let Some(header) = headers
        .chunks_exact(width)
        .find(|header| order.number::<4>(header, 0) == Some(2))
    else {
        return Some(empty());
    };
    let (offset, _, size) = segment(header)?;
    let start = usize::try_from(offset).ok()?;
    let dynamic = start..start.checked_add(usize::try_from(size).ok()?)?;
    let mut strtab = None;
    let mut strsz = None;
    let mut offsets = Vec::new();
    let mut end = None;
    for (index, entry) in data.get(dynamic.clone())?.chunks_exact(stride).enumerate() {
        let value = layout.word(entry, layout.size())?;
        match layout.word(entry, 0)? {
            0 => {
                end = Some(start + stride * (index + 1));
                break;
            }
            5 => strtab = Some(value),
            10 => strsz = Some(value),
            RPATH | RUNPATH => {
                if offsets.len() == 2 {
                    return None;
                }
                offsets.push((start + stride * index, value));
            }
            _ => {}
        }
    }
    if offsets.is_empty() {
        return Some(empty());
    }
    let strtab = strtab?;
    let strsz = strsz?;
    let str_start = headers.chunks_exact(width).find_map(|header| {
        if order.number::<4>(header, 0)? != 1 {
            return None;
        }
        let (offset, address, length) = segment(header)?;
        let delta = strtab.checked_sub(address)?;
        if delta.checked_add(strsz)? > length {
            return None;
        }
        usize::try_from(offset.checked_add(delta)?).ok()
    })?;
    let str_end = str_start.checked_add(usize::try_from(strsz).ok()?)?;
    let strings = data.get(str_start..str_end)?;
    let entries = offsets
        .into_iter()
        .map(|(entry, offset)| {
            let offset = usize::try_from(offset).ok()?;
            let path = strings.get(offset..)?;
            let len = path.iter().position(|&byte| byte == 0)?;
            let start = str_start + offset;
            Some((entry, start..start + len))
        })
        .collect::<Option<Vec<_>>>()?;
    Some(Paths {
        dynamic: dynamic.start..end?,
        stride,
        entries,
    })
}

fn find(path: &[u8], part: &[u8]) -> Option<usize> {
    if part.is_empty() {
        return path.is_empty().then_some(0);
    }
    path.windows(part.len())
        .enumerate()
        .find_map(|(index, value)| {
            let end = index + part.len();
            (value == part
                && (index == 0 || path.get(index - 1) == Some(&b':'))
                && (end == path.len() || path.get(end) == Some(&b':')))
            .then_some(index)
        })
}

fn change(
    data: &mut [u8],
    paths: &Paths,
    old: &[u8],
    new: &[u8],
    strip: bool,
) -> std::result::Result<bool, &'static str> {
    const INVALID: &str = "Invalid ELF runtime path range.";
    let mut edits = Vec::new();
    for (_, range) in &paths.entries {
        let path = data.get(range.clone()).ok_or(INVALID)?;
        let Some(start) = find(path, old) else {
            if find(path, new).is_some() {
                continue;
            }
            return Err("The current runtime path does not contain OLD_RPATH.");
        };
        let mut prefix = if strip {
            &[][..]
        } else {
            path.get(..start).ok_or(INVALID)?
        };
        let mut suffix = path.get(start + old.len()..).ok_or(INVALID)?;
        if new.is_empty() {
            if prefix.is_empty() {
                suffix = suffix.strip_prefix(b":").unwrap_or(suffix);
            } else {
                prefix = prefix
                    .strip_suffix(b":")
                    .filter(|_| suffix.is_empty())
                    .unwrap_or(prefix);
                if !suffix.is_empty() {
                    suffix = suffix.strip_prefix(b":").unwrap_or(suffix);
                }
            }
        }
        let replacement = [prefix, new, suffix].concat();
        if replacement.len() > path.len() {
            return Err("The replacement path is too long for the runtime path field.");
        }
        if replacement != path {
            edits.push((range.clone(), replacement));
        }
    }
    for (range, replacement) in &edits {
        let path = data.get_mut(range.clone()).ok_or(INVALID)?;
        path.fill(0);
        path.get_mut(..replacement.len())
            .ok_or(INVALID)?
            .copy_from_slice(replacement);
    }
    Ok(!edits.is_empty())
}

fn remove(data: &mut [u8], paths: &Paths) -> bool {
    if paths.entries.is_empty() {
        return false;
    }
    let mut skip = paths.entries.iter().peekable();
    let mut write = paths.dynamic.start;
    for read in paths.dynamic.clone().step_by(paths.stride) {
        if skip.next_if(|(offset, _)| *offset == read).is_none() {
            data.copy_within(read..read + paths.stride, write);
            write += paths.stride;
        }
    }
    if let Some(tail) = data.get_mut(write..paths.dynamic.end) {
        tail.fill(0);
    }
    for (_, range) in &paths.entries {
        if let Some(path) = data.get_mut(range.clone()) {
            path.fill(0);
        }
    }
    true
}

pub(super) fn run(cx: &Cx<'_>, command: &[u8], args: &[&[u8]]) -> Result {
    let operation = String::from_utf8_lossy(command);
    let mut file = None;
    let mut old = None;
    let mut new = None;
    let mut strip = false;
    let check = command == b"RPATH_CHECK";
    let removing = command == b"RPATH_REMOVE";
    let mut args = args.iter().copied();
    while let Some(key) = args.next() {
        let slot = match key {
            b"FILE" => &mut file,
            b"RPATH" if check => &mut new,
            b"OLD_RPATH" if !check && !removing => &mut old,
            b"NEW_RPATH" if !check && !removing => &mut new,
            b"INSTALL_REMOVE_ENVIRONMENT_RPATH" if !check && !removing => {
                strip = true;
                continue;
            }
            _ => return Err(error([command, b" given unknown argument ", key].concat())),
        };
        *slot = Some(super::arg(&mut args, key)?);
    }
    let file = file
        .filter(|file| !file.is_empty())
        .ok_or_else(|| error(format!("{operation} not given FILE option.")))?;
    let old = if check || removing {
        &[][..]
    } else {
        old.ok_or_else(|| error(format!("{operation} not given OLD_RPATH option.")))?
    };
    let new = if removing {
        &[][..]
    } else {
        new.ok_or_else(|| {
            error(format!(
                "{operation} not given {} option.",
                if check { "RPATH" } else { "NEW_RPATH" }
            ))
        })?
    };
    let host = cx.host();
    let path = host.initial_cwd().join(rcmake_host::path::to_native(file));
    let meta = host
        .metadata(&path, Follow::Yes)
        .ok()
        .filter(|meta| meta.kind == FileKind::File);
    let Some(meta) = meta else {
        return if check {
            Ok(Flow::Normal)
        } else {
            Err(error(
                [command, b" given FILE \"", file, b"\" that does not exist."].concat(),
            ))
        };
    };
    let mut data = host
        .read(
            &path,
            ReadRange::All {
                limit: cx.limits().max_file_read,
            },
        )
        .map_err(|err| error(err.to_string()))?;
    let paths = parse(&data);
    if check {
        let matches = paths
            .as_ref()
            .filter(|paths| !paths.entries.is_empty())
            .map_or(new.is_empty(), |paths| {
                paths.entries.iter().any(|(_, range)| {
                    let Some(path) = data.get(range.clone()) else {
                        return false;
                    };
                    new.is_empty() && path.is_empty()
                        || !new.is_empty() && find(path, new).is_some()
                })
            });
        if !matches {
            let _ = host.remove_file(&path);
        }
        return Ok(Flow::Normal);
    }
    let change_error = |reason: &str| {
        error(
            [
                b"RPATH_CHANGE could not write new RPATH:\n  ",
                new,
                b"\nto the file:\n  ",
                file,
                b"\n",
                reason.as_bytes(),
            ]
            .concat(),
        )
    };
    let changed = match paths {
        Some(paths) if removing => remove(&mut data, &paths),
        Some(paths) if !paths.entries.is_empty() => {
            if new.is_empty()
                && paths
                    .entries
                    .iter()
                    .all(|(_, range)| data.get(range.clone()) == Some(old))
            {
                remove(&mut data, &paths)
            } else {
                change(&mut data, &paths, old, new, strip).map_err(change_error)?
            }
        }
        _ if removing || old.is_empty() && new.is_empty() => false,
        Some(_) => {
            return Err(change_error(
                "No valid ELF RPATH or RUNPATH entry exists in the file.",
            ));
        }
        None => return Err(change_error("The file format is not recognized.")),
    };
    if changed {
        host.write(&path, &data, WriteMode::Truncate)
            .map_err(|err| error(err.to_string()))?;
        host.set_times(&path, Some(meta.atime), Some(meta.mtime))
            .map_err(|err| error(err.to_string()))?;
        let message = if removing {
            [b"Removed runtime path from \"".as_slice(), file, b"\""].concat()
        } else {
            [
                b"Set non-toolchain portion of runtime path of \"".as_slice(),
                file,
                b"\" to \"",
                new,
                b"\"",
            ]
            .concat()
        };
        status(cx, &message);
    }
    Ok(Flow::Normal)
}
