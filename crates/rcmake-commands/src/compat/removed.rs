use rcmake_diag::{Effect, MessageKind, PolicyId};
use rcmake_eval::{CmdError, CmdFlags, CmdResult, Cx, Registry};
use rcmake_expr::ExpandedArg;

struct RemovedCommand {
    name: &'static str,
    policy: PolicyId,
    flags: CmdFlags,
}

impl RemovedCommand {
    const fn new(name: &'static str, policy: u16, scriptable: bool) -> Self {
        Self {
            name,
            policy: PolicyId::new(policy),
            flags: CmdFlags { scriptable },
        }
    }
}

const REMOVED: &[RemovedCommand] = &[
    RemovedCommand::new("subdir_depends", 29, false),
    RemovedCommand::new("use_mangled_mesa", 30, true),
    RemovedCommand::new("load_command", 31, false),
    RemovedCommand::new("output_required_files", 32, false),
    RemovedCommand::new("export_library_dependencies", 33, false),
    RemovedCommand::new("utility_source", 34, false),
    RemovedCommand::new("variable_requires", 35, false),
    RemovedCommand::new("build_name", 36, true),
];

pub(super) fn register(registry: &mut Registry) {
    for removed in REMOVED {
        registry.add_raw(removed.name, reject, removed.flags);
    }
}

fn reject(cx: &mut Cx<'_>, _: &[ExpandedArg]) -> CmdResult {
    let backtrace = cx.backtrace();
    let called = backtrace
        .top()
        .and_then(|frame| frame.name.as_deref())
        .unwrap_or_default();
    let called = called.strip_prefix(b"_").unwrap_or(called);
    let Some(removed) = REMOVED
        .iter()
        .find(|entry| called.eq_ignore_ascii_case(entry.name.as_bytes()))
    else {
        return Err(crate::command_error(cx, "This command has been removed."));
    };
    let RemovedCommand { name, policy, .. } = removed;
    cx.issue(
        MessageKind::Error,
        Effect::SetError,
        None,
        format!("The {name} command has been removed; see {policy}.").into(),
    );
    Err(CmdError::Reported)
}
