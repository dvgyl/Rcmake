// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::BTreeSet;

use rcmake_expr::ExpandedArg;
use rcmake_syntax::Argument;

use super::ordinary_properties;

pub(super) fn ordinary_sources(sources: &[&[u8]]) -> bool {
    sources.iter().all(|item| {
        !matches!(
            *item,
            b"ALIAS"
                | b"IMPORTED"
                | b"EXCLUDE_FROM_ALL"
                | b"GLOBAL"
                | b"WIN32"
                | b"MACOSX_BUNDLE"
                | b"STATIC"
                | b"SHARED"
                | b"MODULE"
                | b"INTERFACE"
                | b"OBJECT"
                | b"UNKNOWN"
                | b"SYMBOLIC"
        )
    })
}

pub(super) fn install_targets<'a>(args: &[&'a [u8]]) -> Option<Vec<&'a [u8]>> {
    let [b"TARGETS", rest @ ..] = args else {
        return None;
    };
    if rest.iter().any(|arg| {
        matches!(
            *arg,
            b"FILE_SET"
                | b"CXX_MODULES_BMI"
                | b"RUNTIME_DEPENDENCIES"
                | b"RUNTIME_DEPENDENCY_SET"
                | b"NAMELINK_ONLY"
        )
    }) {
        return None;
    }
    let targets: Vec<_> = rest
        .iter()
        .copied()
        .take_while(|arg| {
            !matches!(
                *arg,
                b"EXPORT"
                    | b"ARCHIVE"
                    | b"LIBRARY"
                    | b"RUNTIME"
                    | b"OBJECTS"
                    | b"FRAMEWORK"
                    | b"BUNDLE"
                    | b"PRIVATE_HEADER"
                    | b"PUBLIC_HEADER"
                    | b"RESOURCE"
                    | b"INCLUDES"
                    | b"DESTINATION"
                    | b"PERMISSIONS"
                    | b"CONFIGURATIONS"
                    | b"COMPONENT"
                    | b"OPTIONAL"
                    | b"EXCLUDE_FROM_ALL"
                    | b"NAMELINK_COMPONENT"
                    | b"NAMELINK_SKIP"
            )
        })
        .collect();
    (!targets.is_empty()).then_some(targets)
}

pub(crate) fn valid_expansion(name: &[u8], raw: &[Argument], args: &[ExpandedArg]) -> bool {
    if !raw
        .iter()
        .any(|arg| arg.text.iter().any(|byte| matches!(byte, b'$' | b';')))
    {
        return true;
    }
    let raw: Vec<_> = raw.iter().map(|arg| arg.text.as_slice()).collect();
    let args: Vec<_> = args.iter().map(|arg| arg.value.as_slice()).collect();
    let same_prefix = |length| {
        raw.get(..length)
            .is_some_and(|prefix| args.get(..length) == Some(prefix))
    };
    match name {
        b"add_library" => same_prefix(2) && args.get(2..).is_some_and(ordinary_sources),
        b"add_executable" => same_prefix(1) && args.get(1..).is_some_and(ordinary_sources),
        b"target_compile_definitions"
        | b"target_compile_options"
        | b"target_sources"
        | b"target_include_directories"
        | b"target_compile_features"
        | b"target_link_options" => same_prefix(2) && !args.contains(&b"FILE_SET".as_slice()),
        b"install" => {
            install_targets(&raw).is_some_and(|targets| install_targets(&args) == Some(targets))
        }
        b"set_target_properties" => {
            let Some(marker) = raw.iter().position(|arg| *arg == b"PROPERTIES") else {
                return false;
            };
            let names: BTreeSet<_> = raw.iter().take(marker).copied().collect();
            same_prefix(marker + 1)
                && raw.len() == args.len()
                && ordinary_properties(&args, &names)
                && raw
                    .iter()
                    .skip(marker + 1)
                    .step_by(2)
                    .eq(args.iter().skip(marker + 1).step_by(2))
        }
        b"set_property" => same_prefix(5),
        _ => true,
    }
}
