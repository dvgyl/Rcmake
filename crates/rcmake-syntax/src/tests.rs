#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use rcmake_diag::SourceMap;

use crate::*;

fn parse(src: &[u8], kind: SourceKind) -> ListFile {
    parse_with_limits(src, kind, &Limits::default()).expect("valid listfile")
}

fn file(src: &[u8]) -> ListFile {
    parse(src, SourceKind::File)
}

fn parse_with_limits(
    src: &[u8],
    kind: SourceKind,
    limits: &Limits,
) -> Result<ListFile, ParseError> {
    let mut sources = SourceMap::default();
    let source = sources
        .add("test.cmake".into(), kind, Arc::from(src))
        .expect("available source ID");
    parse_list_file(src, source, kind, limits)
}

fn file_error(src: &[u8]) -> ParseError {
    parse_with_limits(src, SourceKind::File, &Limits::default()).expect_err("invalid listfile")
}

fn args(list: &ListFile) -> Vec<(ArgKind, &[u8])> {
    list.commands[0]
        .args
        .iter()
        .map(|argument| (argument.kind, argument.text.as_slice()))
        .collect()
}

#[test]
fn file_normalization_preserves_raw_spans() {
    let src = b"\xef\xbb\xbf\tMiX\r(\r\n \"a\rb\r\nc\" [=[\r\nd\re\r\n]=]\r\n)\r\nTwo()";
    let list = file(src);
    let command = &list.commands[0];
    assert_eq!(command.name.as_slice(), b"MiX");
    assert_eq!(command.lower.as_slice(), b"mix");
    assert_eq!((command.line, command.close_line), (1, 6));
    assert_eq!(
        &src[command.span.clone()],
        b"MiX\r(\r\n \"a\rb\r\nc\" [=[\r\nd\re\r\n]=]\r\n)"
    );
    assert_eq!(command.args[0].line, 2);
    assert_eq!(command.args[1].line, 3);
    assert_eq!(&src[command.args[0].span.clone()], b"\"a\rb\r\nc\"");
    assert_eq!(&src[command.args[1].span.clone()], b"[=[\r\nd\re\r\n]=]");
    assert_eq!(
        args(&list),
        [
            (ArgKind::Quoted, b"a\rb\nc".as_slice()),
            (ArgKind::Bracket, b"d\re\n".as_slice()),
        ]
    );
    assert_eq!(list.commands[1].line, 7);
    assert_eq!(&src[list.commands[1].span.clone()], b"Two()");
    assert_eq!(list.source.get(), 1);
}

#[test]
fn bom_rules_depend_on_source_kind() {
    assert!(file(b"\xef\xbb\xbf").commands.is_empty());
    for bom in [
        b"\xfe\xff".as_slice(),
        b"\xff\xfe",
        b"\x00\x00\xfe\xff",
        b"\xff\xfe\x00\x00",
    ] {
        assert_eq!(
            file_error(bom).message.as_slice(),
            b"File starts with a Byte-Order-Mark that is not UTF-8."
        );
    }
    let error = parse_with_limits(
        b"\xef\xbb\xbfx()",
        SourceKind::VirtualEval,
        &Limits::default(),
    )
    .expect_err("virtual BOM is part of the token");
    assert_eq!(
        error.message.as_slice(),
        b"Parse error.  Expected a command name, got unquoted argument with text \"\xef\xbb\xbfx\"."
    );
}

#[test]
fn virtual_eval_preserves_crlf_in_arguments() {
    let src = b"x(\"a\\\r\nb\" [=[\r\nb]=]\r c)";
    let list = parse(src, SourceKind::VirtualEval);
    assert_eq!(
        args(&list),
        [
            (ArgKind::Quoted, b"a\\\r\nb".as_slice()),
            (ArgKind::Bracket, b"\r\nb".as_slice()),
            (ArgKind::Unquoted, b"c".as_slice()),
        ]
    );
    assert_eq!(
        args(&file(src)),
        [
            (ArgKind::Quoted, b"ab".as_slice()),
            (ArgKind::Bracket, b"b".as_slice()),
            (ArgKind::Unquoted, b"c".as_slice()),
        ]
    );
}

#[test]
fn command_boundary_requires_newline_token() {
    let list = file(b" # comment\rtext\r\n_x2\t()#[[\n]] # ignored\nB()");
    assert_eq!(list.commands.len(), 2);
    assert_eq!(list.commands[0].line, 2);
    assert_eq!(list.commands[1].line, 4);
    for (src, line) in [
        (b"a()b()".as_slice(), 1),
        (b"a()\r b()", 1),
        (b"a() #[=[\n]=] b()", 2),
        (b"#[[\n]] b()", 2),
    ] {
        let error = file_error(src);
        assert_eq!(error.line, line);
        assert_eq!(
            error.message.as_slice(),
            b"Parse error.  Expected a newline, got identifier with text \"b\"."
        );
    }
}

#[test]
fn invalid_command_tokens_and_missing_delimiters() {
    for (src, line, body) in [
        (
            b"abc\n()".as_slice(),
            2,
            b"Parse error.  Expected \"(\", got newline with text \"\n\".".as_slice(),
        ),
        (
            b"abc #[==[comment]==]()",
            1,
            b"Parse error.  Expected \"(\", got bracket comment with text \"comment\".",
        ),
        (
            b"abc",
            1,
            b"Unexpected end of file.\n\nParse error.  Function missing opening \"(\".",
        ),
        (
            b"a(\n((x)\n",
            1,
            b"Parse error.  Function missing ending \")\".  End of file reached.",
        ),
        (
            b"123(x)",
            1,
            b"Parse error.  Expected a command name, got unquoted argument with text \"123\".",
        ),
        (
            b"a-b(x)",
            1,
            b"Parse error.  Expected a command name, got unquoted argument with text \"a-b\".",
        ),
        (
            b"x())",
            1,
            b"Parse error.  Expected a command name, got right paren with text \")\".",
        ),
        (
            b"x(a\\\nb)",
            1,
            b"Parse error.  Function missing ending \")\".  Instead found bad character with text \"\\\".",
        ),
    ] {
        let error = file_error(src);
        assert_eq!(error.line, line, "{src:?}");
        assert_eq!(error.message.as_slice(), body, "{src:?}");
        assert_eq!(error.command, None);
    }
}

#[test]
fn brackets_use_exact_delimiters_and_strip_one_newline() {
    let list = file(
        b"#[==[ignored #[[ ]] ]==]\nx([[]] [=[\n\n[[a]] ]==] \n]=] [==[a]=]==] a[[b]] #[[ignore]] z)",
    );
    assert_eq!(
        args(&list),
        [
            (ArgKind::Bracket, b"".as_slice()),
            (ArgKind::Bracket, b"\n[[a]] ]==] \n"),
            (ArgKind::Bracket, b"a]="),
            (ArgKind::Unquoted, b"a[[b]]"),
            (ArgKind::Unquoted, b"z"),
        ]
    );
    assert_eq!(list.commands[0].line, 2);
    assert_eq!(list.commands[0].close_line, 5);
    assert!(list.warnings.is_empty());
}

#[test]
fn unterminated_tokens_report_the_scanner_line_and_content() {
    for (src, line, body) in [
        (
            b"x(\n\"a\nb".as_slice(),
            3,
            b"Parse error.  Function missing ending \")\".  Instead found unterminated string with text \"a\nb\".".as_slice(),
        ),
        (
            b"x(\n[=[\na\nb",
            4,
            b"Parse error.  Function missing ending \")\".  Instead found unterminated bracket with text \"a\nb\".",
        ),
        (
            b"#[=[a",
            1,
            b"Parse error.  Expected a command name, got unterminated bracket with text \"a\".",
        ),
    ] {
        let error = file_error(src);
        assert_eq!(error.line, line);
        assert_eq!(error.message.as_slice(), body);
    }
}

#[test]
fn quoted_continuations_use_backslash_parity() {
    let mut src = b"x(".to_vec();
    for count in 0..=5 {
        src.extend_from_slice(b"\"a");
        src.extend(std::iter::repeat_n(b'\\', count));
        src.extend_from_slice(b"\nb\" ");
    }
    src.extend_from_slice(br#""\n\t\"\\\;()#""#);
    src.extend_from_slice(b")");
    let list = file(&src);
    for (count, argument) in list.commands[0].args.iter().take(6).enumerate() {
        let mut expected = vec![b'a'];
        expected.extend(std::iter::repeat_n(b'\\', count - count % 2));
        if count % 2 == 0 {
            expected.push(b'\n');
        }
        expected.push(b'b');
        assert_eq!(argument.kind, ArgKind::Quoted);
        assert_eq!(argument.text.as_slice(), expected);
        assert_eq!(
            argument.line,
            u32::try_from(count + 1).expect("small count")
        );
    }
    assert_eq!(
        list.commands[0].args[6].text.as_slice(),
        br#"\n\t\"\\\;()#"#
    );
    assert_eq!(list.commands[0].close_line, 7);
}

#[test]
fn unquoted_escapes_legacy_quotes_and_make_references() {
    let list = file(br#"x(-Da="b c" a"b"c"d"e a"$(CC) foo"b $(CC)$(9_)$() pre$(CC)suf $(a-b) \  \( \# \; \" \\)"#);
    let text: Vec<_> = list.commands[0]
        .args
        .iter()
        .map(|argument| {
            assert_eq!(argument.kind, ArgKind::Unquoted);
            argument.text.as_slice()
        })
        .collect();
    assert_eq!(
        text,
        [
            br#"-Da="b c""#.as_slice(),
            br#"a"b"c"d"e"#,
            br#"a"$(CC) foo"b"#,
            b"$(CC)$(9_)$()",
            b"pre$(CC)suf",
            b"$",
            b"(",
            b"a-b",
            b")",
            br"\ ",
            br"\(",
            br"\#",
            br"\;",
            br#"\""#,
            br"\\",
        ]
    );
    let list = file(b"x(a\"b#c\"d a\"b(c\"d a\"b\nc\"d a\"b\rc\"d)");
    assert_eq!(list.warnings.len(), 8);
    let tokens = args(&list);
    for (index, quoted) in [b"b#c".as_slice(), b"b(c", b"b\nc", b"b\rc"]
        .into_iter()
        .enumerate()
    {
        let base = 3 * index;
        assert_eq!(
            tokens[base..base + 3],
            [
                (ArgKind::Unquoted, b"a".as_slice()),
                (ArgKind::Quoted, quoted),
                (ArgKind::Unquoted, b"d"),
            ]
        );
    }
}

#[test]
fn nested_parentheses_control_argument_separation() {
    let list = file(br#"x((a (b))y "(" \( \) [[b]](c))"#);
    assert_eq!(
        args(&list),
        [
            (ArgKind::Unquoted, b"(".as_slice()),
            (ArgKind::Unquoted, b"a"),
            (ArgKind::Unquoted, b"("),
            (ArgKind::Unquoted, b"b"),
            (ArgKind::Unquoted, b")"),
            (ArgKind::Unquoted, b")"),
            (ArgKind::Unquoted, b"y"),
            (ArgKind::Quoted, b"("),
            (ArgKind::Unquoted, br"\("),
            (ArgKind::Unquoted, br"\)"),
            (ArgKind::Bracket, b"b"),
            (ArgKind::Unquoted, b"("),
            (ArgKind::Unquoted, b"c"),
            (ArgKind::Unquoted, b")"),
        ]
    );
    assert_eq!(list.warnings.len(), 1);
}

#[test]
fn adjacency_warning_uses_byte_columns_after_the_bom() {
    use rcmake_diag::{DiagCategory, Effect, Location, MessageKind};

    let list = file(b"\xef\xbb\xbfx(\t\"\xc3\xa9\"y)");
    assert_eq!(
        args(&list),
        [
            (ArgKind::Quoted, b"\xc3\xa9".as_slice()),
            (ArgKind::Unquoted, b"y"),
        ]
    );
    let [warning] = list.warnings.as_slice() else {
        panic!("expected exactly one warning");
    };
    assert_eq!(warning.kind, MessageKind::AuthorWarning);
    assert_eq!(warning.category, Some(DiagCategory::Author));
    assert_eq!(warning.effect, Effect::None);
    assert!(matches!(
        warning.location,
        Location::FileLine { line: 1, .. }
    ));
    assert_eq!(
        warning.body.as_slice(),
        b"Syntax Warning in cmake code at column 8\n\nArgument not separated from preceding token by whitespace."
    );
}

#[test]
fn bracket_adjacency_is_fatal_except_at_parentheses() {
    for (src, column) in [
        (b"x([[a]]b)".as_slice(), 8),
        (b"x(\"a\"[=[b]=])", 6),
        (b"x((a)[[b]])", 6),
        (b"x(#[[b]]c)", 9),
        (b"x(a#[[b]]c)", 10),
    ] {
        let error = file_error(src);
        assert_eq!((error.line, error.column), (1, column), "{src:?}");
        assert_eq!(
            error.message.as_slice(),
            format!("Syntax Error in cmake code at column {column}\n\nArgument not separated from preceding token by whitespace.").as_bytes()
        );
    }
    for src in [
        b"x(([[b]]))".as_slice(),
        b"x([[b]](a))",
        b"x(#[[b]](a))",
        b"x(\"a\"#[[b]])",
        b"x(#[[b]] c)",
        b"x(#[[b]]\nc)",
    ] {
        assert!(file(src).warnings.is_empty(), "{src:?}");
    }
}

#[test]
fn flow_control_accepts_case_insensitive_nested_blocks() {
    let list = file(b"IF(1)\nFOREACH(x)\nwhile(1)\nfunction(f)\nmacro(m)\nblock()\nEnDbLoCk()\nendmacro()\nendfunction()\nendwhile()\nendforeach()\nELSEIF(0)\nelseif(1)\nElSe()\nendif()");
    assert_eq!(list.commands.len(), 15);
    assert!(list.warnings.is_empty());
}

#[test]
fn flow_control_reports_the_offending_command() {
    for (src, line, command) in [
        (b"if(1)\n".as_slice(), 1, b"if".as_slice()),
        (b"foreach(x)", 1, b"foreach"),
        (b"while(1)", 1, b"while"),
        (b"function(f)", 1, b"function"),
        (b"macro(m)", 1, b"macro"),
        (b"block()", 1, b"block"),
        (b"endif()", 1, b"endif"),
        (b"endforeach()", 1, b"endforeach"),
        (b"endwhile()", 1, b"endwhile"),
        (b"endfunction()", 1, b"endfunction"),
        (b"endmacro()", 1, b"endmacro"),
        (b"endblock()", 1, b"endblock"),
        (b"else()", 1, b"else"),
        (b"elseif()", 1, b"elseif"),
        (b"x()\nElSe()", 2, b"ElSe"),
        (b"if(1)\nelse()\nELSE()\nendif()", 3, b"ELSE"),
        (b"if(1)\nelse()\nelseif(1)\nendif()", 3, b"elseif"),
        (b"if(1)\nwhile(1)\nendif()", 3, b"endif"),
        (b"if(1)\nwhile(1)\nelse()\nendwhile()\nendif()", 3, b"else"),
        (b"foreach(x)\nFuNcTiOn(f)", 2, b"FuNcTiOn"),
    ] {
        let error = file_error(src);
        assert_eq!((error.line, error.column), (line, 0), "{src:?}");
        assert_eq!(
            error.command.as_ref().map(|name| name.as_slice()),
            Some(command)
        );
        assert_eq!(
            error.message.as_slice(),
            b"Flow control statements are not properly nested."
        );
    }
    assert_eq!(file_error(b"endif()\n\"").command, None);
}

#[test]
fn argument_bytes_do_not_require_utf8() {
    let list = file(b"x(\xfe \"\xff\" [[\x80]])");
    assert_eq!(
        args(&list),
        [
            (ArgKind::Unquoted, b"\xfe".as_slice()),
            (ArgKind::Quoted, b"\xff".as_slice()),
            (ArgKind::Bracket, b"\x80".as_slice()),
        ]
    );
    assert_eq!(
        file_error(b"\xff()").message.as_slice(),
        b"Parse error.  Expected a command name, got unquoted argument with text \"\xff\"."
    );
    for src in [b"x(a\0b)".as_slice(), b"x(\"a\0b\")", b"x([[a\0b]])"] {
        assert_eq!(
            file_error(src).message.as_slice(),
            b"Parse error.  Function missing ending \")\".  Instead found bad character with text \"a\0b\"."
        );
    }
}

#[test]
fn input_limits_depend_on_source_kind() {
    let limits = Limits {
        max_file_read: 3,
        max_value_bytes: 4,
        ..Limits::default()
    };
    assert!(parse_with_limits(b"x()", SourceKind::File, &limits).is_ok());
    assert!(parse_with_limits(b"x() ", SourceKind::File, &limits).is_err());
    assert!(parse_with_limits(b"x() ", SourceKind::VirtualEval, &limits).is_ok());
    assert!(parse_with_limits(b"x()  ", SourceKind::VirtualEval, &limits).is_err());
}

#[test]
fn malformed_byte_sequences_and_deep_nesting_do_not_panic() {
    let punctuation = b"[]()=\\\"$#\r\n\t \0 a\xff";
    let mut state = 0x5f38_da19_u32;
    for length in 0..256 {
        let mut src = b"x(".to_vec();
        for _ in 0..length {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            src.push(if state & 1 == 0 {
                punctuation[(state >> 8) as usize % punctuation.len()]
            } else {
                (state >> 24) as u8
            });
        }
        for kind in [SourceKind::File, SourceKind::VirtualEval] {
            let _ = parse_with_limits(&src, kind, &Limits::default());
        }
    }
    let mut src = b"x(".to_vec();
    src.extend(std::iter::repeat_n(b'(', 12_000));
    src.extend(std::iter::repeat_n(b')', 12_001));
    assert_eq!(file(&src).commands[0].args.len(), 24_000);

    let mut src = b"block()\n".repeat(12_000);
    src.extend_from_slice(&b"endblock()\n".repeat(12_000));
    assert_eq!(file(&src).commands.len(), 24_000);
}
