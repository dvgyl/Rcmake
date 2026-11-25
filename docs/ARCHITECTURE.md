# Architecture

Rcmake follows the execution model of CMake 4.5.1. The work goes through four
stages: command line, configure, generate, and build, test, or package. The
workspace has 34 crates. Each crate has one responsibility, and the crate graph
has no cycles.

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

1. **Command line (`rcmake-cli`, `rcmake-presets`).** The binary selects
   `cmake`, `ctest`, or `cpack` from the name that starts it (`argv[0]`). It
   parses `-P`, `-E`, `--build`, `--install`, `--workflow`, `--preset`, and the
   other CMake options. It loads `CMakePresets.json` and
   `CMakeUserPresets.json` (schema version 13) and creates the `Host`.
2. **Configure (`rcmake-syntax`, `rcmake-regex`, `rcmake-expr`, `rcmake-eval`,
   `rcmake-commands`).** The parser reads `CMakeLists.txt` and `.cmake` files.
   The evaluator runs the commands, applies the policies and diagnostics, and
   updates `CMakeCache.txt`. The result is the `ProjectModel` in
   `rcmake-model`: directories, targets, tests, install rules, and custom
   commands.
3. **Generate (`rcmake-resolve`, `rcmake-genex`, `rcmake-gen-*`).** The
   resolver evaluates generator expressions (`$<...>`) for each configuration.
   It calculates transitive usage requirements, link closures, and C++ and
   Fortran module dependencies. It then lowers each target into a `BuildGraph`.
   The generators write Ninja, Makefile, Xcode, or Visual Studio files.
   `rcmake-gen-common` writes the outputs that do not depend on the generator:
   File API replies, `compile_commands.json`, install scripts, CTest files,
   exports, CPS files, and SBOM files.
4. **Build, test, and package (`rcmake-cli`, `rcmake-ctest`, `rcmake-cpack`).**
   `cmake --build` runs the native build tool. `cmake --install` runs the
   install scripts. `ctest` runs the tests. `cpack` creates the packages.

The `rcmake-api` crate runs stage 2 and stage 3 in-process for other programs.
The `rcmake-lsp` language server uses `rcmake-api` to evaluate open files.

## Resolution layers

The generate stage uses three data layers. Each layer has one owner crate.

| Layer | Owner | Contents |
| --- | --- | --- |
| `ProjectModel` | `rcmake-model` | Raw configure state: cache, properties, directories, targets, tests, install rules. Values keep their backtraces. |
| `ResolvedTarget` | `rcmake-resolve` | Typed target facts for one configuration, with all generator expressions evaluated. The File API and `install(EXPORT)` read this layer. |
| `BuildGraph` | `rcmake-resolve` | Compile, link, and custom command steps with escaped command lines, plus the install manifest. The generators and `rcmake-build` read this layer. |

Generator expression evaluation and usage resolution call each other. For
example, `$<TARGET_PROPERTY:...>` needs the transitive properties of a target.
To keep the crate graph acyclic, `rcmake-genex` defines the `GenexHost` trait,
and `rcmake-resolve` implements it.

## Workspace crate map

The "Depends on" column lists only workspace crates. All crates except
`rcmake-harness` depend on `rcmake-base`.

### Foundation

| Crate | Responsibility | Depends on |
| --- | --- | --- |
| `rcmake-base` | Byte strings, CMake lists, truthiness (`is_on`, `is_off`), version comparison, lexical paths, hashes, and input limits | none |
| `rcmake-json` | JSON parser and writers compatible with jsoncpp, for `string(JSON)`, presets, the File API, and SARIF | `base` |
| `rcmake-diag` | Source spans, backtraces, CMake 4.5.1 diagnostic text, the policy registry `CMP0000` to `CMP0226`, and SARIF 2.1.0 | `base`, `json` |
| `rcmake-host` | The `Host` trait for file system, environment, process, and clock access. `NativeHost`, `PureHost`, and a shared conformance suite. | `base` |

### Language

| Crate | Responsibility | Depends on |
| --- | --- | --- |
| `rcmake-syntax` | Lexer and parser for CMake listfiles. Keeps the raw bytes, argument kinds, and spans. Checks block nesting before execution. | `base`, `diag` |
| `rcmake-regex` | Clean-room byte regular expression engine with the CMake (KWSys) dialect, `CMAKE_MATCH_<n>`, and a glob converter | `base` |
| `rcmake-expr` | Variable expansion (`${}`, `$ENV{}`, `$CACHE{}`), escape decoding, the `if()` and `while()` condition evaluator, and the `configure_file` expander | `base`, `diag`, `host`, `regex`, `syntax` |
| `rcmake-genex` | Generator expression parser, evaluator, `GenexHost` trait, and preprocess helpers such as `string(GENEX_STRIP)` | `base`, `diag`, `regex` |

### Configure

| Crate | Responsibility | Depends on |
| --- | --- | --- |
| `rcmake-model` | Cache store, property maps, global state, and the arena-based `ProjectModel`. The `vs-cross` and `xcode-cross` features enable the IDE generators on all hosts for tests. | `base`, `diag` |
| `rcmake-eval` | Interpreter: variable, policy, and diagnostic scopes, `function`, `macro`, `block`, control flow, `cmake_language()`, the command registry, the configure journal, and the hermetic input policy | `base`, `diag`, `expr`, `host`, `model`, `regex`, `syntax` |
| `rcmake-commands` | The built-in CMake commands, grouped by family (`string_math`, `vars_list`, `file_path`, `process_find`, `targets`, `install`, `custom`, `configure`, `policy_misc`, `compat`, and others). Also `project()`, `enable_language()`, `try_compile()`, the probe cache, configure replay, and FetchContent integration. | `archive`, `base`, `build`, `cps`, `diag`, `eval`, `expr`, `fetch`, `genex`, `host`, `instrument`, `json`, `model`, `regex`, `resolve`, `store`, `syntax` |
| `rcmake-presets` | Presets schema version 13: loader, `include`, inheritance, conditions, and macro expansion | `base`, `diag`, `host`, `json`, `regex` |

### Resolution and build

| Crate | Responsibility | Depends on |
| --- | --- | --- |
| `rcmake-resolve` | Per-configuration target resolution, transitive usage requirements, link lines, IDE facts, and lowering to `BuildGraph`. Includes CUDA, Fortran, Qt `AUTOGEN`, precompiled header, unity build, and install runtime path rules. | `base`, `cxxmodules`, `diag`, `fortran`, `genex`, `host`, `json`, `model` |
| `rcmake-build` | In-process executor for a `BuildGraph`. Runs `try_compile()` and the `cmake --build` fallback without a generator. | `base`, `cxxmodules`, `diag`, `host`, `resolve` |
| `rcmake-cxxmodules` | C++20 named modules: P1689r5 dependency files, `CXXDependInfo.json`, `CXXModules.json`, Ninja `dyndep` files, and compiler module maps | `base`, `json` |
| `rcmake-fortran` | Fortran source scanning and module dependency collation, with no I/O | `base` |

### Generators

| Crate | Responsibility | Depends on |
| --- | --- | --- |
| `rcmake-gen-common` | Outputs for all generators: `compile_commands.json`, `cmake_install.cmake`, `CTestTestfile.cmake`, `CMakeConfigureLog.yaml`, exports, File API replies, and link response files | `base`, `cps`, `diag`, `genex`, `host`, `json`, `model`, `resolve` |
| `rcmake-gen-ninja` | `Ninja` and `Ninja Multi-Config`: response files, dependency files, and module `dyndep` rules | `base`, `cxxmodules`, `diag`, `gen-common`, `genex`, `host`, `model`, `resolve` |
| `rcmake-gen-make` | `Unix Makefiles`, with dependency scanning and grouped parallel output | `base`, `diag`, `gen-common`, `genex`, `host`, `model`, `resolve` |
| `rcmake-gen-xcode` | `Xcode`: `project.pbxproj`, schemes, workspace settings, and the `xcodebuild` command plan (ADR-014) | `base`, `diag`, `gen-common`, `host`, `model`, `resolve` |
| `rcmake-gen-vs` | `Visual Studio 17 2022` (`.sln`) and `Visual Studio 18 2026` (`.slnx`): `.vcxproj`, `.vcxproj.filters`, stable GUIDs, and the MSBuild command plan (ADR-015) | `base`, `diag`, `gen-common`, `host`, `model`, `resolve` |

### Test and package

| Crate | Responsibility | Depends on |
| --- | --- | --- |
| `rcmake-ctest` | `ctest`: `CTestTestfile.cmake` evaluation, parallel scheduler, fixtures, resource specifications, `discover_tests()`, JUnit and dashboard XML, and `-S` scripts | `base`, `commands`, `diag`, `eval`, `expr`, `host`, `instrument`, `json`, `presets`, `regex` |
| `rcmake-cpack` | `cpack`: `CPackConfig.cmake` evaluation, install staging, archive generators, and the macOS `DragNDrop` and `Bundle` generators (ADR-010) | `archive`, `base`, `commands`, `diag`, `eval`, `host`, `json`, `presets` |
| `rcmake-cps` | Common Package Specification 0.15.0: data model, writer, and validating reader (ADR-013) | `base`, `diag`, `json` |
| `rcmake-instrument` | Instrumentation API v1 (data v1.2): queries, snippets, index files, hooks, and trace output (ADR-013) | `base`, `host`, `json` |

### Utilities and storage

| Crate | Responsibility | Depends on |
| --- | --- | --- |
| `rcmake-tool-e` | The `cmake -E` utilities, including `tar`, the hash commands, `cmake_ninja_dyndep`, and the Fortran and Qt tool drivers | `archive`, `base`, `cxxmodules`, `diag`, `fortran`, `host`, `json` |
| `rcmake-archive` | Archive engine for `tar`, gzip, xz, zstd, bzip2, zip, and 7-Zip, with path checks on extraction | `base`, `host` |
| `rcmake-store` | Content-addressed blob store, verified cache entries, input stamps, garbage collection, and remote recovery for the probe cache | `base`, `host` |
| `rcmake-fetch` | FetchContent archive cache, Git mirrors, and project lockfiles (ADR-021) | `base`, `host`, `store` |

### Front ends

| Crate | Responsibility | Depends on |
| --- | --- | --- |
| `rcmake-cli` | The `rcmake` binary. Dispatches `cmake`, `ctest`, and `cpack` by name. Owns `--build`, `--install`, `--workflow`, `--find-package`, SARIF output, profiles, the `explain` subcommand, and the panic hook. | `api`, `base`, `commands`, `cpack`, `ctest`, `diag`, `eval`, `gen-common`, `gen-make`, `gen-ninja`, `gen-vs`, `gen-xcode`, `genex`, `host`, `instrument`, `json`, `model`, `presets`, `resolve`, `syntax`, `tool-e` |
| `rcmake-api` | In-process configure (`run_configure`), workspace index, provenance, and `explain` queries. Builds for WebAssembly with `--no-default-features` (ADR-023, ADR-024). | `base`, `commands`, `diag`, `eval`, `genex`, `host`, `json`, `model`, `resolve`, `syntax` |
| `rcmake-lsp` | Language server for CMake files. Gives lexical results by default. The `eval` feature adds evaluated results through `rcmake-api`. The `stdio` feature builds the `rcmake-lsp` binary. | `api` (optional), `base`, `diag`, `host`, `json`, `syntax` |

### Verification

| Crate | Responsibility | Depends on |
| --- | --- | --- |
| `rcmake-harness` | Black-box differential harness. Runs the staged `rcmake` and the CMake 4.5.1 oracle side by side and compares the results. Links no engine crate. | none |
| `rcmake-fuzz` | Safe-Rust fuzz runner for 14 input surfaces. It is never linked into `rcmake` (ADR-016). | `archive`, `base`, `cps`, `diag`, `expr`, `fortran`, `genex`, `host`, `json`, `presets`, `regex`, `store`, `syntax` |

`rcmake-gen-xcode` is a normal dependency of `rcmake-cli` only on macOS.
`rcmake-gen-vs` is a normal dependency only on Windows. On other hosts, the
`xcode-cross` and `vs-cross` features of `rcmake-cli` add them for tests.

## The `Host` abstraction

The engine crates never call `std::fs`, `std::env`, `std::process`, or the
system clock directly. They use the `rcmake_host::Host` trait. `clippy.toml`
bans the direct calls. Only `rcmake-host`, the `rcmake-cli` entry point, and
`rcmake-harness` can use them.

| Implementation | Use |
| --- | --- |
| `NativeHost` | The real operating system. Uses `rustix` on Unix and job objects on Windows for process groups and timeouts. |
| `PureHost` | An in-memory file system with symlinks, virtual timestamps, scripted subprocesses, and an ordered output log. Unit tests and the WebAssembly build use it. |

These rules apply to all `Host` calls:

- `Host` is `Send + Sync`, so threads can share one `Arc<dyn Host>`.
- All paths are absolute. The engine never uses or changes the process
  working directory.
- `NativeHost` reads the process environment once. Changes go into an overlay
  that each child process receives. The engine never calls `std::env::set_var`.
- Error messages keep the raw operating system text, so diagnostics match
  CMake 4.5.1.
- `rcmake_host::conformance::run_all` runs the same behavior suite against
  `NativeHost` and `PureHost`.

Wrappers around `Host` add behavior without a change to the engine. Examples:
the capture host for `try_compile()` output, the hermetic host that checks
inputs, and the overlay host that the language server uses for unsaved files.

## CMake 4.5.1 behavior that the design must keep

1. **Policy baseline.** `cmake_minimum_required(VERSION <x>)` with `<x>`
   below 3.5 is an error, unless `CMAKE_POLICY_VERSION_MINIMUM` is 3.5 or
   later. Policies `CMP0000` to `CMP0065` always use NEW behavior.
2. **`if()` auto-dereference.** Unquoted arguments in some operator positions
   are variable names. Quoted strings are never dereferenced (`CMP0054` is
   always NEW). The truthy and falsy constants (`ON`, `YES`, `TRUE`, `Y`,
   nonzero numbers, `OFF`, `NO`, `FALSE`, `N`, `IGNORE`, `NOTFOUND`,
   `*-NOTFOUND`, and the empty string) match CMake 4.5.1 exactly.
3. **Scopes.** `macro()` replaces its parameters as text in the caller's
   scope. It creates no variable scope, and `return()` in a macro returns from
   the enclosing function or file. `function()` creates a scope.
   `set(... PARENT_SCOPE)` and `return(PROPAGATE ...)` change the caller's
   scope. `block()` creates variable and policy scopes.
4. **Regular expressions.** CMake uses its own dialect, with no non-greedy
   operators, its own anchor rules, and the `CMAKE_MATCH_<n>` and
   `CMAKE_MATCH_COUNT` side effects. `rcmake-regex` implements this dialect.
   Rcmake does not use the Rust `regex` crate.
5. **Generator expressions.** Configure keeps unevaluated generator
   expressions. The parser handles nested `$<...>`, semicolons, and the full
   operator set. Multi-config generators evaluate them once for each
   configuration.
6. **macOS defaults.** The compiler stays `/usr/bin/cc` and is not mapped into
   Xcode. Rcmake adds `-isysroot` only when `CMAKE_OSX_SYSROOT` is set.
7. **Lists.** A list is a string with `;` separators. `\;` is a literal
   semicolon, and square brackets change splitting. `rcmake-base::list`
   implements one splitting rule for all callers.
8. **Bundled modules.** `CMAKE_ROOT` is `share/cmake-4.5` next to the binary.
   `project()` and `enable_language()` run the bundled CMake 4.5.1 modules.
   Rcmake does not hard-code compiler flags or platform tables in Rust.

## Verification

### Differential testing against CMake 4.5.1

The official CMake 4.5.1 binary is the reference. `scripts/fetch-oracle.sh`
downloads it, and `env/env.sh` exports its paths in `RCMAKE_ORACLE_CMAKE`
and related variables. Only `rcmake-harness`, `tools/`, and the
differential tests read these variables. The `rcmake` binary never runs the
oracle. The licence scanner checks this rule.

Rcmake is checked in four layers:

| Layer | Scope | Pass criterion |
| --- | --- | --- |
| 1. CLI and script output | `cmake -P`, `cmake -E`, CLI flags, `configure_file`, diagnostics, SARIF, presets | Byte-for-byte stdout, stderr, generated files, and exit code after path normalization |
| 2. Upstream `Tests/RunCMake` | The CMake 4.5.1 conformance suite: scripts, policies, diagnostics, targets, generator expressions, presets, File API | Same pass or fail result and same diagnostic matches |
| 3. Structured output | File API replies, `compile_commands.json`, `CMakeCache.txt` | Normalized structural equality |
| 4. Real projects | Configure, build, install, and `ctest` of open-source C and C++ projects | No build errors, same installed files, same `ctest` results |

For IDE generators, Rcmake compares canonical forms, not raw bytes. CMake
4.5.1 Xcode output changes between two runs, and Visual Studio output needs
MSBuild to build. `tools/measure/pbxproj_canon.py` and
`tools/measure/vs_canon.py` make the canonical forms.

### Test tools

| Tool | Purpose |
| --- | --- |
| `rcmake-harness` | Layer 1. Runs `.case` files from `conformance/cases/` and compares with the oracle or with the baselines in `conformance/baselines/`. |
| `tools/measure/runcmake_eval.py` | Layer 2. Runs upstream `Tests/RunCMake` suites with the staged binary. |
| `tools/measure/independent_verify.py` | Black-box scenarios that build small C, C++, Objective-C++, Fortran, and presets projects with both tools. |
| `tools/measure/corpus_eval.py`, `tools/corpus/` | Layer 4. Real projects from `conformance/corpus/`, with PATH replacement and Ninja command comparison. |
| `tools/measure/bench_eval.py` | Median and p95 wall time and peak memory for Rcmake and the oracle. |
| `tools/measure/determinism.py` | Checks that two build directories get the same generated files. |
| `rcmake-fuzz` | Random and adversarial input with a time budget for each case. Each failure can be replayed from its seed. |

### Unit tests

Most unit tests use `PureHost`, so they run without the oracle and without
file system access. The shared host conformance suite runs on both hosts in
CI on Linux, macOS, and Windows.
