use rcmake_diag::{DiagCategory, Effect, MessageKind};
use rcmake_eval::{CmdError, CmdFlags, Cx, Registry, RunMode};

mod files;
pub(crate) use files::can_write_project_file;
mod install;
mod process;
mod removed;
mod ui;

macro_rules! project_command {
    ($registry:expr, $name:literal, $function:path) => {
        $registry.add_raw(
            $name,
            |cx, _| {
                if cx.mode() == rcmake_eval::RunMode::Script {
                    return Err($crate::compat::unknown_in_script(cx, $name));
                }
                let raw = cx.raw_args().to_vec();
                let args = cx.expand_raw_args(&raw)?;
                $function(cx, &args)
            },
            rcmake_eval::CmdFlags { scriptable: true },
        )
    };
}

pub(crate) use project_command;

pub(crate) fn register(registry: &mut Registry) {
    let script = CmdFlags { scriptable: true };
    let project = CmdFlags { scriptable: false };
    registry.add("make_directory", files::make_directory, script);
    registry.add("write_file", files::write_file, script);
    registry.add("remove", files::remove, script);
    registry.add_raw("exec_program", process::exec_program, script);
    project_command!(registry, "install_files", install::files);
    project_command!(registry, "install_programs", install::programs);
    project_command!(registry, "install_targets", install::targets);
    registry.add("fltk_wrap_ui", ui::fltk, project);
    registry.add("qt_wrap_cpp", ui::qt_cpp, project);
    registry.add("qt_wrap_ui", ui::qt_ui, project);
    removed::register(registry);
}

pub(crate) fn unknown_in_script(cx: &mut Cx<'_>, name: &str) -> CmdError {
    let backtrace = cx.backtrace();
    let called = backtrace
        .top()
        .and_then(|frame| frame.name.as_deref())
        .unwrap_or(name.as_bytes());
    crate::command_error(
        cx,
        [b"Unknown CMake command \"".as_slice(), called, b"\"."].concat(),
    )
}

const ABSENT_IN_SCRIPT: &[&[u8]] = &[
    b"add_compile_definitions",
    b"add_custom_rule",
    b"add_link_options",
    b"cmake_instrumentation",
    b"discover_tests",
    b"install_files",
    b"install_programs",
    b"install_targets",
    b"subdirs",
    b"target_link_directories",
    b"target_link_options",
    b"target_precompile_headers",
];

pub(crate) fn command_names(cx: &Cx<'_>) -> Vec<rcmake_base::BString> {
    let mut names = cx.registered_command_names();
    if cx.mode() == RunMode::Script {
        names.retain(|name| {
            let builtin = name.strip_prefix(b"_").unwrap_or(name);
            !ABSENT_IN_SCRIPT.contains(&builtin) || cx.user_command_kind(name).is_some()
        });
    }
    names
}

fn incorrect_arguments() -> CmdError {
    CmdError::Status("called with incorrect number of arguments".into())
}

fn superseded(cx: &mut Cx<'_>, name: &str, replacement: &str) {
    cx.issue(
        MessageKind::DeprecationWarning,
        Effect::None,
        Some(DiagCategory::Deprecated),
        format!("The '{name}' command has been superseded.  Use '{replacement}' instead.").into(),
    );
}
