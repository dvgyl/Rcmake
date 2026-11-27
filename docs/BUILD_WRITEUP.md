# Building rcmake, an independent CMake 4.5.1 reimplementation in Rust

`rcmake` is an independent reimplementation of CMake 4.5.1 in safe Rust. It contains no copied or translated CMake C++ source code, and the core engine never calls an upstream `cmake` binary at runtime. One multi-call binary provides `cmake`, `ccmake` on Unix, `ctest`, `cpack`, and `rcmake-lsp`, while an isolated sibling binary (`rcmake-verify`) runs explicit side-by-side parity checks when you run `rcmake verify`.

## Contents

- [Motivation, scope, and implementation rules](#motivation-scope-and-implementation-rules)
- [Architecture across 39 crates and one Host trait](#architecture-across-39-crates-and-one-host-trait)
- [Timeline and multi-agent division of work](#timeline-and-multi-agent-division-of-work)
- [The differential oracle method](#the-differential-oracle-method)
- [Hard bugs differential testing exposed](#hard-bugs-differential-testing-exposed)
- [Scorecard and limitations](#scorecard-and-limitations)
- [One command for each claim](#one-command-for-each-claim)
- [Lessons that held up](#lessons-that-held-up)

## Motivation, scope, and implementation rules

### Why target one exact release

CMake has no formal language specification. Its observable behavior is defined by 227 policies (`CMP0000` to `CMP0226`), `if()` auto-dereference rules, `CMakeCache.txt` serialization, Ninja quoting, and the upstream `Tests/RunCMake` suite. In CMake 4.x, `CMP0000` to `CMP0065` always use NEW behavior, while `CMP0066` to `CMP0226` are the 161 active policies projects can still set to OLD. Pinning CMake 4.5.1 gives every edge case a single testable oracle answer.

The implementation covers `cmake` (including `-P`, `-E`, `--build`, `--install`, `--workflow`, `--find-package`, Presets v13, SARIF 2.1.0, and Unix `--debugger` DAP), Unix `ccmake`, `ctest`, `cpack` (archives, `DEB`, `RPM`, `External`, macOS `Bundle`, `DragNDrop`, `productbuild`, Windows `WIX` v3 and v4, `NSIS`, `NSIS64`, and `IFW`), File API `codemodel` v2.12, CPS 0.15.0, SPDX 3.0.1 SBOM, Instrumentation API v1, C++20 named modules, CUDA, HIP, Swift, Fortran, and Qt AUTOGEN. Under the `rcmake` name, opt-in native tooling adds the persistent probe cache with portable CI archives, sound setup-checkpoint incremental replay, drift checks, `explain`, `diff-config`, `fmt`, `lint`, `verify`, multi-config `compile_commands.json`, `--profile`, `--diagnostics=rich`, `--hermetic`, `--fetch-lock`, `rcmake-api` (including WebAssembly), and `rcmake-lsp`.

### Why `#![forbid(unsafe_code)]`

A build system parses untrusted `CMakeLists.txt` files, caches, presets, downloaded archives, binary headers, and compiler output. The workspace forbids `unsafe_code` in [`Cargo.toml`](../Cargo.toml) and every crate root, and Clippy rejects `unwrap_used`, `expect_used`, `panic`, `todo`, `unimplemented`, `indexing_slicing`, `string_slice`, `iter_over_hash_type`, and direct `std` filesystem, environment, process, and clock calls in engine crates. Reachable input paths return structured diagnostics bounded by `rcmake_base::Limits`.

This rule covers all 39 `rcmake` crates. External crates such as `rustix` and `memchr` use `unsafe` internally, and the native binary statically links zstd 1.5.7 and liblzma 5.8 (documented in [PRIOR_ART.md](PRIOR_ART.md)). Because Rust 2024 marks `std::env::set_var` as `unsafe`, `NativeHost` snapshots the process environment once at startup, tracks `ENV{...}` mutations in an in-memory overlay, and passes the merged map explicitly to child processes.

### Implementation boundary and licensing

Rust code in `crates/` and Python tools in `tools/` use `MIT OR Apache-2.0`. Five derived help and policy tables listed in [`THIRD_PARTY_NOTICES.md`](../THIRD_PARTY_NOTICES.md) section 1 stay under `BSD-3-Clause`, alongside verbatim bundled CMake 4.5.1 `Modules/`, `Templates/`, and rendered `Help/` files under `modules/cmake-4.5/` ([CMake-BSD-3-Clause.txt](../third_party/licences/CMake-BSD-3-Clause.txt)). Bundling the standard modules preserves exact helper macros and cache variables that upstream projects include directly.

Three rules enforce the boundary. First, no CMake C++ code is copied or translated into Rust. Second, the `rcmake` engine never runs an upstream `cmake` binary at runtime, and only `rcmake-harness` (including the isolated `rcmake-verify` companion) and `tools/` read `RCMAKE_ORACLE_CMAKE`. Third, [`tools/measure/licence_scan.py`](../tools/measure/licence_scan.py) checks dependency licences, forbids `unsafe` in `crates/`, rejects oracle references outside `rcmake-harness`, and checks Git hygiene on every gate. We do not call the codebase clean-room in the strict legal sense because coding models wrote most of the Rust code and their training data probably includes public CMake sources. Upstream C++ symbol names cited below only label behavior confirmed by live CMake 4.5.1 probes.

## Architecture across 39 crates and one `Host` trait

The workspace holds 39 acyclic safe-Rust crates and 47 Architecture Decision Records in [`docs/ADR.md`](ADR.md). [ARCHITECTURE.md](ARCHITECTURE.md) maps every crate and dependency. Five design choices carry most of the engine.

1. **Byte strings end to end.** `rcmake-base` stores raw `BString` values, list splitting (`;` with `\;` escapes and bracket awareness), CMake truthiness, lexical paths, and `Limits`. `rcmake-syntax` preserves raw bytes, spans, and a lossless concrete token stream (`0..src.len()`) for `fmt`, `lint`, and `lsp`.
2. **An independent KWSys regex engine.** `rcmake-regex` implements CMake's regex dialect (no non-greedy quantifiers, distinct anchor rules, `CMAKE_MATCH_<n>` side effects) in pure Rust with a step budget.
3. **Three resolution layers and an acyclic genex seam.** Configure produces an arena-based `ProjectModel` (`rcmake-model`) with backtraces. `rcmake-resolve` evaluates generator expressions through `rcmake_genex::GenexHost` into `ResolvedTarget` records (read by File API and exports) and lowers them into a `BuildGraph`.
4. **Generators only format output.** Ninja, Ninja Multi-Config, Unix Makefiles, Xcode, and Visual Studio 17 and 18 all render the same `BuildGraph`, so a single resolution fix repairs every generator.
5. **All OS access behind `rcmake_host::Host`.** `NativeHost` and in-memory `PureHost` pass the same conformance suite (`rcmake_host::conformance::run_all`).

Wrapping `Host` turned seven features into small layers without modifying the evaluator core, namely deterministic `PureHost` unit tests, `--hermetic` auditing, `JournalHost` probe caching, sound incremental replay, `CaptureHost` output buffering for `ccmake`, `rcmake-lsp` unsaved-buffer overlays, and `rcmake.wasm`. With `--no-default-features`, `rcmake-api` drops native archive and store crates (`zstd`, `liblzma`) and compiles for `wasm32-unknown-unknown` and `wasm32-wasip1`, powering the zero-server [browser playground](https://rcmake.github.io/rcmake/playground.html) with no `unsafe` and no `wasm-bindgen`.

## Timeline and multi-agent division of work

One human maintainer set the goals, boundaries, and review gates, coordinating coding agents across five calendar days.

- **2026-11-23.** Workspace bootstrap, `Host` and `rcmake-syntax` contracts, oracle harness, `rcmake-regex`, `rcmake-expr`, `rcmake-eval`, Presets v13, and `-P` script parity.
- **2026-11-24.** Full configure and resolution pipeline, Ninja, Unix Makefiles, Xcode, Visual Studio, File API, `ctest`, `cpack`, CPS, SBOM, C++20 modules, Fortran, CUDA, Qt AUTOGEN, probe cache, replay, FetchContent lockfiles, `rcmake-api`, `rcmake-lsp`, and safe-Rust fuzzing.
- **2026-11-25.** Large-project corpus parity, 20 independent black-box scenarios, cross-platform CI recovery, diagnostic and File API v2.12 alignment, WebAssembly smoke checks, brand assets, and static site.
- **2026-11-26 (`v0.1.0`).** `rcmake-json` replacing `serde_json`, probe and evaluation performance work, shell completions, man page, release receipts, and `v0.1.0` documentation.
- **2026-11-27 (`v0.2.0`).** Sound setup-checkpoint incremental replay (`vcpkg`, presets, `-C`, `PreLoad.cmake`), clean-room Unix `ccmake`, Unix `--debugger` DAP server, CPack `productbuild`, `NSIS`, `NSIS64`, `WIX`, and `IFW` installers, CUDA, HIP, and Swift lowering, expanded `explain`, `diff-config`, lossless `fmt` and 8-rule `lint` wired into `rcmake-lsp`, isolated `rcmake-verify`, multi-config `compile_commands.json`, drift tracking, and portable probe-cache archives.

By 2026-11-27, the repository holds over 545 commits across 39 crates, 1,073 Rust files, and 470,335 lines of Rust (check the live count with the command in [One command for each claim](#one-command-for-each-claim)). A human did not read most of this code line by line. Cross-model review, the differential oracle, and the seven test layers below verified it instead.

- **Gemini 4 Neon** coordinated shared contracts (`Cargo.toml`, `rcmake-base`, `rcmake-syntax`, `rcmake-diag`, `rcmake-model`, ADRs), triaged differential failures into disjoint crate slices, audited diffs, and authored docs and site tooling.
- **Opus 5.5** handled deep architectural design, tricky semantic subsystems, and high-stakes reviews on resolution, caching, and tooling slices.
- **GPT 6.2 Sol** executed parallel crate implementations, command and generator parity sweeps, measurement tooling, and routine reviews across disjoint worktrees.
- **Deep-reasoning consults** reviewed hard soundness invariants including probe-cache invalidation, replay setup checkpoints, and cross-platform RPATH semantics.

Four engineering habits kept velocity high without regressions. Parallel workers stayed on disjoint crates in separate git worktrees, no model reviewed its own code, local Cargo runs respected a 2-job limit (`CARGO_BUILD_JOBS=2`, `RUST_TEST_THREADS=3`) on a 16 GB machine, and every disagreement became a five-line probe run against CMake 4.5.1.

## The differential oracle method

Seven verification layers compare `rcmake` against the pinned CMake 4.5.1 oracle.

| Layer | Suite | Scope | What it catches |
| --- | --- | --- | --- |
| 1 | Unit and integration tests | `cargo test --workspace` on `PureHost` and `NativeHost` | Logic bugs, panics, host-contract drift |
| 2 | Recorded CLI and script cases | `rcmake-harness` over `conformance/cases/` | Byte-exact `-P`, `-E`, CLI, presets, SARIF, and `configure_file` parity |
| 3 | Upstream `Tests/RunCMake` | 371 macOS arm64, 362 Linux, 367 Windows suites run (of 463 upstream entries) | Diagnostic text, policy transitions, generator corner cases |
| 4 | Independent black-box suite | 20 scenarios, 679 checks (`independent_verify.py`) | End-to-end build, install, RPATH, `tar`, `ctest`, `cpack`, and File API parity |
| 5 | Large C++ projects | 10 open-source projects at pinned tags with reduced options (`large_projects.toml`) | Real module scripts, configure stderr (L1), Ninja build-graph identity (L2) |
| 6 | Fuzzing and ecosystem lanes | Seeded generative fuzzing (14 targets, 1,000,000 inputs each), 64-shard `vcpkg`, Conan, Qt 6 | Panics, resource exhaustion, package-manager compatibility |
| 7 | Sealed unseen holdout | 240 repositories never opened during development | Overfitting to known test suites |

L1 checks configure success and stderr parity, while L2 also compares `CMakeCache.txt`, Ninja commands and graph edges, and File API replies for listed targets. `tools/measure/runcmake_eval.py` lists every excluded `Tests/RunCMake` suite and reason (`LINUX_ONLY`, `WINDOWS_ONLY`, `POSIX_ONLY`, and `EXTERNAL_SUITES` for tools absent on standard CI runners such as `CUDA_architectures`, `Swift`, `Autogen_2` to `Autogen_7`, and `FindOpenSSL`, alongside `CMake_TEST_Qt5=OFF`, `CMake_TEST_Qt6=OFF`, and `CMAKE_Fortran_COMPILER=NOTFOUND`).

### Independent black-box tests, large projects, and documented normalizers

Upstream `Tests/RunCMake` rarely tests end-to-end invariants CMake never broke, such as whether an installed macOS binary launches with `@rpath` load commands. Our 20 independent black-box scenarios (`tools/measure/independent_verify.py`, verified first with `--oracle-only`) check 679 explicit assertions across C, C++, Objective-C++, Fortran, presets, multi-generator builds, install trees, Mach-O and ELF RPATHs, archives, `ctest`, `cpack`, and File API replies.

The 10 pinned large projects in [`conformance/corpus/large_projects.toml`](../conformance/corpus/large_projects.toml) are `duckdb` v1.3.2, `opencv` 4.12.0, `vtk` v9.5.0, `qtbase` v6.8.3, `llvm-project` 20.1.8, `pytorch` v2.7.1, `kcoreaddons` v6.15.0, `boost` 1.88.0, `rclcpp` 28.1.8, and `blender` v4.5.0, configured with documented option subsets so they fit CI runner time and installed dependencies. Separately, a sealed manifest of 240 unseen repositories runs only against the frozen release binary.

Every differential normalizer in `tools/corpus/normalize.py` and `tools/measure/verify/` carries a written rationale audited in commits `f28b50e` and `fb13e49`. Owner and group columns in `tar tvf` are masked because `rcmake` writes uid 0 and gid 0, `OPENCV_TIMESTAMP` masks wall-clock time, File API `backtraces` index arrays normalize as resolved frame chains alongside `index-*.json`, `cmake -E capabilities` normalizes empty `extraGenerators` and Windows `"debugger": false` while keeping `version`, `fileApi`, `instrumentation`, `serverMode`, and `tls` strict, and `Boost.Locale` is excluded on both engines because Boost 1.88 fails `source_group(TREE)` on CMake 4.5.1 as well.

## Hard bugs differential testing exposed

### Listfile replay deduplication (`200ed8c`)

`Interp::record_list_file` and `configure_file` initially skipped pushing paths already present in `dir.listfiles`. That broke File API `cmakeFiles` parity whenever a project included a file or configured a template twice in one directory. Removing the deduplication check matched CMake's ordered read stream.

### CMake diagnostic paragraph spacing (`ef841e3`, `200ed8c`)

CMake wraps diagnostic paragraphs via `cmDocumentationFormatter`. Earlier emitters inserted manual `\n\n` breaks, which caused `crates/rcmake-diag/src/render.rs` to accumulate prefix-matching cleanup hacks. Switching emitters to emit single-newline paragraph breaks let `format_diagnostic_body` collapse to one `format_paragraphs` call and deleted 25 lines of special cases.

### Darwin RPATH resolution and cross-platform paths (`cfacf0b`, `186d525`)

Black-box scenarios failed to launch installed macOS binaries because `rpath_entries` restricted build RPATHs to build-tree and sysroot libraries. Matching CMake's actual rule (skipping only implicit link directories and checking `@rpath` install names or Mach-O headers in `rpath_macho.rs`) fixed Ninja, Ninja Multi-Config, Makefiles, and Xcode in one change. Immediately afterward, Windows CI caught POSIX-rooted Darwin paths (`/opt/.../libfoo.dylib`) being dropped by host-native `Lexical::absolute()`, and checking `path.starts_with(b"/") || file.absolute()` restored cross-platform resolution.

### File API `codemodel` v2.12 parity (`5b51b47`, `fb13e49`)

Large C++ targets exposed drift in header file sets, generated sources, utility targets, and `cmakeFiles` `isCMake` flags. Splitting `codemodel` output into dedicated modules and passing `index-*.json` through the same canonicalizer aligned target replies across all 10 large projects.

### Probe-cache invalidation and sound setup replay (`b9bded2`, `3d6a2a4`, ADR-039)

Every probe runs under `JournalHost`, refusing to store any probe that triggers an `ImpureReason` (`ExternalWrite`, `SharedState`, `NondeterministicInput`, `UnverifiedProcess`, `ChangedInput`, or `JournalLimit`) or unmodeled environment overrides (`CCC_OVERRIDE_OPTIONS`, `COMPILER_PATH`, `GCC_EXEC_PREFIX`, `LD_PRELOAD`, `LD_LIBRARY_PATH`, `DYLD_*`, or unmodeled `CC` and `CXX` wrappers). Lookups verify every recorded file by SHA-256 digest, including the compiler binary, clang `.cfg` files, and GCC `specs`. [`tools/measure/probe_cache_eval.py`](../tools/measure/probe_cache_eval.py) tests 12 invalidation triggers plus cache control, 8 concurrent configures, `SIGKILL` crash recovery, and corruption recovery.

In `v0.1.0`, incremental replay disabled itself globally whenever a toolchain file (`vcpkg`), preset, `-C` script, or `PreLoad.cmake` was active. In `v0.2.0` (ADR-039), setup spans always run live and hash observable interpreter state into setup checkpoints so unchanged subtrees replay safely even under `vcpkg` toolchains and Conan presets.

### Large-project and archive edge cases (`c4e32bb`, `8f5188f`, `2043177`)

Differential runs also surfaced five subtle edge cases. `qtbase` parsed `try_compile` `OUTPUT_VARIABLE` with `.+###(.+)###.+`, requiring CMake's `Change Dir:` and `Run Build Command(s):` header lines. ROS 2 `rclcpp` re-included `ament_cmake_coreConfig.cmake`, revealing that CMake unsets `<Pkg>_FOUND` only when already defined and falsy. PyTorch's ONNX build called `install(EXPORT)` before `install(TARGETS ... EXPORT)`, showing that `install(EXPORT)` creates an empty export set on demand while `export(EXPORT)` stays strict. ZIP end-of-central-directory scanning was bounded to the final 65,557 bytes to eliminate quadratic scans, and `.Z` LZW decoding fixed active byte counts for 12-bit clear codes. Cross-compiling `try_run` aligned imported target resolution to `CMAKE_TRY_COMPILE_CONFIGURATION`.

## Scorecard and limitations

### Frozen release scorecard

`tools/measure/stamp_metrics.py` stamps every cell in the "Frozen release binary" column from one frozen-binary verification run. No value is estimated.

| Area | Scope | Frozen release binary |
| --- | --- | --- |
| Release commit | Git SHA and date | <!-- METRIC:release_commit -->`70c233134676`<!-- /METRIC --> (<!-- METRIC:release_date -->`2026-11-27`<!-- /METRIC -->) |
| 3-OS CI and corpus smoke | `ubuntu-24.04`, `macos-15`, `windows-2025` | <!-- METRIC:ci_jobs -->6 of 6 jobs green<!-- /METRIC --> |
| Upstream `Tests/RunCMake`, macOS arm64 | 371 suites run (`all-macos`) | <!-- METRIC:runcmake_macos -->pending frozen release run<!-- /METRIC --> |
| Upstream `Tests/RunCMake`, Linux | 362 suites run (`linux`) | <!-- METRIC:runcmake_linux -->pending frozen release run<!-- /METRIC --> |
| Upstream `Tests/RunCMake`, Windows | 367 suites run (`windows`) | <!-- METRIC:runcmake_windows -->pending frozen release run<!-- /METRIC --> |
| Upstream Core-62 conformance | 62 core suites (`core`) | <!-- METRIC:runcmake_core62 -->pending frozen release run<!-- /METRIC --> |
| Upstream policies | 35 policy suites for the 161 policies `CMP0066` to `CMP0226` | <!-- METRIC:runcmake_policies -->pending frozen release run<!-- /METRIC --> |
| Independent black-box suite | 20 scenarios, 679 checks | <!-- METRIC:independent_scenarios -->20 / 20 scenarios (679 / 679 checks passed)<!-- /METRIC --> |
| Large C++ projects (L1) | 10 projects, configure stderr parity | <!-- METRIC:large_projects_l1 -->0 / 10 passed<!-- /METRIC --> |
| Large C++ projects (L2) | 10 projects, Ninja graph and File API parity for the listed targets | <!-- METRIC:large_projects_l2 -->7 / 10 passed<!-- /METRIC --> |
| Sealed unseen holdout | 240 repositories, L1 strict parity on oracle-green repos | <!-- METRIC:unseen_holdout -->26.87% (18 / 67 repositories)<!-- /METRIC --> |
| `vcpkg` ecosystem matrix | 64 shards on Linux | <!-- METRIC:vcpkg_matrix -->pending frozen release run<!-- /METRIC --> |
| Conan and ecosystem fixtures | `ecosystem_eval.py` | <!-- METRIC:conan_ecosystem -->0 / 2 scenarios passed<!-- /METRIC --> |
| Silent `PATH` swap | `path_swap_eval.py` | <!-- METRIC:path_swap_eval -->pass (0 differences)<!-- /METRIC --> |
| Probe-cache soundness | 16 baseline checks (12 invalidation triggers, cache control, concurrent configures, crash and corruption recovery) | <!-- METRIC:probe_cache_soundness -->16 / 16 checks passed<!-- /METRIC --> |
| Mutation replay soundness | 20 seeded leaf edits, replay equivalence | <!-- METRIC:mutation_replay_soundness -->20 / 20 edits verified<!-- /METRIC --> |
| Seeded generative fuzzing | 14 targets, 1,000,000 inputs each, not coverage-guided | <!-- METRIC:fuzz_failures -->14,000,000 inputs, 0 failures<!-- /METRIC --> |
| WebAssembly and WASI checks | `wasm32-unknown-unknown` and `wasm32-wasip1` | <!-- METRIC:wasm_smoke -->6 of 6 checks passed<!-- /METRIC --> |
| Cold configure | Synthetic C project with 500 static libraries, new build tree, empty probe cache, median of 5 runs, speedup over CMake 4.5.1 | <!-- METRIC:bench_cold_configure -->1.96x<!-- /METRIC --> |
| New build tree with shared probe cache | Synthetic 24-probe project speedup over a CMake new build tree (CMake has no cache across build trees) | <!-- METRIC:bench_warm_probe -->5.70x<!-- /METRIC --> |
| Leaf edit reconfigure | Synthetic 64-directory project, p95 wall time of 5 runs | <!-- METRIC:bench_incremental_leaf_p95 -->59.8 ms<!-- /METRIC --> |
| Peak memory (synthetic 500-library cold configure) | Resident set size (MiB) | <!-- METRIC:bench_peak_rss -->135.2 MiB<!-- /METRIC --> |
| Release binary size | `rcmake` on macOS arm64 | <!-- METRIC:binary_size_macos -->16.17 MiB (16,959,248 bytes)<!-- /METRIC --> |

Per-project giant benchmark speedups and peak memory across `duckdb`, `opencv`, `protobuf`, `qtbase`, and `llvm-project` appear on the [compatibility report](https://rcmake.github.io/rcmake/dashboard.html#speed). Benchmark runs record N, generator, flags, CPU, RAM, OS, and power source with warm OS file caches. Cold configure clears inherited `RCMAKE_*` variables and starts with an empty private probe store (`--probe-jobs 1` isolates serial probes), while `--leaf-edit compile-definition` measures a leaf edit that alters target flags.

For known parity gaps from the frozen release run, the verification receipt reports <!-- METRIC:known_parity_gaps -->Full RunCMake on Linux, macOS, and Windows and the 64-shard vcpkg matrix were still running in GitHub Actions ecosystem run 46013031088 at the v0.2.0 tag cutoff, while Linux extended conformance passed 495 of 496 cases (99.80 percent) and CLI conformance, Qt 6 AUTOGEN, and Linux T2 toolchains passed. All 10 large C++ projects configured cleanly with matching dry-runs and diagnostics, and 7 of 10 passed strict L2 parity (duckdb, vtk, qtbase, rclcpp, boost, pytorch, and kcoreaddons, up from 1 of 10 in v0.1.0), while Linux CI strict L1 cache diffing failed because ecosystem.yml omitted the ccmake symlink from Linux staging. Conan fixtures passed all 6 stages including build and ctest with byte-identical package config and targets files and passing IDE compile_commands checks, while strict raw file diffs remained on CMakeCache.txt, build.ninja, rules.ninja, and compile_commands.json. The sealed 240-repository unseen holdout on macOS arm64 completed 210 of 240 repositories across 5 shards before cutoff and passed 18 of 67 oracle-green repositories at strict L1 parity (26.87 percent, up from 0 of 74 in v0.1.0), with 131 oracle-red repositories, 10 timeouts, 2 source errors, and 49 L1 differences (38 generator, 8 policy, and 3 find-module). Quiet-host benchmarks passed the synthetic incremental leaf gate at 59.79 ms p95 (2.24x faster than CMake 4.5.1), 5.70x warm probe cache, 1.96x 500-target cold configure, and 1.26x warm giant geomean across all 5 giants, while cold configure on small synthetic projects and cold duckdb, protobuf, and llvm-project remained between 0.82x and 0.97x and determinism.py passed all output comparisons while observing no overlapping parallel probe workers.<!-- /METRIC -->

### What `rcmake` does not support

- **Windows `ccmake` and Windows `--debugger` transport.** Both `ccmake` and `--debugger` DAP work on macOS and Linux in `0.2.0`, while Windows terminal UI and Windows named-pipe transport are deferred, so Windows builds report `"debugger": false`.
- **No `cmake-gui`.** Graphical desktop setup is not included.
- **External installer and verify binaries.** CPack installer generators invoke the host packager (`pkgbuild` and `productbuild`, `makensis`, `wix` or `candle` and `light`, `binarycreator`), and `rcmake verify` requires a user-supplied CMake 4.5.1 binary.
- **Legacy and niche generators.** Supported generators are `Ninja`, `Ninja Multi-Config`, `Unix Makefiles`, `Xcode`, `Visual Studio 17 2022`, and `Visual Studio 18 2026`. Legacy extra generators (`CodeBlocks`, `CodeLite`, `Eclipse CDT4`, `Kate`, `Sublime Text 2`), older Makefile variants, `Green Hills MULTI`, `FASTBuild`, and Visual Studio 14 to 16 are omitted.
- **GPU and Swift CI runner hardware.** CUDA, HIP, and Swift lowering and build-graph generation are implemented, while standard CI runners lack GPU hardware and full Swift or HIP toolchains.
- **Reproducible `tar` ownership.** `cmake -E tar` and CPack archives write uid 0 and gid 0.
- **`--version` line 2.** Line 1 prints `<tool> version 4.5.1` and line 2 prints `rcmake 0.2.0`.
- **In-process `try_compile`.** Probes execute via `rcmake-build` instead of Ninja, Make, MSBuild, or xcodebuild, so MSBuild-only and Xcode-only attributes (`CMAKE_GENERATOR_TOOLSET`, `CMAKE_XCODE_ATTRIBUTE_*`) do not affect probes.
- **Large-project option subsets and Windows ZIP aliases.** The 10 large projects configure reduced option sets for CI limits, and manual Windows ZIP extraction places `cmake.exe`, `ctest.exe`, and `cpack.exe` directly in `bin`.

## One command for each claim

Prepare the pinned CMake 4.5.1 oracle and a staged release build once from the repository root.

```sh
source env/env.sh
scripts/fetch-oracle.sh
eval "$(scripts/fetch-oracle.sh --print-env)"
RCMAKE_STAGE_PROFILE=release scripts/stage.sh
```

| Claim | Single command |
| --- | --- |
| Dependency licences, zero `unsafe` in `rcmake` crates, and Git hygiene | `python3 tools/measure/licence_scan.py --check-hygiene` |
| Rust workspace size (crates, files, lines) | `python3 -c 'import pathlib; f=list(pathlib.Path("crates").rglob("*.rs")); print(len([p for p in pathlib.Path("crates").iterdir() if p.is_dir()]), "crates,", len(f), "files,", sum(len(p.read_text(errors="ignore").splitlines()) for p in f), "lines")'` |
| 3-OS `Tests/RunCMake` (replace `all-macos` with `linux` or `windows`) | `python3 tools/measure/runcmake_eval.py --profile all-macos --all --json runcmake.json` |
| Independent 20-scenario black-box suite (679 checks) | `python3 tools/measure/independent_verify.py --stage target/stage --json independent.json` |
| 10 large C++ projects (5 local macOS projects, full 10 in `ecosystem.yml`) | `python3 tools/corpus/large_projects_local.py --project all --rcmake-stage target/stage --oracle "$RCMAKE_ORACLE_CMAKE" --mode parity --run-dir /tmp/rcmake-large` |
| Sealed 240-repository unseen holdout (L1) | `python3 tools/corpus/unseen_eval.py --split holdout --stage target/stage --oracle "$RCMAKE_ORACLE_CMAKE" --level L1 --out-json unseen.json --out-md unseen.md` |
| Conan and ecosystem fixtures (`gh workflow run ecosystem.yml -f lane=vcpkg` for 64-shard `vcpkg`) | `python3 tools/corpus/ecosystem_eval.py --stage target/stage --oracle "$RCMAKE_ORACLE_CMAKE" --out-json ecosystem.json --out-md ecosystem.md` |
| Silent `PATH` swap (`cmake`, `ctest`, `cpack` on `PATH`) | `python3 tools/measure/path_swap_eval.py --rcmake-bin target/stage/bin/rcmake --out path-swap.json` |
| Cold configure (empty probe cache), same-tree rerun, and peak RSS | `python3 tools/measure/bench_eval.py --include-corpus --json bench.json --markdown bench.md` |
| New build tree with shared probe cache and leaf edit | `python3 tools/measure/bench_configure.py --rcmake target/stage/bin/rcmake --oracle "$RCMAKE_ORACLE_CMAKE" --scenario all --json bench-configure.json --markdown bench-configure.md` |
| Probe-cache invalidation and crash-recovery soundness | `python3 tools/measure/probe_cache_eval.py --rcmake target/stage/bin/rcmake --out-json probe.json --out-md probe.md` |
| Incremental leaf-edit replay soundness | `python3 tools/measure/mutation_eval.py --fixture --rcmake target/stage/bin/rcmake --out-json mutation.json --out-md mutation.md` |
| Side-by-side parity check on any project (`rcmake verify`) | `rcmake verify --oracle "$RCMAKE_ORACLE_CMAKE" -- -S . -G Ninja` |
| Target provenance, profiling, and hermetic audit | `target/stage/bin/rcmake explain app -B build --json` |
| Build standalone `rcmake-lsp` language server | `cargo build --locked --release -p rcmake-lsp --features eval,stdio` |
| WebAssembly dependency audit and WASI `PureHost` smoke (`rcmake.wasm`) | `python3 tools/measure/wasm_smoke.py --require-wasi --json wasm-smoke.json` |
| Seeded generative fuzzing (all 14 targets, 1,000,000 inputs each) | `for s in syntax expr regex genex json presets cps archive path tolerant binfmt diagnostics fortran store-frame; do cargo run --profile fuzz --bin rcmake-fuzz -- "$s" --iterations 1000000; done` |
| Upstream CMake release and policy drift tracker | `python3 tools/measure/upstream_track.py --out-json upstream.json --out-md upstream.md` |

## Lessons that held up

- **Pin one oracle and settle every question with a probe.** Fixing CMake 4.5.1 turned specification arguments into five-line scripts with unambiguous answers.
- **Hide all OS access behind one trait on day one.** Hermetic auditing, probe-cache journaling, sound setup replay, `ccmake` output capture, LSP buffer overlays, deterministic tests, and WebAssembly all grew out of `rcmake_host::Host` without changing the evaluator core.
- **Keep generators simple.** Resolving flags, link closures, language lowering, and runtime paths once in `rcmake-resolve` meant fixing a bug once for Ninja, Makefiles, Xcode, and Visual Studio together.
- **Write independent black-box tests alongside upstream suites.** Upstream tests catch regressions CMake expects, while black-box suites catch gaps a new engine misses.
- **Delete special cases when you find the real rule.** Removing `format_with_added_separators` in `rcmake-diag` and listfile deduplication in `rcmake-eval` shrank the code while raising parity.
- **Audit normalizers and test scripts as strictly as production code.** A truncated diff or an overly broad normalizer makes a broken build look green.
