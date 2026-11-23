//! Built-in CMake 4.5.1 command families organized into disjoint submodules so parallel
//! workers can implement command families concurrently without merge conflicts.

#![forbid(unsafe_code)]

use rcmake_eval::Registry;

mod file_path;
mod policy_misc;
mod process_find;
mod string_math;
mod vars_list;

/// Register all implemented built-in CMake 4.5.1 script commands into `registry`.
pub fn register_all(registry: &mut Registry) {
    vars_list::register(registry);
    string_math::register(registry);
    file_path::register(registry);
    process_find::register(registry);
    policy_misc::register(registry);
}

fn command_error(
    cx: &mut rcmake_eval::Cx<'_>,
    message: impl Into<rcmake_base::BString>,
) -> rcmake_eval::CmdError {
    cx.issue(
        rcmake_diag::MessageKind::Error,
        rcmake_diag::Effect::Fatal,
        None,
        message.into(),
    );
    rcmake_eval::CmdError::Reported
}

fn exit_with_error(
    cx: &mut rcmake_eval::Cx<'_>,
    code: i32,
    message: &[u8],
) -> rcmake_eval::CmdError {
    let mut line = b"CMake Error: ".to_vec();
    line.extend_from_slice(message);
    line.push(b'\n');
    cx.host().write_out(rcmake_host::Stream::Stderr, &line);
    cx.request_exit(code);
    rcmake_eval::CmdError::Reported
}
