# Architecture

`rcmake` splits into 40 acyclic safe-Rust crates (`#![forbid(unsafe_code)]`) matching the four-stage execution model of CMake 4.5.1, plus clean-room interactive, diagnostic, Windows shim, and language tooling.

## Contents

- [Execution pipeline](#execution-pipeline)
- [Resolution layers](#resolution-layers)
- [Workspace crate map](#workspace-crate-map)
- [The `Host` abstraction](#the-host-abstraction)
- [CMake 4.5.1 invariants](#cmake-451-invariants)
- [Native stack use](#native-stack-use)
- [Verification](#verification)

## Execution pipeline

```text
1. Command line  rcmake-cli, rcmake-presets, rcmake-ccmake, rcmake-dap
                      |
                      v
2. Configure     rcmake-syntax -> rcmake-expr -> rcmake-eval + rcmake-commands
                      |
                      v
                 rcmake-model (ProjectModel)
                      |
                      v
3. Generate      rcmake-resolve + rcmake-genex (ResolvedTarget, BuildGraph)
                      |
                      +-> rcmake-gen-ninja, rcmake-gen-make, rcmake-gen-xcode, rcmake-gen-vs
                      +-> rcmake-gen-common (File API, install scripts, CTest, CPS, SBOM, compile_commands)
                      |
                      v
4. Use           cmake --build, cmake --install, rcmake-ctest, rcmake-cpack
```

1. **Command line (`rcmake-cli`, `rcmake-presets`, `rcmake-ccmake`, `rcmake-dap`).** The binary selects `cmake`, `ccmake`, `ctest`, `cpack`, `rcmake-lsp`, or native `rcmake` from `argv[0]`. It parses CLI options, loads Presets schema v13, optionally attaches the Unix DAP server (`--debugger`), or runs the interactive Unix `ccmake` terminal interface over a retained configure session.
2. **Configure (`rcmake-syntax`, `rcmake-regex`, `rcmake-expr`, `rcmake-eval`, `rcmake-commands`).** The parser reads `CMakeLists.txt` and `.cmake` files. The evaluator executes commands, tracks scopes, policies, and diagnostics, runs sound incremental replay with setup checkpoints, and updates `CMakeCache.txt` to build a `ProjectModel`.
3. **Generate (`rcmake-resolve`, `rcmake-genex`, `rcmake-gen-*`).** The resolver evaluates generator expressions per configuration, resolves transitive usage requirements, link closures, C++20 and Fortran modules, CUDA, HIP, Swift, and Qt AUTOGEN rules, and lowers targets into a `BuildGraph`. Generators only format `BuildGraph`, while `rcmake-gen-common` writes File API replies, `compile_commands.json` (including native multi-config databases), install scripts, CTest files, exports, CPS, and SBOM files.
4. **Build, test, package, and inspect (`rcmake-cli`, `rcmake-ctest`, `rcmake-cpack`, `rcmake-buildinfo`, `rcmake-language-tools`).** `--build` and `--install` drive builds and installs, `ctest` executes tests, `cpack` builds archives and native installers, `rcmake-buildinfo` powers `diff-config` and `verify`, and `rcmake-language-tools` powers `fmt`, `lint`, and `rcmake-lsp`.

`rcmake-api` runs stages 2 and 3 in process for other Rust programs, `rcmake-lsp` calls `rcmake-api` for evaluated editor queries, and `rcmake-wasm` runs `rcmake-api` on `PureHost` for the browser playground.

## Resolution layers

| Layer | Owner | Contents |
| --- | --- | --- |
| `ProjectModel` | `rcmake-model` | Raw configure state including cache entries, properties, directories, targets, tests, and install rules with backtraces |
| `ResolvedTarget` | `rcmake-resolve` | Typed per-configuration target facts with generator expressions evaluated, read by File API and `install(EXPORT)` |
| `BuildGraph` | `rcmake-resolve` | Compile, link, device-link, and custom-command steps with escaped arguments plus the install manifest, read by generators and `rcmake-build` |

Because generator expressions such as `$<TARGET_PROPERTY:...>` query transitive usage requirements during resolution, `rcmake-genex` defines the `GenexHost` trait and `rcmake-resolve` implements it, keeping the crate graph acyclic.

## Workspace crate map

The "Depends on" column lists workspace crates only. Every crate except `rcmake-base` depends on `rcmake-base`.

### Foundation and language

| Crate | Responsibility | Depends on |
| --- | --- | --- |
| `rcmake-base` | Byte strings, CMake lists, truthiness (`is_on`, `is_off`), versions, lexical paths, hashes, and resource limits | none |
| `rcmake-json` | JsonCpp-compatible JSON parser and writers for `string(JSON)`, presets, File API, and SARIF | `base` |
| `rcmake-diag` | Source spans, backtraces, CMake 4.5.1 diagnostic text, policy registry `CMP0000` to `CMP0226`, and SARIF 2.1.0 | `base`, `json` |
| `rcmake-host` | `Host` trait for filesystem, environment, process, and clock access, plus `NativeHost`, `PureHost`, and conformance suite | `base` |
| `rcmake-syntax` | Raw-byte lexer, strict `ListFile` parser, and lossless concrete token stream (`0..src.len()`) for `fmt`, `lint`, and `lsp` | `base`, `diag` |
| `rcmake-regex` | Independent CMake (KWSys) byte regex engine with `CMAKE_MATCH_<n>` and glob conversion | `base` |
| `rcmake-expr` | Variable expansion (`${}`, `$ENV{}`, `$CACHE{}`), escapes, `if()` and `while()` evaluation, and `configure_file` expansion | `base`, `diag`, `host`, `regex`, `syntax` |
| `rcmake-genex` | Generator expression parser, evaluator, `GenexHost` trait, and `GENEX_STRIP` helpers | `base`, `diag`, `regex` |

### Configure, resolution, and build

| Crate | Responsibility | Depends on |
| --- | --- | --- |
| `rcmake-model` | Cache store, property maps, global state, and arena-based `ProjectModel` (`vs-cross` and `xcode-cross` test features) | `base`, `diag` |
| `rcmake-eval` | Script interpreter, variable, policy, and diagnostic scopes, `function`, `macro`, `block`, `DebugHook`, setup-checkpoint replay journal, and hermetic policy | `base`, `diag`, `expr`, `host`, `json`, `model`, `regex`, `syntax` |
| `rcmake-commands` | Built-in CMake commands, `project()`, `enable_language()`, `try_compile()`, probe cache, replay activation, and FetchContent | `archive` (opt), `base`, `build`, `cps`, `diag`, `eval`, `expr`, `fetch` (opt), `genex`, `host`, `instrument`, `json`, `model`, `regex`, `resolve`, `store` (opt), `syntax` |
| `rcmake-presets` | Presets schema v13 loader, `include` resolution, inheritance, conditions, and macro expansion | `base`, `diag`, `host`, `json`, `regex` |
| `rcmake-resolve` | Per-config target resolution, transitive usage requirements, link lines, RPATH, CUDA, HIP, Swift, Fortran, Qt AUTOGEN, PCH, unity builds, and `BuildGraph` lowering | `base`, `cxxmodules`, `diag`, `fortran`, `genex`, `host`, `json`, `model` |
| `rcmake-build` | In-process `BuildGraph` executor for `try_compile()` and generator-less build fallback | `base`, `cxxmodules`, `diag`, `host`, `resolve` |
| `rcmake-cxxmodules` | C++20 named modules, P1689r5 depfiles, `CXXDependInfo.json`, `CXXModules.json`, Ninja `dyndep`, and module maps | `base`, `json` |
| `rcmake-fortran` | Pure-Rust Fortran source scanning and `.mod` dependency collation with no disk access | `base` |

### Generators, test, package, and storage

| Crate | Responsibility | Depends on |
| --- | --- | --- |
| `rcmake-gen-common` | Shared generator outputs including File API, `compile_commands.json` (plus native multi-config databases), `cmake_install.cmake`, `CTestTestfile.cmake`, exports, CPS, and SBOM | `base`, `cps`, `diag`, `genex`, `host`, `json`, `model`, `regex`, `resolve` |
| `rcmake-gen-ninja` | `Ninja` and `Ninja Multi-Config` generators with response files, depfiles, Swift multi-edge rules, and C++20 `dyndep` | `base`, `cxxmodules`, `diag`, `gen-common`, `genex`, `host`, `model`, `resolve` |
| `rcmake-gen-make` | `Unix Makefiles` generator with header dependency scanning and grouped parallel output | `base`, `diag`, `gen-common`, `genex`, `host`, `model`, `resolve` |
| `rcmake-gen-xcode` | `Xcode` generator for `project.pbxproj`, schemes, workspace settings, Swift settings, and `xcodebuild` plans (ADR-014) | `base`, `diag`, `gen-common`, `host`, `model`, `resolve` |
| `rcmake-gen-vs` | `Visual Studio 17 2022` (`.sln`) and `18 2026` (`.slnx`) generators for `.vcxproj`, filters, stable GUIDs, and MSBuild plans (ADR-015) | `base`, `diag`, `gen-common`, `host`, `model`, `resolve` |
| `rcmake-ctest` | `ctest` driver for `CTestTestfile.cmake`, parallel scheduling, GNU make jobserver, fixtures, resources, JUnit and dashboard XML, and `-S` scripts | `base`, `commands`, `diag`, `eval`, `expr`, `host`, `instrument`, `json`, `model`, `presets`, `regex` |
| `rcmake-cpack` | `cpack` driver for archives (`TGZ`, `TXZ`, `TZST`, `TBZ2`, `TZ`, `TAR`, `STGZ`, `ZIP`, `7Z`), Linux `DEB` and `RPM`, `External`, macOS `Bundle`, `DragNDrop`, `productbuild`, Windows `WIX` (v3 and v4), and cross-platform `NSIS`, `NSIS64`, and `IFW` (ADR-010, ADR-042) | `archive`, `base`, `commands`, `diag`, `eval`, `host`, `json`, `presets` |
| `rcmake-cps` | Common Package Specification 0.15.0 data model, writer, and validating reader (ADR-013) | `base`, `diag`, `json` |
| `rcmake-instrument` | Instrumentation API v1 (data v1.2) queries, snippets, index files, hooks, and traces (ADR-013) | `base`, `host`, `json` |
| `rcmake-tool-e` | Built-in `cmake -E` utilities including `tar`, hashes, `cmake_ninja_dyndep`, and Fortran and Qt tool drivers | `archive`, `base`, `cxxmodules`, `diag`, `fortran`, `host`, `json` |
| `rcmake-archive` | `tar`, gzip, xz, zstd, bzip2, zip, and 7-Zip codecs, path checks, and deterministic portable cache archives | `base`, `host` |
| `rcmake-store` | Content-addressed probe blob store, input stamps, garbage collection, portable import and export, and remote cache recovery | `base`, `host`, `json` |
| `rcmake-fetch` | FetchContent archive cache, Git mirrors, and project lockfiles (ADR-021) | `base`, `host`, `json` (opt), `store` (opt) |

### Developer tools, front ends, and verification

| Crate | Responsibility | Depends on |
| --- | --- | --- |
| `rcmake-buildinfo` | Bounded readers and 6-section comparator for File API replies, `CMakeCache.txt`, `compile_commands.json`, and find traces, never invoking `cmake` (ADR-043) | `base`, `host`, `json` |
| `rcmake-language-tools` | `.rcmake.toml` configuration loader, bounded file discovery, lossless formatter (`rcmake fmt`), and 8-rule static linter (`rcmake lint`) (ADR-044) | `base`, `diag`, `expr`, `host`, `json`, `syntax` |
| `rcmake-ccmake` | Clean-room Unix terminal cache editor (`ccmake`) with `Terminal` and `CacheDriver` traits, no `ncurses` (ADR-040) | `base`, `model` |
| `rcmake-dap` | CMake 4.5.1 `--debugger` Debug Adapter Protocol server over Unix domain sockets with bounded frames and nonblocking writer (ADR-041) | `base`, `diag`, `eval`, `host`, `json`, `model` |
| `rcmake-cli` | Multi-call `rcmake` binary dispatching `cmake`, `ccmake`, `ctest`, `cpack`, `rcmake-lsp`, and native subcommands (`explain`, `diff-config`, `fmt`, `lint`, `verify`, `doctor`, `cache`) | `api`, `archive`, `base`, `buildinfo`, `ccmake`, `commands`, `cpack`, `ctest`, `dap`, `diag`, `eval`, `gen-common`, `gen-make`, `gen-ninja`, `gen-vs` (Windows or `vs-cross`), `gen-xcode` (macOS or `xcode-cross`), `genex`, `host`, `instrument`, `json`, `language-tools`, `lsp`, `model`, `presets`, `resolve`, `store`, `syntax`, `tool-e` |
| `rcmake-shim` | Tiny dependency-free safe-Rust Windows launcher shim forwarding `cmake.exe`, `ctest.exe`, `cpack.exe`, and `rcmake-lsp.exe` to sibling `rcmake.exe` (ADR-048) | none |
| `rcmake-api` | In-process `run_configure`, workspace index, provenance, and `explain` queries, compiling to WebAssembly with `--no-default-features` (ADR-023, ADR-024) | `base`, `commands`, `diag`, `eval`, `genex`, `host`, `json`, `model`, `resolve`, `syntax` |
| `rcmake-lsp` | CMake LSP server sharing `rcmake-language-tools` formatting and 8 lint rules plus optional `eval` snapshot hover and `stdio` binary | `api` (opt), `base`, `diag`, `genex` (opt), `host`, `json`, `language-tools`, `model` (opt), `resolve` (opt), `syntax` |
| `rcmake-wasm` | Browser playground WASI runtime on `PureHost`, built into `site/wasm/` with no `unsafe` and no `wasm-bindgen` | `api`, `base`, `diag`, `gen-common`, `gen-ninja`, `host`, `json`, `model`, `presets`, `resolve` |
| `rcmake-harness` | Isolated differential harness (`rcmake-harness`) and sibling `rcmake-verify` companion binary, linking no configure or generator engine crate (ADR-045) | `base`, `buildinfo`, `host`, `json`, `presets` |
| `rcmake-fuzz` | Safe-Rust seeded generative fuzzer covering 14 parsers, never linked into `rcmake` (ADR-016) | `archive`, `base`, `cps`, `diag`, `expr`, `fortran`, `genex`, `host`, `json`, `presets`, `regex`, `store`, `syntax` |

## The `Host` abstraction

Engine crates never call `std::fs`, `std::env`, `std::process`, or the system clock directly. `clippy.toml` bans direct calls everywhere except `rcmake-host`, the `rcmake-cli` entry point, and `rcmake-harness`.

- **`NativeHost`.** Real OS backend using `rustix` on Unix and job objects on Windows for process groups and timeouts. It snapshots the process environment once and passes overlays to children so the engine never calls `std::env::set_var`.
- **`PureHost`.** In-memory filesystem with symlinks, virtual timestamps, scripted subprocesses, and ordered output logs, powering unit tests and WebAssembly builds.

Every `Host` is `Send + Sync`, accepts only absolute paths, keeps raw OS error text for CMake diagnostic parity, and passes `rcmake_host::conformance::run_all`. Thin wrappers over `Host` implement `JournalHost` (probe cache and replay), `CaptureHost` (`ccmake` output capture), hermetic input auditing, and `rcmake-lsp` unsaved-buffer overlays without changing the evaluator core.

## CMake 4.5.1 invariants

1. **Policy baseline.** `cmake_minimum_required(VERSION <x>)` below 3.5 errors unless `CMAKE_POLICY_VERSION_MINIMUM` is 3.5 or higher, and `CMP0000` through `CMP0065` always use NEW behavior.
2. **`if()` auto-dereference.** Unquoted operands in specific operator positions dereference as variable names while quoted strings never dereference (`CMP0054` NEW). Truthy and falsy constants match CMake 4.5.1 exactly.
3. **Scopes.** `macro()` expands textually in the caller's scope (`return()` exits the enclosing function or file), `function()` creates a variable scope (`PARENT_SCOPE` and `return(PROPAGATE ...)`), and `block()` scopes variables and policies.
4. **Regular expressions.** `rcmake-regex` implements the KWSys dialect (no non-greedy quantifiers, distinct anchor rules, `CMAKE_MATCH_<n>` side effects) from scratch.
5. **Generator expressions.** Configure preserves unevaluated `$<...>` strings and multi-config generators evaluate them once per configuration.
6. **macOS defaults.** The default compiler stays `/usr/bin/cc`, and `-isysroot` is emitted only when `CMAKE_OSX_SYSROOT` is set.
7. **Lists.** Lists split on `;` with `\;` escapes and bracket awareness via `rcmake-base::list`.
8. **Bundled modules.** `CMAKE_ROOT` points to `share/cmake-4.5` next to the binary so `project()` and `enable_language()` execute upstream CMake 4.5.1 modules directly.
9. **Recursion limit.** `CMAKE_MAXIMUM_RECURSION_DEPTH` (default 1000) stops deep script recursion and structured block nesting cleanly instead of crashing.

## Native stack use

Because the command depth counter does not measure native stack frames, `rcmake_eval::stack_guard` estimates stack depth from local-variable addresses on each command call (ADR-037). The main `rcmake` worker thread gets a 64 MiB stack (56 MiB budget), speculative probe workers get 16 MiB (12 MiB budget), LSP evaluation gets 32 MiB (24 MiB budget), and unbudgeted API callers get a 1 MiB budget requiring at least 2 MiB free stack. `wasm32` omits the stack guard and caps command and block depth at 200.

Measured stack bytes per recursion level from `stack_bytes_per_level` on macOS arm64 are 8,055 debug and 2,726 release for `function()`, 7,719 debug and 2,726 release for `macro()`, and 13,786 debug and 5,416 release for `cmake_language(EVAL CODE)`. At 1,000 levels, the deepest release path uses about 5.4 MB inside the 56 MiB worker budget.

## Verification

The pinned CMake 4.5.1 binary is the reference oracle. Only `rcmake-harness` (including `rcmake-verify`), `tools/`, and differential tests read `RCMAKE_ORACLE_CMAKE`. The `rcmake` binary never executes upstream `cmake`, enforced by `tools/measure/licence_scan.py`.

| Layer | Suite and tool | Pass criterion |
| --- | --- | --- |
| 1 | Unit and integration tests (`cargo test --workspace`) | Pass on Linux, macOS, and Windows across `PureHost` and `NativeHost` |
| 2 | Recorded CLI and script cases (`rcmake-harness`) | Byte-exact stdout, stderr, generated files, and exit code after path normalization |
| 3 | Upstream `Tests/RunCMake` (`runcmake_eval.py`) | Same pass or fail outcome and diagnostic matches across 3 OSes |
| 4 | Independent black-box suite (`independent_verify.py`) | All 679 checks across 20 scenarios pass and match on both binaries |
| 5 | 10 large C++ projects (`corpus_eval.py`) | L1 configure and stderr parity, plus L2 cache, Ninja graph, and File API parity |
| 6 | Fuzzing and ecosystem (`rcmake-fuzz`, `ecosystem_eval.py`, `vcpkg`) | Zero panics across 14 parsers and package-manager build parity |
| 7 | Sealed 240-repository unseen holdout (`unseen_eval.py`) | Aggregate parity measured once on the frozen release binary |

Xcode and Visual Studio diffs compare canonical forms produced by `tools/measure/pbxproj_canon.py` and `tools/measure/vs_canon.py`. Benchmark and soundness checks run via `bench_eval.py`, `bench_configure.py`, `bench_giants.py`, `determinism.py`, `probe_cache_eval.py`, and `mutation_eval.py`.
