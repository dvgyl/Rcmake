//! Configure-time checks of `install()` destinations.

use rcmake_base::{BString, path_lex::Lexical, truth};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::Cx;
use rcmake_model::{TargetId, TargetKind};

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

/// Use the default include directory unless an applicable artifact has an
/// explicit destination. Warn when that destination suppresses a header list.
pub(super) fn drop_headers_without_destination<'a>(
    cx: &mut Cx<'_>,
    args: &[&[u8]],
    generic: Option<&BString>,
    mut snapshots: Vec<Snapshot<'a>>,
) -> Vec<Snapshot<'a>> {
    let found = destinations(args);
    let has_destination = |group: &[u8]| {
        generic.is_some_and(|path| !path.is_empty())
            || found
                .iter()
                .rev()
                .find(|(owner, _)| *owner == Some(group))
                .is_some_and(|(_, path)| !path.is_empty())
    };
    let apple = super::apple::is_apple(cx);
    let text_stubs = apple && !cx.var_or_empty(b"CMAKE_TAPI").is_empty();
    let dll = cx.var_is_on(b"WIN32") || cx.var_is_on(b"CYGWIN");
    let aix = cx.var_or_empty(b"CMAKE_SYSTEM_NAME") == b"AIX";
    for (id, entries) in &mut snapshots {
        let Some(target) = cx.project().target(*id) else {
            continue;
        };
        let on = |key: &[u8]| {
            target
                .props
                .get(key)
                .is_some_and(|value| truth::is_on(value))
        };
        if apple && on(b"FRAMEWORK") {
            continue;
        }
        let explicit_artifact = match target.kind {
            TargetKind::StaticLibrary => has_destination(b"ARCHIVE"),
            TargetKind::SharedLibrary if dll => {
                has_destination(b"ARCHIVE") || has_destination(b"RUNTIME")
            }
            TargetKind::SharedLibrary => {
                has_destination(b"LIBRARY")
                    || (text_stubs && on(b"ENABLE_EXPORTS") && has_destination(b"ARCHIVE"))
            }
            TargetKind::Executable => {
                (!(apple && on(b"MACOSX_BUNDLE")) && has_destination(b"RUNTIME"))
                    || ((dll || aix) && on(b"ENABLE_EXPORTS") && has_destination(b"ARCHIVE"))
            }
            _ => false,
        };
        if !explicit_artifact {
            continue;
        }
        let name = target.name.clone();
        for (key, value) in entries.iter_mut() {
            if !HEADERS.contains(key) || value.is_empty() || has_destination(key) {
                continue;
            }
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
