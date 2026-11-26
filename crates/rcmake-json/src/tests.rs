use super::*;

fn read(input: &[u8]) -> Value {
    parse(input, &ReadOptions::default()).expect("valid JSON")
}

#[test]
fn reads_values_and_all_string_escapes() {
    let input = br#"["\"\\\/\b\f\n\r\t\u0000\u00e9\u6c34\uD83D\uDE03",{},[],null,true,false]"#;
    assert_eq!(
        read(input),
        Value::Array(vec![
            Value::string("\"\\/\x08\x0c\n\r\t\0é水😃"),
            Value::object(),
            Value::Array(vec![]),
            Value::Null,
            Value::Bool(true),
            Value::Bool(false),
        ])
    );
    assert_eq!(read(b"\"\xff\""), Value::string(b"\xff".to_vec()));
}

#[test]
fn reads_integer_boundaries_and_real_numbers() {
    for (input, expected) in [
        ("-9223372036854775808", Value::Int(i64::MIN)),
        ("9223372036854775807", Value::Int(i64::MAX)),
        ("9223372036854775808", Value::UInt(1 << 63)),
        ("18446744073709551615", Value::UInt(u64::MAX)),
        ("18446744073709551616", Value::Real(18446744073709551616.0)),
        ("-9223372036854775809", Value::Real(-9223372036854775808.0)),
        ("-0", Value::Int(0)),
        ("0.0", Value::Real(0.0)),
        ("1.0", Value::Real(1.0)),
        ("1e+2", Value::Real(100.0)),
        ("12.5E-3", Value::Real(0.0125)),
        ("1e500", Value::Real(f64::INFINITY)),
        ("-1e500", Value::Real(f64::NEG_INFINITY)),
    ] {
        assert_eq!(read(input.as_bytes()), expected, "{input}");
    }
    let Value::Real(zero) = read(b"-0.0") else {
        panic!("expected a real value");
    };
    assert!(zero.is_sign_negative());
}

#[test]
fn comments_and_duplicate_keys_follow_options() {
    let input = b"/* start */ {\n  \"k\" : 1, // next\r\n  \"\\u006b\": /* value */ 2\n} // end";
    assert_eq!(write_compact(&read(input)).as_slice(), br#"{"k":2}"#);
    assert_eq!(
        read(b"[/* before */1/* after */]"),
        Value::Array(vec![Value::Int(1)])
    );
    assert_eq!(read(b"{/* empty */}"), Value::object());
    assert_eq!(read(b"[/* empty */]"), Value::Array(vec![]));

    let unique = ReadOptions {
        dup_keys_last_wins: false,
        ..ReadOptions::default()
    };
    let err = parse(input, &unique).expect_err("duplicate key");
    assert_eq!(err.to_string(), "* Line 3, Column 3\n  Duplicate key: 'k'");
    assert!(parse(br#"{"k":1,"nested":{"k":2}}"#, &unique).is_ok());

    let no_comments = ReadOptions {
        allow_comments: false,
        ..ReadOptions::default()
    };
    for input in ["/* before */ 0", "[1, // inside\n2]", "true // after"] {
        assert!(parse(input.as_bytes(), &no_comments).is_err(), "{input}");
    }
    assert!(parse(br#"["//","/*"]"#, &no_comments).is_ok());

    let error = parse(b"{}\n /* unfinished", &ReadOptions::default()).unwrap_err();
    assert_eq!(
        error.to_string(),
        "* Line 2, Column 2\n  Unterminated block comment."
    );
}

#[test]
fn rejects_invalid_json() {
    for input in [
        "",
        " \t\n",
        "// comment only",
        "nul",
        "True",
        "truex",
        "{} null",
        "[1,]",
        "{\"a\":1,}",
        "{a:1}",
        "{'a':1}",
        "[,1]",
        "[",
        "{",
        "\"open",
        "\"a\tb\"",
        "\"a\0b\"",
        r#""\q""#,
        r#""\uxxxx""#,
        r#""\u123""#,
        r#""\ud800""#,
        r#""\ud800xxxxxx""#,
        r#""\udc00""#,
        r#""\ud800\u0041""#,
        "01",
        "-01",
        "-",
        "+1",
        "1.",
        "1e",
        "1e+",
        "1e-",
        "NaN",
        "Infinity",
        "\u{feff}\u{feff}null",
    ] {
        assert!(
            parse(input.as_bytes(), &ReadOptions::default()).is_err(),
            "{input:?}"
        );
    }
}

#[test]
fn reports_byte_columns_and_counts_all_line_endings() {
    assert_eq!(read(b"\xef\xbb\xbf{}"), Value::object());
    for (input, expected) in [
        (
            "\u{feff}[\r\n \"é\", ?]",
            "* Line 2, Column 8\n  Syntax error: value, object or array expected.",
        ),
        (
            "{\r \"a\" 1}",
            "* Line 2, Column 6\n  Missing ':' after object member name",
        ),
        (
            "{a:1}",
            "* Line 1, Column 2\n  Missing '}' or object member name",
        ),
        (
            "[1 2]",
            "* Line 1, Column 4\n  Missing ',' or ']' in array declaration",
        ),
        (
            "[1\n ",
            "* Line 2, Column 2\n  Missing ',' or ']' in array declaration",
        ),
        (
            "{\"a\":1",
            "* Line 1, Column 7\n  Missing ',' or '}' in object declaration",
        ),
        (
            "[0,\n  \"\\q\"]",
            "* Line 2, Column 3\n  Bad escape sequence in string",
        ),
        ("[1e+]", "* Line 1, Column 2\n  '1e+' is not a number."),
        (
            "[]\n  null",
            "* Line 2, Column 3\n  Extra non-whitespace after JSON value.",
        ),
    ] {
        let error = parse(input.as_bytes(), &ReadOptions::default()).unwrap_err();
        assert_eq!(error.to_string(), expected, "{input:?}");
    }
}

#[test]
fn bounds_nesting_for_empty_and_nonempty_containers() {
    let input = format!("{}0{}", "[".repeat(1000), "]".repeat(1000));
    assert_eq!(
        write_compact(&read(input.as_bytes())).as_slice(),
        input.as_bytes()
    );

    let input = "[".repeat(100_000);
    let error = parse(input.as_bytes(), &ReadOptions::default()).unwrap_err();
    assert_eq!(error.column, 1001);

    let input = format!("{}{{}}{}", r#"{"x":"#.repeat(1000), "}".repeat(1000));
    let error = parse(input.as_bytes(), &ReadOptions::default()).unwrap_err();
    assert_eq!(error.column, 5001);
}

#[test]
fn styled_matches_cmake_4_5_1_string_json_set() {
    let input = br#"{"z":null,"b":{"z":[],"a":["\u00e9\ud83d\ude03\/\\\"\b\f\n\r\t\u0000\u001f",{},[]]},"a":[1.0,-0.0,0.1,1e-5,1e17,9223372036854775808,1e500]}"#;
    let Value::Object(mut members) = read(input) else {
        panic!("expected an object");
    };
    members.insert(BString::from("added"), Value::Bool(false));
    let value = Value::Object(members);

    // CMake 4.5.1 output from string(JSON out SET <input> added false).
    let expected = [
        "{",
        "  \"a\" : ",
        "  [",
        "    1.0,",
        "    -0.0,",
        "    0.10000000000000001,",
        "    1.0000000000000001e-05,",
        "    1e+17,",
        "    9223372036854775808,",
        "    1e+9999",
        "  ],",
        "  \"added\" : false,",
        "  \"b\" : ",
        "  {",
        r#"    "a" : [ "\u00e9\ud83d\ude03/\\\"\b\f\n\r\t\u0000\u001f", {}, [] ],"#,
        "    \"z\" : []",
        "  },",
        "  \"z\" : null",
        "}",
    ]
    .join("\n");
    let styled = write_styled(&value, "  ");
    assert_eq!(styled.as_slice(), expected.as_bytes());
    assert_eq!(read(styled.as_slice()), value);
    assert_eq!(write_styled(&read(expected.as_bytes()), "  "), styled);
}

#[test]
fn styled_array_layout_matches_cmake_4_5_1_at_margin() {
    let short = "a".repeat(67);
    let long = "a".repeat(68);
    let input = format!(r#"{{"a":["{short}"],"b":["{long}"],"c":[[1]],"d":[[],{{}}]}}"#);
    let expected = format!(
        "{{\n  \"a\" : [ \"{short}\" ],\n  \"b\" : \n  [\n    \"{long}\"\n  ],\n  \
         \"c\" : \n  [\n    [ 1 ]\n  ],\n  \"d\" : [ [], {{}} ]\n}}"
    );
    assert_eq!(
        write_styled(&read(input.as_bytes()), "  ").as_slice(),
        expected.as_bytes()
    );
    let input = br#"{"z":{"a":1}}"#;
    assert_eq!(
        write_styled(&read(input), "\t").as_slice(),
        b"{\n\t\"z\" : \n\t{\n\t\t\"a\" : 1\n\t}\n}"
    );
}

#[test]
fn compact_matches_cmake_4_5_1_capabilities_version_object() {
    let input = br#"{"suffix":"","string":"4.5.1","patch":1,"minor":5,"major":4,"isDirty":false}"#;
    let expected =
        br#"{"isDirty":false,"major":4,"minor":5,"patch":1,"string":"4.5.1","suffix":""}"#;
    let value = read(input);
    let compact = write_compact(&value);
    assert_eq!(compact.as_slice(), expected);
    assert_eq!(write_styled(&value, ""), compact);
    assert_eq!(read(compact.as_slice()), value);
}

#[test]
fn writes_jsoncpp_string_and_real_representations() {
    let input = b"{\"z\":\"\xc2A \xe2\x82\",\"a\":\"\x7f/\xc3\xa9\xf0\x9f\x98\x83\"}";
    let expected = b"{\"a\":\"\x7f/\\u00e9\\ud83d\\ude03\",\"z\":\"\\u0081 \\ufffd\\ufffd\"}";
    assert_eq!(write_compact(&read(input)).as_slice(), expected);
    let numbers = Value::Array(vec![
        Value::Real(-0.0),
        Value::Real(1e-4),
        Value::Real(1e16),
        Value::Real(1e-300),
        Value::Real(f64::NAN),
        Value::Real(f64::INFINITY),
        Value::Real(f64::NEG_INFINITY),
    ]);
    assert_eq!(
        write_compact(&numbers).as_slice(),
        b"[-0.0,0.0001,10000000000000000.0,1e-300,null,1e+9999,-1e+9999]"
    );
}

#[test]
fn constructs_sorted_objects_and_ordered_arrays() {
    let array = Value::array_from([Value::Bool(true), Value::Int(-1)]);
    let object = Value::object_from([
        ("z", Value::Null),
        ("a", array.clone()),
        ("z", Value::string("last")),
    ]);
    assert_eq!(object.as_object().map(|entries| entries.len()), Some(2));
    assert_eq!(object.get("a"), Some(&array));
    assert_eq!(object.get(b"z").and_then(Value::as_str), Some("last"));
    assert_eq!(object.get("missing"), None);
    assert_eq!(
        array.as_array(),
        Some([Value::Bool(true), Value::Int(-1)].as_slice())
    );
    assert_eq!(array.as_object(), None);
    assert_eq!(array.get("a"), None);
    assert_eq!(object.as_array(), None);
    assert_eq!(
        Value::object_from([(BString::from(b"\xff".as_slice()), Value::Null)]).get(b"\xff"),
        Some(&Value::Null)
    );
}

#[test]
fn scalar_accessors_require_matching_types() {
    assert_eq!(Value::string("é").as_str(), Some("é"));
    assert_eq!(Value::string(b"\xff".as_slice()).as_str(), None);
    assert_eq!(
        Value::string(b"\xff".as_slice()).as_bytes(),
        Some(b"\xff".as_slice())
    );
    assert_eq!(Value::Null.as_bytes(), None);
    assert_eq!(Value::Int(i64::MIN).as_i64(), Some(i64::MIN));
    assert_eq!(Value::UInt(i64::MAX as u64).as_i64(), Some(i64::MAX));
    assert_eq!(Value::UInt(i64::MAX as u64 + 1).as_i64(), None);
    assert_eq!(Value::Real(1.0).as_i64(), None);
    assert_eq!(Value::Bool(true).as_i64(), None);
    assert_eq!(Value::Bool(true).as_bool(), Some(true));
    assert_eq!(Value::Bool(false).as_bool(), Some(false));
    assert_eq!(Value::Int(1).as_bool(), None);
}

#[test]
fn appends_string_with_jsoncpp_escapes() {
    let mut out = b"prefix ".to_vec();
    crate::write_string(&mut out, b"\"\\\0\n\x7f\xc3\xa9");
    assert_eq!(out, b"prefix \"\\\"\\\\\\u0000\\n\x7f\\u00e9\"");
}

#[test]
fn pretty_utf8_matches_serde_json_lockfile_golden() {
    let empty_lock =
        Value::object_from([("version", Value::UInt(1)), ("entries", Value::object())]);
    let expected_empty = "{\n  \"entries\": {},\n  \"version\": 1\n}";
    assert_eq!(
        write_pretty_utf8(&empty_lock).as_slice(),
        expected_empty.as_bytes()
    );

    let lock = Value::object_from([
        ("version", Value::UInt(1)),
        (
            "entries",
            Value::object_from([
                (
                    "fmt",
                    Value::object_from([
                        ("kind", Value::string("url")),
                        (
                            "urls",
                            Value::array_from([
                                Value::string("https://example.invalid/café/fmt-11.tar.gz"),
                                Value::string(r"C:\SDK\fmt-11.tar.gz"),
                            ]),
                        ),
                        (
                            "sha256",
                            Value::string(
                                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                            ),
                        ),
                        ("size", Value::UInt(4096)),
                    ]),
                ),
                (
                    "alpha",
                    Value::object_from([
                        ("kind", Value::string("git")),
                        (
                            "repository",
                            Value::string("https://example.invalid/repo/水.git"),
                        ),
                        ("tag", Value::string("v1.0.0")),
                        (
                            "commit",
                            Value::string("40626af88bd7df9a5fb80be7b25ac85b122d6c21"),
                        ),
                    ]),
                ),
                (
                    "pinned",
                    Value::object_from([
                        ("kind", Value::string("git")),
                        (
                            "repository",
                            Value::string("https://example.invalid/pinned.git"),
                        ),
                        (
                            "commit",
                            Value::string("0123456789abcdef0123456789abcdef01234567"),
                        ),
                    ]),
                ),
            ]),
        ),
    ]);

    let expected = [
        "{",
        "  \"entries\": {",
        "    \"alpha\": {",
        "      \"commit\": \"40626af88bd7df9a5fb80be7b25ac85b122d6c21\",",
        "      \"kind\": \"git\",",
        "      \"repository\": \"https://example.invalid/repo/水.git\",",
        "      \"tag\": \"v1.0.0\"",
        "    },",
        "    \"fmt\": {",
        "      \"kind\": \"url\",",
        "      \"sha256\": \"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\",",
        "      \"size\": 4096,",
        "      \"urls\": [",
        "        \"https://example.invalid/café/fmt-11.tar.gz\",",
        "        \"C:\\\\SDK\\\\fmt-11.tar.gz\"",
        "      ]",
        "    },",
        "    \"pinned\": {",
        "      \"commit\": \"0123456789abcdef0123456789abcdef01234567\",",
        "      \"kind\": \"git\",",
        "      \"repository\": \"https://example.invalid/pinned.git\"",
        "    }",
        "  },",
        "  \"version\": 1",
        "}",
    ]
    .join("\n");

    let pretty = write_pretty_utf8(&lock);
    assert_eq!(pretty.as_slice(), expected.as_bytes());

    let strict = ReadOptions {
        allow_comments: false,
        dup_keys_last_wins: true,
    };
    let parsed = parse(pretty.as_slice(), &strict).expect("valid strict JSON");
    assert_eq!(write_pretty_utf8(&parsed), pretty);
}

#[test]
fn pretty_utf8_escapes_control_chars_and_preserves_utf8() {
    for byte in 0x00_u8..=0x1f {
        let expected = match byte {
            0x08 => r#""\b""#.to_owned(),
            0x09 => r#""\t""#.to_owned(),
            0x0a => r#""\n""#.to_owned(),
            0x0c => r#""\f""#.to_owned(),
            0x0d => r#""\r""#.to_owned(),
            _ => format!(r#""\u{byte:04x}""#),
        };
        assert_eq!(
            write_pretty_utf8(&Value::string([byte].as_slice())).as_slice(),
            expected.as_bytes(),
            "control byte 0x{byte:02x}"
        );
    }

    let value = Value::object_from([
        (
            "clé",
            Value::string("\"\\/\x08\x0c\n\r\t\x00\x0b\x1f\x7fé水😃"),
        ),
        ("raw", Value::string(b"a\xffb".as_slice())),
    ]);
    let expected = "{\n  \"clé\": \"\\\"\\\\/\\b\\f\\n\\r\\t\\u0000\\u000b\\u001f\x7fé水😃\",\n  \"raw\": \"a\u{fffd}b\"\n}";
    assert_eq!(write_pretty_utf8(&value).as_slice(), expected.as_bytes());
}

#[test]
fn pretty_utf8_formats_containers_and_numbers() {
    assert_eq!(write_pretty_utf8(&Value::Null).as_slice(), b"null");
    assert_eq!(write_pretty_utf8(&Value::Bool(true)).as_slice(), b"true");
    assert_eq!(write_pretty_utf8(&Value::Bool(false)).as_slice(), b"false");
    assert_eq!(
        write_pretty_utf8(&Value::Int(i64::MIN)).as_slice(),
        b"-9223372036854775808"
    );
    assert_eq!(
        write_pretty_utf8(&Value::UInt(u64::MAX)).as_slice(),
        b"18446744073709551615"
    );
    assert_eq!(write_pretty_utf8(&Value::Array(vec![])).as_slice(), b"[]");
    assert_eq!(write_pretty_utf8(&Value::object()).as_slice(), b"{}");

    let nested = Value::array_from([
        Value::array_from([Value::Int(1), Value::Int(2)]),
        Value::Array(vec![]),
        Value::array_from([Value::object()]),
        Value::array_from([Value::object_from([
            ("empty_arr", Value::Array(vec![])),
            ("empty_obj", Value::object()),
            ("num", Value::Int(1)),
        ])]),
    ]);
    let expected_nested = [
        "[",
        "  [",
        "    1,",
        "    2",
        "  ],",
        "  [],",
        "  [",
        "    {}",
        "  ],",
        "  [",
        "    {",
        "      \"empty_arr\": [],",
        "      \"empty_obj\": {},",
        "      \"num\": 1",
        "    }",
        "  ]",
        "]",
    ]
    .join("\n");
    assert_eq!(
        write_pretty_utf8(&nested).as_slice(),
        expected_nested.as_bytes()
    );

    let reals = Value::array_from([
        Value::Real(0.0),
        Value::Real(-0.0),
        Value::Real(1.0),
        Value::Real(0.1),
        Value::Real(12.5),
        Value::Real(100.0),
        Value::Real(1e-5),
        Value::Real(1e-6),
        Value::Real(1e15),
        Value::Real(1e16),
        Value::Real(f64::NAN),
        Value::Real(f64::INFINITY),
        Value::Real(f64::NEG_INFINITY),
    ]);
    let expected_reals = [
        "[",
        "  0.0,",
        "  -0.0,",
        "  1.0,",
        "  0.1,",
        "  12.5,",
        "  100.0,",
        "  0.00001,",
        "  1e-6,",
        "  1000000000000000.0,",
        "  1e16,",
        "  null,",
        "  null,",
        "  null",
        "]",
    ]
    .join("\n");
    assert_eq!(
        write_pretty_utf8(&reals).as_slice(),
        expected_reals.as_bytes()
    );
}
