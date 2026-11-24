//! Language server sessions with an in-memory host.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::{path::Path, time::Duration};

use rcmake_host::{Host, NativeHost, PureHost};
use rcmake_json::Value;
use rcmake_lsp::{
    RequestId, RpcMessage, SemanticContext, SemanticLocation, SemanticProvider, Server,
};

const URI: &str = "file:///work/CMakeLists.txt";

fn host() -> PureHost {
    PureHost::new("/work".into(), "/bin/rcmake-lsp".into())
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
fn scripted_editing_session() {
    let host = host();
    host.seed_file(Path::new("/work/helpers space.cmake"), b"set(HELPER ON)\n")
        .unwrap();
    host.create_dir_all(Path::new("/work/sub")).unwrap();
    host.seed_file(Path::new("/work/sub/CMakeLists.txt"), b"project(Sub)\n")
        .unwrap();
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
        &Value::string("project\n\nName the project and enable its languages.")
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
        &Value::string("file:///work/helpers%20space.cmake")
    );
    let subdir = at(&mut server, "textDocument/definition", 9, 18);
    assert_eq!(
        get(&subdir, "uri"),
        &Value::string("file:///work/sub/CMakeLists.txt")
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

    let change = decode(
        r#"{"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":"file:///work/CMakeLists.txt","version":2},"contentChanges":[{"text":"project(Example)\nmessage(\"unfinished"}]}}"#,
    );
    let diagnostics = published(server.handle(change));
    let diagnostic = array(get(&diagnostics, "diagnostics")).first().unwrap();
    assert_eq!(get(diagnostic, "severity"), &Value::UInt(1));
    assert_eq!(
        get(get(get(diagnostic, "range"), "start"), "line"),
        &Value::UInt(1)
    );
    let save = decode(
        r#"{"jsonrpc":"2.0","method":"textDocument/didSave","params":{"textDocument":{"uri":"file:///work/CMakeLists.txt"}}}"#,
    );
    assert_eq!(published(server.handle(save)), diagnostics);
    let close = decode(
        r#"{"jsonrpc":"2.0","method":"textDocument/didClose","params":{"textDocument":{"uri":"file:///work/CMakeLists.txt"}}}"#,
    );
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
            "Rcmake lost synchronization with {URI}. {recovery}"
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
    assert_indent_edits(&response, &[1, 4, 7], "\t");
}

#[test]
fn formatter_aligns_a_close_with_the_opener_when_no_argument_precedes_it() {
    let host = host();
    let mut server = initialized(&host, false);
    let text = concat!(
        "if(ON)\rmessage(\rone\rtwo)\r",
        "message(\rone\r) # close\r",
        "message(\r(\rone\r) two)\rendif()\r",
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
