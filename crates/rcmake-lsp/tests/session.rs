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
    assert_eq!(
        get(array(&formatted).first().unwrap(), "newText"),
        &Value::string(text.replace("# A comment\nmessage", "  # A comment\n  message"))
    );

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

#[test]
fn incremental_edits_use_negotiated_encoding_and_keep_the_last_valid_version() {
    for (utf8, column) in [(false, 11), (true, 13)] {
        let host = host();
        let mut server = initialized(&host, utf8);
        open(&mut server, "message(\"😃X\")\r\n");
        let change = |version, start, end, text| {
            decode(&format!(
                r#"{{"jsonrpc":"2.0","method":"textDocument/didChange","params":{{"textDocument":{{"uri":"{URI}","version":{version}}},"contentChanges":[{{"range":{{"start":{{"line":0,"character":{start}}},"end":{{"line":0,"character":{end}}}}},"text":"{text}"}}]}}}}"#
            ))
        };
        assert!(server.handle(change(2, 10, column, "BAD")).is_empty());
        let diagnostics = published(server.handle(change(3, column, column + 1, "Y")));
        assert_eq!(get(&diagnostics, "version"), &Value::Int(3));
        assert!(array(get(&diagnostics, "diagnostics")).is_empty());
        assert!(server.handle(change(2, column, column + 1, "Z")).is_empty());
    }
}

#[test]
fn formatter_preserves_multiline_literals_comments_and_line_endings() {
    let host = host();
    let mut server = initialized(&host, true);
    let text = "if(ON)\r\nset(V [==[\r\nkeep  this\r\n]==])\r\n#[=[\r\n# keep this too\r\n]=]\r\nendif()\r\n";
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
    let edit = array(&response).first().unwrap();
    assert_eq!(
        get(edit, "newText"),
        &Value::string(text.replace("set(V", "\tset(V").replace("#[=[", "\t#[=["))
    );
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
        *median < Duration::from_millis(5),
        "Median L0 open time exceeded 5 ms. Samples {samples:?}"
    );
}
