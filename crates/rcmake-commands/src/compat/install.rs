use std::path::Path;

use rcmake_base::BString;
use rcmake_eval::{CmdError, CmdResult, Cx, Flow};
use rcmake_expr::ExpandedArg;
use rcmake_host::{
    Follow, Stream,
    path::{from_native_cmake, to_native},
};
use rcmake_model::{BT, InstallKind, InstallOptions, InstallRule, TargetBinding};
use rcmake_regex::MatchOptions;

use super::{incorrect_arguments, superseded};
use crate::install::{default_component, record_args};

#[derive(Clone, Copy)]
enum Kind {
    Files,
    Programs,
}

pub(super) fn files(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    install_paths(cx, args, Kind::Files)
}

pub(super) fn programs(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    install_paths(cx, args, Kind::Programs)
}

fn install_paths(cx: &mut Cx<'_>, args: &[ExpandedArg], kind: Kind) -> CmdResult {
    let (name, replacement) = match kind {
        Kind::Files => ("install_files", "install(FILES)"),
        Kind::Programs => ("install_programs", "install(PROGRAMS)"),
    };
    superseded(cx, name, replacement);
    let [dest, first, rest @ ..] = args else {
        return Err(incorrect_arguments());
    };
    let dest = destination(&dest.value);
    let paths: Vec<BString> = if first.value == b"FILES" {
        rest.iter().map(|arg| arg.value.clone()).collect()
    } else if rest.is_empty() {
        matching_paths(cx, &first.value)?
    } else {
        match kind {
            Kind::Files => rest
                .iter()
                .map(|path| replace_extension(&path.value, &first.value))
                .collect(),
            Kind::Programs => std::iter::once(first)
                .chain(rest)
                .map(|arg| arg.value.clone())
                .collect(),
        }
    };
    let backtrace = cx.backtrace();
    let paths = paths
        .iter()
        .map(|path| BT::new(install_source(cx, path), backtrace.clone()))
        .collect();
    let kind = match kind {
        Kind::Files => InstallKind::Files(paths),
        Kind::Programs => InstallKind::Programs(paths),
    };
    add_rule(cx, kind, dest, None);
    Ok(Flow::Normal)
}

pub(super) fn targets(cx: &mut Cx<'_>, args: &[ExpandedArg]) -> CmdResult {
    superseded(cx, "install_targets", "install(TARGETS)");
    let [dest, rest @ ..] = args else {
        return Err(incorrect_arguments());
    };
    if rest.is_empty() {
        return Err(incorrect_arguments());
    }
    let dest = destination(&dest.value);
    let dir = cx.current_dir_id();
    let mut rest = rest.iter();
    let mut runtime_dir = BString::from("bin");
    let mut targets = Vec::new();
    while let Some(arg) = rest.next() {
        if arg.value == b"RUNTIME_DIRECTORY" {
            let value = rest.next().ok_or_else(|| {
                CmdError::Status("called with RUNTIME_DIRECTORY but no actual directory".into())
            })?;
            runtime_dir = destination(&value.value);
            continue;
        }
        let project = cx.project();
        let id = project.lookup_target(dir, &arg.value).and_then(|binding| {
            let TargetBinding::Target(id) = binding else {
                return None;
            };
            project
                .target(id)
                .filter(|target| target.dir == dir && !target.is_imported())
                .map(|_| id)
        });
        let Some(id) = id else {
            return Err(CmdError::Status(
                [
                    b"Cannot find target: \"".as_slice(),
                    &arg.value,
                    b"\" to install.",
                ]
                .concat()
                .into(),
            ));
        };
        targets.push((id, runtime_dir.clone()));
    }
    if targets.is_empty() {
        add_rule(
            cx,
            InstallKind::Targets {
                targets: Vec::new(),
                export: None,
            },
            dest,
            None,
        );
        return Ok(Flow::Normal);
    }
    let dll_platform = cx.var_is_on(b"WIN32") || cx.var_is_on(b"CYGWIN");
    for (id, runtime_dir) in targets {
        let runtime = dll_platform
            && cx
                .project()
                .target(id)
                .is_some_and(|target| target.kind == rcmake_model::TargetKind::SharedLibrary);
        add_rule(
            cx,
            InstallKind::Targets {
                targets: vec![id],
                export: None,
            },
            dest.clone(),
            runtime.then_some(runtime_dir),
        );
    }
    Ok(Flow::Normal)
}

fn destination(value: &[u8]) -> BString {
    let rest = value.get(1..).unwrap_or_default();
    if rest.is_empty() {
        return ".".into();
    }
    rest.iter()
        .map(|byte| if *byte == b'\\' { b'/' } else { *byte })
        .collect::<Vec<_>>()
        .into()
}

fn replace_extension(path: &[u8], extension: &[u8]) -> BString {
    let name_start = path
        .iter()
        .rposition(|byte| matches!(byte, b'/' | b'\\'))
        .map_or(0, |slash| slash + 1);
    let end = path
        .get(name_start..)
        .unwrap_or_default()
        .iter()
        .rposition(|byte| *byte == b'.')
        .map_or(path.len(), |dot| name_start + dot);
    [path.get(..end).unwrap_or(path), extension].concat().into()
}

fn matching_paths(cx: &mut Cx<'_>, pattern: &[u8]) -> Result<Vec<BString>, CmdError> {
    let regex = match cx.regex(pattern) {
        Ok(regex) => regex,
        Err(error) => {
            cx.host().write_out(
                Stream::Stdout,
                format!(
                    "{}\nRegularExpression::compile(): Error in compile.\n",
                    error.message,
                )
                .as_bytes(),
            );
            return Ok(Vec::new());
        }
    };
    let Ok(mut entries) = cx.host().read_dir(&cx.current_source_dir()) else {
        return Ok(Vec::new());
    };
    let mut budget = cx.limits().regex_steps;
    let mut paths = Vec::new();
    entries.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    for entry in entries {
        if regex
            .find_at(&entry.name, 0, MatchOptions::default(), &mut budget)
            .map_err(|error| CmdError::Status(error.to_string().into()))?
            .is_some()
        {
            paths.push(entry.name);
        }
    }
    Ok(paths)
}

fn install_source(cx: &Cx<'_>, value: &[u8]) -> BString {
    let path = to_native(value);
    if value.starts_with(b"$<") {
        return value.into();
    }
    if path.is_absolute() {
        return from_native_cmake(&path);
    }
    let binary = cx.current_binary_dir().join(&path);
    let source = cx.current_source_dir().join(path);
    if exists(cx, &binary) || !exists(cx, &source) {
        from_native_cmake(&binary)
    } else {
        from_native_cmake(&source)
    }
}

fn exists(cx: &Cx<'_>, path: &Path) -> bool {
    cx.host().metadata(path, Follow::Yes).is_ok()
}

fn add_rule(cx: &mut Cx<'_>, kind: InstallKind, destination: BString, runtime: Option<BString>) {
    let dir = cx.current_dir_id();
    let backtrace = cx.backtrace();
    let options = InstallOptions {
        destination: Some(destination),
        component: Some(default_component(cx)),
        ..InstallOptions::default()
    };
    let rule = InstallRule {
        dir,
        backtrace,
        kind,
        options,
    };
    let project = cx.project();
    let previous = match &rule.kind {
        InstallKind::Targets { targets, .. } if !targets.is_empty() => {
            project.installs.iter_mut().enumerate().find(|(_, rule)| {
                matches!(&rule.kind, InstallKind::Targets { targets: old, .. } if old == targets)
                    && rule
                        .backtrace
                        .top()
                        .and_then(|frame| frame.name.as_deref())
                        .is_some_and(|name| {
                            name.strip_prefix(b"_")
                                .unwrap_or(name)
                                .eq_ignore_ascii_case(b"install_targets")
                        })
            })
        }
        _ => None,
    };
    let (index, replaced) = if let Some((index, previous)) = previous {
        *previous = rule;
        (index, true)
    } else {
        (project.add_install(rule), false)
    };
    let section = format!("_RCMAKE_INSTALL_{index}");
    if let Some(runtime) = runtime {
        record_args(
            cx,
            &section,
            &[b"TARGETS", b"RUNTIME", b"DESTINATION", &runtime],
        );
    } else if replaced {
        for argument in 0..4 {
            cx.global()
                .props
                .unset(format!("{section}_ARG{argument}").as_bytes());
        }
    }
    if let Some(level) = cx.var(b"CMAKE_INSTALL_MESSAGE") {
        cx.global()
            .props
            .set(format!("{section}_MESSAGE").into(), level);
    } else if replaced {
        cx.global()
            .props
            .unset(format!("{section}_MESSAGE").as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcmake_eval::{Interp, InterpConfig, RunMode};
    use rcmake_host::PureHost;
    use std::sync::Arc;

    #[test]
    fn legacy_install_regex_records_paths_in_byte_order() -> Result<(), Box<dyn std::error::Error>>
    {
        let root = Path::new(if cfg!(windows) {
            "C:/legacy-install"
        } else {
            "/legacy-install"
        });
        let source = root.join("source");
        let build = root.join("build");
        let command = root.join("bin/cmake");
        let host = Arc::new(PureHost::new(source.clone(), command.clone()));
        for name in ["z.txt", "b.txt", "a.txt", "skip.md"] {
            host.seed_file(&source.join(name), b"")?;
        }
        host.seed_file(&source.join("CMakeLists.txt"),
            b"cmake_minimum_required(VERSION 4.5)\nproject(Legacy NONE)\ninstall_files(/share \"^[a-z][.]txt$\")\n")?;
        let mut interp = Interp::new(
            InterpConfig {
                services: Default::default(),
                host: host.clone(),
                mode: RunMode::Configure,
                cmake_root: root.join("share/cmake"),
                cmake_command: command,
                cwd: source.clone(),
                limits: Default::default(),
                diag: Default::default(),
                defines: Vec::new(),
                script_args: Vec::new(),
            },
            crate::register_all,
        );
        assert_eq!(
            interp.run_configure(&source, &build, &[]).exit_code,
            0,
            "{}",
            host.stream_bytes(Stream::Stderr)
        );
        let mut cx = interp.cx();
        let [rule] = cx.project().installs.as_slice() else {
            return Err("Expected one install rule".into());
        };
        let InstallKind::Files(files) = &rule.kind else {
            return Err("Expected a file install rule".into());
        };
        let expected: Vec<_> = ["a.txt", "b.txt", "z.txt"]
            .map(|name| from_native_cmake(&source.join(name)))
            .into();
        assert_eq!(
            files.iter().map(|file| &file.value).collect::<Vec<_>>(),
            expected.iter().collect::<Vec<_>>()
        );
        Ok(())
    }
}
