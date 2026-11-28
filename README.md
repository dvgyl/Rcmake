<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/rcmake-wordmark-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/brand/rcmake-wordmark-light.svg">
    <img alt="rcmake" src="assets/brand/rcmake-wordmark-light.svg" width="360">
  </picture>
</p>

<p align="center">
  <a href="https://rcmake.github.io/rcmake/">Website</a> ·
  <a href="https://rcmake.github.io/rcmake/playground.html">Playground</a> ·
  <a href="https://rcmake.github.io/rcmake/dashboard.html">Compatibility</a> ·
  <a href="docs/README.md">Docs</a> ·
  <a href="docs/TOOLING.md">Tooling</a> ·
  <a href="docs/FAQ.md">FAQ</a> ·
  <a href="https://rcmake.github.io/rcmake/llms.txt">LLMs.txt</a>
</p>

# rcmake

`rcmake` is an independent reimplementation of CMake 4.5.1 in safe Rust. It contains no copied or translated CMake C++ source code, and the core engine never calls an upstream `cmake` binary at runtime. One multi-call binary provides `cmake`, `ccmake` on Unix, `ctest`, `cpack`, and the `rcmake-lsp` language server, while an isolated sibling binary (`rcmake-verify`) runs explicit side-by-side parity checks when you ask for `rcmake verify`. It reads existing `CMakeLists.txt` files, presets, and caches, writes compatible `Ninja`, `Unix Makefiles`, `Xcode`, and `Visual Studio` build trees, and works with `vcpkg` and Conan.

All 40 `rcmake` crates enforce `#![forbid(unsafe_code)]`. External Rust dependencies such as `rustix` and `memchr` use `unsafe` internally, and the native binary statically links zstd 1.5.7 and liblzma 5.8. Coding models wrote most of the Rust implementation, and differential testing against CMake 4.5.1 plus cross-model review checked its behavior as described in [docs/BUILD_WRITEUP.md](docs/BUILD_WRITEUP.md#timeline-and-multi-agent-division-of-work). `rcmake` is not affiliated with Kitware, and CMake is a trademark of Kitware, Inc.

## Contents

- [Quick start](#quick-start)
- [Status](#status)
- [Compatibility and native tools](#compatibility-and-native-tools)
- [Install](#install)
- [Modes and troubleshooting](#modes-and-troubleshooting)
- [Intentional differences and limits](#intentional-differences-and-limits)
- [Reproduce verification locally](#reproduce-verification-locally)
- [Documentation and licence](#documentation-and-licence)

## Quick start

Install the latest release into `~/.local` on Linux or macOS.

```sh
curl -fsSL https://raw.githubusercontent.com/rcmake/rcmake/main/scripts/install.sh | bash
```

On Windows PowerShell 5.1 or newer, install into `%LOCALAPPDATA%\rcmake`.

```powershell
irm https://raw.githubusercontent.com/rcmake/rcmake/main/scripts/install.ps1 | iex
```

Check the installed binary and environment.

```sh
rcmake --version
# cmake version 4.5.1
# rcmake 0.2.1
rcmake doctor
```

Test `rcmake` on an existing project in its own build directory without modifying `PATH`.

```sh
rcmake -S . -B build-rcmake -G Ninja          # cold configure (populates probe cache)
rcmake -S . -B build-rcmake-warm -G Ninja     # fresh build tree reusing verified probes
rcmake --build build-rcmake                   # build
rcmake explain app -B build-rcmake --flag -O2 # trace where target "app" gets -O2
rcmake diff-config build-main build-pr        # compare two build directories
rcmake fmt --check && rcmake lint             # format check and static lint
```

You can also try `rcmake` in the [browser playground](https://rcmake.github.io/rcmake/playground.html), which compiles configure, Ninja generation, and `rcmake explain` to WebAssembly.

## Status

`rcmake` is at version `0.2.1`. The script `tools/measure/stamp_metrics.py` fills in the table below from one verification run of the frozen release commit <!-- METRIC:release_commit -->`2a16da12e77c`<!-- /METRIC --> on <!-- METRIC:release_date -->`2026-11-28`<!-- /METRIC --> (recording each CI binary's `tested_binary_sha256` alongside the local macOS arm64 release binary). No value is estimated. The [compatibility report](https://rcmake.github.io/rcmake/dashboard.html) publishes the same dataset along with per-project giant benchmark timings and memory figures.

| Check | Result |
| --- | --- |
| 3-OS CI (`ubuntu-24.04`, `macos-15`, `windows-2025`) and corpus smoke (6 jobs) | <!-- METRIC:ci_jobs -->6 of 6 jobs green<!-- /METRIC --> |
| Upstream `Tests/RunCMake`, macOS arm64 (371 of 463 suites) | <!-- METRIC:runcmake_macos -->99.33% (348 / 371 suites, 9,641 / 9,706 cases)<!-- /METRIC --> |
| Upstream `Tests/RunCMake`, Linux (362 of 463 suites) | <!-- METRIC:runcmake_linux -->99.33% (341 / 362 suites, 9,310 / 9,373 cases)<!-- /METRIC --> |
| Upstream `Tests/RunCMake`, Windows (122 of 367 suites completed, shard 3 of 3) | <!-- METRIC:runcmake_windows -->89.19% (68 / 367 suites, 2,401 / 2,692 cases)<!-- /METRIC --> |
| Independent black-box differential suite (20 scenarios, 679 checks) | <!-- METRIC:independent_scenarios -->20 / 20 scenarios (679 / 679 checks passed)<!-- /METRIC --> |
| Large C++ projects, L1 configure, cache, File API, and graph-edge parity (10 projects) | <!-- METRIC:large_projects_l1 -->2 / 10 passed<!-- /METRIC --> |
| Large C++ projects, L2 Ninja dry-run command parity (10 projects) | <!-- METRIC:large_projects_l2 -->10 / 10 passed<!-- /METRIC --> |
| `vcpkg` ecosystem matrix (64 shards) | <!-- METRIC:vcpkg_matrix -->97.32% (1,488 / 1,529 scored CMake ports across 64 / 64 shards). 2,621 ports inspected<!-- /METRIC --> |
| Conan and ecosystem fixtures | <!-- METRIC:conan_ecosystem -->2 / 2 scenarios passed<!-- /METRIC --> |
| Silent `PATH` swap verification | <!-- METRIC:path_swap_eval -->pass (0 differences)<!-- /METRIC --> |
| Sealed unseen holdout (240 repositories, L1 strict parity on oracle-green repos) | <!-- METRIC:unseen_holdout -->39.24% (31 / 79 repositories)<!-- /METRIC --> |
| Cold configure speedup over CMake 4.5.1 (synthetic 500-library C project, empty probe cache, median of 5 runs) | <!-- METRIC:bench_cold_configure -->2.13x<!-- /METRIC --> |
| New build tree speedup over CMake 4.5.1 (synthetic 24-probe project with shared probe cache) | <!-- METRIC:bench_warm_probe -->5.42x<!-- /METRIC --> |
| Leaf edit reconfigure (synthetic 64-directory project, p95 wall time of 5 runs) | <!-- METRIC:bench_incremental_leaf_p95 -->82.5 ms<!-- /METRIC --> |
| Seeded generative fuzzing, not coverage-guided (14 targets, 1,000,000 inputs each) | <!-- METRIC:fuzz_failures -->14,000,000 inputs, 0 failures<!-- /METRIC --> |
| WebAssembly and WASI checks (`wasm32-unknown-unknown` and `wasm32-wasip1`) | <!-- METRIC:wasm_smoke -->6 of 6 checks passed<!-- /METRIC --> |

L1 parity checks configure diagnostics, `CMakeCache.txt`, File API `codemodel` v2, and Ninja graph edges against CMake 4.5.1 (10 of 10 matched diagnostics and `CMakeCache.txt`, while `duckdb` and `opencv` passed all four L1 checks). L2 parity compares Ninja dry-run commands (`ninja -n -v`) for the listed targets. Full methodology, RunCMake exclusions, and benchmark rules live in [docs/BUILD_WRITEUP.md](docs/BUILD_WRITEUP.md#the-differential-oracle-method).

For known parity gaps from the frozen release run, the verification receipt reports <!-- METRIC:known_parity_gaps -->Full RunCMake passed 9,310 of 9,373 cases (99.33 percent) across all 362 Linux suites with zero incomplete suites and 496 of 496 (100 percent) on Linux extended conformance, and passed 9,641 of 9,706 cases (99.33 percent) across all 371 macOS suites with zero incomplete suites after fixing RunCMake check-script SEND_ERROR attribution on the verbatim CI log (cross-checked locally on Apple Silicon at 9,641 passes). On Windows, RunCMake shard 3 of 3 ran 122 of 367 suites (115 clean plus 7 incomplete suites, 4 where CMake 4.5.1 itself errored on Windows temp paths plus EnvSbom, InstallSbom, and Ninja) at 2,401 of 2,692 cases (89.19 percent), while shards 1 and 2 hit the 165-minute step timeout before writing JSON and remain tracked for expanded Windows sharding in v0.2.2. All 10 large C++ projects passed strict L2 Ninja dry-run parity alongside matching diagnostics and CMakeCache.txt (10 of 10, up from 7 of 10 in v0.2.0), with duckdb and opencv passing all four L1 checks while codemodel or graph-edge differences remain on the other 8 projects. All 64 vcpkg shards completed across 2,621 ports with 1,488 of 1,529 observed CMake configures passing (97.32 percent, 41 configure mismatches, 436 non-CMake ports, 656 oracle-blocked ports, and 1,782 vs 1,831 installs). The sealed 240-repository unseen holdout completed all 6 shards at 31 of 79 oracle-green repositories passing strict L1 parity (39.24 percent, up from 26.87 percent in v0.2.0), with 151 oracle-red, 8 timeouts, 2 source errors, and 48 L1 differences (34 generator, 10 policy, 3 configure, and 1 find-module). Quiet-host unloaded benchmarks passed all 5 giants at 1.06x cold geomean, 1.36x warm probe-cache geomean, 1.09x incremental leaf geomean, and 391.4 MiB peak RSS (below CMake 4.5.1 at 392.3 MiB), while cold protobuf (0.86x), cold llvm-project (0.99x), warm qtbase (0.90x), and leaf-edit protobuf (0.76x), qtbase (0.96x), and llvm-project (0.96x) remain targets for further speedup.<!-- /METRIC --> Report any other difference from CMake 4.5.1 with the [parity bug form](https://github.com/rcmake/rcmake/issues/new?template=bug_report.yml).

## Compatibility and native tools

### CMake 4.5.1 compatibility

- **Command-line tools.** `cmake` (configure, generate, `-P`, `-E`, `--build`, `--install`, `--workflow`, `--find-package`, `--sarif-output`, and `--debugger` DAP server on Unix), `ccmake` (safe-Rust terminal cache editor on macOS and Linux), `ctest` (parallel runs, fixtures, resources, JUnit and dashboard XML, `-S` scripts), and `cpack` (archive formats, Linux `DEB` and `RPM`, `External`, macOS `Bundle`, `DragNDrop`, and `productbuild`, Windows `WIX` v3 and v4, and cross-platform `NSIS`, `NSIS64`, and `IFW`).
- **Generators.** `Ninja`, `Ninja Multi-Config`, `Unix Makefiles`, `Xcode` on macOS, `Visual Studio 17 2022`, and `Visual Studio 18 2026`.
- **Languages.** C, C++ (including C++20 and C++23 named modules with Ninja `dyndep`), Objective-C, Objective-C++, Fortran with `.mod` scanning, CUDA (NVCC and Clang, separable compilation, device linking), HIP (AMD ROCm, SPIR-V, and NVIDIA), Swift (`CMP0157` split compilation, `CMP0195`, `CMP0215`, `CMP0216`), `ASM`, `ASM_MASM`, `ASM_NASM`, Windows `RC`, and Qt `AUTOMOC`, `AUTOUIC`, and `AUTORCC`.
- **Interfaces.** Policies `CMP0000` to `CMP0226` (`CMP0000` to `CMP0065` always NEW, 161 active policies from `CMP0066` to `CMP0226`), Presets v13, File API (`codemodel` v2.12, `cache` v2, `toolchains` v1, `cmakeFiles` v1, `configureLog` v1), CPS 0.15.0, SPDX 3.0.1 SBOM, Instrumentation API v1, `cmake_diagnostic()`, SARIF 2.1.0, and `compile_commands.json`.

### Native `rcmake` workflow tools

Native tools stay strictly opt-in under the `rcmake` name and never alter compatibility mode (`cmake`, `ctest`, `cpack`, `ccmake`). See [docs/TOOLING.md](docs/TOOLING.md) and [docs/CACHING.md](docs/CACHING.md) for full flag references and workflows.

| Tool or flag | What it does | Guide |
| --- | --- | --- |
| `rcmake explain` | Trace target flags, definitions, includes, dependency paths, `find_package()` rejections (`--find`, `why-not`), and generator expressions (`--eval`) in memory | [TOOLING.md](docs/TOOLING.md#provenance-and-inspection-with-rcmake-explain) |
| `rcmake diff-config` | Compare two configured build trees from `rcmake` or official `cmake` across targets, compile, link, toolchain, packages, and cache | [TOOLING.md](docs/TOOLING.md#build-tree-comparison-with-rcmake-diff-config) |
| `rcmake fmt` and `rcmake lint` | Lossless concrete-syntax formatter and 8-rule static linter with `.rcmake.toml`, SARIF output, and `rcmake-lsp` integration | [TOOLING.md](docs/TOOLING.md#lossless-formatting-and-static-linting) |
| `rcmake verify` | Run `rcmake` and a user-supplied CMake 4.5.1 binary side by side inside the isolated `rcmake-verify` companion | [TOOLING.md](docs/TOOLING.md#side-by-side-parity-checks-with-rcmake-verify) |
| `--export-compile-commands` | Write root `compile_commands.json` plus per-config `compile_commands_<Config>.json` on multi-config generators | [TOOLING.md](docs/TOOLING.md#multi-configuration-compile-databases) |
| Probe cache and replay | Cross-project verified probe cache, parallel probes, sound replay with toolchains, presets, `-C`, and `PreLoad.cmake`, plus `cache export` and `cache import` | [CACHING.md](docs/CACHING.md#probe-cache-archives) |
| `rcmake doctor -B <dir>` | Detect stale `CMakeCache.txt` drift across compilers, toolchain files, launchers, and `CMAKE_PREFIX_PATH` | [CACHING.md](docs/CACHING.md#drift-checks) |
| `--profile`, `--hermetic`, `--fetch-lock`, `--diagnostics=rich` | Configure flame graphs, input audits, FetchContent lockfiles, and source-span terminal errors | [TOOLING.md](docs/TOOLING.md#profiling-hermetic-audits-fetchcontent-lockfiles-and-diagnostics) |
| `rcmake-lsp` and `rcmake-api` | Language server over stdio (formatting, 8 lint rules, evaluated hover) and embeddable Rust and WebAssembly engine | [rcmake-lsp](crates/rcmake-lsp/README.md) · [rcmake-api](crates/rcmake-api/README.md) |

## Install

Prebuilt [GitHub Releases](https://github.com/rcmake/rcmake/releases) archives target `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, and `x86_64-pc-windows-msvc`. Intel Macs (`x86_64-apple-darwin`) build from source.

- **Linux and macOS.** `scripts/install.sh` checks SHA-256 sums and installs `bin/rcmake`, `bin/rcmake-lsp`, `bin/rcmake-verify`, `share/cmake-4.5`, completions for Bash, Zsh, Fish, and PowerShell, and the `rcmake(1)` and `ccmake(1)` man pages into `$HOME/.local` (or `$PREFIX`). Pass `--alias-cmake` to also symlink `cmake`, `ctest`, `cpack`, and `ccmake` to `rcmake`, `--version <v>` to pin a release, `--from-source` to build locally, `--archive <path>` for a local tarball, `--force` to replace existing aliases, or `--uninstall` to remove installed files.
- **Windows.** `scripts/install.ps1` installs `bin\rcmake.exe`, `bin\rcmake-lsp.exe`, `bin\rcmake-verify.exe`, and `share\cmake-4.5` into `%LOCALAPPDATA%\rcmake` (or `-Prefix <dir>`). Download the script to pass `-AliasCmake`, `-Version`, `-Archive`, `-Force`, or `-Uninstall`. If you extract the `.zip` archive manually, keep `bin` next to `share\cmake-4.5`.
- **Provenance check.** Verify release archive provenance with `gh attestation verify rcmake-<version>-<target>.tar.gz --repo rcmake/rcmake`.
- **Build from source.** Requires the Rust toolchain in [rust-toolchain.toml](rust-toolchain.toml) (Rust 1.99.1) and Python 3. Run `scripts/install.sh --from-source` to install into `~/.local`, or `source env/env.sh && RCMAKE_STAGE_PROFILE=release scripts/stage.sh` to populate `target/stage/`.
- **Upgrade and uninstall.** Re-run the installer with your existing options to upgrade. Uninstalling leaves the local probe and FetchContent cache intact (`~/Library/Caches/rcmake`, `$XDG_CACHE_HOME/rcmake` or `~/.cache/rcmake`, or `%LOCALAPPDATA%\rcmake\cache`).

Editor setup for `rcmake-lsp` lives in [crates/rcmake-lsp/README.md](crates/rcmake-lsp/README.md), shell completion setup lives in [completions/README.md](completions/README.md), and full CLI manuals live in [docs/man/rcmake.1](docs/man/rcmake.1) and [docs/man/ccmake.1](docs/man/ccmake.1).

## Modes and troubleshooting

The binary selects its persona from `argv[0]`.

| Executable name | Mode |
| --- | --- |
| `cmake`, `ccmake`, `ctest`, `cpack` | Compatibility mode. Flags, diagnostics, and exit codes match CMake 4.5.1, and native flags are rejected. |
| `rcmake` | Native mode. Enables the probe cache, incremental replay, drift checks, multi-config compile databases, and native subcommands. Build-time regenerations keep the native persona. |

Set `RCMAKE_ORACLE_MODE=1` to force compatibility mode when invoking `rcmake`.

| Problem | Cause and fix |
| --- | --- |
| `cmake --version` prints Kitware's maintainer line | Another `cmake` is earlier on `PATH`. Run `type -a cmake` and place the `rcmake` `bin` directory first. |
| `Could not find CMAKE_ROOT` | Keep `bin/rcmake` next to `share/cmake-4.5`, or set `RCMAKE_VENDORED_DATA_ROOT` to `share/cmake-4.5`. |
| `Unknown argument --profile` (or another native flag) | You invoked the binary as `cmake`. Use `rcmake` for native subcommands and flags. |
| Output differs from CMake 4.5.1 | Run `rcmake verify --oracle /path/to/cmake-4.5.1 -- -S . -G Ninja` or retry with `RCMAKE_ORACLE_MODE=1`, then file a [parity bug](https://github.com/rcmake/rcmake/issues/new?template=bug_report.yml). |
| Suspected stale probe or cache drift | Run `rcmake doctor -B <build>` for toolchain drift, or set `RCMAKE_PROBE_CACHE=verify` to verify probe hits against fresh runs. |
| Probe cache stays at 0 entries | Run `rcmake doctor`. On macOS, ensure `/usr/sbin` is on `PATH` for host fingerprinting. Unmodeled `CC` or `CXX` wrappers bypass storage safely. |
| `-G Ninja --hermetic=strict` rejects `ninja` | Set `RCMAKE_HERMETIC_ALLOW_PROGRAMS="$(command -v ninja)"`. |

## Intentional differences and limits

- **Windows TUI and debugger transport.** `ccmake` and `--debugger` DAP support work on macOS and Linux in `0.2.1`. Windows `ccmake` terminal support and Windows `--debugger` named-pipe transport are deferred, and Windows builds report `"debugger": false` in `-E capabilities`.
- **No `cmake-gui`.** Graphical desktop setup is not included. Use `ccmake` on Unix or editor tooling through `rcmake-lsp`.
- **External tools for installers and verify.** CPack installer generators require the native packaging tool on the host (`pkgbuild` and `productbuild` on macOS, `makensis` for NSIS, `wix` or `candle` and `light` for WiX, and `binarycreator` for Qt IFW). `rcmake verify` requires a user-supplied CMake 4.5.1 binary passed via `--oracle` or `RCMAKE_ORACLE_CMAKE`.
- **Omitted legacy generators.** Supported generators are `Ninja`, `Ninja Multi-Config`, `Unix Makefiles`, `Xcode`, `Visual Studio 17 2022`, and `Visual Studio 18 2026`. Legacy extra generators (CodeBlocks, CodeLite, Eclipse CDT4, Kate, Sublime Text 2) and older Makefile or Visual Studio 14 to 16 generators are omitted.
- **GPU and Swift CI coverage.** CUDA, HIP, and Swift target lowering and build-graph generation are implemented, while standard GitHub Actions runners lack GPU hardware and full Swift or HIP toolchains.
- **Reproducible archives.** `cmake -E tar` and CPack archive generators record uid 0 and gid 0 so archive bytes stay deterministic across machines.
- **`--version` line 2.** Line 1 prints `<tool> version 4.5.1` and line 2 prints `rcmake 0.2.1` so the binary never misattributes itself to Kitware.
- **In-process `try_compile`.** Probes execute through `rcmake-build` instead of Ninja, Make, MSBuild, or xcodebuild so probes stay fast and traceable under `JournalHost`. MSBuild-only and Xcode-only attributes (`CMAKE_GENERATOR_TOOLSET`, `CMAKE_XCODE_ATTRIBUTE_*`) do not apply to probes.
- **`CMAKE_STRIP` install guard.** `cmake --install --strip` writes a strip rule only when `CMAKE_STRIP` is not a CMake false value (`OFF`, `NOTFOUND`, `strip-NOTFOUND`).
- **Windows ZIP aliases.** Extracting the Windows ZIP archive places `cmake.exe`, `ctest.exe`, and `cpack.exe` in `bin`, whereas `install.sh` and `install.ps1` create aliases only with `--alias-cmake` or `-AliasCmake`.

## Reproduce verification locally

Fetch the pinned CMake 4.5.1 oracle and stage a release build once.

```sh
source env/env.sh
scripts/fetch-oracle.sh
eval "$(scripts/fetch-oracle.sh --print-env)"
RCMAKE_STAGE_PROFILE=release scripts/stage.sh
```

Run the core local gate or reproduce all local scorecard lanes.

```sh
scripts/gate.sh
scripts/reproduce-scorecard.sh --dry-run
```

[docs/BUILD_WRITEUP.md](docs/BUILD_WRITEUP.md#one-command-for-each-claim) lists the exact command for every verification suite, including `Tests/RunCMake`, the 20 independent black-box scenarios, large C++ projects, `vcpkg`, Conan, fuzzing, benchmarks, and WebAssembly checks.

## Documentation and licence

Browse the full documentation index in [docs/README.md](docs/README.md), or jump straight to [docs/TOOLING.md](docs/TOOLING.md), [docs/CACHING.md](docs/CACHING.md), [docs/FAQ.md](docs/FAQ.md), [docs/ENV_VARS.md](docs/ENV_VARS.md), [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), [docs/ADR.md](docs/ADR.md) (48 architecture decision records), [docs/BUILD_WRITEUP.md](docs/BUILD_WRITEUP.md), [docs/PRIOR_ART.md](docs/PRIOR_ART.md), [CONTRIBUTING.md](CONTRIBUTING.md), or [SECURITY.md](SECURITY.md).

The five CMake-derived tables and help files listed in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) section 1 use `BSD-3-Clause`, and the bundled CMake 4.5.1 `Modules/`, `Templates/`, and rendered `Help/` files under `modules/` stay under `BSD-3-Clause` ([third_party/licences/CMake-BSD-3-Clause.txt](third_party/licences/CMake-BSD-3-Clause.txt)) with all original copyright notices intact. All other Rust and Python code in `crates/` and `tools/` is dual-licensed under either the Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE)) or the MIT License ([LICENSE-MIT](LICENSE-MIT)), at your option. Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in `rcmake` is dual-licensed as above, without any additional terms or conditions.
