# Architecture

`rcmake` splits into 35 acyclic crates that match the four-stage execution model of CMake 4.5.1.

## Execution pipeline

```text
1. Command line  rcmake-cli, rcmake-presets
                      │
                      ▼
2. Configure     rcmake-syntax ─► rcmake-expr ─► rcmake-eval + rcmake-commands
                      │
                      ▼
                 rcmake-model (ProjectModel)
                      │
                      ▼
3. Generate      rcmake-resolve + rcmake-genex (ResolvedTarget, BuildGraph)
                      │
                      ├─► rcmake-gen-ninja, rcmake-gen-make, rcmake-gen-xcode, rcmake-gen-vs
                      └─► rcmake-gen-common (File API, install scripts, CTest files, CPS, SBOM)
                      │
                      ▼
4. Use           cmake --build, cmake --install, rcmake-ctest, rcmake-cpack
```

1. **Command line (`rcmake-cli`, `rcmake-presets`).** The binary selects `cmake`, `ctest`, or `cpack` from the name that starts it (`argv[0]`). It parses `-P`, `-E`, `--build`, `--install`, `--workflow`, `--preset`, and the other CMake options. It loads `CMakePresets.json` and `CMakeUserPresets.json` (schema version 13) and creates the `Host`.
2. **Configure (`rcmake-syntax`, `rcmake-regex`, `rcmake-expr`, `rcmake-eval`, `rcmake-commands`).** The parser reads `CMakeLists.txt` and `.cmake` files. The evaluator runs the commands, applies policies and diagnostics, and updates `CMakeCache.txt`. The output is a `ProjectModel` in `rcmake-model` containing directories, targets, tests, install rules, and custom commands.
3. **Generate (`rcmake-resolve`, `rcmake-genex`, `rcmake-gen-*`).** The resolver evaluates generator expressions (`$<...>`) for each configuration, computes transitive usage requirements, link closures, and C++ and Fortran module dependencies, and lowers each target into a `BuildGraph`. The generators write Ninja, Makefile, Xcode, or Visual Studio build files. `rcmake-gen-common` writes the generator-independent outputs, including File API replies, `compile_commands.json`, install scripts, CTest files, exports, CPS files, and SBOM files.
4. **Build, test, and package (`rcmake-cli`, `rcmake-ctest`, `rcmake-cpack`).** `cmake --build` runs the native build tool, `cmake --install` runs the install scripts, `ctest` runs the tests, and `cpack` creates packages.

The `rcmake-api` crate runs stages 2 and 3 in process for other programs. The `rcmake-lsp` language server calls `rcmake-api` to evaluate open files, and `rcmake-wasm` runs `rcmake-api` on `PureHost` to power the browser playground.

## Resolution layers

| Layer | Owner | Contents |
| --- | --- | --- |
| `ProjectModel` | `rcmake-model` | Raw configure state, including cache entries, properties, directories, targets, tests, and install rules. Values keep their backtraces. |
| `ResolvedTarget` | `rcmake-resolve` | Typed target facts for one configuration, with all generator expressions evaluated. The File API and `install(EXPORT)` read this layer. |
| `BuildGraph` | `rcmake-resolve` | Compile, link, and custom command steps with escaped command lines, plus the install manifest. The generators and `rcmake-build` read this layer. |

Generator expression evaluation and usage resolution call each other. For example, `$<TARGET_PROPERTY:...>` needs the transitive properties of a target. To keep the crate graph acyclic, `rcmake-genex` defines the `GenexHost` trait and `rcmake-resolve` implements it.

## Workspace crate map

The "Depends on" column lists only workspace crates. Every crate except `rcmake-harness` depends on `rcmake-base`.

### Foundation

| Crate | Responsibility | Depends on |
| --- | --- | --- |
| `rcmake-base` | Byte strings, CMake lists, truthiness (`is_on`, `is_off`), version comparison, lexical paths, hashes, and input limits | none |
| `rcmake-json` | JSON parser and writers compatible with jsoncpp for `string(JSON)`, presets, the File API, and SARIF | `base` |
| `rcmake-diag` | Source spans, backtraces, CMake 4.5.1 diagnostic text, the policy registry `CMP0000` to `CMP0226`, and SARIF 2.1.0 | `base`, `json` |
| `rcmake-host` | The `Host` trait for file system, environment, process, and clock access, along with `NativeHost`, `PureHost`, and a shared conformance suite | `base` |

### Language

| Crate | Responsibility | Depends on |
| --- | --- | --- |
| `rcmake-syntax` | Lexer and parser for CMake listfiles that keeps raw bytes, argument kinds, and spans and checks block nesting before execution | `base`, `diag` |
| `rcmake-regex` | Independent byte regular expression engine implementing the CMake (KWSys) dialect, `CMAKE_MATCH_<n>`, and a glob converter | `base` |
| `rcmake-expr` | Variable expansion (`${}`, `$ENV{}`, `$CACHE{}`), escape decoding, the `if()` and `while()` condition evaluator, and the `configure_file` expander | `base`, `diag`, `host`, `regex`, `syntax` |
| `rcmake-genex` | Generator expression parser, evaluator, `GenexHost` trait, and preprocess helpers such as `string(GENEX_STRIP)` | `base`, `diag`, `regex` |

### Configure

| Crate | Responsibility | Depends on |
| --- | --- | --- |
| `rcmake-model` | Cache store, property maps, global state, and the arena-based `ProjectModel`. The `vs-cross` and `xcode-cross` features enable the IDE generators on all hosts for tests. | `base`, `diag` |
| `rcmake-eval` | Script interpreter managing variable, policy, and diagnostic scopes, `function`, `macro`, `block`, control flow, `cmake_language()`, the command registry, the configure journal, and the hermetic input policy | `base`, `diag`, `expr`, `host`, `model`, `regex`, `syntax` |
| `rcmake-commands` | Built-in CMake commands grouped by family (`string_math`, `vars_list`, `file_path`, `process_find`, `targets`, `install`, `custom`, `configure`, `policy_misc`, `compat`, and others), plus `project()`, `enable_language()`, `try_compile()`, the probe cache, configure replay, and FetchContent integration | `archive`, `base`, `build`, `cps`, `diag`, `eval`, `expr`, `fetch`, `genex`, `host`, `instrument`, `json`, `model`, `regex`, `resolve`, `store`, `syntax` |
| `rcmake-presets` | Presets schema version 13 loader, `include` resolution, inheritance, conditions, and macro expansion | `base`, `diag`, `host`, `json`, `regex` |

### Resolution and build

| Crate | Responsibility | Depends on |
| --- | --- | --- |
| `rcmake-resolve` | Per-configuration target resolution, transitive usage requirements, link lines, IDE facts, and lowering to `BuildGraph`, including CUDA, Fortran, Qt `AUTOGEN`, precompiled header, unity build, and install runtime path rules | `base`, `cxxmodules`, `diag`, `fortran`, `genex`, `host`, `json`, `model` |
| `rcmake-build` | In-process executor for a `BuildGraph` that runs `try_compile()` and the `cmake --build` fallback without a generator. Probes skip Ninja, Make, MSBuild, and xcodebuild, so MSBuild-only and Xcode-only settings have no effect on them. | `base`, `cxxmodules`, `diag`, `host`, `resolve` |
| `rcmake-cxxmodules` | C++20 named modules support, including P1689r5 dependency files, `CXXDependInfo.json`, `CXXModules.json`, Ninja `dyndep` files, and compiler module maps | `base`, `json` |
| `rcmake-fortran` | Fortran source scanning and module dependency collation with no I/O | `base` |

### Generators

| Crate | Responsibility | Depends on |
| --- | --- | --- |
| `rcmake-gen-common` | Outputs shared by all generators, including `compile_commands.json`, `cmake_install.cmake`, `CTestTestfile.cmake`, `CMakeConfigureLog.yaml`, exports, File API replies, and link response files | `base`, `cps`, `diag`, `genex`, `host`, `json`, `model`, `resolve` |
| `rcmake-gen-ninja` | `Ninja` and `Ninja Multi-Config` generators, including response files, dependency files, and module `dyndep` rules | `base`, `cxxmodules`, `diag`, `gen-common`, `genex`, `host`, `model`, `resolve` |
| `rcmake-gen-make` | `Unix Makefiles` generator with dependency scanning and grouped parallel output | `base`, `diag`, `gen-common`, `genex`, `host`, `model`, `resolve` |
| `rcmake-gen-xcode` | `Xcode` generator for `project.pbxproj`, schemes, workspace settings, and the `xcodebuild` command plan (ADR-014) | `base`, `diag`, `gen-common`, `host`, `model`, `resolve` |
| `rcmake-gen-vs` | `Visual Studio 17 2022` (`.sln`) and `Visual Studio 18 2026` (`.slnx`) generators for `.vcxproj`, `.vcxproj.filters`, stable GUIDs, and the MSBuild command plan (ADR-015) | `base`, `diag`, `gen-common`, `host`, `model`, `resolve` |

### Test and package

| Crate | Responsibility | Depends on |
| --- | --- | --- |
| `rcmake-ctest` | `ctest` driver for `CTestTestfile.cmake` evaluation, parallel scheduling, fixtures, resource specifications, `discover_tests()`, JUnit and dashboard XML, and `-S` scripts | `base`, `commands`, `diag`, `eval`, `expr`, `host`, `instrument`, `json`, `presets`, `regex` |
| `rcmake-cpack` | `cpack` driver for `CPackConfig.cmake` evaluation, install staging, archive generators (`TGZ`, `TXZ`, `TZST`, `TBZ2`, `TZ`, `TAR`, `STGZ`, `ZIP`, `7Z`), Linux `DEB` and `RPM`, `External`, and macOS `Bundle` and `DragNDrop` generators (ADR-010) | `archive`, `base`, `commands`, `diag`, `eval`, `host`, `json`, `presets` |
| `rcmake-cps` | Common Package Specification 0.15.0 data model, writer, and validating reader (ADR-013) | `base`, `diag`, `json` |
| `rcmake-instrument` | Instrumentation API v1 (data v1.2) queries, snippets, index files, hooks, and trace output (ADR-013) | `base`, `host`, `json` |

### Utilities and storage

| Crate | Responsibility | Depends on |
| --- | --- | --- |
| `rcmake-tool-e` | Built-in `cmake -E` utilities, including `tar`, hash commands, `cmake_ninja_dyndep`, and the Fortran and Qt tool drivers | `archive`, `base`, `cxxmodules`, `diag`, `fortran`, `host`, `json` |
| `rcmake-archive` | Archive engine for `tar`, gzip, xz, zstd, bzip2, zip, and 7-Zip, with path checks on extraction | `base`, `host` |
| `rcmake-store` | Content-addressed blob store, verified cache entries, input stamps, garbage collection, and remote recovery for the probe cache | `base`, `host` |
| `rcmake-fetch` | FetchContent archive cache, Git mirrors, and project lockfiles (ADR-021) | `base`, `host`, `store` |

### Front ends

| Crate | Responsibility | Depends on |
| --- | --- | --- |
| `rcmake-cli` | The `rcmake` binary. Dispatches `cmake`, `ctest`, `cpack`, and `rcmake-lsp` (`rcmake lsp`) by name. Owns `--build`, `--install`, `--workflow`, `--find-package`, SARIF output, profiles, the `explain` subcommand, and the panic hook. | `api`, `base`, `commands`, `cpack`, `ctest`, `diag`, `eval`, `gen-common`, `gen-make`, `gen-ninja`, `gen-vs`, `gen-xcode`, `genex`, `host`, `instrument`, `json`, `model`, `presets`, `resolve`, `syntax`, `tool-e` |
| `rcmake-api` | In-process configure (`run_configure`), workspace index, provenance, and `explain` queries. Builds for WebAssembly with `--no-default-features` (ADR-023, ADR-024). | `base`, `commands`, `diag`, `eval`, `genex`, `host`, `json`, `model`, `resolve`, `syntax` |
| `rcmake-lsp` | Language server for CMake files. Gives lexical results by default, adds evaluated results through `rcmake-api` under the `eval` feature, and builds the `rcmake-lsp` binary under the `stdio` feature. | `api` (optional), `base`, `diag`, `genex` (optional), `host`, `json`, `model` (optional), `resolve` (optional), `syntax` |
| `rcmake-wasm` | Browser playground runtime. Reads a JSON request on standard input, configures the project on `PureHost`, and returns generated Ninja files, diagnostics, and `explain` results as JSON. `scripts/build-wasm.sh` compiles it into `site/wasm/` with no `unsafe` code and no `wasm-bindgen`. | `api`, `base`, `diag`, `gen-common`, `gen-ninja`, `host`, `model`, `presets`, `resolve` |

### Verification

| Crate | Responsibility | Depends on |
| --- | --- | --- |
| `rcmake-harness` | Black-box differential harness that runs the staged `rcmake` and the CMake 4.5.1 oracle side by side and compares results. Links no engine crate. | none |
| `rcmake-fuzz` | Safe-Rust fuzz runner covering 14 input parsers. `rcmake` never links this crate (ADR-016). | `archive`, `base`, `cps`, `diag`, `expr`, `fortran`, `genex`, `host`, `json`, `presets`, `regex`, `store`, `syntax` |

`rcmake-gen-xcode` is a normal dependency of `rcmake-cli` only on macOS, and `rcmake-gen-vs` is a normal dependency only on Windows. On other hosts, the `xcode-cross` and `vs-cross` features of `rcmake-cli` include them for tests.

## The `Host` abstraction

Engine crates never call `std::fs`, `std::env`, `std::process`, or the system clock directly. They go through the `rcmake_host::Host` trait, and `clippy.toml` bans the direct calls everywhere except `rcmake-host`, the `rcmake-cli` entry point, and `rcmake-harness`.

| Implementation | Use |
| --- | --- |
| `NativeHost` | Real operating system backend. Uses `rustix` on Unix and job objects on Windows for process groups and timeouts. |
| `PureHost` | In-memory file system with symlinks, virtual timestamps, scripted subprocesses, and an ordered output log. Unit tests and the WebAssembly build use this backend. |

- `Host` is `Send + Sync`, so multiple threads can share one `Arc<dyn Host>`.
- Every path passed to `Host` is absolute, so the engine never relies on or mutates the process working directory.
- `NativeHost` snapshots the process environment once and stores mutations in an overlay passed to each child process. The engine never calls `std::env::set_var`.
- Error messages keep the raw operating system text so diagnostics match CMake 4.5.1.
- `rcmake_host::conformance::run_all` runs the same behavior suite against both `NativeHost` and `PureHost`.

Wrappers around `Host` add behavior without modifying the engine. Examples include the capture host for `try_compile()` output, the hermetic host that audits inputs, and the overlay host that the language server uses for unsaved files.

## CMake 4.5.1 behavior that the design must keep

1. **Policy baseline.** Calling `cmake_minimum_required(VERSION <x>)` with `<x>` below 3.5 raises an error unless `CMAKE_POLICY_VERSION_MINIMUM` is 3.5 or later. Policies `CMP0000` through `CMP0065` always use NEW behavior.
2. **`if()` auto-dereference.** Unquoted arguments in specific operator positions resolve as variable names, while quoted strings never dereference (`CMP0054` is always NEW). The truthy and falsy constants (`ON`, `YES`, `TRUE`, `Y`, nonzero numbers, `OFF`, `NO`, `FALSE`, `N`, `IGNORE`, `NOTFOUND`, `*-NOTFOUND`, and the empty string) match CMake 4.5.1 exactly.
3. **Scopes.** `macro()` substitutes its parameters as text in the caller's scope without creating a variable scope, so `return()` inside a macro returns from the enclosing function or file. `function()` creates a variable scope, and `set(... PARENT_SCOPE)` or `return(PROPAGATE ...)` writes to the caller's scope. `block()` creates variable and policy scopes.
4. **Regular expressions.** CMake uses its own regex dialect with no non-greedy operators, its own anchor rules, and the `CMAKE_MATCH_<n>` and `CMAKE_MATCH_COUNT` side effects. `rcmake-regex` implements this dialect from scratch rather than using the Rust `regex` crate.
5. **Generator expressions.** Configure preserves unevaluated generator expressions. The parser handles nested `$<...>`, semicolons, and the full operator set, and multi-config generators evaluate them once for each configuration.
6. **macOS defaults.** The compiler stays `/usr/bin/cc` instead of mapping into Xcode, and `rcmake` adds `-isysroot` only when `CMAKE_OSX_SYSROOT` is set.
7. **Lists.** A list is a string separated by `;`, where `\;` is a literal semicolon and square brackets suppress splitting. `rcmake-base::list` gives every caller the same splitting rule.
8. **Bundled modules.** `CMAKE_ROOT` points to `share/cmake-4.5` next to the binary. `project()` and `enable_language()` execute the bundled CMake 4.5.1 modules directly so `rcmake` does not hardcode compiler flags or platform tables in Rust.
9. **Recursion limit.** `CMAKE_MAXIMUM_RECURSION_DEPTH` (default 1000) stops deep script recursion with `Maximum recursion depth of <n> exceeded`, whereas CMake crashes far above that depth. Structured block nesting (`if`, `foreach`, `while`, `block`) tracks its own counter with the same limit. On a native thread with a stack budget, `rcmake` also stops the measured recursion paths below before the native stack overflows (ADR-037).

## Native stack use

Because the command depth counter does not measure native stack consumption, `rcmake_eval::stack_guard` estimates it by comparing the address of a local variable on each command call against the address recorded at the start of the thread's budget. The `rcmake` worker thread gets a 64 MiB stack with a 56 MiB budget, a speculative probe worker gets a 16 MiB stack with a 12 MiB budget, and the LSP evaluation thread gets a 32 MiB stack with a 24 MiB budget. An API call on a thread without an explicit budget receives a 1 MiB budget, so the caller's thread must have at least 2 MiB of free stack. If the `rcmake` worker thread fails to start, `rcmake` exits with an internal error. On `wasm32` targets the build omits the stack guard, and the WebAssembly entry point limits command recursion and block nesting to 200 levels instead.

The table below shows the measured stack bytes for each level of script recursion from `stack_bytes_per_level` in `rcmake-eval` on macOS arm64.

| Recursion path | Debug | Release |
| --- | --- | --- |
| `function()` | 8,055 | 2,726 |
| `macro()` | 7,719 | 2,726 |
| `cmake_language(EVAL CODE)` | 13,786 | 5,416 |

At the default limit of 1000 levels, the worst measured release path uses about 5.4 MB, well inside the 56 MiB worker budget. The guard stops these measured paths before the stack overflows on any thread with a budget. Other native recursion between command frames (expression, generator expression, `math`, JSON, and syntax parsing) enforces its own depth limits, and the headroom above each budget reserves space for those frames.

## Verification

### Differential testing against CMake 4.5.1

The official CMake 4.5.1 binary is the reference oracle. `scripts/fetch-oracle.sh` downloads it, and `env/env.sh` exports its paths in `RCMAKE_ORACLE_CMAKE` and related variables. Only `rcmake-harness`, `tools/`, and the differential tests read these variables. The `rcmake` binary never runs the oracle, and the licence scanner enforces that separation.

Layer numbers match [BUILD_WRITEUP.md](BUILD_WRITEUP.md#4-the-differential-oracle-method).

| Layer | Scope | Pass criterion |
| --- | --- | --- |
| 1. Unit and integration tests | `cargo test --workspace` on `PureHost` and `NativeHost` | All tests pass on Linux, macOS, and Windows |
| 2. Recorded CLI and script cases | `cmake -P`, `cmake -E`, CLI flags, `configure_file`, diagnostics, SARIF, presets | Byte-for-byte stdout, stderr, generated files, and exit code after path normalization |
| 3. Upstream `Tests/RunCMake` | CMake 4.5.1 conformance suite covering scripts, policies, diagnostics, targets, generator expressions, presets, and the File API. `runcmake_eval.py` lists the excluded suites and the reason for each. | Same pass or fail result and same diagnostic matches |
| 4. Independent black-box suite | Small C, C++, Objective-C++, Fortran, and presets projects that both tools build, install, test, and package | All explicit checks pass for both tools, and normalized observations match |
| 5. Large C++ projects | 10 open-source projects at pinned tags using the reduced option sets in `large_projects.toml` | Level L1 requires configure to succeed with matching stderr. Level L2 also requires matching `CMakeCache.txt`, Ninja commands, graph edges, and File API replies for the listed targets. |
| 6. Fuzzing and ecosystem lanes | Seeded generative fuzzing (not coverage-guided) across 14 input parsers, plus `vcpkg`, Conan, and Qt 6 | No panics or resource exhaustion, and package-manager builds match |
| 7. Sealed unseen holdout | 240 repositories that no one opened during development | Level L2 parity on the frozen release binary |

For IDE generators, `rcmake` compares canonical forms rather than raw bytes because CMake 4.5.1 Xcode output changes between runs and Visual Studio output requires MSBuild to build. `tools/measure/pbxproj_canon.py` and `tools/measure/vs_canon.py` produce those canonical forms.

### Test tools

| Tool | Purpose |
| --- | --- |
| `rcmake-harness` | Layer 2. Runs `.case` files from `conformance/cases/` and compares output against the oracle or against the baselines in `conformance/baselines/`. |
| `tools/measure/runcmake_eval.py` | Layer 3. Runs upstream `Tests/RunCMake` suites with the staged binary. |
| `tools/measure/independent_verify.py` | Layer 4. Runs black-box scenarios that build small C, C++, Objective-C++, Fortran, and presets projects with both tools. |
| `tools/measure/corpus_eval.py`, `tools/corpus/` | Layers 5 and 7. Builds real projects from `conformance/corpus/` with `PATH` replacement and Ninja command comparison. |
| `tools/measure/bench_eval.py` | Measures median and p95 wall time and peak memory for `rcmake` and the oracle. Each tool uses a private probe store, and a cold run starts with an empty store. |
| `tools/measure/bench_configure.py`, `tools/measure/bench_giants.py` | Measures new build tree configure times with a shared probe cache, leaf edit reconfigures, and large-project configure times. Each report records the host hardware. |
| `tools/measure/determinism.py` | Verifies that two build directories receive identical generated files. |
| `rcmake-fuzz` | Layer 6. Runs seeded generative input (not coverage-guided) with a time budget per case. Input *i* uses seed + *i*, so you can replay any failure from its seed. |

### Unit tests

Most unit tests run on `PureHost`, so they execute without the oracle and without touching the file system. The shared host conformance suite runs against both hosts in CI on Linux, macOS, and Windows.
