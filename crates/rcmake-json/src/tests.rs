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

fn pretty_utf8(value: &Value) -> BString {
    write_pretty_utf8(value).expect("valid UTF-8 object keys")
}

#[test]
fn pretty_utf8_matches_serde_json_lockfile_golden() {
    let empty_lock =
        Value::object_from([("version", Value::UInt(1)), ("entries", Value::object())]);
    let expected_empty = "{\n  \"entries\": {},\n  \"version\": 1\n}";
    assert_eq!(
        pretty_utf8(&empty_lock).as_slice(),
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

    let pretty = pretty_utf8(&lock);
    assert_eq!(pretty.as_slice(), expected.as_bytes());

    let strict = ReadOptions {
        allow_comments: false,
        dup_keys_last_wins: true,
    };
    let parsed = parse(pretty.as_slice(), &strict).expect("valid strict JSON");
    assert_eq!(pretty_utf8(&parsed), pretty);
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
            pretty_utf8(&Value::string([byte].as_slice())).as_slice(),
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
    assert_eq!(pretty_utf8(&value).as_slice(), expected.as_bytes());
}

#[test]
fn pretty_utf8_formats_containers_and_numbers() {
    assert_eq!(pretty_utf8(&Value::Null).as_slice(), b"null");
    assert_eq!(pretty_utf8(&Value::Bool(true)).as_slice(), b"true");
    assert_eq!(pretty_utf8(&Value::Bool(false)).as_slice(), b"false");
    assert_eq!(
        pretty_utf8(&Value::Int(i64::MIN)).as_slice(),
        b"-9223372036854775808"
    );
    assert_eq!(
        pretty_utf8(&Value::UInt(u64::MAX)).as_slice(),
        b"18446744073709551615"
    );
    assert_eq!(pretty_utf8(&Value::Array(vec![])).as_slice(), b"[]");
    assert_eq!(pretty_utf8(&Value::object()).as_slice(), b"{}");

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
    assert_eq!(pretty_utf8(&nested).as_slice(), expected_nested.as_bytes());

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
        "  1e+16,",
        "  null,",
        "  null,",
        "  null",
        "]",
    ]
    .join("\n");
    assert_eq!(pretty_utf8(&reals).as_slice(), expected_reals.as_bytes());
}

#[test]
fn pretty_utf8_formats_real_exponent_boundaries() {
    let strict = ReadOptions {
        allow_comments: false,
        dup_keys_last_wins: false,
    };
    for (number, expected) in [
        (0.0_f64, "0.0"),
        (-0.0, "-0.0"),
        (1.0, "1.0"),
        (-1.0, "-1.0"),
        (1e-4, "0.0001"),
        (1e-5, "0.00001"),
        (-1e-5, "-0.00001"),
        (1e-6, "1e-6"),
        (1.5e-6, "1.5e-6"),
        (1e15, "1000000000000000.0"),
        (1e16, "1e+16"),
        (-1e16, "-1e+16"),
        (1.5e20, "1.5e+20"),
    ] {
        let rendered = pretty_utf8(&Value::Real(number));
        assert_eq!(rendered.as_slice(), expected.as_bytes(), "{number:?}");
        let Value::Real(roundtrip) =
            parse(rendered.as_slice(), &strict).expect("valid JSON number")
        else {
            panic!("expected real value for {number:?}");
        };
        assert_eq!(roundtrip.to_bits(), number.to_bits(), "{number:?}");
    }

    let halfway = 112_171_935_477_118.0_f64 + 0.125;
    let rendered = pretty_utf8(&Value::Real(halfway));
    let Value::Real(roundtrip) =
        parse(rendered.as_slice(), &strict).expect("valid JSON halfway float")
    else {
        panic!("expected real value for halfway float");
    };
    assert_eq!(roundtrip.to_bits(), halfway.to_bits());
}

#[test]
fn pretty_utf8_rejects_invalid_utf8_object_keys() {
    let distinct_invalid = Value::object_from([
        (BString::from(b"\xfe".as_slice()), Value::Int(1)),
        (BString::from(b"\xff".as_slice()), Value::Int(2)),
    ]);
    assert_eq!(
        write_pretty_utf8(&distinct_invalid),
        Err(WriteError::InvalidUtf8Key)
    );

    let collides_with_replacement = Value::object_from([
        (BString::from("\u{fffd}"), Value::Int(1)),
        (BString::from(b"\xff".as_slice()), Value::Int(2)),
    ]);
    assert_eq!(
        write_pretty_utf8(&collides_with_replacement),
        Err(WriteError::InvalidUtf8Key)
    );

    let nested_invalid = Value::object_from([(
        "items",
        Value::array_from([Value::object_from([(
            BString::from(b"bad\x80key".as_slice()),
            Value::Bool(true),
        )])]),
    )]);
    assert_eq!(
        write_pretty_utf8(&nested_invalid),
        Err(WriteError::InvalidUtf8Key)
    );
    assert_eq!(
        WriteError::InvalidUtf8Key.to_string(),
        "JSON object key is not valid UTF-8"
    );
}

#[test]
fn value_as_u64_and_type_names() {
    assert_eq!(Value::Int(0).as_u64(), Some(0));
    assert_eq!(Value::Int(i64::MAX).as_u64(), Some(i64::MAX as u64));
    assert_eq!(Value::Int(-1).as_u64(), None);
    assert_eq!(Value::Int(i64::MIN).as_u64(), None);
    assert_eq!(Value::UInt(u64::MAX).as_u64(), Some(u64::MAX));
    assert_eq!(Value::Real(1.0).as_u64(), None);
    assert_eq!(Value::Null.type_name(), "null");
    assert_eq!(Value::Bool(true).type_name(), "boolean");
    assert_eq!(Value::Int(1).type_name(), "integer");
    assert_eq!(Value::UInt(1).type_name(), "integer");
    assert_eq!(Value::Real(1.0).type_name(), "number");
    assert_eq!(Value::string("x").type_name(), "string");
    assert_eq!(Value::Array(vec![]).type_name(), "array");
    assert_eq!(Value::object().type_name(), "object");
}

#[test]
fn extractors_cover_valid_range_and_type_errors() {
    assert_eq!(codec::u64(&Value::Int(0)), Ok(0));
    assert_eq!(codec::u64(&Value::UInt(u64::MAX)), Ok(u64::MAX));
    assert_eq!(codec::u64(&Value::Int(-1)), Err(FieldError::range("u64")));
    assert_eq!(
        codec::u64(&Value::string("1")),
        Err(FieldError::expected("u64", &Value::string("1")))
    );

    assert_eq!(codec::i64(&Value::Int(i64::MIN)), Ok(i64::MIN));
    assert_eq!(codec::i64(&Value::UInt(i64::MAX as u64)), Ok(i64::MAX));
    assert_eq!(
        codec::i64(&Value::UInt(i64::MAX as u64 + 1)),
        Err(FieldError::range("i64"))
    );
    assert_eq!(
        codec::i64(&Value::Bool(false)),
        Err(FieldError::expected("i64", &Value::Bool(false)))
    );

    assert_eq!(codec::usize(&Value::Int(42)), Ok(42));
    assert_eq!(codec::usize(&Value::UInt(7)), Ok(7));
    assert_eq!(
        codec::usize(&Value::Int(-1)),
        Err(FieldError::range("usize"))
    );
    assert_eq!(
        codec::usize(&Value::Null),
        Err(FieldError::expected("usize", &Value::Null))
    );

    assert_eq!(codec::bool(&Value::Bool(true)), Ok(true));
    assert_eq!(
        codec::bool(&Value::Int(1)),
        Err(FieldError::expected("boolean", &Value::Int(1)))
    );

    assert_eq!(codec::str(&Value::string("é")), Ok("é"));
    assert_eq!(
        codec::str(&Value::string(b"\xff".as_slice())),
        Err(FieldError::invalid("string is not valid UTF-8"))
    );
    assert_eq!(
        codec::str(&Value::Null),
        Err(FieldError::expected("string", &Value::Null))
    );

    assert_eq!(
        codec::bytes(&Value::string(b"a\xff\0".as_slice())),
        Ok(b"a\xff\0".as_slice())
    );
    assert_eq!(
        codec::bytes(&Value::Int(0)),
        Err(FieldError::expected("string", &Value::Int(0)))
    );

    let arr = Value::array_from([Value::Int(1), Value::Int(2)]);
    assert_eq!(codec::array(&arr).map(<[Value]>::len), Ok(2));
    assert_eq!(
        codec::array(&Value::object()),
        Err(FieldError::expected("array", &Value::object()))
    );
    assert_eq!(codec::array_bounded::<u32>(&arr, 2), Ok(vec![1, 2]));
    assert!(matches!(
        codec::array_bounded::<u32>(&arr, 1),
        Err(FieldError::Invalid { .. })
    ));

    let obj = Value::object_from([("k", Value::Bool(true))]);
    assert_eq!(codec::object(&obj).map(BTreeMap::len), Ok(1));
    assert_eq!(
        codec::object(&arr),
        Err(FieldError::expected("object", &arr))
    );
}

#[test]
fn field_helpers_and_deny_unknown_enforce_schema() {
    let val = Value::object_from([
        ("count", Value::Int(3)),
        ("note", Value::Null),
        ("tag", Value::string("v1")),
    ]);
    let obj = codec::object(&val).expect("object");

    assert_eq!(codec::req::<u32>(obj, "count"), Ok(3));
    assert_eq!(
        codec::req::<u32>(obj, "missing"),
        Err(FieldError::missing("missing"))
    );
    assert_eq!(
        codec::req::<u32>(obj, "tag"),
        Err(FieldError::expected("u32", &Value::string("v1")).in_field("tag"))
    );
    assert_eq!(
        codec::req::<u32>(obj, "tag").unwrap_err().to_string(),
        "field 'tag': expected u32, found string"
    );

    assert_eq!(codec::opt::<String>(obj, "missing"), Ok(None));
    assert_eq!(codec::opt::<String>(obj, "note"), Ok(None));
    assert_eq!(codec::opt::<String>(obj, "tag"), Ok(Some("v1".into())));
    assert!(matches!(
        codec::opt::<u32>(obj, "tag"),
        Err(FieldError::Field { ref field, .. }) if field == "tag"
    ));

    assert_eq!(
        codec::opt_or_else(obj, "missing", || "SHA256".to_owned()),
        Ok("SHA256".to_owned())
    );
    assert_eq!(
        codec::opt_or_else(obj, "tag", || "default".to_owned()),
        Ok("v1".to_owned())
    );
    assert_eq!(
        codec::opt_or_else(obj, "note", || "default".to_owned()),
        Err(FieldError::expected("string", &Value::Null).in_field("note"))
    );

    assert_eq!(codec::deny_unknown(obj, &["count", "note", "tag"]), Ok(()));
    assert_eq!(
        codec::deny_unknown(obj, &["count", "tag"]),
        Err(FieldError::unknown("note"))
    );

    let non_utf8 = Value::object_from([(BString::from(b"bad\xff".as_slice()), Value::Null)]);
    let non_utf8_obj = codec::object(&non_utf8).expect("object");
    assert_eq!(
        codec::deny_unknown(non_utf8_obj, &["ok"]),
        Err(FieldError::unknown("bad\u{fffd}"))
    );
}

#[test]
fn to_json_and_from_json_round_trip_standard_and_custom_types() {
    fn round_trip<T>(value: T)
    where
        T: ToJson + FromJson + PartialEq + std::fmt::Debug,
    {
        let encoded = value.to_json();
        let decoded = T::from_json(&encoded).expect("round-trip decode");
        assert_eq!(decoded, value);
    }

    round_trip(());
    round_trip(true);
    round_trip(false);
    round_trip(u8::MAX);
    round_trip(u16::MAX);
    round_trip(u32::MAX);
    round_trip(u64::MAX);
    round_trip(42_usize);
    round_trip(i8::MIN);
    round_trip(i16::MIN);
    round_trip(i32::MIN);
    round_trip(i64::MIN);
    round_trip(-99_isize);
    round_trip(1.5_f64);
    round_trip("hello café".to_owned());
    round_trip(BString::from(b"raw\xffbytes".as_slice()));
    round_trip(Box::new(123_u32));
    round_trip(Some("present".to_owned()));
    round_trip(Option::<String>::None);
    round_trip(vec![1_u8, 2, 3, 255]);
    round_trip([10_u8, 20, 30, 40]);
    round_trip(std::collections::BTreeSet::from([
        "a".to_owned(),
        "b".to_owned(),
    ]));
    round_trip(BTreeMap::from([("k".to_owned(), 9_u64)]));
    round_trip(BTreeMap::from([(BString::from(b"\xff".as_slice()), true)]));
    round_trip(Value::object_from([("x", Value::Int(1))]));

    assert_eq!("slice".to_json(), Value::string("slice"));
    assert_eq!([1_u32, 2].as_slice().to_json(), vec![1_u32, 2].to_json());
    assert_eq!(f64::from_json(&Value::Int(-4)), Ok(-4.0));
    assert_eq!(f64::from_json(&Value::UInt(8)), Ok(8.0));
    assert_eq!(
        u8::from_json(&Value::UInt(256)),
        Err(FieldError::range("u8"))
    );
    assert_eq!(
        i8::from_json(&Value::Int(-129)),
        Err(FieldError::range("i8"))
    );
    assert!(matches!(
        <[u8; 3]>::from_json(&vec![1_u8, 2].to_json()),
        Err(FieldError::Invalid { .. })
    ));

    let bad_map = Value::object_from([(BString::from(b"\xff".as_slice()), Value::Int(1))]);
    assert_eq!(
        BTreeMap::<String, i64>::from_json(&bad_map),
        Err(FieldError::invalid("object key is not valid UTF-8"))
    );

    #[derive(Debug, PartialEq)]
    struct Record {
        name: String,
        flag: bool,
        tag: Option<String>,
        algo: String,
    }

    impl ToJson for Record {
        fn to_json(&self) -> Value {
            let mut entries = vec![("name", self.name.to_json()), ("flag", self.flag.to_json())];
            if let Some(tag) = &self.tag {
                entries.push(("tag", tag.to_json()));
            }
            if self.algo != "SHA256" {
                entries.push(("algo", self.algo.to_json()));
            }
            Value::object_from(entries)
        }
    }

    impl FromJson for Record {
        fn from_json(value: &Value) -> Result<Self, FieldError> {
            let obj = object(value)?;
            deny_unknown(obj, &["name", "flag", "tag", "algo"])?;
            Ok(Self {
                name: req(obj, "name")?,
                flag: req(obj, "flag")?,
                tag: opt(obj, "tag")?,
                algo: opt_or_else(obj, "algo", || "SHA256".to_owned())?,
            })
        }
    }

    round_trip(Record {
        name: "fmt".into(),
        flag: true,
        tag: Some("v11".into()),
        algo: "SHA256".into(),
    });
    round_trip(Record {
        name: "zlib".into(),
        flag: false,
        tag: None,
        algo: "SHA512".into(),
    });
}

#[test]
fn parse_accepts_raw_control_characters_in_strings_like_jsoncpp() {
    let options = ReadOptions::default();
    let with_lf = parse(
        b"{\"Id\": \"forkfd\", \"Copyright\": \"line1\nline2\"}",
        &options,
    )
    .expect("jsoncpp accepts raw LF in strings");
    assert_eq!(
        with_lf.as_object().and_then(|o| o.get(b"Id".as_slice())),
        Some(&Value::string("forkfd"))
    );
    assert_eq!(
        with_lf
            .as_object()
            .and_then(|o| o.get(b"Copyright".as_slice())),
        Some(&Value::string("line1\nline2"))
    );

    let with_tab =
        parse(b"{\"b\": \"tab\there\"}", &options).expect("jsoncpp accepts raw TAB in strings");
    assert_eq!(
        with_tab.as_object().and_then(|o| o.get(b"b".as_slice())),
        Some(&Value::string("tab\there"))
    );
}

#[test]
fn bounded_parse_rejects_each_exceeded_budget() {
    use crate::{ParseBudget, parse_bounded};
    let options = ReadOptions::default();
    let base = ParseBudget::artifact(1 << 20);
    let ok = parse_bounded(br#"{"a":[1,2,"xy"]}"#, &options, &base);
    assert!(ok.is_ok());

    let small_input = ParseBudget {
        max_input_bytes: 4,
        ..base
    };
    let err = parse_bounded(b"[1,2,3]", &options, &small_input).unwrap_err();
    assert!(err.message.contains("input limit"), "{err}");

    let deep = "[".repeat(65) + &"]".repeat(65);
    let err = parse_bounded(deep.as_bytes(), &options, &base).unwrap_err();
    assert!(err.message.contains("nesting limit of 64"), "{err}");

    let few_nodes = ParseBudget {
        max_nodes: 3,
        ..base
    };
    let err = parse_bounded(b"[1,2,3]", &options, &few_nodes).unwrap_err();
    assert!(err.message.contains("value limit"), "{err}");

    let short_strings = ParseBudget {
        max_string_bytes: 2,
        ..base
    };
    let err = parse_bounded(br#"["abc"]"#, &options, &short_strings).unwrap_err();
    assert!(err.message.contains("string limit"), "{err}");

    let total = ParseBudget {
        max_total_string_bytes: 5,
        ..base
    };
    let err = parse_bounded(br#"{"ab":"cd","ef":1}"#, &options, &total).unwrap_err();
    assert!(err.message.contains("total string limit"), "{err}");
}

#[test]
fn unbounded_parse_keeps_legacy_depth_limit() {
    let options = ReadOptions::default();
    let deep = "[".repeat(1001) + &"]".repeat(1001);
    let err = parse(deep.as_bytes(), &options).unwrap_err();
    assert_eq!(err.message, "Exceeded the JSON nesting limit of 1000.");
}
