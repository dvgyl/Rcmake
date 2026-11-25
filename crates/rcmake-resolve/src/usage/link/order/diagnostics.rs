use std::fmt::Write;
use std::sync::Arc;

use rcmake_base::truth;
use rcmake_diag::{
    DiagCategory, Diagnostic, Effect, Location, MessageKind, PolicyId, PolicyStatus, RenderOptions,
    render,
};
use rcmake_host::Stream;
use rcmake_model::{DirId, ProjectModel, Target};

use super::{ResolvedLinkItem, ResolvedLinkKind, SourcePaths, definition};

pub(super) fn warn_policy(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    target: &Target,
    number: u16,
) {
    let policy = PolicyId::new(number);
    let defined_on =
        |key: &[u8]| definition(model, target.dir, key).is_some_and(|value| truth::is_on(value));
    if target.policies.policies.get(policy) != PolicyStatus::Warn
        || !defined_on(format!("CMAKE_POLICY_WARNING_{policy}").as_bytes())
        || defined_on(b"CMAKE_SUPPRESS_DEVELOPER_WARNINGS")
        || model
            .global
            .props
            .get(b"IN_TRY_COMPILE")
            .is_some_and(|value| truth::is_on(value))
    {
        return;
    }
    let summary = policy.info().map_or("", |info| info.summary);
    let behavior = if number == 156 {
        "legacy libraries de-duplication strategy will be applied."
    } else {
        "static libraries de-duplication will keep the last occurrence of the static libraries."
    };
    let diagnostic = Diagnostic { annotations: None,
        kind: MessageKind::AuthorWarning,
        effect: Effect::None,
        category: Some(DiagCategory::Policy),
        policy: Some(policy),
        location: Location::Backtrace(target.backtrace.clone()),
        body: format!("Policy {policy} is not set: {summary}  Run \"cmake --help-policy {policy}\" for policy details.  Use the cmake_policy command to set the policy and suppress this warning.\n\nSince the policy is not set, {behavior}").into(),
    };
    paths.host.write_out(
        Stream::Stderr,
        &render(
            &diagnostic,
            &RenderOptions {
                relative_base: Some(model.dir(DirId::ROOT).source_dir.clone()),
                ..RenderOptions::default()
            },
        ),
    );
}

pub(super) fn print_link_order(
    paths: &SourcePaths<'_>,
    model: &ProjectModel,
    target: &Target,
    heading: &str,
    items: &[Arc<ResolvedLinkItem>],
) {
    let mut output = format!("target [{}] {heading}:\n", target.name);
    for item in items {
        if let ResolvedLinkKind::Group { feature, .. } = &item.kind {
            let _ = writeln!(output, "  group, feature [{feature}]");
            continue;
        }
        if let Some(target) = item.edge_targets().next().and_then(|id| model.target(id)) {
            let _ = write!(output, "  target [{}]", target.name);
        } else {
            let _ = write!(output, "  item [{}]", item.text.value);
        }
        if let Some(feature) = &item.feature {
            let _ = write!(output, ", feature [{feature}]");
        }
        output.push('\n');
    }
    output.push('\n');
    paths.host.write_out(Stream::Stderr, output.as_bytes());
}
