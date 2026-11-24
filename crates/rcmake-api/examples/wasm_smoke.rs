//! Check portable configure, provenance, and process policy on an in-memory host.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::io::{self, Write as _};
use std::path::Path;
use std::sync::Arc;

use rcmake_api::{
    ApiError, CacheDefine, CancelToken, CompletionState, ConfigureRequest, ExplainQuery,
    ExplainReport, LedgerLevel, ProcessPolicy, Services, WriteMode, run_configure,
};
use rcmake_base::CMAKE_VERSION_STR;
use rcmake_host::{Follow, Host, PureHost, path};
use rcmake_json::{Value, write_compact};
use rcmake_model::{CacheType, DirId, GeneratorKind, UsageScope};

const TARGETS: [&str; 3] = ["core", "util", "app"];

const PROJECT: &[u8] = br#"
cmake_minimum_required(VERSION 4.5)
project(PortableSmoke LANGUAGES CXX)
set(CMAKE_CXX_SCAN_FOR_MODULES OFF)
add_library(core STATIC core.cpp)
target_compile_definitions(core INTERFACE CORE_API=1)
add_library(util INTERFACE)
add_executable(app app.cpp)
target_link_libraries(app PRIVATE core util)
file(WRITE "${CMAKE_BINARY_DIR}/overlay.txt" "only in the overlay")
"#;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let result = verify();
    let report = match &result {
        Ok(denials) => object([
            ("schema", Value::string("rcmake-wasm-smoke-v1")),
            ("ok", Value::Bool(true)),
            (
                "targets",
                Value::Array(TARGETS.into_iter().map(Value::string).collect()),
            ),
            ("provenance", Value::string("-DCORE_API=1")),
            ("complete", Value::Bool(true)),
            ("provisional", Value::Bool(true)),
            ("denied_processes", Value::UInt(*denials)),
            (
                "try_compile",
                Value::string("skipped: child configure uses a scoped thread"),
            ),
            ("cancelled", Value::Bool(true)),
        ]),
        Err(error) => object([
            ("schema", Value::string("rcmake-wasm-smoke-v1")),
            ("ok", Value::Bool(false)),
            ("error", Value::string(error.to_string())),
        ]),
    };
    let mut stdout = io::stdout().lock();
    stdout.write_all(&write_compact(&report))?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    result.map(|_| ())
}

fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect::<BTreeMap<_, _>>(),
    )
}

fn verify() -> Result<u64, Box<dyn std::error::Error>> {
    let root = Path::new(if cfg!(windows) { "C:/" } else { "/" });
    let host = Arc::new(PureHost::new(root.to_owned(), root.join("bin/rcmake")));
    host.seed_file(&root.join("src/CMakeLists.txt"), PROJECT)?;
    host.seed_file(&root.join("src/core.cpp"), b"int core() { return 0; }\n")?;
    host.seed_file(&root.join("src/app.cpp"), b"int main() { return 0; }\n")?;
    let request = ConfigureRequest::new("src", "build", "cmake", host.clone())
        .generator(GeneratorKind::UnixMakefiles)
        .define(cache_entry("CMAKE_PLATFORM_INFO_INITIALIZED", "1"))
        .define(cache_entry(
            "CMAKE_MAKE_PROGRAM",
            path::from_native_cmake(&root.join("bin/make")),
        ))
        .write_mode(WriteMode::Overlay)
        .process_policy(ProcessPolicy::Deny)
        .collect_index(true)
        .services(Services {
            provenance: LedgerLevel::Full { per_name_cap: 32 },
            ..Services::default()
        });
    seed_toolchain(&host, root)?;

    let result = run_configure(request.clone())?;
    require(
        result.model.targets().count() == TARGETS.len(),
        "Configure did not create three targets.",
    )?;
    require(
        result.completion == CompletionState::Complete && result.generation.is_some(),
        "Configure did not complete resolution.",
    )?;
    require(
        result.index.as_ref().is_some_and(|index| {
            !index.is_provisional()
                && TARGETS
                    .iter()
                    .all(|name| index.lookup_target(DirId::ROOT, name.as_bytes()).is_some())
        }),
        "The workspace index is missing a target.",
    )?;
    let ExplainReport::Target(app) = result.explain_filtered(
        &ExplainQuery::Target("app".into()),
        Some("-DCORE_API=1".into()),
    ) else {
        return Err("Explain could not find app.".into());
    };
    require(
        app.configurations
            .iter()
            .flat_map(|config| &config.entries)
            .any(|entry| {
                entry.value == b"CORE_API=1"
                    && entry
                        .spelling
                        .as_ref()
                        .is_some_and(|flag| flag == b"-DCORE_API=1")
                    && entry.chain.as_ref().is_some_and(|chain| {
                        matches!(chain.as_slice(), [hop]
                        if hop.via == UsageScope::Private
                            && hop.target.as_ref().is_some_and(|target| target == b"core"))
                    })
            }),
        "Explain did not trace -DCORE_API=1 from app to core.",
    )?;

    let marker = root.join("build/overlay.txt");
    require(
        result
            .overlay_diff
            .as_ref()
            .and_then(|diff| diff.read_file(&marker))
            == Some(b"only in the overlay".as_slice()),
        "The overlay is missing a configure write.",
    )?;
    require(
        matches!(host.metadata(&marker, Follow::No), Err(error) if error.kind == io::ErrorKind::NotFound),
        "The configure write reached the base host.",
    )?;

    let mut script = PROJECT.to_vec();
    script.extend_from_slice(
        b"execute_process(COMMAND forbidden-tool RESULT_VARIABLE process_result)\n",
    );
    host.seed_file(&root.join("src/CMakeLists.txt"), &script)?;
    let denied = run_configure(request.clone())?;
    let CompletionState::Partial {
        denied_processes,
        omitted_denials,
        cancelled: false,
        ..
    } = denied.completion
    else {
        return Err("Configure did not return a partial result after the denial.".into());
    };
    require(
        denied.model.targets().count() == TARGETS.len()
            && denied
                .index
                .as_ref()
                .is_some_and(|index| index.is_provisional())
            && denied_processes
                .first()
                .and_then(|denial| denial.pipeline.first())
                .is_some_and(|command| command.program == b"forbidden-tool"),
        "The partial result is missing configured targets or the process denial.",
    )?;

    let cancel = CancelToken::default();
    cancel.cancel();
    require(
        matches!(
            run_configure(request.cancel(cancel)),
            Err(ApiError::Cancelled)
        ),
        "The cancelled request did not stop at preparation.",
    )?;
    Ok(u64::try_from(
        denied_processes.len().saturating_add(omitted_denials),
    )?)
}

fn cache_entry(name: &str, value: impl Into<rcmake_base::BString>) -> CacheDefine {
    CacheDefine {
        name: name.into(),
        ty: Some(CacheType::Internal),
        value: value.into(),
    }
}

// Seed the files a configured build tree normally supplies for its C++ toolchain.
fn seed_toolchain(host: &PureHost, root: &Path) -> rcmake_host::HostResult<()> {
    let modules = root.join("cmake/Modules");
    for (name, contents) in [
        (
            "CMakeDetermineSystem.cmake",
            "# Use the saved system settings.\n",
        ),
        (
            "CMakeSystemSpecificInitialize.cmake",
            "set(CMAKE_SYSTEM_SPECIFIC_INITIALIZE_LOADED 1)\n",
        ),
        (
            "CMakeSystemSpecificInformation.cmake",
            "set(CMAKE_SYSTEM_SPECIFIC_INFORMATION_LOADED 1)\n",
        ),
        (
            "CMakeCXXInformation.cmake",
            "set(CMAKE_CXX_INFORMATION_LOADED 1)\n",
        ),
    ] {
        host.seed_file(&modules.join(name), contents.as_bytes())?;
    }
    let platform = root.join("build/CMakeFiles").join(CMAKE_VERSION_STR);
    host.seed_file(
        &platform.join("CMakeSystem.cmake"),
        b"set(CMAKE_SYSTEM Generic)\nset(CMAKE_SYSTEM_NAME Generic)\nset(CMAKE_SYSTEM_LOADED 1)\n",
    )?;
    let compiler = root.join("bin/c++");
    host.seed_file(&compiler, b"")?;
    host.seed_file(&root.join("bin/make"), b"")?;
    let compiler = path::from_native_cmake(&compiler);
    let config = format!(
        "set(CMAKE_CXX_COMPILER \"{compiler}\")\n         set(CMAKE_CXX_COMPILER_LOADED 1)\n         set(CMAKE_CXX_COMPILER_WORKS 1)\n         set(CMAKE_CXX_SOURCE_FILE_EXTENSIONS cpp)\n         set(CMAKE_CXX_OUTPUT_EXTENSION .o)\n         set(CMAKE_CXX_LINKER_PREFERENCE 30)\n"
    );
    host.seed_file(&platform.join("CMakeCXXCompiler.cmake"), config.as_bytes())
}

fn require(condition: bool, message: &'static str) -> Result<(), Box<dyn std::error::Error>> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}
