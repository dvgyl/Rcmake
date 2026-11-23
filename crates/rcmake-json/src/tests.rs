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
