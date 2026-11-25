//! CMake 4.5.1 terminal diagnostic renderer and paragraph wrapper.
//!
//! Empirically verified against pinned CMake 4.5.1:
//! - Default line wrap width is **77 bytes** including the 2-space indent.
//! - Consecutive blank lines collapse to a single paragraph separator; lines starting
//!   with a space are emitted preformatted without wrapping.
//! - Double spaces in prose (such as after sentence-ending periods) are preserved.
//! - Categorized developer warnings print `(author)`, `(deprecated)`, or `(policy)`
//!   with the matching suppression footer followed by one blank line; other framed
//!   diagnostics terminate with two trailing blank lines (`\n\n\n`).
//! - No-location diagnostics (`Location::None`) render `CMake Error: <body...>\n`.

use rcmake_base::BString;

use crate::{DiagCategory, Diagnostic, FrameKind, Location, MessageKind};

/// Options controlling terminal rendering of a [`Diagnostic`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderOptions {
    /// If set, file paths under this prefix are rendered relative to it (e.g., `-P` cwd).
    pub relative_base: Option<BString>,
    /// Target wrap width in bytes (including indent). CMake 4.5.1 uses `77`.
    pub wrap_width: usize,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            relative_base: None,
            wrap_width: 77,
        }
    }
}

/// Render a [`Diagnostic`] to exact CMake 4.5.1 terminal bytes.
#[must_use]
pub fn render(diag: &Diagnostic, opts: &RenderOptions) -> BString {
    let prefix = header_prefix(diag);
    let base = opts.relative_base.as_ref().map(|b| b.as_slice());
    let mut out = Vec::new();

    match &diag.location {
        Location::None if diag.policy == Some(crate::PolicyId::new(224)) => {
            out.extend_from_slice(prefix);
            out.extend_from_slice(b":\n");
            out.extend_from_slice(&format_paragraphs(&diag.body, 2, opts.wrap_width));
            append_footer_and_trailer(diag, &mut out);
        }
        Location::None => {
            out.extend_from_slice(prefix);
            out.extend_from_slice(b": ");
            out.extend_from_slice(diag.body.as_slice());
            if !out.ends_with(b"\n") {
                out.push(b'\n');
            }
            return BString::from(out);
        }
        Location::FileLine { file, line } => {
            let display_file = display_path(file.as_ref(), base);
            out.extend_from_slice(prefix);
            if *line == 0 {
                out.extend_from_slice(b" in ");
                out.extend_from_slice(display_file);
                out.extend_from_slice(b":\n");
            } else {
                out.extend_from_slice(b" at ");
                out.extend_from_slice(display_file);
                out.push(b':');
                out.extend_from_slice(line.to_string().as_bytes());
                out.extend_from_slice(b":\n");
            }
            out.extend_from_slice(
                format_paragraphs(diag.body.as_slice(), 2, opts.wrap_width).as_slice(),
            );
            append_footer_and_trailer(diag, &mut out);
        }
        Location::Backtrace(bt) => {
            let mut frames = bt.frames();
            let Some(top) = frames.next() else {
                out.extend_from_slice(prefix);
                out.extend_from_slice(b": ");
                out.extend_from_slice(diag.body.as_slice());
                if !out.ends_with(b"\n") {
                    out.push(b'\n');
                }
                return BString::from(out);
            };

            let display_file = display_path(top.file.as_ref(), base);
            out.extend_from_slice(prefix);
            if top.kind == FrameKind::Deferred {
                out.extend_from_slice(b" at ");
                out.extend_from_slice(display_file);
                out.extend_from_slice(b":DEFERRED");
            } else if top.line == 0 {
                out.extend_from_slice(b" in ");
                out.extend_from_slice(display_file);
            } else {
                out.extend_from_slice(b" at ");
                out.extend_from_slice(display_file);
                out.push(b':');
                out.extend_from_slice(top.line.to_string().as_bytes());
            }
            if let Some(name) = &top.name {
                out.extend_from_slice(b" (");
                out.extend_from_slice(name.as_ref());
                out.push(b')');
            }
            out.extend_from_slice(b":\n");

            out.extend_from_slice(
                format_paragraphs(diag.body.as_slice(), 2, opts.wrap_width).as_slice(),
            );

            let callers: Vec<_> = frames.collect();
            if !callers.is_empty() {
                out.extend_from_slice(b"Call Stack (most recent call first):\n");
                for frame in callers {
                    let cf = display_path(frame.file.as_ref(), base);
                    out.extend_from_slice(b"  ");
                    out.extend_from_slice(cf);
                    if frame.kind == FrameKind::Deferred {
                        out.extend_from_slice(b":DEFERRED");
                    } else if frame.line > 0 {
                        out.push(b':');
                        out.extend_from_slice(frame.line.to_string().as_bytes());
                    }
                    if let Some(name) = &frame.name {
                        out.extend_from_slice(b" (");
                        out.extend_from_slice(name.as_ref());
                        out.push(b')');
                    }
                    out.push(b'\n');
                }
            }

            append_footer_and_trailer(diag, &mut out);
        }
    }

    BString::from(out)
}

fn append_footer_and_trailer(diag: &Diagnostic, out: &mut Vec<u8>) {
    if let Some(footer) = warning_footer(diag) {
        out.extend_from_slice(footer);
        out.push(b'\n');
    } else {
        out.extend_from_slice(b"\n\n");
    }
}

fn warning_footer(diag: &Diagnostic) -> Option<&'static [u8]> {
    match diag.category {
        Some(DiagCategory::Policy) if is_warning_kind(diag.kind) => Some(
            b"This warning is for project developers.  Use -Wno-author or -Wno-policy to\nsuppress it.\n",
        ),
        Some(DiagCategory::Policy) if is_error_kind(diag.kind) => Some(
            b"This error is for project developers.  Use -Wno-error=author or\n-Wno-error=policy to suppress it.\n",
        ),
        Some(DiagCategory::Deprecated) if is_warning_kind(diag.kind) => Some(
            b"This warning is for project developers.  Use -Wno-author or -Wno-deprecated\nto suppress it.\n",
        ),
        Some(DiagCategory::Deprecated) if is_error_kind(diag.kind) => Some(
            b"This error is for project developers.  Use -Wno-error=author or\n-Wno-error=deprecated to suppress it.\n",
        ),
        Some(DiagCategory::Author) if is_warning_kind(diag.kind) => {
            Some(b"This warning is for project developers.  Use -Wno-author to suppress it.\n")
        }
        Some(DiagCategory::Author) if is_error_kind(diag.kind) => {
            Some(b"This error is for project developers.  Use -Wno-error=author to suppress it.\n")
        }
        Some(DiagCategory::Experimental) if is_warning_kind(diag.kind) => Some(
            b"This warning is for project developers.  Use -Wno-author or -Wno-experimental\nto suppress it.\n",
        ),
        Some(DiagCategory::Experimental) if is_error_kind(diag.kind) => Some(
            b"This error is for project developers.  Use -Wno-error=author or\n-Wno-error=experimental to suppress it.\n",
        ),
        Some(DiagCategory::NonTargetDirective) if is_warning_kind(diag.kind) => Some(
            b"This warning is for project developers.  Use -Wno-author or\n-Wno-non-target-directive to suppress it.\n",
        ),
        Some(DiagCategory::NonTargetDirective) if is_error_kind(diag.kind) => Some(
            b"This error is for project developers.  Use -Wno-error=author or\n-Wno-error=non-target-directive to suppress it.\n",
        ),
        Some(DiagCategory::InstallAbsoluteDestination) if is_warning_kind(diag.kind) => Some(
            b"This warning is for project developers.  Use -Wno-author or\n-Wno-install-absolute-destination to suppress it.\n",
        ),
        Some(DiagCategory::InstallAbsoluteDestination) if is_error_kind(diag.kind) => Some(
            b"This error is for project developers.  Use -Wno-error=author or\n-Wno-error=install-absolute-destination to suppress it.\n",
        ),
        _ => match diag.kind {
            MessageKind::AuthorWarning => {
                Some(b"This warning is for project developers.  Use -Wno-author to suppress it.\n")
            }
            MessageKind::AuthorError => Some(
                b"This error is for project developers.  Use -Wno-error=author to suppress it.\n",
            ),
            MessageKind::DeprecationWarning => Some(
                b"This warning is for project developers.  Use -Wno-author or -Wno-deprecated\nto suppress it.\n",
            ),
            MessageKind::DeprecationError => Some(
                b"This error is for project developers.  Use -Wno-error=author or\n-Wno-error=deprecated to suppress it.\n",
            ),
            _ => None,
        },
    }
}

const fn is_warning_kind(kind: MessageKind) -> bool {
    matches!(
        kind,
        MessageKind::Warning | MessageKind::AuthorWarning | MessageKind::DeprecationWarning
    )
}

const fn is_error_kind(kind: MessageKind) -> bool {
    matches!(
        kind,
        MessageKind::Error | MessageKind::AuthorError | MessageKind::DeprecationError
    )
}

/// Clean-room implementation of CMake's paragraph formatter (`indent` leading spaces,
/// wrapping non-preformatted paragraphs to at most `width` bytes per line).
#[must_use]
pub fn format_paragraphs(text: &[u8], indent: usize, width: usize) -> BString {
    let mut out = Vec::new();
    let mut wrote_any = false;
    let mut prev_preformatted = false;

    for raw_line in text.split(|&b| b == b'\n') {
        if raw_line.is_empty() {
            continue;
        }
        let is_pre = raw_line.first() == Some(&b' ');
        if wrote_any && !(is_pre && prev_preformatted) {
            out.push(b'\n');
        }
        wrote_any = true;
        prev_preformatted = is_pre;

        if is_pre {
            out.extend(core::iter::repeat_n(b' ', indent));
            out.extend_from_slice(raw_line);
            out.push(b'\n');
            continue;
        }

        let mut col = 0usize;
        let mut line_started = false;
        let mut prev_ended_sentence = false;
        let mut i = 0usize;
        while i < raw_line.len() {
            let space_start = i;
            while raw_line.get(i) == Some(&b' ') {
                i += 1;
            }
            let raw_spaces = i - space_start;
            if i >= raw_line.len() {
                break;
            }
            let start = i;
            while i < raw_line.len() && raw_line.get(i) != Some(&b' ') {
                i += 1;
            }
            let word = raw_line.get(start..i).unwrap_or(&[]);
            if !line_started {
                out.extend(core::iter::repeat_n(b' ', indent));
                out.extend_from_slice(word);
                col = indent + word.len();
                line_started = true;
            } else {
                let sep_len = if prev_ended_sentence || raw_spaces >= 2 {
                    2
                } else {
                    1
                };
                if col + sep_len + word.len() <= width {
                    out.extend(core::iter::repeat_n(b' ', sep_len));
                    out.extend_from_slice(word);
                    col += sep_len + word.len();
                } else {
                    out.push(b'\n');
                    out.extend(core::iter::repeat_n(b' ', indent));
                    out.extend_from_slice(word);
                    col = indent + word.len();
                }
            }
            prev_ended_sentence = word.ends_with(b".");
        }
        if line_started {
            out.push(b'\n');
        }
    }

    if wrote_any && text.ends_with(b"\n") {
        out.push(b'\n');
    }

    BString::from(out)
}

fn header_prefix(diag: &Diagnostic) -> &'static [u8] {
    if let Some(cat) = diag.category {
        let is_err = matches!(
            diag.kind,
            MessageKind::Error
                | MessageKind::AuthorError
                | MessageKind::DeprecationError
                | MessageKind::InternalError
        );
        return match (cat, is_err) {
            (DiagCategory::Author, false) => b"CMake Warning (author)",
            (DiagCategory::Author, true) => b"CMake Error (author)",
            (DiagCategory::Deprecated, false) => b"CMake Warning (deprecated)",
            (DiagCategory::Deprecated, true) => b"CMake Error (deprecated)",
            (DiagCategory::Policy, false) => b"CMake Warning (policy)",
            (DiagCategory::Policy, true) => b"CMake Error (policy)",
            (DiagCategory::Experimental, false) => b"CMake Warning (experimental)",
            (DiagCategory::Experimental, true) => b"CMake Error (experimental)",
            (DiagCategory::Strict, false) => b"CMake Warning (strict)",
            (DiagCategory::Strict, true) => b"CMake Error (strict)",
            (DiagCategory::NonTargetDirective, false) => b"CMake Warning (non-target-directive)",
            (DiagCategory::NonTargetDirective, true) => b"CMake Error (non-target-directive)",
            (DiagCategory::InstallAbsoluteDestination, false) => {
                b"CMake Warning (install-absolute-destination)"
            }
            (DiagCategory::InstallAbsoluteDestination, true) => {
                b"CMake Error (install-absolute-destination)"
            }
            (DiagCategory::Uninitialized, false) => b"CMake Warning (uninitialized)",
            (DiagCategory::Uninitialized, true) => b"CMake Error (uninitialized)",
            (DiagCategory::UnusedCli, false) => b"CMake Warning (unused-cli)",
            (DiagCategory::UnusedCli, true) => b"CMake Error (unused-cli)",
        };
    }

    match diag.kind {
        MessageKind::Error => b"CMake Error",
        MessageKind::InternalError => b"CMake Internal Error",
        MessageKind::Warning => b"CMake Warning",
        MessageKind::AuthorWarning => b"CMake Warning (author)",
        MessageKind::AuthorError => b"CMake Error (author)",
        MessageKind::DeprecationWarning => b"CMake Warning (deprecated)",
        MessageKind::DeprecationError => b"CMake Error (deprecated)",
        MessageKind::Log => b"CMake Debug Log",
    }
}

fn display_path<'a>(path: &'a [u8], relative_base: Option<&[u8]>) -> &'a [u8] {
    let Some(base) = relative_base else {
        return path;
    };
    let trimmed_base = base.strip_suffix(b"/").unwrap_or(base);
    if let Some(rest) = path.strip_prefix(trimmed_base)
        && let Some(rel) = rest.strip_prefix(b"/")
        && !rel.is_empty()
    {
        return rel;
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Backtrace, Frame, FrameKind};
    use std::sync::Arc;

    #[test]
    fn wrap_width_77_matches_cmake_4_5_1_probe() {
        let body = BString::from(
            "1234567890 1234567890 1234567890 1234567890 1234567890 1234567890 1234567890 1234567890 1234567890",
        );
        let bt = Backtrace::default().push(Frame {
            file: Arc::from(&b"/tmp/p4.cmake"[..]),
            line: 1,
            name: Some(Arc::from(&b"message"[..])),
            kind: FrameKind::Command,
        });
        let d = Diagnostic {
            annotations: None,
            kind: MessageKind::Warning,
            effect: crate::Effect::None,
            category: None,
            policy: None,
            location: Location::Backtrace(bt),
            body,
        };
        let rendered = render(&d, &RenderOptions::default());
        assert_eq!(
            rendered,
            BString::from(
                "CMake Warning at /tmp/p4.cmake:1 (message):\n  1234567890 1234567890 1234567890 1234567890 1234567890 1234567890\n  1234567890 1234567890 1234567890\n\n\n"
            )
        );
    }

    #[test]
    fn author_and_deprecated_warnings_match_cmake_4_5_1() {
        let bt = Backtrace::default().push(Frame {
            file: Arc::from(&b"/tmp/d.cmake"[..]),
            line: 1,
            name: Some(Arc::from(&b"message"[..])),
            kind: FrameKind::Command,
        });
        let author = Diagnostic {
            annotations: None,
            kind: MessageKind::AuthorWarning,
            effect: crate::Effect::None,
            category: Some(DiagCategory::Author),
            policy: None,
            location: Location::Backtrace(bt.clone()),
            body: BString::from("First sentence. Second sentence."),
        };
        assert_eq!(
            render(&author, &RenderOptions::default()),
            BString::from(
                "CMake Warning (author) at /tmp/d.cmake:1 (message):\n  First sentence.  Second sentence.\nThis warning is for project developers.  Use -Wno-author to suppress it.\n\n"
            )
        );

        let dep = Diagnostic {
            annotations: None,
            kind: MessageKind::DeprecationWarning,
            effect: crate::Effect::None,
            category: Some(DiagCategory::Deprecated),
            policy: None,
            location: Location::Backtrace(bt),
            body: BString::from("Line one.\n\nUpdate value.  Or try this."),
        };
        assert_eq!(
            render(&dep, &RenderOptions::default()),
            BString::from(
                "CMake Warning (deprecated) at /tmp/d.cmake:1 (message):\n  Line one.\n\n  Update value.  Or try this.\nThis warning is for project developers.  Use -Wno-author or -Wno-deprecated\nto suppress it.\n\n"
            )
        );
    }

    #[test]
    fn no_location_error_has_no_indent_or_extra_newlines() {
        let d = Diagnostic {
            annotations: None,
            kind: MessageKind::Error,
            effect: crate::Effect::Fatal,
            category: None,
            policy: None,
            location: Location::None,
            body: BString::from("Not a file: /tmp/missing.cmake"),
        };
        assert_eq!(
            render(&d, &RenderOptions::default()),
            BString::from("CMake Error: Not a file: /tmp/missing.cmake\n")
        );
    }
}
