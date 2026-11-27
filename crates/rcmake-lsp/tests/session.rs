//! Language server sessions with an in-memory host.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::{path::Path, time::Duration};

use rcmake_host::{Host, NativeHost, PureHost};
use rcmake_json::Value;
use rcmake_lsp::{
    RequestId, RpcMessage, SemanticContext, SemanticLocation, SemanticProvider, Server,
};

#[cfg(windows)]
const WORK_DIR: &str = "C:/work";
#[cfg(not(windows))]
const WORK_DIR: &str = "/work";
#[cfg(windows)]
const WORK_URI: &str = "file:///C:/work";
#[cfg(not(windows))]
const WORK_URI: &str = "file:///work";
#[cfg(windows)]
const URI: &str = "file:///C:/work/CMakeLists.txt";
#[cfg(not(windows))]
const URI: &str = "file:///work/CMakeLists.txt";

fn host() -> PureHost {
    let work = Path::new(WORK_DIR);
    PureHost::new(work.into(), work.join("bin/rcmake-lsp"))
}

fn decode(json: &str) -> RpcMessage {
    RpcMessage::decode(json.as_bytes()).unwrap()
}

fn get<'a>(value: &'a Value, key: &str) -> &'a Value {
    let Value::Object(object) = value else {
        panic!("Expected an object. Got {value:?}.")
    };
    object.get(key.as_bytes()).unwrap()
}

fn array(value: &Value) -> &[Value] {
    let Value::Array(array) = value else {
        panic!("Expected an array. Got {value:?}.")
    };
    array
}

fn object(fields: &[(&str, Value)]) -> Value {
    Value::Object(
        fields
            .iter()
            .map(|(key, value)| ((*key).into(), value.clone()))
            .collect(),
    )
}

fn result(messages: Vec<RpcMessage>) -> Value {
    let [RpcMessage::Result { result, .. }] = messages.as_slice() else {
        panic!("Expected a result. Got {messages:?}.")
    };
    result.clone()
}

fn published(messages: Vec<RpcMessage>) -> Value {
    let [
        RpcMessage::Call {
            id: None,
            method,
            params,
        },
    ] = messages.as_slice()
    else {
        panic!("Expected a notification. Got {messages:?}.")
    };
    assert_eq!(method, "textDocument/publishDiagnostics");
    params.clone()
}

fn initialized(host: &dyn Host, utf8: bool) -> Server<'_> {
    let mut server = Server::new(host);
    let capabilities = if utf8 {
        r#""capabilities":{"general":{"positionEncodings":["utf-8","utf-16"]}}"#
    } else {
        r#""capabilities":{}"#
    };
    let response = result(server.handle(decode(&format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{{capabilities}}}}}"#
    ))));
    assert_eq!(
        get(get(&response, "capabilities"), "positionEncoding"),
        &Value::string(if utf8 { "utf-8" } else { "utf-16" })
    );
    assert!(
        server
            .handle(RpcMessage::notification("initialized", Value::object()))
            .is_empty()
    );
    server
}

fn open(server: &mut Server<'_>, text: &str) -> Value {
    published(server.handle(RpcMessage::notification(
        "textDocument/didOpen",
        object(&[(
            "textDocument",
            object(&[
                ("uri", Value::string(URI)),
                ("version", Value::Int(1)),
                ("text", Value::string(text)),
            ]),
        )]),
    )))
}

fn request(server: &mut Server<'_>, method: &str, params: &[(&str, Value)]) -> Value {
    let mut params = params.to_vec();
    params.push(("textDocument", object(&[("uri", Value::string(URI))])));
    result(server.handle(RpcMessage::request(
        RequestId::Int(2),
        method,
        object(&params),
    )))
}

fn at(server: &mut Server<'_>, method: &str, line: i64, character: i64) -> Value {
    request(
        server,
        method,
        &[(
            "position",
            object(&[
                ("line", Value::Int(line)),
                ("character", Value::Int(character)),
            ]),
        )],
    )
}

#[test]
fn scripted_editing_session() -> Result<(), Box<dyn std::error::Error>> {
    let host = host();
    let work = Path::new(WORK_DIR);
    host.seed_file(&work.join("helpers space.cmake"), b"set(HELPER ON)\n")?;
    host.seed_file(&work.join("sub/CMakeLists.txt"), b"project(Sub)\n")?;
    let mut server = initialized(&host, false);
    let text = concat!(
        "project(Example)\n",
        "set(VALUE ON)\n",
        "function(demo)\n",
        "# A comment\n",
        "message(\"😃 ${VALUE}\")\n",
        "endfunction()\n",
        "add_library(lib lib.c)\n",
        "target_link_libraries(lib PUBLIC dep)\n",
        "include(\"helpers space.cmake\")\n",
        "add_subdirectory(sub)\n",
        "demo()\n",
        "cmake_policy(SET CMP0000 NEW)\n",
    );
    assert!(array(get(&open(&mut server, text), "diagnostics")).is_empty());

    let symbols = request(&mut server, "textDocument/documentSymbol", &[]);
    let names: Vec<_> = array(&symbols)
        .iter()
        .map(|value| get(value, "name"))
        .collect();
    assert_eq!(
        names,
        ["Example", "VALUE", "demo", "lib", "sub"]
            .map(Value::string)
            .iter()
            .collect::<Vec<_>>()
    );
    let hover = at(&mut server, "textDocument/hover", 0, 2);
    assert_eq!(
        get(get(&hover, "contents"), "value"),
        &Value::string(concat!(
            "project\n\nSet the name of the project.\n\n",
            "project(<PROJECT-NAME> [<language-name>...])"
        ))
    );
    let hover = at(&mut server, "textDocument/hover", 11, 18);
    assert_ne!(hover, Value::Null);
    let hover = at(&mut server, "textDocument/hover", 4, 14);
    assert_eq!(
        get(get(&hover, "contents"), "value"),
        &Value::string("Defined on line 2.\n\nset(VALUE ON)")
    );
    for (line, column, declaration) in [(4, 14, 1), (7, 24, 6), (10, 2, 2)] {
        let definition = at(&mut server, "textDocument/definition", line, column);
        assert_eq!(get(&definition, "uri"), &Value::string(URI));
        assert_eq!(
            get(get(get(&definition, "range"), "start"), "line"),
            &Value::UInt(declaration)
        );
    }
    let include = at(&mut server, "textDocument/definition", 8, 15);
    assert_eq!(
        get(&include, "uri"),
        &Value::string(format!("{WORK_URI}/helpers%20space.cmake"))
    );
    let subdir = at(&mut server, "textDocument/definition", 9, 18);
    assert_eq!(
        get(&subdir, "uri"),
        &Value::string(format!("{WORK_URI}/sub/CMakeLists.txt"))
    );
    let items = at(&mut server, "textDocument/completion", 10, 2);
    assert!(
        array(&items)
            .iter()
            .any(|item| get(item, "label") == &Value::string("demo"))
    );

    let tokens = request(&mut server, "textDocument/semanticTokens/full", &[]);
    let data = array(get(&tokens, "data"));
    assert_eq!(data.len() % 5, 0);
    for kind in 0..5 {
        assert!(
            data.as_chunks::<5>()
                .0
                .iter()
                .any(|chunk| chunk.get(3) == Some(&Value::UInt(kind)))
        );
    }
    let options = object(&[
        ("tabSize", Value::Int(2)),
        ("insertSpaces", Value::Bool(true)),
    ]);
    let formatted = request(
        &mut server,
        "textDocument/formatting",
        &[("options", options)],
    );
    assert_indent_edits(&formatted, &[3, 4], "  ");

    let change = change(2, r#"[{"text":"project(Example)\nmessage(\"unfinished"}]"#);
    let diagnostics = published(server.handle(change));
    let diagnostic = array(get(&diagnostics, "diagnostics"))
        .first()
        .ok_or("Expected a diagnostic for the unfinished call.")?;
    assert_eq!(get(diagnostic, "severity"), &Value::UInt(1));
    assert_eq!(
        get(get(get(diagnostic, "range"), "start"), "line"),
        &Value::UInt(1)
    );
    let save = decode(&format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/didSave","params":{{"textDocument":{{"uri":"{URI}"}}}}}}"#,
    ));
    assert_eq!(published(server.handle(save)), diagnostics);
    let close = decode(&format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/didClose","params":{{"textDocument":{{"uri":"{URI}"}}}}}}"#,
    ));
    assert!(array(get(&published(server.handle(close)), "diagnostics")).is_empty());
    assert!(
        server
            .handle(RpcMessage::notification("$/cancelRequest", Value::Null))
            .is_empty()
    );
    assert_eq!(
        result(server.handle(RpcMessage::request(
            RequestId::Int(3),
            "shutdown",
            Value::Null
        ))),
        Value::Null
    );
    assert!(
        server
            .handle(RpcMessage::notification("exit", Value::Null))
            .is_empty()
    );
    assert_eq!(server.exit_code(), Some(0));
    Ok(())
}

fn change(version: i64, changes: &str) -> RpcMessage {
    decode(&format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/didChange","params":{{"textDocument":{{"uri":"{URI}","version":{version}}},"contentChanges":{changes}}}}}"#
    ))
}

fn rejected(mut messages: Vec<RpcMessage>, recovery: &str) -> Value {
    let Some(RpcMessage::Call {
        id: None,
        method,
        params,
    }) = messages.pop()
    else {
        panic!("Expected a warning.")
    };
    assert_eq!(method, "window/showMessage");
    assert_eq!(get(&params, "type"), &Value::Int(2));
    assert_eq!(
        get(&params, "message"),
        &Value::string(format!(
            "rcmake lost synchronization with {URI}. {recovery}"
        )),
    );
    published(messages)
}

struct EchoText;

impl SemanticProvider for EchoText {
    fn hover(&self, context: &SemanticContext<'_>) -> Option<String> {
        Some(context.text.into())
    }
}

fn assert_text(server: &mut Server<'_>, expected: &str) {
    assert_eq!(
        get(&at(server, "textDocument/hover", 0, 999), "contents"),
        &object(&[
            ("kind", Value::string("plaintext")),
            ("value", Value::string(expected)),
        ]),
    );
}

#[test]
fn incremental_edits_use_exact_unicode_boundaries_and_clamp_at_line_end() {
    for (utf8, end) in [(false, 11), (true, 13)] {
        let host = host();
        let mut server = initialized(&host, utf8).with_semantic_provider(&EchoText);
        open(&mut server, "message(\"😃X\")\r\n");
        let changes = format!(
            r#"[{{"range":{{"start":{{"line":0,"character":9}},"end":{{"line":0,"character":{end}}}}},"rangeLength":123,"text":"Y"}},
                {{"range":{{"start":{{"line":0,"character":999}},"end":{{"line":0,"character":1000}}}},"text":" "}},
                {{"range":{{"start":{{"line":1,"character":0}},"end":{{"line":1,"character":0}}}},"text":"message(EOF)"}}]"#
        );
        let diagnostics = published(server.handle(change(2, &changes)));
        assert_eq!(get(&diagnostics, "version"), &Value::Int(2));
        assert!(array(get(&diagnostics, "diagnostics")).is_empty());
        assert_text(&mut server, "message(\"YX\") \r\nmessage(EOF)");
        assert!(server.handle(change(2, &changes)).is_empty());
    }
}

#[test]
fn incremental_edits_reject_split_unicode_scalars() {
    for (utf8, end) in [(false, 11), (true, 13)] {
        let host = host();
        let mut server = initialized(&host, utf8).with_semantic_provider(&EchoText);
        open(&mut server, "message(\"😀\")\n");
        let changes = format!(
            r#"[{{"range":{{"start":{{"line":0,"character":10}},"end":{{"line":0,"character":{end}}}}},"text":"Y"}}]"#
        );
        let diagnostics = rejected(
            server.handle(change(2, &changes)),
            "Save the file with full text or reopen it to restore language features.",
        );
        assert!(array(get(&diagnostics, "diagnostics")).is_empty());
        assert_eq!(at(&mut server, "textDocument/hover", 0, 0), Value::Null);
    }
}

#[test]
fn incremental_edits_track_changes_to_carriage_return_lines() {
    let host = host();
    let mut server = initialized(&host, false).with_semantic_provider(&EchoText);
    let text = "# lead\rmessage(OLD)\rset(A 1)\r";
    assert!(array(get(&open(&mut server, text), "diagnostics")).is_empty());
    let changes = r#"[
        {"range":{"start":{"line":1,"character":0},"end":{"line":1,"character":0}},"text":"\n"},
        {"range":{"start":{"line":1,"character":8},"end":{"line":1,"character":11}},"text":"NEXT"},
        {"range":{"start":{"line":2,"character":0},"end":{"line":3,"character":99}},"text":"set(A 2)\r\nset(B 3)\r"},
        {"range":{"start":{"line":3,"character":6},"end":{"line":3,"character":7}},"text":"4"}]"#;
    let diagnostics = published(server.handle(change(2, changes)));
    assert!(array(get(&diagnostics, "diagnostics")).is_empty());
    assert_text(
        &mut server,
        "# lead\r\nmessage(NEXT)\rset(A 2)\r\nset(B 4)\r",
    );
}

#[test]
fn many_cursor_changes_keep_a_large_buffer_synchronized() {
    let host = host();
    let mut server = initialized(&host, false).with_semantic_provider(&EchoText);
    let line = format!("set(VALUE old) # {}\n", "a".repeat(1007));
    let text = line.repeat(1024);
    assert!(text.len() >= 1024 * 1024);
    open(&mut server, &text);
    let mut edits = Vec::new();
    let mut expected = text;
    for row in (0..1024).step_by(8) {
        edits.push(format!(
            r#"{{"range":{{"start":{{"line":{row},"character":10}},"end":{{"line":{row},"character":13}}}},"text":"next"}}"#
        ));
    }
    for row in (0..1024).step_by(8).rev() {
        let start = row * line.len() + 10;
        expected.replace_range(start..start + 3, "next");
    }
    let diagnostics = published(server.handle(change(2, &format!("[{}]", edits.join(",")))));
    assert!(array(get(&diagnostics, "diagnostics")).is_empty());
    assert_text(&mut server, &expected);
}

#[test]
fn rejected_incremental_edits_disable_features_until_full_text_arrives() {
    let host = host();
    let mut server = initialized(&host, false);
    let text = "if(ON)\nmessage(STATUS hello)\nendif()\n";
    let format = |server: &mut Server<'_>| {
        request(
            server,
            "textDocument/formatting",
            &[(
                "options",
                object(&[
                    ("tabSize", Value::Int(2)),
                    ("insertSpaces", Value::Bool(true)),
                ]),
            )],
        )
    };
    for (offset, recovery) in [(2, "didChange"), (5, "didSave"), (7, "didOpen")] {
        if offset == 2 {
            open(&mut server, text);
        }
        assert_indent_edits(&format(&mut server), &[1], "  ");
        let invalid = r#"[{"range":{"start":{"line":9,"character":0},"end":{"line":9,"character":0}},"text":"x"}]"#;
        let diagnostics = rejected(
            server.handle(change(offset, invalid)),
            "Save the file with full text or reopen it to restore language features.",
        );
        assert_eq!(get(&diagnostics, "version"), &Value::Int(offset));
        assert!(array(get(&diagnostics, "diagnostics")).is_empty());
        assert_eq!(format(&mut server), Value::Null);
        assert_eq!(at(&mut server, "textDocument/hover", 0, 0), Value::Null);
        let valid = r#"[{"range":{"start":{"line":1,"character":0},"end":{"line":1,"character":0}},"text":"x"}]"#;
        published(server.handle(change(offset + 1, valid)));
        assert_eq!(format(&mut server), Value::Null);
        published(server.handle(RpcMessage::notification(
            "textDocument/didSave",
            object(&[("textDocument", object(&[("uri", Value::string(URI))]))]),
        )));
        assert_eq!(format(&mut server), Value::Null);
        let doc = object(&[
            ("uri", Value::string(URI)),
            ("version", Value::Int(offset + 1)),
            ("text", Value::string(text)),
        ]);
        let update = match recovery {
            "didOpen" => {
                RpcMessage::notification("textDocument/didOpen", object(&[("textDocument", doc)]))
            }
            "didSave" => RpcMessage::notification(
                "textDocument/didSave",
                object(&[("textDocument", doc), ("text", Value::string(text))]),
            ),
            _ => change(
                offset + 2,
                r#"[{"text":"if(ON)\nmessage(STATUS hello)\nendif()\n"}]"#,
            ),
        };
        published(server.handle(update));
        assert_indent_edits(&format(&mut server), &[1], "  ");
    }
}

fn assert_indent_edits(result: &Value, lines: &[u64], indent: &str) {
    let edits = array(result);
    assert_eq!(edits.len(), lines.len());
    for (edit, line) in edits.iter().zip(lines) {
        assert_indent_edit(edit, *line, indent);
    }
}

fn assert_indent_edit(edit: &Value, line: u64, indent: &str) {
    assert_eq!(get(edit, "newText"), &Value::string(indent));
    for endpoint in ["start", "end"] {
        let point = get(get(edit, "range"), endpoint);
        assert_eq!(get(point, "line"), &Value::UInt(line));
        assert_eq!(get(point, "character"), &Value::UInt(0));
    }
}

#[test]
fn formatter_preserves_multiline_literals_comments_and_line_endings() {
    let host = host();
    let mut server = initialized(&host, true);
    let text = "if(ON)\r\nset(V [==[\r\nkeep  this\r\n]==])\r\n#[=[\r\n# keep this too\r\n]=]\r\nset(V \"keep\r\n  this too\")\r\n  \r\nendif()\r\n";
    open(&mut server, text);
    let options = object(&[
        ("tabSize", Value::Int(2)),
        ("insertSpaces", Value::Bool(false)),
    ]);
    let response = request(
        &mut server,
        "textDocument/formatting",
        &[("options", options)],
    );
    let edits = array(&response);
    assert_eq!(edits.len(), 4);
    for (edit, line) in edits.iter().take(3).zip([1u64, 4, 7]) {
        assert_indent_edit(edit, line, "\t");
    }
    // Line 9 trailing horizontal whitespace on the blank line is stripped while CRLF is kept.
    assert_eq!(get(&edits[3], "newText"), &Value::string(""));
    assert_eq!(
        get(get(get(&edits[3], "range"), "start"), "line"),
        &Value::UInt(9)
    );
    assert_eq!(
        get(get(get(&edits[3], "range"), "start"), "character"),
        &Value::UInt(0)
    );
    assert_eq!(
        get(get(get(&edits[3], "range"), "end"), "line"),
        &Value::UInt(9)
    );
    assert_eq!(
        get(get(get(&edits[3], "range"), "end"), "character"),
        &Value::UInt(2)
    );
}

#[test]
fn formatter_aligns_a_close_with_the_opener_when_no_argument_precedes_it() {
    let host = host();
    let mut server = initialized(&host, false);
    let text = concat!(
        "if(ON)\nmessage(\none\ntwo)\n",
        "message(\none\n) # close\n",
        "message(\n(\none\n) two)\nendif()\n",
    );
    assert!(array(get(&open(&mut server, text), "diagnostics")).is_empty());
    let response = request(
        &mut server,
        "textDocument/formatting",
        &[(
            "options",
            object(&[
                ("tabSize", Value::Int(2)),
                ("insertSpaces", Value::Bool(true)),
            ]),
        )],
    );
    let edits = array(&response);
    let expected = [
        "  ", "    ", "    ", "  ", "    ", "  ", "  ", "    ", "    ", "    ",
    ];
    assert_eq!(edits.len(), expected.len());
    for (line, (edit, indent)) in edits.iter().zip(expected).enumerate() {
        assert_indent_edit(edit, line as u64 + 1, indent);
    }
}

fn apply_lsp_edits(text: &str, edits: &[Value], encoding: &str) -> String {
    let mut line_starts = vec![0usize];
    for (idx, b) in text.bytes().enumerate() {
        if b == b'\n' || (b == b'\r' && text.as_bytes().get(idx + 1) != Some(&b'\n')) {
            line_starts.push(idx + 1);
        }
    }
    let to_offset = |point: &Value| -> usize {
        let Value::UInt(line) = get(point, "line") else {
            panic!("expected uint line");
        };
        let Value::UInt(ch) = get(point, "character") else {
            panic!("expected uint character");
        };
        let start = *line_starts.get(*line as usize).unwrap();
        let end = line_starts
            .get(*line as usize + 1)
            .copied()
            .unwrap_or(text.len());
        let raw_line = text.get(start..end).unwrap();
        let line_str = raw_line
            .strip_suffix("\r\n")
            .or_else(|| raw_line.strip_suffix(['\r', '\n']))
            .unwrap_or(raw_line);
        let ch = *ch as usize;
        match encoding {
            "utf-8" => start + ch,
            "utf-16" => {
                let mut units = 0usize;
                for (off, c) in line_str.char_indices() {
                    if units == ch {
                        return start + off;
                    }
                    units += c.len_utf16();
                }
                start + line_str.len()
            }
            "utf-32" => {
                for (units, (off, _)) in line_str.char_indices().enumerate() {
                    if units == ch {
                        return start + off;
                    }
                }
                start + line_str.len()
            }
            other => panic!("unexpected encoding {other}"),
        }
    };
    let mut byte_edits: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|edit| {
            let range = get(edit, "range");
            let start = to_offset(get(range, "start"));
            let end = to_offset(get(range, "end"));
            let Value::String(rep) = get(edit, "newText") else {
                panic!("expected string newText");
            };
            (start, end, std::str::from_utf8(rep).unwrap())
        })
        .collect();
    byte_edits.sort_by_key(|(s, e, _)| (*s, *e));
    let mut out = String::new();
    let mut cursor = 0usize;
    for (s, e, rep) in byte_edits {
        assert!(s >= cursor && s <= e);
        out.push_str(text.get(cursor..s).unwrap());
        out.push_str(rep);
        cursor = e;
    }
    out.push_str(text.get(cursor..).unwrap());
    out
}

#[test]
fn p1_11_bare_cr_non_bmp_crlf_and_cli_lsp_formatting_equivalence() {
    // 1. Bare CR between commands is a disk syntax error: LSP must refuse to format (empty edits).
    let bare_host = host();
    let mut server = initialized(&bare_host, false);
    open(&mut server, "if(ON)\rmessage(hi)\rendif()\r");
    let response = request(
        &mut server,
        "textDocument/formatting",
        &[(
            "options",
            object(&[
                ("tabSize", Value::Int(2)),
                ("insertSpaces", Value::Bool(true)),
            ]),
        )],
    );
    assert!(array(&response).is_empty());

    // 2. Non-BMP text + CRLF + protected bracket CR + `.rcmake.toml` overrides client indent
    //    produce byte-for-byte identical results across UTF-8, UTF-16, UTF-32, and CLI!
    let sample = concat!(
        "IF(ON)\r\n",
        "MESSAGE(STATUS   \"😀\"   [=[\r\nkeep\rraw\r\n]=])\r\n",
        "ENDIF()\r\n",
    );
    let config_toml = "[fmt]\nindent_width = 4\ncommand_case = \"lower\"\nend_of_line = \"lf\"\n";
    let expected_cli = rcmake_language_tools::format_bytes(
        sample.as_bytes(),
        &rcmake_language_tools::FormatStyle::from_config(
            &rcmake_language_tools::parse_config(config_toml.as_bytes(), None)
                .unwrap()
                .fmt,
        ),
        &rcmake_language_tools::FormatLimits::default(),
    )
    .unwrap();
    let expected_str = String::from_utf8(expected_cli.output).unwrap();
    assert_eq!(
        expected_str,
        "if(ON)\n    message(STATUS \"😀\" [=[\r\nkeep\rraw\r\n]=])\nendif()\n"
    );

    for encoding in ["utf-8", "utf-16", "utf-32"] {
        let host = host();
        host.seed_file(
            &Path::new(WORK_DIR).join(".rcmake.toml"),
            config_toml.as_bytes(),
        )
        .unwrap();
        let mut server = Server::new(&host);
        let init = result(server.handle(decode(&format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"rootUri":"{WORK_URI}","capabilities":{{"general":{{"positionEncodings":["{encoding}"]}}}}}}}}"#
        ))));
        assert_eq!(
            get(get(&init, "capabilities"), "positionEncoding"),
            &Value::string(encoding)
        );
        server.handle(RpcMessage::notification("initialized", Value::object()));
        open(&mut server, sample);
        // Client passes tabSize=2, insertSpaces=false, but .rcmake.toml indent_width=4 wins.
        let edits = request(
            &mut server,
            "textDocument/formatting",
            &[(
                "options",
                object(&[
                    ("tabSize", Value::Int(2)),
                    ("insertSpaces", Value::Bool(false)),
                ]),
            )],
        );
        let applied = apply_lsp_edits(sample, array(&edits), encoding);
        assert_eq!(applied, expected_str, "encoding={encoding}");
    }
}

#[test]
fn p1_12_live_workspace_lint_tracks_open_includes_globs_and_rcmake_toml_watches() {
    let host = host();
    let work = Path::new(WORK_DIR);
    host.seed_file(&work.join("helpers.cmake"), b"set(UNRELATED 1)\n")
        .unwrap();
    let mut server = Server::new(&host);
    result(server.handle(decode(&format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"rootUri":"{WORK_URI}","capabilities":{{"workspace":{{"didChangeWatchedFiles":{{"dynamicRegistration":true}}}}}}}}}}"#
    ))));
    let reg_msgs = server.handle(RpcMessage::notification("initialized", Value::object()));
    // Verify watcher registration covers **/.rcmake.toml.
    let reg_json = format!("{reg_msgs:?}");
    assert!(
        reg_json.contains(".rcmake.toml"),
        "expected .rcmake.toml watcher in {reg_json}"
    );

    // 1. Open caller referencing MY_HELPER_VAR (typo of MY_HELPER_VA unless helpers.cmake defines it)
    //    plus an overwritten glob (must NOT warn) and directory-options + unguarded-build-type.
    let caller_uri = format!("{WORK_URI}/sub/CMakeLists.txt");
    let caller_text = concat!(
        "project(Sub)\n",
        "include(\"${CMAKE_SOURCE_DIR}/helpers.cmake\")\n",
        "set(MY_HELPER_VA 1)\n",
        "message(STATUS \"${MY_HELPER_VAR}\")\n",
        "file(GLOB OVERWRITTEN *.cpp)\n",
        "set(OVERWRITTEN main.cpp)\n",
        "add_executable(app ${OVERWRITTEN})\n",
        "include_directories(inc)\n",
        "set(CMAKE_BUILD_TYPE Release)\n",
    );
    let open_caller = published(server.handle(RpcMessage::notification(
        "textDocument/didOpen",
        object(&[(
            "textDocument",
            object(&[
                ("uri", Value::string(caller_uri.as_str())),
                ("version", Value::Int(1)),
                ("text", Value::string(caller_text)),
            ]),
        )]),
    )));
    let codes = |params: &Value| -> Vec<String> {
        array(get(params, "diagnostics"))
            .iter()
            .filter_map(|d| match d {
                Value::Object(map) => map.get(b"code".as_slice()).and_then(|v| match v {
                    Value::String(s) => Some(String::from_utf8_lossy(s).into_owned()),
                    _ => None,
                }),
                _ => None,
            })
            .collect()
    };
    let initial_codes = codes(&open_caller);
    assert!(initial_codes.contains(&"undefined-variable".to_owned()));
    assert!(initial_codes.contains(&"directory-options".to_owned()));
    assert!(initial_codes.contains(&"unguarded-build-type".to_owned()));
    assert!(
        !initial_codes.contains(&"glob-without-configure-depends".to_owned()),
        "overwritten glob must not warn: {initial_codes:?}"
    );

    // 2. Open helpers.cmake in memory (unsaved!) defining MY_HELPER_VAR -> caller republishes!
    let helpers_uri = format!("{WORK_URI}/helpers.cmake");
    let msgs = server.handle(RpcMessage::notification(
        "textDocument/didOpen",
        object(&[(
            "textDocument",
            object(&[
                ("uri", Value::string(helpers_uri.as_str())),
                ("version", Value::Int(1)),
                ("text", Value::string("set(MY_HELPER_VAR 42)\n")),
            ]),
        )]),
    ));
    let caller_update = msgs
        .iter()
        .find_map(|m| match m {
            RpcMessage::Call { method, params, .. }
                if method == "textDocument/publishDiagnostics"
                    && get(params, "uri") == &Value::string(caller_uri.as_str()) =>
            {
                Some(params)
            }
            _ => None,
        })
        .expect("caller should be republished when open include changes");
    assert!(
        !codes(caller_update).contains(&"undefined-variable".to_owned()),
        "unsaved include buffer should resolve MY_HELPER_VAR: {:?}",
        codes(caller_update)
    );

    // 3. Close helpers.cmake -> caller reverts to disk state and re-emits undefined-variable!
    let close_msgs = server.handle(RpcMessage::notification(
        "textDocument/didClose",
        object(&[(
            "textDocument",
            object(&[("uri", Value::string(helpers_uri.as_str()))]),
        )]),
    ));
    let caller_after_close = close_msgs
        .iter()
        .find_map(|m| match m {
            RpcMessage::Call { method, params, .. }
                if method == "textDocument/publishDiagnostics"
                    && get(params, "uri") == &Value::string(caller_uri.as_str()) =>
            {
                Some(params)
            }
            _ => None,
        })
        .expect("caller should be republished when open include closes");
    assert!(codes(caller_after_close).contains(&"undefined-variable".to_owned()));

    // 4. Create root .rcmake.toml disabling directory-options + notify watched file change.
    host.seed_file(
        &work.join(".rcmake.toml"),
        b"[lint]\ndisable = [\"directory-options\"]\n",
    )
    .unwrap();
    let cfg_msgs = server.handle(RpcMessage::notification(
        "workspace/didChangeWatchedFiles",
        object(&[(
            "changes",
            Value::Array(vec![object(&[
                ("uri", Value::string(format!("{WORK_URI}/.rcmake.toml"))),
                ("type", Value::Int(1)),
            ])]),
        )]),
    ));
    let after_root_cfg = published(cfg_msgs);
    assert_eq!(
        get(&after_root_cfg, "uri"),
        &Value::string(caller_uri.as_str())
    );
    assert!(!codes(&after_root_cfg).contains(&"directory-options".to_owned()));

    // 5. Create nearer /work/sub/.rcmake.toml overriding severity to error -> republishes!
    host.seed_file(
        &work.join("sub/.rcmake.toml"),
        b"[lint.severity]\nunguarded-build-type = \"error\"\n",
    )
    .unwrap();
    let nearer_msgs = server.handle(RpcMessage::notification(
        "workspace/didChangeWatchedFiles",
        object(&[(
            "changes",
            Value::Array(vec![object(&[
                ("uri", Value::string(format!("{WORK_URI}/sub/.rcmake.toml"))),
                ("type", Value::Int(1)),
            ])]),
        )]),
    ));
    let after_nearer = published(nearer_msgs);
    let nearer_codes = codes(&after_nearer);
    assert!(nearer_codes.contains(&"directory-options".to_owned()));
    let bt_sev = array(get(&after_nearer, "diagnostics"))
        .iter()
        .find(|d| get(d, "code") == &Value::string("unguarded-build-type"))
        .map(|d| get(d, "severity"));
    assert_eq!(bt_sev, Some(&Value::UInt(1)));

    // 6. Delete nearer /work/sub/.rcmake.toml -> falls back to root .rcmake.toml!
    host.remove_file(&work.join("sub/.rcmake.toml")).unwrap();
    let del_msgs = server.handle(RpcMessage::notification(
        "workspace/didChangeWatchedFiles",
        object(&[(
            "changes",
            Value::Array(vec![object(&[
                ("uri", Value::string(format!("{WORK_URI}/sub/.rcmake.toml"))),
                ("type", Value::Int(3)),
            ])]),
        )]),
    ));
    let after_del = published(del_msgs);
    assert!(!codes(&after_del).contains(&"directory-options".to_owned()));
}

#[test]
fn finding_15_loss_of_sync_non_cmake_watchers_dep_cap_and_unresolved_include_globs() {
    let diag_codes = |params: &Value| -> Vec<String> {
        array(get(params, "diagnostics"))
            .iter()
            .filter_map(|d| match d {
                Value::Object(map) => map.get(b"code".as_slice()).and_then(|v| match v {
                    Value::String(s) => Some(String::from_utf8_lossy(s).into_owned()),
                    _ => None,
                }),
                _ => None,
            })
            .collect()
    };
    fn find_publish<'a>(messages: &'a [RpcMessage], target_uri: &str) -> Option<&'a Value> {
        messages.iter().find_map(|m| match m {
            RpcMessage::Call { method, params, .. }
                if method == "textDocument/publishDiagnostics"
                    && get(params, "uri") == &Value::string(target_uri) =>
            {
                Some(params)
            }
            _ => None,
        })
    }

    // 1. Loss of sync on an open included buffer drops its overlay and refreshes callers!
    {
        let host = host();
        let work = Path::new(WORK_DIR);
        host.seed_file(&work.join("helpers.cmake"), b"set(UNRELATED 1)\n")
            .unwrap();
        let mut server = initialized(&host, false);
        let caller_uri = format!("{WORK_URI}/CMakeLists.txt");
        let helpers_uri = format!("{WORK_URI}/helpers.cmake");
        let caller_text = concat!(
            "project(SyncLoss)\n",
            "include(helpers.cmake)\n",
            "set(MY_HELPER_VA 1)\n",
            "message(STATUS \"${MY_HELPER_VAR}\")\n",
        );
        let open_caller = published(server.handle(RpcMessage::notification(
            "textDocument/didOpen",
            object(&[(
                "textDocument",
                object(&[
                    ("uri", Value::string(caller_uri.as_str())),
                    ("version", Value::Int(1)),
                    ("text", Value::string(caller_text)),
                ]),
            )]),
        )));
        assert!(diag_codes(&open_caller).contains(&"undefined-variable".to_owned()));

        // Open unsaved helpers.cmake defining MY_HELPER_VAR -> caller becomes clean.
        let open_helpers = server.handle(RpcMessage::notification(
            "textDocument/didOpen",
            object(&[(
                "textDocument",
                object(&[
                    ("uri", Value::string(helpers_uri.as_str())),
                    ("version", Value::Int(1)),
                    ("text", Value::string("set(MY_HELPER_VAR 42)\n")),
                ]),
            )]),
        ));
        let clean = find_publish(&open_helpers, &caller_uri).expect("caller republished on open");
        assert!(
            !diag_codes(clean).contains(&"undefined-variable".to_owned()),
            "got {:?}",
            diag_codes(clean)
        );

        // Send invalid incremental edit to helpers.cmake -> loss of sync drops overlay AND
        // immediately republishes caller against disk state (re-emitting undefined-variable)!
        let bad_edit = decode(&format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didChange","params":{{"textDocument":{{"uri":"{helpers_uri}","version":2}},"contentChanges":[{{"range":{{"start":{{"line":99,"character":0}},"end":{{"line":99,"character":0}}}},"text":"x"}}]}}}}"#
        ));
        let desync_msgs = server.handle(bad_edit);
        let refreshed = find_publish(&desync_msgs, &caller_uri)
            .expect("caller must be republished when included buffer loses synchronization");
        assert!(
            diag_codes(refreshed).contains(&"undefined-variable".to_owned()),
            "caller should revert to disk facts on loss of sync: {:?}",
            diag_codes(refreshed)
        );
    }

    // 2. Non-.cmake consumed include file gets dynamically watched and refreshes caller on change,
    //    and unresolved include clears glob origin (no false glob-without-configure-depends).
    {
        let host = host();
        let work = Path::new(WORK_DIR);
        host.seed_file(&work.join("defs.inc"), b"set(OTHER 1)\n")
            .unwrap();
        let mut server = Server::new(&host);
        result(server.handle(decode(&format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"rootUri":"{WORK_URI}","capabilities":{{"workspace":{{"didChangeWatchedFiles":{{"dynamicRegistration":true}}}}}}}}}}"#
        ))));
        server.handle(RpcMessage::notification("initialized", Value::object()));

        let caller_uri = format!("{WORK_URI}/CMakeLists.txt");
        let caller_text = concat!(
            "project(NonCmakeInclude)\n",
            "include(defs.inc)\n",
            "set(MY_INC_VA 1)\n",
            "message(STATUS \"${MY_INC_VAR}\")\n",
            "file(GLOB SRCS src/*.cpp)\n",
            "include(unresolved_override.cmake)\n",
            "add_executable(app ${SRCS})\n",
        );
        let open_msgs = server.handle(RpcMessage::notification(
            "textDocument/didOpen",
            object(&[(
                "textDocument",
                object(&[
                    ("uri", Value::string(caller_uri.as_str())),
                    ("version", Value::Int(1)),
                    ("text", Value::string(caller_text)),
                ]),
            )]),
        ));
        let initial = find_publish(&open_msgs, &caller_uri).expect("initial publish");
        let codes = diag_codes(initial);
        assert!(codes.contains(&"undefined-variable".to_owned()));
        assert!(
            !codes.contains(&"glob-without-configure-depends".to_owned()),
            "unresolved include must clear glob origins: {codes:?}"
        );
        // Must have dynamically registered a watcher covering defs.inc!
        let has_inc_watch = open_msgs.iter().any(|m| match m {
            RpcMessage::Call {
                id: Some(_),
                method,
                params,
            } if method == "client/registerCapability" => {
                format!("{params:?}").contains("defs.inc")
            }
            _ => false,
        });
        assert!(
            has_inc_watch,
            "expected dynamic watcher registration for defs.inc in {open_msgs:?}"
        );

        // Update defs.inc on disk and deliver didChangeWatchedFiles -> caller republishes clean!
        host.seed_file(&work.join("defs.inc"), b"set(MY_INC_VAR 1)\n")
            .unwrap();
        let watch_msgs = server.handle(RpcMessage::notification(
            "workspace/didChangeWatchedFiles",
            object(&[(
                "changes",
                Value::Array(vec![object(&[
                    ("uri", Value::string(format!("{WORK_URI}/defs.inc"))),
                    ("type", Value::Int(2)),
                ])]),
            )]),
        ));
        let after_watch =
            find_publish(&watch_msgs, &caller_uri).expect("caller refreshed after defs.inc edit");
        assert!(
            !diag_codes(after_watch).contains(&"undefined-variable".to_owned()),
            "defs.inc update should clear undefined-variable: {:?}",
            diag_codes(after_watch)
        );
    }

    // 3. Dep-cap overflow marks dependencies incomplete so creating a file beyond the probe cap
    //    still triggers conservative refresh!
    {
        let host = host();
        let work = Path::new(WORK_DIR);
        let mut server = Server::new(&host);
        result(server.handle(decode(&format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"rootUri":"{WORK_URI}","capabilities":{{"workspace":{{"didChangeWatchedFiles":{{"dynamicRegistration":true}}}}}}}}}}"#
        ))));
        server.handle(RpcMessage::notification("initialized", Value::object()));

        // Exceed default probe miss cap (64 * 4 = 256) before probing tail_probe.cmake.
        let mut caller_text = String::from("project(CapTest)\n");
        for i in 0..260 {
            caller_text.push_str(&format!("include(missing_{i}.cmake OPTIONAL)\n"));
        }
        caller_text.push_str("include(tail_probe.cmake OPTIONAL)\n");
        caller_text.push_str("set(TAIL_VA 1)\nmessage(STATUS \"${TAIL_VAR}\")\n");

        let caller_uri = format!("{WORK_URI}/CMakeLists.txt");
        let open_msgs = server.handle(RpcMessage::notification(
            "textDocument/didOpen",
            object(&[(
                "textDocument",
                object(&[
                    ("uri", Value::string(caller_uri.as_str())),
                    ("version", Value::Int(1)),
                    ("text", Value::string(caller_text)),
                ]),
            )]),
        ));
        let initial = find_publish(&open_msgs, &caller_uri).expect("initial publish");
        assert!(diag_codes(initial).contains(&"undefined-variable".to_owned()));

        // Create tail_probe.cmake (probe #261, beyond cap!) and notify watcher.
        host.seed_file(&work.join("tail_probe.cmake"), b"set(TAIL_VAR 1)\n")
            .unwrap();
        let watch_msgs = server.handle(RpcMessage::notification(
            "workspace/didChangeWatchedFiles",
            object(&[(
                "changes",
                Value::Array(vec![object(&[
                    ("uri", Value::string(format!("{WORK_URI}/tail_probe.cmake"))),
                    ("type", Value::Int(1)),
                ])]),
            )]),
        ));
        let refreshed = find_publish(&watch_msgs, &caller_uri).expect(
            "incomplete dependency set must conservatively refresh on workspace file event",
        );
        assert!(
            !diag_codes(refreshed).contains(&"undefined-variable".to_owned()),
            "conservative refresh should pick up tail_probe.cmake: {:?}",
            diag_codes(refreshed)
        );
    }

    // 4. Creating a nearer .rcmake.toml in an intermediate ancestor directory immediately updates
    //    diagnostics for deeply nested open documents.
    {
        let host = host();
        let work = Path::new(WORK_DIR);
        host.seed_file(
            &work.join(".rcmake.toml"),
            b"[lint]\ndisable = [\"directory-options\"]\n",
        )
        .unwrap();
        let mut server = Server::new(&host);
        result(server.handle(decode(&format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"rootUri":"{WORK_URI}","capabilities":{{"workspace":{{"didChangeWatchedFiles":{{"dynamicRegistration":true}}}}}}}}}}"#
        ))));
        server.handle(RpcMessage::notification("initialized", Value::object()));

        let nested_uri = format!("{WORK_URI}/pkg/sub/CMakeLists.txt");
        let open_nested = published(server.handle(RpcMessage::notification(
            "textDocument/didOpen",
            object(&[(
                "textDocument",
                object(&[
                    ("uri", Value::string(nested_uri.as_str())),
                    ("version", Value::Int(1)),
                    (
                        "text",
                        Value::string("project(Nested)\ninclude_directories(inc)\n"),
                    ),
                ]),
            )]),
        )));
        assert!(!diag_codes(&open_nested).contains(&"directory-options".to_owned()));

        // Create nearer /work/pkg/.rcmake.toml (re-enabling directory-options).
        host.seed_file(
            &work.join("pkg/.rcmake.toml"),
            b"[lint.severity]\ndirectory-options = \"error\"\n",
        )
        .unwrap();
        let nearer_msgs = server.handle(RpcMessage::notification(
            "workspace/didChangeWatchedFiles",
            object(&[(
                "changes",
                Value::Array(vec![object(&[
                    ("uri", Value::string(format!("{WORK_URI}/pkg/.rcmake.toml"))),
                    ("type", Value::Int(1)),
                ])]),
            )]),
        ));
        let refreshed = find_publish(&nearer_msgs, &nested_uri)
            .expect("nested doc must refresh when intermediate .rcmake.toml is created");
        assert!(diag_codes(refreshed).contains(&"directory-options".to_owned()));
    }
}

#[test]
fn formatter_reports_the_response_limit() {
    let host = host();
    let mut server = initialized(&host, false);
    let text = format!(
        "{}{}{}",
        "if(ON)\n".repeat(1000),
        "message(TEXT)\n".repeat(600),
        "endif()\n".repeat(1000),
    );
    assert!(array(get(&open(&mut server, &text), "diagnostics")).is_empty());
    let response = server.handle(RpcMessage::request(
        RequestId::Int(2),
        "textDocument/formatting",
        object(&[
            ("textDocument", object(&[("uri", Value::string(URI))])),
            (
                "options",
                object(&[
                    ("tabSize", Value::Int(32)),
                    ("insertSpaces", Value::Bool(true)),
                ]),
            ),
        ]),
    ));
    assert!(matches!(
        response.as_slice(),
        [RpcMessage::Error { code: -32803, message, .. }]
            if message.contains("16 MiB response limit")
    ));
}

#[test]
fn semantic_provider_can_override_hover_and_definition() {
    struct Provider;
    impl SemanticProvider for Provider {
        fn hover(&self, context: &SemanticContext<'_>) -> Option<String> {
            Some(format!("Expanded value for version {}", context.version))
        }

        fn definition(&self, context: &SemanticContext<'_>) -> Option<SemanticLocation> {
            Some(SemanticLocation {
                uri: "file:///work/evaluated.cmake".into(),
                start: context.position,
                end: context.position,
            })
        }
    }
    let host = host();
    let provider = Provider;
    let mut server = initialized(&host, false).with_semantic_provider(&provider);
    open(&mut server, "message(STATUS \"${VALUE}\")\n");
    let hover = at(&mut server, "textDocument/hover", 0, 19);
    assert_eq!(
        get(get(&hover, "contents"), "value"),
        &Value::string("Expanded value for version 1")
    );
    let def = at(&mut server, "textDocument/definition", 0, 19);
    assert_eq!(
        get(&def, "uri"),
        &Value::string("file:///work/evaluated.cmake")
    );
}

#[test]
fn lifecycle_rejects_early_requests_and_reports_exit_without_shutdown() {
    let host = host();
    let mut server = Server::new(&host);
    let response = server.handle(RpcMessage::request(
        RequestId::Int(1),
        "textDocument/hover",
        Value::Null,
    ));
    assert!(matches!(
        response.as_slice(),
        [RpcMessage::Error { code: -32002, .. }]
    ));
    server.handle(RpcMessage::notification("exit", Value::Null));
    assert_eq!(server.exit_code(), Some(1));
}

#[test]
fn l0_open_latency_on_two_thousand_lines() {
    #[cfg(debug_assertions)]
    const BUDGET_MS: u64 = 50;
    #[cfg(not(debug_assertions))]
    const BUDGET_MS: u64 = 5;

    let host = host();
    let clock = NativeHost::new().unwrap();
    let text: String = (0..2000)
        .map(|line| format!("set(VALUE_{line} \"example\")\n"))
        .collect();
    let mut samples = Vec::new();
    for _ in 0..9 {
        let mut server = initialized(&host, false);
        let start = clock.monotonic();
        assert!(array(get(&open(&mut server, &text), "diagnostics")).is_empty());
        samples.push(clock.monotonic() - start);
    }
    samples.sort_unstable();
    let median = samples.get(4).unwrap();
    assert!(
        *median < Duration::from_millis(BUDGET_MS),
        "Median L0 open time reached or exceeded {BUDGET_MS} ms. Samples {samples:?}"
    );
}
