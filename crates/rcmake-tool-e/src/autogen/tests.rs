use std::{
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use rcmake_base::{BString, hash::HashAlgo};
use rcmake_host::{
    CommandSpec, Follow, Host, HostError, HostResult, ProcessResult, PureHost, RunOutput,
    RunRequest, Stream, path::to_native,
};
use rcmake_json::{ReadOptions, parse, write_compact};

use super::{DATA_LIMIT, ToolCtx, read, run, run_rcc};

const INFO: &str = "/build/autogen.json";
const RCC_INFO: &str = "/build/autorcc.json";

const COMMON: &str = r#"
    "MULTI_CONFIG": false, "USE_BETTER_GRAPH": true, "CROSS_CONFIG": false,
    "CMAKE_SOURCE_DIR": "/src", "CMAKE_CURRENT_SOURCE_DIR": "/src",
    "CMAKE_BINARY_DIR": "/build", "CMAKE_CURRENT_BINARY_DIR": "/build",
    "BUILD_DIR": "/build/qt", "INCLUDE_DIR": "/build/qt/include",
    "SETTINGS_FILE": "/build/settings", "VERBOSITY": 0
"#;

const AUTOGEN: &str = r#"
    "CMAKE_EXECUTABLE": "/bin/cmake", "PARSE_CACHE_FILE": "/build/cache",
    "PARALLEL": 8, "QT_VERSION_MAJOR": 6, "QT_VERSION_MINOR": 8,
    "DEP_FILE": "/build/qt/deps", "DEP_FILE_RULE_NAME": "qt/timestamp",
    "CMAKE_LIST_FILES": ["/src/CMakeLists.txt"], "HEADER_EXTENSIONS": ["h", "hpp"],
    "QT_MOC_EXECUTABLE": "/bin/moc", "MOC_PATH_PREFIX": true,
    "MOC_INCLUDES": ["/src", "/headers"],
    "MOC_DEFINITIONS": ["TEST=1"], "MOC_OPTIONS": ["--no-notes"],
    "MOC_MACRO_NAMES": ["Q_OBJECT"], "MOC_COMPILATION_FILE": "/build/qt/mocs_compilation.cpp",
    "MOC_PREDEFS_CMD": ["/bin/compiler", "-dM", "-E"],
    "MOC_PREDEFS_FILE": "/build/qt/moc_predefs.h",
    "QT_UIC_EXECUTABLE": "/bin/uic", "UIC_OPTIONS": ["--tr", "base", "--no-protection"],
    "UIC_SEARCH_PATHS": ["/ui/preferred", "/ui/other"], "UIC_SKIP": [],
    "UIC_UI_FILES": [["/ui/other/form.ui", ["--tr", "specific", "--no-implicit-includes"]]],
    "HEADERS": [["/src/plain.h", "Mu", "ABC/moc_plain.cpp", null],
                ["/headers/shared.h", "Mu", "XYZ/moc_shared.cpp", null]],
    "SOURCES": [["/src/main.cpp", "MU", null],
                ["/src/other.cpp", "MU", null],
                ["/src/release.cpp", "MU", ["Release"]]]
"#;

const RCC: &str = r#"
    "GENERATOR": "Ninja", "LOCK_FILE": "/build/resource.lock",
    "SOURCE": "/src/form.qrc", "OUTPUT_CHECKSUM": "ABC", "OUTPUT_NAME": "qrc_form.cpp",
    "RCC_LIST_OPTIONS": [], "RCC_EXECUTABLE": "/bin/rcc",
    "OPTIONS": ["-name", "form"], "INPUTS": []
"#;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn host() -> HostResult<PureHost> {
    let host = PureHost::new(fixture_path("/build"), fixture_path("/bin/cmake"));
    for exe in [
        "/bin/cmake",
        "/bin/moc",
        "/bin/uic",
        "/bin/rcc",
        "/bin/compiler",
    ] {
        seed(&host, exe, b"")?;
    }
    Ok(host)
}

fn fixture_path(path: &str) -> PathBuf {
    to_native(&fixture_arg(path))
}

fn fixture_arg(arg: &str) -> BString {
    if cfg!(windows) && arg.starts_with('/') {
        format!("C:{arg}").into()
    } else {
        arg.into()
    }
}

fn seed(host: &PureHost, path: &str, bytes: &[u8]) -> HostResult<()> {
    host.seed_file(&fixture_path(path), bytes)
}

fn seed_info(host: &PureHost, path: &str, kind: &str, extra: &str) -> TestResult {
    let joint = if extra.is_empty() { "" } else { "," };
    let json = format!("{{{COMMON},{kind}{joint}{extra}}}");
    // Read and write this fixture to keep only the last value of an overridden field.
    let json = if cfg!(windows) {
        json.replace("\"/", "\"C:/")
    } else {
        json
    };
    let json = parse(json.as_bytes(), &ReadOptions::default())?;
    seed(host, path, &write_compact(&json))?;
    Ok(())
}

fn context(host: &PureHost) -> ToolCtx<'_> {
    ToolCtx::for_test(
        host,
        Path::new(if cfg!(windows) { "C:/build" } else { "/build" }),
    )
}

fn contents(host: &PureHost, file: &str) -> Result<Vec<u8>, String> {
    read(&context(host), &fixture_path(file), DATA_LIMIT)
}

fn action(host: &PureHost, command: fn(&[BString], &ToolCtx<'_>) -> i32, args: &[&str]) -> i32 {
    command(
        &args.iter().map(|arg| fixture_arg(arg)).collect::<Vec<_>>(),
        &context(host),
    )
}

fn missing_argument(message: &str) -> HostError {
    HostError::from_io(io::Error::new(io::ErrorKind::InvalidInput, message), None)
}

fn first_command(request: &RunRequest) -> HostResult<&CommandSpec> {
    request
        .pipeline
        .first()
        .ok_or_else(|| missing_argument("Expected a command."))
}

fn output_arg(command: &CommandSpec) -> HostResult<PathBuf> {
    command
        .args
        .windows(2)
        .find_map(|pair| match pair {
            [flag, output] if flag == b"-o" => Some(to_native(output)),
            _ => None,
        })
        .ok_or_else(|| missing_argument("Expected an output argument."))
}

fn setup_generators(host: &PureHost) -> HostResult<Arc<Mutex<Vec<RunRequest>>>> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let observer = Arc::clone(&requests);
    host.set_command_handler(move |host, request, _| {
        observer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.clone());
        for command in &request.pipeline {
            if command.program == fixture_arg("/bin/compiler") {
                return Ok(RunOutput {
                    stdout: b"#define BUILT_IN 1\n".to_vec(),
                    stderr: b"Compiler note\n".to_vec(),
                    results: vec![ProcessResult::Exited(0)],
                });
            }
            if command.args.iter().any(|arg| arg == b"--list") {
                assert_eq!(request.cwd, fixture_path("/src"));
                assert_eq!(
                    command.args,
                    [BString::from("--list"), BString::from("form.qrc")]
                );
                return Ok(RunOutput {
                    stdout: b"assets/data.txt\r\nassets/data.txt\r\n".to_vec(),
                    results: vec![ProcessResult::Exited(0)],
                    ..RunOutput::default()
                });
            }
            let output = output_arg(command)?;
            host.seed_file(&output, b"// Generated by Qt\n")?;
            if command.args.iter().any(|arg| arg == b"--output-dep-file") {
                let depfile = super::suffix_path(&output, ".d");
                host.seed_file(
                    &depfile,
                    if cfg!(windows) {
                        b"moc.cpp: C:/headers/plugin\\ data.json C:/headers/hash\\#file\n"
                    } else {
                        b"moc.cpp: /headers/plugin\\ data.json /headers/hash\\#file\n"
                    },
                )?;
            }
        }
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            ..RunOutput::default()
        })
    })?;
    Ok(requests)
}

#[test]
fn generates_moc_and_ui_then_reuses_outputs_and_tracks_moc_dependencies() -> TestResult {
    let host = host()?;
    let requests = setup_generators(&host)?;
    seed_info(&host, INFO, AUTOGEN, "")?;
    seed(&host, "/src/CMakeLists.txt", b"")?;
    seed(&host, "/src/plain.h", b"class Plain {\n  Q_OBJECT\n};\n")?;
    seed(
        &host,
        "/headers/shared.h",
        b"class Shared {\n  Q_OBJECT\n};\n",
    )?;
    seed(&host, "/src/main.cpp", b"class Main {\n Q_OBJECT\n};\n#include \"main.moc\"\n#include \"moc_shared.cpp\"\n#include \"forms/ui_form.h\"\n")?;
    seed(&host, "/src/other.cpp", b"#include <moc_shared.cpp>\n")?;
    seed(&host, "/ui/preferred/forms/form.ui", b"<ui/>")?;
    seed(&host, "/ui/other/form.ui", b"<ui/>")?;
    seed(&host, "/headers/plugin data.json", b"{}")?;
    seed(&host, "/headers/hash#file", b"")?;

    assert_eq!(action(&host, run, &[INFO, "Debug"]), 0);
    let expected_includes = b"#include \"ABC/moc_plain.cpp\"\n";
    assert!(contents(&host, "/build/qt/mocs_compilation.cpp")?.ends_with(expected_includes));
    assert_eq!(
        contents(&host, "/build/qt/moc_predefs.h")?,
        b"#define BUILT_IN 1\n"
    );
    let dependencies = String::from_utf8(contents(&host, "/build/qt/deps")?)?;
    assert!(dependencies.starts_with("qt/timestamp:"));
    assert!(dependencies.contains("/headers/plugin\\ data.json"));
    assert!(dependencies.contains("/headers/hash\\#file"));
    assert!(dependencies.contains("/ui/other/form.ui"));
    assert!(!dependencies.contains("/src/release.cpp"));
    let settings = String::from_utf8(contents(&host, "/build/settings")?)?;
    let expected = if cfg!(windows) {
        b"C:/bin/moc;-DTEST=1;-IC:/src;-IC:/headers;--no-notes;C:/bin/compiler;-dM;-E;Q_OBJECT;"
            .as_slice()
    } else {
        b"/bin/moc;-DTEST=1;-I/src;-I/headers;--no-notes;/bin/compiler;-dM;-E;Q_OBJECT;".as_slice()
    };
    let expected = HashAlgo::Sha256.hex_digest(expected);
    assert!(settings.starts_with(&format!("moc:{expected}\n")));
    let cache = String::from_utf8(contents(&host, "/build/cache")?)?;
    assert!(cache.contains(" mmc:Q_OBJECT\n"));
    assert!(cache.contains(" miu:moc_shared.cpp\n"));
    assert!(cache.contains(" mid:main.moc\n"));
    assert!(cache.contains(" uic:forms/ui_form.h\n"));
    {
        let calls = requests.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(calls.len(), 5);
        for call in calls.iter() {
            assert_eq!(call.timeout, Some(Duration::from_secs(1800)));
        }
        let uic = calls
            .iter()
            .flat_map(|call| &call.pipeline)
            .find(|command| command.program == fixture_arg("/bin/uic"))
            .ok_or("Expected a uic command.")?;
        assert_eq!(
            uic.args,
            [
                "--tr",
                "specific",
                "--no-protection",
                "--no-implicit-includes",
                "-o",
                "/build/qt/include/forms/ui_form.h",
                "/ui/other/form.ui"
            ]
            .map(fixture_arg)
        );
    }

    assert_eq!(action(&host, run, &[INFO, "Debug"]), 0);
    assert_eq!(
        requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len(),
        5
    );
    host.sleep(Duration::from_secs(1));
    seed(&host, "/headers/plugin data.json", b"{\"version\":2}")?;
    assert_eq!(action(&host, run, &[INFO, "Debug"]), 0);
    assert_eq!(
        requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len(),
        8
    );
    Ok(())
}

#[test]
fn uses_output_config_rows_and_executable_config_tools() -> TestResult {
    let host = host()?;
    let requests = setup_generators(&host)?;
    seed_info(
        &host,
        INFO,
        AUTOGEN,
        r#"
        "MULTI_CONFIG": true, "CROSS_CONFIG": true,
        "INCLUDE_DIR_Debug": "/build/qt/include_Debug",
        "SETTINGS_FILE_Debug": "/build/settings_Debug", "PARSE_CACHE_FILE_Debug": "/build/cache_Debug",
        "DEP_FILE_Debug": "/build/qt/deps_Debug", "DEP_FILE_RULE_NAME_Debug": "qt/timestamp_Debug",
        "QT_MOC_EXECUTABLE_Release": "/bin/moc", "QT_MOC_EXECUTABLE_Debug": "/missing/moc",
        "MOC_COMPILATION_FILE_Debug": "/build/qt/mocs_compilation_Debug.cpp",
        "HEADERS": [["/src/plain.h", "Mu", "ABC/moc_plain.cpp", ["Debug"]]],
        "SOURCES": [], "MOC_PREDEFS_CMD": [], "MOC_PREDEFS_FILE": ""
    "#,
    )?;
    seed(&host, "/src/plain.h", b"class Plain {\n Q_OBJECT\n};\n")?;
    assert_eq!(action(&host, run, &[INFO, "Debug", "Release"]), 0);
    let requests = requests.lock().unwrap_or_else(PoisonError::into_inner);
    let request = requests.first().ok_or("Expected a moc request.")?;
    assert_eq!(first_command(request)?.program, fixture_arg("/bin/moc"));
    assert!(
        contents(&host, "/build/qt/mocs_compilation_Debug.cpp")?
            .ends_with(b"#include <ABC/moc_plain.cpp>\n")
    );
    assert!(contents(&host, "/build/qt/deps_Debug")?.starts_with(b"qt/timestamp_Debug:"));
    Ok(())
}

#[test]
fn fails_when_a_source_does_not_include_its_own_moc() -> TestResult {
    let host = host()?;
    let calls = setup_generators(&host)?;
    seed_info(
        &host,
        INFO,
        AUTOGEN,
        r#"
        "HEADERS": [], "SOURCES": [["/src/main.cpp", "Mu", null]]
    "#,
    )?;
    seed(&host, "/src/main.cpp", b"class Main {\n Q_OBJECT\n};\n")?;
    assert_eq!(action(&host, run, &[INFO, ""]), 1);
    assert!(String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).contains("main.moc"));
    assert!(
        calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
    );
    assert!(
        host.metadata(&fixture_path("/build/settings"), Follow::Yes)
            .is_err()
    );
    assert!(
        host.metadata(&fixture_path("/build/qt/timestamp"), Follow::Yes)
            .is_err()
    );
    Ok(())
}

#[test]
fn skips_ui_includes_when_qt_provides_no_uic() -> TestResult {
    let host = host()?;
    let calls = setup_generators(&host)?;
    seed_info(
        &host,
        INFO,
        AUTOGEN,
        r#"
        "QT_UIC_EXECUTABLE": "", "QT_MOC_EXECUTABLE": "",
        "HEADERS": [], "SOURCES": [["/src/main.cpp", "mU", null]]
    "#,
    )?;
    seed(&host, "/src/main.cpp", b"#include \"ui_missing.h\"\n")?;
    assert_eq!(action(&host, run, &[INFO, ""]), 0);
    assert!(
        calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
    );
    Ok(())
}

#[test]
fn regenerates_moc_outputs_after_a_failed_settings_change() -> TestResult {
    let host = host()?;
    let info = r#"
        "SOURCES": [], "MOC_PREDEFS_CMD": [], "QT_UIC_EXECUTABLE": "",
        "HEADERS": [["/src/plain.h", "Mu", "ABC/moc_plain.cpp", null],
                    ["/headers/shared.h", "Mu", "XYZ/moc_shared.cpp", null]]
    "#;
    for path in ["/src/plain.h", "/headers/shared.h"] {
        seed(&host, path, b"class Test {\n Q_OBJECT\n};\n")?;
    }
    host.set_command_handler(|host, request, _| {
        let tool = first_command(request)?;
        let path = output_arg(tool)?;
        let changed = tool.args.iter().any(|arg| arg == b"-DTEST=2");
        if changed && path.ends_with("moc_shared.cpp") {
            return Ok(RunOutput {
                results: vec![ProcessResult::Exited(9)],
                ..Default::default()
            });
        }
        host.seed_file(
            &path,
            if changed {
                b"new settings"
            } else {
                b"old settings"
            },
        )?;
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(0)],
            ..Default::default()
        })
    })?;

    seed_info(&host, INFO, AUTOGEN, info)?;
    assert_eq!(action(&host, run, &[INFO, ""]), 0);
    seed_info(
        &host,
        INFO,
        AUTOGEN,
        &format!(r#"{info}, "MOC_DEFINITIONS": ["TEST=2"]"#),
    )?;
    assert_eq!(action(&host, run, &[INFO, ""]), 1);
    let output = "/build/qt/ABC/moc_plain.cpp";
    assert_eq!(contents(&host, output)?, b"new settings");
    assert!(
        host.metadata(&fixture_path("/build/settings"), Follow::Yes)
            .is_err()
    );
    seed_info(&host, INFO, AUTOGEN, info)?;
    assert_eq!(action(&host, run, &[INFO, ""]), 0);
    assert_eq!(contents(&host, output)?, b"old settings");
    Ok(())
}

#[test]
fn rejects_invalid_rows_and_conflicting_generated_ui_includes() -> TestResult {
    let host = host()?;
    seed_info(
        &host,
        INFO,
        AUTOGEN,
        r#""HEADERS": [["/src/plain.h", "XU", "", null]]"#,
    )?;
    assert_eq!(action(&host, run, &[INFO, ""]), 1);
    assert!(String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).contains("HEADERS[0]"));

    seed_info(
        &host,
        INFO,
        AUTOGEN,
        r#"
        "HEADERS": [],
        "SOURCES": [["/src/a/main.cpp", "mU", null], ["/src/b/main.cpp", "mU", null]]
    "#,
    )?;
    for dir in ["/src/a", "/src/b"] {
        seed(
            &host,
            &format!("{dir}/main.cpp"),
            b"#include \"ui_form.h\"\n",
        )?;
        seed(&host, &format!("{dir}/form.ui"), b"<ui/>")?;
    }
    assert_eq!(action(&host, run, &[INFO, ""]), 1);
    assert!(
        String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr)).contains("conflicting inputs")
    );
    Ok(())
}

#[test]
fn writes_module_output_and_omits_it_from_the_compilation_file() -> TestResult {
    let host = host()?;
    let calls = setup_generators(&host)?;
    seed_info(
        &host,
        INFO,
        AUTOGEN,
        r#"
        "QT_VERSION_MINOR": 13, "HEADERS": [], "SOURCES": [],
        "MOC_PREDEFS_CMD": [], "MOC_PREDEFS_FILE": "",
        "CXX_MODULE_UNITS": [["/src/empty.cppm", "Mu", null, "ABC/moc_empty.cpp"],
                             ["/src/mod.cppm", "Mu", null, "ABC/moc_mod.cpp"]]
    "#,
    )?;
    seed(&host, "/src/empty.cppm", b"export module Empty;")?;
    seed(
        &host,
        "/src/mod.cppm",
        b"export module Mod;\nclass Mod {\n Q_OBJECT\n};\n",
    )?;
    assert_eq!(action(&host, run, &[INFO, ""]), 0);
    assert_eq!(
        calls.lock().unwrap_or_else(PoisonError::into_inner).len(),
        1
    );
    assert_eq!(
        contents(&host, "/build/qt/include/ABC/moc_empty.cpp")?,
        b"enum some_compilers { need_more_than_nothing };\n"
    );
    assert_eq!(
        contents(&host, "/build/qt/include/ABC/moc_mod.cpp")?,
        b"// Generated by Qt\n"
    );
    assert!(
        !String::from_utf8_lossy(&contents(&host, "/build/qt/mocs_compilation.cpp")?)
            .contains("moc_mod")
    );
    Ok(())
}

#[test]
fn generates_rcc_wrapper_and_reuses_payload_under_the_lock() -> TestResult {
    let host = host()?;
    let calls = setup_generators(&host)?;
    seed_info(
        &host,
        RCC_INFO,
        RCC,
        r#"
        "MULTI_CONFIG": true, "CROSS_CONFIG": true,
        "INCLUDE_DIR_Debug": "/build/qt/include_Debug", "SETTINGS_FILE_Debug": "/build/settings_Debug",
        "RCC_EXECUTABLE_Release": "/bin/rcc", "RCC_LIST_OPTIONS_Debug": ["--list"],
        "OUTPUT_NAME": "qrc_form_$<CONFIG>.cpp"
    "#,
    )?;
    seed(&host, "/src/form.qrc", b"<RCC/>")?;
    seed(&host, "/src/assets/data.txt", b"data")?;
    assert_eq!(action(&host, run_rcc, &[RCC_INFO, "Debug", "Release"]), 0);
    assert_eq!(
        calls.lock().unwrap_or_else(PoisonError::into_inner).len(),
        1
    );
    let wrapper = "/build/qt/ABC/qrc_form_Debug.cpp";
    assert!(contents(&host, wrapper)?.ends_with(b"#include <ABC/qrc_form_Debug_CMAKE_.cpp>\n"));
    assert_eq!(
        contents(
            &host,
            "/build/qt/include_Debug/ABC/qrc_form_Debug_CMAKE_.cpp"
        )?,
        b"// Generated by Qt\n"
    );
    assert_eq!(action(&host, run_rcc, &[RCC_INFO, "Debug", "Release"]), 0);
    assert_eq!(
        calls.lock().unwrap_or_else(PoisonError::into_inner).len(),
        2
    );
    let before = host.metadata(&fixture_path(wrapper), Follow::Yes)?.mtime;
    host.sleep(Duration::from_secs(1));
    seed(&host, "/src/assets/data.txt", b"new data")?;
    assert_eq!(action(&host, run_rcc, &[RCC_INFO, "Debug", "Release"]), 0);
    assert_eq!(
        calls.lock().unwrap_or_else(PoisonError::into_inner).len(),
        4
    );
    assert!(host.metadata(&fixture_path(wrapper), Follow::Yes)?.mtime > before);
    Ok(())
}

#[test]
fn reports_missing_resources_and_cleans_failed_tool_outputs() -> TestResult {
    let host = host()?;
    seed_info(&host, RCC_INFO, RCC, "")?;
    seed(
        &host,
        "/src/form.qrc",
        b"<RCC><qresource><file>asset.txt</file></qresource></RCC>",
    )?;
    setup_generators(&host)?;
    seed(&host, "/src/asset.txt", b"")?;
    assert_eq!(action(&host, run_rcc, &[RCC_INFO]), 0);
    host.remove_file(&fixture_path("/src/asset.txt"))?;
    host.set_command_handler(|host, _, _| {
        host.seed_file(&fixture_path("/build/qt/ABC/qrc_form.cpp"), b"partial")?;
        Ok(RunOutput {
            results: vec![ProcessResult::Exited(1)],
            stderr: b"rcc test error".to_vec(),
            ..RunOutput::default()
        })
    })?;

    assert_eq!(action(&host, run_rcc, &[RCC_INFO]), 1);
    assert!(
        String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr))
            .contains(&format!("listed in \"{}\"", fixture_arg("/src/form.qrc")))
    );
    seed(&host, "/src/asset.txt", b"")?;
    seed_info(&host, RCC_INFO, RCC, r#""OPTIONS": ["-name", "changed"]"#)?;
    assert_eq!(action(&host, run_rcc, &[RCC_INFO]), 1);
    let diagnostic = host.stream_bytes(Stream::Stderr);
    for detail in [
        "rcc test error",
        "Command:",
        "Working directory:",
        "Exited(1)",
    ] {
        assert!(String::from_utf8_lossy(&diagnostic).contains(detail));
    }
    assert!(
        host.metadata(&fixture_path("/build/qt/ABC/qrc_form.cpp"), Follow::Yes)
            .is_err()
    );
    assert!(
        host.metadata(&fixture_path("/build/settings"), Follow::Yes)
            .is_err()
    );
    Ok(())
}

#[test]
fn legacy_moc_discovers_the_header_and_its_plugin_metadata() -> TestResult {
    let host = host()?;
    let calls = setup_generators(&host)?;
    seed_info(
        &host,
        INFO,
        AUTOGEN,
        r#"
        "QT_VERSION_MAJOR": 5, "QT_VERSION_MINOR": 14, "MOC_RELAXED_MODE": true,
        "MOC_DEPEND_FILTERS": [["Q_PLUGIN_METADATA", "[\\n][ \\t]*Q_PLUGIN_METADATA[ \\t]*\\([^\\)]*FILE[ \\t]*\"([^\"]+)\""]],
        "HEADERS": [], "SOURCES": [["/src/main.cpp", "Mu", null]],
        "MOC_PREDEFS_CMD": [], "MOC_PREDEFS_FILE": ""
    "#,
    )?;
    seed(&host, "/src/main.cpp", b"#include \"plugin.moc\"\n")?;
    seed(
        &host,
        "/headers/plugin.h",
        b"class Plugin {\n Q_OBJECT\n Q_PLUGIN_METADATA(IID \"org.test\"\n    FILE \"plugin FILE.json\")\n};",
    )?;
    seed(&host, "/headers/plugin FILE.json", b"{}")?;

    assert_eq!(action(&host, run, &[INFO, ""]), 0);
    let requests = calls.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(requests.len(), 1);
    let request = requests.first().ok_or("Expected a moc request.")?;
    let command = first_command(request)?;
    assert!(!command.args.iter().any(|arg| arg == b"--output-dep-file"));
    assert_eq!(command.args.last(), Some(&fixture_arg("/headers/plugin.h")));
    assert!(
        String::from_utf8_lossy(&host.stream_bytes(Stream::Stderr))
            .contains("AUTOMOC_RELAXED_MODE")
    );
    assert!(
        String::from_utf8_lossy(&contents(&host, "/build/qt/deps")?)
            .contains("/headers/plugin\\ FILE.json")
    );
    Ok(())
}

#[test]
fn requeries_an_updated_autorcc_manifest_and_refreshes_on_info_timestamp() -> TestResult {
    let host = host()?;
    let calls = setup_generators(&host)?;
    seed_info(&host, RCC_INFO, RCC, "")?;
    seed(&host, "/src/form.qrc", b"<RCC><file>old.txt</file></RCC>")?;
    seed(&host, "/src/old.txt", b"old")?;
    assert_eq!(action(&host, run_rcc, &[RCC_INFO]), 0);
    let output = &fixture_path("/build/qt/ABC/qrc_form.cpp");
    let before = host.metadata(output, Follow::Yes)?.mtime;

    host.sleep(Duration::from_secs(1));
    seed_info(&host, RCC_INFO, RCC, r#""VERBOSITY":1"#)?;
    assert_eq!(action(&host, run_rcc, &[RCC_INFO]), 0);
    assert_eq!(
        calls.lock().unwrap_or_else(PoisonError::into_inner).len(),
        1
    );
    assert!(host.metadata(output, Follow::Yes)?.mtime > before);

    host.sleep(Duration::from_secs(1));
    seed(&host, "/src/form.qrc", b"<RCC><file>new.txt</file></RCC>")?;
    assert_eq!(action(&host, run_rcc, &[RCC_INFO]), 0);
    assert_eq!(action(&host, run_rcc, &[RCC_INFO]), 1);
    assert_eq!(
        calls.lock().unwrap_or_else(PoisonError::into_inner).len(),
        2
    );
    seed(&host, "/src/new.txt", b"new")?;
    assert_eq!(action(&host, run_rcc, &[RCC_INFO]), 0);
    assert_eq!(
        calls.lock().unwrap_or_else(PoisonError::into_inner).len(),
        3
    );
    Ok(())
}
