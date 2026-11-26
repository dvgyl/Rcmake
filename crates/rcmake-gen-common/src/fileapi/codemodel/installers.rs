use rcmake_base::{
    BString,
    path_lex::{Lexical, cmake_style},
};
use rcmake_resolve::{NamelinkMode, TargetFileInstaller, TargetInstaller};

use super::super::{Bool, Members, Value, array, members, object, relative};
use super::{number, targets::TargetRef};

pub(super) fn target(
    installer: &TargetInstaller,
    reference: &TargetRef<'_>,
    build: &[u8],
) -> Members {
    let mut row = members([
        ("type", Value::string("target")),
        ("targetId", Value::string(reference.id.as_str())),
        ("targetIndex", number(reference.index)),
        ("destination", Value::string(installer.destination.clone())),
        ("component", Value::string(installer.component.clone())),
        (
            "paths",
            array(
                installer
                    .paths
                    .iter()
                    .map(|(from, to)| path(build, from, to)),
            ),
        ),
    ]);
    for (key, flag) in [
        ("isOptional", installer.optional),
        ("isExcludeFromAll", installer.exclude_from_all),
        ("targetIsImportLibrary", installer.import_library),
    ] {
        if flag {
            row.insert(key.into(), Bool(true));
        }
    }
    let mode = match installer.namelink {
        NamelinkMode::None => return row,
        NamelinkMode::Only => "only",
        NamelinkMode::Skip => "skip",
    };
    row.insert("targetInstallNamelink".into(), Value::string(mode));
    row
}

pub(super) fn target_file(installer: &TargetFileInstaller, source: &[u8]) -> Members {
    let dest_norm = cmake_style(&installer.destination);
    let mut dest = dest_norm.as_slice();
    while dest.len() > 1 && !dest.ends_with(b":/") {
        let Some(trimmed) = dest.strip_suffix(b"/") else {
            break;
        };
        dest = trimmed;
    }
    let mut row = members([
        ("type", Value::string("file")),
        ("destination", Value::string(dest)),
        ("component", Value::string(installer.component.clone())),
        (
            "paths",
            array(
                installer
                    .files
                    .iter()
                    .map(|file| Value::string(relative(file, source))),
            ),
        ),
    ]);
    if installer.optional {
        row.insert("isOptional".into(), Bool(true));
    }
    if installer.exclude_from_all {
        row.insert("isExcludeFromAll".into(), Bool(true));
    }
    row
}

pub(super) fn path(root: &[u8], from: &[u8], to: &[u8]) -> Value {
    let from = relative(from, root);
    if Lexical::new(&from).filename() == to {
        return Value::string(from);
    }
    object([("from", Value::string(from)), ("to", Value::string(to))])
}

pub(super) fn export_path(dir_rel: &[u8], destination: &[u8], file: &BString) -> BString {
    let digest = rcmake_base::hash::HashAlgo::Md5.hex_digest(destination);
    if dir_rel.is_empty() || dir_rel == b"." {
        format!("CMakeFiles/Export/{digest}/{file}").into()
    } else {
        let dir_rel = BString::from(dir_rel);
        format!("{dir_rel}/CMakeFiles/Export/{digest}/{file}").into()
    }
}
