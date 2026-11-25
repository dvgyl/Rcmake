# Building rcmake, a clean-room CMake 4.5.1 replacement in safe Rust

`rcmake` is a clean-room implementation of CMake 4.5.1 written in safe Rust. One binary acts as `cmake`, `ctest`, and `cpack` based on `argv[0]`. A companion binary, `rcmake-lsp`, provides Language Server Protocol support for CMake files.

The goal is drop-in compatibility. A project should configure, build, install, test, and package with `rcmake` first on `PATH` and produce the same outputs as CMake 4.5.1.

This write-up covers the architecture, how a human owner and three coding agents divided the work over three calendar days, the differential oracle method, hard bugs that testing exposed, honest limitations, and one command to reproduce every claim locally.

All numbers that depend on the final frozen release binary stay inside machine-readable metric markers until one frozen-binary verification run stamps them. We never estimate unreleased numbers. Immutable architectural counts and verified pre-freeze soundness baselines are labeled explicitly.

## 1. Motivation, scope, and clean-room rules

### Why target one exact release

CMake has no formal language specification. Its observable behavior is the specification, defined by 227 policies (`CMP0000` to `CMP0226`), subtle `if()` auto-dereference rules, `CMakeCache.txt` serialization details, Ninja quoting rules, and the upstream `Tests/RunCMake` suite. Real build trees depend on those details. A partial "CMake-like" reimplementation breaks as soon as a project inspects `CMakeCache.txt`, parses `try_compile` output with a regular expression, or checks File API JSON replies.

Pinning one version, CMake 4.5.1, gives every design question a single test answer. When behavior is unclear, we run CMake 4.5.1 and `rcmake` side by side on a small probe and compare their outputs. A fixed target version keeps that answer stable.

The implementation covers:

- `cmake`: configure and generate, `-P` script mode, `-E` command-line utilities, `--build`, `--install`, `--workflow`, `--find-package`, presets (schema version 13), and SARIF 2.1.0 diagnostic output.
- `ctest`: parallel test scheduling, fixtures, resource specifications, labels, repeat modes, JUnit and dashboard XML output, `discover_tests()`, and `-S` scripts.
- `cpack`: archive generators (`TGZ`, `TXZ`, `TZST`, `TBZ2`, `TZ`, `TAR`, `ZIP`, `7Z`, and their compression variants), `External`, and macOS `DragNDrop` and `Bundle`.
- Interfaces: policies `CMP0000` to `CMP0226` (OLD behavior removed for `CMP0000` to `CMP0065` as in CMake 4.x), File API `codemodel` v2.12, `cache` v2, `toolchains` v1, `cmakeFiles` v1, `configureLog` v1, Common Package Specification (CPS) 0.15.0, SPDX 3.0.1 SBOM generation, Instrumentation API v1, `compile_commands.json`, and C++20 named modules with Ninja `dyndep`.
- Native extensions under the `rcmake` name: persistent compiler probe cache, incremental configure replay, `rcmake explain`, `--profile`, `--diagnostics=rich`, `--hermetic`, `--fetch-lock`, the in-process `rcmake-api` crate (including WebAssembly targets), and `rcmake-lsp`.

### Why `#![forbid(unsafe_code)]`

A build system parses untrusted inputs: third-party `CMakeLists.txt` scripts, cache files, presets, downloaded archives, binary headers, and compiler output. Memory corruption on any of those paths is a security vulnerability.

The workspace sets `unsafe_code = "forbid"` in [`Cargo.toml`](../Cargo.toml), and every crate root repeats `#![forbid(unsafe_code)]`. Workspace Clippy lints also flag `unwrap_used`, `expect_used`, `panic`, `todo`, `unimplemented`, `indexing_slicing`, `string_slice`, `iter_over_hash_type`, and disallowed `std` I/O methods. Reachable input paths return structured diagnostics instead of panicking, bounded by explicit limits in `rcmake_base::Limits` on recursion depth, macro expansion, loop iterations, regular expression steps, and file sizes.

Rust 2024 marks `std::env::set_var` as `unsafe`, yet CMake scripts mutate `ENV{...}` constantly during configure. Rather than allowing unsafe code, `NativeHost` snapshots the process environment once at startup, tracks script mutations in an in-memory overlay, and passes the merged map explicitly to each child process. The parent process environment never changes.

### Clean-room boundary and licensing

All Rust code in `crates/` and Python tooling in `tools/` is written from scratch under `MIT OR Apache-2.0`.

A large share of CMake's behavior lives in CMake-language standard library modules such as `FindThreads.cmake` and `CMakeDetermineCCompiler.cmake` under `Modules/` and `Templates/`. Projects include those files directly and rely on their internal helper macros and cache variables. Rewriting hundreds of platform modules would create a second, incompatible CMake. Because CMake is licensed under `BSD-3-Clause`, `rcmake` bundles the CMake 4.5.1 `Modules/`, `Templates/`, and rendered `Help/` files verbatim under `modules/cmake-4.5/`, keeps every copyright header intact, and records full attribution in [`THIRD_PARTY_NOTICES.md`](../THIRD_PARTY_NOTICES.md) and [`third_party/licences/CMake-BSD-3-Clause.txt`](../third_party/licences/CMake-BSD-3-Clause.txt).

Our clean-room boundary means we never copy or translate CMake C++ code into Rust, and `rcmake` never invokes an upstream `cmake` binary at runtime. Only `rcmake-harness` and the measurement scripts under `tools/` read `RCMAKE_ORACLE_CMAKE`. When a differential test uncovers an undocumented edge case, we inspect CMake's behavior (and cite the relevant upstream rule in commit messages when needed), design the Rust implementation from scratch, and verify it against the oracle binary. [`tools/measure/licence_scan.py`](../tools/measure/licence_scan.py) enforces allowed dependency licences, zero `unsafe` blocks, zero oracle environment references outside `rcmake-harness`, and clean repository hygiene on every check.

## 2. Architecture across 34 crates and one `Host` trait

At commit `3778695`, the workspace spans 34 crates, 897 Rust source files, 322,440 lines of safe Rust, and 31 Architecture Decision Records in [`docs/ADR.md`](ADR.md). The dependency graph is acyclic and mirrors CMake's execution stages.

```text
argv[0], presets          rcmake-cli, rcmake-presets
        |
        v
listfiles --> rcmake-syntax --> rcmake-expr --> rcmake-eval + rcmake-commands
        |                      (${}, if())      (scopes, policies, commands)
        v
rcmake-model               ProjectModel: cache, directories, targets, tests
        |
        v
rcmake-genex <--GenexHost--> rcmake-resolve
($<...>)                     ResolvedTarget, usage closure, BuildGraph
        |
        +--> rcmake-gen-ninja, rcmake-gen-make, rcmake-gen-xcode, rcmake-gen-vs
        +--> rcmake-gen-common (File API, install scripts, CTest files, CPS, SBOM)
        +--> rcmake-build (in-process try_compile), rcmake-ctest, rcmake-cpack
```

[`docs/ARCHITECTURE.md`](ARCHITECTURE.md) documents every crate. Five design choices did the most work:

1. **Byte strings end to end.** CMake variables, paths, and listfiles are raw byte sequences rather than guaranteed UTF-8. `rcmake-base` defines `BString`, CMake list splitting (`;` with `\;` escapes and bracket awareness), CMake truthiness (`is_on` and `is_off`), lexical path helpers, and `Limits`. `rcmake-syntax` preserves raw bytes, argument kinds (quoted, unquoted, bracket), and spans.
2. **A clean-room CMake regex engine.** CMake uses the KWSys regular expression dialect, which has no non-greedy quantifiers, distinct anchor rules, and `CMAKE_MATCH_<n>` side effects. `rcmake-regex` implements that dialect in pure Rust with an explicit step budget instead of wrapping the Rust `regex` crate.
3. **Three resolution layers and an acyclic generator-expression seam.** Configure produces an arena-based `ProjectModel` (`rcmake-model`) where values carry backtraces. `rcmake-resolve` evaluates generator expressions per configuration into typed `ResolvedTarget` records (read by File API and export writers) and lowers those targets into a `BuildGraph` of compile, link, and custom-command steps. Because generator expressions such as `$<TARGET_PROPERTY:...>` query transitive usage requirements during resolution, `rcmake-genex` defines a `GenexHost` trait and `rcmake-resolve` implements it. This keeps the crate graph acyclic.
4. **Generators only format output.** Ninja, Ninja Multi-Config, Unix Makefiles, Xcode, and Visual Studio 17 and 18 all consume the same `BuildGraph`. Compile flags, link lines, precompiled headers, unity sources, runtime paths, and install rules are resolved once in `rcmake-resolve`, so a single resolution fix repairs every generator at once.
5. **All operating-system access behind `rcmake_host::Host`.** `clippy.toml` forbids engine crates from calling `std::fs`, `std::env`, `std::process`, or the system clock directly. Every file read, write, directory listing, symlink query, file lock, environment lookup, process spawn, stream write, and timestamp goes through `Host`.

### How the `Host` trait paid off, including `rcmake.wasm`

`rcmake-host` ships two implementations checked against the exact same conformance suite (`rcmake_host::conformance::run_all`):

- `NativeHost` talks to the real operating system using `rustix` on Unix and job objects on Windows for process groups and timeouts.
- `PureHost` provides an in-memory file system with symlink resolution, virtual timestamps, scripted subprocesses, and an ordered stdout and stderr log.

Putting every system boundary behind one trait made six higher-level features small wrappers instead of invasive rewrites:

- **Deterministic unit tests.** Most engine tests configure projects inside `PureHost` with no temporary directories, no host compiler, and stable error text (`HostError` keeps the raw OS message without Rust's `(os error N)` suffix).
- **Hermetic configure (`--hermetic=warn|strict`).** A host wrapper audits environment reads, file reads, subprocess spawns, and network access at the single boundary the engine can reach.
- **Persistent compiler probe cache.** `JournalHost` wraps `Host` during `try_compile` probes, records every file read, search directory, environment key, and child process, and stores verified stamps in `rcmake-store`.
- **Incremental configure replay.** The same input-recording mechanism validates unchanged directory regions before replaying them from a previous configure snapshot.
- **Editor buffer isolation.** `rcmake-lsp` evaluates unsaved buffers through a read overlay, routes writes to a scratch overlay, and denies subprocesses.
- **Real WebAssembly compilation (`rcmake.wasm`).** Because `rcmake-api` with `--no-default-features` drops native archive and disk-store crates (`rcmake-store`, `rcmake-archive`, `zstd`, `liblzma`) and runs on `PureHost`, the real configure engine compiles cleanly to both `wasm32-unknown-unknown` and `wasm32-wasip1`. [`tools/measure/wasm_smoke.py`](../tools/measure/wasm_smoke.py) audits the Cargo dependency tree for zero native C crates, compiles for `wasm32-unknown-unknown`, builds `wasm_smoke.wasm` for `wasm32-wasip1`, and executes it in Wasmtime to verify three configured targets, provenance queries, cancellation, and both permitted and denied `try_compile` calls. The browser playground at [`https://dvgyl.github.io/rcmake/playground.html`](https://dvgyl.github.io/rcmake/playground.html) exposes the `wasm-adapter.js` contract for loading the compiled `rcmake.wasm` module directly in the browser with no server. When the binary bundle is absent, the page falls back to precomputed previews.

## 3. Timeline and multi-agent division of work

One human maintainer set the goals, clean-room rules, and review gates, and coordinated coding agents across 338 commits on `HEAD` in three calendar days:

- **2026-11-23 (`160c701` to `3e2cc7d` and beyond):** initial specification, workspace bootstrap, `Host` and `rcmake-syntax` contracts, oracle tooling, `rcmake-regex`, `rcmake-expr`, `rcmake-eval`, presets v13, and `-P` script parity.
- **2026-11-24:** full configure and resolution pipeline, Ninja, Unix Makefiles, Xcode, Visual Studio, File API, `ctest`, `cpack`, CPS, SBOM, C++20 modules, Fortran, CUDA, Qt `AUTOGEN`, persistent probe cache, incremental replay, FetchContent lockfiles, `rcmake-api`, `rcmake-lsp`, and safe-Rust fuzzing.
- **2026-11-25 (`3778695`):** large-project corpus parity, 20 independent black-box scenarios, cross-platform Windows and Linux CI recovery, diagnostic formatting and File API v2.12 alignment, WebAssembly verification, brand kit, static site, and public documentation polish.

### How work was divided across models

We assigned distinct roles across models:

- **Gemini 4 Neon** acted as coordinator and systems planner: owning shared contracts (`Cargo.toml`, `rcmake-base`, `rcmake-syntax`, `rcmake-diag`, `rcmake-model` public types, and ADRs), triaging differential failures into disjoint crate slices, auditing diffs against the architecture, and authoring documentation and frontend tooling.
- **Opus 5.5** handled deep architectural planning, tricky semantic subsystems, and high-stakes code reviews on large resolution and caching changes.
- **GPT 6.2 Sol** executed the majority of parallel crate implementations, parity sweeps across CMake commands and generators, measurement scripts, and routine diff reviews.
- **Deep-reasoning technical consults** were brought in on hard soundness and specification design problems, including probe-cache invalidation invariants, incremental replay boundaries, and cross-platform runtime path semantics.

### Engineering habits that mattered

- **Disjoint worktrees.** Parallel implementation batches touched non-overlapping crates or command families inside separate git worktrees. That separation prevented merge conflicts while 3 to 5 agents worked in parallel.
- **Cross-model review.** No agent reviewed its own code. Opus 5.5 reviewed large GPT 6.2 Sol architectural changes, GPT 6.2 Sol reviewed Opus 5.5 slices, and every patch faced automated oracle verification before landing.
- **Resource limits on a 16 GB laptop.** Every local Cargo run shared at most two build jobs (`CARGO_BUILD_JOBS=2`, `RUST_TEST_THREADS=3` in `env/env.sh`). Read-only triage, specification checks, code reviews, Python verification scripts, and documentation ran in parallel without contending for a Cargo build.
- **Probes over debate.** Agents (like humans) write confident, plausible explanations for wrong hypotheses. Every disagreement about CMake semantics became a small script executed against CMake 4.5.1. For example, one early proposal suggested relaxing our export-file comparison to accept quoted `add_library("Demo::...")` names. Running the oracle proved CMake never quotes those target names, so the bug was in `rcmake`'s export writer and the test stayed strict.
- **Treating test scripts as code under test.** Our large-project CI job originally truncated per-file diffs at 500 lines. On `llvm-project` and `vtk`, the first 500 lines contained only oracle entries, hiding every `rcmake` row. Commit `f28b50e` raised the cap to 2,000 lines and added per-field summaries with counts and samples from both sides.

## 4. The differential oracle method

No single test suite catches every class of incompatibility. We stacked seven verification layers against the pinned CMake 4.5.1 oracle binary:

| Layer | Suite | Scope | What it catches |
| --- | --- | --- | --- |
| 1 | Unit and integration tests | `cargo test --workspace` on `PureHost` and `NativeHost` | Logic bugs, panics, host-contract drift |
| 2 | Recorded CLI and script cases | `rcmake-harness` over `conformance/cases/` | Byte-exact `-P`, `-E`, CLI, presets, SARIF, and `configure_file` parity |
| 3 | Upstream `Tests/RunCMake` | 371 macOS arm64, 362 Linux, 367 Windows suites (from 463 upstream entries) | Diagnostic text, policy transitions, generator corner cases |
| 4 | Independent black-box suite | 20 scenarios, 679 checks (`independent_verify.py`) | End-to-end build, install, RPATH, `tar`, `ctest`, `cpack`, and File API parity |
| 5 | Large C++ projects | 10 unmodified open-source giants (`large_projects.toml`) | Real module scripts, configure stderr (L1), Ninja build-graph identity (L2) |
| 6 | Fuzzing and ecosystem lanes | 14 fuzz targets at 1,000,000 inputs each, plus 64-shard `vcpkg`, Conan, and Qt 6 | Panics, resource exhaustion, broad package-manager compatibility |
| 7 | Sealed unseen holdout | 240 repositories never opened during development | Overfitting to known test suites |

### Why independent black-box tests were essential

CMake's upstream `Tests/RunCMake` suite tests regressions that CMake developers expect to break in CMake. It rarely tests things CMake itself never got wrong, such as whether an installed macOS binary actually runs with `@rpath` load commands or whether `cpack` archive listings match byte structure across generators.

We wrote 20 independent black-box scenarios (`tools/measure/independent_verify.py` and `tools/measure/independent_scenarios_extra.py`) covering 679 checks across C, C++, Objective-C++, Fortran, presets, multi-generator builds, install trees, Mach-O and ELF runtime paths, archive listings, and File API replies. A scenario passes only when all explicit checks pass for both binaries and all normalized observations match. Running the suite with `--oracle-only` first proves every assertion holds on CMake 4.5.1 itself.

### Large-project corpus and sealed unseen holdout

The 10 large projects pinned in [`conformance/corpus/large_projects.toml`](../conformance/corpus/large_projects.toml) are `duckdb` v1.3.2, `opencv` 4.12.0, `vtk` v9.5.0, `qtbase` v6.8.3, `llvm-project` 20.1.8, `pytorch` v2.7.1, `kcoreaddons` v6.15.0, `boost` 1.88.0, `rclcpp` 28.1.8, and `blender` v4.5.0. L1 checks configure success and stderr parity. L2 compares `CMakeCache.txt`, normalized Ninja dry-run commands, Ninja graph edges, and File API `codemodel` replies.

Separately, a 240-repository unseen holdout manifest lives in the repository and is strictly off-limits to every agent during development (`licence_scan.py` even skips reading it). It runs once against the frozen release binary to measure genuine generalization without overfitting.

### Documented normalizers only

Differential comparison needs narrow normalizers for machine paths, timestamps, and intentional differences, and every normalizer risks hiding a bug. Each normalizer in `tools/corpus/normalize.py` and `tools/measure/independent_scenarios_extra.py` carries a written rationale checked against CMake 4.5.1 (see commits `f28b50e` and `fb13e49`):

- `tar tvf` owner and group columns are masked because `rcmake` intentionally writes uid 0 and gid 0 for reproducible archives.
- `OPENCV_TIMESTAMP` holds wall-clock configure time, so only its value is masked.
- File API `backtraces` index arrays are dropped where `backtraceGraph` is compared as resolved frame chains, and `index-*.json` objects pass through the same documented canonicalizer as the reply files (`fb13e49`).
- `cmake -E capabilities` drops the absent `debugger` member and empty `extraGenerators` lists while keeping `version`, `fileApi`, `instrumentation`, `serverMode`, and `tls` strict.
- `Boost.Locale` is excluded on both engines in `large_projects.toml` because Boost 1.88 fails `source_group(TREE)` prefix validation on CMake 4.5.1 as well.

## 5. Hard bugs differential testing exposed

Every bug below comes from verified commits on `HEAD`.

### 5.1 Listfile replay deduplication (`200ed8c`)

`Interp::record_list_file` in `crates/rcmake-eval/src/interp/listfile.rs` and `configure_file` in `crates/rcmake-commands/src/policy_misc/files.rs` both checked whether a path was already present in `dir.listfiles` before pushing it. That looked tidy, and it silently broke parity whenever a project included the same `.cmake` file or configured the same template more than once inside a directory. CMake records every read in order, and File API `cmakeFiles` replies expose that exact sequence. Removing the deduplication check in `200ed8c` preserved repeated listfile reads and matched CMake's recorded listfile stream.

### 5.2 CMake diagnostic paragraph spacing (`ef841e3`, `200ed8c`)

CMake formats error and warning bodies through `cmDocumentationFormatter`, which wraps paragraphs, inserts blank separator lines between them, and treats indented lines as preformatted blocks. Earlier in development, emitters such as generator-expression errors and missing-link-target diagnostics manually inserted double newlines (`\n\n`). When tests failed on extra blank lines, `crates/rcmake-diag/src/render.rs` accumulated prefix-matching special cases (`format_with_added_separators`) that stripped `\n\n` back out for recognized strings.

Commits `ef841e3` and `200ed8c` fixed the root cause: every diagnostic producer emits the single-newline structure `cmDocumentationFormatter` expects (for example `validate_link_items` in `crates/rcmake-resolve/src/pipeline.rs`), and `format_diagnostic_body` collapsed to a single call to `format_paragraphs`. Deleting 25 lines of special-case code fixed both known cases and future diagnostics.

### 5.3 Darwin RPATH resolution and cross-platform path parsing (`cfacf0b`, `186d525`)

Two bugs lived in the same function, `rpath_entries` in `crates/rcmake-resolve/src/lower/link/rpath.rs`:

1. **Missing build-tree `@rpath` entries (`cfacf0b`).** Black-box scenarios s01, s15, and s18 install a shared library and link a consumer against it (including transitively through a static library). On macOS, the built consumer failed at runtime with `Library not loaded: @rpath/libdemo_shared.1.dylib ... Reason: no LC_RPATH's found` across all four generators. `rcmake` had filtered build RPATH entries to libraries inside the build tree or a sysroot. CMake's actual rule (`cmComputeLinkInformation::AddLibraryRuntimeInfo`) skips only implicit link directories and, on Apple platforms, checks whether the library install name uses `@rpath` (from `IMPORTED_SONAME` or by inspecting the Mach-O header in `rpath_macho.rs`). Because Ninja, Ninja Multi-Config, Unix Makefiles, and Xcode all call `rpath_entries`, fixing that one function repaired all four generators.
2. **POSIX-rooted Darwin paths parsed on Windows (`186d525`).** Immediately afterward, Windows CI failed unit tests that resolve Darwin RPATHs for `/opt/.../libfoo.dylib`. `Lexical::absolute()` uses host path rules, so on Windows a leading `/` without a drive letter was not recognized as absolute and got dropped from the runtime path set. Accepting `path.starts_with(b"/") || file.absolute()` (and the matching check for `libraries.user_dirs`) restored cross-platform Darwin resolution on Windows hosts.

### 5.4 File API `codemodel` v2.12 parity (`5b51b47`, `fb13e49`)

IDE tooling relies heavily on CMake File API `codemodel` v2.12 and `cmakeFiles` v1 replies. Early replies matched toy projects but drifted on large C++ targets with header file sets, generated sources, utility targets, and mixed compile languages. Commit `5b51b47` split target reply generation into dedicated modules (`crates/rcmake-gen-common/src/fileapi/codemodel/{sources,dependencies,file_sets,targets}.rs`) to match CMake's grouping of compile flags, file-set membership and provenance traces, direct target dependency entries, and `cmakeFiles` rules (where `isCMake` is true only for paths under `<CMAKE_ROOT>/Modules`, which stay absolute while project files stay relative to the source directory). Commit `fb13e49` closed the remaining measurement gap by running File API `index-*.json` objects through the same documented canonicalizer as target replies.

### 5.5 Probe-cache invalidation soundness (`b9bded2`, `3d6a2a4`)

Compiler probes (`try_compile`, `check_c_source_compiles`, `check_symbol_exists`) take most of the wall time in a cold configure. Reusing probe outcomes across build directories and projects delivers major configure speedups, but a single stale cache hit corrupts a build silently.

Commits `b9bded2` and `3d6a2a4` hardened the cache boundary across `rcmake-commands` and `rcmake-store`. Every probe executes under `JournalHost` (`crates/rcmake-commands/src/configure/try_compile/journal.rs`), which intercepts all file reads, compiler search paths, depfiles, environment reads, and child processes. If a probe touches anything that cannot be re-verified safely, the journal marks an `ImpureReason` (`ExternalWrite`, `SharedState`, `NondeterministicInput`, `UnverifiedProcess`, `ChangedInput`, or `JournalLimit`) and refuses to publish the entry. On lookup, `rcmake-store` validates file and directory-tree input stamps (`crates/rcmake-store/src/stamp/tree.rs`) before accepting a hit. [`tools/measure/probe_cache_eval.py`](../tools/measure/probe_cache_eval.py) tests invalidation against 12 distinct triggers (`CPATH`, `__has_include`, header shadowing, `LANG`, compiler wrapper swaps, transitive includes, `DEVELOPER_DIR`, archive replacement, and others) plus crash-interruption recovery during staged publication.

### 5.6 Large-project and archive parity edge cases (`c4e32bb`, `8f5188f`, `2043177`)

Running unmodified large C++ projects exposed rules that documentation never spells out:

- **`qtbase` `try_compile` header (`c4e32bb`).** `QtSeparateDebugInfo.cmake` extracts a binary path from `try_compile` `OUTPUT_VARIABLE` using `string(REGEX REPLACE ".+###(.+)###.+" "\\1" ...)`. Because `rcmake` ran `try_compile` in-process and returned only raw tool output (`###<path>###\n`), nothing preceded `###` and the `.+` failed to match. CMake prepends `Change Dir: '<bindir>'` and `Run Build Command(s): ...` before the tool output. Emitting the same header fixed Qt and any other project parsing `OUTPUT_VARIABLE`.
- **ROS 2 `rclcpp` re-inclusion guard (`c4e32bb`).** `ament_cmake_coreConfig.cmake` checks `if(_ament_cmake_core_CONFIG_INCLUDED)` and sets `<Pkg>_FOUND` to `FALSE` if `<Pkg>_FOUND` is undefined. `rcmake` unconditionally unset `<Pkg>_FOUND` before reading a package config file, breaking every second `find_package()`. CMake unsets `<Pkg>_FOUND` only when it is already defined and evaluates to false.
- **PyTorch ONNX export-set creation (`c4e32bb`).** `third_party/onnx/CMakeLists.txt` calls `install(EXPORT ONNXTargets ...)` before any `install(TARGETS ... EXPORT ONNXTargets)`. CMake's `GetExportSets()[name]` uses C++ `std::map::operator[]`, which silently inserts an empty export set on first lookup, whereas `export(EXPORT)` stays strict. Creating export sets on `install(EXPORT)`, `EXPORT_ANDROID_MK`, `PACKAGE_INFO`, and `SBOM` while keeping `export(EXPORT)` strict matched CMake.
- **Bounded ZIP scanning and LZW `.Z` clear codes (`8f5188f`).** The old ZIP end-of-central-directory reader scanned backward across an entire archive in 64 KiB chunks, giving worst-case quadratic work on truncated files with false `PK\x05\x06` signatures. Because the end record is 22 bytes with at most a 65,535-byte comment, the fix reads the last 65,557 bytes once. In the same commit, the Unix `compress` (`.Z`) decoder fixed a bug comparing the fixed 16-byte array length `group.len()` instead of the active byte count `len`, which had rejected valid 12-bit clear codes.
- **Cross-compiling `try_run` and CPS alignment (`2043177`).** `try_compile` now resolves imported targets using `CMAKE_TRY_COMPILE_CONFIGURATION` rather than `CMAKE_BUILD_TYPE` passed in `CMAKE_FLAGS`, `file(INSTALL)` prints `Installing:` for directory symlinks, and CPS definitions preserve alphabetical ordering and case-insensitive language mapping.

## 6. Scorecard and honest limitations

### Frozen release scorecard

Every metric in the "Frozen release binary" column is stamped mechanically from one final release-binary verification run.

| Area | Scope | Verified pre-freeze baseline | Frozen release binary |
| --- | --- | --- | --- |
| Release commit | Git SHA and date | `HEAD = 3778695` (`2026-11-25`) | <!-- METRIC:release_commit -->pending frozen release run<!-- /METRIC --> (<!-- METRIC:release_date -->pending frozen release run<!-- /METRIC -->) |
| 3-OS CI + corpus smoke | `ubuntu-24.04`, `macos-15`, `windows-2025` | 6 of 6 jobs green | <!-- METRIC:ci_jobs -->pending frozen release run<!-- /METRIC --> |
| Upstream `Tests/RunCMake`, macOS arm64 | 371 OS-applicable suites (`all-macos`) | Pre-freeze run in progress | <!-- METRIC:runcmake_macos -->pending frozen release run<!-- /METRIC --> |
| Upstream `Tests/RunCMake`, Linux | 362 OS-applicable suites (`linux`) | Pre-freeze run in progress | <!-- METRIC:runcmake_linux -->pending frozen release run<!-- /METRIC --> |
| Upstream `Tests/RunCMake`, Windows | 367 OS-applicable suites (`windows`) | Pre-freeze run in progress | <!-- METRIC:runcmake_windows -->pending frozen release run<!-- /METRIC --> |
| Upstream Core-62 conformance | 62 core suites (`core`) | Pre-freeze run in progress | <!-- METRIC:runcmake_core62 -->pending frozen release run<!-- /METRIC --> |
| Upstream policies | 56 policy suites (`CMP0066` to `CMP0226`) | Pre-freeze run in progress | <!-- METRIC:runcmake_policies -->pending frozen release run<!-- /METRIC --> |
| Independent black-box suite | 20 scenarios, 679 checks | 20 scenarios, 679 checks implemented | <!-- METRIC:independent_scenarios -->pending frozen release run<!-- /METRIC --> |
| Large C++ projects (L1) | 10 projects, configure stderr parity | Unmodified pinned commits | <!-- METRIC:large_projects_l1 -->pending frozen release run<!-- /METRIC --> |
| Large C++ projects (L2) | 10 projects, Ninja graph and File API parity | Unmodified pinned commits | <!-- METRIC:large_projects_l2 -->pending frozen release run<!-- /METRIC --> |
| Sealed unseen holdout | 240 repositories, Layer 2 dry-run parity | Sealed (never opened during dev) | <!-- METRIC:unseen_holdout -->pending frozen release run<!-- /METRIC --> |
| `vcpkg` ecosystem matrix | 64 shards on Linux | Workflow ready | <!-- METRIC:vcpkg_matrix -->pending frozen release run<!-- /METRIC --> |
| Conan and ecosystem fixtures | `ecosystem_eval.py` | Workflow ready | <!-- METRIC:conan_ecosystem -->pending frozen release run<!-- /METRIC --> |
| Silent `PATH` swap | `path_swap_eval.py` | Smoke green on CI | <!-- METRIC:path_swap_eval -->pending frozen release run<!-- /METRIC --> |
| Probe-cache soundness | 12 invalidation triggers + crash recovery | Smoke green on CI | <!-- METRIC:probe_cache_soundness -->pending frozen release run<!-- /METRIC --> |
| Mutation replay soundness | Seeded leaf-edit replay equivalence | Smoke green on CI | <!-- METRIC:mutation_replay_soundness -->pending frozen release run<!-- /METRIC --> |
| Adversarial fuzzing | 14 targets, 1,000,000 inputs each | 14,000,000 inputs, 0 failures | <!-- METRIC:fuzz_failures -->pending frozen release run<!-- /METRIC --> |
| WebAssembly and WASI checks | `wasm32-unknown-unknown` and `wasm32-wasip1` | 6 of 6 checks passed | <!-- METRIC:wasm_smoke -->pending frozen release run<!-- /METRIC --> |
| Cold 500-target configure | Speedup over CMake 4.5.1 | Pending idle release run | <!-- METRIC:bench_cold_configure -->pending frozen release run<!-- /METRIC --> |
| Warm probe-cache configure | Median speedup over CMake 4.5.1 | Pending idle release run | <!-- METRIC:bench_warm_probe -->pending frozen release run<!-- /METRIC --> |
| Incremental leaf reconfigure | p95 wall time | Pending idle release run | <!-- METRIC:bench_incremental_leaf_p95 -->pending frozen release run<!-- /METRIC --> |
| Peak memory (500 targets) | Resident set size (MiB) | Pending idle release run | <!-- METRIC:bench_peak_rss -->pending frozen release run<!-- /METRIC --> |
| Release binary size | `rcmake` on macOS arm64 | Pending frozen release build | <!-- METRIC:binary_size_macos -->pending frozen release run<!-- /METRIC --> |

Final known parity gaps from the frozen release run: <!-- METRIC:known_parity_gaps -->pending frozen release run<!-- /METRIC -->.

### What `rcmake` does not support

These items are deliberate scope boundaries or unverified areas. Use CMake 4.5.1 when a build requires any of them:

- **Interactive front ends.** There is no `cmake-gui` desktop application and no `ccmake` terminal interface. The `edit_cache` target prints `No interactive CMake dialog available.`, matching CMake when `ccmake` is absent.
- **DAP debugger.** `cmake --debugger` fails with `CMake was not built with support for --debugger`, matching a CMake build compiled without debugger support. `cmake -E capabilities` omits the `debugger` key.
- **Swift builds are unverified.** Basic Swift target properties and compile-rule exclusions exist in `rcmake-resolve`, but real Swift toolchain projects are not exercised in CI or in the differential suites. Treat Swift support as unverified until a differential Swift suite lands.
- **CPack native installers.** `rcmake` supports the archive generators (`TGZ`, `TXZ`, `TZST`, `TBZ2`, `TZ`, `TAR`, `ZIP`, `7Z`, and their compression variants), `External`, and macOS `DragNDrop` and `Bundle`. It does not implement `DEB`, `RPM`, `NSIS`, `WIX`, `productbuild`, or `IFW`.
- **Legacy and niche generators.** Supported generators are `Ninja`, `Ninja Multi-Config`, `Unix Makefiles`, `Xcode`, `Visual Studio 17 2022`, and `Visual Studio 18 2026`. Legacy extra generators (`CodeBlocks`, `CodeLite`, `Eclipse CDT4`, `Kate`, `Sublime Text 2`) and older or platform-niche generators (`NMake Makefiles`, `NMake Makefiles JOM`, `MSYS Makefiles`, `MinGW Makefiles`, `Borland Makefiles`, `Watcom WMake`, `Green Hills MULTI`, `FASTBuild`, and Visual Studio 14, 15, and 16) are omitted.
- **Reproducible `tar` ownership.** `cmake -E tar` and CPack archive generators intentionally write uid 0 and gid 0 instead of copying host user IDs.

## 7. Reproduce every claim in one command

Prepare the pinned CMake 4.5.1 oracle and a staged `rcmake` installation once from the repository root:

```sh
source env/env.sh
scripts/fetch-oracle.sh
eval "$(scripts/fetch-oracle.sh --print-env)"
RCMAKE_STAGE_PROFILE=release scripts/stage.sh
```

Then run any verification check directly:

| Claim | Single command |
| --- | --- |
| Clean-room licences, zero `unsafe`, and Git hygiene | `python3 tools/measure/licence_scan.py --check-hygiene` |
| Rust workspace size (crates, files, lines) | `python3 -c 'import pathlib; f=list(pathlib.Path("crates").rglob("*.rs")); print(len(list(pathlib.Path("crates").iterdir())), "crates,", len(f), "files,", sum(len(p.read_text(errors="ignore").splitlines()) for p in f), "lines")'` |
| 3-OS `Tests/RunCMake` (replace `all-macos` with `linux` or `windows`) | `python3 tools/measure/runcmake_eval.py --profile all-macos --all --json runcmake.json` |
| Independent 20-scenario black-box suite (679 checks) | `python3 tools/measure/independent_verify.py --stage target/stage --json independent.json` |
| 10 large C++ projects (5 local macOS projects, full 10 in `ecosystem.yml`) | `python3 tools/corpus/large_projects_local.py --project all --rcmake-stage target/stage --oracle "$RCMAKE_ORACLE_CMAKE" --mode parity --run-dir /tmp/rcmake-large` |
| Sealed 240-repository unseen holdout (Layer 2) | `python3 tools/corpus/unseen_eval.py --split holdout --stage target/stage --oracle "$RCMAKE_ORACLE_CMAKE" --level L2 --out-json unseen.json --out-md unseen.md` |
| Conan and ecosystem fixtures (`gh workflow run ecosystem.yml -f lane=vcpkg` for 64-shard `vcpkg`) | `python3 tools/corpus/ecosystem_eval.py --stage target/stage --oracle "$RCMAKE_ORACLE_CMAKE" --out-json ecosystem.json --out-md ecosystem.md` |
| Silent `PATH` swap (`cmake`, `ctest`, `cpack` on `PATH`) | `python3 tools/measure/path_swap_eval.py --rcmake-bin target/stage/bin/rcmake --out path-swap.json` |
| Cold configure latency and peak RSS compared to CMake 4.5.1 | `python3 tools/measure/bench_eval.py --include-corpus --json bench.json --markdown bench.md` |
| Warm probe-cache and incremental leaf-edit benchmarks | `python3 tools/measure/bench_configure.py --rcmake target/stage/bin/rcmake --oracle "$RCMAKE_ORACLE_CMAKE" --scenario all --json bench-configure.json --markdown bench-configure.md` |
| Probe-cache invalidation and crash-recovery soundness | `python3 tools/measure/probe_cache_eval.py --rcmake target/stage/bin/rcmake --out-json probe.json --out-md probe.md` |
| Incremental leaf-edit replay soundness | `python3 tools/measure/mutation_eval.py --fixture --rcmake target/stage/bin/rcmake --out-json mutation.json --out-md mutation.md` |
| Target and variable provenance (`rcmake explain`) | `target/stage/bin/rcmake explain app -B build --json` |
| Configure flame graph and trace (`--profile`) | `target/stage/bin/rcmake -S . -B build -G Ninja --profile=profile-out` |
| Hermetic input auditing (`--hermetic`) | `target/stage/bin/rcmake -S . -B build -G Ninja --hermetic=strict` |
| Build the `rcmake-lsp` language server | `cargo build --locked --release -p rcmake-lsp --features stdio` |
| WebAssembly dependency audit and WASI `PureHost` smoke (`rcmake.wasm`) | `python3 tools/measure/wasm_smoke.py --require-wasi --json wasm-smoke.json` |
| Safe-Rust adversarial fuzzing (all 14 targets, 1,000,000 inputs each) | `for s in syntax expr regex genex json presets cps archive path tolerant binfmt diagnostics fortran store-frame; do cargo run --release --bin rcmake-fuzz -- "$s" --iterations 1000000; done` |
| Upstream CMake release and policy drift tracker | `python3 tools/measure/upstream_track.py --out-json upstream.json --out-md upstream.md` |

## 8. Lessons that held up

- **Pin one oracle and settle every question with a probe.** Fixing CMake 4.5.1 turned vague specification arguments into five-line test scripts with binary answers.
- **Hide all I/O behind one trait on day one.** Hermetic auditing, probe-cache journaling, incremental replay, LSP unsaved-buffer overlays, deterministic tests, and WebAssembly all grew out of `rcmake_host::Host` without altering the evaluator core.
- **Keep generators dumb.** Putting compile flags, link closures, and runtime paths in `rcmake-resolve` meant fixing a bug once for Ninja, Makefiles, Xcode, and Visual Studio together.
- **Write independent black-box tests alongside upstream suites.** Upstream tests guard what CMake authors expect to regress. End-to-end black-box checks catch what a brand-new implementation misses.
- **Delete special cases when you find the real rule.** Removing `format_with_added_separators` in `rcmake-diag` and listfile deduplication in `rcmake-eval` shrank the codebase while raising parity.
- **Audit normalizers and test scripts as strictly as production code.** A truncated diff or an overly broad normalizer makes a broken build look green.
