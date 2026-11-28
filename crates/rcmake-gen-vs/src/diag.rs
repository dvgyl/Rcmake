//! Generator diagnostics written to standard error.

use rcmake_base::{BString, truth};
use rcmake_diag::{DiagCategory, Diagnostic, Effect, Location, MessageKind, RenderOptions};
use rcmake_host::{Host, Stream};
use rcmake_model::{DirId, ProjectModel};

/// Render an author warning for the listfile of directory `dir`.
///
/// CMake issues these warnings for a directory without a call stack, so the
/// location is `in <dir>/CMakeLists.txt:`. The path is relative to the
/// top-level source directory. The result is `None` when developer warnings
/// are off (`-Wno-dev`).
pub(crate) fn author_warning(
    model: &ProjectModel,
    dir: DirId,
    body: impl Into<BString>,
) -> Option<BString> {
    if model
        .definition(DirId::ROOT, b"CMAKE_SUPPRESS_DEVELOPER_WARNINGS")
        .is_some_and(|value| truth::is_on(value))
    {
        return None;
    }
    let root = &model.dir(DirId::ROOT).source_dir;
    let source = &model.dir(dir).source_dir;
    let relative = source
        .strip_prefix(root.as_slice())
        .map(|tail| tail.strip_prefix(b"/").unwrap_or(tail));
    let file: Vec<u8> = match relative {
        Some(tail) if !tail.is_empty() => [tail, b"/CMakeLists.txt"].concat(),
        Some(_) => b"CMakeLists.txt".to_vec(),
        None => [source.as_slice(), b"/CMakeLists.txt"].concat(),
    };
    Some(rcmake_diag::render(
        &Diagnostic {
            annotations: None,
            kind: MessageKind::AuthorWarning,
            effect: Effect::None,
            category: Some(DiagCategory::Author),
            policy: None,
            location: Location::FileLine {
                file: file.as_slice().into(),
                line: 0,
            },
            body: body.into(),
        },
        &RenderOptions::default(),
    ))
}

/// Write an author warning for directory `dir` to standard error.
pub(crate) fn emit_author_warning(
    host: &dyn Host,
    model: &ProjectModel,
    dir: DirId,
    body: impl Into<BString>,
) {
    if let Some(text) = author_warning(model, dir, body) {
        host.write_out(Stream::Stderr, &text);
    }
}
