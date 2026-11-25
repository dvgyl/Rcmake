use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_eval::{Interp, InterpConfig, RunMode};
use rcmake_host::{Host, PureHost, ReadRange, Stream};
use rcmake_model::{DirId, GeneratorKind, VsVersion};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn context(mode: RunMode) -> (Arc<PureHost>, Interp) {
    let cwd = PathBuf::from("/work");
    let exe = PathBuf::from("/bin/cmake");
    let host = Arc::new(PureHost::new(cwd.clone(), exe.clone()));
    let interp = Interp::new(
        InterpConfig {
            services: Default::default(),
            host: host.clone(),
            mode,
            cmake_root: "/share/cmake-4.5".into(),
            cmake_command: exe,
            cwd,
            limits: Default::default(),
            diag: Default::default(),
            defines: Vec::new(),
            script_args: Vec::new(),
        },
        crate::register_all,
    );
    (host, interp)
}

fn configure(host: &PureHost, interp: &mut Interp, source: &str, code: i32) -> TestResult {
    let root = Path::new("/work/src");
    let source = format!("cmake_minimum_required(VERSION 4.4)\nproject(check NONE)\n{source}");
    host.seed_file(&root.join("CMakeLists.txt"), source.as_bytes())?;
    assert_eq!(
        interp
            .run_configure(root, Path::new("/work/build"), &[])
            .exit_code,
        code,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    Ok(())
}

fn read(host: &PureHost, filename: &str) -> Result<BString, rcmake_host::HostError> {
    host.read(Path::new(filename), ReadRange::All { limit: 64 * 1024 })
        .map(BString::from)
}

#[test]
fn source_directory_and_include_regex_commands() -> TestResult {
    let (host, mut interp) = context(RunMode::Configure);
    for name in ["b.cpp", "a.c", "a.h", "x.cu", "a.cppm", "plain"] {
        host.seed_file(&Path::new("/work/src/sub").join(name), b"")?;
    }
    host.seed_file(
        Path::new("/work/src/child/CMakeLists.txt"),
        br#"
        get_property(pattern DIRECTORY PROPERTY INCLUDE_REGULAR_EXPRESSION)
        if(NOT pattern STREQUAL "^headers")
          message(FATAL_ERROR "Expected the parent directory pattern")
        endif()
        "#,
    )?;
    configure(
        &host,
        &mut interp,
        r#"
        get_directory_property(default_pattern INCLUDE_REGULAR_EXPRESSION)
        include_regular_expression("^headers" "^local")
        add_subdirectory(child)
        include_regular_expression("^other")
        get_directory_property(pattern INCLUDE_REGULAR_EXPRESSION)
        set_property(DIRECTORY PROPERTY INCLUDE_REGULAR_EXPRESSION)
        get_property(pattern_set DIRECTORY PROPERTY INCLUDE_REGULAR_EXPRESSION SET)
        set(src prior)
        aux_source_directory(sub src)
        aux_source_directory(missing src)
        if(NOT default_pattern STREQUAL "^.*$" OR NOT pattern STREQUAL "^other"
           OR pattern_set
           OR NOT src STREQUAL "prior;sub/a.c;sub/a.cppm;sub/b.cpp;sub/x.cu;")
          message(FATAL_ERROR "Directory command returned an unexpected value")
        endif()
    "#,
        0,
    )
}

#[test]
fn external_microsoft_project_records_metadata_only_for_visual_studio() -> TestResult {
    let (host, mut interp) = context(RunMode::Configure);
    let configure_project = r#"
        add_custom_target(required)
        include_external_msproject(external [[dir\project.vcxproj]]
          TYPE project-type GUID project-guid PLATFORM platform required)
    "#;
    configure(&host, &mut interp, configure_project, 0)?;
    assert!(
        interp
            .cx()
            .project()
            .find_target(DirId::ROOT, b"external")
            .is_none()
    );

    let (host, mut interp) = context(RunMode::Configure);
    interp.cx().project().generator = Some(GeneratorKind::VisualStudio(VsVersion::Vs17));
    configure(&host, &mut interp, configure_project, 0)?;
    let mut cx = interp.cx();
    let model = cx.project();
    let id = model
        .find_target(DirId::ROOT, b"external")
        .ok_or("Expected external target")?;
    let target = model.target(id).ok_or("Expected external target")?;
    for (key, value) in [
        ("EXTERNAL_MSPROJECT", "dir/project.vcxproj"),
        ("VS_PLATFORM_MAPPING", "platform"),
        ("VS_PROJECT_TYPE", "project-type"),
    ] {
        assert_eq!(
            target
                .props
                .get(key.as_bytes())
                .map(|entry| entry.as_slice()),
            Some(value.as_bytes())
        );
    }
    assert_eq!(target.utility_deps[0].value, "required");
    assert_eq!(
        model
            .cache
            .get(b"external_GUID_CMAKE")
            .map(|entry| entry.value.as_slice()),
        Some(b"project-guid".as_slice())
    );
    Ok(())
}

#[test]
fn file_api_keeps_project_queries_in_memory() -> TestResult {
    let (host, mut interp) = context(RunMode::Configure);
    host.seed_file(
        Path::new("/work/src/child/CMakeLists.txt"),
        b"cmake_file_api(QUERY API_VERSION 1 CODEMODEL 3 2.12 CACHE 2)",
    )?;
    host.seed_file(
        Path::new("/work/build/.cmake/api/v1/query/cache-v2"),
        b"existing query",
    )?;
    configure(
        &host,
        &mut interp,
        r#"
        add_subdirectory(child)
        cmake_file_api(QUERY API_VERSION 1 CMAKEFILES 1.1 TOOLCHAINS 1)
        cmake_file_api(QUERY API_VERSION 4294967297 CACHE 4294967298)
    "#,
        0,
    )?;
    let root = "/work/build/.cmake/api/v1/query";
    assert!(read(&host, &format!("{root}/client-rcmake/cache-v2")).is_err());
    assert_eq!(read(&host, &format!("{root}/cache-v2"))?, "existing query");
    let queries = interp
        .cx()
        .global()
        .props
        .get(b"_RCMAKE_FILE_API_PROJECT_QUERIES")
        .cloned()
        .unwrap_or_default();
    assert_eq!(queries, "codemodel-v2;cache-v2;cmakeFiles-v1;toolchains-v1");
    Ok(())
}

#[test]
fn file_api_rejects_invalid_queries_before_writing_files() -> TestResult {
    let (host, mut interp) = context(RunMode::Configure);
    configure(
        &host,
        &mut interp,
        "cmake_file_api(QUERY API_VERSION 1 CACHE 2 CODEMODEL 2.13)",
        1,
    )?;
    assert!(
        host.stream_bytes(Stream::Stderr)
            .to_string()
            .contains("None of the specified CODEMODEL versions is supported")
    );
    let query = "/work/build/.cmake/api/v1/query/client-rcmake/cache-v2";
    assert!(read(&host, query).is_err());
    Ok(())
}

#[test]
fn cache_loads_prefixed_variables_and_legacy_internal_entries() -> TestResult {
    let cache = b"KEEP:STRING=first\nKEEP:STRING=' last  '\n\"A:B\":PATH=raw\nINCLUDED:INTERNAL=yes\nHIDDEN:INTERNAL=no\nSKIP:BOOL=ON\n";
    let (host, mut interp) = context(RunMode::Configure);
    host.seed_file(Path::new("/work/other/CMakeCache.txt"), cache)?;
    host.seed_file(
        Path::new("/work/src/other/CMakeCache.txt"),
        b"KEEP:STRING=wrong\n",
    )?;
    configure(
        &host,
        &mut interp,
        r#"
        set(P_MISSING present)
        set(quoted_key "A:B")
        load_cache(other READ_WITH_PREFIX P_ KEEP "${quoted_key}" HIDDEN MISSING)
        if(NOT P_KEEP STREQUAL " last  " OR NOT "${P_${quoted_key}}" STREQUAL "raw" OR NOT P_HIDDEN STREQUAL "no"
           OR NOT P_MISSING STREQUAL "present" OR DEFINED CACHE{P_KEEP})
          message(FATAL_ERROR "Unexpected prefixed cache values")
        endif()
        load_cache(other EXCLUDE SKIP INCLUDE_INTERNALS INCLUDED)
        get_property(type CACHE KEEP PROPERTY TYPE)
        if(NOT type STREQUAL "INTERNAL" OR NOT KEEP STREQUAL " last  "
           OR NOT INCLUDED STREQUAL "yes" OR DEFINED SKIP OR DEFINED HIDDEN)
          message(FATAL_ERROR "Unexpected imported cache entries")
        endif()
    "#,
        0,
    )?;
    let (host, mut interp) = context(RunMode::Script);
    host.seed_file(Path::new("/work/other/CMakeCache.txt"), cache)?;
    let script = Path::new("/work/check.cmake");
    host.seed_file(script, b"load_cache(other READ_WITH_PREFIX TEST_ KEEP)\n")?;
    assert_eq!(interp.run_script(script).exit_code, 0);
    assert_eq!(interp.cx().var(b"TEST_KEEP"), Some(" last  ".into()));
    host.seed_file(script, b"load_cache(other)\n")?;
    assert_eq!(interp.run_script(script).exit_code, 1);
    assert!(
        host.stream_bytes(Stream::Stderr)
            .to_string()
            .contains("Only load_cache(READ_WITH_PREFIX) may be used")
    );
    Ok(())
}

#[test]
fn test_driver_configures_the_bundled_template() -> TestResult {
    let (host, mut interp) = context(RunMode::Configure);
    host.seed_file(
        Path::new("/share/cmake-4.5/Templates/TestDriver.cxx.in"),
        include_bytes!("../../../../modules/cmake-4.5/Templates/TestDriver.cxx.in"),
    )?;
    configure(
        &host,
        &mut interp,
        r#"
        set(CMAKE_TESTDRIVER_BEFORE_TESTMAIN "int before;")
        set(CMAKE_TESTDRIVER_AFTER_TESTMAIN "int after;")
        create_test_sourcelist(source_list drivers/driver.c sub/a-test.c b.c
          EXTRA_INCLUDE extra.h FUNCTION process_args)
        if(NOT source_list STREQUAL "/work/build/drivers/driver.c;sub/a-test.c;b.c")
          message(FATAL_ERROR "Unexpected test driver source list")
        endif()
    "#,
        0,
    )?;
    let generated = read(&host, "/work/build/drivers/driver.c")?.to_string();
    for text in [
        "#include \"extra.h\"",
        "extern int sub_a_test(int, char*[]);",
        "\"sub/a-test\",",
        "process_args(&ac, &av);",
        "int before;",
        "int after;",
    ] {
        assert!(generated.contains(text), "{text}");
    }
    Ok(())
}

#[test]
fn get_warning_returns_the_cmp0225_message_for_each_policy_state() -> TestResult {
    let (host, mut interp) = context(RunMode::Script);
    let script = Path::new("/work/check.cmake");
    host.seed_file(
        script,
        br#"
        cmake_minimum_required(VERSION 4.4)
        cmake_policy(GET_WARNING CMP0225 unset)
        cmake_policy(SET CMP0225 OLD)
        cmake_policy(GET_WARNING CMP0225 old)
        cmake_policy(SET CMP0225 NEW)
        cmake_policy(GET_WARNING CMP0225 new)
    "#,
    )?;
    assert_eq!(interp.run_script(script).exit_code, 0);
    assert!(host.stream_bytes(Stream::Stderr).is_empty());
    let expected = "Policy CMP0225 is not set: ExternalProject sets CMAKE_INSTALL_PREFIX to <INSTALL_DIR> in the default configure command.  Run \"cmake --help-policy CMP0225\" for policy details.  Use the cmake_policy command to set the policy and suppress this warning.";
    for variable in ["unset", "old", "new"] {
        assert_eq!(interp.cx().var(variable.as_bytes()), Some(expected.into()));
    }
    Ok(())
}

#[test]
fn precompile_headers_keep_scope_paths_and_reuse_names() -> TestResult {
    let (host, mut interp) = context(RunMode::Configure);
    configure(
        &host,
        &mut interp,
        r#"
        add_library(owner STATIC owner.c)
        add_library(owner_alias ALIAS owner)
        add_library(user STATIC user.c)
        target_precompile_headers(owner_alias PRIVATE relative.h PUBLIC [[<stdio.h>]]
          INTERFACE [["public.h"]])
        target_precompile_headers(owner PRIVATE "$<$<COMPILE_LANGUAGE:C>:<stddef.h$<ANGLE-R>>")
        target_precompile_headers(user REUSE_FROM owner_alias)
        get_target_property(local owner PRECOMPILE_HEADERS)
        get_target_property(public owner INTERFACE_PRECOMPILE_HEADERS)
        get_target_property(reuse user PRECOMPILE_HEADERS_REUSE_FROM)
        if(NOT local STREQUAL "/work/src/relative.h;<stdio.h>;$<$<COMPILE_LANGUAGE:C>:<stddef.h$<ANGLE-R>>"
           OR NOT public STREQUAL [[<stdio.h>;"public.h"]]
           OR NOT reuse STREQUAL "owner_alias")
          message(FATAL_ERROR "Unexpected precompiled header properties")
        endif()
    "#,
        0,
    )
}

#[test]
fn external_project_configure_respects_cmp0225() -> TestResult {
    const MODULES: &[(&str, &[u8])] = &[
        (
            "ExternalProject.cmake",
            include_bytes!("../../../../modules/cmake-4.5/Modules/ExternalProject.cmake"),
        ),
        (
            "ExternalProject/shared_internal_commands.cmake",
            include_bytes!(
                "../../../../modules/cmake-4.5/Modules/ExternalProject/shared_internal_commands.cmake"
            ),
        ),
        (
            "ExternalProject/mkdirs.cmake.in",
            include_bytes!("../../../../modules/cmake-4.5/Modules/ExternalProject/mkdirs.cmake.in"),
        ),
        (
            "ExternalProject/cfgcmd.txt.in",
            include_bytes!("../../../../modules/cmake-4.5/Modules/ExternalProject/cfgcmd.txt.in"),
        ),
        (
            "ExternalProject/RepositoryInfo.txt.in",
            include_bytes!(
                "../../../../modules/cmake-4.5/Modules/ExternalProject/RepositoryInfo.txt.in"
            ),
        ),
        (
            "ExternalProject/UpdateInfo.txt.in",
            include_bytes!(
                "../../../../modules/cmake-4.5/Modules/ExternalProject/UpdateInfo.txt.in"
            ),
        ),
        (
            "ExternalProject/PatchInfo.txt.in",
            include_bytes!(
                "../../../../modules/cmake-4.5/Modules/ExternalProject/PatchInfo.txt.in"
            ),
        ),
    ];
    for (policy, has_prefix, has_warning) in [
        ("", false, true),
        ("cmake_policy(SET CMP0225 OLD)", false, false),
        ("cmake_policy(SET CMP0225 NEW)", true, false),
    ] {
        let (host, mut interp) = context(RunMode::Configure);
        interp.cx().project().generator = Some(GeneratorKind::Ninja);
        for &(name, contents) in MODULES {
            host.seed_file(&Path::new("/share/cmake-4.5/Modules").join(name), contents)?;
        }
        host.seed_file(
            Path::new("/work/src/vendor/CMakeLists.txt"),
            b"cmake_minimum_required(VERSION 4.5)\nproject(Vendor NONE)\n",
        )?;
        configure(
            &host,
            &mut interp,
            &format!(
                r#"
                set(CMAKE_GENERATOR Ninja)
                {policy}
                include(ExternalProject)
                ExternalProject_Add(dep SOURCE_DIR "${{CMAKE_CURRENT_SOURCE_DIR}}/vendor")
            "#
            ),
            0,
        )?;
        let command = read(&host, "/work/build/dep-prefix/tmp/dep-cfgcmd.txt")?.to_string();
        assert_eq!(
            command.contains("-DCMAKE_INSTALL_PREFIX:PATH=<INSTALL_DIR>"),
            has_prefix,
            "{policy}\n{command}"
        );
        let diagnostics = host.stream_bytes(Stream::Stderr).to_string();
        assert_eq!(
            diagnostics.contains("Policy CMP0225 is not set"),
            has_warning,
            "{policy}\n{diagnostics}"
        );
    }
    Ok(())
}
