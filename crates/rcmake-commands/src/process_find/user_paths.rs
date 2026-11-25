use rcmake_base::{BString, ByteSlice as _};
use rcmake_eval::{CmdError, Cx};
use rcmake_host::{FileKind, Follow, path};

pub(super) fn expand(
    cx: &Cx<'_>,
    inputs: &[BString],
    view: Option<&BString>,
    program: bool,
) -> Result<Vec<BString>, CmdError> {
    let mut result = Vec::new();
    let mut work = cx.limits().max_value_bytes.min(16 * 1024 * 1024);
    for input in inputs {
        for path in
            super::registry_path::expand(cx, input, view.map(|view| view.as_slice()), program)?
        {
            glob(cx, path, &mut result, &mut work, 0)?;
        }
    }
    Ok(result)
}

fn glob(
    cx: &Cx<'_>,
    input: BString,
    result: &mut Vec<BString>,
    work: &mut usize,
    depth: u32,
) -> Result<(), CmdError> {
    let exceeded = || CmdError::status("Search path expansion exceeded its limit.");
    *work = work
        .checked_sub(input.len().saturating_add(64))
        .ok_or_else(exceeded)?;
    let Some(star) = input.find(b"/*") else {
        result.push(input);
        return Ok(());
    };
    if depth >= cx.limits().max_recursion.min(64) {
        return Err(exceeded());
    }
    let prefix = input.get(..=star).ok_or_else(exceeded)?;
    let tail = input.get(star + 2..).ok_or_else(exceeded)?;
    let dir = cx.host().initial_cwd().join(path::to_native(prefix));
    if let Ok(entries) = cx.host().read_dir(&dir) {
        for entry in entries {
            *work = work
                .checked_sub(entry.name.len().saturating_add(64))
                .ok_or_else(exceeded)?;
            if matches!(entry.name.as_slice(), b"." | b"..")
                || !cx
                    .host()
                    .metadata(&dir.join(path::to_native(&entry.name)), Follow::Yes)
                    .is_ok_and(|meta| meta.kind == FileKind::Dir)
            {
                continue;
            }
            let next = [prefix, entry.name.as_slice(), tail].concat();
            glob(cx, next.into(), result, work, depth + 1)?;
        }
    }
    Ok(())
}
