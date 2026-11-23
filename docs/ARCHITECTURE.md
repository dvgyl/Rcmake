# Architecture

Rcmake is structured around the real four-stage execution model of CMake 4.5.1, decomposed into **disjoint crates** so multiple parallel workers (primarily GPT 6.2 Sol) can implement, test, and review subsystems concurrently without merge conflicts.

## Execution Pipeline

1. **CLI & Presets (`rcmake-cli`, `rcmake-presets`):** Parses invocation mode (`argv[0]` multi-call dispatch for `cmake`, `ctest`, `cpack`, plus `-P`, `-E`, `--build`, `--install`, `--workflow`, `--preset`), loads `CMakePresets.json` / `CMakeUserPresets.json` (schema v13), and sets up the `Host`.
2. **Parse & Evaluate (`rcmake-syntax`, `rcmake-regex`, `rcmake-expr`, `rcmake-eval`, `rcmake-commands`):** Parses `CMakeLists.txt` / `.cmake` scripts, enforces policies (`CMP0066`–`CMP0219+`) and diagnostics (`cmake_diagnostic()`), executes commands, manages `CMakeCache.txt`, and builds the in-memory `ProjectModel` (directories, targets, tests, install rules, custom commands/rules).
3. **Generate (`rcmake-genex`, `rcmake-model`, `rcmake-gen-*`):** Evaluates `$<...>` generator expressions per configuration, resolves transitive usage requirements (`PUBLIC`/`PRIVATE`/`INTERFACE`), computes link closures and C++20 module `dyndep` graphs, emits File-API (`codemodel` v2.12), `compile_commands.json`, CPS packages, SBOMs, and writes build files (`Ninja`, `Ninja Multi-Config`, `Unix Makefiles`, and later IDE generators).
4. **Build / Test / Package (`rcmake-cli`, `rcmake-ctest`, `rcmake-cpack`):** Drives underlying build tools (`cmake --build`), executes install scripts (`cmake --install`), runs tests (`ctest` + `discover_tests`), and builds distribution packages (`cpack`).

## Workspace Crate Map

| Crate | Responsibility | Depends on |
| --- | --- | --- |
| `rcmake-diag` | Source spans, CMake-exact stack traces, `cmake_diagnostic()` categories (`CMD_NON_TARGET_DIRECTIVE`, `CMD_STRICT`, etc.), `CMP0066`–`CMP0219+` policy stack, `--sarif-output` | none |
| `rcmake-syntax` | Zero-copy lexer/parser for CMake 4.5.1 grammar: unquoted/quoted/bracket (`[=[...]=]`) arguments, escapes, line continuations, comments, AST | `rcmake-diag` |
| `rcmake-regex` | CMake-compatible regular expression engine (POSIX/Henry-Spencer syntax & `CMAKE_MATCH_0..9` semantics — distinct from Rust `regex`/PCRE) | `rcmake-diag` |
| `rcmake-host` | `Host` trait (FS, env, processes, symlinks, permissions, clock, network/download), `NativeHost`, deterministic `PureHost`, resource/timeout limits | `rcmake-diag` |
| `rcmake-expr` | Variable expansion (`${...}`, `$ENV{...}`, `$CACHE{...}`, nested expansion), `if()` / `while()` condition evaluator including legacy auto-deref rules | `rcmake-diag`, `rcmake-regex` |
| `rcmake-genex` | Full `$<...>` generator expression parser, AST, compile/link/install context rules, and multi-config evaluator | `rcmake-diag`, `rcmake-regex` |
| `rcmake-model` | `CMakeCache.txt` store, global/directory/target/source/test properties, target graph (`EXECUTABLE`, `STATIC`, `SHARED`, `MODULE`, `OBJECT`, `INTERFACE`, `IMPORTED`, `ALIAS`), transitive usage requirements, file sets (`HEADERS`, `CXX_MODULES`, `SOURCES`), CPS, SBOM metadata, File-API v2.12, `compile_commands.json` | `rcmake-diag`, `rcmake-genex`, `rcmake-host` |
| `rcmake-eval` | Interpreter runtime: directory/function/`block()` scopes, `PARENT_SCOPE`, `return(PROPAGATE ...)`, `macro` textual expansion (`CMP0219`), `cmake_language()`, command dispatch registry | `rcmake-syntax`, `rcmake-expr`, `rcmake-model`, `rcmake-host` |
| `rcmake-commands` | Built-in CMake commands grouped into disjoint submodules (`string`, `list`, `math`, `file`, `cmake_path`, `find_*`, `process`, `configure_file`, `project_lang`, `target`, `custom_rule`, `install_export`, `test_cmd`, `parse_args`, `policy_diag`) | `rcmake-eval`, `rcmake-model`, `rcmake-regex`, `rcmake-host` |
| `rcmake-presets` | `CMakePresets.json` / `CMakeUserPresets.json` schema v13 parser, inheritance, macro expansion (`${configurePresetName}`, etc.), validation | `rcmake-diag`, `rcmake-host` |
| `rcmake-tool-e` | All `cmake -E` cross-platform CLI tools (`copy`, `tar`, `md5sum`/`sha*sum`, `env`, `rm`, `cat`, `touch`, `make_directory`, `create_symlink`, `compare_files`, `time`, `capabilities`, etc.) | `rcmake-diag`, `rcmake-host` |
| `rcmake-gen-ninja` | `Ninja` and `Ninja Multi-Config` generators, rspfiles, depfiles, C++20/23 module `dyndep` integration | `rcmake-model`, `rcmake-genex`, `rcmake-host` |
| `rcmake-gen-make` | `Unix Makefiles` generator, implicit dependency scanning, CMake 4.5 grouped parallel output (`--output-sync`) | `rcmake-model`, `rcmake-genex`, `rcmake-host` |
| `rcmake-gen-ide` | Platform IDE generators (`Xcode`, `Visual Studio` — Milestone 4, isolated from Tier-1 builds) | `rcmake-model`, `rcmake-genex`, `rcmake-host` |
| `rcmake-ctest` | `ctest` engine: `CTestTestfile.cmake`, parallel scheduler, fixtures, resource allocation specs, `discover_tests(BUILD_DEPENDS ...)`, JUnit/CDash XML | `rcmake-eval`, `rcmake-commands`, `rcmake-host` |
| `rcmake-cpack` | `cpack` engine: `CPackConfig.cmake`, archive generators (`TGZ`, `TXZ`, `TZST`, `ZIP`), platform packagers (`DragNDrop`/DMG, `DEB`, `RPM`, etc.) | `rcmake-eval`, `rcmake-commands`, `rcmake-host` |
| `rcmake-cli` | Unified multi-call binary (`rcmake`, plus `cmake` / `ctest` / `cpack` dispatch via `argv[0]`), `--build`, `--install`, `--workflow`, instrumentation hooks | all engine crates |
| `rcmake-harness` | Isolated differential test harness comparing `rcmake` against official CMake 4.5.1 (does not link core crates; invokes binaries black-box) | none of the core crates |

## Coordinator-Owned Shared Contracts

To prevent merge conflicts during parallel agent waves, **only the coordinator (main thread)** modifies:
- Root `Cargo.toml`, `Cargo.lock` dependency additions, `scripts/`, `PROGRESS.md`, `docs/ADR.md`
- Public trait signatures and core data types in `rcmake-diag`, `rcmake-syntax`, `rcmake-host`, and `rcmake-model`
- The command registry wiring in `rcmake-commands/src/lib.rs`
- `rcmake-harness` case format and baseline manifests

Workers implement disjoint crates or disjoint submodules under `rcmake-commands/src/<family>/` and request any shared contract change from the coordinator.

## Critical CMake 4.5.1 Invariants & Known Traps

Design these right on Day 1 (consult **GPT 6.2 Astra** and **Opus 5.5** before locking contracts):

1. **CMake 4.0+ Policy Baseline:**
   - Reject `cmake_minimum_required(VERSION <x>)` where `<x> < 3.5` (unless `CMAKE_POLICY_VERSION_MINIMUM` is set to `>= 3.5`).
   - `CMP0000`–`CMP0065` always behave as `NEW` (OLD behaviors are deleted). Track explicit policy states for `CMP0066` through `CMP0219+`.
2. **`if()` Auto-Dereference & Truthiness:**
   - Unquoted arguments in `if()` undergo legacy variable dereferencing in specific operator positions; quoted strings never auto-deref (`CMP0054` is unconditionally NEW in 4.x).
   - Truthy/falsy literals (`ON`, `YES`, `TRUE`, `Y`, non-zero numbers vs `0`, `OFF`, `NO`, `FALSE`, `N`, `IGNORE`, `NOTFOUND`, `*-NOTFOUND`, empty) must match CMake 4.5.1 byte-for-byte.
3. **Macro vs. Function vs. `block()` Scoping:**
   - `macro()` performs textual parameter substitution (`${ARGC}`, `${ARGV}`, `${ARGN}`, `${ARGV0}`, and `CMP0219` backslash preservation in 4.5) in the caller's scope — it does **not** create a variable scope and `return()` returns from the enclosing function/file!
   - `function()` creates a variable scope; `set(... PARENT_SCOPE)` and `return(PROPAGATE ...)` update the caller's scope.
   - `block(SCOPE_FOR VARIABLES POLICIES PROPAGATE ...)` introduces lexical variable/policy scopes.
4. **CMake Regex Semantics:**
   - CMake regexes follow its historical POSIX/Henry-Spencer dialect (no non-greedy `*?`, specific `^`/`$` anchoring, `CMAKE_MATCH_0..9` + `CMAKE_MATCH_COUNT` side effects). Build or wrap a faithful dialect in `rcmake-regex` rather than passing raw strings to Rust `regex` blindly.
5. **Generator Expressions (`$<...>`):**
   - Must preserve unevaluated genexes during configure, handle nested `$<...>` with balanced angle brackets and semicolons, support `$<GENEX_EVAL:...>`, `$<LINK_ONLY:...>`, `$<BUILD_INTERFACE:...>`, `$<INSTALL_INTERFACE:...>`, `$<TARGET_PROPERTY:...>`, `$<COMPILE_LANGUAGE:...>`, `$<PATH:NATIVE_PATH,...>`, `$<LIST:...>`, and config-specific evaluation for `Ninja Multi-Config`.
6. **macOS 4.0+ Compiler & SDK Defaults:**
   - Do not remap `/usr/bin` compilers into Xcode automatically; do not force `-isysroot` unless `CMAKE_OSX_SYSROOT` is explicitly set (matches CMake 4.0+ macOS behavior).
7. **List Semicolon Escaping (`\;`):**
   - CMake lists are `;`-delimited strings where escaping interacts subtly with argument splitting, `foreach(IN LISTS ...)`, `list(...)`, and `set(...)`. Model list splitting/joining explicitly and test heavily against `cmake -P`.
8. **Vendored `Modules/` Bootstrap Path:**
   - `CMAKE_ROOT` / `CMAKE_MODULE_PATH` must cleanly locate vendored CMake 4.5.1 `Modules/` and `Templates/` (either embedded in a packed bundle or loaded from `share/cmake-4.5/` relative to the binary) so `project()`, `find_package()`, `CheckCCompilerFlag`, `GNUInstallDirs`, and `FetchContent` work out of the box.

## Layered Differential Oracle Strategy

Unlike a pure text converter, CMake requires **four verification layers** against the official `cmake 4.5.1` binary:

| Layer | What it tests | Pass criterion |
| --- | --- | --- |
| **Layer 1: Exact CLI / Script Parity** | `cmake -P`, `cmake -E`, CLI flags, `configure_file`, diagnostics/warnings, `--sarif-output`, Presets v13 | **Byte-for-byte** stdout, stderr (normalized paths), generated files, and exit code match vs. `cmake 4.5.1` |
| **Layer 2: Upstream `Tests/RunCMake`** | CMake 4.5.1's own BSD-3-Clause conformance suite (script, policy, diagnostic, target, genex, preset, File-API cases) | Matching pass/fail exit codes and diagnostic regex assertions |
| **Layer 3: Structured Graph Parity** | File-API (`codemodel` v2.12, `cache-v2`, `toolchains-v1`), `compile_commands.json`, `CMakeCache.txt` | Normalized structural JSON / semantic parity vs. `cmake 4.5.1` |
| **Layer 4: Real-World C/C++ Corpus** | End-to-end `rcmake` configure → `ninja`/`make` build → `rcmake --install` → `ctest` on real open-source projects | Zero build errors, identical install file trees, and 100% matching `ctest` results vs. `cmake 4.5.1` |
