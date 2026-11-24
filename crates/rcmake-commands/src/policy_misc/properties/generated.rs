use rcmake_base::truth;
use rcmake_diag::{DiagCategory, Effect, MessageKind, PolicyId, PolicyStatus};
use rcmake_eval::Cx;
use rcmake_model::DirId;

use super::computed::boolean;
use super::scope::{select_source, source_paths};
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

pub(super) fn global_value(
    cx: &mut Cx<'_>,
    dir: DirId,
    source: &[u8],
) -> Option<rcmake_base::BString> {
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
            cx.issue_policy_warning(CMP0118, b"", &[
                b"Attempt to set property 'GENERATED' with the following non-boolean value (which will be interpreted as \"0\"):\n",
                value,
                b"\nThat exact value will not be retrievable. A value of \"0\" will be returned instead.\nThis will be an error under policy CMP0118.",
            ].concat());
        }
        if unset {
            cx.issue_policy_warning(
                CMP0118,
                b"",
                b"Unsetting property 'GENERATED' will not be allowed under policy CMP0118!",
            );
        }
        if appending {
            cx.issue_policy_warning(
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
        let [source_path, binary_path] = source_paths(cx, source);
        let path = if let Some(rcmake_model::PropScope::Source { path, .. }) =
            select_source(cx, dir, source, scoped, false)
            && cx.project().dir(dir).source_props.contains_key(&path)
        {
            // An unscoped relative source with no resolved file uses the binary directory.
            if !scoped
                && path == source_path
                && !rcmake_host::path::to_native(source).is_absolute()
                && cx
                    .host()
                    .metadata(
                        &rcmake_host::path::to_native(&path),
                        rcmake_host::Follow::Yes,
                    )
                    .is_err()
            {
                binary_path
            } else {
                path
            }
        } else if scoped {
            source_path
        } else {
            binary_path
        };
        cx.project().mark_generated(dir, path);
        return true;
    };
    let _ = select_source(cx, dir, source, scoped, true);
    if dir == cx.current_dir_id() {
        cx.issue(
            MessageKind::AuthorWarning,
            Effect::None,
            Some(DiagCategory::Author),
            diagnostic.into(),
        );
    } else {
        cx.issue_in_directory(
            MessageKind::AuthorWarning,
            Effect::None,
            Some(DiagCategory::Author),
            diagnostic.into(),
        );
    }
    true
}

pub(super) fn is_strict(cx: &mut Cx<'_>, dir: DirId) -> bool {
    [CMP0163, CMP0118]
        .into_iter()
        .any(|id| policy(cx, dir, id) == PolicyStatus::New)
}
