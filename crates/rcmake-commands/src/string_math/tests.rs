use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::{ByteSlice, Limits, hash::HashAlgo};
use rcmake_diag::{PolicyId, PolicyStatus};
use rcmake_eval::{Interp, InterpConfig, RunMode};
use rcmake_host::{Host, PureHost, Stream};

fn setup(script: &str) -> (Interp, Arc<PureHost>) {
    let host = Arc::new(PureHost::new(
        PathBuf::from("/work"),
        PathBuf::from("/bin/cmake"),
    ));
    assert!(
        host.seed_file(Path::new("/work/test.cmake"), script.as_bytes())
            .is_ok()
    );
    let interp = Interp::new(
        InterpConfig {
            host: host.clone(),
            mode: RunMode::Script,
            cmake_root: PathBuf::from("/share/cmake"),
            cmake_command: PathBuf::from("/bin/cmake"),
            cwd: PathBuf::from("/work"),
            limits: Limits::default(),
            diag: Default::default(),
            defines: Vec::new(),
            script_args: Vec::new(),
        },
        crate::register_all,
    );
    (interp, host)
}

fn run(interp: &mut Interp, host: &PureHost) {
    let result = interp.run_script(Path::new("/work/test.cmake"));
    assert_eq!(
        result.exit_code,
        0,
        "{}",
        host.stream_bytes(Stream::Stderr).to_str_lossy()
    );
}

fn check(interp: &mut Interp, values: &[(&str, &[u8])]) {
    for &(name, expected) in values {
        assert_eq!(
            interp
                .cx()
                .var(name.as_bytes())
                .as_deref()
                .map(|value| value.as_slice()),
            Some(expected),
            "{name}"
        );
    }
}

#[test]
fn search_replace_and_join() {
    let (mut interp, host) = setup(
        r#"
        string(FIND "éab-ab" ab first)
        string(FIND "éab-ab" ab last REVERSE)
        string(FIND "éab-ab" xx missing)
        string(FIND "éab-ab" "" end REVERSE)
        string(REPLACE abc Z replaced a bcabc a)
        string(REPLACE "" Z unchanged a bc)
        string(CONCAT text "a;b" c)
        string(APPEND text d e)
        string(PREPEND text ">" "<")
        string(JOIN "::" joined "" "a;b" "" c)
        string(JOIN "::" empty)
        string(CONCAT empty_concat)
        string(APPEND untouched)
        string(PREPEND untouched)
    "#,
    );
    run(&mut interp, &host);
    check(
        &mut interp,
        &[
            ("first", b"2"),
            ("last", b"5"),
            ("missing", b"-1"),
            ("end", b"7"),
            ("replaced", b"ZZa"),
            ("unchanged", b"abc"),
            ("text", b"><a;bcde"),
            ("joined", b"::a;b::::c"),
            ("empty", b""),
            ("empty_concat", b""),
        ],
    );
    assert!(!interp.cx().var_defined(b"untouched"));
}

#[test]
fn byte_transforms_and_comparisons() {
    let (mut interp, host) = setup(
        r#"
        string(ASCII 65x 128 255 bytes)
        string(HEX "${bytes}" hex)
        string(LENGTH "${bytes}" length)
        string(SUBSTRING "${bytes}" 1 -1 suffix)
        string(SUBSTRING "é" 0 1 first_byte)
        string(SUBSTRING "abc" 3 10 end)
        string(SUBSTRING "abc" 2junk 20 clamped)
        string(TOUPPER "abÇ" upper)
        string(TOLOWER "ABÇ" lower extra)
        string(STRIP " \tword\n\r " stripped)
        string(GENEX_STRIP "x$<outer$<inner>y>z$<unfinished" genex)
        string(MAKE_C_IDENTIFIER "1a-Ü" ident)
        string(REPEAT xyz +2 repeat)
        string(REPEAT xyz 0 zero)
        string(REPEAT "" 9223372036854775808 wide)
        string(COMPARE LESS a b lt)
        string(COMPARE GREATER a b gt)
        string(COMPARE EQUAL a a eq)
        string(COMPARE NOTEQUAL a a ne)
        string(COMPARE LESS_EQUAL a a le)
        string(COMPARE GREATER_EQUAL b a ge)
    "#,
    );
    run(&mut interp, &host);
    check(
        &mut interp,
        &[
            ("bytes", b"A\x80\xff"),
            ("hex", b"4180ff"),
            ("length", b"3"),
            ("suffix", b"\x80\xff"),
            ("first_byte", b"\xc3"),
            ("end", b""),
            ("clamped", b"c"),
            ("upper", "ABÇ".as_bytes()),
            ("lower", "abÇ".as_bytes()),
            ("stripped", b"word"),
            ("genex", b"xz$<unfinished"),
            ("ident", b"_1a___"),
            ("repeat", b"xyzxyz"),
            ("zero", b""),
            ("wide", b""),
            ("lt", b"1"),
            ("gt", b"0"),
            ("eq", b"1"),
            ("ne", b"0"),
            ("le", b"1"),
            ("ge", b"1"),
        ],
    );
}

#[test]
fn regex_captures_replacements_and_empty_matches() {
    let (mut interp, host) = setup(
        r#"
        string(REGEX MATCH "(a)?(b)" match "c" "b")
        string(CONCAT captures "${CMAKE_MATCH_0}|${CMAKE_MATCH_1}|${CMAKE_MATCH_2}|${CMAKE_MATCH_COUNT}")
        string(REGEX MATCHALL "a*" all b a)
        string(CONCAT last "${CMAKE_MATCH_0}|${CMAKE_MATCH_2}|${CMAKE_MATCH_COUNT}")
        string(REGEX REPLACE "a*" Z retry ba)
        string(REGEX REPLACE "(a)?(b)" [=[\2-\1-\0\\\n]=] replaced c b ab)
        string(REGEX QUOTE quoted [=[a.b[1]+?{|}\]=])
        string(REGEX MATCH "${quoted}" literal [=[a.b[1]+?{|}\]=])
        string(REGEX REPLACE a [=[\2]=] no_match b)
    "#,
    );
    run(&mut interp, &host);
    check(
        &mut interp,
        &[
            ("match", b"b"),
            ("captures", b"b||b|2"),
            ("all", b";a;"),
            ("last", b"||"),
            ("retry", b"ZbZZ"),
            ("replaced", b"cb--b\\\nb-a-ab\\\n"),
            ("quoted", br"a\.b\[1\]\+\?{\|}\\"),
            ("literal", br"a.b[1]+?{|}\"),
            ("no_match", b"b"),
            ("CMAKE_MATCH_COUNT", b"0"),
        ],
    );
}

#[test]
fn regex_matchall_clears_intermediate_captures_and_notifies_watches() {
    let (mut interp, host) = setup(
        r#"
        string(REGEX MATCHALL "(a)?b" plain abb)
        set(cleared "${CMAKE_MATCH_1}")
        variable_watch(CMAKE_MATCH_1)
        string(REGEX MATCHALL "(a)?b" watched abb)
        "#,
    );
    run(&mut interp, &host);
    check(
        &mut interp,
        &[
            ("plain", b"ab;b"),
            ("watched", b"ab;b"),
            ("cleared", b""),
            ("CMAKE_MATCH_1", b""),
            ("CMAKE_MATCH_0", b"b"),
        ],
    );
    let log = host.stream_bytes(Stream::Stderr);
    assert_eq!(log.to_str_lossy().matches("MODIFIED_ACCESS").count(), 2);
}

#[test]
fn regex_anchor_policy() {
    for (policy, expected) in [
        (PolicyStatus::Old, b"a;b;c".as_slice()),
        (PolicyStatus::New, b"a"),
    ] {
        let (mut interp, host) = setup("string(REGEX MATCHALL \"^.\" out abc)");
        assert!(interp.cx().set_policy(PolicyId::new(186), policy).is_ok());
        run(&mut interp, &host);
        check(&mut interp, &[("out", expected)]);
    }
}

#[test]
fn hashes_and_uuids() {
    let names = [
        "MD5", "SHA1", "SHA224", "SHA256", "SHA384", "SHA512", "SHA3_224", "SHA3_256", "SHA3_384",
        "SHA3_512",
    ];
    let mut script = String::new();
    for name in names {
        use std::fmt::Write as _;
        assert!(writeln!(&mut script, "string({name} {name} \"abc\")").is_ok());
    }
    script.push_str(r#"
        string(UUID v3 NAMESPACE 6ba7b810-9dad-11d1-80b4-00c04fd430c8 NAME www.widgets.com TYPE MD5)
        string(UUID v5 TYPE SHA1 UPPER NAME www.widgets.com NAMESPACE 6BA7B810-9DAD-11D1-80B4-00C04FD430C8)
    "#);
    let (mut interp, host) = setup(&script);
    run(&mut interp, &host);
    for name in names {
        let expected = HashAlgo::parse(name.as_bytes()).map(|algo| algo.hex_digest(b"abc").into());
        assert_eq!(interp.cx().var(name.as_bytes()), expected);
    }
    check(
        &mut interp,
        &[
            ("v3", b"3d813cbb-47fb-32ba-91df-831e1593ac29"),
            ("v5", b"21F7F8DE-8051-5B89-8680-0195EF798B6A"),
        ],
    );
}

#[test]
fn configure_random_and_timestamp() {
    let (mut interp, host) = setup(
        r#"
        string(CONFIGURE [=[@NAME@ ${NAME} $ENV{TOKEN} @ABSENT@ \n]=] expanded)
        string(CONFIGURE [=[@NAME@ ${NAME}]=] quoted @ONLY ESCAPE_QUOTES)
        string(CONFIGURE [=[#cmakedefine01 FLAG
#cmakedefine ABSENT
end
]=] defined)
        string(RANDOM RANDOM_SEED 1 LENGTH 3 generated)
        string(RANDOM LENGTH 3 continued)
        string(RANDOM RANDOM_SEED 1 ALPHABET 0123456789 LENGTH 12 digits)
        string(TIMESTAMP iso UTC)
        string(TIMESTAMP fields "%a|%A|%b|%B|%d|%j|%m|%Y|%y|%H|%I|%M|%S|%s|%f|%U|%V|%w|%z|%Z|%%|%W|%" UTC)
        string(TIMESTAMP local)
    "#,
    );
    interp.cx().set_var(b"NAME", "a\"b".into());
    interp.cx().set_var(b"FLAG", "ON".into());
    host.env_set(b"TOKEN", Some(b"env"));
    host.env_set(b"SOURCE_DATE_EPOCH", Some(b"946684800"));
    run(&mut interp, &host);
    check(&mut interp, &[
        ("expanded", b"a\"b a\"b env  \\n"), ("quoted", b"a\\\"b ${NAME}"),
        ("defined", b"#define FLAG 1\n/* #undef ABSENT */\nend\n"),
        ("generated", b"qoX"), ("continued", b"EIf"), ("digits", b"017452066935"),
        ("iso", b"2000-01-01T00:00:00Z"),
        ("fields", b"Sat|Saturday|Jan|January|01|001|01|2000|00|00|12|00|00|946684800|000000|00|52|6|+0000|GMT|%|%W|%"),
        ("local", b"2000-01-01T00:00:00"),
    ]);

    let (mut interp, host) = setup(r#"string(TIMESTAMP now "%s.%f" UTC)"#);
    let reference = host.now();
    host.sleep(std::time::Duration::from_micros(654_321));
    run(&mut interp, &host);
    let now = host.now();
    assert_eq!(now.secs, reference.secs);
    check(
        &mut interp,
        &[(
            "now",
            format!("{}.{:06}", now.secs, now.nanos / 1000).as_bytes(),
        )],
    );
}

#[test]
fn timestamp_accepts_negative_epoch_values() {
    let (mut interp, host) = setup(
        r#"
        set(ENV{SOURCE_DATE_EPOCH} -86400)
        string(TIMESTAMP prior "%Y-%m-%dT%H:%M:%SZ %s" UTC)
        set(ENV{SOURCE_DATE_EPOCH} -1)
        string(TIMESTAMP sentinel UTC)
        set(ENV{SOURCE_DATE_EPOCH} 9223372036854775807)
        string(TIMESTAMP too_large UTC)
        "#,
    );
    run(&mut interp, &host);
    check(
        &mut interp,
        &[
            ("prior", b"1969-12-31T00:00:00Z -86400"),
            ("sentinel", b""),
            ("too_large", b""),
        ],
    );
}

#[test]
fn json_queries_and_mutations() {
    let (mut interp, host) = setup(
        r#"
        string(JSON data SET [=[{"z":true,"a":[1,null]}]=] a 8 [=["end"]=])
        string(JSON raw GET_RAW "${data}" a)
        string(JSON flag GET "${data}" z)
        string(JSON raw_flag GET_RAW "${data}" z)
        string(JSON end GET "${data}" a +2)
        string(JSON none GET "${data}" a 01)
        string(JSON kind TYPE "${data}" a 1)
        string(JSON key MEMBER "${data}" 0)
        string(JSON length LENGTH "${data}" a)
        string(JSON removed REMOVE "${data}" a 0)
        string(JSON equal EQUAL "${removed}" [=[{"z":true,"a":[null,"end"]}]=])
        string(JSON different EQUAL [=[1]=] [=[1.0]=])
        string(JSON partial PARTIAL_EQUAL [=[{"a":["end"]}]=] "${removed}")
        string(JSON encoded STRING_ENCODE [=[➕;"]=])
        string(JSON items ARRAY_SPLIT [=[["[","]","a;b",{"k":"[;]"},2]]=])
        string(JSON permissive GET [=[{"a":[01,],} trailing text]=] a 0)
    "#,
    );
    run(&mut interp, &host);
    check(
        &mut interp,
        &[
            ("raw", br#"[ 1, null, "end" ]"#),
            ("flag", b"ON"),
            ("raw_flag", b"true"),
            ("end", b"end"),
            ("none", b""),
            ("kind", b"NULL"),
            ("key", b"a"),
            ("length", b"3"),
            ("equal", b"ON"),
            ("different", b"OFF"),
            ("partial", b"ON"),
            ("encoded", br#""\u2795;\"""#),
            (
                "items",
                br#""\u005B";"\u005D";"a\;b";{"k":"\u005B\;\u005D"};2"#,
            ),
            ("permissive", b"1"),
        ],
    );
}

#[test]
fn json_error_paths() {
    let (mut interp, host) = setup(
        r#"
        string(JSON missing ERROR_VARIABLE missing_err GET [=[{"root":{}}]=] root item leaf)
        string(JSON index ERROR_VARIABLE index_err GET [=[{"root":[]}]=] root 1)
        string(JSON scalar ERROR_VARIABLE scalar_err GET [=[{"root":1}]=] root item)
        string(JSON invalid ERROR_VARIABLE parse_err TYPE [=[nonsense]=])
        string(JSON valid ERROR_VARIABLE valid_err TYPE [=[true]=])
        string(JSON same ERROR_VARIABLE same GET [=[{}]=] x)
    "#,
    );
    run(&mut interp, &host);
    check(&mut interp, &[
        ("missing", b"root-item-NOTFOUND"), ("missing_err", b"member 'item' not found"),
        ("index", b"root-1-NOTFOUND"), ("index_err", b"expected an index less than 0 got '1'"),
        ("scalar", b"root-item-NOTFOUND"),
        ("scalar_err", b"invalid path 'root item', need element of OBJECT or ARRAY type to lookup 'item' got NUMBER"),
        ("invalid", b"NOTFOUND"), ("parse_err", b"failed parsing json string:\nSyntax error: value, object or array expected.\nnonsense\n^"),
        ("valid", b"BOOLEAN"), ("valid_err", b"NOTFOUND"), ("same", b"member 'x' not found"),
    ]);
}

#[test]
fn math_precedence_and_formats() {
    let (mut interp, host) = setup(
        r#"
        math(EXPR order "1 | 3 ^ 1 & 7 << 1 + 2 * 3")
        math(EXPR grouping "((7 + 5) * -3 / 4) % 4")
        math(EXPR unary "~ + - 2")
        math(EXPR decimal "010 + 09 + 0xa + 0XA" OUTPUT_FORMAT DECIMAL)
        math(EXPR hex "-1" OUTPUT_FORMAT HEXADECIMAL)
        math(EXPR shifted "-16 >> 2")
        math(EXPR left "1 << 63")
        math(EXPR overflow "9223372036854775807 + 1")
        math(EXPR ignored "1a + 2")
        math(INCREMENT counter)
        math(DECREMENT counter)
    "#,
    );
    interp.cx().set_var(b"counter", " 02".into());
    run(&mut interp, &host);
    check(
        &mut interp,
        &[
            ("order", b"3"),
            ("grouping", b"-1"),
            ("unary", b"1"),
            ("decimal", b"39"),
            ("hex", b"0xffffffffffffffff"),
            ("shifted", b"-4"),
            ("left", b"-9223372036854775808"),
            ("overflow", b"-9223372036854775808"),
            ("ignored", b"3"),
            ("counter", b"2"),
        ],
    );
    let stderr = host.stream_bytes(Stream::Stderr);
    assert!(stderr.contains_str("signed integer overflow"));
    assert!(stderr.contains_str("Unexpected character in expression"));
}

#[test]
fn command_errors() {
    for (script, diagnostic) in [
        ("math(EXPR x \"2/0\")", "divide by zero"),
        ("math(EXPR x \"2%0\")", "modulo by zero"),
        ("math(EXPR x \"0x8000000000000000\")", "a numeric value"),
        ("math(EXPR x \"(1 2)\")", "unexpected"),
        ("math(EXPR x \"1\" OUTPUT_FORMAT BINARY)", "is invalid"),
        (
            "string(ASCII 256 x)",
            "Character with code 256 does not exist",
        ),
        (
            "string(SUBSTRING a -1 1 x)",
            "begin index: -1 is out of range",
        ),
        (
            "string(REPEAT abc -1 x)",
            "repeat count is not a positive number",
        ),
        (
            "string(UUID x NAMESPACE wrong NAME foo TYPE MD5)",
            "malformed NAMESPACE UUID",
        ),
        ("string(REGEX MATCH \"(a\" x a)", "Failed to compile regex"),
        ("string(REGEX REPLACE a [[\\q]] x b)", "Unknown escape"),
        ("string(REGEX REPLACE a [[\\2]] x a)", "out-of-range escape"),
        (
            "string(JSON x GET [[{}]] absent)",
            "member 'absent' not found",
        ),
    ] {
        let (mut interp, host) = setup(script);
        assert_eq!(
            interp.run_script(Path::new("/work/test.cmake")).exit_code,
            1,
            "{script}"
        );
        let stderr = host.stream_bytes(Stream::Stderr);
        assert!(
            stderr.contains_str(diagnostic),
            "{script}\n{}",
            stderr.to_str_lossy()
        );
    }
}

#[test]
fn configure_tracks_template_lines() {
    let (mut interp, host) = setup(
        "string(CONFIGURE [=[${CMAKE_CURRENT_LIST_LINE}\n${CMAKE_CURRENT_LIST_LINE}]=] lines)",
    );
    run(&mut interp, &host);
    check(&mut interp, &[("lines", b"1\n2")]);
}
