//! JSON-RPC framing and input validation tests.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::io::{BufReader, Cursor, ErrorKind};

use rcmake_json::Value;
use rcmake_lsp::{
    RequestId, RpcMessage,
    rpc::{MAX_MESSAGE_BYTES, read_frame, write_frame},
};

#[test]
fn frames_round_trip_with_small_reads_and_clean_eof() {
    let first = RpcMessage::notification("initialized", Value::object());
    let second = RpcMessage::Result {
        id: RequestId::String("résultat".into()),
        result: Value::Null,
    };
    let mut output = Vec::new();
    write_frame(&mut output, &first.encode()).unwrap();
    write_frame(&mut output, &second.encode()).unwrap();
    let mut input = BufReader::with_capacity(3, Cursor::new(output));
    assert_eq!(
        RpcMessage::decode(&read_frame(&mut input).unwrap().unwrap()).unwrap(),
        first
    );
    assert_eq!(
        RpcMessage::decode(&read_frame(&mut input).unwrap().unwrap()).unwrap(),
        second
    );
    assert_eq!(read_frame(&mut input).unwrap(), None);
}

#[test]
fn frame_rejects_invalid_duplicate_and_huge_lengths() {
    for header in [
        "Content-Length: 16777217\r\n\r\n",
        "Content-Length: 999999999999999999999999999999999\r\n\r\n",
        "Content-Length: -1\r\n\r\n",
        "Content-Length: 1\r\ncontent-length: 1\r\n\r\n",
        "Content-Length 3\r\n\r\n",
        "Content-Length: 0\r\nContent-Type: application/vscode-jsonrpc; charset=latin1\r\n\r\n",
        "Content-Type: application/vscode-jsonrpc\r\n\r\n",
    ] {
        assert_eq!(
            read_frame(&mut Cursor::new(header)).unwrap_err().kind(),
            ErrorKind::InvalidData
        );
    }
    assert_eq!(
        read_frame(&mut Cursor::new(vec![b'x'; 8193]))
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidData
    );
    assert_eq!(
        read_frame(&mut Cursor::new("Content-Length: 2\r\n\r\na"))
            .unwrap_err()
            .kind(),
        ErrorKind::UnexpectedEof
    );
    assert_eq!(
        read_frame(&mut Cursor::new("Content-Leng"))
            .unwrap_err()
            .kind(),
        ErrorKind::UnexpectedEof
    );
    assert_eq!(
        write_frame(&mut Vec::new(), &vec![0; MAX_MESSAGE_BYTES + 1])
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidData
    );
}

#[test]
fn decode_rejects_malformed_json_utf8_and_envelopes() {
    for body in [
        b"\xff".as_slice(),
        b"{",
        br#"{"jsonrpc":"2.0","method":"a","method":"b"}"#,
        br#"{"jsonrpc":"2.0",/*comment*/"method":"a"}"#,
    ] {
        assert!(matches!(
            RpcMessage::decode(body),
            Err(RpcMessage::Error { code: -32700, .. })
        ));
    }
    for body in [
        br#"{}"#.as_slice(),
        br#"{"jsonrpc":"1.0","method":"a"}"#,
        br#"{"jsonrpc":"2.0","id":true,"method":"a"}"#,
        br#"{"jsonrpc":"2.0","id":1,"method":"a","result":null}"#,
        br#"{"jsonrpc":"2.0","method":"a","params":"invalid"}"#,
        br#"{"jsonrpc":"2.0","result":null}"#,
    ] {
        assert!(matches!(
            RpcMessage::decode(body),
            Err(RpcMessage::Error { code: -32600, .. })
        ));
    }
}
