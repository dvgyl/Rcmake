use std::fmt::Write;
use std::sync::Arc;

use rcmake_base::{BString, truth};
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
        location: Location::Backtrace(crate::lower::listed_backtrace(&target.backtrace)),
        body: format!("Policy {policy} is not set: {summary}  Run \"cmake --help-policy {policy}\" for policy details.  Use the cmake_policy command to set the policy and suppress this warning.\nSince the policy is not set, {behavior}").into(),
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

/// Warn when an imported target lists an item with an unsupported feature.
///
/// Runs during dependency expansion before `LINK_LIBRARY_OVERRIDE*` overrides
/// and deduplication, matching CMake's `cmComputeLinkDepends`.
pub(super) fn warn_imported_features(
    model: &ProjectModel,
    paths: &SourcePaths<'_>,
    target: &Target,
    imported: &Target,
    language: &[u8],
    items: &[Arc<ResolvedLinkItem>],
) {
    if definition(model, target.dir, b"CMAKE_SUPPRESS_DEVELOPER_WARNINGS")
        .is_some_and(|value| truth::is_on(value))
    {
        return;
    }
    let language_name = BString::from(language);
    let warn = |operator: &str, feature: &BString| {
        let variable = format!("CMAKE_{language_name}_{operator}_USING_{feature}");
        paths.warn_once(
            model,
            MessageKind::AuthorWarning,
            None,
            Location::Backtrace(target.backtrace.clone()),
            format!(
                "The 'IMPORTED' target '{}' uses the generator-expression '$<{operator}>' with the feature '{feature}', which is undefined or unsupported.\nDid you miss to define it by setting variables \"{variable}\" and \"{variable}_SUPPORTED\"?",
                imported.name
            )
            .into(),
        );
    };
    let check_library = |item: &ResolvedLinkItem| {
        if item.text.value == target.name || item.text.value.is_empty() {
            return;
        }
        if let Some(feature) = item
            .feature
            .as_ref()
            .filter(|feature| feature.as_slice() != b"DEFAULT")
            && !is_library_feature_supported(model, target.dir, language, feature)
        {
            warn("LINK_LIBRARY", feature);
        }
    };
    for item in items {
        if let ResolvedLinkKind::Group { feature, members } = &item.kind {
            if !members.is_empty()
                && !is_group_feature_supported(model, target.dir, language, feature)
            {
                warn("LINK_GROUP", feature);
            }
            for member in members {
                check_library(member);
            }
        } else {
            check_library(item);
        }
    }
}

fn is_library_feature_supported(
    model: &ProjectModel,
    dir: DirId,
    language: &[u8],
    feature: &[u8],
) -> bool {
    let specific = [
        b"CMAKE_",
        language,
        b"_LINK_LIBRARY_USING_",
        feature,
        b"_SUPPORTED",
    ]
    .concat();
    if let Some(value) = definition(model, dir, &specific) {
        return truth::is_on(value);
    }
    let generic = [b"CMAKE_LINK_LIBRARY_USING_", feature, b"_SUPPORTED"].concat();
    definition(model, dir, &generic).is_some_and(|value| truth::is_on(value))
}

fn is_group_feature_supported(
    model: &ProjectModel,
    dir: DirId,
    language: &[u8],
    feature: &[u8],
) -> bool {
    let specific = [
        b"CMAKE_",
        language,
        b"_LINK_GROUP_USING_",
        feature,
        b"_SUPPORTED",
    ]
    .concat();
    if definition(model, dir, &specific).is_some_and(|value| truth::is_on(value)) {
        return true;
    }
    let generic = [b"CMAKE_LINK_GROUP_USING_", feature, b"_SUPPORTED"].concat();
    definition(model, dir, &generic).is_some_and(|value| truth::is_on(value))
}
