use rcmake_base::{BString, path_lex::Lexical};
use rcmake_resolve::{NamelinkMode, TargetInstaller};

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

pub(super) fn path(root: &[u8], from: &[u8], to: &[u8]) -> Value {
    let from = relative(from, root);
    if Lexical::new(&from).filename() == to {
        return Value::string(from);
    }
    object([("from", Value::string(from)), ("to", Value::string(to))])
}

pub(super) fn export_path(destination: &[u8], file: &BString) -> BString {
    let digest = rcmake_base::hash::HashAlgo::Md5.hex_digest(destination);
    format!("CMakeFiles/Export/{digest}/{file}").into()
}
