//! Generation diagnostics that follow the CMake 4.5.1 RunCMake suites.

use super::*;
use rcmake_host::Stream;

fn fixture() -> Result<(ProjectModel, PureHost), Box<dyn std::error::Error>> {
    let root = if cfg!(windows) { "C:/work" } else { "/work" };
    let host = PureHost::new(Path::new(root).to_path_buf(), Path::new(root).join("cmake"));
    let source = format!("{root}/src");
    host.seed_file(Path::new(&format!("{source}/main.c")), b"")?;
    let model = project_with_roots(&source, &format!("{root}/build"));
    Ok((model, host))
}

fn stderr(host: &PureHost) -> String {
    String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).into_owned()
}

#[test]
fn own_relative_include_directory_names_the_consumer() -> TestResult {
    let (mut model, host) = fixture()?;
    let mut app = target("app", TargetKind::Executable, &["main.c"]);
    app.usage
        .push(UsageField::IncludeDirectories, entry("relative"));
    model.add_target(app)?;
    assert!(matches!(
        resolve(&model, &host, &[]),
        Err(ResolveError::Reported(_))
    ));
    let text = stderr(&host);
    assert!(
        text.contains("Found relative path while evaluating include directories of \"app\":"),
        "{text}"
    );
    assert!(text.contains("\"relative\""), "{text}");
    Ok(())
}

#[test]
fn evaluation_errors_of_each_target_are_reported() -> TestResult {
    let (mut model, host) = fixture()?;
    for name in ["first", "second"] {
        let mut lib = target(name, TargetKind::StaticLibrary, &["main.c"]);
        lib.usage.push(
            UsageField::IncludeDirectories,
            entry(&format!(
                "$<TARGET_PROPERTY:{name}_missing/x,INCLUDE_DIRECTORIES>"
            )),
        );
        model.add_target(lib)?;
    }
    assert!(matches!(
        resolve(&model, &host, &[]),
        Err(ResolveError::Reported(_))
    ));
    let text = stderr(&host);
    assert!(text.contains("first_missing/x"), "{text}");
    assert!(text.contains("second_missing/x"), "{text}");
    Ok(())
}

#[test]
fn invalid_language_standard_is_an_error_at_the_target() -> TestResult {
    let (mut model, host) = fixture()?;
    let mut app = target("app", TargetKind::Executable, &["main.c"]);
    app.props.set("C_STANDARD".into(), "bad".into());
    model.add_target(app)?;
    let error = resolve(&model, &host, &[]).expect_err("The standard is invalid.");
    let text = error.to_string();
    assert!(text.starts_with("CMake Error at "), "{text}");
    assert!(
        text.contains("C_STANDARD is set to invalid value 'bad'"),
        "{text}"
    );
    Ok(())
}

#[test]
fn relative_link_directory_follows_cmp0081() -> TestResult {
    for status in [PolicyStatus::New, PolicyStatus::Warn, PolicyStatus::Old] {
        let (mut model, host) = fixture()?;
        let mut app = target("app", TargetKind::Executable, &["main.c"]);
        app.usage.push(UsageField::LinkDirectories, entry("../lib"));
        app.policies.policies.set(PolicyId::new(81), status)?;
        model.add_target(app)?;
        let result = resolve(&model, &host, &[]);
        assert_eq!(result.is_err(), status == PolicyStatus::New);
        let text = stderr(&host);
        let found = "Found relative path while evaluating link directories of \"app\":";
        assert_eq!(text.contains(found), status != PolicyStatus::Old, "{text}");
        assert_eq!(
            text.contains("Policy CMP0081 is not set"),
            status == PolicyStatus::Warn,
            "{text}"
        );
    }
    Ok(())
}

#[test]
fn missing_link_targets_are_reported_in_target_order() -> TestResult {
    let (mut model, host) = fixture()?;
    let mut iface = target("iface", TargetKind::InterfaceLibrary, &[]);
    iface.usage.interface_link_libraries = Some(vec![link_item("config::iface")]);
    model.add_target(iface)?;
    let mut lib = target("impl", TargetKind::StaticLibrary, &["main.c"]);
    lib.usage.link_libraries = Some(vec![link_item("config::impl"), link_item("iface")]);
    model.add_target(lib)?;
    assert!(matches!(
        resolve(&model, &host, &[]),
        Err(ResolveError::Reported(_))
    ));
    let text = stderr(&host);
    let interface = text
        .find("The link interface of target \"iface\" contains:")
        .ok_or("missing interface error")?;
    let implementation = text
        .find("Target \"impl\" links to:")
        .ok_or("missing link error")?;
    assert!(interface < implementation, "{text}");
    assert!(
        text.contains("  * An ALIAS target is missing.\n\n"),
        "{text}"
    );
    Ok(())
}

#[test]
fn old_cmp0108_drops_a_self_link_through_an_alias() -> TestResult {
    let (mut model, host) = fixture()?;
    let mut lib = target("foo", TargetKind::SharedLibrary, &["main.c"]);
    lib.policies
        .policies
        .set(PolicyId::new(108), PolicyStatus::Old)?;
    lib.usage.link_libraries = Some(vec![link_item("Bar::foo")]);
    let id = model.add_target(lib)?;
    model.add_alias(DirId::ROOT, "Bar::foo".into(), id)?;
    resolve(&model, &host, &[])?;
    Ok(())
}

#[test]
fn framework_feature_names_a_framework_target_by_bundle() -> TestResult {
    let (mut model, host) = fixture()?;
    model.generator = Some(rcmake_model::GeneratorKind::Ninja);
    for (name, value) in [
        (
            "CMAKE_C_LINK_LIBRARY_USING_FRAMEWORK",
            "-framework <LIBRARY>",
        ),
        ("CMAKE_C_LINK_LIBRARY_USING_FRAMEWORK_SUPPORTED", "TRUE"),
        ("CMAKE_SHARED_LIBRARY_RUNTIME_C_FLAG", "-Wl,-rpath,"),
        ("CMAKE_SKIP_BUILD_RPATH", "OFF"),
    ] {
        model.global.props.set(name.into(), value.into());
    }
    let mut framework = target("Kit", TargetKind::SharedLibrary, &["main.c"]);
    framework.props.set("FRAMEWORK".into(), "TRUE".into());
    model.add_target(framework)?;
    let mut app = target("app", TargetKind::Executable, &["main.c"]);
    app.usage.link_libraries = Some(vec![link_item("$<LINK_LIBRARY:FRAMEWORK,Kit>")]);
    model.add_target(app)?;
    let result = resolve(&model, &host, &[])?;
    let BuildStep::Link {
        invocations,
        inputs,
        ..
    } = link_build_step(&result.graphs[0], "app")?
    else {
        return Err("Expected a link command.".into());
    };
    let argv: Vec<_> = invocations
        .iter()
        .flat_map(|invocation| invocation.argv.iter())
        .collect();
    let at = argv
        .iter()
        .position(|arg| *arg == "-framework")
        .ok_or("missing -framework")?;
    assert_eq!(
        argv.get(at + 1).map(|arg| arg.as_slice()),
        Some(b"Kit".as_slice())
    );
    // The link step still depends on the framework binary.
    assert!(
        inputs
            .iter()
            .any(|input| input.ends_with(b"Kit.framework/Versions/A/Kit")),
        "{inputs:?}"
    );
    Ok(())
}

#[test]
fn file_set_membership_follows_cmp0211_at_generation() -> TestResult {
    for status in [PolicyStatus::New, PolicyStatus::Warn, PolicyStatus::Old] {
        let (mut model, host) = fixture()?;
        let mut lib = target("lib", TargetKind::StaticLibrary, &["main.c"]);
        lib.policies.policies.set(PolicyId::new(211), status)?;
        let id = model.add_target(lib)?;
        for name in ["s1", "s2"] {
            model.add_file_set(rcmake_model::FileSet {
                target: id,
                name: name.into(),
                kind: rcmake_model::FileSetKind::Sources,
                scope: rcmake_model::UsageScope::Private,
                base_dirs: Vec::new(),
                files: vec![entry("main.c")],
                props: Default::default(),
            })?;
        }
        let result = resolve(&model, &host, &[]);
        assert_eq!(result.is_err(), status == PolicyStatus::New);
        let text = stderr(&host);
        // The file set and the SOURCES entry both conflict with s1.
        assert_eq!(
            text.matches("already belongs to file set \"s1\".").count(),
            if status == PolicyStatus::Old { 0 } else { 2 },
            "{text}"
        );
        assert_eq!(
            text.contains("Policy CMP0211 is not set"),
            status == PolicyStatus::Warn
        );
    }
    Ok(())
}

#[test]
fn cmp0128_optional_warning_names_the_old_behavior() -> TestResult {
    for (standard, extensions, expected) in [
        (Some("17"), None, "unnecessary flags for"),
        (None, Some("OFF"), "be disabled."),
    ] {
        let (mut model, host) = fixture()?;
        for (name, value) in [
            ("CMAKE_C_STANDARD_DEFAULT", "17"),
            ("CMAKE_C_EXTENSIONS_DEFAULT", "ON"),
            ("CMAKE_POLICY_WARNING_CMP0128", "ON"),
        ] {
            model.global.props.set(name.into(), value.into());
        }
        let mut lib = target("lib", TargetKind::StaticLibrary, &["main.c"]);
        // CMake also warns when the project sets the policy to OLD.
        lib.policies
            .policies
            .set(PolicyId::new(128), PolicyStatus::Old)?;
        if let Some(standard) = standard {
            lib.props.set("C_STANDARD".into(), standard.into());
        }
        if let Some(extensions) = extensions {
            lib.props.set("C_EXTENSIONS".into(), extensions.into());
        }
        model.add_target(lib)?;
        resolve(&model, &host, &[])?;
        let text = stderr(&host);
        assert!(
            text.contains("Policy CMP0128 is not set: Selection of language standard"),
            "{text}"
        );
        assert!(text.contains(expected), "{text}");
    }
    Ok(())
}

#[test]
fn cmp0211_checks_only_the_file_sets_active_in_each_configuration() -> TestResult {
    for overlap in [false, true] {
        let (mut model, host) = fixture()?;
        let mut lib = target("lib", TargetKind::StaticLibrary, &[]);
        lib.policies
            .policies
            .set(PolicyId::new(211), PolicyStatus::New)?;
        let id = model.add_target(lib)?;
        for (name, expression) in [
            ("s1", "$<$<CONFIG:Debug>:main.c>"),
            (
                "s2",
                if overlap {
                    "$<$<CONFIG:Debug>:./main.c>"
                } else {
                    "$<$<CONFIG:Release>:main.c>"
                },
            ),
        ] {
            model.add_file_set(FileSet {
                target: id,
                name: name.into(),
                kind: FileSetKind::Sources,
                scope: UsageScope::Private,
                base_dirs: Vec::new(),
                files: vec![entry(expression)],
                props: Default::default(),
            })?;
        }
        let configs = if overlap {
            vec![ConfigId("Debug".into())]
        } else {
            vec![ConfigId("Debug".into()), ConfigId("Release".into())]
        };
        let result = resolve(&model, &host, &configs);
        assert_eq!(result.is_err(), overlap, "{result:?}");
        assert_eq!(
            stderr(&host).contains("already belongs to file set \"s1\""),
            overlap
        );
    }
    Ok(())
}

#[test]
fn cmp0128_new_unset_extensions_defaults_to_standard_option() -> TestResult {
    let (mut model, host) = fixture()?;
    model.global.props.set(
        "CMAKE_Cc_std_11_STANDARD_COMPILE_OPTION".into(),
        "-std=c11".into(),
    );
    model.global.props.set(
        "CMAKE_Cc_std_11_EXTENSION_COMPILE_OPTION".into(),
        "-std=gnu11".into(),
    );
    let mut lib = target("lib", TargetKind::StaticLibrary, &["main.c"]);
    lib.policies
        .policies
        .set(PolicyId::new(128), PolicyStatus::New)?;
    lib.props.set("C_STANDARD".into(), "c_std_11".into());
    lib.props.set("C_STANDARD_REQUIRED".into(), "ON".into());
    model.add_target(lib)?;

    let result = resolve(&model, &host, &[])?;
    let compile = result.graphs[0]
        .steps
        .iter()
        .find_map(|step| match step {
            BuildStep::Compile { invocation, .. } => Some(&invocation.argv),
            _ => None,
        })
        .ok_or("Missing compile step")?;
    assert!(compile.contains(&BString::from("-std=c11")), "{compile:?}");
    assert!(
        !compile.contains(&BString::from("-std=gnu11")),
        "{compile:?}"
    );
    Ok(())
}

#[test]
fn visual_studio_short_intdir_uses_local_binary_directory() -> TestResult {
    let (mut model, host) = fixture()?;
    model.generator = Some(rcmake_model::GeneratorKind::VisualStudio(
        rcmake_model::VsVersion::Vs17,
    ));
    model.cache.insert(
        "CMAKE_INTERMEDIATE_DIR_STRATEGY".into(),
        rcmake_model::CacheEntry {
            value: "SHORT".into(),
            ty: rcmake_model::CacheType::String,
            help: BString::default(),
            advanced: false,
            props: Default::default(),
        },
    );
    let root_src = model.dir(DirId::ROOT).source_dir.clone();
    let root_bin = model.dir(DirId::ROOT).binary_dir.clone();
    // sub62 and sub347 share the 4-hex SHA3-512 prefix 0226; foo and bar share no collision
    // unless both targets use the build root instead of their local binary directories.
    let dir_a = model.add_directory(
        DirId::ROOT,
        format!("{root_src}/sub62").into(),
        format!("{root_bin}/sub62").into(),
        Backtrace::default(),
    )?;
    let dir_b = model.add_directory(
        DirId::ROOT,
        format!("{root_src}/sub347").into(),
        format!("{root_bin}/sub347").into(),
        Backtrace::default(),
    )?;
    host.seed_file(Path::new(&format!("{root_src}/sub62/a.c")), b"")?;
    host.seed_file(Path::new(&format!("{root_src}/sub347/b.c")), b"")?;
    let mut t1 = target("Collides0", TargetKind::StaticLibrary, &["a.c"]);
    t1.dir = dir_a;
    let mut t2 = target("Collides0_other", TargetKind::StaticLibrary, &["b.c"]);
    t2.dir = dir_b;
    // Use two names that collide on the 4-hex SHA3-512 target hash.
    t1.name = "t0".into();
    t2.name = "t2565".into();
    // Verify both dirs hash identically to prove the local-dir prefix avoids collision.
    let hash_a = crate::lower::artifacts::short_support_dir(&model, &t1);
    let hash_b = crate::lower::artifacts::short_support_dir(&model, &t2);
    assert!(hash_a.starts_with(b"sub62/.o/"), "{hash_a}");
    assert!(hash_b.starts_with(b"sub347/.o/"), "{hash_b}");
    model.add_target(t1)?;
    model.add_target(t2)?;
    let _ = resolve(&model, &host, &[])?;
    let text = stderr(&host);
    assert!(
        !text.contains("share an intermediate directory"),
        "unexpected VS cross-directory collision warning: {text}"
    );
    Ok(())
}

#[test]
fn imported_link_feature_warnings_run_before_override_and_follow_group_predicate() -> TestResult {
    // 1. LINK_LIBRARY warning runs even when consumer sets LINK_LIBRARY_OVERRIDE_<item>=DEFAULT.
    {
        let (mut model, host) = fixture()?;
        let mut imp_leaf = target("NS::lib", TargetKind::StaticLibrary, &[]);
        imp_leaf.origin = TargetOrigin::Imported(ImportedVisibility::Global);
        imp_leaf
            .props
            .set("IMPORTED_LOCATION".into(), "/work/libfoo.a".into());
        model.add_target(imp_leaf)?;

        let mut imp = target("NS::lib2", TargetKind::SharedLibrary, &[]);
        imp.origin = TargetOrigin::Imported(ImportedVisibility::Global);
        imp.props
            .set("IMPORTED_LOCATION".into(), "/work/libbar.so".into());
        imp.usage.interface_link_libraries =
            Some(vec![link_item("$<LINK_LIBRARY:custom_feat,NS::lib>")]);
        model.add_target(imp)?;

        let mut app = target("app", TargetKind::SharedLibrary, &["main.c"]);
        app.usage.link_libraries = Some(vec![link_item("NS::lib2")]);
        app.props
            .set("LINK_LIBRARY_OVERRIDE_NS::lib".into(), "DEFAULT".into());
        model.add_target(app)?;

        let _ = resolve(&model, &host, &[])?;
        let text = stderr(&host);
        let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            collapsed.contains("The 'IMPORTED' target 'NS::lib2' uses the generator-expression '$<LINK_LIBRARY>' with the feature 'custom_feat'"),
            "{text}"
        );
    }

    // 2. LINK_GROUP omits author warning when generic _SUPPORTED is ON even if per-lang is OFF,
    //    while still reporting the fatal unsupported-feature error.
    {
        let (mut model, host) = fixture()?;
        model.global.props.set(
            "CMAKE_C_LINK_GROUP_USING_feat_SUPPORTED".into(),
            "FALSE".into(),
        );
        model.global.props.set(
            "CMAKE_LINK_GROUP_USING_feat_SUPPORTED".into(),
            "TRUE".into(),
        );
        model
            .global
            .props
            .set("CMAKE_LINK_GROUP_USING_feat".into(), "a;b".into());
        let mut imp_leaf = target("NS::lib", TargetKind::StaticLibrary, &[]);
        imp_leaf.origin = TargetOrigin::Imported(ImportedVisibility::Global);
        imp_leaf
            .props
            .set("IMPORTED_LOCATION".into(), "/work/libfoo.a".into());
        model.add_target(imp_leaf)?;

        let mut imp = target("NS::lib2", TargetKind::SharedLibrary, &[]);
        imp.origin = TargetOrigin::Imported(ImportedVisibility::Global);
        imp.props
            .set("IMPORTED_LOCATION".into(), "/work/libbar.so".into());
        imp.usage.interface_link_libraries = Some(vec![link_item("$<LINK_GROUP:feat,NS::lib>")]);
        model.add_target(imp)?;

        let mut app = target("app", TargetKind::SharedLibrary, &["main.c"]);
        app.usage.link_libraries = Some(vec![link_item("NS::lib2")]);
        model.add_target(app)?;

        let result = resolve(&model, &host, &[]);
        assert!(result.is_err(), "expected fatal unsupported-feature error");
        let text = stderr(&host);
        assert!(
            !text.contains("The 'IMPORTED' target 'NS::lib2'"),
            "unexpected LINK_GROUP author warning when generic support is ON: {text}"
        );
        assert!(
            text.contains("Feature 'feat', specified through generator-expression '$<LINK_GROUP>'"),
            "{text}"
        );
    }
    Ok(())
}

#[test]
fn notfound_include_directory_reports_target_property_error() -> TestResult {
    let (mut model, host) = fixture()?;
    let mut app = target("app", TargetKind::Executable, &["main.c"]);
    app.usage.push(
        UsageField::IncludeDirectories,
        entry("MISSING_DIR-NOTFOUND"),
    );
    model.add_target(app)?;

    let err = resolve(&model, &host, &[]).expect_err("expected NOTFOUND include error");
    let text = format!("{err}\n{}", stderr(&host));
    assert!(text.contains("MISSING_DIR"), "{text}");
    assert!(
        text.contains("used as include directory in directory"),
        "{text}"
    );
    Ok(())
}

#[test]
fn compile_only_adds_custom_target_dependency() -> TestResult {
    let (mut model, host) = fixture()?;
    let custom = target("gen_headers", TargetKind::Utility, &[]);
    let custom_id = model.add_target(custom)?;
    let mut app = target("app", TargetKind::Executable, &["main.c"]);
    app.usage.link_libraries = Some(vec![link_item("$<COMPILE_ONLY:gen_headers>")]);
    model.add_target(app)?;

    let result = resolve(&model, &host, &[])?;
    let graph = result.graphs.first().ok_or("missing graph")?;
    let app_node = graph
        .targets
        .iter()
        .find(|t| t.name == "app")
        .ok_or("missing app target")?;
    let custom_index = graph
        .targets
        .iter()
        .position(|t| t.name == "gen_headers")
        .ok_or("missing custom target")?;
    assert_eq!(custom_id.index(), 0);
    assert!(
        app_node.depends.contains(&custom_index),
        "expected app to depend on gen_headers via COMPILE_ONLY: {:?}",
        app_node.depends
    );
    Ok(())
}

#[test]
fn cmp0187_old_matches_earlier_generated_source_with_extension() -> TestResult {
    let (mut model, host) = fixture()?;
    let mut policies = rcmake_diag::PolicyMap::default();
    let _ = policies.set(PolicyId::new(115), PolicyStatus::New);
    let _ = policies.set(PolicyId::new(187), PolicyStatus::Old);
    model.dir_mut(DirId::ROOT).final_policies = Some(policies);
    for out in ["z.h", "z"] {
        let mut cmd = rcmake_model::CustomCommand::new(
            DirId::ROOT,
            rcmake_model::CustomCommandKind::Output,
            Default::default(),
            policies,
        );
        cmd.outputs.push(entry(out));
        cmd.command_lines.push(vec!["true".into()]);
        let _ = model.add_command(cmd);
    }
    let mut app = target("app", TargetKind::Executable, &["main.c", "z"]);
    app.policies.policies = policies;
    model.add_target(app)?;

    let result = resolve(&model, &host, &[])?;
    let graph = result.graphs.first().ok_or("missing graph")?;
    let app_node = graph
        .targets
        .iter()
        .find(|t| t.name == "app")
        .ok_or("missing app")?;
    let outputs: Vec<&[u8]> = app_node
        .steps
        .iter()
        .filter_map(|&idx| match graph.steps.get(idx)? {
            BuildStep::Custom(step) => Some(step.outputs.iter().map(|o| o.as_slice())),
            _ => None,
        })
        .flatten()
        .collect();
    assert!(
        outputs.iter().any(|o| o.ends_with(b"z.h")),
        "expected extensionless 'z' under CMP0187 OLD to match earlier 'z.h': {:?}",
        outputs
            .iter()
            .map(|o| String::from_utf8_lossy(o))
            .collect::<Vec<_>>()
    );
    Ok(())
}

#[test]
fn cmp0199_and_cmp0200_emit_policy_warnings() -> TestResult {
    let (mut model, host) = fixture()?;
    model
        .dir_mut(DirId::ROOT)
        .vars
        .insert("CMAKE_POLICY_WARNING_CMP0199".into(), "ON".into());
    model
        .dir_mut(DirId::ROOT)
        .vars
        .insert("CMAKE_POLICY_WARNING_CMP0200".into(), "ON".into());

    let mut imp = target("imp", TargetKind::StaticLibrary, &[]);
    imp.origin = TargetOrigin::Imported(ImportedVisibility::Global);
    imp.props
        .set("IMPORTED_CONFIGURATIONS".into(), "RELEASE".into());
    imp.props
        .set("IMPORTED_LOCATION".into(), "/work/libimp.a".into());
    imp.props.set(
        "IMPORTED_LOCATION_RELEASE".into(),
        "/work/libimp_r.a".into(),
    );
    imp.usage.push(
        UsageField::InterfaceCompileDefinitions,
        entry("$<$<CONFIG:Debug>:IMP_DBG>"),
    );
    model.add_target(imp)?;

    let mut app = target("app", TargetKind::Executable, &["main.c"]);
    app.usage.link_libraries = Some(vec![link_item("imp")]);
    model.add_target(app)?;

    let _ = resolve(&model, &host, &[rcmake_genex::ConfigId("Debug".into())])?;
    let text = stderr(&host);
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        collapsed.contains("Policy CMP0199 is not set: $<CONFIG:cfgs> does not match mapped configurations that are not selected."),
        "{text}"
    );
    assert!(
        collapsed.contains("Policy CMP0200 is not set: Location and configuration selection for imported targets is more consistent."),
        "{text}"
    );
    Ok(())
}

#[test]
fn explicit_full_path_source_precedes_later_generated_output() -> TestResult {
    let (mut model, host) = fixture()?;
    let root = if cfg!(windows) { "C:/work" } else { "/work" };
    let src_dup = format!("{root}/src/dup.c");
    let bin_dup = format!("{root}/build/dup.c");
    host.seed_file(Path::new(&src_dup), b"int dup(void) { return 1; }\n")?;

    // Record the explicit full-path source file before the generated binary-dir output.
    model
        .dir_mut(DirId::ROOT)
        .source_props
        .entry(src_dup.as_str().into())
        .or_default();
    let mut cmd = rcmake_model::CustomCommand::new(
        DirId::ROOT,
        rcmake_model::CustomCommandKind::Output,
        Default::default(),
        Default::default(),
    );
    cmd.outputs.push(entry("dup.c"));
    cmd.command_lines.push(vec!["true".into()]);
    let _ = model.add_command(cmd);
    model
        .dir_mut(DirId::ROOT)
        .source_props
        .entry(bin_dup.as_str().into())
        .or_default()
        .set("GENERATED".into(), "1".into());

    model.add_target(target("app", TargetKind::Executable, &["dup.c"]))?;
    let result = resolve(&model, &host, &[])?;
    let graph = result.graphs.first().ok_or("missing graph")?;
    let app_node = graph
        .targets
        .iter()
        .find(|t| t.name == "app")
        .ok_or("missing app")?;
    let compiled: Vec<&[u8]> = app_node
        .steps
        .iter()
        .filter_map(|&idx| match graph.steps.get(idx)? {
            BuildStep::Compile { source, .. } => Some(source.as_slice()),
            _ => None,
        })
        .collect();
    assert!(
        compiled.iter().any(|p| p.ends_with(b"/src/dup.c")),
        "expected relative 'dup.c' to resolve to earlier explicit source path: {:?}",
        compiled
            .iter()
            .map(|p| String::from_utf8_lossy(p))
            .collect::<Vec<_>>()
    );
    assert!(
        !app_node
            .steps
            .iter()
            .any(|&idx| matches!(graph.steps.get(idx), Some(BuildStep::Custom(_)))),
        "did not expect generated custom command step when explicit source wins"
    );
    Ok(())
}

#[test]
fn multi_config_traced_custom_commands_propagate_byproducts_and_utility_deps() -> TestResult {
    let (mut model, host) = fixture()?;
    model.generator = Some(rcmake_model::GeneratorKind::NinjaMultiConfig);
    let echo_id = model.add_target(target("echo", TargetKind::Utility, &[]))?;
    let u1_id = model.add_target(target("u1", TargetKind::Utility, &[]))?;
    let u2_id = model.add_target(target("u2", TargetKind::Utility, &[]))?;

    let mut c1 = rcmake_model::CustomCommand::new(
        DirId::ROOT,
        rcmake_model::CustomCommandKind::Output,
        Default::default(),
        Default::default(),
    );
    c1.outputs.push(entry("$<$<CONFIG:Debug>:dbg.txt>"));
    c1.depends.push(entry("echo"));
    c1.command_lines.push(vec!["true".into()]);
    let _ = model.add_command(c1);

    let mut c2 = rcmake_model::CustomCommand::new(
        DirId::ROOT,
        rcmake_model::CustomCommandKind::Output,
        Default::default(),
        Default::default(),
    );
    c2.outputs.push(entry("$<$<CONFIG:Debug>:dbgx.txt>"));
    c2.byproducts.push(entry("byprod_$<CONFIG>.txt"));
    c2.depends.push(entry("echo"));
    c2.command_lines.push(vec!["true".into()]);
    let _ = model.add_command(c2);

    for (owner, dep) in [(u1_id, "dbg.txt"), (u2_id, "dbgx.txt")] {
        let mut util = rcmake_model::CustomCommand::new(
            DirId::ROOT,
            rcmake_model::CustomCommandKind::Utility(owner),
            Default::default(),
            Default::default(),
        );
        util.depends.push(entry(dep));
        let _ = model.add_command(util);
    }

    let configs = [
        rcmake_genex::ConfigId("Debug".into()),
        rcmake_genex::ConfigId("Release".into()),
    ];
    let result = resolve(&model, &host, &configs)?;
    let release = result.graphs.get(1).ok_or("missing Release graph")?;
    let echo_idx = release
        .targets
        .iter()
        .position(|t| t.name == "echo")
        .ok_or("missing echo")?;
    let u1 = release
        .targets
        .iter()
        .find(|t| t.name == "u1")
        .ok_or("missing u1")?;
    let u2 = release
        .targets
        .iter()
        .find(|t| t.name == "u2")
        .ok_or("missing u2")?;
    assert_eq!(echo_id.index(), 0);
    assert!(
        u1.depends.contains(&echo_idx),
        "expected u1 in Release to keep traced utility dep on echo: {:?}",
        u1.depends
    );
    assert!(
        u2.depends.contains(&echo_idx),
        "expected u2 in Release to depend on echo: {:?}",
        u2.depends
    );
    let has_byprod = u2.steps.iter().any(|&idx| {
        matches!(
            release.steps.get(idx),
            Some(BuildStep::Custom(step))
                if step.outputs.is_empty()
                    && step
                        .byproducts
                        .iter()
                        .any(|b| b.as_slice().ends_with(b"byprod_Release.txt"))
        )
    });
    assert!(
        has_byprod,
        "expected u2 in Release to attach byproduct-only traced custom step"
    );
    Ok(())
}

#[test]
fn swift_link_rule_expands_swift_placeholders() -> TestResult {
    let (mut model, host) = fixture()?;
    let root = if cfg!(windows) { "C:/work" } else { "/work" };
    host.seed_file(Path::new(&format!("{root}/src/main.swift")), b"print(1)\n")?;
    model
        .global
        .props
        .set("ENABLED_LANGUAGES".into(), "Swift".into());
    let vars = &mut model.dir_mut(DirId::ROOT).vars;
    vars.insert("CMAKE_Swift_COMPILER".into(), "/usr/bin/swiftc".into());
    vars.insert("CMAKE_Swift_LINKER_PREFERENCE".into(), "50".into());
    vars.insert("CMAKE_Swift_SOURCE_FILE_EXTENSIONS".into(), "swift".into());
    vars.insert("CMAKE_Swift_COMPILE_OBJECT".into(), ":".into());
    vars.insert(
        "CMAKE_Swift_LINK_EXECUTABLE".into(),
        "<CMAKE_Swift_COMPILER> -emit-executable -o <TARGET> -module-name <SWIFT_MODULE_NAME> -emit-module-path <SWIFT_MODULE> <DEFINES> <INCLUDES> <SWIFT_SOURCES>".into(),
    );
    let mut app = target("app", TargetKind::Executable, &["main.swift"]);
    app.props.set("Swift_MODULE_NAME".into(), "DemoMod".into());
    model.add_target(app)?;

    let result = resolve(&model, &host, &[])?;
    let graph = result.graphs.first().ok_or("missing graph")?;
    let link_cmd = graph
        .steps
        .iter()
        .find_map(|step| match step {
            BuildStep::Link { invocations, .. } => Some(
                invocations
                    .iter()
                    .map(|inv| String::from_utf8_lossy(&inv.shell).into_owned())
                    .collect::<Vec<_>>()
                    .join(" && "),
            ),
            _ => None,
        })
        .ok_or("missing link step")?;
    assert!(link_cmd.contains("main.swift"), "{link_cmd}");
    assert!(link_cmd.contains("DemoMod"), "{link_cmd}");
    assert!(link_cmd.contains("DemoMod.swiftmodule"), "{link_cmd}");
    for placeholder in [
        "<DEFINES>",
        "<INCLUDES>",
        "<SWIFT_SOURCES>",
        "<SWIFT_MODULE_NAME>",
        "<SWIFT_MODULE>",
    ] {
        assert!(
            !link_cmd.contains(placeholder),
            "unexpanded {placeholder} in {link_cmd}"
        );
    }
    Ok(())
}

#[test]
fn swift_ios_static_library_receives_sdk_and_target_triple() -> TestResult {
    let (mut model, host) = fixture()?;
    let root = if cfg!(windows) { "C:/work" } else { "/work" };
    host.seed_file(
        Path::new(&format!("{root}/src/main.swift")),
        b"public struct S {}\n",
    )?;
    model
        .global
        .props
        .set("ENABLED_LANGUAGES".into(), "Swift".into());
    let vars = &mut model.dir_mut(DirId::ROOT).vars;
    vars.insert("CMAKE_SYSTEM_NAME".into(), "iOS".into());
    vars.insert("APPLE".into(), "1".into());
    vars.insert("CMAKE_OSX_ARCHITECTURES".into(), "arm64".into());
    vars.insert(
        "CMAKE_OSX_SYSROOT".into(),
        "/sdks/iPhoneSimulator.sdk".into(),
    );
    vars.insert("CMAKE_OSX_DEPLOYMENT_TARGET".into(), "17.0".into());
    vars.insert("CMAKE_Swift_COMPILER".into(), "/usr/bin/swiftc".into());
    vars.insert("CMAKE_Swift_SYSROOT_FLAG".into(), "-sdk".into());
    vars.insert("CMAKE_Swift_LINKER_PREFERENCE".into(), "50".into());
    vars.insert("CMAKE_Swift_SOURCE_FILE_EXTENSIONS".into(), "swift".into());
    vars.insert("CMAKE_Swift_COMPILE_OBJECT".into(), ":".into());
    vars.insert(
        "CMAKE_Swift_CREATE_STATIC_LIBRARY".into(),
        "<CMAKE_Swift_COMPILER> -emit-library -static -o <TARGET> <FLAGS> <SWIFT_SOURCES>".into(),
    );
    model.add_target(target("lib", TargetKind::StaticLibrary, &["main.swift"]))?;

    let result = resolve(&model, &host, &[])?;
    let graph = result.graphs.first().ok_or("missing graph")?;
    let link_cmd = graph
        .steps
        .iter()
        .find_map(|step| match step {
            BuildStep::Link { invocations, .. } => Some(
                invocations
                    .iter()
                    .map(|inv| String::from_utf8_lossy(&inv.shell).into_owned())
                    .collect::<Vec<_>>()
                    .join(" && "),
            ),
            _ => None,
        })
        .ok_or("missing link step")?;
    assert!(
        link_cmd.contains("-sdk /sdks/iPhoneSimulator.sdk"),
        "{link_cmd}"
    );
    assert!(
        link_cmd.contains("-target arm64-apple-ios17.0-simulator"),
        "{link_cmd}"
    );
    assert!(
        !link_cmd.contains("-mios-simulator-version-min"),
        "{link_cmd}"
    );
    Ok(())
}

#[test]
fn swift_one_step_link_includes_non_swift_objects_defines_includes_and_output_name() -> TestResult {
    let (mut model, host) = fixture()?;
    let root = if cfg!(windows) { "C:/work" } else { "/work" };
    host.seed_file(
        Path::new(&format!("{root}/src/main.swift")),
        b"public func f() {}\n",
    )?;
    host.seed_file(
        Path::new(&format!("{root}/src/helper.c")),
        b"int helper(void) { return 1; }\n",
    )?;
    model.generator = Some(rcmake_model::GeneratorKind::NinjaMultiConfig);
    model
        .global
        .props
        .set("ENABLED_LANGUAGES".into(), "C;Swift".into());
    let vars = &mut model.dir_mut(DirId::ROOT).vars;
    vars.insert("CMAKE_C_COMPILER".into(), "/usr/bin/cc".into());
    vars.insert("CMAKE_C_SOURCE_FILE_EXTENSIONS".into(), "c".into());
    vars.insert(
        "CMAKE_C_COMPILE_OBJECT".into(),
        "<CMAKE_C_COMPILER> -c <SOURCE> -o <OBJECT>".into(),
    );
    vars.insert("CMAKE_Swift_COMPILER".into(), "/usr/bin/swiftc".into());
    vars.insert("CMAKE_Swift_LINKER_PREFERENCE".into(), "50".into());
    vars.insert("CMAKE_Swift_SOURCE_FILE_EXTENSIONS".into(), "swift".into());
    vars.insert("CMAKE_Swift_COMPILE_OBJECT".into(), ":".into());
    vars.insert(
        "CMAKE_Swift_CREATE_SHARED_LIBRARY".into(),
        "<CMAKE_Swift_COMPILER> -emit-library -o <TARGET> -module-name <SWIFT_MODULE_NAME> -module-link-name <SWIFT_LIBRARY_NAME> -emit-module-path <SWIFT_MODULE> <DEFINES> <INCLUDES> <FLAGS> <SWIFT_SOURCES>".into(),
    );
    let mut lib = target(
        "my-lib",
        TargetKind::SharedLibrary,
        &["main.swift", "helper.c"],
    );
    lib.props.set("OUTPUT_NAME".into(), "custom_swift".into());
    lib.usage.push(
        UsageField::CompileDefinitions,
        BT::new("SWIFT_DEF=1".into(), Default::default()),
    );
    lib.usage.push(
        UsageField::IncludeDirectories,
        BT::new(format!("{root}/inc").into(), Default::default()),
    );
    model.add_target(lib)?;

    let configs = [ConfigId("Release".into())];
    let result = resolve(&model, &host, &configs)?;
    let graph = result.graphs.first().ok_or("missing graph")?;
    let compile_sources: Vec<_> = graph
        .steps
        .iter()
        .filter_map(|step| match step {
            BuildStep::Compile { source, .. } => Some(source.as_slice()),
            _ => None,
        })
        .collect();
    assert_eq!(
        compile_sources.len(),
        1,
        "one-step Swift must emit compile steps only for non-Swift sources: {compile_sources:?}"
    );
    assert!(
        compile_sources
            .first()
            .is_some_and(|src| src.ends_with(b"helper.c")),
        "{compile_sources:?}"
    );
    let (link_inputs, link_output, link_cmd) = graph
        .steps
        .iter()
        .find_map(|step| match step {
            BuildStep::Link {
                inputs,
                step,
                invocations,
                ..
            } => Some((
                inputs.clone(),
                step.output.clone(),
                invocations
                    .iter()
                    .map(|inv| String::from_utf8_lossy(&inv.shell).into_owned())
                    .collect::<Vec<_>>()
                    .join(" && "),
            )),
            _ => None,
        })
        .ok_or("missing link step")?;
    assert!(
        link_inputs.iter().any(|p| p.ends_with(b"main.swift")),
        "Swift source must be an explicit link input: {link_inputs:?}"
    );
    assert!(
        link_inputs.iter().any(|p| p.ends_with(b"helper.c.o")),
        "C object must be an explicit link input: {link_inputs:?}"
    );
    assert!(
        !link_inputs.iter().any(|p| p.ends_with(b"main.swift.o")),
        "Swift object must be a link output, not an input: {link_inputs:?}"
    );
    let extra_outputs = graph
        .link_extra_outputs
        .get(&link_output)
        .ok_or("missing one-step Swift extra link outputs")?;
    assert!(
        extra_outputs.iter().any(|p| p.ends_with(b"main.swift.o")),
        "{extra_outputs:?}"
    );
    assert!(
        extra_outputs
            .iter()
            .any(|p| p.ends_with(b"Release/my_lib.swiftmodule")),
        "{extra_outputs:?}"
    );
    assert_eq!(graph.swift_output_file_maps.len(), 1);
    let (map_path, map_json) = graph
        .swift_output_file_maps
        .iter()
        .next()
        .ok_or("missing output-file-map.json")?;
    assert!(map_path.ends_with(b"output-file-map.json"), "{map_path}");
    assert!(
        map_json
            .as_slice()
            .windows(b"main.swift.o".len())
            .any(|w| w == b"main.swift.o"),
        "{map_json}"
    );
    assert!(
        link_cmd.contains("-output-file-map") && link_cmd.contains("output-file-map.json"),
        "{link_cmd}"
    );
    assert!(link_cmd.contains("main.swift"), "{link_cmd}");
    assert!(link_cmd.contains("helper.c.o"), "{link_cmd}");
    assert!(link_cmd.contains("-DSWIFT_DEF=1"), "{link_cmd}");
    assert!(link_cmd.contains(&format!("-I{root}/inc")), "{link_cmd}");
    assert!(link_cmd.contains("-module-name my_lib"), "{link_cmd}");
    assert!(
        link_cmd.contains("-module-link-name custom_swift"),
        "{link_cmd}"
    );
    assert!(
        link_cmd.contains("Release/my_lib.swiftmodule"),
        "{link_cmd}"
    );
    Ok(())
}

#[test]
fn swift_one_step_executable_with_exports_uses_export_link_rule() -> TestResult {
    let host = PureHost::default();
    let root = if cfg!(windows) { "C:/work" } else { "/work" };
    host.seed_file(
        Path::new(&format!("{root}/main.swift")),
        b"public func go() {}\n",
    )?;

    let mut model = ProjectModel::default();
    model.generator = Some(rcmake_model::GeneratorKind::Ninja);
    model.set_root_dirs(root.into(), format!("{root}/build").into());
    model
        .global
        .props
        .set("ENABLED_LANGUAGES".into(), "Swift".into());
    let vars = &mut model.dir_mut(DirId::ROOT).vars;
    vars.insert("CMAKE_Swift_COMPILER".into(), "/usr/bin/swiftc".into());
    vars.insert("CMAKE_Swift_SOURCE_FILE_EXTENSIONS".into(), "swift".into());
    vars.insert("CMAKE_Swift_COMPILE_OBJECT".into(), ":".into());
    vars.insert(
        "CMAKE_Swift_LINK_EXECUTABLE".into(),
        "<CMAKE_Swift_COMPILER> -emit-executable -o <TARGET> <FLAGS> <SWIFT_SOURCES>".into(),
    );
    vars.insert(
        "CMAKE_Swift_LINK_EXECUTABLE_WITH_EXPORTS".into(),
        "<CMAKE_Swift_COMPILER> -emit-executable -o <TARGET> <FLAGS> <SWIFT_SOURCES> -emit-module -emit-module-path <SWIFT_MODULE>".into(),
    );

    let mut exported = Target::new(
        "exp_exe".into(),
        TargetKind::Executable,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    );
    exported.props.set("ENABLE_EXPORTS".into(), "ON".into());
    exported
        .sources
        .push(BT::new("main.swift".into(), Default::default()));
    model.add_target(exported)?;

    let mut plain = Target::new(
        "plain_exe".into(),
        TargetKind::Executable,
        TargetOrigin::Normal,
        DirId::ROOT,
        Default::default(),
        Default::default(),
    );
    plain
        .sources
        .push(BT::new("main.swift".into(), Default::default()));
    model.add_target(plain)?;

    let result = resolve(&model, &host, &[])?;
    let graph = result.graphs.first().ok_or("missing graph")?;
    let link_for = |name: &[u8]| -> Option<(BString, String)> {
        let target = graph.targets.iter().find(|t| t.name.as_slice() == name)?;
        target
            .steps
            .iter()
            .find_map(|&id| match graph.steps.get(id) {
                Some(BuildStep::Link {
                    step, invocations, ..
                }) => Some((
                    step.output.clone(),
                    invocations
                        .iter()
                        .map(|inv| String::from_utf8_lossy(&inv.shell).into_owned())
                        .collect::<Vec<_>>()
                        .join(" && "),
                )),
                _ => None,
            })
    };

    let (exp_out, exp_cmd) = link_for(b"exp_exe").ok_or("missing exp_exe link")?;
    assert!(
        exp_cmd.contains("-emit-module")
            && exp_cmd.contains("-emit-module-path exp_exe.swiftmodule"),
        "{exp_cmd}"
    );
    let exp_extra = graph
        .link_extra_outputs
        .get(&exp_out)
        .ok_or("missing exp_exe extra outputs")?;
    assert!(
        exp_extra
            .iter()
            .any(|p| p.ends_with(b"exp_exe.swiftmodule")),
        "{exp_extra:?}"
    );

    let (plain_out, plain_cmd) = link_for(b"plain_exe").ok_or("missing plain_exe link")?;
    assert!(!plain_cmd.contains("-emit-module"), "{plain_cmd}");
    let plain_extra = graph
        .link_extra_outputs
        .get(&plain_out)
        .ok_or("missing plain_exe extra outputs")?;
    assert!(
        !plain_extra.iter().any(|p| p.ends_with(b".swiftmodule")),
        "{plain_extra:?}"
    );
    Ok(())
}

#[test]
fn swift_macos_deployment_target_uses_intel_default_arch_and_preserves_explicit_target()
-> TestResult {
    let (mut model, host) = fixture()?;
    let root = if cfg!(windows) { "C:/work" } else { "/work" };
    host.seed_file(Path::new(&format!("{root}/src/main.swift")), b"print(1)\n")?;
    model
        .global
        .props
        .set("ENABLED_LANGUAGES".into(), "Swift".into());
    let vars = &mut model.dir_mut(DirId::ROOT).vars;
    vars.insert("CMAKE_SYSTEM_NAME".into(), "Darwin".into());
    vars.insert("APPLE".into(), "1".into());
    vars.insert("_CMAKE_APPLE_ARCHS_DEFAULT".into(), "x86_64".into());
    vars.insert("CMAKE_OSX_DEPLOYMENT_TARGET".into(), "14.0".into());
    vars.insert("CMAKE_Swift_COMPILER".into(), "/usr/bin/swiftc".into());
    vars.insert("CMAKE_Swift_LINKER_PREFERENCE".into(), "50".into());
    vars.insert("CMAKE_Swift_SOURCE_FILE_EXTENSIONS".into(), "swift".into());
    vars.insert("CMAKE_Swift_COMPILE_OBJECT".into(), ":".into());
    vars.insert(
        "CMAKE_Swift_LINK_EXECUTABLE".into(),
        "<CMAKE_Swift_COMPILER> -o <TARGET> <FLAGS> <SWIFT_SOURCES>".into(),
    );
    model.add_target(target("app", TargetKind::Executable, &["main.swift"]))?;

    let result = resolve(&model, &host, &[])?;
    let link_cmd = result
        .graphs
        .first()
        .and_then(|graph| {
            graph.steps.iter().find_map(|step| match step {
                BuildStep::Link { invocations, .. } => invocations
                    .first()
                    .map(|inv| String::from_utf8_lossy(&inv.shell).into_owned()),
                _ => None,
            })
        })
        .ok_or("missing link step")?;
    assert!(
        link_cmd.contains("-target x86_64-apple-macosx14.0"),
        "{link_cmd}"
    );
    assert!(!link_cmd.contains("arm64-apple-macosx"), "{link_cmd}");

    model.dir_mut(DirId::ROOT).vars.insert(
        "CMAKE_Swift_COMPILER_TARGET".into(),
        "x86_64-apple-macosx13.0".into(),
    );
    model.dir_mut(DirId::ROOT).vars.insert(
        "CMAKE_Swift_COMPILE_OPTIONS_TARGET".into(),
        "-target".into(),
    );
    let explicit = resolve(&model, &host, &[])?;
    let explicit_cmd = explicit
        .graphs
        .first()
        .and_then(|graph| {
            graph.steps.iter().find_map(|step| match step {
                BuildStep::Link { invocations, .. } => invocations
                    .first()
                    .map(|inv| String::from_utf8_lossy(&inv.shell).into_owned()),
                _ => None,
            })
        })
        .ok_or("missing explicit link step")?;
    assert!(
        explicit_cmd.contains("x86_64-apple-macosx13.0"),
        "{explicit_cmd}"
    );
    assert!(
        !explicit_cmd.contains("x86_64-apple-macosx14.0"),
        "{explicit_cmd}"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn file_set_list_expansion_decodes_semicolons_without_stripping_literal_backslashes() -> TestResult
{
    let (mut model, host) = fixture()?;
    host.seed_file(
        Path::new("/work/src/a.c"),
        b"int main(void) { return 0; }\n",
    )?;
    host.seed_file(Path::new("/work/src/a;b.h"), b"")?;
    let app_id = model.add_target(target("app", TargetKind::Executable, &["a.c"]))?;
    // Three backslashes decode through the 3 CMake file-set list expansions to `a;b.h`.
    model.add_file_set(rcmake_model::FileSet {
        target: app_id,
        name: "HEADERS".into(),
        kind: rcmake_model::FileSetKind::Headers,
        scope: rcmake_model::UsageScope::Private,
        base_dirs: vec![BT::new("/work/src".into(), Default::default())],
        files: vec![BT::new(r"/work/src/a\\\;b.h".into(), Default::default())],
        props: Default::default(),
    })?;
    let ok = resolve(&model, &host, &[])?;
    assert_eq!(ok.targets.len(), 1);

    // Four backslashes decode to literal `a\;b.h`, which does NOT exist and must not match `a;b.h`.
    let mut bad = model.clone();
    let bad_id = bad.add_target(target("bad", TargetKind::Executable, &["a.c"]))?;
    bad.add_file_set(rcmake_model::FileSet {
        target: bad_id,
        name: "HEADERS".into(),
        kind: rcmake_model::FileSetKind::Headers,
        scope: rcmake_model::UsageScope::Private,
        base_dirs: vec![BT::new("/work/src".into(), Default::default())],
        files: vec![BT::new(r"/work/src/a\\\\;b.h".into(), Default::default())],
        props: Default::default(),
    })?;
    assert!(
        resolve(&bad, &host, &[]).is_err(),
        "four-backslash literal `a\\;b.h` must not silently match `a;b.h`"
    );

    // Once `a\;b.h` exists on disk, four backslashes resolve cleanly to `a\;b.h`.
    host.seed_file(Path::new(r"/work/src/a\;b.h"), b"")?;
    let resolved_literal = resolve(&bad, &host, &[])?;
    assert_eq!(resolved_literal.targets.len(), 2);
    Ok(())
}
