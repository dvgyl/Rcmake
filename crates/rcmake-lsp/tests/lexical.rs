//! Lexical language features with open documents and project files.

#![allow(
    clippy::indexing_slicing,
    clippy::panic,
    clippy::string_slice,
    clippy::unwrap_used
)]

use std::path::Path;

use rcmake_host::{Host, HostResult, PureHost};
use rcmake_json::Value;
use rcmake_lsp::{RequestId, RpcMessage, SemanticContext, SemanticProvider, Server};

#[cfg(windows)]
const WORK_DIR: &str = "C:/work";
#[cfg(not(windows))]
const WORK_DIR: &str = "/work";
#[cfg(windows)]
const WORK_URI: &str = "file:///C:/work";
#[cfg(not(windows))]
const WORK_URI: &str = "file:///work";
#[cfg(windows)]
const ROOT: &str = "file:///C:/work/CMakeLists.txt";
#[cfg(not(windows))]
const ROOT: &str = "file:///work/CMakeLists.txt";

fn object(fields: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
}

fn get<'a>(value: &'a Value, key: &str) -> &'a Value {
    let Value::Object(fields) = value else {
        panic!("Expected an object. Got {value:?}.");
    };
    fields.get(key.as_bytes()).unwrap()
}

fn array(value: &Value) -> &[Value] {
    let Value::Array(items) = value else {
        panic!("Expected an array. Got {value:?}.");
    };
    items
}

fn number(value: &Value) -> usize {
    let Value::UInt(value) = value else {
        panic!("Expected an unsigned integer. Got {value:?}.");
    };
    usize::try_from(*value).unwrap()
}

fn text(value: &Value) -> &str {
    let Value::String(value) = value else {
        panic!("Expected a string. Got {value:?}.");
    };
    std::str::from_utf8(value).unwrap()
}

fn request(server: &mut Server<'_>, method: &str, params: Value) -> Value {
    let mut messages = server.handle(RpcMessage::request(RequestId::Int(1), method, params));
    match messages.pop() {
        Some(RpcMessage::Result { result, .. }) => result,
        other => panic!("Expected a result for {method}. Got {other:?}."),
    }
}

fn notify(server: &mut Server<'_>, method: &str, params: Value) {
    server.handle(RpcMessage::notification(method, params));
}

fn server<'a>(host: &'a dyn Host, formats: &[&str]) -> Server<'a> {
    let mut server = Server::new(host);
    request(
        &mut server,
        "initialize",
        object([
            ("rootUri", Value::string(WORK_URI)),
            (
                "capabilities",
                object([(
                    "textDocument",
                    object([(
                        "hover",
                        object([(
                            "contentFormat",
                            Value::Array(formats.iter().copied().map(Value::string).collect()),
                        )]),
                    )]),
                )]),
            ),
        ]),
    );
    notify(&mut server, "initialized", Value::object());
    server
}

fn open(server: &mut Server<'_>, uri: &str, text: &str) {
    notify(
        server,
        "textDocument/didOpen",
        object([(
            "textDocument",
            object([
                ("uri", Value::string(uri)),
                ("version", Value::Int(1)),
                ("text", Value::string(text)),
            ]),
        )]),
    );
}

fn at(server: &mut Server<'_>, method: &str, uri: &str, line: usize, character: usize) -> Value {
    request(
        server,
        method,
        object([
            ("textDocument", object([("uri", Value::string(uri))])),
            (
                "position",
                object([
                    ("line", Value::UInt(line as u64)),
                    ("character", Value::UInt(character as u64)),
                ]),
            ),
        ]),
    )
}

fn site(
    server: &mut Server<'_>,
    method: &str,
    uri: &str,
    text: &str,
    mark: &str,
    delta: usize,
) -> Value {
    let offset = text.find(mark).unwrap() + delta;
    let prefix = &text[..offset];
    let line = prefix.bytes().filter(|&byte| byte == b'\n').count();
    let character = prefix.rsplit('\n').next().unwrap().encode_utf16().count();
    at(server, method, uri, line, character)
}

fn file(host: &PureHost, path: &str, text: &str) -> HostResult<()> {
    host.seed_file(&Path::new(WORK_DIR).join(path), text.as_bytes())
}

fn workspace_uri(path: &str) -> String {
    format!("{WORK_URI}/{path}")
}

fn host() -> PureHost {
    let work = Path::new(WORK_DIR);
    PureHost::new(work.into(), work.join("bin/rcmake-lsp"))
}

fn assert_location(value: &Value, uri: &str, line: usize) {
    assert_eq!(get(value, "uri"), &Value::string(uri));
    assert_eq!(number(get(get(get(value, "range"), "start"), "line")), line);
}

fn tokens(server: &mut Server<'_>, text: &str) -> Vec<(usize, String, usize)> {
    let response = request(
        server,
        "textDocument/semanticTokens/full",
        object([("textDocument", object([("uri", Value::string(ROOT))]))]),
    );
    let mut line = 0;
    let mut character = 0;
    let source: Vec<_> = text.lines().collect();
    array(get(&response, "data"))
        .as_chunks::<5>()
        .0
        .iter()
        .map(|values| {
            let delta_line = number(&values[0]);
            if delta_line > 0 {
                line += delta_line;
                character = 0;
            }
            character += number(&values[1]);
            let width = number(&values[2]);
            let spelling = String::from_utf16(
                &source[line]
                    .encode_utf16()
                    .skip(character)
                    .take(width)
                    .collect::<Vec<_>>(),
            )
            .unwrap();
            (line, spelling, number(&values[3]))
        })
        .collect()
}

#[test]
fn unfinished_call_preserves_outline_tokens_and_completion() {
    let host = host();
    let mut server = server(&host, &[]);
    let text = concat!(
        "if(ON)\n",
        "  function(saved)\n",
        "  endfunction()\n",
        "  target_link_libraries(app PRI\n",
        "  add_library(recovered INTERFACE)\n",
        "endif()\n",
    );
    open(&mut server, ROOT, text);
    let outline = request(
        &mut server,
        "textDocument/documentSymbol",
        object([("textDocument", object([("uri", Value::string(ROOT))]))]),
    );
    let names = array(&outline)
        .iter()
        .map(|item| get(item, "name"))
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [&Value::string("saved"), &Value::string("recovered")]
    );
    let completion = site(&mut server, "textDocument/completion", ROOT, text, "PRI", 3);
    let [private] = array(&completion) else {
        panic!("Expected one completion. Got {completion:?}.");
    };
    assert_eq!(get(private, "label"), &Value::string("PRIVATE"));
    let edit = get(private, "textEdit");
    let replacement = get(edit, "range");
    assert_eq!(number(get(get(replacement, "start"), "line")), 3);
    assert_eq!(
        number(get(get(replacement, "end"), "character"))
            - number(get(get(replacement, "start"), "character")),
        3
    );
    assert!(tokens(&mut server, text).contains(&(4, "recovered".into(), 5)));
}

#[test]
fn project_definitions_use_source_context_and_open_edits() -> HostResult<()> {
    let host = host();
    let setup = concat!(
        "include(Extras.cmake)\n",
        "include(\"${CMAKE_CURRENT_LIST_DIR}/Helpers.cmake\")\n",
        "add_subdirectory(src)\n",
        "list(APPEND CMAKE_MODULE_PATH \"${CMAKE_CURRENT_LIST_DIR}/modules\")\n",
        "include(LocalModule)\n",
    );
    file(&host, "cmake/Setup.cmake", setup)?;
    file(
        &host,
        "Extras.cmake",
        "function(my_helper)\nendfunction()\n",
    )?;
    file(
        &host,
        "cmake/Extras.cmake",
        "function(decoy)\nendfunction()\n",
    )?;
    file(
        &host,
        "cmake/Helpers.cmake",
        "macro(My_Macro)\nendmacro()\n",
    )?;
    file(
        &host,
        "src/CMakeLists.txt",
        "add_library(child_lib INTERFACE)\n",
    )?;
    file(
        &host,
        "cmake/src/CMakeLists.txt",
        "add_library(decoy INTERFACE)\n",
    )?;
    file(
        &host,
        "cmake/modules/LocalModule.cmake",
        "add_library(tools INTERFACE)\nadd_library(tools_alias ALIAS tools)\n",
    )?;
    let root = concat!(
        "include(cmake/Setup.cmake)\n",
        "add_executable(app main.c)\n",
        "MY_HELPER()\n",
        "my_macro()\n",
        "target_link_libraries(app PRIVATE tools_alias child_lib)\n",
    );
    let mut server = server(&host, &[]);
    open(&mut server, ROOT, root);

    let go = "textDocument/definition";
    let extras = workspace_uri("Extras.cmake");
    let setup_uri = workspace_uri("cmake/Setup.cmake");
    for (mark, path, line) in [
        ("MY_HELPER", "Extras.cmake", 0),
        ("my_macro", "cmake/Helpers.cmake", 0),
        ("tools_alias", "cmake/modules/LocalModule.cmake", 1),
        ("child_lib", "src/CMakeLists.txt", 0),
    ] {
        assert_location(
            &site(&mut server, go, ROOT, root, mark, 1),
            &workspace_uri(path),
            line,
        );
    }

    open(&mut server, &setup_uri, setup);
    for (mark, path) in [
        ("Extras.cmake", "Extras.cmake"),
        ("Helpers.cmake", "cmake/Helpers.cmake"),
        ("(src)", "src/CMakeLists.txt"),
        ("(LocalModule)", "cmake/modules/LocalModule.cmake"),
    ] {
        assert_location(
            &site(&mut server, go, &setup_uri, setup, mark, 1),
            &workspace_uri(path),
            0,
        );
    }
    open(
        &mut server,
        &extras,
        "\nfunction(my_helper)\nendfunction()\n",
    );
    notify(
        &mut server,
        "textDocument/didChange",
        object([
            (
                "textDocument",
                object([
                    ("uri", Value::string(extras.as_str())),
                    ("version", Value::Int(2)),
                ]),
            ),
            (
                "contentChanges",
                Value::Array(vec![object([(
                    "text",
                    Value::string("\n\nfunction(my_helper)\nendfunction()\n"),
                )])]),
            ),
        ]),
    );
    assert_location(
        &site(&mut server, go, ROOT, root, "MY_HELPER", 1),
        &extras,
        2,
    );
    notify(
        &mut server,
        "textDocument/didClose",
        object([(
            "textDocument",
            object([("uri", Value::string(extras.as_str()))]),
        )]),
    );
    assert_location(
        &site(&mut server, go, ROOT, root, "MY_HELPER", 1),
        &extras,
        0,
    );
    Ok(())
}

#[test]
fn conditional_module_paths_return_both_candidates() -> Result<(), Box<dyn std::error::Error>> {
    let host = host();
    file(&host, "a/Choices.cmake", "message(STATUS a)\n")?;
    file(&host, "b/Choices.cmake", "message(STATUS b)\n")?;
    let text = "if(FLAG)\n  set(CMAKE_MODULE_PATH a)\nelse()\n  set(CMAKE_MODULE_PATH b)\nendif()\ninclude(Choices)\n";
    let mut server = server(&host, &[]);
    open(&mut server, ROOT, text);
    let definition = site(
        &mut server,
        "textDocument/definition",
        ROOT,
        text,
        "Choices",
        2,
    );
    let candidates = array(&definition);
    let [a, b] = candidates else {
        return Err(format!("Expected two module definitions. Got {definition:?}.").into());
    };
    assert_location(a, &workspace_uri("a/Choices.cmake"), 0);
    assert_location(b, &workspace_uri("b/Choices.cmake"), 0);
    Ok(())
}

#[test]
fn definitions_and_tokens_keep_argument_roles_and_variable_namespaces() {
    let host = host();
    let mut server = server(&host, &[]);
    let text = concat!(
        "set(HOME local)\n",
        "set(CACHE{HOME} TYPE STRING VALUE cached)\n",
        "set(ENV{SHELL} env)\n",
        "set(INNER suffix)\n",
        "add_library(PRIVATE INTERFACE)\n",
        "add_library(app INTERFACE)\n",
        "message(STATUS PRIVATE)\n",
        "target_compile_options(app PRIVATE app)\n",
        "message(\"😃 ${HOME} $ENV{HOME} $CACHE{HOME} $ENV{SHELL}\")\n",
        "message(\"$<IF:$<TARGET_EXISTS:app>,${VAR_${INNER}},no>\")\n",
    );
    open(&mut server, ROOT, text);
    let go = "textDocument/definition";
    for (line, column) in [(6, 16), (7, 28), (7, 36)] {
        assert_eq!(at(&mut server, go, ROOT, line, column), Value::Null);
    }
    assert_eq!(
        site(&mut server, go, ROOT, text, "$ENV{HOME}", 6),
        Value::Null
    );
    for (mark, delta, target) in [
        ("${HOME}", 3, 0),
        ("$CACHE{HOME}", 8, 1),
        ("$ENV{SHELL}", 6, 2),
        ("${INNER}", 3, 3),
        ("TARGET_EXISTS:app", 15, 5),
    ] {
        assert_location(
            &site(&mut server, go, ROOT, text, mark, delta),
            ROOT,
            target,
        );
    }
    let semantic = tokens(&mut server, text);
    assert!(
        !semantic
            .iter()
            .any(|(line, spelling, _)| *line == 6 && spelling == "PRIVATE")
    );
    for (line, spelling, kind) in [
        (7, "PRIVATE", 1),
        (7, "app", 5),
        (9, "$<TARGET_EXISTS", 6),
        (9, "app", 5),
        (9, "${INNER}", 2),
    ] {
        assert!(
            semantic.contains(&(line, spelling.into(), kind)),
            "Missing {spelling} in {semantic:?}."
        );
    }
}

#[test]
fn hover_negotiates_markup_and_completion_replaces_the_word() {
    let host = host();
    let source = concat!(
        "add_library(app INTERFACE)\n",
        "target_link_libraries(app PRIVATE lib)\n",
        "FetchContent_Declare(example URL url)\n",
        "# target_link\n",
        "message([=[target_link]=])\n",
    );
    for (format, signature) in [
        ("markdown", "```cmake\ntarget_link_libraries"),
        ("plaintext", "\ntarget_link_libraries(<target>"),
    ] {
        let mut server = server(&host, &[format, "markdown"]);
        open(&mut server, ROOT, source);
        let hover = "textDocument/hover";
        let command = site(&mut server, hover, ROOT, source, "target_link_libraries", 4);
        let contents = get(&command, "contents");
        assert_eq!(get(contents, "kind"), &Value::string(format));
        assert!(text(get(contents, "value")).contains(signature));
        let keyword = site(&mut server, hover, ROOT, source, "PRIVATE", 2);
        assert!(
            text(get(get(&keyword, "contents"), "value"))
                .contains("Use these items to build this target.")
        );
        let module = site(&mut server, hover, ROOT, source, "FetchContent_Declare", 3);
        assert!(
            text(get(get(&module, "contents"), "value"))
                .contains("Record how to obtain a dependency")
        );
        let completion = "textDocument/completion";
        let command = site(
            &mut server,
            completion,
            ROOT,
            source,
            "target_link_libraries",
            8,
        );
        let item = array(&command)
            .iter()
            .find(|item| get(item, "label") == &Value::string("target_link_libraries"))
            .unwrap();
        let span = get(get(item, "textEdit"), "range");
        assert_eq!(number(get(get(span, "start"), "character")), 0);
        assert_eq!(
            number(get(get(span, "end"), "character")),
            "target_link_libraries".len()
        );
        let target = at(
            &mut server,
            completion,
            ROOT,
            1,
            "target_link_libraries(a".len(),
        );
        assert_eq!(array(&target).len(), 1);
        assert_eq!(get(&array(&target)[0], "label"), &Value::string("app"));
        for (line, column) in [(3, 8), (4, 17)] {
            assert!(array(&at(&mut server, completion, ROOT, line, column)).is_empty());
        }
    }
}

#[test]
fn hover_marks_values_from_an_earlier_snapshot() {
    struct Snapshot(i64);

    impl SemanticProvider for Snapshot {
        fn hover_with_version(&self, _: &SemanticContext<'_>) -> Option<(String, Option<i64>)> {
            Some(("Expanded value is ON.".into(), Some(self.0)))
        }
    }

    let host = host();
    for (version, expected) in [
        (
            0,
            "Provisional. This result uses document version 0.\n\nExpanded value is ON.",
        ),
        (1, "Expanded value is ON."),
    ] {
        let snapshot = Snapshot(version);
        let mut server = server(&host, &[]).with_semantic_provider(&snapshot);
        open(&mut server, ROOT, "message(\"${VALUE}\")\n");
        let hover = at(&mut server, "textDocument/hover", ROOT, 0, 14);
        assert_eq!(text(get(get(&hover, "contents"), "value")), expected);
    }
}
