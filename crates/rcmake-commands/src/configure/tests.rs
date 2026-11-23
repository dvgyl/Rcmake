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
