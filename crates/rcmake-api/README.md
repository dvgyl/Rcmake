# rcmake-api

`rcmake-api` runs the `rcmake` configure engine inside your Rust program. You pass it a project and a `Host`, and it returns the evaluated project model, resolved targets, and diagnostics. From that result, you can ask where a target got a flag or where a variable got its value. The `rcmake explain` command, the `rcmake-lsp` language server, and the browser playground all use this crate.

## Add the dependency

These crates are not published on crates.io, so add them as Git dependencies in `Cargo.toml`.

```toml
[dependencies]
rcmake-api = { git = "https://github.com/rcmake/rcmake" }
rcmake-base = { git = "https://github.com/rcmake/rcmake" }
rcmake-host = { git = "https://github.com/rcmake/rcmake" }
rcmake-model = { git = "https://github.com/rcmake/rcmake" }
```

For WebAssembly targets, turn off the default features.

```toml
rcmake-api = { git = "https://github.com/rcmake/rcmake", default-features = false }
```

| Feature | Default | Effect |
| --- | --- | --- |
| `native` | on | Enables the persistent probe cache in `rcmake-store` and the FetchContent cache and lockfiles in `rcmake-fetch`. |
| `archive` | on | Enables `file(ARCHIVE_CREATE)` and `file(ARCHIVE_EXTRACT)` through `rcmake-archive` and its native codec crates. |

With both features off, the crate compiles for `wasm32-unknown-unknown` and `wasm32-wasip1`.

## Configure a project in memory and explain a target

This example runs on `PureHost`, an in-memory file system that needs no compiler and no disk access. The `seed_toolchain` helper writes the files that a configured build tree normally holds, so `project()` skips compiler checks. See [`examples/wasm_smoke.rs`](examples/wasm_smoke.rs) for a complete, tested version of this flow.

```rust
use std::{path::Path, sync::Arc};

use rcmake_api::{
	CacheDefine, ConfigureRequest, ExplainQuery, ExplainRenderOptions, LedgerLevel, ProcessPolicy,
	Services, WriteMode, render_explain_terminal, run_configure,
};
use rcmake_base::CMAKE_VERSION_STR;
use rcmake_host::{PureHost, path};
use rcmake_model::{CacheType, GeneratorKind};

const PROJECT: &[u8] = br#"
cmake_minimum_required(VERSION 4.5)
project(Demo LANGUAGES CXX)
set(CMAKE_CXX_SCAN_FOR_MODULES OFF)
add_library(core STATIC core.cpp)
target_compile_definitions(core INTERFACE CORE_API=1)
add_executable(app app.cpp)
target_link_libraries(app PRIVATE core)
"#;

fn main() -> Result<(), Box<dyn std::error::Error>> {
	let root = Path::new(if cfg!(windows) { "C:/" } else { "/" });
	let host = Arc::new(PureHost::new(root.to_owned(), root.join("bin/rcmake")));
	host.seed_file(&root.join("src/CMakeLists.txt"), PROJECT)?;
	host.seed_file(&root.join("src/core.cpp"), b"int core() { return 0; }\n")?;
	host.seed_file(&root.join("src/app.cpp"), b"int main() { return 0; }\n")?;
	seed_toolchain(&host, root)?;

	// Paths are relative to the host's working directory. "cmake" holds Modules/.
	let request = ConfigureRequest::new("src", "build", "cmake", host.clone())
		.generator(GeneratorKind::UnixMakefiles)
		.define(internal("CMAKE_PLATFORM_INFO_INITIALIZED", "1"))
		.define(internal("CMAKE_MAKE_PROGRAM", path::from_native_cmake(&root.join("bin/make"))))
		.write_mode(WriteMode::Overlay)
		.process_policy(ProcessPolicy::Deny)
		.services(Services {
			provenance: LedgerLevel::Full { per_name_cap: 32 },
			..Services::default()
		});
	let result = run_configure(request)?;
	println!("{} targets, completion: {:?}", result.model.targets().count(), result.completion);

	// Why does app compile with -DCORE_API=1?
	let report = result.explain_filtered(&ExplainQuery::Target("app".into()), Some("-DCORE_API=1".into()));
	print!("{}", render_explain_terminal(&report, ExplainRenderOptions::default()));
	Ok(())
}

fn internal(name: &str, value: impl Into<rcmake_base::BString>) -> CacheDefine {
	CacheDefine { name: name.into(), ty: Some(CacheType::Internal), value: value.into() }
}

fn seed_toolchain(host: &PureHost, root: &Path) -> rcmake_host::HostResult<()> {
	let modules = root.join("cmake/Modules");
	for (name, contents) in [
		("CMakeDetermineSystem.cmake", "# Use the saved system settings.\n"),
		("CMakeSystemSpecificInitialize.cmake", "set(CMAKE_SYSTEM_SPECIFIC_INITIALIZE_LOADED 1)\n"),
		("CMakeSystemSpecificInformation.cmake", "set(CMAKE_SYSTEM_SPECIFIC_INFORMATION_LOADED 1)\n"),
		("CMakeCXXInformation.cmake", "set(CMAKE_CXX_INFORMATION_LOADED 1)\n"),
	] {
		host.seed_file(&modules.join(name), contents.as_bytes())?;
	}
	let platform = root.join("build/CMakeFiles").join(CMAKE_VERSION_STR);
	host.seed_file(
		&platform.join("CMakeSystem.cmake"),
		b"set(CMAKE_SYSTEM Generic)\nset(CMAKE_SYSTEM_NAME Generic)\nset(CMAKE_SYSTEM_LOADED 1)\n",
	)?;
	host.seed_file(&root.join("bin/c++"), b"")?;
	host.seed_file(&root.join("bin/make"), b"")?;
	let compiler = path::from_native_cmake(&root.join("bin/c++"));
	let config = format!(
		"set(CMAKE_CXX_COMPILER \"{compiler}\")\n\
		 set(CMAKE_CXX_COMPILER_LOADED 1)\n\
		 set(CMAKE_CXX_COMPILER_WORKS 1)\n\
		 set(CMAKE_CXX_SOURCE_FILE_EXTENSIONS cpp)\n\
		 set(CMAKE_CXX_OUTPUT_EXTENSION .o)\n\
		 set(CMAKE_CXX_LINKER_PREFERENCE 30)\n"
	);
	host.seed_file(&platform.join("CMakeCXXCompiler.cmake"), config.as_bytes())
}
```

For JSON output, call `render_explain_json(&report, None)` instead of `render_explain_terminal`. The constant `EXPLAIN_SCHEMA` holds the JSON schema name.

## Explain a project on disk

To inspect a real project on disk, use `NativeHost` together with the installed CMake modules. The `rcmake explain` command uses the same setup.

```rust,no_run
use std::sync::Arc;

use rcmake_api::{ConfigureRequest, ExplainQuery, LedgerLevel, ProcessPolicy, Services, WriteMode, run_configure};
use rcmake_host::{Host, NativeHost, discover_cmake_root};

fn main() -> Result<(), Box<dyn std::error::Error>> {
	let host: Arc<dyn Host> = Arc::new(NativeHost::new()?);
	// Uses RCMAKE_VENDORED_DATA_ROOT, or share/cmake-4.5 next to the executable.
	let (_exe, cmake_root) = discover_cmake_root(host.as_ref(), None);
	let cmake_root = cmake_root.ok_or("Set RCMAKE_VENDORED_DATA_ROOT to the share/cmake-4.5 directory.")?;
	let source = std::env::current_dir()?;
	let request = ConfigureRequest::new(source.clone(), source.join("build"), cmake_root, host)
		.write_mode(WriteMode::Overlay)
		.process_policy(ProcessPolicy::Deny)
		.services(Services { provenance: LedgerLevel::Full { per_name_cap: 64 }, ..Services::default() });
	let result = run_configure(request)?;
	let report = result.explain(&ExplainQuery::Auto("app".into()));
	println!("{report:?}");
	Ok(())
}
```

`WriteMode::Overlay` keeps every configure write in memory, so the build directory on disk stays unchanged, and `result.overlay_diff` records those writes. `ProcessPolicy::Deny` blocks subprocesses such as compiler checks. If configure can finish without a blocked process, `result.completion` is `CompletionState::Partial` and lists the denied requests. If configure cannot continue, `run_configure` returns `ApiError::DeniedProcess`.

When you inspect a build directory that is already configured, pass its saved cache through `initial_cache`, as `rcmake explain` does, so configure reuses the saved compiler results. If you want configure to run compiler checks instead, use `ProcessPolicy::Allow` with `WriteMode::Direct`.

## Main types

| Item | Use |
| --- | --- |
| `ConfigureRequest` | Holds the source, build, and CMake data directories along with the `Host`. Builder methods set `generator`, `define`, `initial_cache`, `process_policy`, `write_mode`, `services`, `collect_index`, `cancel`, `step_budget`, and `limits`. |
| `run_configure` | Runs configure and generation and returns `ConfigureResult` or `ApiError`. |
| `ConfigureResult` | Holds `model`, `generation`, `diagnostics`, `completion`, `index`, and `overlay_diff`, plus the `explain`, `explain_filtered`, and `explain_in` query methods. |
| `ExplainQuery` | Selects `Target(name)`, `Variable(name)`, or `Auto(name)`, where `Auto` matches either a target or a variable. |
| `ExplainReport` | Reports `Target`, `Variable`, `Ambiguous`, `NotFound`, or `InvalidDirectory`. |
| `render_explain_terminal`, `render_explain_json` | Format an `ExplainReport` as terminal text or JSON. |
| `WorkspaceIndex` | Stores definitions, references, and completions for editors when you set `collect_index(true)`. |
| `CancelToken` | Stops a running configure from another thread so `run_configure` returns `ApiError::Cancelled`. |

Run `cargo doc -p rcmake-api --open` to read the full API reference.
