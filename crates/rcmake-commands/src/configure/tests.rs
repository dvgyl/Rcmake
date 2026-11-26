use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcmake_base::BString;
use rcmake_diag::DiagOptions;
use rcmake_eval::{CacheDefine, Interp, InterpConfig, RunMode};
use rcmake_host::{Follow, Host, PureHost, Stream};
use rcmake_model::CacheType;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const ROOT: &str = "/share/cmake-4.5/Modules";
const INFO_DIR: &str = "/work/build/CMakeFiles/4.5.1";

fn interp(host: &Arc<PureHost>, defines: Vec<CacheDefine>) -> Interp {
    Interp::new(
        InterpConfig {
            services: Default::default(),
            host: host.clone(),
            mode: RunMode::Configure,
            cmake_root: "/share/cmake-4.5".into(),
            cmake_command: "/bin/cmake".into(),
            cwd: "/work".into(),
            limits: Default::default(),
            diag: DiagOptions::default(),
            defines,
            script_args: Vec::new(),
        },
        crate::register_all,
    )
}

fn new_host() -> Arc<PureHost> {
    Arc::new(PureHost::new(
        PathBuf::from("/work"),
        PathBuf::from("/bin/cmake"),
    ))
}

fn log(name: &str) -> String {
    format!("set_property(GLOBAL APPEND PROPERTY BOOT_LOG {name})\n")
}

/// Seed a fake `Modules/` tree whose files log their names and set the variables that
/// the real modules set.
fn seed_modules(host: &PureHost, languages: &[&str]) -> TestResult {
    let module = |name: &str, body: &str| {
        host.seed_file(
            &Path::new(ROOT).join(name),
            format!("{}{body}", log(name.trim_end_matches(".cmake"))).as_bytes(),
        )
    };
    module(
        "CMakeDetermineSystem.cmake",
        &format!(
            "file(WRITE ${{CMAKE_PLATFORM_INFO_DIR}}/CMakeSystem.cmake \"set(CMAKE_SYSTEM Fake-1)\\n\
             set(CMAKE_SYSTEM_NAME Fake)\\nset(CMAKE_SYSTEM_LOADED 1)\\n{}\")\n",
            log("CMakeSystem")
        ),
    )?;
    module(
        "CMakeUnixFindMake.cmake",
        "set(CMAKE_MAKE_PROGRAM /bin/make CACHE FILEPATH \"\")\n",
    )?;
    module(
        "CMakeSystemSpecificInitialize.cmake",
        "set(CMAKE_SYSTEM_SPECIFIC_INITIALIZE_LOADED 1)\n",
    )?;
    module(
        "CMakeSystemSpecificInformation.cmake",
        "set(CMAKE_SYSTEM_SPECIFIC_INFORMATION_LOADED 1)\n",
    )?;
    for lang in languages {
        let compiler = if *lang == "Missing" {
            String::new()
        } else {
            format!("set(CMAKE_{lang}_COMPILER /bin/cc)\\n")
        };
        module(
            &format!("CMakeDetermine{lang}Compiler.cmake"),
            &format!(
                "file(WRITE ${{CMAKE_PLATFORM_INFO_DIR}}/CMake{lang}Compiler.cmake \"{compiler}\
                 set(CMAKE_{lang}_COMPILER_LOADED 1)\\n{}\")\n",
                log(&format!("CMake{lang}Compiler"))
            ),
        )?;
        module(
            &format!("CMake{lang}Information.cmake"),
            &format!("set(CMAKE_{lang}_INFORMATION_LOADED 1)\n"),
        )?;
        module(
            &format!("CMakeTest{lang}Compiler.cmake"),
            &format!("set(CMAKE_{lang}_COMPILER_WORKS TRUE)\n"),
        )?;
    }
    host.seed_file(Path::new("/bin/cc"), b"")?;
    Ok(())
}

fn configure(host: &PureHost, interp: &mut Interp, script: &str, code: i32) -> TestResult {
    host.seed_file(Path::new("/work/src/CMakeLists.txt"), script.as_bytes())?;
    let outcome = interp.run_configure(Path::new("/work/src"), Path::new("/work/build"), &[]);
    assert_eq!(
        outcome.exit_code,
        code,
        "{}",
        host.stream_bytes(Stream::Stderr)
    );
    Ok(())
}

fn global(interp: &mut Interp, name: &str) -> Option<BString> {
    interp.cx().global().props.get(name.as_bytes()).cloned()
}

fn var(interp: &mut Interp, name: &str) -> Option<BString> {
    interp.cx().var(name.as_bytes())
}

fn define(name: &str, value: &str) -> CacheDefine {
    CacheDefine {
        name: name.into(),
        ty: Some(CacheType::Internal),
        value: value.into(),
    }
}

#[test]
fn project_runs_the_bootstrap_modules_in_cmake_order() -> TestResult {
    let host = new_host();
    seed_modules(&host, &["C", "CXX"])?;
    let mut first = interp(&host, Vec::new());
    configure(&host, &mut first, "project(P)", 0)?;
    assert_eq!(
        global(&mut first, "BOOT_LOG"),
        Some(
            "CMakeDetermineSystem;CMakeSystem;CMakeUnixFindMake;CMakeSystemSpecificInitialize;\
             CMakeDetermineCCompiler;CMakeCCompiler;CMakeDetermineCXXCompiler;CMakeCXXCompiler;\
             CMakeSystemSpecificInformation;CMakeCInformation;CMakeTestCCompiler;\
             CMakeCXXInformation;CMakeTestCXXCompiler"
                .into()
        )
    );
    assert_eq!(
        global(&mut first, "ENABLED_LANGUAGES"),
        Some("C;CXX".into())
    );
    assert_eq!(global(&mut first, "IN_TRY_COMPILE"), Some("0".into()));
    assert!(
        global(&mut first, "CMAKE_CXX_KNOWN_FEATURES")
            .is_some_and(|features| features.starts_with(b"cxx_std_98;"))
    );
    assert_eq!(
        var(&mut first, "CMAKE_PLATFORM_INFO_DIR"),
        Some(INFO_DIR.into())
    );
    assert_eq!(var(&mut first, "CMAKE_SYSTEM_NAME"), Some("Fake".into()));
    assert_eq!(var(&mut first, "CMAKE_CFG_INTDIR"), Some(".".into()));
    assert_eq!(var(&mut first, "RUN_CONFIGURE"), Some("ON".into()));
    assert_eq!(
        first
            .cx()
            .cache()
            .get(b"CMAKE_PLATFORM_INFO_INITIALIZED")
            .map(|entry| entry.value.clone()),
        Some("1".into())
    );

    // A warm reconfigure reads the saved system and compiler files and skips the tests.
    let mut warm = interp(&host, vec![define("CMAKE_PLATFORM_INFO_INITIALIZED", "1")]);
    configure(&host, &mut warm, "project(P C)", 0)?;
    assert_eq!(
        global(&mut warm, "BOOT_LOG"),
        Some(
            "CMakeSystem;CMakeUnixFindMake;CMakeSystemSpecificInitialize;CMakeCCompiler;\
             CMakeSystemSpecificInformation;CMakeCInformation"
                .into()
        )
    );

    // Without the cache mark, the saved platform information is discarded.
    let mut reset = interp(&host, Vec::new());
    configure(&host, &mut reset, "project(P C)", 0)?;
    assert!(
        global(&mut reset, "BOOT_LOG").is_some_and(|log| log.starts_with(b"CMakeDetermineSystem;"))
    );
    Ok(())
}

#[test]
fn project_none_determines_the_system_only_once() -> TestResult {
    let host = new_host();
    seed_modules(&host, &["C"])?;
    let mut session = interp(&host, Vec::new());
    configure(
        &host,
        &mut session,
        "project(P NONE)\nset(after_none \"${CMAKE_SYSTEM_NAME}\")\nenable_language(C)",
        0,
    )?;
    assert_eq!(var(&mut session, "after_none"), Some("Fake".into()));
    assert_eq!(
        global(&mut session, "BOOT_LOG"),
        Some(
            "CMakeDetermineSystem;CMakeSystem;CMakeUnixFindMake;CMakeSystemSpecificInitialize;\
             CMakeSystemSpecificInformation;CMakeDetermineCCompiler;CMakeCCompiler;\
             CMakeCInformation;CMakeTestCCompiler"
                .into()
        )
    );
    assert_eq!(
        global(&mut session, "ENABLED_LANGUAGES"),
        Some("C;NONE".into())
    );
    Ok(())
}

#[test]
fn project_none_without_modules_describes_the_host() -> TestResult {
    let host = new_host();
    let mut session = interp(&host, Vec::new());
    configure(&host, &mut session, "project(P LANGUAGES)", 0)?;
    assert_eq!(
        global(&mut session, "ENABLED_LANGUAGES"),
        Some("NONE".into())
    );
    assert_eq!(
        var(&mut session, "CMAKE_SYSTEM_NAME"),
        Some("PureOS".into())
    );
    Ok(())
}

#[test]
fn missing_compiler_fails_unless_optional() -> TestResult {
    let host = new_host();
    seed_modules(&host, &["Missing"])?;
    let mut required = interp(&host, Vec::new());
    configure(&host, &mut required, "project(P Missing)", 1)?;
    let stderr = host.stream_bytes(Stream::Stderr).to_string();
    assert!(
        stderr.contains("No CMAKE_Missing_COMPILER could be found."),
        "{stderr}"
    );
    assert!(
        host.metadata(
            &Path::new(INFO_DIR).join("CMakeMissingCompiler.cmake"),
            Follow::Yes
        )
        .is_err()
    );

    let host = new_host();
    seed_modules(&host, &["Missing"])?;
    let mut optional = interp(&host, Vec::new());
    configure(
        &host,
        &mut optional,
        "project(P NONE)\nenable_language(Missing OPTIONAL)",
        0,
    )?;
    assert_eq!(
        global(&mut optional, "ENABLED_LANGUAGES"),
        Some("Missing;NONE".into())
    );
    Ok(())
}

#[test]
fn enable_language_before_project_follows_cmp0165() -> TestResult {
    let host = new_host();
    seed_modules(&host, &["C"])?;
    let mut warn = interp(&host, Vec::new());
    configure(&host, &mut warn, "enable_language(C)", 0)?;
    assert!(
        host.stream_bytes(Stream::Stderr)
            .to_string()
            .contains("project() should be called prior to this enable_language() call.")
    );

    let host = new_host();
    let mut new = interp(&host, Vec::new());
    configure(
        &host,
        &mut new,
        "cmake_minimum_required(VERSION 4.5)\nenable_language(C)",
        1,
    )?;
    assert!(
        host.stream_bytes(Stream::Stderr)
            .to_string()
            .contains("project() must be called prior to this enable_language() call.")
    );
    Ok(())
}

#[test]
fn project_include_hooks_run_around_the_bootstrap() -> TestResult {
    let host = new_host();
    seed_modules(&host, &["C"])?;
    for (file, name) in [
        ("/work/before.cmake", "before"),
        ("/work/named_before.cmake", "named_before"),
        ("/work/top.cmake", "top"),
        ("/work/after.cmake", "after"),
        ("/work/named_after.cmake", "named_after"),
    ] {
        let body = format!("{}set({name}_project \"${{PROJECT_NAME}}\")\n", log(name));
        host.seed_file(Path::new(file), body.as_bytes())?;
    }
    let mut session = interp(&host, Vec::new());
    configure(
        &host,
        &mut session,
        "set(CMAKE_PROJECT_INCLUDE_BEFORE /work/before.cmake)\n\
         set(CMAKE_PROJECT_P_INCLUDE_BEFORE /work/named_before.cmake)\n\
         set(CMAKE_PROJECT_TOP_LEVEL_INCLUDES /work/top.cmake)\n\
         set(CMAKE_PROJECT_INCLUDE /work/after.cmake)\n\
         set(CMAKE_PROJECT_P_INCLUDE /work/named_after.cmake)\n\
         project(P C)\nproject(P C)",
        0,
    )?;
    let log = global(&mut session, "BOOT_LOG")
        .unwrap_or_default()
        .to_string();
    assert!(
        log.starts_with(
            "before;named_before;CMakeDetermineSystem;CMakeSystem;CMakeUnixFindMake;top;"
        ),
        "{log}"
    );
    assert!(
        log.ends_with("CMakeTestCCompiler;after;named_after;before;named_before;after;named_after"),
        "{log}"
    );
    assert_eq!(var(&mut session, "before_project"), Some("P".into()));
    assert_eq!(var(&mut session, "after_project"), Some("P".into()));
    Ok(())
}

/// A configure session with a cache byte limit.
fn interp_with_cache_limit(host: &Arc<PureHost>, max_cache_bytes: usize) -> Interp {
    Interp::new(
        InterpConfig {
            services: Default::default(),
            host: host.clone(),
            mode: RunMode::Configure,
            cmake_root: "/share/cmake-4.5".into(),
            cmake_command: "/bin/cmake".into(),
            cwd: "/work".into(),
            limits: rcmake_base::Limits {
                max_cache_bytes,
                ..rcmake_base::Limits::default()
            },
            diag: DiagOptions::default(),
            defines: Vec::new(),
            script_args: Vec::new(),
        },
        crate::register_all,
    )
}

fn assert_cache_refusal(host: &PureHost, limit: usize) {
    let stderr = host.stream_bytes(Stream::Stderr).to_string();
    assert!(
        stderr.contains(&format!(
            "Cache storage would exceed the {limit}-byte limit."
        )),
        "{stderr}"
    );
}

#[test]
fn legacy_build_command_respects_the_cache_byte_limit() -> TestResult {
    let host = new_host();
    let mut session = interp_with_cache_limit(&host, 64 * 1024);
    configure(
        &host,
        &mut session,
        "project(P NONE)\nstring(REPEAT y 70000 big)\nset(CMAKE_COMMAND \"${big}\")\nbuild_command(legacy ignored)\n",
        1,
    )?;
    assert_cache_refusal(&host, 64 * 1024);
    assert!(session.cx().cache().get(b"legacy").is_none());
    Ok(())
}

#[test]
fn project_stops_before_normal_variables_when_the_cache_refuses() -> TestResult {
    let host = new_host();
    let mut session = interp_with_cache_limit(&host, 32 * 1024);
    let name = "p".repeat(40_000);
    configure(&host, &mut session, &format!("project({name} NONE)\n"), 1)?;
    assert_cache_refusal(&host, 32 * 1024);
    assert_eq!(var(&mut session, "PROJECT_NAME"), None);
    assert_eq!(var(&mut session, &format!("{name}_SOURCE_DIR")), None);
    Ok(())
}

#[test]
fn project_named_cmake_project_leaves_no_shadowing_normal_binding() -> TestResult {
    let host = new_host();
    let mut session = interp(&host, Vec::new());
    configure(
        &host,
        &mut session,
        "cmake_minimum_required(VERSION 4.5)\nproject(CMAKE_PROJECT VERSION 1.2 DESCRIPTION text LANGUAGES NONE)\n",
        0,
    )?;
    let mut cx = session.cx();
    for name in [
        b"CMAKE_PROJECT_VERSION".as_slice(),
        b"CMAKE_PROJECT_DESCRIPTION",
    ] {
        assert_eq!(cx.var_normal_only(name), None, "{}", BString::from(name));
        assert!(cx.cache().get(name).is_some());
    }
    assert_eq!(cx.var(b"CMAKE_PROJECT_VERSION"), Some("1.2".into()));
    Ok(())
}

#[test]
fn subdirectory_project_needs_no_cache_room_for_a_same_name_alias() -> TestResult {
    let script = "project(P NONE)\nadd_subdirectory(sub)\n";
    let sub = b"project(CMAKE_PROJECT VERSION 1.0 LANGUAGES NONE)\n";
    // Measure the cache with room to spare.
    let host = new_host();
    host.seed_file(Path::new("/work/src/sub/CMakeLists.txt"), sub)?;
    let mut session = interp(&host, Vec::new());
    configure(&host, &mut session, script, 0)?;
    let bytes = session.cx().cache().bytes();
    assert!(session.cx().cache().get(b"CMAKE_PROJECT_VERSION").is_none());

    // The alias defines each CMAKE_PROJECT_VERSION* name in the subdirectory, so no cache
    // entry is stored and an exactly full cache is enough.
    let host = new_host();
    host.seed_file(Path::new("/work/src/sub/CMakeLists.txt"), sub)?;
    let mut session = interp_with_cache_limit(&host, bytes);
    configure(&host, &mut session, script, 0)?;
    assert_eq!(session.cx().cache().bytes(), bytes);
    Ok(())
}

#[test]
fn project_reserves_its_cache_entry_against_a_watch_callback() -> TestResult {
    let project = "project(P VERSION 1.2 LANGUAGES NONE)\n";
    let host = new_host();
    let mut session = interp(&host, Vec::new());
    configure(&host, &mut session, project, 0)?;
    let bytes = session.cx().cache().bytes();

    // The watch runs during the last alias write, just before the last cache entry. The
    // filler fits only in the bytes that the project entry reserved.
    let filler = 1000;
    let limit = bytes + "FILLER".len() + filler + 128 - 1;
    let script = format!(
        "function(fill var access value)\n  string(REPEAT x {filler} text)\n  set(FILLER \"${{text}}\" CACHE STRING \"\")\nendfunction()\nvariable_watch(PROJECT_HOMEPAGE_URL fill)\n{project}"
    );
    let host = new_host();
    let mut session = interp_with_cache_limit(&host, limit);
    configure(&host, &mut session, &script, 1)?;
    assert_cache_refusal(&host, limit);
    let mut cx = session.cx();
    assert!(
        cx.cache().get(b"FILLER").is_none(),
        "the watch cannot use reserved bytes"
    );
    assert!(cx.cache().get(b"CMAKE_PROJECT_HOMEPAGE_URL").is_some());
    assert_eq!(cx.cache().reserved_bytes(), 0);
    Ok(())
}

#[test]
fn a_watch_cannot_spend_the_bytes_of_a_removed_reserved_entry() -> TestResult {
    // The project replaces a large existing entry with a small one.
    let old = "o".repeat(500);
    let project = format!(
        "set(CMAKE_PROJECT_HOMEPAGE_URL \"{old}\" CACHE STRING \"\")\nproject(P VERSION 1.2 LANGUAGES NONE)\n"
    );
    let host = new_host();
    let mut session = interp(&host, Vec::new());
    configure(&host, &mut session, &project, 0)?;
    let bytes = session.cx().cache().bytes();

    // The watch removes the old entry and tries to use the freed bytes, which the
    // reservation of the new entry keeps.
    let filler = 1000;
    let limit = bytes + "FILLER".len() + filler + 128 - 1;
    let script = format!(
        "function(fill var access value)\n  unset(CMAKE_PROJECT_HOMEPAGE_URL CACHE)\n  string(REPEAT x {filler} text)\n  set(FILLER \"${{text}}\" CACHE STRING \"\")\nendfunction()\nvariable_watch(PROJECT_HOMEPAGE_URL fill)\n{project}"
    );
    let host = new_host();
    let mut session = interp_with_cache_limit(&host, limit);
    configure(&host, &mut session, &script, 1)?;
    assert_cache_refusal(&host, limit);
    let mut cx = session.cx();
    assert!(
        cx.cache().get(b"FILLER").is_none(),
        "the freed bytes stay reserved"
    );
    assert_eq!(
        cx.cache()
            .get(b"CMAKE_PROJECT_HOMEPAGE_URL")
            .map(|entry| entry.value.clone()),
        Some(BString::default())
    );
    assert_eq!(cx.cache().reserved_bytes(), 0);
    Ok(())
}

#[test]
fn subdirectory_project_checks_the_name_again_after_a_watch_defines_it() -> TestResult {
    let host = new_host();
    host.seed_file(
        Path::new("/work/src/sub/CMakeLists.txt"),
        b"variable_watch(PROJECT_VERSION define)\nproject(Q VERSION 1.0 LANGUAGES NONE)\n",
    )?;
    let mut session = interp(&host, Vec::new());
    configure(
        &host,
        &mut session,
        "project(P LANGUAGES NONE)\nfunction(define var access value)\n  if(access STREQUAL \"MODIFIED_ACCESS\")\n    set(CMAKE_PROJECT_VERSION from-watch PARENT_SCOPE)\n  endif()\nendfunction()\nadd_subdirectory(sub)\n",
        0,
    )?;
    // CMake 4.5.1 stores no entry, because the watch defined the name first.
    let mut cx = session.cx();
    assert!(cx.cache().get(b"CMAKE_PROJECT_VERSION").is_none());
    assert_eq!(cx.cache().reserved_bytes(), 0);
    Ok(())
}

#[test]
fn nested_project_releases_only_its_own_reservation() -> TestResult {
    // The outer project reserves a large entry. A watch runs a nested project, which
    // reserves and stores a small entry for the same key, and then tries to use the
    // rest of the outer floor.
    let url = "u".repeat(500);
    let script = format!(
        "function(nest var access value)\n  get_property(done GLOBAL PROPERTY nested)\n  if(NOT done)\n    set_property(GLOBAL PROPERTY nested 1)\n    project(Q LANGUAGES NONE)\n    string(REPEAT x 1000 text)\n    set(FILLER \"${{text}}\" CACHE STRING \"\")\n  endif()\nendfunction()\nvariable_watch(PROJECT_HOMEPAGE_URL nest)\nproject(P HOMEPAGE_URL \"{url}\" LANGUAGES NONE)\n"
    );
    let host = new_host();
    let mut session = interp(&host, Vec::new());
    configure(&host, &mut session, &script, 0)?;
    let limit = session.cx().cache().bytes() - 1;

    let host = new_host();
    let mut session = interp_with_cache_limit(&host, limit);
    configure(&host, &mut session, &script, 1)?;
    assert_cache_refusal(&host, limit);
    let mut cx = session.cx();
    assert!(
        cx.cache().get(b"FILLER").is_none(),
        "the outer floor stays reserved"
    );
    assert_eq!(
        cx.cache()
            .get(b"CMAKE_PROJECT_HOMEPAGE_URL")
            .map(|entry| entry.value.clone()),
        Some(BString::from(url))
    );
    assert_eq!(cx.cache().reserved_bytes(), 0);
    Ok(())
}

#[test]
fn subdirectory_project_admits_a_name_that_a_watch_removed() -> TestResult {
    let root = "project(P LANGUAGES NONE)\nset(CMAKE_PROJECT_VERSION old)\nfunction(drop var access value)\n  if(access STREQUAL \"MODIFIED_ACCESS\")\n    unset(CMAKE_PROJECT_VERSION PARENT_SCOPE)\n  endif()\nendfunction()\nadd_subdirectory(sub)\n";
    let run = |sub: &[u8], limit: Option<usize>, code| -> Result<_, Box<dyn std::error::Error>> {
        let host = new_host();
        host.seed_file(Path::new("/work/src/sub/CMakeLists.txt"), sub)?;
        let mut session = match limit {
            Some(limit) => interp_with_cache_limit(&host, limit),
            None => interp(&host, Vec::new()),
        };
        configure(&host, &mut session, root, code)?;
        let mut cx = session.cx();
        let bytes = cx.cache().bytes();
        let value = cx
            .cache()
            .get(b"CMAKE_PROJECT_VERSION")
            .map(|entry| entry.value.clone());
        Ok((host, bytes, value))
    };
    let watched: &[u8] =
        b"variable_watch(PROJECT_VERSION drop)\nproject(Q VERSION 1.0 LANGUAGES NONE)\n";

    // CMake stores the entry, because the name is not defined after the alias writes.
    let (_, _, value) = run(watched, None, 0)?;
    assert_eq!(value, Some(BString::from("1.0")));

    // The entry was not reserved, so the ordinary check after the alias writes refuses it.
    let (_, bytes, _) = run(b"project(Q LANGUAGES NONE)\n", None, 0)?;
    let (host, _, value) = run(watched, Some(bytes + 1), 1)?;
    assert_cache_refusal(&host, bytes + 1);
    assert_eq!(value, None);
    Ok(())
}

#[test]
fn find_package_stops_before_normal_variables_when_the_cache_refuses() -> TestResult {
    let host = new_host();
    let mut session = interp_with_cache_limit(&host, 64 * 1024);
    let name = "Q".repeat(40_000);
    configure(
        &host,
        &mut session,
        &format!("project(P NONE)\nfind_package({name} CONFIG QUIET)\n"),
        1,
    )?;
    assert_cache_refusal(&host, 64 * 1024);
    assert_eq!(var(&mut session, &format!("{name}_FOUND")), None);
    assert_eq!(var(&mut session, &format!("{name}_CONFIG")), None);
    Ok(())
}

#[test]
fn build_command_matches_cmake_build_syntax() -> TestResult {
    let host = new_host();
    let mut session = interp(&host, Vec::new());
    configure(
        &host,
        &mut session,
        "project(P NONE)\nbuild_command(plain)\n\
         build_command(full CONFIGURATION Debug TARGET all PARALLEL_LEVEL 4)\n\
         build_command(legacy ignored)",
        0,
    )?;
    assert_eq!(
        var(&mut session, "plain"),
        Some("/bin/cmake --build . --config \"Release\"".into())
    );
    assert_eq!(
        var(&mut session, "full"),
        Some("/bin/cmake --build . --config \"Debug\" --parallel \"4\" --target \"all\"".into())
    );
    assert_eq!(
        session
            .cx()
            .cache()
            .get(b"legacy")
            .map(|entry| (entry.value.clone(), entry.ty)),
        Some((
            "/bin/cmake --build . --config \"Release\"".into(),
            CacheType::String
        ))
    );
    let host = new_host();
    let mut failing = interp(&host, Vec::new());
    configure(
        &host,
        &mut failing,
        "project(P NONE)\nbuild_command(x TARGET t BOGUS)",
        1,
    )?;
    assert!(
        host.stream_bytes(Stream::Stderr)
            .to_string()
            .contains("build_command unknown argument \"BOGUS\"")
    );
    Ok(())
}

#[test]
fn project_sets_xcode_generator_variables() -> TestResult {
    use rcmake_host::{ProcessResult, RunOutput};
    use rcmake_model::{GeneratorKind, ParsedToolset};

    let host = new_host();
    seed_modules(&host, &[])?;
    host.seed_file(
        &Path::new(ROOT).join("CMakeFindXCode.cmake"),
        b"set(CMAKE_MAKE_PROGRAM /usr/bin/xcodebuild CACHE FILEPATH \"\")",
    )?;
    host.set_command_handler(|_, request, _| {
        assert!(matches!(request.pipeline.as_slice(), [command]
            if command.program == b"xcodebuild" && command.args == [BString::from("-version")]));
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            stdout: b"Xcode 27.2\nBuild version 27C12\n".to_vec(),
            stderr: Vec::new(),
        })
    })?;
    let mut interp = interp(&host, Vec::new());
    interp.cx().project().generator = Some(GeneratorKind::Xcode);
    interp.cx().project().generator_spec.toolset = "foo,buildsystem=12".into();

    configure(&host, &mut interp, "project(P NONE)", 0)?;
    for (name, value) in [
        ("XCODE", "1"),
        ("XCODE_VERSION", "27.2"),
        ("CMAKE_XCODE_BUILD_SYSTEM", "12"),
        ("CMAKE_XCODE_PLATFORM_TOOLSET", "foo"),
        (
            "CMAKE_CFG_INTDIR",
            "$(CONFIGURATION)$(EFFECTIVE_PLATFORM_NAME)",
        ),
        (
            "CMAKE_CONFIGURATION_TYPES",
            "Debug;Release;MinSizeRel;RelWithDebInfo",
        ),
        ("CMAKE_GENERATOR_TOOLSET", "foo,buildsystem=12"),
        ("CMAKE_GENERATOR_PLATFORM", ""),
        ("CMAKE_GENERATOR_INSTANCE", ""),
    ] {
        assert_eq!(var(&mut interp, name), Some(value.into()), "{name}");
    }
    assert!(matches!(
        &interp.cx().project().generator_spec.parsed,
        ParsedToolset::Xcode { toolset: Some(toolset), .. } if toolset == b"foo"
    ));
    assert_eq!(
        interp
            .cx()
            .cache()
            .get(b"CMAKE_CONFIGURATION_TYPES")
            .map(|entry| entry.ty),
        Some(CacheType::String)
    );
    Ok(())
}

#[test]
fn enable_language_sets_xcode_defaults_without_xcodebuild() -> TestResult {
    let host = new_host();
    let mut interp = interp(&host, Vec::new());
    interp.cx().project().generator = Some(rcmake_model::GeneratorKind::Xcode);
    configure(
        &host,
        &mut interp,
        "cmake_minimum_required(VERSION 3.29)\nenable_language(NONE)\nproject(P NONE)",
        0,
    )?;
    assert_eq!(var(&mut interp, "XCODE_VERSION"), Some("".into()));
    assert_eq!(
        var(&mut interp, "CMAKE_XCODE_BUILD_SYSTEM"),
        Some("12".into())
    );
    assert_eq!(var(&mut interp, "CMAKE_XCODE_PLATFORM_TOOLSET"), None);
    Ok(())
}

#[test]
fn project_sets_visual_studio_generator_variables() -> TestResult {
    use rcmake_model::{GeneratorKind, VsVersion};

    for (version, toolset, platform, expected_toolset, expected_platform, expected_host) in [
        (VsVersion::Vs17, "", "", "v143", "ARM64", "ARM64"),
        (VsVersion::Vs18, "", "", "v145", "ARM64", "ARM64"),
        (
            VsVersion::Vs18,
            "ClangCL,host=x64",
            "Win32,version=10.0",
            "ClangCL",
            "Win32",
            "x64",
        ),
    ] {
        let host = new_host();
        seed_modules(&host, &[])?;
        let mut interp = interp(
            &host,
            vec![
                define("CMAKE_GENERATOR_INSTANCE", "C:/VS,version="),
                define("CMAKE_MAKE_PROGRAM", "C:/VS/MSBuild.exe"),
            ],
        );
        let mut cx = interp.cx();
        let project = cx.project();
        project.generator = Some(GeneratorKind::VisualStudio(version));
        project.generator_spec.toolset = toolset.into();
        project.generator_spec.platform = platform.into();
        configure(
            &host,
            &mut interp,
            "set(CMAKE_CONFIGURATION_TYPES Profile)\nproject(P NONE)",
            0,
        )?;
        for (name, value) in [
            ("MSVC_IDE", "1"),
            ("CMAKE_VS_PLATFORM_NAME", expected_platform),
            ("CMAKE_VS_PLATFORM_NAME_DEFAULT", "ARM64"),
            ("CMAKE_VS_PLATFORM_TOOLSET", expected_toolset),
            ("CMAKE_VS_PLATFORM_TOOLSET_HOST_ARCHITECTURE", expected_host),
            ("CMAKE_CFG_INTDIR", "$(Configuration)"),
            ("CMAKE_CONFIGURATION_TYPES", "Profile"),
            ("CMAKE_GENERATOR_PLATFORM", platform),
            ("CMAKE_GENERATOR_TOOLSET", toolset),
            ("CMAKE_GENERATOR_INSTANCE", "C:/VS,version="),
        ] {
            assert_eq!(var(&mut interp, name), Some(value.into()), "{name}");
        }
    }
    Ok(())
}

#[test]
fn visual_studio_finds_msbuild_and_preserves_a_user_program() -> TestResult {
    use rcmake_model::{GeneratorKind, VsVersion};

    for (instance, override_program, expected) in [
        ("", None, "/prepared tools/MSBuild.exe"),
        (
            "/custom VS,version=17.8.0.0",
            None,
            "/custom VS/MSBuild/Current/Bin/MSBuild.exe",
        ),
        ("", Some("/explicit/msbuild"), "/explicit/msbuild"),
    ] {
        let host = new_host();
        let separator = if cfg!(windows) { ";" } else { ":" };
        host.env_set(
            b"PATH",
            Some(format!("/missing{separator}/prepared tools").as_bytes()),
        );
        host.seed_file(Path::new("/prepared tools/MSBuild.exe"), b"")?;
        host.seed_file(Path::new("/custom VS/MSBuild/Current/Bin/MSBuild.exe"), b"")?;
        seed_modules(&host, &[])?;
        let definitions = override_program
            .map(|value| define("CMAKE_MAKE_PROGRAM", value))
            .into_iter()
            .collect();
        let mut interp = interp(&host, definitions);
        let mut cx = interp.cx();
        let project = cx.project();
        project.generator = Some(GeneratorKind::VisualStudio(VsVersion::Vs17));
        project.generator_spec.instance = instance.into();
        configure(&host, &mut interp, "project(P NONE)", 0)?;
        assert_eq!(
            var(&mut interp, "CMAKE_MAKE_PROGRAM"),
            Some(expected.into())
        );
        assert_eq!(
            var(&mut interp, "CMAKE_VS_MSBUILD_COMMAND"),
            Some(expected.into())
        );
    }
    Ok(())
}

#[test]
fn visual_studio_uses_msbuild_for_the_requested_instance_version() -> TestResult {
    use rcmake_host::{ProcessResult, RunOutput};
    use rcmake_model::{GeneratorKind, VsVersion};

    let host = new_host();
    let locator = "/programs/Microsoft Visual Studio/Installer/vswhere.exe";
    let selected = "/selected VS/MSBuild/Current/Bin/arm64/MSBuild.exe";
    host.env_set(b"ProgramFiles(x86)", Some(b"/programs"));
    host.env_set(b"VSINSTALLDIR", Some(b"/default VS"));
    host.seed_file(Path::new(locator), b"")?;
    host.seed_file(Path::new(selected), b"")?;
    host.seed_file(
        Path::new("/selected VS/MSBuild/Current/Bin/MSBuild.exe"),
        b"",
    )?;
    host.seed_file(
        Path::new("/default VS/MSBuild/Current/Bin/MSBuild.exe"),
        b"",
    )?;
    host.set_command_handler(move |_, request, _| {
        let Some(command) = request.pipeline.first() else {
            return Err(rcmake_host::HostError::unsupported("Expected vswhere.exe."));
        };
        assert_eq!(
            rcmake_host::path::to_native(&command.program),
            Path::new(locator)
        );
        assert!(command.args.iter().any(|arg| arg == "[17.8.0.0,17.8.0.0]"));
        Ok(RunOutput {
            stdout: b"\xef\xbb\xbf/selected VS\r\n".to_vec(),
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })?;
    seed_modules(&host, &[])?;
    let mut interp = interp(
        &host,
        vec![define("CMAKE_GENERATOR_INSTANCE", "version=17.8.0.0")],
    );
    interp.cx().project().generator = Some(GeneratorKind::VisualStudio(VsVersion::Vs17));
    configure(&host, &mut interp, "project(P NONE)", 0)?;
    assert_eq!(
        var(&mut interp, "CMAKE_MAKE_PROGRAM"),
        Some(selected.into())
    );
    assert_eq!(
        var(&mut interp, "CMAKE_VS_MSBUILD_COMMAND"),
        Some(selected.into())
    );
    Ok(())
}

#[test]
fn file_generate_records_the_policy_at_each_call() -> TestResult {
    let host = new_host();
    let mut interp = interp(&host, vec![]);
    configure(
        &host,
        &mut interp,
        r#"
cmake_minimum_required(VERSION 3.5)
project(P NONE)
file(GENERATE OUTPUT warn.txt CONTENT warn)
cmake_policy(PUSH)
cmake_policy(SET CMP0070 OLD)
file(GENERATE OUTPUT old.txt CONTENT old)
cmake_policy(SET CMP0070 NEW)
file(GENERATE OUTPUT new.txt CONTENT new)
cmake_policy(POP)
"#,
        0,
    )?;
    for (index, policy) in ["Warn", "Old", "New"].into_iter().enumerate() {
        let property = format!("_RCMAKE_FILE_GENERATE_{index}_CMP0070");
        assert_eq!(global(&mut interp, &property), Some(policy.into()));
    }
    Ok(())
}

#[test]
fn install_records_scripts_in_order_and_preserves_empty_destinations() -> TestResult {
    use rcmake_model::InstallKind;

    let host = new_host();
    let mut interp = interp(&host, vec![]);
    configure(
        &host,
        &mut interp,
        r#"
cmake_minimum_required(VERSION 4.5)
project(P NONE)
install(SCRIPT first.cmake COMPONENT "" CODE DESTINATION SCRIPT "$<1:last.cmake>" EXCLUDE_FROM_ALL)
install(DIRECTORY DESTINATION "")
install(DIRECTORY TYPE DATA)
install(FILES notes.txt DESTINATION "" TYPE DATA)
"#,
        0,
    )?;
    let mut cx = interp.cx();
    let [first, code, last, empty, files] = cx.project().installs.as_slice() else {
        return Err("Incorrect install rule count.".into());
    };
    assert_eq!(
        first.kind,
        InstallKind::Script("/work/src/first.cmake".into())
    );
    assert_eq!(code.kind, InstallKind::Code("DESTINATION".into()));
    assert_eq!(
        last.kind,
        InstallKind::Script("/work/src/$<1:last.cmake>".into())
    );
    for rule in [first, code, last] {
        assert_eq!(rule.options.component, Some("".into()));
        assert!(rule.options.exclude_from_all);
    }
    assert!(matches!(&empty.kind, InstallKind::Directory(dirs) if dirs.is_empty()));
    assert_eq!(empty.options.destination, Some("".into()));
    assert_eq!(files.options.destination, Some("share".into()));
    Ok(())
}

#[test]
fn install_targets_uses_default_for_missing_artifact_destination() -> TestResult {
    let host = new_host();
    let mut interp = interp(&host, vec![]);
    configure(
        &host,
        &mut interp,
        r#"
cmake_minimum_required(VERSION 4.5)
project(P NONE)
add_library(sample STATIC sample.c)
install(TARGETS sample ARCHIVE DESTINATION ${unset_dir} LIBRARY DESTINATION ${unset_dir})
install(TARGETS sample DESTINATION "")
"#,
        0,
    )?;
    let mut cx = interp.cx();
    let [implicit, explicit] = cx.project().installs.as_slice() else {
        return Err("Incorrect target install rule count.".into());
    };
    assert!(implicit.options.destination.is_none());
    assert!(explicit.options.destination.is_none());
    Ok(())
}

#[test]
fn project_reports_xcode_generator_spec_errors() -> TestResult {
    use rcmake_host::{ProcessResult, RunOutput};
    use rcmake_model::GeneratorKind;

    for (toolset, platform, instance, body) in [
        (
            "buildsystem=1",
            "",
            "",
            "toolset specification field\n\n    buildsystem=1\n\n  is not allowed with Xcode 27.2.",
        ),
        (
            "bogus=1",
            "",
            "",
            "given toolset specification that contains invalid field 'bogus'.",
        ),
        (
            "",
            "arm64",
            "",
            "does not support platform specification, but platform\n\n    arm64\n\n  was specified.",
        ),
        (
            "",
            "",
            "/IDE",
            "does not support instance specification, but instance\n\n    /IDE\n\n  was specified.",
        ),
        (
            "buildsystem=7",
            "",
            "",
            "toolset specification field\n\n    buildsystem=7\n\n  value is unknown.  It must be '1' or '12'.",
        ),
        (
            "foo,bar",
            "",
            "",
            "given toolset specification\n\n    foo,bar\n\n  that contains a field after the first ',' with no '='.",
        ),
        (
            "foo,buildsystem=12,buildsystem=12",
            "",
            "",
            "given toolset specification\n\n    foo,buildsystem=12,buildsystem=12\n\n  that contains duplicate field key 'buildsystem'.",
        ),
    ] {
        let host = new_host();
        host.set_command_handler(|_, _, _| {
            Ok(RunOutput {
                results: vec![ProcessResult::Exited(0)],
                stdout: b"Xcode 27.2\n".to_vec(),
                stderr: Vec::new(),
            })
        })?;
        let mut interp = interp(&host, Vec::new());
        let mut cx = interp.cx();
        let project = cx.project();
        project.generator = Some(GeneratorKind::Xcode);
        project.generator_spec.toolset = toolset.into();
        project.generator_spec.platform = platform.into();
        project.generator_spec.instance = instance.into();
        configure(
            &host,
            &mut interp,
            "cmake_minimum_required(VERSION 4.5)\nproject(P NONE)",
            1,
        )?;
        assert_generator_error(&host, "Xcode", body);
    }
    Ok(())
}

#[test]
fn project_reports_visual_studio_generator_spec_errors() -> TestResult {
    use rcmake_model::{GeneratorKind, VsVersion};

    for (toolset, platform, instance, field, spec, reason) in [
        (
            "v143,host=bad",
            "",
            "",
            "toolset",
            "v143,host=bad",
            "that contains invalid field 'host=bad'.",
        ),
        (
            "v143,host=x64,host=x86",
            "",
            "",
            "toolset",
            "v143,host=x64,host=x86",
            "that contains duplicate field key 'host'.",
        ),
        (
            "v143,host",
            "",
            "",
            "toolset",
            "v143,host",
            "that contains a field after the first ',' with no '='.",
        ),
        (
            "",
            "x64,bad=1",
            "",
            "platform",
            "x64,bad=1",
            "that contains invalid field 'bad=1'.",
        ),
        (
            "",
            "x64,version=8.1,version=10.0",
            "",
            "platform",
            "x64,version=8.1,version=10.0",
            "that contains duplicate field key 'version'.",
        ),
        (
            "",
            "x64,version",
            "",
            "platform",
            "x64,version",
            "that contains a field after the first ',' with no '='.",
        ),
        (
            "",
            "",
            "C:/VS,bad=1",
            "instance",
            "C:/VS,bad=1",
            "that contains invalid field 'bad=1'.",
        ),
        (
            "",
            "",
            "C:/VS,version",
            "instance",
            "C:/VS,version",
            "that contains a field after the first ',' with no '='.",
        ),
        (
            "",
            "",
            "version=17.0.12345.1,version=17.0.12345.2",
            "instance",
            "version=17.0.12345.1,version=17.0.12345.2",
            "that contains duplicate field key 'version'.",
        ),
        (
            "",
            "x64,bad=1",
            "version=18.0.12345.1",
            "instance",
            "version=18.0.12345.1",
            "but the version field is not 4 integer components starting in 17.",
        ),
    ] {
        let host = new_host();
        let mut interp = interp(&host, vec![define("CMAKE_GENERATOR_INSTANCE", instance)]);
        let mut cx = interp.cx();
        let project = cx.project();
        project.generator = Some(GeneratorKind::VisualStudio(VsVersion::Vs17));
        project.generator_spec.toolset = toolset.into();
        project.generator_spec.platform = platform.into();
        configure(
            &host,
            &mut interp,
            "cmake_minimum_required(VERSION 4.5)\nproject(P NONE)",
            1,
        )?;
        assert_generator_error(
            &host,
            "Visual Studio 17 2022",
            &format!("given {field} specification\n\n    {spec}\n\n  {reason}"),
        );
    }
    Ok(())
}

fn assert_generator_error(host: &PureHost, generator: &str, body: &str) {
    let stderr = host.stream_bytes(Stream::Stderr);
    let error = format!(
        "CMake Error at CMakeLists.txt:2 (project):\n  Generator\n\n    {generator}\n\n  {body}"
    );
    assert!(
        stderr
            .windows(error.len())
            .any(|part| part == error.as_bytes()),
        "{stderr}"
    );
}

fn stderr(host: &PureHost) -> String {
    host.stream_bytes(Stream::Stderr).to_string()
}

#[test]
fn ninja_version_validation_rejects_old_and_invalid_output_and_accepts_valid() -> TestResult {
    use rcmake_host::{HostError, ProcessResult, RunOutput};
    use rcmake_model::GeneratorKind;

    let run_with_version = |response: Result<&'static str, &'static str>| -> Result<(i32, String), Box<dyn std::error::Error>> {
        let host = new_host();
        seed_modules(&host, &[])?;
        host.seed_file(
            &Path::new(ROOT).join("CMakeNinjaFindMake.cmake"),
            b"set(CMAKE_MAKE_PROGRAM /bin/ninja CACHE FILEPATH \"\")\n",
        )?;
        host.set_command_handler(move |_, req, _| {
            let [cmd] = req.pipeline.as_slice() else {
                return Err(HostError::unsupported("unexpected pipeline"));
            };
            assert_eq!(cmd.program, "/bin/ninja");
            assert_eq!(cmd.args.as_slice(), [BString::from("--version")]);
            match response {
                Ok(stdout) => Ok(RunOutput {
                    stdout: stdout.as_bytes().to_vec(),
                    results: vec![ProcessResult::Exited(0)],
                    ..RunOutput::default()
                }),
                Err(msg) => Err(HostError::unsupported(msg)),
            }
        })?;
        let mut session = interp(&host, Vec::new());
        session.cx().project().generator = Some(GeneratorKind::Ninja);
        host.seed_file(
            Path::new("/work/src/CMakeLists.txt"),
            b"cmake_minimum_required(VERSION 4.5)\nproject(NinjaVer NONE)\n",
        )?;
        let outcome = session.run_configure(Path::new("/work/src"), Path::new("/work/build"), &[]);
        Ok((outcome.exit_code, stderr(&host)))
    };

    for valid in [
        "1.12.1\n",
        "1.3\n",
        "1.11.1.git.kitware.jobserver-1\n",
        "1.10.2.git\n",
        "1.11.0.dyndep-1\n",
        "1.12.0-rc1\n",
    ] {
        let (code, err) = run_with_version(Ok(valid))?;
        assert_eq!(code, 0, "valid={valid:?} err={err}");
    }

    for (bad, detected) in [
        ("1.2.0\n", "1.2.0"),
        ("1.11.\n", "1.11."),
        ("1.11.foo\n", "1.11.foo"),
        ("1.11.1.\n", "1.11.1."),
        ("1.11..1\n", "1.11..1"),
        ("1.12.0-\n", "1.12.0-"),
        ("1.11.1a\n", "1.11.1a"),
        ("not-a-version\n", "not-a-version"),
        ("", ""),
    ] {
        let (code, err) = run_with_version(Ok(bad))?;
        assert_eq!(code, 1, "bad={bad:?}");
        let normalized = err.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            normalized.contains(&format!(
                "The detected version of Ninja ({detected}) is less than the version of Ninja required by CMake (1.3)."
            )),
            "bad={bad:?} err={err}"
        );
    }

    let (code, err) = run_with_version(Err("cannot run ninja"))?;
    assert_eq!(code, 1, "err={err}");
    let normalized = err.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        normalized.contains("Running '/bin/ninja' '--version' failed with: cannot run ninja"),
        "{err}"
    );
    Ok(())
}

#[cfg(all(feature = "native", any(unix, windows)))]
#[test]
fn ninja_wrapper_script_is_not_reused_across_directories_via_build_tool_facts() -> TestResult {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use rcmake_host::{HostError, ProcessResult, RunOutput};
    use rcmake_model::GeneratorKind;

    use crate::configure::build_tool_facts::BuildToolFacts;

    let run_case = |binary_bytes: &[u8],
                    extra_env: Option<(&[u8], &[u8])>|
     -> Result<(usize, Vec<PathBuf>, bool), Box<dyn std::error::Error>> {
        let host = new_host();
        seed_modules(&host, &[])?;
        let ninja_path = Path::new("/bin/ninja");
        host.seed_file(ninja_path, binary_bytes)?;
        if let Some((key, value)) = extra_env {
            host.env_set(key, Some(value));
        }
        host.seed_file(
            &Path::new(ROOT).join("CMakeNinjaFindMake.cmake"),
            b"set(CMAKE_MAKE_PROGRAM /bin/ninja CACHE FILEPATH \"\")\n",
        )?;
        host.seed_file(
            Path::new("/work/src/CMakeLists.txt"),
            b"cmake_minimum_required(VERSION 4.5)\nproject(NinjaReuse NONE)\n",
        )?;
        let launches = Arc::new(AtomicUsize::new(0));
        let seen_cwds = Arc::new(Mutex::new(Vec::<PathBuf>::new()));
        let launches_clone = launches.clone();
        let cwds_clone = seen_cwds.clone();
        host.set_command_handler(move |_, req, _| {
            launches_clone.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut guard) = cwds_clone.lock() {
                guard.push(req.cwd.clone());
            }
            let [cmd] = req.pipeline.as_slice() else {
                return Err(HostError::unsupported("unexpected pipeline"));
            };
            assert_eq!(cmd.program, "/bin/ninja");
            assert_eq!(cmd.args.as_slice(), [BString::from("--version")]);
            Ok(RunOutput {
                stdout: b"1.12.1\n".to_vec(),
                results: vec![ProcessResult::Exited(0)],
                ..RunOutput::default()
            })
        })?;
        let facts = Arc::new(BuildToolFacts::default());
        for build_dir in ["/work/build-a", "/work/build-b"] {
            let mut session = interp(&host, Vec::new());
            session.add_session_data(facts.clone());
            session.cx().project().generator = Some(GeneratorKind::Ninja);
            let outcome = session.run_configure(Path::new("/work/src"), Path::new(build_dir), &[]);
            assert_eq!(outcome.exit_code, 0, "{}", stderr(&host));
        }
        let digest = rcmake_store::file_digest(host.as_ref(), ninja_path)?;
        let sampled = facts
            .sample_ninja(
                host.as_ref(),
                ninja_path,
                digest,
                host.env_all(),
                PathBuf::from("/work/build-c"),
            )
            .is_some();
        let cwds = seen_cwds.lock().map_err(|_| "lock poisoned")?.clone();
        Ok((launches.load(Ordering::Relaxed), cwds, sampled))
    };

    // Shell wrapper must never be cached in BuildToolFacts: runs once per configure in its own cwd.
    let (wrapper_launches, wrapper_cwds, wrapper_sampled) =
        run_case(b"#!/bin/sh\necho 1.12.1\n", None)?;
    assert_eq!(wrapper_launches, 2);
    assert_eq!(
        wrapper_cwds,
        vec![
            PathBuf::from("/work/build-a"),
            PathBuf::from("/work/build-b")
        ]
    );
    assert!(!wrapper_sampled);

    // Native ELF binary under admitted env is cached and reused cleanly on the second configure.
    let (native_launches, _, native_sampled) = run_case(b"\x7fELFninja-binary", None)?;
    assert_eq!(native_launches, 1);
    assert!(native_sampled);

    // Native ELF binary under non-admitted env (LD_PRELOAD) must also stay off the cache path.
    let (preload_launches, _, preload_sampled) = run_case(
        b"\x7fELFninja-binary",
        Some((b"LD_PRELOAD", b"/tmp/hook.so")),
    )?;
    assert_eq!(preload_launches, 2);
    assert!(!preload_sampled);
    Ok(())
}

mod project_install;
