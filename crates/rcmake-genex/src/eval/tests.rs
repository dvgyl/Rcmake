use std::collections::HashMap;

use rcmake_base::{BString, Limits};

use crate::{
    ArtifactKind, ArtifactPart, ConfigId, EvalContext, EvalFlags, EvalSite, GenexError, GenexHost,
    InterfaceView, TargetHandle, UsageTraversal, evaluate, parse,
};

#[path = "tests_artifacts.rs"]
mod tests_artifacts;
#[path = "tests_rules.rs"]
mod tests_rules;

const HEAD: TargetHandle = TargetHandle(1);
const OTHER: TargetHandle = TargetHandle(2);
const LEFT: TargetHandle = TargetHandle(3);
const RIGHT: TargetHandle = TargetHandle(4);

#[cfg(windows)]
const OBJECT_ROOTS: (&str, &str) = ("C:/src", "C:/build");
#[cfg(not(windows))]
const OBJECT_ROOTS: (&str, &str) = ("/src", "/out");

#[derive(Clone, Default)]
struct Host {
    properties: HashMap<(TargetHandle, BString), BString>,
    definitions: HashMap<BString, BString>,
    visits: Vec<TargetHandle>,
    link_queries: Vec<TargetHandle>,
    limits: Limits,
    warnings: Vec<BString>,
}

impl Host {
    fn set(&mut self, target: TargetHandle, property: &str, value: &str) {
        self.properties
            .insert((target, property.into()), value.into());
    }
}

impl GenexHost for Host {
    fn find_target(&self, name: &[u8]) -> Option<TargetHandle> {
        match name {
            b"head" => Some(HEAD),
            b"other" | b"alias" => Some(OTHER),
            b"left" => Some(LEFT),
            b"right" => Some(RIGHT),
            _ => None,
        }
    }

    fn target_name(&self, target: TargetHandle) -> BString {
        match target {
            HEAD => "head",
            OTHER => "other",
            LEFT => "left",
            RIGHT => "right",
            _ => "",
        }
        .into()
    }

    fn directory_policy(&self, policy: &[u8], _ctx: &EvalContext) -> Option<bool> {
        self.definitions.get(policy).map(|state| state == "NEW")
    }

    fn raw_property(&self, target: TargetHandle, name: &[u8]) -> Option<BString> {
        self.properties.get(&(target, name.into())).cloned()
    }

    fn transitive_property(
        &mut self,
        target: TargetHandle,
        name: &[u8],
        ctx: &EvalContext,
    ) -> Result<InterfaceView, GenexError> {
        self.visits.push(target);
        let ctx = EvalContext {
            current: Some(target),
            site: EvalSite {
                property: Some(name.into()),
                ..ctx.site.clone()
            },
            ..ctx.clone()
        };
        let raw = self.raw_property(target, name).unwrap_or_default();
        let (value, mut flags) = run(&raw, &ctx, self)?;
        flags.context_sensitive = true;
        Ok(InterfaceView { value, flags })
    }

    fn artifact(
        &self,
        target: TargetHandle,
        kind: ArtifactKind,
        part: ArtifactPart,
        config: &ConfigId,
    ) -> Option<BString> {
        if target != OTHER || !config.matches(b"Debug") {
            return None;
        }
        let value = match (kind, part) {
            (ArtifactKind::File, ArtifactPart::Full) => "/out/libother_d.so",
            (ArtifactKind::File, ArtifactPart::Dir) => "/out",
            (ArtifactKind::File, ArtifactPart::Name) => "libother_d.so",
            (ArtifactKind::File, ArtifactPart::BaseName) => "other_d",
            (ArtifactKind::File, ArtifactPart::Prefix) => "lib",
            (ArtifactKind::File, ArtifactPart::Suffix) => ".so",
            (ArtifactKind::LinkerFile, ArtifactPart::Full) => "/out/libother.so",
            (ArtifactKind::SonameFile, ArtifactPart::Name) => "libother.so.1",
            _ => return None,
        };
        Some(value.into())
    }

    fn link_closure(
        &mut self,
        target: TargetHandle,
        ctx: &EvalContext,
    ) -> Result<Vec<TargetHandle>, GenexError> {
        self.link_queries.push(target);
        if let Some(property) = self.raw_property(target, b"LINK_LIBRARIES") {
            let context = EvalContext {
                site: EvalSite {
                    property: Some("LINK_LIBRARIES".into()),
                    ..ctx.site.clone()
                },
                ..ctx.clone()
            };
            run(&property, &context, self)?;
        }
        Ok(Vec::new())
    }

    fn definition(&self, name: &[u8]) -> Option<BString> {
        if let Some(value) = self.definitions.get(name) {
            return Some(value.clone());
        }
        let value = match name {
            b"CMAKE_SYSTEM_NAME" => "Darwin",
            b"CMAKE_C_COMPILER_ID" => "Clang",
            b"CMAKE_CXX_COMPILER_ID" => "AppleClang",
            b"CMAKE_C_COMPILER_VERSION" => "18.1.2",
            b"CMAKE_CXX_COMPILER_VERSION" => "21.0.0",
            b"CMAKE_C_COMPILER_FRONTEND_VARIANT" | b"CMAKE_CXX_COMPILER_FRONTEND_VARIANT" => "GNU",
            b"CMAKE_CXX_COMPILE_FEATURES" => "cxx_std_17;cxx_std_20",
            _ => return None,
        };
        Some(value.into())
    }

    fn limits(&self) -> Limits {
        self.limits.clone()
    }

    fn target_objects(
        &self,
        target: TargetHandle,
        config: &ConfigId,
        sources: Option<&[u8]>,
    ) -> Option<BString> {
        (target == OTHER && config.matches(b"Debug")).then(|| {
            let (source_root, build_root) = OBJECT_ROOTS;
            let other = format!("{source_root}/other.c");
            let second = format!("{source_root}/second.c");
            match sources {
                Some(path) if path == other.as_bytes() => format!("{build_root}/other.o"),
                Some(path) if path == second.as_bytes() => format!("{build_root}/second.o"),
                Some(paths)
                    if paths != format!("{second};{other}").as_bytes()
                        && paths != format!("{other};{second}").as_bytes() =>
                {
                    String::new()
                }
                _ => format!("{build_root}/other.o;{build_root}/second.o"),
            }
            .into()
        })
    }

    fn timestamp(&self, format: Option<&[u8]>, utc: bool) -> Option<BString> {
        (format == Some(b"%Y") && utc).then(|| "2000".into())
    }

    fn warning(&mut self, body: BString, _ctx: &EvalContext) {
        self.warnings.push(body);
    }
}

fn context() -> EvalContext {
    EvalContext {
        config: ConfigId("Debug".into()),
        head: Some(HEAD),
        current: Some(HEAD),
        language: Some("CXX".into()),
        link_language: Some("C".into()),
        ..EvalContext::default()
    }
}

fn run(
    input: &[u8],
    ctx: &EvalContext,
    host: &mut Host,
) -> Result<(BString, EvalFlags), GenexError> {
    let ast = parse(input)?;
    let mut traced_host = host.clone();
    let mut trace = crate::GenexTrace::default();
    let traced = crate::evaluate_traced(&ast, ctx, &mut traced_host, &mut trace);
    let plain = evaluate(&ast, ctx, host);
    assert_eq!(
        format!("{traced:?}"),
        format!("{plain:?}"),
        "traced result differs"
    );
    plain
}

#[test]
fn target_lookup_tracking_records_property_and_artifact_references() -> Result<(), GenexError> {
    let mut host = Host::default();
    host.set(OTHER, "TYPE", "SHARED_LIBRARY");
    host.set(OTHER, "VALUE", "property");
    let ast = parse(
        concat!(
            "$<TARGET_EXISTS:left>;",
            "$<TARGET_NAME_IF_EXISTS:right>;",
            "$<TARGET_GENEX_EVAL:head,literal>;",
            "$<TARGET_PROPERTY:other,VALUE>;",
            "$<TARGET_FILE:alias>"
        )
        .as_bytes(),
    )?;
    let mut lookups = Vec::new();
    let (value, _) =
        crate::evaluate_with_target_lookups(&ast, &context(), &mut host, &mut lookups)?;
    assert_eq!(value, "1;right;literal;property;/out/libother_d.so");
    assert_eq!(lookups, [("other".into(), OTHER), ("alias".into(), OTHER)]);
    Ok(())
}

fn error_text(result: Result<(BString, EvalFlags), GenexError>) -> String {
    assert!(result.is_err(), "Expected an error. Got {result:?}.");
    result
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default()
}

fn check(cases: &[(&str, &str)], ctx: &EvalContext, host: &mut Host) {
    for (expr, expected) in cases {
        assert_eq!(
            run(expr.as_bytes(), ctx, host).map(|(value, _)| value),
            Ok((*expected).into()),
            "{expr}"
        );
    }
}

#[test]
fn lazy_branches_and_recovered_delimiters() {
    check(
        &[
            ("$<0:$<BAD>>$<1:a,b>", "a,b"),
            ("$<$<BOOL:ON>:selected>", "selected"),
            ("$<IF:0,$<BAD>,else>$<IF:1,then,$<BAD>>", "elsethen"),
            ("$<AND:0,$<BAD>>/$<OR:1,$<BAD>>/$<NOT:1>", "0/1/0"),
            ("$<A:$<1:yes>", "$<A:yes"),
            ("literal>:,$<incomplete", "literal>:,$<incomplete"),
            ("$<ANGLE-R>$<COMMA>$<SEMICOLON>$<QUOTE>", ">,;\""),
        ],
        &context(),
        &mut Host::default(),
    );
    for input in [
        b"$<>".as_slice(),
        b"$<0>",
        b"$<1>",
        b"$<IF:ON,no,yes>",
        b"$<BOOL:a,b>",
        b"$<OR:0,$<BAD>>",
    ] {
        assert!(matches!(
            run(input, &context(), &mut Host::default()),
            Err(GenexError::Message { .. })
        ));
    }
}

#[test]
fn boolean_arguments_preserve_short_circuit_and_error_order() {
    let ctx = context();
    let mut host = Host::default();
    check(
        &[("$<IF:$<ARGS:1;selected>,unused,unused>", "selected")],
        &ctx,
        &mut host,
    );
    for (expression, expected) in [
        (
            "$<IF:asdf,a,b>",
            "First parameter to $<IF> must resolve to exactly one '0' or '1' value.",
        ),
        (
            "$<IF:asdf,a>",
            "$<IF> expression requires 3 comma separated parameters, but got 2 instead.",
        ),
        (
            "$<AND:1,x>",
            "Parameters to $<AND> must resolve to either '0' or '1'.",
        ),
        (
            "$<OR:0,x>",
            "Parameters to $<OR> must resolve to either '0' or '1'.",
        ),
    ] {
        let error = error_text(run(expression.as_bytes(), &ctx, &mut host));
        assert_eq!(error, expected);
    }
}

#[test]
fn boolean_and_comparisons() {
    check(
        &[
            (
                "$<BOOL:00>/$<BOOL:0.0>/$<BOOL:OFF>/$<BOOL:>/$<BOOL:notfound>/$<BOOL:X-NOTFOUND>",
                "1/1/0/0/1/0",
            ),
            ("$<EQUAL:0x10,16>/$<EQUAL:012,10>/$<EQUAL:-1,1>", "1/1/0"),
            (
                "$<STREQUAL:a,A>/$<STRLESS:A,a>/$<STRGREATER:a,A>/$<STRLESS_EQUAL:a,a>/$<STRGREATER_EQUAL:a,b>",
                "0/1/1/1/0",
            ),
            (
                "$<VERSION_LESS:1.9,1.10>/$<VERSION_GREATER:2,1>/$<VERSION_EQUAL:1,1.0>/$<VERSION_LESS_EQUAL:1.0,1>/$<VERSION_GREATER_EQUAL:1,2>",
                "1/1/1/1/0",
            ),
        ],
        &context(),
        &mut Host::default(),
    );
    assert!(run(b"$<EQUAL:1x,1>", &context(), &mut Host::default()).is_err());
}

#[test]
fn string_operations() {
    check(
        &[
            (
                "$<UPPER_CASE:a,b:c>/$<LOWER_CASE:AB>/$<MAKE_C_IDENTIFIER:3+é>",
                "A,B:C/ab/_3___",
            ),
            (
                "$<STRING:LENGTH,é>/$<STRING:SUBSTRING,abcc,2,-1>/$<STRING:FIND,ababa,FROM:END,ba>",
                "2/cc/3",
            ),
            (
                "$<STRING:REPLACE,abcabc,b,B>/$<STRING:REPLACE,STRING,abc,,x>",
                "aBcaBc/abc",
            ),
            (
                r"$<STRING:REPLACE,REGEX,ab12cd34,([0-9]+),[\1]>",
                "ab[12]cd[34]",
            ),
            (
                "$<STRING:MATCH,aba,a>/$<STRING:MATCH,aaa,SEEK:ALL,a*>",
                "a/aaa;",
            ),
            (
                "$<STRING:APPEND,abc,d,e>/$<STRING:PREPEND,abc,d,e>/$<STRING:JOIN,-,a,,b>",
                "abcde/deabc/a--b",
            ),
            (
                "$<STRING:STRIP,SPACES,  a >/$<STRING:TOLOWER,ABC>/$<STRING:TOUPPER,abc>/$<STRING:MAKE_C_IDENTIFIER,2+3>",
                "a/abc/ABC/_2_3",
            ),
            (
                r"$<STRING:QUOTE,REGEX,a+b.{}>/$<STRING:ASCII,65,90>/$<STRING:HEX,é>",
                r"a\+b\.{}/AZ/c3a9",
            ),
            (
                "$<STRING:HASH,abc,ALGORITHM:MD5>",
                "900150983cd24fb0d6963f7d28e17f72",
            ),
            (
                "$<STRING:UUID,NAMESPACE:6ba7b810-9dad-11d1-80b4-00c04fd430c8,TYPE:SHA1,NAME:www.widgets.com,CASE:UPPER>",
                "21F7F8DE-8051-5B89-8680-0195EF798B6A",
            ),
            ("$<STRING:TIMESTAMP,UTC,%Y>", "2000"),
        ],
        &context(),
        &mut Host::default(),
    );
}

#[test]
#[cfg(target_os = "macos")]
fn seeded_random_matches_the_macos_oracle() {
    check(
        &[
            (
                "$<STRING:RANDOM,RANDOM_SEED:1,LENGTH:16>",
                "qoXEIfeJJ5bUMerU",
            ),
            (
                "$<STRING:RANDOM,RANDOM_SEED:123,LENGTH:16,ALPHABET:abcd>",
                "aadbcddccdadabca",
            ),
        ],
        &context(),
        &mut Host::default(),
    );
}

#[test]
fn list_operations() {
    check(
        &[
            (
                "$<IN_LIST:b,a;;b>/$<JOIN:a;;b,|>/$<LIST:JOIN,a;;b,|>",
                "1/a|b/a||b",
            ),
            (
                "$<REMOVE_DUPLICATES:;a;;b;>/$<FILTER:;a;;b;,INCLUDE,^$>",
                ";a;b/;;",
            ),
            (
                "$<LIST:LENGTH,a;;c>/$<LIST:LENGTH,>/$<LIST:FIND,a;;c,c>",
                "3/0/2",
            ),
            (
                "$<LIST:GET,a;;c,-1,0,1>/$<LIST:SUBLIST,a;;c,1,-1>",
                "c;a;/;c",
            ),
            (
                "$<LIST:APPEND,a,b;c>/$<LIST:PREPEND,c,a,b>/$<LIST:INSERT,a;c,1,b>",
                "a;b;c/a;b;c/a;b;c",
            ),
            (
                "$<LIST:POP_BACK,a;b>/$<LIST:POP_FRONT,a;b>/$<LIST:REMOVE_ITEM,a;b;a;c,a;c>",
                "a/b/b",
            ),
            (
                "$<LIST:REMOVE_AT,a;b;c;d,0;2,-1>/$<LIST:REMOVE_DUPLICATES,;a;;a;b>/$<LIST:REVERSE,a;;b>",
                "b/;a;b/b;;a",
            ),
            (
                "$<LIST:FILTER,a;ab;b,EXCLUDE,REGEX,^a>/$<LIST:FILTER,a;b,INCLUDE,^b>",
                "b/b",
            ),
            (
                "$<LIST:TRANSFORM,a;b;c,TOUPPER,FOR,0,-1,2>/$<LIST:TRANSFORM,a;b,APPEND,!,AT,0,0>",
                "A;b;C/a!!;b",
            ),
            (
                "$<LIST:TRANSFORM, a ; b ,STRIP>/$<LIST:TRANSFORM,a;b,PREPEND,!,REGEX,b>",
                "a;b/a;!b",
            ),
            (
                "$<LIST:TRANSFORM,a;b,REPLACE,a,x>/$<LIST:SORT,v2;v1;V3,CASE:INSENSITIVE,COMPARE:NATURAL>",
                "x;b/v1;v2;V3",
            ),
        ],
        &context(),
        &mut Host::default(),
    );
    for expr in [
        b"$<LIST:GET,,1>".as_slice(),
        b"$<LIST:SUBLIST,a;b,2,0>",
        b"$<LIST:SORT,a,CASE:SENSITIVE,CASE:INSENSITIVE>",
    ] {
        assert!(run(expr, &context(), &mut Host::default()).is_err());
    }
}

#[test]
fn list_callbacks_and_argument_expansion() {
    check(
        &[
            ("$<ARGS:a;;b>", "<ARGS>a,,b</ARGS>"),
            (
                "$<CONFIG:$<ARGS:Release;Debug>>/$<CONFIG:$<1:$<ARGS:Release;Debug>>>",
                "1/0",
            ),
            (
                "$<IF:$<ARGS:0;1>,a,b>/$<OR:$<ARGS:0;1>>/$<BOOL:$<ARGS:0;1>>",
                "a/1/0",
            ),
            (
                "$<LIST:GET,a;b,$<ARGS:1;0>>/$<LIST:APPEND,a,$<ARGS:>>",
                "b;a/a;",
            ),
            (
                "$<LIST:FILTER,a;b,INCLUDE,PREDICATE,$<STREQUAL:$<_0>,b>>",
                "b",
            ),
            (
                "$<LIST:TRANSFORM,a;b,APPLY,$<UPPER_CASE:$<_0>>,PREDICATE,$<STREQUAL:$<_0>,b>>",
                "a;B",
            ),
            ("$<LIST:TRANSFORM,a;b,APPLY,x$<_0>,AT,1,1,0>", "xa;xb"),
            (
                "$<LIST:SORT,B;a;C,COMPARATOR,$<STRLESS:$<_0>,$<_1>>,CASE:INSENSITIVE,ORDER:DESCENDING>",
                "C;B;a",
            ),
        ],
        &context(),
        &mut Host::default(),
    );
    for (expr, expected) in [
        (
            "$<_0>",
            "$<_0> may only be used inside the body of a binding operation.",
        ),
        (
            "$<LIST:TRANSFORM,a,APPLY,$<_1>>",
            "$<_1> is out of range for the current binding operation",
        ),
        (
            "$<LIST:TRANSFORM,,APPLY>",
            "sub-command TRANSFORM, action APPLY expects a <body> argument.",
        ),
        (
            "$<LIST:FILTER,a,INCLUDE,PREDICATE,x>",
            "PREDICATE body must evaluate to \"0\" or \"1\"",
        ),
        (
            "$<LIST:SORT,b;a,COMPARATOR,1>",
            "does not induce a strict weak ordering",
        ),
        (
            r"$<LIST:TRANSFORM,abc,REPLACE,(a),\2>",
            r#"replace expression "\2" contains an out-of-range escape for regex "(a)""#,
        ),
    ] {
        let error = error_text(run(expr.as_bytes(), &context(), &mut Host::default()));
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn path_operations() {
    check(
        &[
            ("$<PATH_EQUAL:/a//b,/a/b>/$<PATH_EQUAL:/a/../b,/b>", "1/0"),
            ("$<PATH:HAS_ROOT_NAME,/a>", "0"),
            ("$<PATH:HAS_ROOT_PATH,/a>", "1"),
            ("$<PATH:HAS_EXTENSION,a.tar.gz>", "1"),
            ("$<PATH:HAS_STEM,a.tar.gz>", "1"),
            ("$<PATH:HAS_RELATIVE_PART,/>", "0"),
            ("$<PATH:CMAKE_PATH,a>", "a"),
            ("$<PATH:NATIVE_PATH,a>", "a"),
            (
                "$<PATH:HAS_ROOT_DIRECTORY,/a>/$<PATH:HAS_FILENAME,/a/>/$<PATH:HAS_PARENT_PATH,/>/$<PATH:IS_ABSOLUTE,/a>/$<PATH:IS_RELATIVE,a>",
                if cfg!(windows) {
                    "1/0/1/0/1"
                } else {
                    "1/0/1/1/1"
                },
            ),
            (
                "$<PATH:IS_PREFIX,NORMALIZE,/a/../b,/b/c>/$<PATH:IS_PREFIX,,/a>",
                "1/0",
            ),
            (
                "$<PATH:GET_ROOT_NAME,/a;/b>/$<PATH:GET_FILENAME,/a/;/b>/$<PATH:GET_STEM,LAST_ONLY,a.tar.gz>/$<PATH:GET_EXTENSION,a.tar.gz>",
                ";/;b/a.tar/.tar.gz",
            ),
            (
                "$<PATH:GET_ROOT_PATH,/a>/$<PATH:GET_ROOT_DIRECTORY,/a>/$<PATH:GET_RELATIVE_PART,/a/b>/$<PATH:GET_PARENT_PATH,/a/b>",
                "////a/b//a",
            ),
            (
                "$<PATH:REMOVE_FILENAME,/a/b>/$<PATH:REPLACE_FILENAME,/a/,b>/$<PATH:REMOVE_EXTENSION,LAST_ONLY,a.tar.gz>/$<PATH:REPLACE_EXTENSION,foo,txt>",
                "/a///a//a.tar/foo.txt",
            ),
            (
                "$<PATH:APPEND,/a;/b,c;d,e>/$<PATH:APPEND,foo>/$<PATH:NORMAL_PATH,/a/../b>",
                "/a/c;d/e;/b/c;d/e/foo//b",
            ),
            (
                "$<PATH:RELATIVE_PATH,/a/c,/a/b>/$<PATH:ABSOLUTE_PATH,NORMALIZE,../c,/a/b>",
                "../c//a/c",
            ),
        ],
        &context(),
        &mut Host::default(),
    );
    #[cfg(not(windows))]
    check(
        &[
            ("$<SHELL_PATH:/a b;/b//c>", "/a b:/b//c"),
            (
                "$<PATH:CMAKE_PATH,NORMALIZE,/a/../b>/$<PATH:NATIVE_PATH,/a>",
                "/b//a",
            ),
        ],
        &context(),
        &mut Host::default(),
    );
    assert!(run(b"$<SHELL_PATH:relative>", &context(), &mut Host::default()).is_err());
}

#[test]
fn contexts_and_interface_operators() {
    let mut host = Host::default();
    let mut ctx = context();
    ctx.output_config = Some(ConfigId("Release".into()));
    ctx.command_config = Some(ConfigId("MinSizeRel".into()));
    ctx.site.property = Some("COMPILE_OPTIONS".into());
    check(
        &[
            (
                "$<CONFIG>/$<CONFIG:release,debug>/$<PLATFORM_ID>/$<PLATFORM_ID:Linux,Darwin>",
                "Debug/1/Darwin/1",
            ),
            (
                "$<OUTPUT_CONFIG:$<CONFIG>>/$<COMMAND_CONFIG:$<CONFIG>>",
                "Release/MinSizeRel",
            ),
            ("$<COMPILE_LANGUAGE>/$<COMPILE_LANGUAGE:C,CXX>", "CXX/1"),
            (
                "$<C_COMPILER_ID>/$<CXX_COMPILER_ID:Clang,AppleClang>/$<C_COMPILER_VERSION:18.1.2.0>/$<CXX_COMPILER_VERSION>",
                "Clang/1/1/21.0.0",
            ),
            (
                "$<COMPILE_LANG_AND_ID:CXX,AppleClang>/$<COMPILE_FEATURES:cxx_std_17,cxx_std_20>",
                "1/1",
            ),
            (
                "$<C_COMPILER_FRONTEND_VARIANT:GNU>/$<CXX_COMPILER_FRONTEND_VARIANT>",
                "1/GNU",
            ),
            (
                "$<BUILD_INTERFACE:a,b>/$<BUILD_LOCAL_INTERFACE:c>/$<INSTALL_INTERFACE:$<BAD>>",
                "a,b/c/",
            ),
        ],
        &ctx,
        &mut host,
    );
    ctx.site.property = Some("LINK_LIBRARIES".into());
    host.set(HEAD, "CMP0131", "NEW");
    check(
        &[("$<LINK_ONLY:link>/$<COMPILE_ONLY:compile>", "/compile")],
        &ctx,
        &mut host,
    );
    ctx.traversal = UsageTraversal::Link;
    check(
        &[("$<LINK_ONLY:link>/$<COMPILE_ONLY:compile>", "link/")],
        &ctx,
        &mut host,
    );
    ctx.site.property = Some("LINK_OPTIONS".into());
    check(
        &[
            ("$<HOST_LINK:-a,-b>/$<DEVICE_LINK:-c>", "-a;-b/"),
            (
                "$<COMPILE_LANGUAGE>/$<LINK_LANGUAGE>/$<LINK_LANGUAGE:CXX>/$<LINK_LANG_AND_ID:C,Clang>",
                "C/C/0/1",
            ),
        ],
        &ctx,
        &mut host,
    );
    assert!(run(b"$<DEVICE_LINK:$<BAD>>", &ctx, &mut host).is_err());
    ctx.device_link = true;
    check(
        &[(
            "$<DEVICE_LINK:-a>/$<HOST_LINK:-b>",
            "<DEVICE_LINK>;-a;</DEVICE_LINK>/",
        )],
        &ctx,
        &mut host,
    );
    assert!(run(b"$<INSTALL_PREFIX>", &ctx, &mut host).is_err());
}

#[test]
fn targets_and_second_pass_context() {
    let (source_root, build_root) = OBJECT_ROOTS;
    let mut host = Host::default();
    host.set(HEAD, "FOO", "first");
    host.set(HEAD, "CODE", "$<TARGET_PROPERTY:FOO>");
    host.set(OTHER, "FOO", "second");
    host.set(OTHER, "CODE", "$<BAD>");
    host.set(OTHER, "DEBUG_POSTFIX", "_d");
    host.set(HEAD, "CMP0099", "NEW");
    check(
        &[
            (
                "$<TARGET_EXISTS:head>/$<TARGET_EXISTS:no>/$<TARGET_NAME_IF_EXISTS:alias>/$<TARGET_NAME:absent>",
                "1/0/alias/absent",
            ),
            (
                "$<TARGET_POLICY:CMP0099>/$<TARGET_PROPERTY:FOO>/$<TARGET_PROPERTY:other,FOO>/$<TARGET_PROPERTY:other,MISSING>",
                "1/first/second/",
            ),
            (
                "$<GENEX_EVAL:$<TARGET_PROPERTY:CODE>>/$<TARGET_GENEX_EVAL:other,$<TARGET_PROPERTY:CODE>>",
                "first/second",
            ),
            (
                "$<TARGET_FILE:other>/$<TARGET_FILE_DIR:other>/$<TARGET_FILE_NAME:other>",
                "/out/libother_d.so//out/libother_d.so",
            ),
            (
                "$<TARGET_FILE_BASE_NAME:other>/$<TARGET_FILE_BASE_NAME:other,POSTFIX:EXCLUDE>/$<TARGET_FILE_PREFIX:other>/$<TARGET_FILE_SUFFIX:other>",
                "other_d/other/lib/.so",
            ),
            (
                "$<TARGET_LINKER_FILE:other>/$<TARGET_SONAME_FILE_NAME:other>",
                "/out/libother.so/libother.so.1",
            ),
            (
                &format!(
                    "$<TARGET_OBJECTS:other>/$<TARGET_OBJECTS:other,SOURCE_FILES:{source_root}/other.c>"
                ),
                &format!("{build_root}/other.o;{build_root}/second.o/{build_root}/other.o"),
            ),
        ],
        &context(),
        &mut host,
    );
    assert!(run(b"$<TARGET_NAME:$<1:head>>", &context(), &mut host).is_err());
}

#[test]
fn dependency_flags_follow_only_evaluated_expressions() {
    let mut host = Host::default();
    host.set(OTHER, "FOO", "other");
    let mut ctx = context();
    ctx.site.property = Some("LINK_LIBRARIES".into());
    for (input, expected) in [
        (
            "$<IF:0,$<TARGET_PROPERTY:FOO>,$<1:ok>>",
            EvalFlags::default(),
        ),
        (
            "$<TARGET_PROPERTY:FOO>",
            EvalFlags {
                head_sensitive: true,
                ..EvalFlags::default()
            },
        ),
        (
            "$<CONFIG>$<TARGET_PROPERTY:other,FOO>",
            EvalFlags {
                context_sensitive: true,
                ..EvalFlags::default()
            },
        ),
        (
            "$<LINK_LANGUAGE:C>",
            EvalFlags {
                context_sensitive: true,
                head_sensitive: true,
                link_language_sensitive: true,
            },
        ),
        (
            "$<TARGET_GENEX_EVAL:other,$<1:$<1:ok>>>",
            EvalFlags {
                context_sensitive: true,
                ..EvalFlags::default()
            },
        ),
    ] {
        assert_eq!(
            run(input.as_bytes(), &ctx, &mut host).map(|(_, flags)| flags),
            Ok(expected),
            "{input}"
        );
    }
}

#[test]
fn dag_allows_diamonds_and_reports_active_property_cycles() -> Result<(), GenexError> {
    let mut host = Host::default();
    let property = "INTERFACE_COMPILE_DEFINITIONS";
    host.set(HEAD, property, "$<TARGET_PROPERTY:left,INTERFACE_COMPILE_DEFINITIONS>;$<TARGET_PROPERTY:right,INTERFACE_COMPILE_DEFINITIONS>");
    for target in [LEFT, RIGHT] {
        host.set(
            target,
            property,
            "$<TARGET_PROPERTY:other,INTERFACE_COMPILE_DEFINITIONS>",
        );
    }
    host.set(OTHER, property, "$<TARGET_PROPERTY:FOO>");
    host.set(HEAD, "FOO", "from-head");
    let ctx = context();
    let input = b"$<TARGET_PROPERTY:head,INTERFACE_COMPILE_DEFINITIONS>";
    let (value, flags) = run(input, &ctx, &mut host)?;
    assert_eq!(value, "from-head;from-head");
    assert_eq!(host.visits, [HEAD, LEFT, OTHER, RIGHT, OTHER]);
    assert!(flags.head_sensitive && flags.context_sensitive);

    host.set(
        OTHER,
        property,
        "cycle;$<TARGET_PROPERTY:left,INTERFACE_COMPILE_DEFINITIONS>",
    );
    assert_eq!(run(input, &ctx, &mut host)?.0, "cycle;;cycle;");
    host.set(
        OTHER,
        property,
        "$<TARGET_PROPERTY:other,INTERFACE_COMPILE_DEFINITIONS>",
    );
    let error = error_text(run(input, &ctx, &mut host));
    assert!(error.starts_with("CMake Error:\n"));
    assert!(error.contains("$<TARGET_PROPERTY:other,INTERFACE_COMPILE_DEFINITIONS>"));
    assert!(error.contains("Self reference on target \"other\"."));
    host.set(OTHER, property, "recovered");
    assert_eq!(run(input, &ctx, &mut host)?.0, "recovered;recovered");
    Ok(())
}

#[test]
fn second_pass_detects_a_raw_property_cycle() {
    let mut host = Host::default();
    let expression = "$<GENEX_EVAL:$<TARGET_PROPERTY:head,CODE>>";
    host.set(HEAD, "CODE", expression);
    assert_eq!(
        run(expression.as_bytes(), &context(), &mut host),
        Err(GenexError::SelfReference {
            target: "head".into()
        })
    );
}

#[test]
fn recursion_and_expansion_are_bounded() {
    let mut host = Host::default();
    let ctx = context();
    host.limits.max_recursion = 3;
    assert_eq!(
        run(b"$<1:$<1:$<1:$<1:x>>>>", &ctx, &mut host),
        Err(GenexError::TooDeep)
    );
    host.limits.max_value_bytes = 10;
    assert!(run(b"hello world", &ctx, &mut host).is_err());
    assert!(run(b"$<JOIN:a;b;c,d1234>", &ctx, &mut host).is_err());
    assert!(
        run(
            b"$<1:x>$<1:x>$<1:x>$<1:x>$<1:x>$<1:x>$<1:x>$<1:x>$<1:x>$<1:x>$<1:x>",
            &ctx,
            &mut host
        )
        .is_err()
    );
    host.limits.max_value_bytes = 40;
    assert!(run(b"$<LIST:GET,abcdefghijkl,0,0,0,0>", &ctx, &mut host).is_err());
    host.limits.max_value_bytes = 1000;
    host.limits.regex_steps = 0;
    assert!(run(b"$<FILTER:a;b,INCLUDE,a>", &ctx, &mut host).is_err());
}

#[test]
fn transform_skips_action_and_selector_validation_for_empty_lists() {
    check(
        &[
            ("$<LIST:TRANSFORM,,BOGUS>", ""),
            ("$<LIST:TRANSFORM,,TOUPPER,AT,5>", ""),
            ("$<LIST:TRANSFORM,,TOUPPER,REGEX,[>", ""),
            ("$<LIST:TRANSFORM,,APPEND>", ""),
        ],
        &context(),
        &mut Host::default(),
    );
}

#[test]
fn remove_at_parses_all_indices_before_checking_bounds() {
    for (expression, expected) in [
        ("$<LIST:REMOVE_AT,,1>", "index: 1 out of range (0, 0)"),
        ("$<LIST:REMOVE_AT,,00>", "index: 0 out of range (0, 0)"),
        ("$<LIST:REMOVE_AT,,-1>", "index: -1 out of range (0, 0)"),
        (
            "$<LIST:REMOVE_AT,,1;bad>",
            "index: \"bad\" is not a valid index",
        ),
        (
            "$<LIST:REMOVE_AT,a,99;bad>",
            "index: \"bad\" is not a valid index",
        ),
    ] {
        let error = error_text(run(expression.as_bytes(), &context(), &mut Host::default()));
        assert!(error.contains(expected), "{error:?}");
    }
}

#[test]
fn config_rejects_only_an_invalid_first_name() {
    let mut host = Host::default();
    let ctx = context();
    assert_eq!(
        error_text(run(b"$<CONFIG:Foo-Bar>", &ctx, &mut host)),
        "Expression syntax not recognized."
    );
    assert!(host.warnings.is_empty());
    assert_eq!(
        run(b"$<CONFIG:Debug,Foo-Second>", &ctx, &mut host).map(|(value, _)| value),
        Ok("1".into())
    );
    assert_eq!(
        host.warnings,
        [BString::from(
            "Warning evaluating generator expression:\n  $<CONFIG:Debug,Foo-Second>\nThe config name of \"Foo-Second\" is invalid"
        )]
    );
}

#[test]
fn linker_language_property_is_rejected_in_static_link_and_source_properties() {
    let mut host = Host::default();
    host.set(OTHER, "TYPE", "STATIC_LIBRARY");
    host.set(OTHER, "LINKER_LANGUAGE", "C");
    for property in [
        "LINK_LIBRARIES",
        "INTERFACE_LINK_LIBRARIES",
        "SOURCES",
        "INTERFACE_SOURCES",
    ] {
        let mut ctx = context();
        ctx.site.property = Some(property.into());
        let error = run(b"$<TARGET_PROPERTY:other,LINKER_LANGUAGE>", &ctx, &mut host)
            .expect_err("The static library linker language is not available here.");
        assert_eq!(
            error.to_string(),
            "LINKER_LANGUAGE target property can not be used while evaluating link libraries for a static library"
        );
    }
    check(
        &[("$<TARGET_PROPERTY:other,LINKER_LANGUAGE>", "C")],
        &context(),
        &mut host,
    );
}

#[test]
fn target_file_queries_allow_other_targets_during_link_evaluation() {
    let mut host = Host::default();
    host.set(HEAD, "TYPE", "EXECUTABLE");
    host.set(OTHER, "TYPE", "SHARED_LIBRARY");
    for property in ["LINK_LIBRARIES", "SOURCES", "INTERFACE_SOURCES"] {
        let mut ctx = context();
        ctx.site.property = Some(property.into());
        let error = run(b"$<TARGET_FILE_NAME:head>", &ctx, &mut host)
            .expect_err("The head target artifact needs the linker language.");
        assert_eq!(
            error.to_string(),
            "Expressions which require the linker language may not be used while evaluating link libraries"
        );
        check(
            &[("$<TARGET_FILE:other>", "/out/libother_d.so")],
            &ctx,
            &mut host,
        );
    }
}

#[test]
fn missing_imported_artifact_follows_cmp0111() {
    let mut host = Host::default();
    host.set(LEFT, "TYPE", "STATIC_LIBRARY");
    host.set(LEFT, "IMPORTED", "TRUE");
    let expression = b"$<TARGET_FILE:left>";
    let ctx = context();
    assert_eq!(
        run(expression, &ctx, &mut host).expect("CMP0111 WARN").0,
        "left-NOTFOUND"
    );
    host.set(LEFT, "CMP0111", "OLD");
    assert_eq!(
        run(expression, &ctx, &mut host).expect("CMP0111 OLD").0,
        "left-NOTFOUND"
    );
    host.set(LEFT, "CMP0111", "NEW");
    assert_eq!(
        run(expression, &ctx, &mut host),
        Err(GenexError::Generation {
            message:
                "IMPORTED_LOCATION not set for imported target \"left\" configuration \"Debug\"."
                    .into(),
        })
    );
}
