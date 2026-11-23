//! `build_command()`: the `cmake --build` command line for this build tree.

use rcmake_base::BString;
use rcmake_diag::{DiagCategory, Effect, MessageKind};
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_model::{CacheEntry, CacheType};

/// `build_command(<var> [CONFIGURATION <cfg>] [PARALLEL_LEVEL <n>] [TARGET <tgt>]
/// [PROJECT_NAME <name>])`, and the legacy `build_command(<cachevar> <makecommand>)`.
pub(super) fn build_command(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    if let [define, _] = args {
        return legacy(cx, &define.value);
    }
    let Some((variable, rest)) = args.split_first() else {
        return Err(CmdError::Status(
            "requires at least one argument naming a CMake variable".into(),
        ));
    };
    let mut configuration = BString::default();
    let mut project_name = BString::default();
    let mut target = BString::default();
    let mut parallel = BString::default();
    let mut slot: Option<&mut BString> = None;
    for arg in rest {
        match arg.value.as_slice() {
            b"CONFIGURATION" => slot = Some(&mut configuration),
            b"PROJECT_NAME" => slot = Some(&mut project_name),
            b"TARGET" => slot = Some(&mut target),
            b"PARALLEL_LEVEL" => slot = Some(&mut parallel),
            value => match slot.take() {
                Some(field) => *field = value.into(),
                None => {
                    let message = [b"unknown argument \"".as_slice(), value, b"\""].concat();
                    return Err(CmdError::Status(message.into()));
                }
            },
        }
    }
    if configuration.is_empty() {
        configuration = cx.host().env_get(b"CMAKE_CONFIG_TYPE").unwrap_or_default();
    }
    if configuration.is_empty() {
        configuration = "Release".into();
    }
    if !project_name.is_empty() {
        cx.issue(
            MessageKind::AuthorWarning,
            Effect::None,
            Some(DiagCategory::Author),
            "Ignoring PROJECT_NAME option because it has no effect.".into(),
        );
    }
    let command = cmake_build_command(cx, &target, &configuration, &parallel);
    cx.set_var(&variable.value, command);
    Ok(Flow::Normal)
}

/// `build_command(<cachevar> <makecommand>)`: set the cache entry once.
fn legacy(cx: &mut Cx<'_>, define: &[u8]) -> CmdResult {
    let configuration = cx
        .host()
        .env_get(b"CMAKE_CONFIG_TYPE")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Release".into());
    let command = cmake_build_command(cx, b"", &configuration, b"");
    if cx.var_defined(define) {
        return Ok(Flow::Normal);
    }
    cx.cache().insert(
        define.into(),
        CacheEntry {
            value: command,
            ty: CacheType::String,
            help: "Command used to build entire project from the command line.".into(),
            advanced: false,
            props: Default::default(),
        },
    );
    if cx.policy(rcmake_diag::PolicyId::new(126)) != rcmake_diag::PolicyStatus::New {
        cx.unset_var(define);
    }
    Ok(Flow::Normal)
}

/// `cmGlobalGenerator::GenerateCMakeBuildCommand` without native tool options.
fn cmake_build_command(
    cx: &mut Cx<'_>,
    target: &[u8],
    configuration: &[u8],
    parallel: &[u8],
) -> BString {
    let mut command = unix_output_path(&cx.var_or_empty(b"CMAKE_COMMAND"));
    command.extend_from_slice(b" --build .");
    for (option, value) in [
        (b" --config \"".as_slice(), configuration),
        (b" --parallel \"", parallel),
        (b" --target \"", target),
    ] {
        if !value.is_empty() {
            command.extend_from_slice(option);
            command.extend_from_slice(value);
            command.push(b'"');
        }
    }
    command.into()
}

/// `SystemTools::ConvertToUnixOutputPath`: drop repeated slashes after the first byte
/// and escape spaces that are not escaped yet.
fn unix_output_path(path: &[u8]) -> Vec<u8> {
    let mut collapsed = Vec::with_capacity(path.len());
    for (index, byte) in path.iter().enumerate() {
        if *byte == b'/' && index > 1 && collapsed.last() == Some(&b'/') {
            continue;
        }
        collapsed.push(*byte);
    }
    if !collapsed.contains(&b' ') {
        return collapsed;
    }
    let mut escaped = Vec::with_capacity(collapsed.len() + 4);
    let mut last = 0u8;
    for byte in collapsed {
        if byte == b' ' && last != b'\\' {
            escaped.push(b'\\');
        }
        escaped.push(byte);
        last = byte;
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::unix_output_path;

    #[test]
    fn output_path_collapses_slashes_and_escapes_spaces() {
        assert_eq!(unix_output_path(b"/usr//bin/cmake"), b"/usr/bin/cmake");
        assert_eq!(unix_output_path(b"//net/a b/c\\ d"), b"//net/a\\ b/c\\ d");
    }
}
