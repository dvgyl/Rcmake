use std::{path::Path, time::Duration};

use rcmake_base::{BString, list};
use rcmake_eval::{Cx, Flow};
use rcmake_host::{
    CommandSpec, Follow, Input, Output, ProcessResult, RunRequest, path::from_native,
};

use super::{Result, error, macho::Order};

#[cfg(test)]
mod tests;

const LC_RPATH: u64 = 0x8000_001c;

pub(super) fn paths(data: &[u8]) -> Option<Vec<Vec<&[u8]>>> {
    let (order, width) = match data.get(..4)? {
        [0xca, 0xfe, 0xba, 0xbe] => (Order::Big, 20),
        [0xbe, 0xba, 0xfe, 0xca] => (Order::Little, 20),
        [0xca, 0xfe, 0xba, 0xbf] => (Order::Big, 32),
        [0xbf, 0xba, 0xfe, 0xca] => (Order::Little, 32),
        _ => return Some(vec![thin_paths(data)?]),
    };
    let count = usize::try_from(order.number::<4>(data, 4)?).ok()?;
    if !(1..=4096).contains(&count) {
        return None;
    }
    let table_end = 8 + count * width;
    let mut remaining_bytes = data.len().checked_sub(table_end)?;
    data.get(8..table_end)?
        .chunks_exact(width)
        .map(|entry| {
            let (start, size) = if width == 20 {
                (order.number::<4>(entry, 8)?, order.number::<4>(entry, 12)?)
            } else {
                (order.number::<8>(entry, 8)?, order.number::<8>(entry, 16)?)
            };
            let start = usize::try_from(start).ok()?;
            let size = usize::try_from(size).ok()?;
            if start < table_end {
                return None;
            }
            remaining_bytes = remaining_bytes.checked_sub(size)?;
            thin_paths(data.get(start..)?.get(..size)?)
        })
        .collect()
}

fn thin_paths(data: &[u8]) -> Option<Vec<&[u8]>> {
    let (order, header_size) = match data.get(..4)? {
        [0xfe, 0xed, 0xfa, 0xce] => (Order::Big, 28),
        [0xce, 0xfa, 0xed, 0xfe] => (Order::Little, 28),
        [0xfe, 0xed, 0xfa, 0xcf] => (Order::Big, 32),
        [0xcf, 0xfa, 0xed, 0xfe] => (Order::Little, 32),
        _ => return None,
    };
    let count = order.number::<4>(data, 16)?;
    let length = usize::try_from(order.number::<4>(data, 20)?).ok()?;
    let mut commands = data.get(header_size..)?.get(..length)?;
    let mut paths = Vec::new();
    for _ in 0..count {
        let size = usize::try_from(order.number::<4>(commands, 4)?).ok()?;
        if size < 8 {
            return None;
        }
        let (command, remaining) = commands.split_at_checked(size)?;
        if order.number::<4>(command, 0)? == LC_RPATH {
            let start = usize::try_from(order.number::<4>(command, 8)?).ok()?;
            if start < 12 {
                return None;
            }
            let string = command.get(start..)?;
            let end = string.iter().position(|&byte| byte == 0)?;
            paths.push(string.get(..end)?);
        }
        commands = remaining;
    }
    commands.is_empty().then_some(paths)
}

pub(super) fn edit(
    cx: &mut Cx<'_>,
    path: &Path,
    paths: Vec<Vec<&[u8]>>,
    command: &[u8],
    old: Option<&[u8]>,
    new: &[u8],
    tool: Option<&[u8]>,
) -> Result {
    let new = list::split(new, list::Empty::Drop);
    if command == b"RPATH_CHECK" {
        let matching = paths.iter().all(|slice| {
            if new.is_empty() {
                slice.is_empty()
            } else {
                new.iter().all(|entry| slice.contains(&entry.as_slice()))
            }
        });
        if !matching {
            let _ = cx.host().remove_file(path);
        }
        return Ok(Flow::Normal);
    }
    let old = old.map_or_else(
        || {
            paths
                .iter()
                .flatten()
                .map(|entry| BString::from(*entry))
                .collect()
        },
        |old| list::split(old, list::Empty::Drop),
    );
    let has_path = |entry: &BString| paths.iter().any(|p| p.contains(&entry.as_slice()));
    let mut args = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for value in &old {
        if has_path(value) && !new.contains(value) && seen.insert(value) {
            args.extend(["-delete_rpath".into(), value.clone()]);
        }
    }
    seen.clear();
    for value in &new {
        if !has_path(value) && seen.insert(value) {
            args.extend(["-add_rpath".into(), value.clone()]);
        }
    }
    if args.is_empty() {
        return Ok(Flow::Normal);
    }
    args.push(from_native(path));
    let program = tool
        .map(BString::from)
        .or_else(|| cx.var(b"CMAKE_INSTALL_NAME_TOOL"))
        .unwrap_or_else(|| "install_name_tool".into());
    let host = cx.host();
    let metadata = host
        .metadata(path, Follow::Yes)
        .map_err(|err| error(err.to_string()))?;
    let output = host
        .run(
            &RunRequest {
                pipeline: vec![CommandSpec { program, args }],
                cwd: host.initial_cwd(),
                env: host.env_all(),
                stdin: Input::Null,
                stdout: Output::Null,
                stderr: Output::Capture,
                timeout: Some(Duration::from_secs(60)),
                tee: false,
            },
            &mut |_, _| {},
        )
        .map_err(|err| error(err.to_string()))?;
    if output.results != [ProcessResult::Exited(0)] {
        return Err(error(
            [
                command,
                b" could not change the Mach-O runtime paths in ",
                &from_native(path),
                b": ",
                format!("{:?}", output.results).as_bytes(),
                b"\n",
                &output.stderr,
            ]
            .concat(),
        ));
    }
    host.set_times(path, Some(metadata.atime), Some(metadata.mtime))
        .map_err(|err| error(err.to_string()))?;
    Ok(Flow::Normal)
}
