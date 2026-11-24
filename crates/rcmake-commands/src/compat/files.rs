use rcmake_base::{BString, list};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_host::{Follow, WriteMode, path::to_native};
use rcmake_model::DirId;

use super::{incorrect_arguments, superseded};

pub(super) fn make_directory(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    superseded(cx, "make_directory", "file(MAKE_DIRECTORY)");
    let [dir] = args else {
        return Err(incorrect_arguments());
    };
    protect_source(cx, &dir.value, b"attempted to create a directory: ")?;
    if !dir.value.is_empty() {
        let path = cx.host().initial_cwd().join(to_native(&dir.value));
        let _ = cx.host().create_dir_all(&path);
    }
    Ok(Flow::Normal)
}

pub(super) fn write_file(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    superseded(cx, "write_file", "file(WRITE)");
    let [filename, content @ ..] = args else {
        return Err(incorrect_arguments());
    };
    if content.is_empty() {
        return Err(incorrect_arguments());
    }
    let mut mode = WriteMode::Truncate;
    let mut data = Vec::new();
    for arg in content {
        if arg.value == b"APPEND" {
            mode = WriteMode::Append;
        } else if data.len().saturating_add(arg.value.len()) >= cx.limits().max_value_bytes {
            return Err(CmdError::Status(
                "write content exceeds the size limit".into(),
            ));
        } else {
            data.extend_from_slice(&arg.value);
        }
    }
    data.push(b'\n');
    protect_source(cx, &filename.value, b"attempted to write a file: ")?;
    let path = cx.host().initial_cwd().join(to_native(&filename.value));
    let operation = || {
        let host = cx.host();
        if let Some(parent) = path.parent() {
            let _ = host.create_dir_all(parent);
        }
        let permissions = host
            .metadata(&path, Follow::Yes)
            .ok()
            .map(|meta| meta.mode)
            .filter(|mode| mode & 0o200 == 0);
        if let Some(previous) = permissions {
            let _ = host.set_permissions(&path, previous | 0o220);
        }
        let result = host.write(&path, &data, mode);
        if let Some(previous) = permissions {
            let _ = host.set_permissions(&path, previous);
        }
        result
    };
    operation().map_err(|_| {
        CmdError::Status(
            [
                b"Internal CMake error when trying to open file: ".as_slice(),
                &filename.value,
                b" for writing.",
            ]
            .concat()
            .into(),
        )
    })?;
    Ok(Flow::Normal)
}

pub(super) fn remove(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    superseded(cx, "remove", "list(REMOVE_ITEM)");
    let Some((name, items)) = args.split_first() else {
        return Ok(Flow::Normal);
    };
    let Some(value) = cx.var(&name.value) else {
        return Ok(Flow::Normal);
    };
    let removed: BTreeSet<BString> = items
        .iter()
        .flat_map(|item| list::split(&item.value, list::Empty::Drop))
        .collect();
    let value = list::split(&value, list::Empty::Drop)
        .into_iter()
        .filter(|entry| !removed.contains(entry));
    cx.set_var(&name.value, list::join(value));
    Ok(Flow::Normal)
}

fn protect_source(cx: &mut Cx<'_>, path: &[u8], operation: &[u8]) -> Result<(), CmdError> {
    if !can_write_project_file(cx, &to_native(path)) {
        cx.set_fatal();
        return Err(CmdError::Status(
            [operation, path, b" into a source directory."]
                .concat()
                .into(),
        ));
    }
    Ok(())
}

pub(crate) fn can_write_project_file(cx: &mut Cx<'_>, path: &std::path::Path) -> bool {
    if !cx.var_is_on(b"CMAKE_DISABLE_SOURCE_CHANGES") {
        return true;
    }
    let root = cx.project().dir(DirId::ROOT);
    let source = to_native(&root.source_dir);
    let binary = to_native(&root.binary_dir);
    let path = crate::dirs::normalized_path(path);
    let source = cx.host().canonicalize(&source).unwrap_or(source);
    let binary = cx.host().canonicalize(&binary).unwrap_or(binary);
    let path = cx.host().canonicalize(&path).unwrap_or(path);
    if source == binary {
        !cx.var_is_on(b"CMAKE_DISABLE_IN_SOURCE_BUILD")
    } else {
        path == source || !path.starts_with(source) || path.starts_with(binary)
    }
}
use std::collections::BTreeSet;
