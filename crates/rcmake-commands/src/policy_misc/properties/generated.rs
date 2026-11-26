use rcmake_base::{BString, truth};
use rcmake_diag::{DiagCategory, Effect, MessageKind, PolicyId, PolicyStatus};
use rcmake_eval::Cx;
use rcmake_model::{DirId, PropScope};

use crate::custom::{SOURCE_BINARY_ALTERNATIVE, bind_source};

use super::computed::boolean;
use super::scope::{
    RESOLVED_SOURCE_LOCATION, select_source, source_file_resolves_path, source_paths,
};
use super::set::Update;

const CMP0118: PolicyId = PolicyId::new(118);
const CMP0163: PolicyId = PolicyId::new(163);

pub(super) fn policy(cx: &mut Cx<'_>, dir: DirId, id: PolicyId) -> PolicyStatus {
    if dir != cx.current_dir_id()
        && let Some(policies) = cx.project().dir(dir).final_policies
    {
        return policies.get(id);
    }
    cx.policy(id)
}

pub(super) fn global_value(cx: &mut Cx<'_>, dir: DirId, source: &[u8]) -> Option<BString> {
    if policy(cx, dir, CMP0163) != PolicyStatus::New {
        return None;
    }
    let paths = source_paths(cx, source);
    Some(boolean(
        paths
            .iter()
            .any(|path| cx.project().is_globally_generated(path)),
    ))
}

pub(super) fn set(
    cx: &mut Cx<'_>,
    dir: DirId,
    source: &[u8],
    scoped: bool,
    value: &[u8],
    update: Update,
) -> bool {
    let policy_id = if policy(cx, dir, CMP0163) == PolicyStatus::New {
        CMP0163
    } else {
        CMP0118
    };
    let status = policy(cx, dir, policy_id);
    let appending = !matches!(update, Update::Set);
    let unset = truth::is_off(value);
    let non_boolean = !unset && !truth::is_on(value);
    if status == PolicyStatus::Warn {
        if non_boolean {
            cx.issue_policy_warning_in(dir, CMP0118, b"", &[
                b"Attempt to set property 'GENERATED' with the following non-boolean value (which will be interpreted as \"0\"):\n",
                value,
                b"\nThat exact value will not be retrievable. A value of \"0\" will be returned instead.\nThis will be an error under policy CMP0118.",
            ].concat());
        }
        if unset {
            cx.issue_policy_warning_in(
                dir,
                CMP0118,
                b"",
                b"Unsetting property 'GENERATED' will not be allowed under policy CMP0118!",
            );
        }
        if appending {
            cx.issue_policy_warning_in(
                dir,
                CMP0118,
                b"",
                b"Appending to property 'GENERATED' will not be allowed under policy CMP0118!",
            );
        }
    }
    if status != PolicyStatus::New {
        return false;
    }

    let diagnostic = if non_boolean {
        format!(
            "Policy {policy_id} is set to NEW and the following non-boolean value given for property 'GENERATED' is therefore not allowed:\n{}\nReplace it with a boolean value!\n",
            String::from_utf8_lossy(value)
        )
    } else if unset {
        format!("Unsetting the 'GENERATED' property is not allowed under {policy_id}!\n")
    } else if appending {
        format!(
            "Policy {policy_id} is set to NEW and appending to the 'GENERATED' property is therefore not allowed. Only setting it to \"1\" is allowed!\n"
        )
    } else {
        mark_source(cx, dir, source, scoped);
        return true;
    };
    let _ = select_source(cx, dir, source, scoped, true);
    cx.issue_for_directory(
        dir,
        MessageKind::AuthorWarning,
        Effect::None,
        Some(DiagCategory::Author),
        diagnostic.into(),
    );
    true
}

fn mark_source(cx: &mut Cx<'_>, dir: DirId, source: &[u8], scoped: bool) {
    let [source_path, binary_path] = source_paths(cx, source);
    let fallback =
        if scoped || source_path == binary_path || source_file_resolves_path(cx, &source_path) {
            source_path
        } else {
            binary_path
        };
    let (path, created) = if let Some(PropScope::Source { path, .. }) =
        select_source(cx, dir, source, scoped, false)
    {
        let unresolved = cx
            .project()
            .dir(dir)
            .source_props
            .get(&path)
            .is_some_and(|props| props.get(SOURCE_BINARY_ALTERNATIVE).is_some());
        if unresolved {
            bind_source(cx.project().dir_mut(dir), &path, &fallback);
            (fallback, false)
        } else {
            (path, false)
        }
    } else {
        (fallback, true)
    };
    let model = cx.project();
    if created {
        crate::custom::record_source_order(model.dir_mut(dir), &path, false);
    }
    model.mark_generated(dir, path.clone());
    model
        .dir_mut(dir)
        .source_props
        .entry(path.clone())
        .or_default()
        .set(RESOLVED_SOURCE_LOCATION.into(), path);
}

pub(super) fn is_strict(cx: &mut Cx<'_>, dir: DirId) -> bool {
    [CMP0163, CMP0118]
        .into_iter()
        .any(|id| policy(cx, dir, id) == PolicyStatus::New)
}
