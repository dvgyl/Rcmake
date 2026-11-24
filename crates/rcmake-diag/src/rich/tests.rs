use std::sync::Arc;

use crate::annotate::{Confidence, Edit, Fix};
use crate::{Backtrace, Effect, PolicyId, source::SourceKind};

use super::*;

fn add(map: &mut SourceMap, path: &str, text: &[u8]) -> Result<SourceId, &'static str> {
    map.add(path.into(), SourceKind::File, Arc::from(text))
        .ok_or("Cannot register source")
}

fn diagnostic(location: Location) -> Diagnostic {
    Diagnostic {
        kind: MessageKind::Error,
        effect: Effect::Fatal,
        category: None,
        policy: None,
        location,
        body: "Unknown command.".into(),
        annotations: None,
    }
}

fn span(source: SourceId, start: u32, end: u32) -> SourceSpan {
    SourceSpan { source, start, end }
}

fn ascii() -> RichOptions {
    RichOptions {
        unicode: false,
        ..RichOptions::default()
    }
}

#[test]
fn tabs_crlf_and_invalid_utf8_align_markers() -> Result<(), &'static str> {
    let mut map = SourceMap::default();
    let source = add(&mut map, "CMakeLists.txt", b"x\tm\xffs\r\nok\r\n")?;
    let annotations = Annotations::primary(span(source, 2, 5), "first line")
        .with_secondary(span(source, 7, 9), "second line");
    let result = render_rich(
        &diagnostic(Location::None),
        Some(&annotations),
        &map,
        ascii(),
    );
    assert_eq!(
        result.as_slice(),
        concat!(
            "error[CMake.FatalError]: Unknown command.\n",
            " --> CMakeLists.txt:1:3\n",
            "  |\n",
            "1 | x   m�s\n",
            "  |     ^^^ first line\n",
            "2 | ok\n",
            "  | -- second line\n",
            "  |\n",
        )
        .as_bytes(),
    );
    Ok(())
}

#[test]
fn offsets_inside_a_utf8_scalar_mark_the_scalar() -> Result<(), &'static str> {
    let mut map = SourceMap::default();
    let source = add(&mut map, "input", "aéz".as_bytes())?;
    let annotations = Annotations::primary(span(source, 2, 3), "scalar");
    let result = render_rich(
        &diagnostic(Location::None),
        Some(&annotations),
        &map,
        ascii(),
    );
    let text = result.to_str_lossy();
    assert!(text.contains(" --> input:1:2\n"));
    assert!(text.contains("1 | aéz\n  |  ^ scalar\n"));
    Ok(())
}

#[test]
fn eof_empty_input_and_reverse_ranges_have_point_markers() -> Result<(), &'static str> {
    let mut map = SourceMap::default();
    let empty = add(&mut map, "empty", b"")?;
    let final_newline = add(&mut map, "newline", b"set(a)\n")?;
    let no_newline = add(&mut map, "no-newline", b"set(b)")?;
    let annotations = Annotations::primary(span(empty, 0, 0), "empty input")
        .with_secondary(span(final_newline, u32::MAX, u32::MAX), "past EOF")
        .with_secondary(span(no_newline, 6, 3), "reversed range");
    let result = render_rich(
        &diagnostic(Location::None),
        Some(&annotations),
        &map,
        ascii(),
    );
    let text = result.to_str_lossy();
    assert!(text.contains(" --> empty:1:1\n  |\n1 | \n  | ^ empty input\n"));
    assert!(text.contains(" --> newline:2:1\n  |\n2 | \n  | - past EOF\n"));
    assert!(text.contains(" --> no-newline:1:7\n  |\n1 | set(b)\n  |       - reversed range\n"));
    Ok(())
}

#[test]
fn multiline_overlapping_labels_share_source_lines() -> Result<(), &'static str> {
    let mut map = SourceMap::default();
    let source = add(&mut map, "input", b"aa\nbb\ncc\n")?;
    let mut annotations = Annotations::primary(span(source, 1, 7), "multiline")
        .with_secondary(span(source, 3, 5), "whole line")
        .with_secondary(span(source, 6, 8), "overlap");
    annotations.labels.push(Label {
        span: span(source, 4, 5),
        message: "another primary".into(),
        primary: true,
    });
    let result = render_rich(
        &diagnostic(Location::None),
        Some(&annotations),
        &map,
        ascii(),
    );
    let text = result.to_str_lossy();
    assert_eq!(text.matches("2 | bb\n").count(), 1);
    assert!(text.contains("1 | aa\n  |  ^\n"));
    assert!(text.contains("2 | bb\n  | ^^\n  | -- whole line\n  |  ^ another primary\n"));
    assert!(text.contains("3 | cc\n  | ^ multiline\n  | -- overlap\n"));
    Ok(())
}

#[test]
fn long_spans_skip_middle_lines_and_align_line_numbers() -> Result<(), &'static str> {
    let mut map = SourceMap::default();
    let source = add(&mut map, "input", b"a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n")?;
    let annotations = Annotations::primary(span(source, 0, 19), "");
    let result = render_rich(
        &diagnostic(Location::None),
        Some(&annotations),
        &map,
        ascii(),
    );
    let text = result.to_str_lossy();
    assert!(text.contains(" 1 | a\n   | ^\n   | ...\n10 | j\n   | ^\n"));
    Ok(())
}

#[test]
fn narrow_terminal_keeps_distant_labels_visible() -> Result<(), &'static str> {
    let mut map = SourceMap::default();
    let source = add(&mut map, "a", b"a012345678901234567890123456789z")?;
    let annotations =
        Annotations::primary(span(source, 0, 1), "one").with_secondary(span(source, 31, 32), "two");
    let opts = RichOptions {
        terminal_width: 20,
        ..ascii()
    };
    let result = render_rich(&diagnostic(Location::None), Some(&annotations), &map, opts);
    let text = result.to_str_lossy();
    assert!(text.contains("1 | a012345678901...\n  | ^ one\n"));
    assert!(text.contains("1 | ..."));
    assert!(text.contains("z\n"));
    assert!(text.contains("two\n"));
    assert!(
        text.lines()
            .filter(|line| line.starts_with("1 |"))
            .all(|line| line.len() <= 20)
    );
    Ok(())
}

#[test]
fn fix_preview_preserves_context_for_replacement_and_insertion() -> Result<(), &'static str> {
    let mut map = SourceMap::default();
    let source = add(&mut map, "input", b"mesage(ok)\r\n")?;
    let annotations = Annotations::default()
        .with_fix(Fix {
            message: "Use message.".into(),
            edits: vec![Edit {
                span: span(source, 0, 6),
                replacement: "message".into(),
            }],
            confidence: Confidence::MachineApplicable,
        })
        .with_fix(Fix {
            message: "Insert a newline.".into(),
            edits: vec![Edit {
                span: span(source, 7, 7),
                replacement: "\n".into(),
            }],
            confidence: Confidence::MaybeIncorrect,
        })
        .with_note("Check the spelling.");
    let result = render_rich(
        &diagnostic(Location::None),
        Some(&annotations),
        &map,
        ascii(),
    );
    let text = result.to_str_lossy();
    assert!(text.contains(
        "  = help: Use message.\n    --> input:1:4\n    - mesage(ok)\n    + message(ok)\n"
    ));
    assert!(text.contains("  = help (review required): Insert a newline.\n"));
    assert!(text.contains("    --> input:1:8\n    - mesage(ok)\n    + mesage(\n    + ok)\n"));
    assert!(text.contains("  = note: Check the spelling.\n"));
    Ok(())
}

#[test]
fn color_and_gutters_are_independent_and_controls_are_safe() -> Result<(), &'static str> {
    let mut map = SourceMap::default();
    let source = add(&mut map, "input", b"\x1b[0m")?;
    let annotations = Annotations::primary(span(source, 0, 1), "");
    let mut diag = diagnostic(Location::None);
    diag.kind = MessageKind::Warning;
    diag.category = Some(DiagCategory::Policy);
    let plain = render_rich(&diag, Some(&annotations), &map, RichOptions::default());
    let plain = plain.to_str_lossy();
    assert!(plain.starts_with("warning[CMake.Policy]: Unknown command.\n"));
    assert!(plain.contains(" ╭─▶ input:1:1\n"));
    assert!(plain.contains("1 │ �[0m\n"));
    assert!(!plain.contains('\x1b'));
    let colored = render_rich(
        &diag,
        Some(&annotations),
        &map,
        RichOptions {
            color: true,
            ..ascii()
        },
    );
    let colored = colored.to_str_lossy();
    assert!(colored.contains("\x1b[33mwarning[CMake.Policy]:\x1b[0m"));
    assert!(colored.contains("\x1b[33m^\x1b[0m"));
    assert!(colored.contains("1 | �[0m\n"));
    Ok(())
}

struct Lookup {
    map: SourceMap,
    source: SourceId,
    show_bundled: bool,
}

impl SourceLookup for Lookup {
    fn source(&self, id: SourceId) -> Option<&SourceFile> {
        self.map.get(id)
    }

    fn source_by_path(&self, path: &[u8]) -> Option<&SourceFile> {
        self.map
            .get(self.source)
            .filter(|file| file.path.as_slice() == path)
    }

    fn is_bundled_module(&self, path: &[u8]) -> bool {
        path.starts_with(b"/bundle/Modules/")
    }

    fn show_bundled_frames(&self) -> bool {
        self.show_bundled
    }
}

#[test]
fn fallback_underlines_command_and_stack_groups_bundled_frames() -> Result<(), &'static str> {
    let mut map = SourceMap::default();
    let source = add(&mut map, "script.cmake", b"\tMeSage()\n")?;
    let mut lookup = Lookup {
        map,
        source,
        show_bundled: true,
    };
    let frame = |file: &[u8], line, name: &[u8]| Frame {
        file: Arc::from(file),
        line,
        name: Some(Arc::from(name)),
        kind: FrameKind::Command,
    };
    let stack = Backtrace::default()
        .push(frame(b"project with spaces/root.cmake", 10, b"include"))
        .push(frame(b"/bundle/Modules/B.cmake", 1, b"include"))
        .push(frame(b"/bundle/Modules/A.cmake", 2, b"include"))
        .push(frame(b"script.cmake", 1, b"mesage"));
    let diag = diagnostic(Location::Backtrace(stack));
    let result = render_rich(&diag, None, &lookup, ascii());
    let text = result.to_str_lossy();
    assert!(text.contains(" --> script.cmake:1:2 (mesage)\n"));
    assert!(text.contains("1 |     MeSage()\n  |     ^^^^^^\n"));
    assert!(text.contains(concat!(
        "  Call stack (most recent call first):\n",
        "    ... 2 frames in CMake modules\n",
        "      /bundle/Modules/A.cmake:2 (include)\n",
        "      /bundle/Modules/B.cmake:1 (include)\n",
        "    project with spaces/root.cmake:10 (include)\n",
    )));
    lookup.show_bundled = false;
    let collapsed = render_rich(&diag, None, &lookup, ascii());
    assert!(collapsed.to_str_lossy().contains(concat!(
        "  Call stack (most recent call first):\n",
        "    ... 2 frames in CMake modules\n",
        "    project with spaces/root.cmake:10 (include)\n",
    )));
    assert!(!collapsed.to_str_lossy().contains("/bundle/Modules/"));
    let result = render_rich(
        &diagnostic(Location::Backtrace(Backtrace::default().push(frame(
            b"script.cmake",
            1,
            b"mesage",
        )))),
        None,
        &SourceMap::default(),
        ascii(),
    );
    assert!(
        result
            .to_str_lossy()
            .contains(" --> script.cmake:1 (mesage)\n")
    );
    Ok(())
}

#[test]
fn locationless_body_keeps_preformatted_lines() {
    let mut diag = diagnostic(Location::None);
    diag.body = "First line.\nUse /a/two  spaces.cmake.\n\n\tkeep   spacing\n\n  link_libraries(previous_lib)\nMore detail.".into();
    let result = render_rich(&diag, None, &SourceMap::default(), ascii());
    assert_eq!(
        result.as_slice(),
        b"error: First line.\n  Use /a/two  spaces.cmake.\n\n  \tkeep   spacing\n\n    link_libraries(previous_lib)\n  More detail.\n",
    );
}

#[test]
fn unavailable_source_keeps_fallback_location_and_label_message() -> Result<(), &'static str> {
    let mut map = SourceMap::default();
    let source = add(&mut map, "unavailable", b"input")?;
    let annotations = Annotations::primary(span(source, 0, 1), "missing source");
    let diag = diagnostic(Location::FileLine {
        file: Arc::from(b"fallback".as_slice()),
        line: 23,
    });
    let result = render_rich(&diag, Some(&annotations), &SourceMap::default(), ascii());
    let text = result.to_str_lossy();
    assert!(text.contains(" --> fallback:23\n"));
    assert!(text.contains("  = note: missing source\n"));
    Ok(())
}

#[test]
fn attached_annotations_use_editor_columns_and_terminal_cells() -> Result<(), &'static str> {
    let mut map = SourceMap::default();
    let prefix = "\t界x\na\u{0301}👩🏽‍💻界\tx\n";
    let mut bytes = prefix.as_bytes().to_vec();
    bytes.extend_from_slice(b"\xffx");
    let source = add(&mut map, "input", &bytes)?;
    let second = u32::try_from(prefix.rfind('x').ok_or("Missing token")?).map_err(|_| "Offset")?;
    let third = u32::try_from(bytes.len()).map_err(|_| "Offset")?;
    let annotations = Annotations::primary(span(source, 4, 5), "wide")
        .with_secondary(span(source, second, second + 1), "combined")
        .with_secondary(span(source, third - 1, third), "malformed")
        .with_secondary(span(source, third, third), "end of file");
    let mut diag = diagnostic(Location::None);
    diag.annotations = Some(Box::new(annotations));
    let result = render_rich(&diag, None, &map, ascii());
    assert!(result.to_str_lossy().contains(concat!(
        " --> input:1:3\n",
        "  |\n",
        "1 |     界x\n",
        "  |       ^ wide\n",
        "2 | á👩🏽‍💻界   x\n",
        "  |         - combined\n",
        "3 | �x\n",
        "  |  - malformed\n",
        "  |   - end of file\n",
    )));
    Ok(())
}

#[test]
fn crop_keeps_the_primary_token_and_wraps_the_label_at_eighty_cells() -> Result<(), &'static str> {
    let mut map = SourceMap::default();
    let left = "界".repeat(160);
    let right = "界".repeat(40);
    let bytes = format!("{left}👩‍💻{right}");
    let source = add(&mut map, "input", bytes.as_bytes())?;
    let start = u32::try_from(left.len()).map_err(|_| "Offset")?;
    let annotations = Annotations::primary(span(source, start + 1, start + 2), "a".repeat(100));
    let result = render_rich(
        &diagnostic(Location::None),
        Some(&annotations),
        &map,
        RichOptions {
            terminal_width: 80,
            ..ascii()
        },
    );
    let text = result.to_str_lossy();
    let mut lines = text.lines();
    let excerpt = lines
        .find(|line| line.starts_with("1 | "))
        .ok_or("Missing excerpt")?;
    assert!(excerpt.starts_with("1 | ..."));
    assert!(excerpt.contains("👩‍💻"));
    assert!(excerpt.ends_with("..."));
    assert!(Cells::new(excerpt.as_bytes()).width() <= 80);
    let marker = lines.next().ok_or("Missing marker")?;
    assert!(marker.contains("^^"));
    let marker_col = marker.find('^').ok_or("Missing marker column")?;
    let token_byte = excerpt.find('👩').ok_or("Missing token column")?;
    assert_eq!(
        marker_col,
        Cells::new(excerpt.as_bytes()).column(token_byte, false)
    );
    let wrap = lines.next().ok_or("Missing wrapped label")?;
    assert!(wrap.starts_with("  | a"));
    assert!(wrap.len() <= 80);
    Ok(())
}

#[test]
fn policy_heading_and_suppression_remain_visible() {
    let mut diag = diagnostic(Location::None);
    diag.category = Some(DiagCategory::Policy);
    diag.policy = Some(PolicyId::new(224));
    diag.kind = MessageKind::Warning;
    let map = SourceMap::default();
    let warning = render_rich(&diag, None, &map, ascii());
    assert!(
        warning
            .to_str_lossy()
            .starts_with("warning[CMake.Policy CMP0224]:")
    );
    assert!(
        warning
            .to_str_lossy()
            .contains("-Wno-author or -Wno-policy")
    );
    diag.kind = MessageKind::AuthorError;
    let error = render_rich(&diag, None, &map, ascii());
    assert!(
        error
            .to_str_lossy()
            .starts_with("error[CMake.Policy CMP0224]:")
    );
    assert!(
        error
            .to_str_lossy()
            .contains("-Wno-error=author or -Wno-error=policy")
    );
}

#[test]
fn fix_edits_on_the_same_and_adjacent_lines_share_a_hunk() -> Result<(), &'static str> {
    let mut map = SourceMap::default();
    let source = add(&mut map, "input", b"set(a mesage)\nset(b old)\n")?;
    let edits = [(20, 23, "new"), (6, 12, "message"), (4, 5, "first")]
        .into_iter()
        .map(|(start, end, replacement)| Edit {
            span: span(source, start, end),
            replacement: replacement.into(),
        })
        .collect();
    let annotations = Annotations::default().with_fix(Fix {
        message: "Correct all names.".into(),
        edits,
        confidence: Confidence::MachineApplicable,
    });
    let result = render_rich(
        &diagnostic(Location::None),
        Some(&annotations),
        &map,
        ascii(),
    );
    let text = result.to_str_lossy();
    assert_eq!(text.matches("    --> input:").count(), 1);
    assert!(text.contains(concat!(
        "    - set(a mesage)\n",
        "    - set(b old)\n",
        "    + set(first message)\n",
        "    + set(b new)\n",
    )));
    Ok(())
}

#[test]
fn invalid_edits_keep_help_without_showing_a_partial_fix() -> Result<(), &'static str> {
    let mut map = SourceMap::default();
    let source = add(&mut map, "input", "aéz".as_bytes())?;
    let bad_source = add(&mut map, "bad", b"\xff")?;
    let mut unavailable = SourceMap::default();
    add(&mut unavailable, "unused 1", b"")?;
    add(&mut unavailable, "unused 2", b"")?;
    let absent = add(&mut unavailable, "absent", b"a")?;
    let invalid = [
        (source, 4, 5, b"b".as_slice()),
        (source, 3, 1, b"b".as_slice()),
        (source, 2, 3, b"b".as_slice()),
        (source, 1, 3, b"\xff".as_slice()),
        (source, 0, 3, b"b".as_slice()),
        (source, 0, 0, b"b".as_slice()),
        (bad_source, 0, 1, b"b".as_slice()),
        (absent, 0, 1, b"b".as_slice()),
    ];
    for (id, start, end, replacement) in invalid {
        let annotations = Annotations::default().with_fix(Fix {
            message: "Inspect the replacement.".into(),
            edits: vec![
                Edit {
                    span: span(source, 0, 1),
                    replacement: "x".into(),
                },
                Edit {
                    span: span(id, start, end),
                    replacement: replacement.into(),
                },
            ],
            confidence: Confidence::MaybeIncorrect,
        });
        let result = render_rich(
            &diagnostic(Location::None),
            Some(&annotations),
            &map,
            ascii(),
        );
        let text = result.to_str_lossy();
        assert!(text.contains("  = help (review required): Inspect the replacement.\n"));
        assert!(text.contains("  = note: Cannot show the edit preview.\n"));
        assert!(!text.contains("    -->"));
        assert!(!text.contains("    + "));
    }
    Ok(())
}

#[test]
fn fix_preview_crops_cells_and_marks_omitted_lines() -> Result<(), &'static str> {
    let mut map = SourceMap::default();
    let left = format!("{}\t", "界".repeat(101));
    let bytes = format!("{left}old end\n");
    let end = u32::try_from(left.len()).map_err(|_| "Offset")? + 3;
    let source = add(&mut map, "input", bytes.as_bytes())?;
    let annotations = Annotations::default().with_fix(Fix {
        message: "Replace the token.".into(),
        edits: vec![Edit {
            span: span(source, 0, end),
            replacement: format!("{left}👩‍💻new\ntwo\nthree\nfour\nfive").into(),
        }],
        confidence: Confidence::MachineApplicable,
    });
    let result = render_rich(
        &diagnostic(Location::None),
        Some(&annotations),
        &map,
        RichOptions {
            terminal_width: 80,
            ..ascii()
        },
    );
    let text = result.to_str_lossy();
    assert!(text.contains("  old end\n"));
    assert!(text.contains("  👩‍💻new\n"));
    assert!(text.contains("    + ...\n    + five end\n"));
    assert!(
        text.lines()
            .filter(|line| line.starts_with("    + ") || line.starts_with("    - "))
            .all(|line| Cells::new(line.as_bytes()).width() <= 80)
    );
    Ok(())
}
