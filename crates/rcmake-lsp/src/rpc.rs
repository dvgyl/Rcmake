//! JSON-RPC messages and bounded Content-Length frames.

use std::io::{self, BufRead, Write};

use rcmake_json::{ReadOptions, Value, parse, write_compact};

use crate::json::{get, object, string};

/// Maximum body size in bytes.
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

const MAX_HEADER_BYTES: usize = 8192;

/// A JSON-RPC request identifier.
#[derive(Clone, Debug, PartialEq)]
pub enum RequestId {
    /// A string identifier.
    String(String),
    /// A signed integer identifier.
    Int(i64),
    /// An unsigned integer identifier.
    UInt(u64),
    /// Unknown identifier, used in error responses.
    Null,
}

impl RequestId {
    fn parse(value: &Value) -> Option<Self> {
        Some(match value {
            Value::String(value) => Self::String(std::str::from_utf8(value).ok()?.into()),
            Value::Int(value) => Self::Int(*value),
            Value::UInt(value) => Self::UInt(*value),
            Value::Null => Self::Null,
            _ => return None,
        })
    }

    fn value(&self) -> Value {
        match self {
            Self::String(value) => Value::string(value.as_str()),
            Self::Int(value) => Value::Int(*value),
            Self::UInt(value) => Value::UInt(*value),
            Self::Null => Value::Null,
        }
    }
}

/// A single JSON-RPC 2.0 message.
#[derive(Clone, Debug, PartialEq)]
pub enum RpcMessage {
    /// A method call. Calls without an ID are notifications.
    Call {
        /// Request identifier, when present.
        id: Option<RequestId>,
        /// Requested method.
        method: String,
        /// Optional request parameters. Use null for absent parameters.
        params: Value,
    },
    /// A successful response.
    Result {
        /// Identifier from the request.
        id: RequestId,
        /// Method result.
        result: Value,
    },
    /// An error response.
    Error {
        /// Identifier from the request, or null on a parse error.
        id: RequestId,
        /// JSON-RPC or Language Server Protocol error code.
        code: i64,
        /// Error message.
        message: String,
        /// Optional error data. Use null when absent.
        data: Value,
    },
}

impl RpcMessage {
    /// Make a request with an identifier.
    pub fn request(id: RequestId, method: impl Into<String>, params: Value) -> Self {
        Self::Call {
            id: Some(id),
            method: method.into(),
            params,
        }
    }

    /// Make a notification without an identifier.
    pub fn notification(method: impl Into<String>, params: Value) -> Self {
        Self::Call {
            id: None,
            method: method.into(),
            params,
        }
    }

    /// Make an error response without extra data.
    pub fn error(id: RequestId, code: i64, message: impl Into<String>) -> Self {
        Self::Error {
            id,
            code,
            message: message.into(),
            data: Value::Null,
        }
    }

    /// Decode one UTF-8 JSON-RPC message. On failure, return a protocol error response.
    pub fn decode(body: &[u8]) -> Result<Self, Self> {
        if body.len() > MAX_MESSAGE_BYTES || std::str::from_utf8(body).is_err() {
            return Err(Self::error(RequestId::Null, -32700, "Invalid JSON body."));
        }
        let value = parse(
            body,
            &ReadOptions {
                allow_comments: false,
                dup_keys_last_wins: false,
            },
        )
        .map_err(|_| Self::error(RequestId::Null, -32700, "Invalid JSON body."))?;
        Self::from_json(&value)
            .ok_or_else(|| Self::error(RequestId::Null, -32600, "Invalid JSON-RPC message."))
    }

    fn from_json(value: &Value) -> Option<Self> {
        if string(get(value, "jsonrpc")?)? != "2.0" {
            return None;
        }
        let id = match get(value, "id") {
            Some(id) => Some(RequestId::parse(id)?),
            None => None,
        };
        let result = get(value, "result");
        let error = get(value, "error");
        match (get(value, "method"), result, error) {
            (Some(method), None, None) => {
                let params = get(value, "params").unwrap_or(&Value::Null);
                if !matches!(params, Value::Null | Value::Array(_) | Value::Object(_)) {
                    return None;
                }
                Some(Self::Call {
                    id,
                    method: string(method)?.into(),
                    params: params.clone(),
                })
            }
            (None, Some(result), None) => Some(Self::Result {
                id: id?,
                result: result.clone(),
            }),
            (None, None, Some(error)) => {
                let code = match get(error, "code")? {
                    Value::Int(code) => *code,
                    Value::UInt(code) => i64::try_from(*code).ok()?,
                    _ => return None,
                };
                Some(Self::Error {
                    id: id?,
                    code,
                    message: string(get(error, "message")?)?.into(),
                    data: get(error, "data").cloned().unwrap_or(Value::Null),
                })
            }
            _ => None,
        }
    }

    /// Encode this message as compact UTF-8 JSON without a frame header.
    pub fn encode(&self) -> Vec<u8> {
        let mut fields = vec![("jsonrpc", Value::string("2.0"))];
        match self {
            Self::Call { id, method, params } => {
                fields.push(("method", Value::string(method.as_str())));
                if let Some(id) = id {
                    fields.push(("id", id.value()));
                }
                if *params != Value::Null {
                    fields.push(("params", params.clone()));
                }
            }
            Self::Result { id, result } => {
                fields.extend([("id", id.value()), ("result", result.clone())]);
            }
            Self::Error {
                id,
                code,
                message,
                data,
            } => {
                let mut error = vec![
                    ("code", Value::Int(*code)),
                    ("message", Value::string(message.as_str())),
                ];
                if *data != Value::Null {
                    error.push(("data", data.clone()));
                }
                fields.extend([("id", id.value()), ("error", object(error))]);
            }
        }
        let bytes: Vec<u8> = write_compact(&object(fields)).into();
        match String::from_utf8_lossy(&bytes) {
            std::borrow::Cow::Borrowed(_) => bytes,
            std::borrow::Cow::Owned(text) => text.into_bytes(),
        }
    }
}

/// Read one frame body. Return `None` on EOF before the next header.
///
/// A framing error is fatal to the stream because the body boundary is unknown.
pub fn read_frame(input: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut headers = Vec::new();
    loop {
        let bytes = input.fill_buf()?;
        if bytes.is_empty() {
            return if headers.is_empty() {
                Ok(None)
            } else {
                Err(io::ErrorKind::UnexpectedEof.into())
            };
        }
        let remaining = MAX_HEADER_BYTES.saturating_sub(headers.len());
        let chunk = bytes.get(..bytes.len().min(remaining)).unwrap_or_default();
        let count = chunk
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(chunk.len(), |pos| pos + 1);
        if count == 0 {
            return Err(frame_error("Frame header exceeds the size limit."));
        }
        headers.extend_from_slice(chunk.get(..count).unwrap_or_default());
        input.consume(count);
        if headers.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let length = content_length(&headers)?;
    let mut body = vec![0; length];
    input.read_exact(&mut body)?;
    Ok(Some(body))
}

/// Write one UTF-8 message body with its Content-Length header.
pub fn write_frame(output: &mut impl Write, body: &[u8]) -> io::Result<()> {
    if body.len() > MAX_MESSAGE_BYTES {
        return Err(frame_error("Message body exceeds the size limit."));
    }
    write!(output, "Content-Length: {}\r\n\r\n", body.len())?;
    output.write_all(body)
}

fn content_length(headers: &[u8]) -> io::Result<usize> {
    if !headers.is_ascii() {
        return Err(frame_error("Frame headers must use ASCII."));
    }
    let headers = std::str::from_utf8(headers).map_err(|_| frame_error("Invalid frame header."))?;
    let mut length = None;
    let mut has_content_type = false;
    for header in headers.trim_end_matches("\r\n").split("\r\n") {
        let (name, value) = header
            .split_once(':')
            .ok_or_else(|| frame_error("Invalid frame header."))?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || value
                .bytes()
                .any(|byte| byte.is_ascii_control() && byte != b'\t')
        {
            return Err(frame_error("Invalid frame header."));
        }
        let value = value.trim_ascii();
        if name.eq_ignore_ascii_case("Content-Length") {
            if length.is_some()
                || value.is_empty()
                || !value.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(frame_error("Invalid Content-Length."));
            }
            length = Some(
                value
                    .parse::<usize>()
                    .ok()
                    .filter(|&len| len <= MAX_MESSAGE_BYTES)
                    .ok_or_else(|| frame_error("Message body exceeds the size limit."))?,
            );
        } else if name.eq_ignore_ascii_case("Content-Type") {
            if has_content_type {
                return Err(frame_error("Duplicate Content-Type."));
            }
            has_content_type = true;
            let charset = value
                .split(';')
                .skip(1)
                .filter_map(|parameter| parameter.split_once('='))
                .find(|(key, _)| key.trim_ascii().eq_ignore_ascii_case("charset"));
            if let Some((_, charset)) = charset {
                let charset = charset.trim_ascii().trim_matches('"');
                if !charset.eq_ignore_ascii_case("utf-8") && !charset.eq_ignore_ascii_case("utf8") {
                    return Err(frame_error("Unsupported message character encoding."));
                }
            }
        }
    }
    length.ok_or_else(|| frame_error("Missing Content-Length."))
}

fn frame_error(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
