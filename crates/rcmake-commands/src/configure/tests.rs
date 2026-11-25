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
