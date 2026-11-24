use rcmake_base::{BString, path_lex::Lexical};
use rcmake_eval::{CmdResult, Cx, Flow};
use rcmake_model::InstallOptions;

use super::{Result, common, fail, keyword, record_request, values, warn_absolute_destination};

#[derive(Default)]
struct DependencySetOptions {
    common: InstallOptions,
    library: InstallOptions,
    runtime: InstallOptions,
    framework: InstallOptions,
}

pub(super) fn set(cx: &mut Cx<'_>, input: &[&[u8]]) -> CmdResult {
    let Some((_, options)) = input.split_first().filter(|(name, _)| !name.is_empty()) else {
        return Err(fail(
            "RUNTIME_DEPENDENCY_SET not given a runtime dependency set.",
        ));
    };
    let system = require_supported(cx, b"RUNTIME_DEPENDENCY_SET")?;

    let mut groups = DependencySetOptions::default();
    let mut group = &mut groups.common;
    let mut options = options.iter().copied().peekable();
    while let Some(key) = options.next() {
        match key {
            b"LIBRARY" => group = &mut groups.library,
            b"RUNTIME" => group = &mut groups.runtime,
            b"FRAMEWORK" => group = &mut groups.framework,
            b"DESTINATION" | b"COMPONENT" if options.peek().is_none_or(|arg| keyword(arg)) => {}
            key if filter_keyword(key) => {
                values(&mut options);
            }
            key if common(cx, b"RUNTIME_DEPENDENCY_SET", group, key, &mut options)? => {}
            _ => {
                return Err(fail(
                    [
                        b"RUNTIME_DEPENDENCY_SET given unknown argument \"".as_slice(),
                        key,
                        b"\".",
                    ]
                    .concat(),
                ));
            }
        }
    }

    let libraries = if system == b"Windows" {
        groups.runtime
    } else {
        groups.library
    };
    let frameworks = (system == b"Darwin").then_some(groups.framework);
    for artifact in std::iter::once(libraries).chain(frameworks) {
        if let Some(destination) = artifact
            .destination
            .as_ref()
            .or(groups.common.destination.as_ref())
            .filter(|destination| Lexical::new(destination).absolute())
        {
            warn_absolute_destination(cx, destination);
        }
    }
    if !cx.fatal_occurred() {
        record_request(cx, "_RCMAKE_RUNTIME_DEPENDENCY_SET", input);
    }
    Ok(Flow::Normal)
}

pub(super) fn require_supported(cx: &Cx<'_>, signature: &[u8]) -> Result<BString> {
    let system = cx.var_or_empty(b"CMAKE_SYSTEM_NAME");
    let system = if system.is_empty() {
        cx.host().system().sysname
    } else {
        system
    };
    if matches!(system.as_slice(), b"Linux" | b"Darwin" | b"Windows") {
        Ok(system)
    } else {
        Err(fail(
            [signature, b" is not supported on system \"", &system, b"\""].concat(),
        ))
    }
}

pub(super) fn filter_keyword(key: &[u8]) -> bool {
    matches!(
        key,
        b"DIRECTORIES"
            | b"PRE_INCLUDE_REGEXES"
            | b"PRE_EXCLUDE_REGEXES"
            | b"POST_INCLUDE_REGEXES"
            | b"POST_EXCLUDE_REGEXES"
            | b"POST_INCLUDE_FILES"
            | b"POST_EXCLUDE_FILES"
    )
}
