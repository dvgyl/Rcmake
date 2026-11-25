//! Check custom command outputs at generate time.
//!
//! CMake creates custom commands during generation. It reports a bad `OUTPUT` or
//! `BYPRODUCTS` path there, after it evaluates generator expressions, and it continues
//! with the next command so that one run reports all problems.

use rcmake_base::{BString, list, path_lex::Lexical, truth};
use rcmake_genex::{ConfigId, GenexError};
use rcmake_model::{CustomCommand, DirId, ProjectModel};

use super::{CommandEvaluation, command_result};
use crate::lower::{definition, join};
use crate::{ResolveError, usage::UsageResolver};

/// Evaluate the outputs and byproducts of `command` and check each path.
///
/// # Errors
///
/// Returns all evaluation errors and path errors of the command, in order.
pub(crate) fn checked_command_outputs(
    model: &ProjectModel,
    command: &CustomCommand,
    config: &ConfigId,
    usage: &mut UsageResolver<'_>,
    host: &dyn rcmake_host::Host,
) -> Result<(Vec<BString>, Vec<BString>), ResolveError> {
    let binary = &model.dir(command.dir).binary_dir;
    let source_root = model.dir(DirId::ROOT).source_dir.clone();
    let mut eval = CommandEvaluation::new(model, command, config, config, usage);
    let mut errors = Vec::new();
    let mut outputs = Vec::new();
    let mut byproducts = Vec::new();
    for (keyword, entries, paths) in [
        ("OUTPUT", &command.outputs, &mut outputs),
        ("BYPRODUCTS", &command.byproducts, &mut byproducts),
    ] {
        for entry in entries {
            let value = match eval.value(&entry.value) {
                Ok(value) => value,
                Err(error) => {
                    errors.push(error);
                    continue;
                }
            };
            for path in list::split(&value, list::Empty::Drop) {
                let path = Lexical::new(&join(binary, &path)).normalize();
                if let Some(body) = output_problem(model, command.dir, keyword, &path, Some(host)) {
                    let error = GenexError::Message {
                        expression: BString::default(),
                        message: body.into(),
                    };
                    errors.push(error.at(&command.backtrace, &source_root).into());
                    continue;
                }
                paths.push(path);
            }
        }
    }
    command_result((outputs, byproducts), errors)
}

/// Return CMake's error text for an output path that a custom command may not write.
pub(crate) fn output_problem(
    model: &ProjectModel,
    dir: DirId,
    keyword: &str,
    path: &[u8],
    host: Option<&dyn rcmake_host::Host>,
) -> Option<String> {
    if !can_write_project_file(model, dir, path, host) {
        return Some(format!(
            "{keyword} path\n  {}\nin a source directory as an output of custom command.",
            BString::from(path)
        ));
    }
    let invalid = path.iter().find(|byte| matches!(byte, b'<' | b'>'))?;
    Some(format!(
        "{keyword} containing a \"{}\" is not allowed.",
        char::from(*invalid)
    ))
}

/// Apply `CMAKE_DISABLE_SOURCE_CHANGES` and `CMAKE_DISABLE_IN_SOURCE_BUILD` to `path`.
fn can_write_project_file(
    model: &ProjectModel,
    dir: DirId,
    path: &[u8],
    host: Option<&dyn rcmake_host::Host>,
) -> bool {
    let on = |name: &[u8]| definition(model, dir, name).is_some_and(|value| truth::is_on(value));
    if !on(b"CMAKE_DISABLE_SOURCE_CHANGES") {
        return true;
    }
    let root = model.dir(DirId::ROOT);
    let insensitive = host.map_or(cfg!(any(windows, target_os = "macos")), |host| {
        matches!(host.system().sysname.as_slice(), b"Windows" | b"Darwin")
    });
    let normalize = |value: &[u8]| {
        let mut value = Lexical::new(value).normalize();
        if insensitive {
            value.make_ascii_lowercase();
        }
        value
    };
    let source = normalize(&root.source_dir);
    let binary = normalize(&root.binary_dir);
    let same_directory = source == binary
        || host.is_some_and(|host| {
            let source = rcmake_host::path::to_native(&root.source_dir);
            let binary = rcmake_host::path::to_native(&root.binary_dir);
            let identity = |path| {
                host.metadata(path, rcmake_host::Follow::Yes)
                    .ok()
                    .filter(|metadata| metadata.ino != 0)
                    .map(|metadata| (metadata.dev, metadata.ino))
            };
            identity(source.as_path())
                .zip(identity(binary.as_path()))
                .is_some_and(|(source, binary)| source == binary)
                || host
                    .canonicalize(&source)
                    .ok()
                    .zip(host.canonicalize(&binary).ok())
                    .is_some_and(|(source, binary)| source == binary)
        });
    if same_directory {
        return !on(b"CMAKE_DISABLE_IN_SOURCE_BUILD");
    }
    let path = normalize(path);
    path == source || !within(&path, &source) || within(&path, &binary)
}

fn within(path: &[u8], base: &[u8]) -> bool {
    path.strip_prefix(base)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(b"/") || base.ends_with(b"/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(disable: bool) -> ProjectModel {
        let root = if cfg!(windows) { "C:/work" } else { "/work" };
        let mut model = ProjectModel::default();
        model.set_root_dirs(format!("{root}/src").into(), format!("{root}/build").into());
        if disable {
            model
                .dir_mut(DirId::ROOT)
                .vars
                .insert("CMAKE_DISABLE_SOURCE_CHANGES".into(), "ON".into());
        }
        model
    }

    #[test]
    fn rejects_angle_brackets_and_source_outputs() {
        let root = if cfg!(windows) { "C:/work" } else { "/work" };
        let model = project(true);
        let source = format!("{root}/src/out.txt");
        let binary = format!("{root}/build/a>");
        assert_eq!(
            output_problem(&model, DirId::ROOT, "OUTPUT", source.as_bytes(), None),
            Some(format!(
                "OUTPUT path\n  {source}\nin a source directory as an output of custom command."
            ))
        );
        assert_eq!(
            output_problem(&model, DirId::ROOT, "BYPRODUCTS", binary.as_bytes(), None),
            Some("BYPRODUCTS containing a \">\" is not allowed.".into())
        );
        let good = format!("{root}/build/out.txt");
        assert_eq!(
            output_problem(&model, DirId::ROOT, "OUTPUT", good.as_bytes(), None),
            None
        );
    }

    #[test]
    fn allows_source_outputs_without_the_restriction() {
        let root = if cfg!(windows) { "C:/work" } else { "/work" };
        let model = project(false);
        let source = format!("{root}/src/out.txt");
        assert_eq!(
            output_problem(&model, DirId::ROOT, "OUTPUT", source.as_bytes(), None),
            None
        );
        assert!(within(b"/a/b/c", b"/a/b"));
        assert!(!within(b"/a/bc", b"/a/b"));
    }
}
