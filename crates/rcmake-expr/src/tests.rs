#![allow(clippy::unwrap_used)]

use std::{collections::BTreeMap, path::Path};

use rcmake_base::Limits;
use rcmake_diag::{DiagCategory, SourceMap, source::SourceKind};
use rcmake_host::{PureHost, WriteMode};
use rcmake_syntax::parse_list_file;

use crate::configure::{ConfigureOptions, configure, configure_file};
use crate::*;

struct Env {
    vars: BTreeMap<BString, BString>,
    cache: BTreeMap<BString, BString>,
    environment: BTreeMap<BString, BString>,
    policies: BTreeMap<u16, PolicyStatus>,
    warnings: Vec<Diagnostic>,
    lookups: Vec<BString>,
    regex_patterns: Vec<BString>,
    match_clears: usize,
    host: PureHost,
    budget: ArgumentBudget,
}

impl Env {
    fn new() -> Self {
        Self {
            vars: BTreeMap::new(),
            cache: BTreeMap::new(),
            environment: BTreeMap::new(),
            policies: BTreeMap::new(),
            warnings: Vec::new(),
            lookups: Vec::new(),
            regex_patterns: Vec::new(),
            match_clears: 0,
            host: PureHost::new("/work".into(), "/bin/rcmake".into()),
            budget: ArgumentBudget::default(),
        }
    }

    fn set(&mut self, name: &str, value: impl Into<BString>) {
        self.vars.insert(name.into(), value.into());
    }

    fn condition(&mut self, args: &str) -> Result<bool, CondError> {
        let source = format!("if({args})\nendif()");
        eval_condition(&expanded(source.as_bytes(), self), self)
    }
}

impl ExpandEnv for Env {
    fn var(&mut self, name: &[u8]) -> Option<BString> {
        self.lookups.push(name.into());
        self.vars
            .get(name)
            .or_else(|| self.cache.get(name))
            .cloned()
    }

    fn cache_var(&mut self, name: &[u8]) -> Option<BString> {
        self.cache.get(name).cloned()
    }

    fn env_var(&mut self, name: &[u8]) -> Option<BString> {
        self.environment.get(name).cloned()
    }

    fn current_line(&self) -> u32 {
        42
    }

    fn argument_budget(&self) -> ArgumentBudget {
        self.budget
    }
}

impl CondEnv for Env {
    fn is_defined(&mut self, name: &[u8]) -> bool {
        self.vars.contains_key(name) || self.cache.contains_key(name)
    }

    fn cache_defined(&mut self, name: &[u8]) -> bool {
        self.cache.contains_key(name)
    }

    fn env_defined(&mut self, name: &[u8]) -> bool {
        self.environment.contains_key(name)
    }

    fn command_exists(&self, name: &[u8]) -> bool {
        name.eq_ignore_ascii_case(b"message")
    }

    fn policy_known(&self, name: &[u8]) -> bool {
        PolicyId::parse(name).and_then(PolicyId::info).is_some()
    }

    fn diagnostic_known(&self, name: &[u8]) -> bool {
        DiagCategory::parse(name).is_some()
    }

    fn target_exists(&self, name: &[u8]) -> bool {
        name == b"project::library"
    }

    fn test_exists(&self, name: &[u8]) -> bool {
        name == b"check"
    }

    fn rule_exists(&self, name: &[u8]) -> bool {
        name == b"CXX_COMPILER"
    }

    fn policy(&self, id: PolicyId) -> PolicyStatus {
        self.policies
            .get(&id.number())
            .copied()
            .unwrap_or(PolicyStatus::New)
    }

    fn host(&self) -> &dyn Host {
        &self.host
    }

    fn base_dir(&self) -> &Path {
        Path::new("/work")
    }

    fn regex(&mut self, pattern: &[u8]) -> Result<Arc<Regex>, RegexError> {
        self.regex_patterns.push(pattern.into());
        Err(RegexError {
            message: "test regex compilation failure".to_owned(),
        })
    }

    fn set_match_vars(&mut self, captures: Option<(&Captures, &[u8])>) {
        if captures.is_none() {
            self.match_clears += 1;
        }
    }

    fn warn(&mut self, diag: Diagnostic) {
        self.warnings.push(diag);
    }
}

fn expanded(src: &[u8], env: &mut Env) -> Vec<ExpandedArg> {
    let mut sources = SourceMap::default();
    let id = sources
        .add("test.cmake".into(), SourceKind::File, Arc::from(src))
        .unwrap();
    let parsed = parse_list_file(src, id, SourceKind::File, &Limits::default()).unwrap();
    let mut out = Vec::new();
    expand_args(&parsed.commands.first().unwrap().args, env, &mut out).unwrap();
    out
}

fn assert_conditions(env: &mut Env, cases: &[(&str, bool)]) {
    for &(condition, expected) in cases {
        assert_eq!(env.condition(condition).unwrap(), expected, "{condition}");
    }
}

#[test]
fn expansion_keeps_delimiters_and_splits_only_unquoted_arguments() {
    let mut env = Env::new();
    env.set("list", r";a;b\;c;;[d;e]");
    let args = expanded(
        br#"f(a\;b "a\;b" [=[${list}\n;a]=] ${list} "${list}" ${missing} "${missing}")"#,
        &mut env,
    );
    let values: Vec<_> = args
        .iter()
        .map(|arg| (arg.kind, arg.value.as_slice()))
        .collect();
    assert_eq!(
        values,
        [
            (ArgKind::Unquoted, b"a;b".as_slice()),
            (ArgKind::Quoted, br"a\;b"),
            (ArgKind::Bracket, br"${list}\n;a"),
            (ArgKind::Unquoted, b"a"),
            (ArgKind::Unquoted, b"b;c"),
            (ArgKind::Unquoted, b"[d;e]"),
            (ArgKind::Quoted, br";a;b\;c;;[d;e]"),
            (ArgKind::Quoted, b""),
        ]
    );
}

#[test]
fn expansion_resolves_nested_names_and_inserts_raw_bytes() {
    let mut env = Env::new();
    env.set("b", "suffix");
    env.set("a_suffix", br"${b}\t".as_slice());
    env.set("a;b", "escaped name");
    env.set("raw", b"\xff".as_slice());
    env.cache.insert("slot".into(), "cached".into());
    env.environment.insert("slot".into(), "env".into());
    let actual = expand_string(
        br"${a_${b}}|${a\;b}|$CACHE{slot}|$ENV{slot}|$<X:${b}>|@b@|\$${b}|\t\r\n\?|${raw}|${}",
        &mut env,
    )
    .unwrap();
    assert_eq!(
        actual.as_slice(),
        b"${b}\\t|escaped name|cached|env|$<X:suffix>|@b@|$suffix|\t\r\n?|\xff|"
    );
}

#[test]
fn expansion_tracks_raw_lines_and_reports_errors() {
    let mut env = Env::new();
    let args = expanded(
        b"\nf(\"${CMAKE_CURRENT_LIST_LINE}\\n\n${CMAKE_CURRENT_LIST_LINE}\")",
        &mut env,
    );
    let arg = args.first().unwrap();
    assert_eq!(arg.line, 2);
    assert_eq!(arg.value.as_slice(), b"2\n\n3");
    assert_eq!(
        expand_string(b"${CMAKE_CURRENT_LIST_LINE}", &mut env).unwrap(),
        "42"
    );
    for (src, expected) in [
        (br"\z".as_slice(), "Invalid character escape '\\z'."),
        (br"\7", "Invalid character escape '\\7'."),
        (
            b"${unfinished",
            "There is an unterminated variable reference.",
        ),
        (b"${a:b}", "Invalid character (':') in a variable name: 'a'"),
        (
            b"$BAD{x}",
            "Syntax $BAD{{} is not supported.  Only ${}, $ENV{}, and $CACHE{} are allowed.",
        ),
    ] {
        assert_eq!(expand_string(src, &mut env).unwrap_err().message, expected);
    }
    let deep = format!("{}x{}", "${".repeat(257), "}".repeat(257));
    assert!(expand_string(deep.as_bytes(), &mut env).is_err());
}

fn argument(kind: ArgKind, text: &str) -> rcmake_syntax::Argument {
    rcmake_syntax::Argument {
        kind,
        text: text.into(),
        line: 1,
        span: 0..text.len(),
    }
}

fn list_of(count: usize) -> String {
    vec!["a"; count].join(";")
}

#[test]
fn unquoted_expansion_stops_at_the_element_cap() {
    let mut env = Env::new();
    env.budget = ArgumentBudget::new(1_000, usize::MAX);
    env.set("FULL", list_of(1_000));
    env.set("OVER", list_of(1_001));
    let mut out = Vec::new();
    expand_args(
        &[argument(ArgKind::Unquoted, "${FULL}")],
        &mut env,
        &mut out,
    )
    .unwrap();
    assert_eq!(out.len(), 1_000);

    let mut out = Vec::new();
    let error = expand_args(
        &[argument(ArgKind::Unquoted, "${OVER}")],
        &mut env,
        &mut out,
    )
    .unwrap_err();
    assert_eq!(
        error.message,
        "Command arguments expanded to more than 1000 elements."
    );
    assert!(out.is_empty());
}

#[test]
fn cap_counts_all_arguments_of_one_command() {
    let mut env = Env::new();
    env.budget = ArgumentBudget::new(1_000, usize::MAX);
    env.set("L", list_of(600));
    let args = [
        argument(ArgKind::Unquoted, "${L}"),
        argument(ArgKind::Unquoted, "${L}"),
    ];
    assert!(expand_args(&args, &mut env, &mut Vec::new()).is_err());

    // The evaluator expands one argument at a time into the same output vector.
    let mut out = Vec::new();
    expand_args(&args[..1], &mut env, &mut out).unwrap();
    assert!(expand_args(&args[1..], &mut env, &mut out).is_err());
    assert_eq!(out.len(), 600);

    env.set("L", list_of(1_000));
    let quoted = [
        argument(ArgKind::Unquoted, "${L}"),
        argument(ArgKind::Quoted, "one more"),
    ];
    assert!(expand_args(&quoted, &mut env, &mut Vec::new()).is_err());
}

#[test]
fn byte_cap_counts_all_arguments_and_the_caller_bytes() {
    let mut env = Env::new();
    env.budget = ArgumentBudget::new(usize::MAX, 10);
    env.set("L", "abcd;efgh");
    let args = [argument(ArgKind::Unquoted, "${L}")];
    let mut out = Vec::new();
    expand_args(&args, &mut env, &mut out).unwrap();
    assert_eq!(out.len(), 2);

    // The caller reports the bytes of the arguments that it already holds.
    env.budget.used_bytes = 8;
    let error = expand_args(&args, &mut env, &mut out).unwrap_err();
    assert!(error.is_resource_limit());
    assert_eq!(
        error.message,
        "Command arguments expanded to more than 10 bytes."
    );
    assert_eq!(
        out.len(),
        2,
        "a failed call keeps only the earlier arguments"
    );

    env.budget.used_bytes = 4;
    assert!(expand_args(&args, &mut env, &mut Vec::new()).is_err());
}

#[test]
fn syntax_errors_are_not_resource_limits() {
    let mut env = Env::new();
    let args = [argument(ArgKind::Quoted, "a\\z")];
    let error = expand_args(&args, &mut env, &mut Vec::new()).unwrap_err();
    assert!(!error.is_resource_limit());
}

#[test]
fn listfile_expansion_errors_include_the_source_and_argument() {
    let mut env = Env::new();
    env.set("CMAKE_CURRENT_LIST_FILE", "/work/check.cmake");
    let args = [
        rcmake_syntax::Argument {
            kind: ArgKind::Unquoted,
            text: "first".into(),
            line: 1,
            span: 0..5,
        },
        rcmake_syntax::Argument {
            kind: ArgKind::Quoted,
            text: b"a\n\\z".as_slice().into(),
            line: 2,
            span: 6..12,
        },
    ];
    let mut out = Vec::new();
    let error = expand_args(&args, &mut env, &mut out).unwrap_err();
    assert!(out.is_empty());
    assert_eq!(
        error.message,
        "Syntax error in cmake code at\n  /work/check.cmake:3\n\
         when parsing string\n  a\n\\z\nInvalid character escape '\\z'."
    );
}

#[test]
fn condition_reduces_groups_negation_and_logical_operators() {
    let mut env = Env::new();
    env.set("zero", "0.0");
    env.set("one", "zero");
    env.set("left", "ON");
    env.set("right", "OFF");
    assert_conditions(
        &mut env,
        &[
            ("", false),
            ("()", false),
            ("NOT ()", true),
            ("TRUE OR FALSE AND FALSE", false),
            ("TRUE OR (FALSE AND FALSE)", true),
            ("NOT FALSE EQUAL 0", true),
            ("(TRUE) STREQUAL 1", true),
            ("zero", true),
            (r#""zero""#, false),
            (r#""0.0""#, false),
            ("[[zero]]", false),
            ("one", true),
            ("[[TRUE]]", true),
            (r#""(" "#, false),
            (r#"[=[)]=]"#, false),
            ("0x1", true),
            ("inf", true),
            ("nan", true),
        ],
    );
    for bad in [
        "NOT NOT TRUE",
        "TRUE AND",
        "OR TRUE",
        r#"TRUE "AND" TRUE"#,
        "TRUE [[OR]] TRUE",
    ] {
        assert_eq!(
            env.condition(bad).unwrap_err().message,
            "Unknown arguments specified"
        );
    }
    for bad in ["(", ")"] {
        let arg = ExpandedArg {
            value: bad.into(),
            kind: ArgKind::Unquoted,
            line: 1,
        };
        assert!(eval_condition(&[arg], &mut env).is_err());
    }
    env.lookups.clear();
    assert_conditions(
        &mut env,
        &[("left OR right", true), ("right AND left", false)],
    );
    assert_eq!(
        env.lookups,
        ["left", "right", "right", "left"].map(BString::from)
    );
}

#[test]
fn condition_queries_context_predicates_without_dereferencing() {
    let mut env = Env::new();
    env.set("name", "message");
    env.set("empty", "");
    env.cache.insert("c".into(), "".into());
    env.environment.insert("e".into(), "".into());
    assert_conditions(
        &mut env,
        &[
            ("DEFINED empty", true),
            ("DEFINED missing", false),
            ("DEFINED c", true),
            ("DEFINED CACHE{c}", true),
            (r#"DEFINED "CACHE{c}""#, true),
            ("DEFINED CACHE{empty}", false),
            ("DEFINED ENV{e}", true),
            ("DEFINED ENV{}", false),
            ("COMMAND MESSAGE", true),
            ("COMMAND name", false),
            ("POLICY CMP0226", true),
            ("POLICY CMP9999", false),
            ("DIAGNOSTIC CMD_STRICT", true),
            ("DIAGNOSTIC bogus", false),
            ("TARGET project::library", true),
            ("TARGET missing", false),
            ("TEST check", true),
            ("TEST missing", false),
            ("RULE CXX_COMPILER", true),
            ("RULE missing", false),
            ("IS_ABSOLUTE /file", true),
            ("IS_ABSOLUTE relative", false),
            ("IS_ABSOLUTE ~", true),
        ],
    );
}

#[test]
fn condition_compares_numbers_strings_versions_and_lists() {
    let mut env = Env::new();
    env.set("number", "1.25suffix");
    env.set("text", "Alpha");
    env.set("list", r"a;b\;c;;d");
    env.set("empty", "");
    env.set("alias", "list");
    assert_conditions(
        &mut env,
        &[
            ("number LESS 2", true),
            ("number GREATER 1e+suffix", true),
            ("number EQUAL 1.25", true),
            ("2 LESS_EQUAL 2", true),
            ("2 GREATER_EQUAL 2", true),
            ("number EQUAL junk", false),
            ("3 EQUAL 0x1.8p1", true),
            ("0 EQUAL 0x", true),
            ("0 LESS inf", true),
            ("0 EQUAL 0x1p-1075", true),
            ("0 EQUAL 0x1.0000000000000001p-1075", false),
            ("0x1.ffffp1024 EQUAL inf", true),
            ("1 LESS_EQUAL nan", false),
            ("1 EQUAL 1 EQUAL 1", true),
            (r#""number" EQUAL 1.25"#, false),
            ("text STRLESS alpha", true),
            ("beta STRGREATER text", true),
            ("text STREQUAL Alpha", true),
            ("text STRLESS_EQUAL Alpha", true),
            ("Alpha STRGREATER_EQUAL text", true),
            ("1.1 VERSION_LESS 1.2", true),
            ("1.2.0.1 VERSION_GREATER 1.2", true),
            ("1.2.0 VERSION_EQUAL 1.2", true),
            ("1.2 VERSION_LESS_EQUAL 1.2.0", true),
            ("1.2 VERSION_GREATER_EQUAL 1.2.0", true),
            (r#""b;c" IN_LIST "list""#, true),
            (r#""b;c" IN_LIST alias"#, false),
            (r#""" IN_LIST empty"#, true),
            (r#""" IN_LIST missing"#, false),
        ],
    );
}

#[test]
fn condition_checks_paths_using_policies_and_the_host() {
    let mut env = Env::new();
    env.host.create_dir_all(Path::new("/work/folder")).unwrap();
    env.host
        .write(Path::new("/work/old"), b"", WriteMode::Truncate)
        .unwrap();
    env.host
        .write(Path::new("/work/new"), b"", WriteMode::Truncate)
        .unwrap();
    env.host
        .set_permissions(Path::new("/work/old"), 0o400)
        .unwrap();
    env.host
        .set_permissions(Path::new("/work/new"), 0o700)
        .unwrap();
    env.host
        .symlink(Path::new("missing"), Path::new("/work/link"))
        .unwrap();
    env.host
        .write(Path::new("/work/write_only"), b"", WriteMode::Truncate)
        .unwrap();
    env.host
        .set_permissions(Path::new("/work/write_only"), 0o200)
        .unwrap();
    env.set("as_path", "old");
    assert_conditions(
        &mut env,
        &[
            ("EXISTS old", true),
            ("EXISTS write_only", false),
            ("IS_READABLE write_only", false),
            ("EXISTS as_path", false),
            ("EXISTS link", false),
            (r#"EXISTS """#, false),
            ("IS_READABLE old", true),
            ("IS_WRITABLE old", false),
            ("IS_WRITABLE new", true),
            ("IS_EXECUTABLE old", false),
            ("IS_EXECUTABLE new", true),
            ("IS_DIRECTORY folder", true),
            ("IS_DIRECTORY old", false),
            ("IS_SYMLINK link", true),
            ("IS_SYMLINK old", false),
            ("new IS_NEWER_THAN old", true),
            ("old IS_NEWER_THAN new", false),
            ("old IS_NEWER_THAN old", true),
            ("as_path IS_NEWER_THAN new", true),
            ("old IS_NEWER_THAN missing", true),
            ("a//b PATH_EQUAL a/b", true),
            ("a/ PATH_EQUAL a", false),
            ("a/./b PATH_EQUAL a/b", false),
            ("a/ PATH_IS_PREFIX a/b", true),
            ("a PATH_IS_PREFIX a/", true),
            ("a/ PATH_IS_PREFIX a", false),
            ("a PATH_IS_PREFIX ab", false),
            (r#""" PATH_IS_PREFIX a"#, false),
            (r#""" PATH_IS_PREFIX """#, false),
        ],
    );

    env.policies.insert(223, PolicyStatus::Old);
    assert_conditions(&mut env, &[(r#""" PATH_IS_PREFIX /a"#, true)]);
    env.policies.insert(223, PolicyStatus::Warn);
    assert_conditions(&mut env, &[(r#""" PATH_IS_PREFIX """#, true)]);
    env.policies.insert(139, PolicyStatus::Warn);
    assert!(env.condition("a PATH_EQUAL a").is_err());
    env.policies.insert(222, PolicyStatus::Warn);
    assert!(env.condition("a PATH_IS_PREFIX a").is_err());
    assert_eq!(
        env.warnings
            .iter()
            .map(|warning| warning.policy)
            .collect::<Vec<_>>(),
        [223, 139, 222].map(|number| Some(PolicyId::new(number)))
    );

    env.policies.insert(139, PolicyStatus::Old);
    env.policies.insert(222, PolicyStatus::Old);
    assert!(env.condition("a PATH_EQUAL a").is_err());
    assert!(env.condition("a PATH_IS_PREFIX a").is_err());
    assert_eq!(env.warnings.len(), 3);
}

#[test]
fn matches_uses_a_literal_pattern_and_reports_compile_failure() {
    let mut env = Env::new();
    env.set("pattern", "^x");
    assert_conditions(&mut env, &[(r#"MATCHES "[""#, false)]);
    assert!(env.regex_patterns.is_empty());
    assert_eq!(env.match_clears, 0);
    assert_eq!(
        env.condition("TRUE OR x MATCHES pattern")
            .unwrap_err()
            .message,
        "Regular expression \"pattern\" cannot compile"
    );
    assert_eq!(
        env.condition(r#"FALSE AND x MATCHES "${pattern}""#)
            .unwrap_err()
            .message,
        "Regular expression \"^x\" cannot compile"
    );
    assert_eq!(env.regex_patterns, ["pattern", "^x"].map(BString::from));
    assert_eq!(env.match_clears, 2);
}

#[test]
fn configure_expands_directives_and_preserves_template_bytes() {
    let mut env = Env::new();
    env.set("FOO", br#"yes""#.as_slice());
    env.set("VAL", "0.0");
    env.set("OFF_VALUE", "x-NOTFOUND");
    env.set("key", "VAL");
    env.set("raw", br"${VAL}@VAL@".as_slice());
    env.environment.insert("slot".into(), "env".into());
    let input = b"  #  cmakedefine FOO @FOO@ ${FOO}\r\n\
                  \t#cmakedefine01 VAL tail\n\
                  #cmakedefine OFF_VALUE @FOO@\n\
                  #cmakedefine01 missing\n\
                  @missing@|${@key@}|@raw@|$ENV{slot}|\\@VAL@|@@";
    let options = ConfigureOptions {
        escape_quotes: true,
        ..ConfigureOptions::default()
    };
    assert_eq!(
        configure(input, &mut env, options).unwrap().as_slice(),
        b"  #  define FOO yes\\\" yes\\\"\n\
          \t#define VAL tail 1\n\
          /* #undef OFF_VALUE */\n\
          #define missing 0\n\
          |0.0|${VAL}@VAL@|env|\\0.0|@@\n"
    );
    let options = ConfigureOptions {
        at_only: true,
        ..options
    };
    assert_eq!(
        configure(br"@FOO@ ${FOO} $ENV{slot} ${@key@} \\n", &mut env, options)
            .unwrap()
            .as_slice(),
        b"yes\\\" ${FOO} $ENV{slot} ${VAL} \\\\n\n"
    );
    assert_eq!(
        configure(
            b"#cmakedefine FOO #\tcmakedefine OFF_VALUE\n#cmakedefine\rVAL",
            &mut env,
            options,
        )
        .unwrap()
        .as_slice(),
        b"#define FOO #\tcmakedefine OFF_VALUE\n#cmakedefine\rVAL\n"
    );
    assert!(configure(b"${oops", &mut env, ConfigureOptions::default()).is_err());
    assert!(configure(b"", &mut env, options).unwrap().is_empty());
}

#[test]
fn file_templates_reject_non_utf8_boms_and_stop_each_line_at_nul() {
    let mut env = Env::new();
    let options = ConfigureOptions::default();
    let path = b"/work/template.in";
    for bom in [
        b"\xfe\xff".as_slice(),
        b"\xff\xfe",
        b"\x00\x00\xfe\xff",
        b"\xff\xfe\x00\x00",
    ] {
        assert_eq!(
            configure_file(bom, path, &mut env, options)
                .unwrap_err()
                .message,
            "File starts with a Byte-Order-Mark that is not UTF-8:\n\n  /work/template.in"
        );
    }
    assert_eq!(
        configure_file(
            b"\xef\xbb\xbftext\r\nx\0ignored\nz",
            path,
            &mut env,
            options
        )
        .unwrap()
        .as_slice(),
        b"\xef\xbb\xbftext\nx\nz\n"
    );
}
