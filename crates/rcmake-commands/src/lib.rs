//! Built-in CMake 4.5.1 command families organized into disjoint submodules so parallel
//! workers can implement command families concurrently without merge conflicts.

#![forbid(unsafe_code)]

use rcmake_eval::Registry;

mod active;
mod compat;
mod configure;
pub use configure::{ConfigureRuntime, ConfigureSelection};
mod custom;
mod dirs;
#[cfg(all(feature = "native", any(unix, windows)))]
mod fetch_observe;
mod file_path;
mod hermetic;
mod install;
mod instrumentation;
mod policy_misc;
pub use policy_misc::introspection::print_deferred_properties;
mod process_find;
mod string_math;
mod targets;
mod vars_list;

#[cfg(test)]
mod registry_tests;

/// Register all implemented built-in CMake 4.5.1 script commands into `registry`.
pub fn register_all(registry: &mut Registry) {
    vars_list::register(registry);
    string_math::register(registry);
    file_path::register(registry);
    #[cfg(all(feature = "native", any(unix, windows)))]
    registry.guard("file", fetch_observe::check_upload);
    process_find::register(registry);
    policy_misc::register(registry);
    targets::register(registry);
    dirs::register(registry);
    custom::register(registry);
    install::register(registry);
    configure::register(registry);
    instrumentation::register(registry);
    active::register(registry);
    compat::register(registry);
    registry.mark_replay_commands();
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
