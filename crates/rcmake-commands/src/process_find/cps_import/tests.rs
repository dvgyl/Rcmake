use std::sync::Arc;
use std::time::Duration;

use rcmake_eval::{Interp, InterpConfig, RunMode};
use rcmake_host::{
    CommandSpec, Host, Input, NativeHost, Output, ProcessResult, PureHost, RunRequest, Stream,
    WriteMode,
};
use rcmake_model::{DirId, UsageField};

use std::path::{Path, PathBuf};

use rcmake_base::ByteSlice as _;
use rcmake_host::path;

type TestResult = Result<(), Box<dyn std::error::Error>>;
const ROOT: &str = "/work/prefix/lib/cps/Foo";
const FOO: &str = r#"
{
  "cps_version": "0.15.0", "name": "Foo", "version": "2.4.3+suffix",
  "compat_version": "2.0", "cps_path": "@prefix@/lib/cps/Foo",
  "default_components": ["bar"],
  "components": {"bar": {
    "type": "archive", "includes": ["@prefix@/include"],
    "compile_flags": {"*":["-Wall"], "cpp":["-Wextra"]},
    "definitions": {"*":{"HAVE_FOO":null}},
    "link_flags": ["-flag"], "requires": ["Dep:api"]
  }}
}"#;

fn interpreter(host: Arc<dyn Host>, cwd: &Path) -> Interp {
    Interp::new(
        InterpConfig {
            services: Default::default(),
            host,
            mode: RunMode::Configure,
            cmake_root: "/cmake".into(),
            cmake_command: "/cmake/bin/cmake".into(),
            cwd: cwd.into(),
            limits: Default::default(),
            diag: Default::default(),
            defines: Vec::new(),
            script_args: Vec::new(),
        },
        crate::register_all,
    )
}

fn fixture(
    code: &str,
    expected: i32,
) -> Result<(Arc<PureHost>, Interp), Box<dyn std::error::Error>> {
    let host = Arc::new(PureHost::default());
    for (name, contents) in [
        ("Foo.cps", FOO),
        (
            "Foo@noconfig.cps",
            r#"{
              "name":"Foo", "configuration":"noconfig", "components":{"bar":{
                "location":"@prefix@/lib/libbar.a",
                "link_location":"@prefix@/lib/bar.imp",
                "link_languages":["cpp"]
              }}
            }"#,
        ),
        (
            "Foo-extra.cps",
            r#"{
              "name":"Foo", "components":{"extra":{"type":"interface", "requires":[":bar"]}}
            }"#,
        ),
        (
            "Foo-skipped.cps",
            r#"{
              "name":"Foo", "requires":{"Absent":null}, "components":{"skipped":{"type":"interface"}}
            }"#,
        ),
    ] {
        host.seed_file(&Path::new(ROOT).join(name), contents.as_bytes())?;
    }
    host.seed_file(
            Path::new("/work/prefix/share/cps/Dep/dep.cps"),
            br#"{"name":"Dep","cps_version":"0.15.0","prefix":"/work/prefix","components":{"api":{"type":"interface"}}}"#,
        )?;
    host.seed_file(
            Path::new("/work/prefix/cps/custom/custom.cps"),
            br#"{"name":"Custom","cps_version":"0.15.0","prefix":"/work/prefix","version_schema":"custom","version":"42","components":{"api":{"type":"interface"}}}"#,
        )?;
    let src = Path::new("/work/src");
    let code =
        format!("cmake_minimum_required(VERSION 4.5)\nset(CMAKE_PREFIX_PATH /work/prefix)\n{code}");
    host.seed_file(&src.join("CMakeLists.txt"), code.as_bytes())?;
    let mut interp = interpreter(host.clone(), Path::new("/work"));
    let outcome = interp.run_configure(src, Path::new("/work/build"), &[]);
    assert_eq!(
        outcome.exit_code,
        expected,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    Ok((host, interp))
}

#[test]
fn imports_configurations_usage_requirements_and_default_target() -> TestResult {
    fixture(
        r#"
find_package(Foo CONFIG REQUIRED GLOBAL)
if(NOT Foo_FOUND OR NOT Dep_FOUND OR NOT Foo_bar_FOUND OR NOT Foo_extra_FOUND
   OR NOT TARGET Foo OR NOT TARGET Foo::extra OR NOT TARGET Dep::api OR TARGET Foo::skipped)
  message(FATAL_ERROR "Expected imported CPS targets")
endif()
if(NOT Foo_VERSION STREQUAL "2.4.3+suffix" OR NOT Foo_VERSION_MAJOR EQUAL 2
   OR NOT Foo_VERSION_MINOR EQUAL 4 OR NOT Foo_VERSION_PATCH EQUAL 3
   OR NOT Foo_VERSION_TWEAK EQUAL 0 OR NOT Foo_DIR STREQUAL "/work/prefix/lib/cps/Foo"
   OR NOT Foo_CONFIG STREQUAL "/work/prefix/lib/cps/Foo/Foo.cps")
  message(FATAL_ERROR "Unexpected CPS package variables")
endif()
get_target_property(location Foo::bar IMPORTED_LOCATION_NOCONFIG)
get_target_property(implib Foo::bar IMPORTED_IMPLIB_NOCONFIG)
get_target_property(langs Foo::bar IMPORTED_LINK_INTERFACE_LANGUAGES_NOCONFIG)
get_target_property(configs Foo::bar IMPORTED_CONFIGURATIONS)
get_target_property(includes Foo::bar INTERFACE_INCLUDE_DIRECTORIES)
get_target_property(system_includes Foo::bar INTERFACE_SYSTEM_INCLUDE_DIRECTORIES)
get_target_property(defs Foo::bar INTERFACE_COMPILE_DEFINITIONS)
get_target_property(options Foo::bar INTERFACE_COMPILE_OPTIONS)
get_target_property(link_options Foo::bar INTERFACE_LINK_OPTIONS)
get_target_property(default_link Foo INTERFACE_LINK_LIBRARIES)
get_target_property(internal_link Foo::extra INTERFACE_LINK_LIBRARIES)
get_target_property(external_link Foo::bar INTERFACE_LINK_LIBRARIES)
get_target_property(global Foo::bar IMPORTED_GLOBAL)
if(NOT global OR NOT includes STREQUAL "/work/prefix/include"
   OR NOT system_includes STREQUAL "/work/prefix/include"
   OR NOT location STREQUAL "/work/prefix/lib/libbar.a"
   OR NOT implib STREQUAL "/work/prefix/lib/bar.imp" OR NOT langs STREQUAL "CXX"
   OR NOT configs STREQUAL "NOCONFIG" OR NOT defs STREQUAL "HAVE_FOO"
   OR NOT options STREQUAL "-Wall;$<$<COMPILE_LANGUAGE:CXX>:-Wextra>"
   OR NOT link_options STREQUAL "-flag" OR NOT default_link STREQUAL "Foo::bar"
   OR NOT external_link STREQUAL "Dep::api" OR NOT internal_link STREQUAL "Foo::bar")
  message(FATAL_ERROR "Unexpected imported properties")
endif()
"#,
        0,
    )?;
    Ok(())
}

#[test]
fn checks_versions_and_selects_appendices_on_each_call() -> TestResult {
    fixture(
        r#"
find_package(Custom 42.0 QUIET CONFIG)
if(Custom_FOUND)
  message(FATAL_ERROR "Custom versions require an exact string match")
endif()
find_package(Custom 42 CONFIG REQUIRED)
if(NOT Custom_VERSION STREQUAL "42" OR NOT Custom_VERSION_COUNT EQUAL 0)
  message(FATAL_ERROR "Unexpected custom version")
endif()
find_package(Foo 2.1...<2.4.3 QUIET CONFIG)
if(Foo_FOUND OR TARGET Foo::bar)
  message(FATAL_ERROR "CPS version exceeds the exclusive limit")
endif()
find_package(Foo 2.4.3 EXACT CONFIG REQUIRED COMPONENTS bar OPTIONAL_COMPONENTS skipped)
if(NOT Foo_FOUND OR Foo_skipped_FOUND OR TARGET Foo::extra)
  message(FATAL_ERROR "Unexpected CPS appendix")
endif()
find_package(Foo 2.1...2.4.3 CONFIG REQUIRED COMPONENTS extra)
if(NOT Foo_extra_FOUND OR NOT TARGET Foo::extra)
  message(FATAL_ERROR "Expected requested CPS appendix")
endif()
find_package(Foo QUIET CONFIG CONFIGS FooConfig.cmake)
if(Foo_FOUND)
  message(FATAL_ERROR "CONFIGS must suppress CPS discovery")
endif()
"#,
        0,
    )?;
    let (host, mut interp) = fixture("find_package(Foo CONFIG REQUIRED COMPONENTS absent)", 1)?;
    let stderr = host.stream_bytes(Stream::Stderr);
    assert!(stderr.contains_str("Required component was not found: absent."));
    assert!(
        interp
            .cx()
            .project()
            .find_target(DirId::ROOT, b"Foo::bar")
            .is_none()
    );
    Ok(())
}

struct Scratch {
    host: Arc<NativeHost>,
    path: PathBuf,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = self.host.remove_dir_all(&self.path);
    }
}

#[test]
fn imports_package_info_installed_by_cmake_4_5_1() -> TestResult {
    let host = Arc::new(NativeHost::new()?);
    let Some(cmake) = host.env_get(b"RCMAKE_TEST_REFERENCE_CMAKE") else {
        return Ok(());
    };
    let now = host.now();
    let path = std::env::temp_dir().join(format!(
        "rcmake-cps-{}-{}-{}",
        host.system().pid,
        now.secs,
        now.nanos
    ));
    host.create_dir_all(&path)?;
    let scratch = Scratch {
        host: host.clone(),
        path,
    };
    let root = host.canonicalize(&scratch.path)?;
    let run = |args| {
        let output = host.run(
            &RunRequest {
                pipeline: vec![CommandSpec {
                    program: cmake.clone(),
                    args,
                }],
                cwd: root.clone(),
                env: host.env_all(),
                stdin: Input::Null,
                stdout: Output::Capture,
                stderr: Output::Capture,
                timeout: Some(Duration::from_secs(30)),
                tee: false,
            },
            &mut |_, _| {},
        )?;
        assert_eq!(
            output.results,
            [ProcessResult::Exited(0)],
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok::<_, rcmake_host::HostError>(output.stdout)
    };
    assert!(run(vec!["--version".into()])?.starts_with(b"cmake version 4.5.1\n"));
    let producer = root.join("producer");
    host.create_dir_all(&producer)?;
    host.write(
        &producer.join("CMakeLists.txt"),
        br#"
cmake_minimum_required(VERSION 4.5)
project(Producer NONE)
add_library(api INTERFACE)
target_include_directories(api INTERFACE "$<INSTALL_INTERFACE:include>")
install(TARGETS api EXPORT DemoTargets)
install(PACKAGE_INFO Demo EXPORT DemoTargets DESTINATION lib/cps/Demo
        VERSION 2.3 COMPAT_VERSION 2.0 DEFAULT_TARGETS api LOWER_CASE_FILE)
"#,
        WriteMode::CreateNew,
    )?;
    run(vec![
        "-S".into(),
        "producer".into(),
        "-B".into(),
        "build".into(),
    ])?;
    let prefix = root.join("prefix");
    run(vec![
        "--install".into(),
        "build".into(),
        "--prefix".into(),
        path::from_native(&prefix),
    ])?;
    let consumer = root.join("consumer");
    host.create_dir_all(&consumer)?;
    host.write(
        &consumer.join("CMakeLists.txt"),
        br#"
cmake_minimum_required(VERSION 4.5)
find_package(Demo 2.1 CONFIG REQUIRED PATHS "${CMAKE_CURRENT_SOURCE_DIR}/../prefix" NO_DEFAULT_PATH)
if(NOT TARGET Demo::api OR NOT TARGET Demo OR NOT Demo_VERSION STREQUAL "2.3")
  message(FATAL_ERROR "Expected installed CPS targets")
endif()
"#,
        WriteMode::CreateNew,
    )?;
    let mut interp = interpreter(host, &root);
    assert_eq!(
        interp
            .run_configure(&consumer, &root.join("consumer-build"), &[])
            .exit_code,
        0
    );
    let mut cx = interp.cx();
    let project = cx.project();
    let api = project
        .find_target(DirId::ROOT, b"Demo::api")
        .ok_or("missing API target")?;
    let target = project.target(api).ok_or("missing imported target")?;
    assert_eq!(
        target.usage.joined(UsageField::InterfaceIncludeDirectories),
        Some(path::from_native_cmake(&prefix.join("include")))
    );
    Ok(())
}

#[test]
fn selected_cps_missing_dependency_fails_without_fallback() -> TestResult {
    let host = Arc::new(PureHost::default());
    host.seed_file(
        Path::new("/work/first/lib/cps/Foo/Foo.cps"),
        br#"{
        "cps_version":"0.15.0", "name":"Foo", "cps_path":"@prefix@/lib/cps/Foo",
        "requires":{"Bar":null}, "components":{"core":{"type":"interface"}}
    }"#,
    )?;
    host.seed_file(Path::new("/work/later/FooConfig.cmake"), b"set(FALLBACK 1)")?;
    host.seed_file(
        Path::new("/work/CMakeLists.txt"),
        br#"
        cmake_minimum_required(VERSION 4.5)
        project(Consumer NONE)
        find_package(Foo CONFIG QUIET NO_DEFAULT_PATH PATHS /work/first /work/later)
    "#,
    )?;
    let mut interp = interpreter(host.clone(), Path::new("/work"));
    let outcome = interp.run_configure(Path::new("/work"), Path::new("/work/build"), &[]);
    let mut cx = interp.cx();
    assert_eq!(
        outcome.exit_code,
        1,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    assert!(cx.var_is_on(b"Foo_FOUND"));
    assert!(!cx.var_defined(b"FALLBACK"));
    assert!(
        cx.project()
            .find_target(DirId::ROOT, b"Foo::core")
            .is_none()
    );
    assert!(
        !host
            .stream_bytes(Stream::Stderr)
            .contains_str("find_package find_package")
    );
    assert!(
        host.stream_bytes(Stream::Stderr)
            .contains_str("find_package could not find Bar, required by Foo.")
    );
    Ok(())
}

#[test]
fn imports_cps_features_and_link_requirements_in_order() -> TestResult {
    let host = Arc::new(PureHost::default());
    host.seed_file(Path::new("/work/prefix/lib/cps/Foo/Foo.cps"), br#"{
        "cps_version":"0.15.0", "name":"Foo", "cps_path":"@prefix@/lib/cps/Foo",
        "components":{
            "api":{"type":"interface", "compile_features":["c11","C++20","C99","gnu","c++2","THREADS"],
                   "link_features":["THREAD","unknown"], "requires":[":core"],
                   "compile_requires":[":headers"], "link_requires":[":runtime"],
                   "link_libraries":["m","z"]},
            "core":{"type":"interface"},
            "headers":{"type":"interface"},
            "runtime":{"type":"interface"}
        }
    }"#)?;
    let source = Path::new("/work/src");
    host.seed_file(
        &source.join("CMakeLists.txt"),
        br#"
        cmake_minimum_required(VERSION 4.5)
        add_library(Threads::Threads INTERFACE IMPORTED)
        find_package(Foo CONFIG REQUIRED COMPONENTS api NO_DEFAULT_PATH PATHS /work/prefix)
        get_target_property(features Foo::api INTERFACE_COMPILE_FEATURES)
        get_target_property(links Foo::api INTERFACE_LINK_LIBRARIES)
    "#,
    )?;
    let mut interp = interpreter(host.clone(), Path::new("/work"));
    let outcome = interp.run_configure(source, Path::new("/work/build"), &[]);
    assert_eq!(
        outcome.exit_code,
        0,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    let cx = interp.cx();
    assert_eq!(cx.var_or_empty(b"features"), "c_std_11;cxx_std_20;c_std_99");
    assert_eq!(
        cx.var_or_empty(b"links"),
        "Threads::Threads;Threads::Threads;Foo::core;$<COMPILE_ONLY:Foo::headers>;$<LINK_ONLY:Foo::runtime>;m;z"
    );
    Ok(())
}
