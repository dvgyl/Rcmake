//! CMake 4.5.1 terminal diagnostic renderer and paragraph wrapper.
//!
//! Empirically verified against pinned CMake 4.5.1:
//! - Default line wrap width is **77 bytes** including the 2-space indent.
//! - Each `\n` starts a new paragraph separated by a blank line; lines starting with
//!   a space are emitted preformatted without wrapping.
//! - Framed diagnostics terminate with `\n\n\n` (two trailing blank lines).
//! - No-location diagnostics (`Location::None`) render `CMake Error: <body...>\n`.

use rcmake_base::BString;

use crate::{Diagnostic, Location, MessageKind};

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
    let prefix = kind_prefix(diag.kind);
    let mut out = Vec::new();

    match &diag.location {
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
            let display_file = display_path(
                file.as_ref(),
                opts.relative_base.as_ref().map(|b| b.as_slice()),
            );
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
            out.extend_from_slice(b"\n\n");
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

            let display_file = display_path(
                top.file.as_ref(),
                opts.relative_base.as_ref().map(|b| b.as_slice()),
            );
            out.extend_from_slice(prefix);
            if top.line == 0 {
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
                    let cf = display_path(
                        frame.file.as_ref(),
                        opts.relative_base.as_ref().map(|b| b.as_slice()),
                    );
                    out.extend_from_slice(b"  ");
                    out.extend_from_slice(cf);
                    if frame.line > 0 {
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

            if matches!(
                diag.kind,
                MessageKind::AuthorWarning | MessageKind::AuthorError
            ) {
                out.extend_from_slice(
                    b"This warning is for project developers.  Use -Wno-dev to suppress it.\n",
                );
            }
            out.extend_from_slice(b"\n\n");
        }
    }

    BString::from(out)
}

/// Clean-room implementation of CMake's paragraph formatter (`indent` leading spaces,
/// wrapping non-preformatted paragraphs to at most `width` bytes per line).
#[must_use]
pub fn format_paragraphs(text: &[u8], indent: usize, width: usize) -> BString {
    let mut out = Vec::new();
    let mut first_para = true;

    for raw_line in text.split(|&b| b == b'\n') {
        if !first_para {
            out.push(b'\n');
        }
        first_para = false;

        if raw_line.is_empty() {
            continue;
        }
        if raw_line.first() == Some(&b' ') {
            out.extend(core::iter::repeat_n(b' ', indent));
            out.extend_from_slice(raw_line);
            out.push(b'\n');
            continue;
        }

        let mut col = 0usize;
        let mut line_started = false;
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
            } else if col + 1 + word.len() <= width {
                out.push(b' ');
                out.extend_from_slice(word);
                col += 1 + word.len();
            } else {
                out.push(b'\n');
                out.extend(core::iter::repeat_n(b' ', indent));
                out.extend_from_slice(word);
                col = indent + word.len();
            }
        }
        if line_started {
            out.push(b'\n');
        }
    }

    BString::from(out)
}

fn kind_prefix(kind: MessageKind) -> &'static [u8] {
    match kind {
        MessageKind::Error => b"CMake Error",
        MessageKind::InternalError => b"CMake Internal Error",
        MessageKind::Warning => b"CMake Warning",
        MessageKind::AuthorWarning => b"CMake Warning (dev)",
        MessageKind::AuthorError => b"CMake Error (dev)",
        MessageKind::DeprecationWarning => b"CMake Deprecation Warning",
        MessageKind::DeprecationError => b"CMake Deprecation Error",
        MessageKind::Log => b"CMake",
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
    fn no_location_error_has_no_indent_or_extra_newlines() {
        let d = Diagnostic {
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
