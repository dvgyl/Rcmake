use std::path::Path;
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_diag::{DiagOptions, PolicyId, PolicyStatus};
use rcmake_eval::{Interp, InterpConfig, RunMode};
use rcmake_host::{PureHost, Stream};
use rcmake_model::{DirId, FileSetKind, PropScope, TargetOrigin, UsageField, UsageScope};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn configure(
    script: &str,
    children: &[(&str, &str)],
    exit: i32,
) -> Result<(Arc<PureHost>, Interp), Box<dyn std::error::Error>> {
    let cwd = Path::new("/work");
    let source = cwd.join("src");
    let exe = Path::new("/bin/cmake");
    let host = Arc::new(PureHost::new(cwd.into(), exe.into()));
    let mut interp = Interp::new(
        InterpConfig {
            host: host.clone(),
            mode: RunMode::Configure,
            cmake_root: "/share/cmake-4.5".into(),
            cmake_command: exe.into(),
            cwd: cwd.into(),
            limits: Default::default(),
            diag: DiagOptions::default(),
            defines: Vec::new(),
            script_args: Vec::new(),
        },
        |registry| {
            crate::register_all(registry);
            registry.add(
                "test_subdirectory",
                |cx, args| {
                    let dir = super::bytes(&args[0]);
                    let src = cx.current_source_dir().join(BString::from(dir).to_string());
                    let bin = cx.current_binary_dir().join(BString::from(dir).to_string());
                    cx.run_subdirectory(&src, &bin, Default::default())?;
                    Ok(rcmake_eval::Flow::Normal)
                },
                rcmake_eval::CmdFlags { scriptable: false },
            );
        },
    );
    let input = format!("cmake_minimum_required(VERSION 4.5)\n{script}");
    host.seed_file(&source.join("CMakeLists.txt"), input.as_bytes())?;
    for (dir, content) in children {
        host.seed_file(&source.join(dir).join("CMakeLists.txt"), content.as_bytes())?;
    }
    let outcome = interp.run_configure(&source, &cwd.join("build"), &[]);
    assert_eq!(
        outcome.exit_code,
        exit,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    Ok((host, interp))
}

fn var(interp: &mut Interp, name: &str, value: &str) {
    assert_eq!(
        interp.cx().var(name.as_bytes()),
        Some(value.into()),
        "{name}"
    );
}

#[test]
fn creates_targets_and_captures_creation_properties() -> TestResult {
    let (_, mut interp) = configure(
        r#"
        set(CMAKE_BUILD_TYPE Debug)
        set(CMAKE_DEBUG_POSTFIX _d)
        set(CMAKE_RUNTIME_OUTPUT_DIRECTORY_DEBUG out/debug)
        set(CMAKE_CXX_STANDARD 23)
        define_property(TARGET PROPERTY my_property INITIALIZE_FROM_VARIABLE init_my_property)
        set(init_my_property first)
        add_library(static src.c)
        set(init_my_property second)
        set(CMAKE_CXX_STANDARD 20)
        set(BUILD_SHARED_LIBS TRUE)
        add_library(shared src.c)
        add_executable(app WIN32 MACOSX_BUNDLE EXCLUDE_FROM_ALL app.c)
        add_library(iface INTERFACE EXCLUDE_FROM_ALL iface.c)
        get_target_property(static_type static TYPE)
        get_target_property(shared_type shared TYPE)
        get_target_property(iface_src iface SOURCES)
        get_target_property(win app WIN32_EXECUTABLE)
        get_target_property(bundle app MACOSX_BUNDLE)
        get_target_property(exclude app EXCLUDE_FROM_ALL)
        get_target_property(first static my_property)
        get_target_property(second shared my_property)
        get_target_property(standard static CXX_STANDARD)
        get_target_property(static_pic static POSITION_INDEPENDENT_CODE)
        get_target_property(shared_pic shared POSITION_INDEPENDENT_CODE)
        get_target_property(postfix static DEBUG_POSTFIX)
        get_target_property(app_postfix app DEBUG_POSTFIX)
        get_target_property(output app RUNTIME_OUTPUT_DIRECTORY_DEBUG)
    "#,
        &[],
        0,
    )?;
    for (name, value) in [
        ("static_type", "STATIC_LIBRARY"),
        ("shared_type", "SHARED_LIBRARY"),
        ("iface_src", "iface.c"),
        ("win", "TRUE"),
        ("bundle", "TRUE"),
        ("exclude", "TRUE"),
        ("first", "first"),
        ("second", "second"),
        ("standard", "23"),
        ("static_pic", "static_pic-NOTFOUND"),
        ("shared_pic", "True"),
        ("postfix", "_d"),
        ("app_postfix", "app_postfix-NOTFOUND"),
        ("output", "out/debug"),
    ] {
        var(&mut interp, name, value);
    }
    let mut cx = interp.cx();
    let model = cx.project();
    let id = model
        .find_target(DirId::ROOT, b"static")
        .ok_or("missing target")?;
    let target = model.target(id).ok_or("missing target")?;
    assert_eq!(
        target.policies.policies.get(PolicyId::new(79)),
        PolicyStatus::New
    );
    assert!(!target.sources[0].backtrace.is_empty());
    Ok(())
}

#[test]
fn captures_generated_properties_at_target_creation() -> TestResult {
    let (_, mut interp) = configure(
        r#"
        set(CMAKE_AUTOMOC ON)
        set(CMAKE_CXX_STANDARD 23)
        set(CMAKE_C_VISIBILITY_PRESET hidden)
        set(CMAKE_INTERPROCEDURAL_OPTIMIZATION TRUE)
        set(CMAKE_INSTALL_RPATH "first;second")
        set(CMAKE_PDB_OUTPUT_DIRECTORY pdb)
        set(CMAKE_MACOSX_RPATH OFF)
        set(CMAKE_COMPILE_WARNING_AS_ERROR ON)
        add_library(lib STATIC lib.c)
        set(CMAKE_AUTOMOC OFF)
        unset(CMAKE_CXX_STANDARD)
        set(CMAKE_C_VISIBILITY_PRESET default)
        set(CMAKE_INTERPROCEDURAL_OPTIMIZATION FALSE)
        set(CMAKE_INSTALL_RPATH "")
        set(CMAKE_PDB_OUTPUT_DIRECTORY app-pdb)
        set(CMAKE_MACOSX_RPATH ON)
        add_executable(app app.c)
        add_library(iface INTERFACE)
        add_library(imported STATIC IMPORTED)
        foreach(target lib app iface imported)
          foreach(property AUTOMOC CXX_STANDARD C_VISIBILITY_PRESET
                           INTERPROCEDURAL_OPTIMIZATION INSTALL_RPATH
                           PDB_OUTPUT_DIRECTORY MACOSX_RPATH COMPILE_WARNING_AS_ERROR)
            get_target_property(${target}_${property} ${target} ${property})
          endforeach()
        endforeach()
        "#,
        &[],
        0,
    )?;
    for (name, value) in [
        ("lib_AUTOMOC", "ON"),
        ("lib_CXX_STANDARD", "23"),
        ("lib_C_VISIBILITY_PRESET", "hidden"),
        ("lib_INTERPROCEDURAL_OPTIMIZATION", "TRUE"),
        ("lib_INSTALL_RPATH", "first;second"),
        ("lib_PDB_OUTPUT_DIRECTORY", "pdb"),
        ("lib_MACOSX_RPATH", "OFF"),
        ("lib_COMPILE_WARNING_AS_ERROR", "ON"),
        ("app_AUTOMOC", "OFF"),
        ("app_CXX_STANDARD", "app_CXX_STANDARD-NOTFOUND"),
        ("app_C_VISIBILITY_PRESET", "default"),
        ("app_INTERPROCEDURAL_OPTIMIZATION", "FALSE"),
        ("app_INSTALL_RPATH", ""),
        ("app_PDB_OUTPUT_DIRECTORY", "app-pdb"),
        ("app_MACOSX_RPATH", "ON"),
        ("app_COMPILE_WARNING_AS_ERROR", "ON"),
        ("iface_AUTOMOC", "iface_AUTOMOC-NOTFOUND"),
        ("imported_AUTOMOC", "imported_AUTOMOC-NOTFOUND"),
    ] {
        var(&mut interp, name, value);
    }
    Ok(())
}

#[test]
fn selects_enable_exports_initializer_by_target_type() -> TestResult {
    let (_, mut interp) = configure(
        r#"
        set(CMAKE_ENABLE_EXPORTS ON)
        set(CMAKE_SHARED_LIBRARY_ENABLE_EXPORTS ON)
        add_executable(legacy)
        set(CMAKE_EXECUTABLE_ENABLE_EXPORTS OFF)
        add_executable(app)
        add_library(shared SHARED)
        add_library(static STATIC)
        get_target_property(legacy_exports legacy ENABLE_EXPORTS)
        get_target_property(app_exports app ENABLE_EXPORTS)
        get_target_property(shared_exports shared ENABLE_EXPORTS)
        get_target_property(static_exports static ENABLE_EXPORTS)
        "#,
        &[],
        0,
    )?;
    var(&mut interp, "legacy_exports", "ON");
    var(&mut interp, "app_exports", "OFF");
    var(&mut interp, "shared_exports", "ON");
    var(&mut interp, "static_exports", "static_exports-NOTFOUND");
    Ok(())
}

#[test]
fn captures_properties_for_each_active_configuration() -> TestResult {
    let (_, mut interp) = configure(
        r#"
        set(CMAKE_GENERATOR "Ninja Multi-Config")
        set(CMAKE_CONFIGURATION_TYPES "Debug;Custom")
        set(CMAKE_BUILD_TYPE Release)
        set(CMAKE_DEBUG_POSTFIX _debug)
        set(CMAKE_CUSTOM_POSTFIX _custom)
        set(CMAKE_RELEASE_POSTFIX _release)
        set(CMAKE_ARCHIVE_OUTPUT_DIRECTORY_CUSTOM out/custom)
        set(CMAKE_FRAMEWORK_MULTI_CONFIG_POSTFIX_CUSTOM _fw)
        set(CMAKE_MAP_IMPORTED_CONFIG_CUSTOM Release)
        set(CMAKE_OSX_ARCHITECTURES_CUSTOM arm64)
        add_library(lib STATIC)
        add_library(imported INTERFACE IMPORTED)
        get_target_property(debug lib DEBUG_POSTFIX)
        get_target_property(custom lib CUSTOM_POSTFIX)
        get_target_property(release lib RELEASE_POSTFIX)
        get_target_property(output lib ARCHIVE_OUTPUT_DIRECTORY_CUSTOM)
        get_target_property(framework lib FRAMEWORK_MULTI_CONFIG_POSTFIX_CUSTOM)
        get_target_property(imported_config imported MAP_IMPORTED_CONFIG_CUSTOM)
        get_target_property(architectures lib OSX_ARCHITECTURES_CUSTOM)
        "#,
        &[],
        0,
    )?;
    var(&mut interp, "debug", "_debug");
    var(&mut interp, "custom", "_custom");
    var(&mut interp, "release", "release-NOTFOUND");
    var(&mut interp, "output", "out/custom");
    var(&mut interp, "framework", "_fw");
    var(&mut interp, "imported_config", "Release");
    var(&mut interp, "architectures", "arm64");
    Ok(())
}

#[test]
fn resolves_aliases_and_preserves_import_visibility() -> TestResult {
    let (_, mut interp) = configure(
        r#"
        define_property(TARGET PROPERTY my_value INITIALIZE_FROM_VARIABLE init_my_value)
        set(init_my_value imported)
        add_library(local UNKNOWN IMPORTED)
        add_library(ns::local ALIAS local)
        add_executable(app IMPORTED GLOBAL)
        add_executable(ns::app ALIAS app)
        add_dependencies(ns::app ns::local future)
        get_target_property(referent ns::local ALIASED_TARGET)
        get_target_property(global_app ns::app ALIAS_GLOBAL)
        get_target_property(global_local ns::local ALIAS_GLOBAL)
        get_target_property(imported ns::local IMPORTED)
        get_target_property(custom local my_value)
        get_target_property(deps app MANUALLY_ADDED_DEPENDENCIES)
        set_target_properties(ns::local PROPERTIES IMPORTED_GLOBAL TRUE)
        get_target_property(promoted local IMPORTED_GLOBAL)
        get_target_property(alias_still_local ns::local ALIAS_GLOBAL)
    "#,
        &[],
        0,
    )?;
    for (name, value) in [
        ("referent", "local"),
        ("global_app", "TRUE"),
        ("global_local", "FALSE"),
        ("imported", "TRUE"),
        ("custom", "imported"),
        ("deps", "future;ns::local"),
        ("promoted", "TRUE"),
        ("alias_still_local", "FALSE"),
    ] {
        var(&mut interp, name, value);
    }
    Ok(())
}

#[test]
fn local_imports_and_aliases_follow_directory_visibility() -> TestResult {
    let (_, mut interp) = configure(
        r#"
        test_subdirectory(first)
        test_subdirectory(second)
    "#,
        &[
            (
                "first",
                "add_library(dep INTERFACE IMPORTED)\nadd_library(ns::dep ALIAS dep)\ntest_subdirectory(nested)",
            ),
            ("first/nested", "add_library(ns::nested ALIAS dep)"),
            (
                "second",
                "add_library(dep INTERFACE IMPORTED GLOBAL)\nadd_library(ns::global ALIAS dep)",
            ),
        ],
        0,
    )?;
    let mut cx = interp.cx();
    let model = cx.project();
    let mut dirs = model.dirs().skip(1);
    let (first, _) = dirs.next().ok_or("missing first directory")?;
    let (nested, _) = dirs.next().ok_or("missing nested directory")?;
    let (second, _) = dirs.next().ok_or("missing second directory")?;
    let local = model
        .find_target(first, b"dep")
        .ok_or("missing local target")?;
    let global = model
        .find_target(second, b"dep")
        .ok_or("missing global target")?;
    assert_ne!(local, global);
    assert_eq!(model.find_target(nested, b"ns::dep"), Some(local));
    assert_eq!(model.find_target(nested, b"ns::nested"), Some(local));
    assert_eq!(model.find_target(second, b"ns::dep"), None);
    assert_eq!(model.find_target(DirId::ROOT, b"ns::global"), Some(global));
    Ok(())
}

#[test]
fn sets_properties_and_applies_usage_scopes_and_order() -> TestResult {
    let (_, mut interp) = configure(
        r#"
        add_library(lib STATIC lib.c)
        add_library(other INTERFACE)
        add_library(ns::lib ALIAS lib)
        set_target_properties(ns::lib other PROPERTIES CUSTOM value)
        set_target_properties(ns::lib PROPERTIES INCLUDE_DIRECTORIES "/old;/next" COMPILE_DEFINITIONS OLD)
        target_include_directories(ns::lib SYSTEM BEFORE PUBLIC first PRIVATE private PUBLIC final)
        target_include_directories(lib SYSTEM AFTER INTERFACE ext)
        target_compile_definitions(lib PUBLIC -DHELLO -D-DONCE PRIVATE -DLOCAL INTERFACE -DAPI)
        target_compile_features(lib PUBLIC cxx_std_20)
        target_compile_options(lib PUBLIC "SHELL:-I path")
        target_compile_options(lib BEFORE PUBLIC -Wall)
        target_link_options(lib BEFORE PUBLIC "SHELL:-X foo")
        target_link_directories(lib AFTER PUBLIC ./lib "$<1:unchanged>")
        get_target_property(custom other CUSTOM)
        get_target_property(name ns::lib NAME)
        get_target_property(missing lib MISSING)
        get_target_property(own_inc lib INCLUDE_DIRECTORIES)
        get_target_property(api_inc lib INTERFACE_INCLUDE_DIRECTORIES)
        get_target_property(system_inc lib INTERFACE_SYSTEM_INCLUDE_DIRECTORIES)
        get_target_property(own_def lib COMPILE_DEFINITIONS)
        get_target_property(api_def lib INTERFACE_COMPILE_DEFINITIONS)
        get_target_property(features lib INTERFACE_COMPILE_FEATURES)
        get_target_property(compile lib COMPILE_OPTIONS)
        get_target_property(link lib LINK_OPTIONS)
        get_target_property(dirs lib LINK_DIRECTORIES)
    "#,
        &[],
        0,
    )?;
    for (name, value) in [
        ("custom", "value"),
        ("name", "lib"),
        ("missing", "missing-NOTFOUND"),
        (
            "own_inc",
            "/work/src/final;/work/src/private;/work/src/first;/old;/next",
        ),
        ("api_inc", "/work/src/final;/work/src/first;/work/src/ext"),
        (
            "system_inc",
            "/work/src/first;/work/src/final;/work/src/ext",
        ),
        ("own_def", "OLD;HELLO;-DONCE;LOCAL"),
        ("api_def", "HELLO;-DONCE;API"),
        ("features", "cxx_std_20"),
        ("compile", "-Wall;SHELL:-I path"),
        ("link", "SHELL:-X foo"),
        ("dirs", "/work/src/./lib;$<1:unchanged>"),
    ] {
        var(&mut interp, name, value);
    }
    Ok(())
}

#[test]
fn records_sources_and_file_sets() -> TestResult {
    let (_, mut interp) = configure(
        r#"
        add_library(lib STATIC lib.c)
        target_sources(lib PUBLIC src.c "$<1:unchanged>" PRIVATE private.c)
        target_sources(lib PUBLIC FILE_SET headers TYPE HEADERS BASE_DIRS include FILES include/lib.h)
        target_sources(lib PUBLIC FILE_SET headers FILES include/extra.h)
        target_sources(lib FILE_SET HEADERS SCOPE INTERFACE FILES default.h)
        target_sources(lib PRIVATE FILE_SET CXX_MODULES BASE_DIRS modules FILES modules/example.cxx)
        get_target_property(sources lib SOURCES)
        get_target_property(api_src lib INTERFACE_SOURCES)
        get_target_property(api_inc lib INTERFACE_INCLUDE_DIRECTORIES)
        get_target_property(headers lib HEADER_SET_headers)
        get_target_property(sets lib INTERFACE_HEADER_SETS)
        get_target_property(modules lib CXX_MODULE_DIRS)
    "#,
        &[],
        0,
    )?;
    for (name, value) in [
        (
            "sources",
            "lib.c;/work/src/src.c;$<1:unchanged>;/work/src/private.c",
        ),
        ("api_src", "/work/src/src.c;$<1:unchanged>"),
        (
            "api_inc",
            "$<BUILD_INTERFACE:/work/src/include>;$<BUILD_INTERFACE:/work/src>",
        ),
        (
            "headers",
            "/work/src/include/lib.h;/work/src/include/extra.h",
        ),
        ("sets", "headers;HEADERS"),
        ("modules", "/work/src/modules"),
    ] {
        var(&mut interp, name, value);
    }
    let mut cx = interp.cx();
    let model = cx.project();
    let id = model
        .find_target(DirId::ROOT, b"lib")
        .ok_or("missing target")?;
    let target = model.target(id).ok_or("missing target")?;
    assert_eq!(target.file_sets.len(), 3);
    let headers = model
        .file_set(target.file_sets[0])
        .ok_or("missing file set")?;
    assert_eq!(headers.kind, FileSetKind::Headers);
    assert_eq!(headers.scope, UsageScope::Public);
    assert_eq!(headers.base_dirs.len(), 1);
    assert_eq!(headers.files.len(), 2);
    assert!(!headers.files[1].backtrace.is_empty());
    assert!(
        !target
            .usage
            .entries(UsageField::InterfaceIncludeDirectories)[0]
            .backtrace
            .is_empty()
    );
    Ok(())
}

#[test]
fn rejects_overlap_across_file_set_calls() -> TestResult {
    let (host, _) = configure(
        r#"
        add_library(lib INTERFACE)
        target_sources(lib INTERFACE FILE_SET headers TYPE HEADERS BASE_DIRS a a-sibling FILES a/a.h)
        target_sources(lib INTERFACE FILE_SET headers BASE_DIRS "unrelated;b/../a/sub")
    "#,
        &[],
        1,
    )?;
    let diagnostic = host.stream_bytes(Stream::Stderr).to_string();
    let words = diagnostic.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        words.contains("cannot be subdirectories of each other"),
        "{diagnostic}"
    );
    Ok(())
}

#[test]
fn propagates_private_links_by_library_kind() -> TestResult {
    let (_, mut interp) = configure(
        r#"
        set_property(GLOBAL PROPERTY DEBUG_CONFIGURATIONS "Debug;ASan")
        add_library(s STATIC)
        add_library(o OBJECT)
        add_library(h SHARED)
        add_library(m MODULE)
        add_library(n STATIC)
        foreach(t s o h m)
          target_link_libraries(${t} PUBLIC p PRIVATE a debug b optimized c general d INTERFACE e)
          get_target_property(${t}_own ${t} LINK_LIBRARIES)
          get_target_property(${t}_interface ${t} INTERFACE_LINK_LIBRARIES)
        endforeach()
        target_link_libraries(n a general b)
        get_target_property(plain n INTERFACE_LINK_LIBRARIES)
    "#,
        &[],
        0,
    )?;
    for target in ["s", "o", "h", "m"] {
        var(
            &mut interp,
            &format!("{target}_own"),
            "p;a;$<$<OR:$<CONFIG:DEBUG>,$<CONFIG:ASAN>>:b>;$<$<NOT:$<OR:$<CONFIG:DEBUG>,$<CONFIG:ASAN>>>:c>;d",
        );
        let interface = if matches!(target, "s" | "o") {
            "p;$<LINK_ONLY:a>;$<LINK_ONLY:$<$<OR:$<CONFIG:DEBUG>,$<CONFIG:ASAN>>:b>>;$<LINK_ONLY:$<$<NOT:$<OR:$<CONFIG:DEBUG>,$<CONFIG:ASAN>>>:c>>;$<LINK_ONLY:d>;e"
        } else {
            "p;e"
        };
        var(&mut interp, &format!("{target}_interface"), interface);
    }
    var(&mut interp, "plain", "a;b");
    Ok(())
}

#[test]
fn records_link_lookup_directory_and_backtrace() -> TestResult {
    let (_, mut interp) = configure(
        "add_library(root STATIC)\ntest_subdirectory(child)",
        &[(
            "child",
            "add_library(local INTERFACE IMPORTED)\ntarget_link_libraries(root PRIVATE local)",
        )],
        0,
    )?;
    let mut cx = interp.cx();
    let model = cx.project();
    let root = model
        .find_target(DirId::ROOT, b"root")
        .ok_or("missing root target")?;
    let (child, _) = model.dirs().nth(1).ok_or("missing child directory")?;
    let target = model.target(root).ok_or("missing root target")?;
    let own = target
        .usage
        .link_libraries
        .as_ref()
        .ok_or("missing link entries")?;
    let interface = target
        .usage
        .interface_link_libraries
        .as_ref()
        .ok_or("missing interface entries")?;
    assert_eq!(own[0].lookup_dir, child);
    assert_eq!(interface[0].lookup_dir, child);
    assert!(!own[0].text.backtrace.is_empty());
    assert_eq!(
        model.get_prop(&PropScope::Target(root), b"INTERFACE_LINK_LIBRARIES"),
        Some("$<LINK_ONLY:local>".into())
    );
    Ok(())
}

#[test]
fn reports_invalid_targets_and_signatures() -> TestResult {
    for (script, expected) in [
        (
            "add_library(t INTERFACE)\nadd_executable(t)",
            "The existing target is an interface library",
        ),
        (
            "add_library(t INTERFACE)\nadd_library(a ALIAS t)\nadd_library(b ALIAS a)",
            "is itself an ALIAS",
        ),
        (
            "add_library(t INTERFACE)\nadd_executable(a ALIAS t)",
            "is not an executable",
        ),
        (
            "add_library(t INTERFACE)\nadd_library(a ALIAS t)\ntarget_link_libraries(a INTERFACE d)",
            "can not be used on an ALIAS target.",
        ),
        (
            "add_library(t STATIC)\ntarget_link_libraries(t d)\ntarget_link_libraries(t PRIVATE e)",
            "The plain signature for target_link_libraries has already been used",
        ),
        (
            "add_library(t STATIC)\ntarget_link_libraries(t PRIVATE d)\ntarget_link_libraries(t e)",
            "The keyword signature for target_link_libraries has already been used",
        ),
        (
            "add_library(t INTERFACE)\ntarget_link_libraries(t INTERFACE optimized)",
            "The \"optimized\" argument must be followed by a library.",
        ),
        (
            "add_library(t STATIC IMPORTED)\ntarget_include_directories(t PRIVATE inc)",
            "may only set INTERFACE properties on IMPORTED targets",
        ),
        (
            "add_library(t INTERFACE)\ntarget_compile_options(t PUBLIC -Wall)",
            "may only set INTERFACE properties on INTERFACE targets",
        ),
    ] {
        let (host, _) = configure(script, &[], 1)?;
        let diagnostic = host.stream_bytes(Stream::Stderr).to_string();
        let words = diagnostic.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(words.contains(expected), "{diagnostic}");
    }
    Ok(())
}

#[test]
fn keeps_global_imports_in_global_lookup() -> TestResult {
    let (_, mut interp) = configure("add_library(t MODULE IMPORTED GLOBAL)", &[], 0)?;
    let mut cx = interp.cx();
    let model = cx.project();
    let id = model
        .find_target(DirId::ROOT, b"t")
        .ok_or("missing target")?;
    assert_eq!(
        model.target(id).ok_or("missing target")?.origin,
        TargetOrigin::Imported(rcmake_model::ImportedVisibility::Global)
    );
    Ok(())
}
