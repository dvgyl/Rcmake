//! Debug Adapter Protocol requests and message builders.
//!
//! A request that does not have the required argument fields is a session error, as in
//! the `cppdap` library that CMake uses: CMake prints
//! `[CMake Debugger] DAP session error: <text>` and the session ends.

use rcmake_base::BString;
use rcmake_json::{ReadOptions, Value};

/// Text of a session error for a request that does not deserialize.
pub const DESERIALIZE_ERROR: &str = "Failed to deserialize request";

/// A parsed client request.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    /// Sequence number of the request (`request_seq` of the response).
    pub seq: i64,
    /// Request name (`command`).
    pub command: String,
    /// Typed arguments.
    pub kind: RequestKind,
}

/// `format` argument of `stackTrace`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameFormat {
    /// Show parameter names, as `name(...)`.
    pub parameters: bool,
    /// Show parameter values.
    pub parameter_values: bool,
    /// Show the line, as `name Line: <n>`.
    pub line: bool,
}

/// The requests that the adapter handles.
#[derive(Clone, Debug, PartialEq)]
pub enum RequestKind {
    /// `initialize`.
    Initialize {
        /// `supportsVariableType`.
        supports_variable_type: bool,
    },
    /// `setBreakpoints`.
    SetBreakpoints {
        /// `source.path` (empty if absent).
        path: BString,
        /// `breakpoints[].line`.
        lines: Vec<i64>,
    },
    /// `setExceptionBreakpoints`.
    SetExceptionBreakpoints {
        /// `filters`.
        filters: Vec<BString>,
    },
    /// `exceptionInfo`.
    ExceptionInfo,
    /// `configurationDone`.
    ConfigurationDone,
    /// `threads`.
    Threads,
    /// `stackTrace`.
    StackTrace {
        /// `threadId`.
        thread: i64,
        /// `format`.
        format: FrameFormat,
    },
    /// `scopes`.
    Scopes {
        /// `frameId`.
        frame: i64,
    },
    /// `variables`.
    Variables {
        /// `variablesReference`.
        reference: i64,
        /// `start`.
        start: Option<u64>,
        /// `count`.
        count: Option<u64>,
    },
    /// `evaluate`.
    Evaluate {
        /// `expression`.
        expression: BString,
        /// `frameId`.
        frame: Option<i64>,
    },
    /// `pause`.
    Pause,
    /// `continue`.
    Continue,
    /// `next`.
    Next,
    /// `stepIn`.
    StepIn,
    /// `stepOut`.
    StepOut,
    /// `launch`.
    Launch,
    /// `disconnect`.
    Disconnect,
}

/// A message that does not give a request the adapter can handle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestError {
    /// The body is not a JSON object request (ignored by the session).
    NotRequest,
    /// The request has no handler. The text is the session error.
    Unhandled(String),
    /// The arguments do not deserialize.
    Deserialize,
}

impl RequestError {
    /// Session error text, if the error ends the session.
    #[must_use]
    pub fn session_error(&self) -> Option<String> {
        match self {
            Self::NotRequest => None,
            Self::Unhandled(text) => Some(text.clone()),
            Self::Deserialize => Some(DESERIALIZE_ERROR.to_owned()),
        }
    }
}

/// Parse one message body.
///
/// # Errors
///
/// Return a [`RequestError`] if the body is not a request that the adapter handles.
pub fn parse_request(body: &[u8]) -> Result<Request, RequestError> {
    let options = ReadOptions {
        allow_comments: false,
        dup_keys_last_wins: true,
    };
    let value = rcmake_json::parse(body, &options)
        .map_err(|_| RequestError::Unhandled("Failed to parse JSON message".to_owned()))?;
    if value.get("type").and_then(Value::as_str) != Some("request") {
        return Err(RequestError::NotRequest);
    }
    let seq = value
        .get("seq")
        .and_then(Value::as_i64)
        .ok_or(RequestError::NotRequest)?;
    let command = value
        .get("command")
        .and_then(Value::as_str)
        .ok_or(RequestError::NotRequest)?
        .to_owned();
    let args = Args(value.get("arguments"));
    let kind = parse_kind(&command, &args)?;
    Ok(Request { seq, command, kind })
}

/// Arguments of a request, with typed field access.
struct Args<'a>(Option<&'a Value>);

impl Args<'_> {
    fn field(&self, name: &str) -> Option<&Value> {
        self.0?.get(name)
    }

    /// A required object, as `cppdap` requires.
    fn present(&self) -> Result<(), RequestError> {
        match self.0 {
            Some(Value::Object(_)) => Ok(()),
            _ => Err(RequestError::Deserialize),
        }
    }

    fn int(&self, name: &str) -> Result<i64, RequestError> {
        self.field(name)
            .and_then(Value::as_i64)
            .ok_or(RequestError::Deserialize)
    }

    fn opt_int(&self, name: &str) -> Result<Option<i64>, RequestError> {
        match self.field(name) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value.as_i64().map(Some).ok_or(RequestError::Deserialize),
        }
    }

    fn opt_uint(&self, name: &str) -> Result<Option<u64>, RequestError> {
        match self.field(name) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value.as_u64().map(Some).ok_or(RequestError::Deserialize),
        }
    }

    fn opt_bool(&self, name: &str) -> Result<bool, RequestError> {
        match self.field(name) {
            None | Some(Value::Null) => Ok(false),
            Some(value) => value.as_bool().ok_or(RequestError::Deserialize),
        }
    }

    fn string(&self, name: &str) -> Result<BString, RequestError> {
        self.field(name)
            .and_then(Value::as_bytes)
            .map(BString::from)
            .ok_or(RequestError::Deserialize)
    }
}

fn opt_bool(value: &Value, name: &str) -> Result<bool, RequestError> {
    match value.get(name) {
        None | Some(Value::Null) => Ok(false),
        Some(field) => field.as_bool().ok_or(RequestError::Deserialize),
    }
}

fn parse_kind(command: &str, args: &Args<'_>) -> Result<RequestKind, RequestError> {
    Ok(match command {
        "initialize" => {
            args.present()?;
            args.string("adapterID")?;
            RequestKind::Initialize {
                supports_variable_type: args.opt_bool("supportsVariableType")?,
            }
        }
        "setBreakpoints" => {
            let source = args.field("source").ok_or(RequestError::Deserialize)?;
            if !matches!(source, Value::Object(_)) {
                return Err(RequestError::Deserialize);
            }
            let path = match source.get("path") {
                None | Some(Value::Null) => BString::default(),
                Some(value) => value
                    .as_bytes()
                    .map(BString::from)
                    .ok_or(RequestError::Deserialize)?,
            };
            let lines = match args.field("breakpoints") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(items)) => items
                    .iter()
                    .map(|item| {
                        item.get("line")
                            .and_then(Value::as_i64)
                            .ok_or(RequestError::Deserialize)
                    })
                    .collect::<Result<_, _>>()?,
                Some(_) => return Err(RequestError::Deserialize),
            };
            RequestKind::SetBreakpoints { path, lines }
        }
        "setExceptionBreakpoints" => {
            let Some(Value::Array(items)) = args.field("filters") else {
                return Err(RequestError::Deserialize);
            };
            let filters = items
                .iter()
                .map(|item| {
                    item.as_bytes()
                        .map(BString::from)
                        .ok_or(RequestError::Deserialize)
                })
                .collect::<Result<_, _>>()?;
            RequestKind::SetExceptionBreakpoints { filters }
        }
        "exceptionInfo" => {
            args.int("threadId")?;
            RequestKind::ExceptionInfo
        }
        "configurationDone" => RequestKind::ConfigurationDone,
        "threads" => RequestKind::Threads,
        "stackTrace" => {
            let thread = args.int("threadId")?;
            let format = match args.field("format") {
                None | Some(Value::Null) => FrameFormat::default(),
                Some(format @ Value::Object(_)) => FrameFormat {
                    parameters: opt_bool(format, "parameters")?,
                    parameter_values: opt_bool(format, "parameterValues")?,
                    line: opt_bool(format, "line")?,
                },
                Some(_) => return Err(RequestError::Deserialize),
            };
            RequestKind::StackTrace { thread, format }
        }
        "scopes" => RequestKind::Scopes {
            frame: args.int("frameId")?,
        },
        "variables" => RequestKind::Variables {
            reference: args.int("variablesReference")?,
            start: args.opt_uint("start")?,
            count: args.opt_uint("count")?,
        },
        "evaluate" => RequestKind::Evaluate {
            expression: args.string("expression")?,
            frame: args.opt_int("frameId")?,
        },
        "pause" | "continue" | "next" | "stepIn" | "stepOut" => {
            args.int("threadId")?;
            match command {
                "pause" => RequestKind::Pause,
                "continue" => RequestKind::Continue,
                "next" => RequestKind::Next,
                "stepIn" => RequestKind::StepIn,
                _ => RequestKind::StepOut,
            }
        }
        "launch" => RequestKind::Launch,
        "disconnect" => RequestKind::Disconnect,
        other => {
            return Err(RequestError::Unhandled(format!(
                "No request handler registered for command '{other}'"
            )));
        }
    })
}

/// Build a successful response body message (without `seq`).
#[must_use]
pub fn response(request: &Request, body: Value) -> Value {
    Value::object_from([
        ("type", Value::string("response")),
        ("request_seq", Value::Int(request.seq)),
        ("success", Value::Bool(true)),
        ("command", Value::string(request.command.as_str())),
        ("body", body),
    ])
}

/// Build an error response message (without `seq`).
#[must_use]
pub fn error_response(request: &Request, message: &str) -> Value {
    Value::object_from([
        ("type", Value::string("response")),
        ("request_seq", Value::Int(request.seq)),
        ("success", Value::Bool(false)),
        ("command", Value::string(request.command.as_str())),
        ("message", Value::string(message)),
    ])
}

/// Build an event message (without `seq`).
#[must_use]
pub fn event(name: &str, body: Value) -> Value {
    Value::object_from([
        ("type", Value::string("event")),
        ("event", Value::string(name)),
        ("body", body),
    ])
}

/// Build an object from `(key, value)` pairs.
pub(crate) fn object<const N: usize>(entries: [(&str, Value); N]) -> Value {
    Value::object_from(entries)
}

/// The `source` object of a path: `{"name": path, "path": path}`.
pub(crate) fn source(path: &[u8]) -> Value {
    object([("name", Value::string(path)), ("path", Value::string(path))])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Request, RequestError> {
        parse_request(text.as_bytes())
    }

    fn kind(text: &str) -> RequestKind {
        parse(text).unwrap().kind
    }

    #[test]
    fn each_request_type_parses() {
        let cases: [(&str, RequestKind); 17] = [
            (
                r#"{"seq":1,"type":"request","command":"initialize","arguments":{"adapterID":"a","supportsVariableType":true}}"#,
                RequestKind::Initialize {
                    supports_variable_type: true,
                },
            ),
            (
                r#"{"seq":1,"type":"request","command":"setBreakpoints","arguments":{"source":{"path":"/a"},"breakpoints":[{"line":3},{"line":9}]}}"#,
                RequestKind::SetBreakpoints {
                    path: "/a".into(),
                    lines: vec![3, 9],
                },
            ),
            (
                r#"{"seq":1,"type":"request","command":"setExceptionBreakpoints","arguments":{"filters":["WARNING"]}}"#,
                RequestKind::SetExceptionBreakpoints {
                    filters: vec!["WARNING".into()],
                },
            ),
            (
                r#"{"seq":1,"type":"request","command":"exceptionInfo","arguments":{"threadId":1}}"#,
                RequestKind::ExceptionInfo,
            ),
            (
                r#"{"seq":1,"type":"request","command":"configurationDone"}"#,
                RequestKind::ConfigurationDone,
            ),
            (
                r#"{"seq":1,"type":"request","command":"threads"}"#,
                RequestKind::Threads,
            ),
            (
                r#"{"seq":1,"type":"request","command":"stackTrace","arguments":{"threadId":1,"format":{"parameters":true,"line":true}}}"#,
                RequestKind::StackTrace {
                    thread: 1,
                    format: FrameFormat {
                        parameters: true,
                        parameter_values: false,
                        line: true,
                    },
                },
            ),
            (
                r#"{"seq":1,"type":"request","command":"scopes","arguments":{"frameId":4}}"#,
                RequestKind::Scopes { frame: 4 },
            ),
            (
                r#"{"seq":1,"type":"request","command":"variables","arguments":{"variablesReference":2,"start":1,"count":5}}"#,
                RequestKind::Variables {
                    reference: 2,
                    start: Some(1),
                    count: Some(5),
                },
            ),
            (
                r#"{"seq":1,"type":"request","command":"evaluate","arguments":{"expression":"X","frameId":3}}"#,
                RequestKind::Evaluate {
                    expression: "X".into(),
                    frame: Some(3),
                },
            ),
            (
                r#"{"seq":1,"type":"request","command":"pause","arguments":{"threadId":1}}"#,
                RequestKind::Pause,
            ),
            (
                r#"{"seq":1,"type":"request","command":"continue","arguments":{"threadId":1}}"#,
                RequestKind::Continue,
            ),
            (
                r#"{"seq":1,"type":"request","command":"next","arguments":{"threadId":1}}"#,
                RequestKind::Next,
            ),
            (
                r#"{"seq":1,"type":"request","command":"stepIn","arguments":{"threadId":1}}"#,
                RequestKind::StepIn,
            ),
            (
                r#"{"seq":1,"type":"request","command":"stepOut","arguments":{"threadId":1}}"#,
                RequestKind::StepOut,
            ),
            (
                r#"{"seq":1,"type":"request","command":"launch","arguments":{}}"#,
                RequestKind::Launch,
            ),
            (
                r#"{"seq":1,"type":"request","command":"disconnect","arguments":{}}"#,
                RequestKind::Disconnect,
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(kind(text), expected, "{text}");
        }
    }

    #[test]
    fn missing_required_fields_do_not_deserialize() {
        for text in [
            r#"{"seq":1,"type":"request","command":"next"}"#,
            r#"{"seq":1,"type":"request","command":"stackTrace","arguments":{"threadId":"1"}}"#,
            r#"{"seq":1,"type":"request","command":"initialize","arguments":{}}"#,
            r#"{"seq":1,"type":"request","command":"setBreakpoints","arguments":{"breakpoints":[]}}"#,
            r#"{"seq":1,"type":"request","command":"setBreakpoints","arguments":{"source":{},"breakpoints":[{}]}}"#,
            r#"{"seq":1,"type":"request","command":"variables","arguments":{"variablesReference":1,"start":-1}}"#,
        ] {
            assert_eq!(parse(text), Err(RequestError::Deserialize), "{text}");
        }
    }

    #[test]
    fn unknown_commands_and_non_requests() {
        assert_eq!(
            parse(r#"{"seq":1,"type":"request","command":"bogus"}"#)
                .unwrap_err()
                .session_error()
                .as_deref(),
            Some("No request handler registered for command 'bogus'")
        );
        assert!(matches!(parse("{"), Err(RequestError::Unhandled(_))));
        for text in [
            "[]",
            r#"{"seq":1,"type":"event","command":"x"}"#,
            r#"{"seq":"1","type":"request","command":"threads"}"#,
            r#"{"seq":1,"type":"request"}"#,
        ] {
            assert_eq!(parse(text), Err(RequestError::NotRequest), "{text}");
        }
    }

    #[test]
    fn random_bodies_never_panic() {
        let mut state: u64 = 0x1234_5678_9abc_def1;
        let alphabet = br#"{}[]":,0123456789-.truefalsnl seqtypecommandargumentsrequest"#;
        for _ in 0..20_000 {
            let mut body = Vec::new();
            for _ in 0..(state % 64) {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                body.push(alphabet[(state % alphabet.len() as u64) as usize]);
            }
            let _ = parse_request(&body);
        }
    }
}
