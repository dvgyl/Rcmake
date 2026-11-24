use super::{
    EvalContext, HEAD, Host, LEFT, OTHER, RIGHT, UsageTraversal, check, context, error_text, run,
};
use rcmake_diag::{Frame, FrameKind};

use crate::{
    ArtifactKind, ArtifactPart, BString, ConfigId, GenexError, GenexHost, InterfaceView,
    TargetHandle, evaluate, parse,
};

struct DllHost(Host);

impl GenexHost for DllHost {
    fn find_target(&self, name: &[u8]) -> Option<TargetHandle> {
        self.0.find_target(name)
    }

    fn target_name(&self, target: TargetHandle) -> BString {
        self.0.target_name(target)
    }

    fn raw_property(&self, target: TargetHandle, name: &[u8]) -> Option<BString> {
        self.0.raw_property(target, name)
    }

    fn transitive_property(
        &mut self,
        target: TargetHandle,
        name: &[u8],
        ctx: &EvalContext,
    ) -> Result<InterfaceView, GenexError> {
        self.0.transitive_property(target, name, ctx)
    }

    fn artifact(
        &self,
        target: TargetHandle,
        kind: ArtifactKind,
        part: ArtifactPart,
        _: &ConfigId,
    ) -> Option<BString> {
        let name = match (target, kind) {
            (OTHER, ArtifactKind::LinkerLibraryFile | ArtifactKind::File)
            | (RIGHT, ArtifactKind::File) => "shared.dll",
            (LEFT, ArtifactKind::File) => "module.dll",
            (LEFT, ArtifactKind::LinkerLibraryFile)
                if self
                    .raw_property(target, b"TYPE")
                    .is_some_and(|kind| kind == "STATIC_LIBRARY") =>
            {
                "static.lib"
            }
            _ => return None,
        };
        match part {
            ArtifactPart::Full => Some(format!("/out/{name}").into()),
            ArtifactPart::Name => Some(name.into()),
            ArtifactPart::Dir => Some("/out".into()),
            _ => None,
        }
    }

    fn link_closure(
        &mut self,
        _: TargetHandle,
        _: &EvalContext,
    ) -> Result<Vec<TargetHandle>, GenexError> {
        Ok(vec![OTHER, RIGHT, OTHER, LEFT])
    }

    fn definition(&self, name: &[u8]) -> Option<BString> {
        self.0.definition(name)
    }
}

#[test]
fn dll_platform_suppresses_linker_library_paths_for_shared_and_module_targets() {
    let mut host = DllHost(Host::default());
    host.0
        .definitions
        .insert("CMAKE_SYSTEM_NAME".into(), "Windows".into());
    host.0.set(OTHER, "TYPE", "SHARED_LIBRARY");
    host.0.set(LEFT, "TYPE", "MODULE_LIBRARY");
    let ctx = context();
    for (suffix, shared, module) in [
        ("", "/out/shared.dll", "/out/module.dll"),
        ("_NAME", "shared.dll", "module.dll"),
        ("_DIR", "/out", "/out"),
    ] {
        for (name, expected) in [("other", shared), ("left", module)] {
            let expression = format!("$<TARGET_LINKER_LIBRARY_FILE{suffix}:{name}>");
            let value = |host: &mut DllHost| {
                evaluate(&parse(expression.as_bytes())?, &ctx, host).map(|(value, _)| value)
            };
            host.0
                .definitions
                .insert("CMAKE_IMPORT_LIBRARY_SUFFIX".into(), ".lib".into());
            assert_eq!(value(&mut host), Ok(BString::default()), "{expression}");
            host.0
                .definitions
                .insert("CMAKE_IMPORT_LIBRARY_SUFFIX".into(), "".into());
            assert_eq!(value(&mut host), Ok(expected.into()), "{expression}");
        }
    }
    host.0
        .definitions
        .insert("CMAKE_IMPORT_LIBRARY_SUFFIX".into(), ".lib".into());
    host.0.set(LEFT, "TYPE", "STATIC_LIBRARY");
    let result = parse(b"$<TARGET_LINKER_LIBRARY_FILE:left>")
        .and_then(|ast| evaluate(&ast, &ctx, &mut host));
    assert_eq!(result.map(|(value, _)| value), Ok("/out/static.lib".into()));
}

#[test]
fn runtime_dll_lists_keep_distinct_dependency_targets_with_the_same_file() {
    let mut host = DllHost(Host::default());
    host.0
        .definitions
        .insert("CMAKE_SYSTEM_NAME".into(), "CustomToolchain".into());
    host.0
        .definitions
        .insert("CMAKE_IMPORT_LIBRARY_SUFFIX".into(), ".implib".into());
    host.0.set(HEAD, "TYPE", "EXECUTABLE");
    host.0.set(OTHER, "TYPE", "SHARED_LIBRARY");
    host.0.set(RIGHT, "TYPE", "SHARED_LIBRARY");
    host.0.set(LEFT, "TYPE", "MODULE_LIBRARY");
    let ctx = context();
    for (expr, expected) in [
        (
            "$<TARGET_RUNTIME_DLLS:head>",
            "/out/shared.dll;/out/shared.dll",
        ),
        ("$<TARGET_RUNTIME_DLL_DIRS:head>", "/out"),
    ] {
        let result = parse(expr.as_bytes()).and_then(|ast| evaluate(&ast, &ctx, &mut host));
        assert_eq!(
            result.map(|(value, _)| value),
            Ok(expected.into()),
            "{expr}"
        );
    }
}

#[test]
fn runtime_dll_queries_validate_targets_and_evaluate_links_on_darwin() {
    let mut host = Host::default();
    let ctx = context();
    host.set(HEAD, "TYPE", "EXECUTABLE");
    host.set(OTHER, "TYPE", "SHARED_LIBRARY");
    host.set(LEFT, "TYPE", "MODULE_LIBRARY");
    for operator in ["TARGET_RUNTIME_DLLS", "TARGET_RUNTIME_DLL_DIRS"] {
        for name in ["head", "other", "left"] {
            let expression = format!("$<{operator}:{name}>");
            check(&[(&expression, "")], &ctx, &mut host);
        }
        for kind in [
            "STATIC_LIBRARY",
            "OBJECT_LIBRARY",
            "INTERFACE_LIBRARY",
            "UTILITY",
        ] {
            host.set(RIGHT, "TYPE", kind);
            let expression = format!("$<{operator}:right>");
            assert_eq!(
                error_text(run(expression.as_bytes(), &ctx, &mut host)),
                "Objects of target \"right\" referenced but is not one of the allowed target types (EXECUTABLE, SHARED, MODULE)."
            );
        }
        for args in ["", ":head,other"] {
            let expression = format!("$<{operator}{args}>");
            assert_eq!(
                error_text(run(expression.as_bytes(), &ctx, &mut host)),
                format!("$<{operator}> expression requires exactly one parameter.")
            );
        }
        let expression = format!("$<{operator}:missing>");
        assert_eq!(
            error_text(run(expression.as_bytes(), &ctx, &mut host)),
            "Objects of target \"missing\" referenced but no such target exists."
        );
    }
    assert_eq!(host.link_queries, [HEAD, OTHER, LEFT, HEAD, OTHER, LEFT]);

    host.set(HEAD, "LINK_LIBRARIES", "$<TARGET_RUNTIME_DLLS:other>");
    check(&[("$<TARGET_RUNTIME_DLLS:head>", "")], &ctx, &mut host);
    host.set(OTHER, "LINK_LIBRARIES", "$<TARGET_RUNTIME_DLL_DIRS:head>");
    assert_eq!(
        error_text(run(b"$<TARGET_RUNTIME_DLLS:head>", &ctx, &mut host)),
        "The SOURCES of \"head\" use a generator expression that depends on the SOURCES themselves."
    );
}

#[test]
fn artifact_queries_validate_the_target_before_naming_options() {
    let mut host = Host::default();
    let ctx = context();
    host.set(OTHER, "TYPE", "SHARED_LIBRARY");
    host.set(RIGHT, "TYPE", "OBJECT_LIBRARY");
    host.set(HEAD, "TYPE", "EXECUTABLE");
    host.set(LEFT, "TYPE", "STATIC_LIBRARY");
    check(
        &[
            ("$<TARGET_IMPORT_FILE:other>", ""),
            ("$<TARGET_IMPORT_FILE_DIR:other>", ""),
            ("$<TARGET_IMPORT_FILE_SUFFIX:other>", ""),
            ("$<TARGET_IMPORT_FILE_BASE_NAME:other,POSTFIX:EXCLUDE>", ""),
            ("$<TARGET_LINKER_IMPORT_FILE:other>", ""),
            ("$<TARGET_SONAME_IMPORT_FILE:other>", ""),
            ("$<TARGET_IMPORT_FILE:left>", ""),
        ],
        &ctx,
        &mut host,
    );
    for (expr, expected) in [
        (
            "$<TARGET_FILE_PREFIX:>",
            "Expression syntax not recognized.",
        ),
        (
            "$<TARGET_FILE_BASE_NAME:right,bad>",
            "Target \"right\" is not an executable or library.",
        ),
        (
            "$<TARGET_FILE_BASE_NAME:missing,bad>",
            "No target \"missing\"",
        ),
        (
            "$<TARGET_FILE_BASE_NAME:other,POSTFIX:EXCLUDE,extra>",
            "Unexpected parameters, require one or two parameters.",
        ),
        (
            "$<TARGET_FILE_BASE_NAME:other,bad>",
            "Wrong second parameter: \"POSTFIX:INCLUDE\" or \"POSTFIX:EXCLUDE\" is expected",
        ),
        (
            "$<TARGET_LINKER_LIBRARY_FILE:head>",
            "TARGET_LINKER_LIBRARY_FILE is allowed only for libraries with ENABLE_EXPORTS.",
        ),
        (
            "$<TARGET_LINKER_LIBRARY_FILE_NAME:head>",
            "TARGET_LINKER_LIBRARY_FILE is allowed only for libraries with ENABLE_EXPORTS.",
        ),
        (
            "$<TARGET_LINKER_IMPORT_FILE:head>",
            "TARGET_LINKER_IMPORT_FILE is allowed only for libraries and executables with ENABLE_EXPORTS.",
        ),
        (
            "$<TARGET_SONAME_FILE:left>",
            "TARGET_SONAME_FILE is allowed only for SHARED libraries.",
        ),
        (
            "$<TARGET_PDB_FILE:left>",
            "TARGET_PDB_FILE is not supported by the target linker.",
        ),
        (
            "$<TARGET_PDB_FILE_DIR:left>",
            "TARGET_PDB_FILE is not supported by the target linker.",
        ),
        (
            "$<TARGET_BUNDLE_DIR:other>",
            "TARGET_BUNDLE_DIR is allowed only for Bundle targets.",
        ),
    ] {
        assert_eq!(
            error_text(run(expr.as_bytes(), &ctx, &mut host)),
            expected,
            "{expr}"
        );
    }
    host.set(OTHER, "IMPORTED", "TRUE");
    assert_eq!(
        error_text(run(b"$<TARGET_PDB_FILE_BASE_NAME:other>", &ctx, &mut host)),
        "TARGET_PDB_FILE_BASE_NAME not allowed for IMPORTED targets."
    );
}

#[test]
fn object_query_filters_require_absolute_sources_from_the_target() {
    let mut host = Host::default();
    let ctx = context();
    host.set(OTHER, "TYPE", "OBJECT_LIBRARY");
    host.set(OTHER, "SOURCE_DIR", "/src");
    host.set(OTHER, "SOURCES", "other.c;second.c;/src/header.h");
    check(
        &[
            (
                "$<TARGET_OBJECTS:other,SOURCE_FILES:/src/second.c,SOURCE_FILES:/src/other.c>",
                "/out/other.o;/out/second.o",
            ),
            ("$<TARGET_OBJECTS:other,SOURCE_FILES:/src/header.h>", ""),
        ],
        &ctx,
        &mut host,
    );
    for (expr, expected) in [
        (
            "$<TARGET_OBJECTS:other,BAD,EXTRA>",
            "Unrecognized argument:\n\n  BAD",
        ),
        (
            "$<TARGET_OBJECTS:other,SOURCE_FILES:other.c>",
            "Source file:\n\n  other.c\n\nis not an absolute path",
        ),
        (
            "$<TARGET_OBJECTS:other,SOURCE_FILES:/src/missing.c>",
            "Source file:\n\n  /src/missing.c\n\ndoes not exist for target \"other\"",
        ),
    ] {
        assert_eq!(
            error_text(run(expr.as_bytes(), &ctx, &mut host)),
            expected,
            "{expr}"
        );
    }
    host.set(OTHER, "IMPORTED", "TRUE");
    assert_eq!(
        error_text(run(
            b"$<TARGET_OBJECTS:other,SOURCE_FILES:other.c>",
            &ctx,
            &mut host
        )),
        "Cannot use SOURCE_FILES argument on imported target \"other\""
    );
}

#[test]
fn target_property_uses_the_consumer_or_defaults_to_the_selected_target() {
    let mut host = Host::default();
    host.set(HEAD, "LABEL", "consumer");
    host.set(OTHER, "LABEL", "owner");
    host.set(
        OTHER,
        "INTERFACE_COMPILE_DEFINITIONS",
        "$<TARGET_PROPERTY:LABEL>",
    );
    host.set(OTHER, "SCOPE:LABEL", "named");
    host.set(OTHER, "INTERFACE_LINK_LIBRARIES", "$<1:dep>");
    let ctx = context();
    let no_consumer = EvalContext {
        head: None,
        current: None,
        ..ctx.clone()
    };
    let expression = "$<TARGET_PROPERTY:other,INTERFACE_COMPILE_DEFINITIONS>";
    check(
        &[
            (expression, "consumer"),
            ("$<TARGET_PROPERTY:other,SCOPE:LABEL>", "named"),
        ],
        &ctx,
        &mut host,
    );
    check(&[(expression, "owner")], &no_consumer, &mut host);
    let link_query = "$<TARGET_PROPERTY:other,INTERFACE_LINK_LIBRARIES>";
    host.definitions.insert("CMP0189".into(), "OLD".into());
    check(&[(link_query, "$<1:dep>")], &ctx, &mut host);
    host.definitions.insert("CMP0189".into(), "NEW".into());
    check(&[(link_query, "dep")], &ctx, &mut host);
    host.set(
        OTHER,
        "INTERFACE_LINK_LIBRARIES",
        "$<LINK_LIBRARY:feature,head>",
    );
    let mut other_property = ctx.clone();
    other_property.site.property = Some("SOURCES".into());
    check(
        &[(
            link_query,
            "<LINK_LIBRARY:feature>;head;</LINK_LIBRARY:feature>",
        )],
        &other_property,
        &mut host,
    );
    for (expr, expected) in [
        (
            "$<TARGET_PROPERTY:,>",
            "$<TARGET_PROPERTY:tgt,prop> expression requires a non-empty target name and property name.",
        ),
        (
            "$<TARGET_PROPERTY:bad/name,>",
            "Target name and property name not supported.",
        ),
        (
            "$<TARGET_PROPERTY:bad/name,OK>",
            "Target name not supported.",
        ),
        (
            "$<TARGET_PROPERTY:other,bad/name>",
            "Property name not supported.",
        ),
        (
            "$<TARGET_PROPERTY:other,>",
            "$<TARGET_PROPERTY:...> expression requires a non-empty property name.",
        ),
        (
            "$<TARGET_PROPERTY:missing,bad/name>",
            "Target \"missing\" not found.",
        ),
    ] {
        assert_eq!(
            error_text(run(expr.as_bytes(), &ctx, &mut host)),
            expected,
            "{expr}"
        );
    }
}

#[test]
fn custom_target_requires_an_explicit_target_for_a_property_read() {
    let mut host = Host::default();
    host.set(HEAD, "TYPE", "UTILITY");
    host.set(HEAD, "LABEL", "utility");
    let mut ctx = context();
    check(&[("$<TARGET_PROPERTY:LABEL>", "utility")], &ctx, &mut host);
    ctx.site.backtrace = ctx.site.backtrace.push(Frame {
        file: b"CMakeLists.txt".as_slice().into(),
        line: 4,
        name: Some(b"add_custom_target".as_slice().into()),
        kind: FrameKind::Command,
    });
    check(
        &[("$<TARGET_PROPERTY:head,LABEL>", "utility")],
        &ctx,
        &mut host,
    );
    assert!(
        error_text(run(b"$<TARGET_PROPERTY:LABEL>", &ctx, &mut host))
            .contains("$<TARGET_PROPERTY:prop> may only be used with binary targets.")
    );
}

#[test]
fn link_features_require_link_libraries_and_validate_members() {
    let mut host = Host::default();
    let mut ctx = context();
    ctx.traversal = UsageTraversal::Link;
    ctx.site.property = Some("LINK_OPTIONS".into());
    assert!(run(b"$<LINK_GROUP:RESCAN>", &ctx, &mut host).is_err());
    ctx.site.property = Some("INTERFACE_LINK_LIBRARIES_DIRECT_EXCLUDE".into());
    assert_eq!(
        error_text(run(b"$<LINK_LIBRARY:DEFAULT,other>", &ctx, &mut host)),
        "$<LINK_LIBRARY:...> may only be used with binary targets to specify link libraries through 'LINK_LIBRARIES', 'INTERFACE_LINK_LIBRARIES', and 'INTERFACE_LINK_LIBRARIES_DIRECT' properties."
    );
    ctx.site.property = Some("LINK_LIBRARIES".into());
    check(
        &[
            ("$<LINK_LIBRARY:DEFAULT>", ""),
            ("$<LINK_LIBRARY:invalid:without_members>", ""),
            (
                "$<LINK_LIBRARY:;DEFAULT,other>",
                "<LINK_LIBRARY:DEFAULT>;other;</LINK_LIBRARY:DEFAULT>",
            ),
            (
                "$<LINK_LIBRARY:DEFAULT,a;$<LINK_LIBRARY:DEFAULT,b>>",
                "<LINK_LIBRARY:DEFAULT>;a;b;</LINK_LIBRARY:DEFAULT>",
            ),
            (
                "$<LINK_GROUP:RESCAN,a,b>",
                "<LINK_GROUP:RESCAN:a|b>;a;b;</LINK_GROUP:RESCAN>",
            ),
            (
                "$<LINK_GROUP:RESCAN,$<LINK_LIBRARY:DEFAULT,other>>",
                "<LINK_GROUP:RESCAN:<LINK_LIBRARY:DEFAULT>|other|</LINK_LIBRARY:DEFAULT>>;<LINK_LIBRARY:DEFAULT>;other;</LINK_LIBRARY:DEFAULT>;</LINK_GROUP:RESCAN>",
            ),
        ],
        &ctx,
        &mut host,
    );
    for (expression, expected) in [
        (
            "$<LINK_GROUP:,>",
            "$<LINK_GROUP:...> expects a feature name as first argument.",
        ),
        (
            "$<LINK_GROUP:bad:name,a>",
            "The feature name 'bad:name' contains invalid characters.",
        ),
        (
            "$<LINK_GROUP:RESCAN,$<LINK_GROUP:RESCAN,a>>",
            "$<LINK_GROUP:...> cannot be nested.",
        ),
        (
            "$<LINK_LIBRARY:DEFAULT,$<LINK_LIBRARY:OTHER,a>>",
            "$<LINK_LIBRARY:...> with different features cannot be nested.",
        ),
        (
            "$<LINK_LIBRARY:DEFAULT,$<LINK_GROUP:RESCAN,a>>",
            "$<LINK_GROUP:...> cannot be nested inside a $<LINK_LIBRARY:...> expression.",
        ),
    ] {
        let error = error_text(run(expression.as_bytes(), &ctx, &mut host));
        assert_eq!(error, expected);
    }
    host.set(HEAD, "LINK_LIBRARIES_ONLY_TARGETS", "ON");
    check(
        &[
            (
                "$<LINK_GROUP:RESCAN,other,-lexternal,/sdk/lib.a>",
                "<LINK_GROUP:RESCAN:other|-lexternal|/sdk/lib.a>;other;-lexternal;/sdk/lib.a;</LINK_GROUP:RESCAN>",
            ),
            (
                "$<IF:$<BOOL:$<LINK_GROUP:RESCAN,external>>,other,other>",
                "other",
            ),
        ],
        &ctx,
        &mut host,
    );
    let error = error_text(run(b"$<LINK_GROUP:RESCAN,external>", &ctx, &mut host));
    assert!(error.contains("LINK_LIBRARIES_ONLY_TARGETS"));
}

#[test]
fn link_library_uses_attributes_for_the_selected_language() {
    let mut host = Host::default();
    let mut ctx = context();
    ctx.traversal = UsageTraversal::Link;
    ctx.site.property = Some("LINK_LIBRARIES".into());
    host.definitions.insert(
        "CMAKE_LINK_LIBRARY_feat_ATTRIBUTES".into(),
        "DEDUPLICATION=YES,NO".into(),
    );
    host.definitions.insert(
        "CMAKE_C_LINK_LIBRARY_feat_ATTRIBUTES".into(),
        "LIBRARY_TYPE=STATIC,SHARED;OVERRIDE=DEFAULT".into(),
    );
    let expr = b"$<LINK_LIBRARY:feat,other>";
    assert!(run(expr, &ctx, &mut host).is_ok());
    ctx.link_language = Some("CXX".into());
    let error = error_text(run(expr, &ctx, &mut host));
    assert!(error.contains("Erroneous option(s) for 'CMAKE_LINK_LIBRARY_feat_ATTRIBUTES'"));
    assert!(error.contains("DEDUPLICATION=YES,NO"));
    host.definitions.insert(
        "CMAKE_LINK_LIBRARY_DEFAULT_ATTRIBUTES".into(),
        "BAD=VALUE".into(),
    );
    assert!(run(b"$<LINK_LIBRARY:DEFAULT,other>", &ctx, &mut host).is_ok());
}

#[test]
fn target_property_link_recursion_uses_the_current_property_role() {
    let mut host = Host::default();
    let mut ctx = context();
    ctx.site.property = Some("INTERFACE_LINK_LIBRARIES".into());
    let error = error_text(run(
        b"$<TARGET_PROPERTY:other,INCLUDE_DIRECTORIES>",
        &ctx,
        &mut host,
    ));
    assert!(error.contains("transitive over the link libraries, creating a recursion"));
    assert!(host.visits.is_empty());

    let error = error_text(run(
        b"$<TARGET_PROPERTY:missing,INCLUDE_DIRECTORIES>",
        &ctx,
        &mut host,
    ));
    assert!(error.contains("Target \"missing\" not found."));

    host.set(HEAD, "CODE", "$<TARGET_PROPERTY:other,INCLUDE_DIRECTORIES>");
    host.set(OTHER, "INCLUDE_DIRECTORIES", "/inc");
    check(
        &[("$<GENEX_EVAL:$<TARGET_PROPERTY:CODE>>", "/inc")],
        &ctx,
        &mut host,
    );

    ctx.site.property = Some("SOURCES".into());
    assert!(
        run(
            b"$<TARGET_PROPERTY:other,INCLUDE_DIRECTORIES>",
            &ctx,
            &mut host,
        )
        .is_ok()
    );
}
