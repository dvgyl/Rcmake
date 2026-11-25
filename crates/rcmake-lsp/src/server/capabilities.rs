use rcmake_json::Value;

use crate::{analysis, json::object, position::PositionEncoding};

pub(super) fn capabilities(encoding: PositionEncoding) -> Value {
    object([
        (
            "serverInfo",
            object([
                ("name", Value::string("rcmake-lsp")),
                ("version", Value::string(env!("CARGO_PKG_VERSION"))),
            ]),
        ),
        (
            "capabilities",
            object([
                ("positionEncoding", Value::string(encoding.name())),
                (
                    "textDocumentSync",
                    object([
                        ("openClose", Value::Bool(true)),
                        ("change", Value::Int(2)),
                        ("save", object([("includeText", Value::Bool(true))])),
                    ]),
                ),
                ("documentSymbolProvider", Value::Bool(true)),
                ("documentFormattingProvider", Value::Bool(true)),
                ("definitionProvider", Value::Bool(true)),
                ("hoverProvider", Value::Bool(true)),
                ("codeActionProvider", Value::Bool(true)),
                (
                    "completionProvider",
                    object([(
                        "triggerCharacters",
                        Value::Array(["$", "{", "<", ":"].map(Value::string).into()),
                    )]),
                ),
                (
                    "semanticTokensProvider",
                    object([
                        ("full", Value::Bool(true)),
                        (
                            "legend",
                            object([
                                (
                                    "tokenTypes",
                                    Value::Array(analysis::TOKEN_TYPES.map(Value::string).into()),
                                ),
                                (
                                    "tokenModifiers",
                                    Value::Array(
                                        analysis::TOKEN_MODIFIERS.map(Value::string).into(),
                                    ),
                                ),
                            ]),
                        ),
                    ]),
                ),
            ]),
        ),
    ])
}
