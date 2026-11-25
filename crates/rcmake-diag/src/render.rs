//! CMake 4.5.1 terminal diagnostic renderer and paragraph wrapper.
//!
//! Empirically verified against pinned CMake 4.5.1:
//! - Default line wrap width is **77 bytes** including the 2-space indent.
//! - Blank lines keep their spacing. Lines that start with a space do not wrap.
//! - Prose uses one space between words and two after a word ending in a period.
//! - Categorized developer warnings print `(author)`, `(deprecated)`, or `(policy)`
//!   with the matching suppression footer followed by one blank line; other framed
//!   diagnostics terminate with two trailing blank lines (`\n\n\n`).
//! - No-location diagnostics (`Location::None`) render `CMake Error: <body...>\n`.

use rcmake_base::BString;

use crate::{DiagCategory, Diagnostic, Frame, FrameKind, Location, MessageKind};

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
            out.extend_from_slice(&format_diagnostic_body(diag, opts.wrap_width));
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
            out.extend_from_slice(format_diagnostic_body(diag, opts.wrap_width).as_slice());
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

            out.extend_from_slice(format_diagnostic_body(diag, opts.wrap_width).as_slice());

            let callers = printed_callers(top, frames);
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

fn format_diagnostic_body(diag: &Diagnostic, width: usize) -> BString {
    format_paragraphs(diag.body.as_slice(), 2, width)
}

/// Select the caller frames that `cmMessenger::PrintCallStack` prints. A frame
/// without a command name (a file frame) is skipped when the frame printed before
/// it is in the same file, because that frame already gives a more specific line.
fn printed_callers<'a>(top: &'a Frame, frames: impl Iterator<Item = &'a Frame>) -> Vec<&'a Frame> {
    let mut last_file: &[u8] = top.file.as_ref();
    let mut callers = Vec::new();
    for frame in frames {
        if frame.name.is_none()
            && frame.kind != FrameKind::Deferred
            && frame.file.as_ref() == last_file
        {
            continue;
        }
        last_file = frame.file.as_ref();
        callers.push(frame);
    }
    callers
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
    let mut previous_preformatted = false;
    for line in text.split_inclusive(|&byte| byte == b'\n') {
        let raw_line = line.strip_suffix(b"\n").unwrap_or(line);
        let has_newline = raw_line.len() != line.len();
        let preformatted = raw_line.starts_with(b" ");
        if !preformatted && previous_preformatted {
            out.push(b'\n');
        }
        previous_preformatted = preformatted;
        if preformatted {
            out.extend(core::iter::repeat_n(b' ', indent));
            out.extend_from_slice(raw_line);
            if has_newline {
                out.push(b'\n');
            }
            continue;
        } else if raw_line.is_empty() {
            if has_newline {
                out.extend_from_slice(b"\n\n");
            }
            continue;
        }

        let mut col = 0usize;
        let mut line_started = false;
        let mut prev_ended_sentence = false;
        let mut i = 0usize;
        while i < raw_line.len() {
            while raw_line.get(i) == Some(&b' ') {
                i += 1;
            }
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
                let sep_len = if prev_ended_sentence { 2 } else { 1 };
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
        if has_newline {
            out.push(b'\n');
        }
    }

    if previous_preformatted {
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
    fn prose_spacing_and_wrap_preserve_preformatted_lines() {
        assert_eq!(
            format_paragraphs(
                b"alpha  beta. gamma     delta\ttab\n  /path  with spaces",
                2,
                80
            ),
            BString::from("  alpha beta.  gamma delta\ttab\n\n    /path  with spaces\n")
        );
        assert_eq!(
            format_paragraphs(b"abc  d", 2, 7),
            BString::from("  abc d\n")
        );
        assert_eq!(
            format_paragraphs(b"ab. d", 2, 7),
            BString::from("  ab.\n  d\n")
        );
    }

    #[test]
    fn preserves_blank_lines_between_paragraphs() {
        assert_eq!(
            format_paragraphs(b"alpha\n\nbeta\n\n\ngamma", 2, 77),
            "  alpha\n\n\n\n  beta\n\n\n\n\n\n  gamma\n"
        );
    }

    #[test]
    fn formats_generator_expression_errors_without_a_location() {
        assert_eq!(
            format_paragraphs(
                b"Error evaluating generator expression:\n  $<UNKNOWN>\nUnknown expression",
                2,
                77,
            ),
            "  Error evaluating generator expression:\n\n    $<UNKNOWN>\n\n  Unknown expression\n"
        );
    }

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
                "CMake Warning (deprecated) at /tmp/d.cmake:1 (message):\n  Line one.\n\n\n\n  Update value.  Or try this.\nThis warning is for project developers.  Use -Wno-author or -Wno-deprecated\nto suppress it.\n\n"
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

    fn frame(file: &str, line: u32, name: Option<&str>, kind: FrameKind) -> Frame {
        Frame {
            file: Arc::from(file.as_bytes()),
            line,
            name: name.map(|n| Arc::from(n.as_bytes())),
            kind,
        }
    }

    #[test]
    fn call_stack_skips_file_frames_like_cmake_print_call_stack() {
        let root = if cfg!(windows) { "C:/work" } else { "/work" };
        let lists = format!("{root}/CMakeLists.txt");
        let inc = format!("{root}/inc.cmake");
        let other = format!("{root}/other.cmake");
        let opts = RenderOptions {
            relative_base: Some(BString::from(root)),
            ..RenderOptions::default()
        };
        let diag = |bt: Backtrace| Diagnostic {
            annotations: None,
            kind: MessageKind::Error,
            effect: crate::Effect::None,
            category: None,
            policy: None,
            location: Location::Backtrace(bt),
            body: BString::from("Body."),
        };
        let included = Backtrace::default()
            .push(frame(&lists, 0, None, FrameKind::ModuleInclude))
            .push(frame(&lists, 3, Some("include"), FrameKind::Command))
            .push(frame(&inc, 0, None, FrameKind::ModuleInclude))
            .push(frame(
                &inc,
                2,
                Some("add_custom_target"),
                FrameKind::Command,
            ));
        assert_eq!(
            render(&diag(included), &opts),
            BString::from(
                "CMake Error at inc.cmake:2 (add_custom_target):\n  Body.\nCall Stack (most recent call first):\n  CMakeLists.txt:3 (include)\n\n\n"
            )
        );
        let foreign = Backtrace::default()
            .push(frame(&other, 0, None, FrameKind::ModuleInclude))
            .push(frame(
                &inc,
                2,
                Some("add_custom_target"),
                FrameKind::Command,
            ));
        assert_eq!(
            render(&diag(foreign), &opts),
            BString::from(
                "CMake Error at inc.cmake:2 (add_custom_target):\n  Body.\nCall Stack (most recent call first):\n  other.cmake\n\n\n"
            )
        );
    }
}
