//! Configure-time checks of `install()` destinations.

use rcmake_base::{BString, path_lex::Lexical, truth};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::Cx;
use rcmake_model::TargetId;

use super::{artifact, keyword};

const CMP0177: PolicyId = PolicyId::new(177);

/// Header properties that install to `CMAKE_INSTALL_INCLUDEDIR` by default.
const HEADERS: [&[u8]; 2] = [b"PUBLIC_HEADER", b"PRIVATE_HEADER"];

/// Snapshot of the install-relevant properties of one target.
pub(super) type Snapshot<'a> = (TargetId, [(&'a [u8], BString); 4]);

/// `CMP0177` `WARN`: report each `DESTINATION` that normalization would change. A
/// destination with a generator expression is not known before generation.
pub(super) fn warn_unnormalized(cx: &mut Cx<'_>, args: &[&[u8]]) {
    if cx.policy(CMP0177) != PolicyStatus::Warn {
        return;
    }
    for (_, path) in destinations(args) {
        if !path.is_empty()
            && !path.windows(2).any(|window| window == b"$<")
            && Lexical::new(path).normalize().as_slice() != path
        {
            cx.issue_policy_warning(CMP0177, b"", b"");
        }
    }
}

/// `install(TARGETS)` installs header files without a destination to the default
/// include directory only when the call names no artifact group. Otherwise CMake
/// warns and does not install those headers.
pub(super) fn drop_headers_without_destination<'a>(
    cx: &mut Cx<'_>,
    args: &[&[u8]],
    mut snapshots: Vec<Snapshot<'a>>,
) -> Vec<Snapshot<'a>> {
    let found = destinations(args);
    let artifacts_specified = args.iter().any(|token| artifact(token));
    let has_destination = |group: &[u8]| {
        found
            .iter()
            .any(|(owner, path)| !path.is_empty() && owner.is_none_or(|owner| owner == group))
    };
    if !artifacts_specified {
        return snapshots;
    }
    for (id, entries) in &mut snapshots {
        let framework = entries
            .iter()
            .any(|(key, value)| *key == b"FRAMEWORK" && truth::is_on(value));
        if framework {
            continue;
        }
        for (key, value) in entries.iter_mut() {
            if !HEADERS.contains(key) || value.is_empty() || has_destination(key) {
                continue;
            }
            let name = cx
                .project()
                .target(*id)
                .map(|target| target.name.clone())
                .unwrap_or_default();
            let key_text = String::from_utf8_lossy(key);
            crate::author_warning(
                cx,
                format!(
                    "Target {} has {key_text} files but no {key_text} DESTINATION.",
                    String::from_utf8_lossy(&name)
                ),
            );
            *value = BString::default();
        }
    }
    snapshots
}

/// Each `DESTINATION` value with the artifact group that owns it (`None` for the
/// generic destination). `INCLUDES DESTINATION` is not an install destination.
fn destinations<'a>(args: &[&'a [u8]]) -> Vec<(Option<&'a [u8]>, &'a [u8])> {
    let mut found = Vec::new();
    let mut group = None;
    let mut previous: &[u8] = b"";
    let mut tokens = args.iter().copied().peekable();
    while let Some(token) = tokens.next() {
        if artifact(token) {
            group = Some(token);
        } else if token == b"DESTINATION"
            && previous != b"INCLUDES"
            && let Some(path) = tokens.next_if(|path| !keyword(path))
        {
            found.push((group, path));
            previous = path;
            continue;
        }
        previous = token;
    }
    found
}
