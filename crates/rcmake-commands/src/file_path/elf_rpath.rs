use std::ops::Range;

use rcmake_base::BString;
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
    entries: Vec<PathEntry>,
}

struct PathEntry {
    offset: usize,
    tag: u64,
    range: Range<usize>,
    storage: Range<usize>,
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
            tag @ (RPATH | RUNPATH) => {
                if offsets.len() == 2 {
                    return None;
                }
                offsets.push((start + stride * index, tag, value));
            }
            _ => {}
        }
    }
    let dynamic = dynamic.start..end?;
    if offsets.is_empty() {
        return Some(Paths { dynamic, ..empty() });
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
        .map(|(entry, tag, offset)| {
            let offset = usize::try_from(offset).ok()?;
            let path = strings.get(offset..)?;
            let len = path.iter().position(|&byte| byte == 0)?;
            let start = str_start + offset;
            let padding = path
                .get(len..)?
                .iter()
                .take_while(|&&byte| byte == 0)
                .count();
            Some(PathEntry {
                offset: entry,
                tag,
                range: start..start + len,
                storage: start..start + len + padding,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(Paths {
        dynamic,
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

fn replace_component(
    path: &[u8],
    old: &[u8],
    new: &[u8],
    strip: bool,
) -> std::result::Result<Option<Vec<u8>>, &'static str> {
    const INVALID: &str = "Invalid ELF runtime path range.";
    let Some(start) = find(path, old) else {
        if find(path, new).is_some() {
            return Ok(None);
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
    Ok(Some([prefix, new, suffix].concat()))
}

fn update(
    data: &mut [u8],
    paths: &Paths,
    old: Option<&[u8]>,
    new: &[u8],
    strip: bool,
) -> std::result::Result<bool, &'static str> {
    const INVALID: &str = "Invalid ELF runtime path range.";
    let mut edits = Vec::new();
    for entry in &paths.entries {
        let path = data.get(entry.range.clone()).ok_or(INVALID)?;
        let replacement = match old {
            Some(old) => replace_component(path, old, new, strip)?,
            None => Some(new.to_vec()),
        };
        let Some(replacement) = replacement.filter(|replacement| replacement != path) else {
            continue;
        };
        if replacement.len() >= entry.storage.len() {
            return Err(if entry.tag == RPATH {
                "The replacement path is too long for the RPATH entry."
            } else {
                "The replacement path is too long for the RUNPATH entry."
            });
        }
        edits.push((entry.storage.clone(), replacement));
    }
    if edits.len() == paths.entries.len() && edits.iter().all(|(_, value)| value.is_empty()) {
        return Ok(remove(data, paths));
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
        if skip.next_if(|entry| entry.offset == read).is_none() {
            data.copy_within(read..read + paths.stride, write);
            write += paths.stride;
        }
    }
    if let Some(tail) = data.get_mut(write..paths.dynamic.end) {
        tail.fill(0);
    }
    for PathEntry { storage, .. } in &paths.entries {
        if let Some(path) = data.get_mut(storage.clone()) {
            path.fill(0);
        }
    }
    true
}

pub(super) fn read(cx: &mut Cx<'_>, args: &[&[u8]]) -> Result {
    let [input, options @ ..] = args else {
        return Err(error(
            "READ_ELF must be called with at least three additional arguments.",
        ));
    };
    if options.len() < 2 {
        return Err(error(
            "READ_ELF must be called with at least three additional arguments.",
        ));
    }
    let mut rpath = None;
    let mut runpath = None;
    let mut capture = None;
    let mut options = options.iter().copied().peekable();
    while let Some(key) = options.next() {
        let slot = match key {
            b"RPATH" => &mut rpath,
            b"RUNPATH" => &mut runpath,
            b"CAPTURE_ERROR" => &mut capture,
            _ => continue,
        };
        *slot =
            options.next_if(|&value| !matches!(value, b"RPATH" | b"RUNPATH" | b"CAPTURE_ERROR"));
    }
    let file = cx.resolve_path(input);
    let host = cx.host();
    if !host
        .metadata(&file, Follow::Yes)
        .is_ok_and(|metadata| metadata.kind == FileKind::File)
    {
        return Err(error(
            [
                b"READ_ELF given FILE \"".as_slice(),
                input,
                b"\" that does not exist.",
            ]
            .concat(),
        ));
    }
    let data = host
        .read(
            &file,
            ReadRange::All {
                limit: cx.limits().max_file_read,
            },
        )
        .ok();
    if let Some((data, paths)) = data.as_deref().and_then(|data| Some((data, parse(data)?))) {
        for entry in paths.entries {
            if let Some(name) = if entry.tag == RPATH { rpath } else { runpath }
                && !name.is_empty()
                && let Some(value) = data.get(entry.range)
            {
                let value = value
                    .iter()
                    .map(|&byte| if byte == b':' { b';' } else { byte })
                    .collect::<Vec<_>>();
                cx.set_var(name, BString::from(value));
            }
        }
        return Ok(Flow::Normal);
    }
    if let Some(name) = capture.filter(|name| !name.is_empty()) {
        super::set(cx, name, "not a valid ELF file")
    } else {
        Err(error(
            [
                b"READ_ELF given FILE:\n  ".as_slice(),
                input,
                b"\nthat is not a valid ELF file.",
            ]
            .concat(),
        ))
    }
}

pub(super) fn run(cx: &mut Cx<'_>, command: &[u8], args: &[&[u8]]) -> Result {
    let operation = String::from_utf8_lossy(command);
    let mut file = None;
    let mut old = None;
    let mut new = None;
    let mut tool = None;
    let mut strip = false;
    let check = command == b"RPATH_CHECK";
    let removing = command == b"RPATH_REMOVE";
    let setting = command == b"RPATH_SET";
    let mut args = args.iter().copied();
    while let Some(key) = args.next() {
        let slot = match key {
            b"FILE" => &mut file,
            b"RPATH" if check => &mut new,
            b"OLD_RPATH" if !check && !setting => &mut old,
            b"NEW_RPATH" if !check && !removing => &mut new,
            b"INSTALL_NAME_TOOL" if !check && !setting => &mut tool,
            b"INSTALL_REMOVE_ENVIRONMENT_RPATH" if !check && !removing && !setting => {
                strip = true;
                continue;
            }
            _ if setting => {
                return Err(error(
                    [command, b" given unrecognized argument \"", key, b"\"."].concat(),
                ));
            }
            _ => return Err(error([command, b" given unknown argument ", key].concat())),
        };
        *slot = Some(super::arg(&mut args, key)?);
    }
    let file = file
        .filter(|file| !file.is_empty())
        .ok_or_else(|| error(format!("{operation} not given FILE option.")))?;
    if !check && !removing && !setting && old.is_none() {
        return Err(error(format!("{operation} not given OLD_RPATH option.")));
    }
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
    if !setting && let Some(paths) = super::macho_rpath::paths(&data) {
        return super::macho_rpath::edit(cx, &path, paths, command, old, new, tool);
    }
    if let Some(key) = tool
        .map(|_| b"INSTALL_NAME_TOOL".as_slice())
        .or((removing && old.is_some()).then_some(b"OLD_RPATH"))
    {
        return Err(error([command, b" given unknown argument ", key].concat()));
    }
    let paths = parse(&data);
    if check {
        let matches = paths
            .as_ref()
            .filter(|paths| !paths.entries.is_empty())
            .map_or(new.is_empty(), |paths| {
                paths.entries.iter().any(|PathEntry { range, .. }| {
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
                if setting {
                    b"RPATH_SET".as_slice()
                } else {
                    b"RPATH_CHANGE".as_slice()
                },
                b" could not write new RPATH:\n  ",
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
            update(&mut data, &paths, old, new, strip).map_err(change_error)?
        }
        Some(paths) if paths.dynamic.is_empty() => false,
        _ if removing || new.is_empty() => false,
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
        if meta.mode & 0o6000 != 0 {
            host.set_permissions(&path, meta.mode)
                .map_err(|err| error(err.to_string()))?;
        }
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
