use rcmake_base::truth;
use rcmake_diag::{
    DiagCategory, Diagnostic, Effect, Location, MessageKind, PolicyId, PolicyStatus, RenderOptions,
    render,
};
use rcmake_host::{Host, Stream};
use rcmake_model::{DirId, InstallKind, InstallRule, ProjectModel};

use super::{definition, has_genex, join};

const POLICY: PolicyId = PolicyId::new(87);

pub(super) fn status(model: &ProjectModel, dir: DirId) -> PolicyStatus {
    model.dir(dir).final_policies.map_or_else(
        || match definition(model, dir, b"CMAKE_POLICY_DEFAULT_CMP0087").map(|v| v.as_slice()) {
            Some(b"NEW") => PolicyStatus::New,
            Some(b"OLD") => PolicyStatus::Old,
            _ => PolicyStatus::Warn,
        },
        |policies| policies.get(POLICY),
    )
}

pub(super) fn warn(model: &ProjectModel, rule: &InstallRule, host: &dyn Host) {
    let (InstallKind::Script(value) | InstallKind::Code(value)) = &rule.kind else {
        return;
    };
    if status(model, rule.dir) != PolicyStatus::Warn
        || !has_genex(value)
        || definition(model, rule.dir, b"CMAKE_SUPPRESS_DEVELOPER_WARNINGS")
            .is_some_and(|value| truth::is_on(value))
    {
        return;
    }
    let warning = Diagnostic {
        annotations: None,
        kind: MessageKind::AuthorWarning,
        effect: Effect::None,
        category: Some(DiagCategory::Policy),
        policy: Some(POLICY),
        location: Location::FileLine {
            file: join(&model.dir(rule.dir).source_dir, b"CMakeLists.txt")
                .as_slice()
                .into(),
            line: 0,
        },
        body: POLICY.warning_text().into(),
    };
    host.write_out(
        Stream::Stderr,
        &render(
            &warning,
            &RenderOptions {
                relative_base: Some(model.dir(DirId::ROOT).source_dir.clone()),
                ..RenderOptions::default()
            },
        ),
    );
}
