# rcmake-api

`rcmake-api` runs the `rcmake` configure engine inside your Rust program. You give it a project and a `Host`, and it returns the evaluated project model, the resolved targets, and diagnostics. You can then ask where a target got a flag or where a variable got its value. The `rcmake explain` command, the `rcmake-lsp` language server, and the browser playground all use this crate.

## Add the dependency

The crates are not on crates.io. Use Git dependencies:

```toml
[dependencies]
rcmake-api = { git = "https://github.com/dvgyl/rcmake" }
rcmake-base = { git = "https://github.com/dvgyl/rcmake" }
rcmake-host = { git = "https://github.com/dvgyl/rcmake" }
rcmake-model = { git = "https://github.com/dvgyl/rcmake" }
```

For WebAssembly targets, turn off the default features:

```toml
rcmake-api = { git = "https://github.com/dvgyl/rcmake", default-features = false }
```

| Feature | Default | Effect |
| --- | --- | --- |
| `native` | on | The persistent probe cache (`rcmake-store`) and the FetchContent cache and lockfiles (`rcmake-fetch`). |
| `archive` | on | `file(ARCHIVE_CREATE)` and `file(ARCHIVE_EXTRACT)` through `rcmake-archive` and its native codec crates. |

Without both features, the crate compiles for `wasm32-unknown-unknown` and `wasm32-wasip1`.

## Configure a project in memory and explain a target

This example uses `PureHost`, an in-memory file system. It needs no compiler and no disk access. The `seed_toolchain` function writes the files that a configured build tree normally has, so that `project()` does not run compiler checks. [`examples/wasm_smoke.rs`](examples/wasm_smoke.rs) is a complete, tested version of this flow.

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

For JSON output, use `render_explain_json(&report, None)` in place of `render_explain_terminal`. The JSON schema name is in `EXPLAIN_SCHEMA`.

## Explain a project on disk

To read a real project, use `NativeHost` and the installed CMake modules. This setup is the same as the one that `rcmake explain` uses:

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

`WriteMode::Overlay` keeps every configure write in memory, so the build directory does not change. `result.overlay_diff` lists the writes. `ProcessPolicy::Deny` blocks subprocesses such as compiler checks. When configure can continue, `result.completion` is `CompletionState::Partial` and lists the denied requests. When it cannot continue, `run_configure` returns `ApiError::DeniedProcess`. For a build directory that is already configured, pass its saved cache with `initial_cache`, as `rcmake explain` does, so that configure reuses the saved compiler results. To run compiler checks, use `ProcessPolicy::Allow` with `WriteMode::Direct`.

## Main types

| Item | Use |
| --- | --- |
| `ConfigureRequest` | Source, build, and CMake data directories, the `Host`, and the builder options: `generator`, `define`, `initial_cache`, `process_policy`, `write_mode`, `services`, `collect_index`, `cancel`, `step_budget`, and `limits`. |
| `run_configure` | Runs configure and generation. Returns `ConfigureResult` or `ApiError`. |
| `ConfigureResult` | `model`, `generation`, `diagnostics`, `completion`, `index`, `overlay_diff`, and the `explain`, `explain_filtered`, and `explain_in` queries. |
| `ExplainQuery` | `Target(name)`, `Variable(name)`, or `Auto(name)`, which finds a target or a variable. |
| `ExplainReport` | `Target`, `Variable`, `Ambiguous`, `NotFound`, or `InvalidDirectory`. |
| `render_explain_terminal`, `render_explain_json` | Turn an `ExplainReport` into terminal text or JSON. |
| `WorkspaceIndex` | Definitions, references, and completions for editors. Set `collect_index(true)` to build it. |
| `CancelToken` | Stops a running configure from another thread. `run_configure` then returns `ApiError::Cancelled`. |

Run `cargo doc -p rcmake-api --open` for the full API reference.
