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
  <a href="docs/FAQ.md">FAQ</a> ·
  <a href="docs/BUILD_WRITEUP.md">Build write-up</a> ·
  <a href="docs/ARCHITECTURE.md">Architecture</a> ·
  <a href="https://rcmake.github.io/rcmake/llms.txt">LLMs.txt</a>
</p>

# rcmake

`rcmake` is an independent reimplementation of CMake 4.5.1 in Rust. It contains no copied or translated CMake C++ source code, and it never calls an upstream `cmake` binary at runtime. One binary provides `cmake`, `ctest`, `cpack`, and the `rcmake-lsp` language server. It reads existing `CMakeLists.txt` files, presets, and caches, writes compatible `Ninja`, `Unix Makefiles`, `Xcode`, and `Visual Studio` build trees, and works with `vcpkg` and Conan.

Starting the binary as `rcmake-lsp` or `rcmake lsp` runs a Language Server Protocol server for CMake files. The `rcmake-api` crate also compiles to WebAssembly.

All 35 `rcmake` crates enforce `#![forbid(unsafe_code)]`. External dependencies such as `rustix` and `memchr` use `unsafe` internally, and the native binary statically links two C compression codecs, zstd 1.5.7 and liblzma 5.8.

Coding models wrote most of the Rust implementation. Differential testing against CMake 4.5.1 and cross-model review checked its behavior, as described in [docs/BUILD_WRITEUP.md](docs/BUILD_WRITEUP.md#3-timeline-and-multi-agent-division-of-work).

`rcmake` is not affiliated with Kitware. "CMake" is a trademark of Kitware, Inc.

## Quick start

Run this command to install the latest release into `~/.local` on Linux or macOS.

```sh
curl -fsSL https://raw.githubusercontent.com/rcmake/rcmake/main/scripts/install.sh | bash
```

You can also download and extract a `.tar.gz` or `.zip` archive from [GitHub Releases](https://github.com/rcmake/rcmake/releases). Keep `bin` next to `share/cmake-4.5` so `rcmake` can find its bundled modules. To verify GitHub build provenance for an archive, run `gh attestation verify rcmake-<version>-<target>.tar.gz --repo rcmake/rcmake`.

To build from source instead, you need Rust and Python 3.

```sh
git clone https://github.com/rcmake/rcmake.git && cd rcmake
scripts/install.sh --from-source
```

Verify the install and environment with these two commands.

```sh
rcmake --version
# cmake version 4.5.1
# rcmake 0.1.0
rcmake doctor
```

You can test `rcmake` on an existing CMake project in a separate build directory without changing `PATH`. Running a second configure into a fresh directory shows the shared probe cache reusing verified compiler checks.

```sh
rcmake -S . -B build-rcmake -G Ninja          # cold configure (populates probe cache)
rcmake -S . -B build-rcmake-warm -G Ninja     # new build tree reusing verified probes
rcmake --build build-rcmake                   # build
rcmake --build build-rcmake --target test     # run tests (or ctest --test-dir build-rcmake)
rcmake cache status                           # inspect probe cache entries and size
rcmake explain app -B build-rcmake            # show where target "app" gets each flag
rcmake explain CMAKE_CXX_FLAGS -B build-rcmake
rcmake -S . -B build-rcmake --profile         # write a flame graph of the configure run
```

You can also try `rcmake` in the [browser playground](https://rcmake.github.io/rcmake/playground.html), which compiles the configure engine, Ninja generator, and `rcmake explain` to WebAssembly.

## Status

`rcmake` is at version `0.1.0`. The script `tools/measure/stamp_metrics.py` fills in the scorecard below from one verification run of the frozen release binary at commit <!-- METRIC:release_commit -->`8d9182957c82`<!-- /METRIC --> on <!-- METRIC:release_date -->`2026-11-26`<!-- /METRIC -->. Each cell shows a number only after that run measures it, and no value is estimated. The [compatibility report](https://rcmake.github.io/rcmake/dashboard.html) displays the same data.

| Check | Result |
| --- | --- |
| 3-OS CI (`ubuntu-24.04`, `macos-15`, `windows-2025`) and corpus smoke (6 jobs) | <!-- METRIC:ci_jobs -->6 of 6 jobs green<!-- /METRIC --> |
| Upstream `Tests/RunCMake`, macOS arm64 (371 of 463 suites run) | <!-- METRIC:runcmake_macos -->pending frozen release run<!-- /METRIC --> |
| Upstream `Tests/RunCMake`, Linux (362 of 463 suites run) | <!-- METRIC:runcmake_linux -->99.35% (352 / 362 suites, 9,312 / 9,373 cases)<!-- /METRIC --> |
| Upstream `Tests/RunCMake`, Windows (367 of 463 suites run) | <!-- METRIC:runcmake_windows -->pending frozen release run<!-- /METRIC --> |
| Independent black-box differential suite (20 scenarios, 679 checks) | <!-- METRIC:independent_scenarios -->20 / 20 scenarios (679 / 679 checks passed)<!-- /METRIC --> |
| Large C++ projects, L1 configure parity (10 projects) | <!-- METRIC:large_projects_l1 -->0 / 10 passed<!-- /METRIC --> |
| Large C++ projects, L2 Ninja build-graph and File API parity (10 projects) | <!-- METRIC:large_projects_l2 -->1 / 10 passed<!-- /METRIC --> |
| `vcpkg` ecosystem matrix (64 shards) | <!-- METRIC:vcpkg_matrix -->96.08% (319 / 332 scored CMake ports across 16 / 64 shards). 656 ports inspected<!-- /METRIC --> |
| Conan and ecosystem fixtures | <!-- METRIC:conan_ecosystem -->0 / 2 scenarios passed<!-- /METRIC --> |
| Silent `PATH` swap verification | <!-- METRIC:path_swap_eval -->pass (0 differences)<!-- /METRIC --> |
| Sealed unseen holdout (240 repositories, L1 strict parity on oracle-green repos) | <!-- METRIC:unseen_holdout -->0.00% (0 / 74 repositories)<!-- /METRIC --> |
| Cold configure speedup over CMake 4.5.1 (synthetic C project with 500 static libraries, new build tree, empty probe cache, median of 5 runs) | <!-- METRIC:bench_cold_configure -->2.03x<!-- /METRIC --> |
| New build tree speedup over CMake 4.5.1 (synthetic 24-probe project with shared probe cache, CMake with no cross-tree cache) | <!-- METRIC:bench_warm_probe -->5.80x<!-- /METRIC --> |
| Leaf edit reconfigure (synthetic 64-directory project, p95 wall time of 5 runs) | <!-- METRIC:bench_incremental_leaf_p95 -->110.0 ms<!-- /METRIC --> |
| Seeded generative fuzzing, not coverage-guided (14 targets, 1,000,000 inputs each) | <!-- METRIC:fuzz_failures -->14,000,000 inputs, 0 failures<!-- /METRIC --> |
| WebAssembly and WASI checks (`wasm32-unknown-unknown` and `wasm32-wasip1`) | <!-- METRIC:wasm_smoke -->6 of 6 checks passed<!-- /METRIC --> |

On the five real-world C++ giant benchmarks (`duckdb`, `opencv`, `protobuf`, `qtbase`, and `llvm-project`), the geometric mean speedups on `8d9182957c82` are `1.02x` on cold configure, `1.41x` on warm probe-cache configure (including `1.91x` on `llvm-project`, `1.48x` on `protobuf`, and `1.45x` on `opencv`), and `1.21x` on incremental leaf edits (`1.80x` on `opencv` and `1.45x` on `qtbase`). Full per-project timings and peak memory figures appear on the [compatibility report](https://rcmake.github.io/rcmake/dashboard.html#benchmarks).

L1 parity means configure succeeds and writes the same standard error as CMake 4.5.1. L2 parity also compares `CMakeCache.txt`, the Ninja commands and graph edges, and the File API replies for the targets listed in `large_projects.toml`.

The RunCMake counts show how many suites execute on each operating system. The runner script `tools/measure/runcmake_eval.py` lists every excluded suite and its reason in `LINUX_ONLY`, `WINDOWS_ONLY`, `POSIX_ONLY`, and `EXTERNAL_SUITES`. Several exclusions come from missing runner toolchains rather than operating system differences, including `CUDA_architectures`, `Swift`, `Autogen_2` to `Autogen_7`, and `FindOpenSSL`. The RunCMake invocations also pass `CMake_TEST_Qt5=OFF`, `CMake_TEST_Qt6=OFF`, and `CMAKE_Fortran_COMPILER=NOTFOUND` for the same runner setup reason. Fortran is tested by the independent suite and the ecosystem lane, and Qt is tested by the `qtbase` and `kcoreaddons` large projects.

These rules govern the benchmark numbers above.

- The cold configure row comes from `tools/measure/bench_eval.py`. Every run starts in a new build tree with an empty private probe store, and the runner strips inherited `RCMAKE_*` settings.
- By default, `rcmake` runs up to four compiler probes at once (`RCMAKE_PROBE_JOBS`), whereas CMake runs them one at a time. Passing `--probe-jobs 1` to `bench_eval.py` measures `rcmake` with serial probes.
- The new build tree row comes from `tools/measure/bench_configure.py` and `tools/measure/bench_giants.py`. CMake has no cache across separate build trees. Because a CMake rerun inside the same build tree also skips checks, `bench_eval.py` reports same-tree rerun times alongside new-tree times.
- The default leaf edit touches a directory without changing any target. Passing `--leaf-edit compile-definition` adds a compile definition to the edited directory.
- Every report records N, the generator, the host CPU, memory, OS version, and power source. The operating system keeps its file cache warm across runs.

For known parity gaps from the frozen release run, the verification receipt reports <!-- METRIC:known_parity_gaps -->Full RunCMake on macOS and Windows and 48 of 64 vcpkg shards were still running on GitHub Actions at the v0.1.0 cutoff. Linux Full RunCMake passed 99.35 percent across 362 suites and 16 completed vcpkg shards are recorded. Synthetic incremental leaf reconfigure recorded 110.05 ms p95 under host load average 3.94 against the 100 ms target, while staying 1.46x faster by median (106.89 ms vs 155.66 ms) and 2.47x faster at p95 (110.05 ms vs 272.34 ms) than CMake 4.5.1. Conan fixtures passed all 6 stages including build and ctest with byte-identical package config and targets files, while the test runner failed the strict compile_commands.json path diff because CONAN_HOME was not normalized. All 10 large C++ projects configured cleanly, while strict L1 and L2 diffing failed on absent ccmake, codemodel abstractTargets ordering, Ninja phony listfile grouping, and cmake_symlink_library spacing. The sealed 240-repository unseen holdout on macOS arm64 recorded 132 oracle-red repositories, 33 timeouts, 2 clone errors, and 0 of 74 strict L1 matches due to the same absent ccmake and Ninja phony grouping differences in unseen_eval.py. The Linux CLI strict-mode check rejected /lib32 on Ubuntu while macOS CLI checks passed 31 of 31 subchecks, and the macOS probe cache requires /usr/sbin on PATH and bypasses unmodeled CC or CXX wrappers.<!-- /METRIC -->. Report any other difference from CMake 4.5.1 with the [parity bug form](https://github.com/rcmake/rcmake/issues/new?template=bug_report.yml).

## Features

### Command-line tools

- **`cmake`.** Configure and generate, `-P` scripts, `-E` utilities, `--build`, `--install`, `--workflow`, `--find-package`, and `--sarif-output`.
- **`ctest`.** Parallel test scheduling, fixtures, resource specifications, labels, repeat modes, JUnit and dashboard XML output, `discover_tests()`, and `-S` scripts.
- **`cpack`.** Archive generators (`TGZ`, `TXZ`, `TZST`, `TBZ2`, `TZ`, `TAR`, `STGZ`, `ZIP`, `7Z`, and the `ZIP_*` and `7Z_*` compression variants), Linux `DEB` and `RPM`, `External`, and macOS `Bundle` and `DragNDrop`.

### Generators

- `Ninja` and `Ninja Multi-Config`
- `Unix Makefiles`
- `Xcode` on macOS
- `Visual Studio 17 2022` and `Visual Studio 18 2026` on Windows

### Languages

- C and C++, including C++20 and C++23 named modules with Ninja `dyndep`
- Objective-C and Objective-C++
- Assembly through `ASM`, `ASM_MASM`, and `ASM_NASM`
- Fortran with module dependency scanning
- Windows resource files (`RC`)
- Qt `AUTOMOC`, `AUTOUIC`, and `AUTORCC`

See [intentional differences and limits](#intentional-differences-and-limits) for partial CUDA, HIP, and Swift coverage.

### CMake 4.5.1 interfaces

- Policies `CMP0000` to `CMP0226`, with OLD behavior removed for `CMP0000` to `CMP0065` as in CMake 4.x. Projects can still set the 161 policies from `CMP0066` to `CMP0226` to OLD. The 35 upstream `RunCMake` policy suites exercise policies in this range, and other suites cover additional policies indirectly.
- CMake Presets, schema version 13
- File API replies for `codemodel` v2.12, `cache` v2, `toolchains` v1, `cmakeFiles` v1, and `configureLog` v1
- Common Package Specification (CPS) 0.15.0, both reading and writing
- SPDX 3.0.1 software bill of materials (SBOM) through `CMAKE_INSTALL_SBOM_FORMATS`
- Instrumentation API v1
- Diagnostics through `cmake_diagnostic()` and SARIF 2.1.0 output
- `compile_commands.json`

### Native `rcmake` extensions

Native extensions are active only when you invoke the binary as `rcmake`.

- **Persistent compiler probe cache.** Reuses verified compiler check results across build directories and projects. Before reusing an entry, `rcmake` validates all recorded host inputs. In CI and release builds, set `RCMAKE_PROBE_CACHE=verify` to compare each hit against a fresh probe.
- **Parallel compiler probes.** Runs up to four compile probes at the same time by default (`RCMAKE_PROBE_JOBS`). Speculative lookahead runs only literal compile checks (`check_include_file`, `check_c_compiler_flag`, `check_cxx_compiler_flag`, `check_symbol_exists`, and `try_compile`), never `try_run` or `execute_process`. The foreground call accepts a speculative result only when it computes the exact same probe key.
- **Incremental configure replay.** Replays unchanged directory subtrees from the previous configure when the entry context and every recorded input match, applying recorded variable, cache, and property changes again in their original order.
- **`rcmake explain <NAME>`.** Shows where a target gets each build flag or where a variable gets its value, either as terminal text or `--json`.
- **`--profile[=<dir>]`.** Writes `trace.json`, `stacks.folded`, and `flamegraph.html` for a configure run. The default `<dir>` is `rcmake-profile` inside the build directory.
- **`--diagnostics=rich`.** Renders source spans and annotations in a terminal.
- **`--hermetic[=warn|strict]`.** Audits environment reads, file reads, subprocesses, and network access during configure.
- **`--fetch-lock=off|auto|update|frozen`.** Controls FetchContent lockfile checks and pinned reuse. Prefetch can start downloading an already-declared `FetchContent_Declare()` source before the script populates it, provided the source specifies an archive hash or a full Git commit ID. Compatibility mode never prefetches.
- **In-process and WebAssembly API.** [`rcmake-api`](crates/rcmake-api/README.md) runs configure inside your own Rust program and compiles for `wasm32-unknown-unknown` and `wasm32-wasip1`.

[`docs/ENV_VARS.md`](docs/ENV_VARS.md) lists all environment variables.

## Install

### Linux and macOS

GitHub Releases publishes `.tar.gz` archives for `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, and `aarch64-unknown-linux-gnu`. The `x86_64-pc-windows-msvc` archive is a `.zip` package covered in [Windows](#windows). No prebuilt `x86_64-apple-darwin` archive is published, so Intel Macs build from source with `--from-source`.

The installer `scripts/install.sh` downloads the latest release, or the version selected by `--version`, and verifies its SHA-256 sum against the release checksum file to catch damaged downloads. It installs `bin/rcmake`, `bin/rcmake-lsp`, `share/cmake-4.5`, bundled shell completions, and the `rcmake(1)` man page into `$HOME/.local`, or into `$PREFIX` if set. The script requires `bash`, `python3`, and `curl`.

```sh
curl -fsSL https://raw.githubusercontent.com/rcmake/rcmake/main/scripts/install.sh | bash
```

Pass installer options after `bash -s --`. For example, this command also creates `cmake`, `ctest`, and `cpack` links to `rcmake`.

```sh
curl -fsSL https://raw.githubusercontent.com/rcmake/rcmake/main/scripts/install.sh | bash -s -- --alias-cmake
```

| Option | Effect |
| --- | --- |
| `--version <v>` | Download release `<v>`, such as `v0.1.0` or `0.1.0`, instead of the latest release. |
| `--from-source` | Build a release archive from a local checkout using the Rust toolchain. |
| `--archive <path>` | Install a local `.tar.gz` archive. The script verifies `<path>.sha256` when present and prints a warning when absent. |
| `--alias-cmake` | Create `cmake`, `ctest`, and `cpack` links to `rcmake`. If any target already exists and does not point to `rcmake`, the script stops before changing files. |
| `--force` | With `--alias-cmake`, replace existing `cmake`, `ctest`, and `cpack` files. |
| `--uninstall` | Remove files installed by the script. |

When it finishes, the script lists every installed path. If the `bin` directory is not on `PATH`, it prints the `export PATH=...` line to add to your shell profile.

A SHA-256 sum detects corruption, not provenance. The release workflow also attaches GitHub build provenance to every archive and to `SHA256SUMS.txt`. Run this command to confirm that a downloaded file came from the release workflow of this repository.

```sh
gh attestation verify rcmake-<version>-<target>.tar.gz --repo rcmake/rcmake
```

### Windows

The PowerShell installer `scripts/install.ps1` downloads the latest `x86_64-pc-windows-msvc` release, checks its SHA-256 sum against the release checksum file, and installs `bin\rcmake.exe`, `bin\rcmake-lsp.exe`, and `share\cmake-4.5` into `%LOCALAPPDATA%\rcmake` (or `-Prefix <dir>`). It works on Windows PowerShell 5.1 and PowerShell 7 and newer.

```powershell
irm https://raw.githubusercontent.com/rcmake/rcmake/main/scripts/install.ps1 | iex
```

To pass options such as `-AliasCmake`, `-Prefix`, `-Archive`, `-Version`, `-Force`, or `-Uninstall`, save the script first and run it directly.

```powershell
Invoke-WebRequest https://raw.githubusercontent.com/rcmake/rcmake/main/scripts/install.ps1 -OutFile install.ps1
.\install.ps1 -AliasCmake
```

Follow these steps to install manually.

1. Download `rcmake-<version>-x86_64-pc-windows-msvc.zip` and `SHA256SUMS.txt` from [GitHub Releases](https://github.com/rcmake/rcmake/releases).
2. Compare the downloaded file's hash against the entry in `SHA256SUMS.txt`.

   ```powershell
   Get-FileHash .\rcmake-<version>-x86_64-pc-windows-msvc.zip -Algorithm SHA256
   ```

3. Extract the archive and add its `bin` folder to your user `PATH`.

   ```powershell
   Expand-Archive .\rcmake-<version>-x86_64-pc-windows-msvc.zip -DestinationPath "$env:LOCALAPPDATA\Programs"
   $bin = "$env:LOCALAPPDATA\Programs\rcmake-<version>-x86_64-pc-windows-msvc\bin"
   [Environment]::SetEnvironmentVariable("Path", "$bin;" + [Environment]::GetEnvironmentVariable("Path", "User"), "User")
   ```

4. Open a new terminal and run `rcmake --version`.

Keep the `bin` folder next to `share\cmake-4.5` because `rcmake` loads its bundled CMake modules from that relative path.

### Build from source

`rcmake` builds with the Rust toolchain pinned in [`rust-toolchain.toml`](rust-toolchain.toml) (Rust 1.99.1).

```sh
git clone https://github.com/rcmake/rcmake.git
cd rcmake
source env/env.sh
scripts/stage.sh
```

Running `scripts/stage.sh` builds `rcmake` and writes a staged tree under `target/stage`.

```text
target/stage/
├── bin/
│   ├── rcmake
│   ├── cmake      -> rcmake
│   ├── ctest      -> rcmake
│   ├── cpack      -> rcmake
│   └── rcmake-lsp -> rcmake
└── share/cmake-4.5/
    ├── Modules/
    ├── Templates/
    └── Help/
```

Set `RCMAKE_STAGE_PROFILE=release` for an optimized stage. To install a local release build into `~/.local`, run `scripts/install.sh --from-source`.

### Language server

Release archives and `scripts/install.sh` include `rcmake-lsp`, which is a symlink to the same multi-call `rcmake` binary and also works as `rcmake lsp` with `--help` and `--version`. To build the standalone `rcmake-lsp` crate from source with evaluated hover values, enable both `eval` and `stdio`.

```sh
cargo build --locked --release -p rcmake-lsp --features eval,stdio
```

Building with `--features stdio` alone produces a lexical-only server. [`crates/rcmake-lsp/README.md`](crates/rcmake-lsp/README.md) covers setup for VS Code, Neovim, Helix, Zed, Emacs, and CLion.

### Shell completions and man page

Release tarballs and `scripts/install.sh` install completions for Bash, Zsh, Fish, and PowerShell along with the `rcmake(1)` man page under `$PREFIX/share/`. See [`completions/README.md`](completions/README.md) for shell setup commands and [`docs/man/rcmake.1`](docs/man/rcmake.1) for the manual source.

### Upgrade

Run the install command again with the same options. The script replaces `rcmake`, `rcmake-lsp`, and `share/cmake-4.5` while preserving your links. On Windows, extract the new archive and point your `PATH` entry at the new `bin` folder.

### Uninstall

Run the installer with `--uninstall` and the same `PREFIX` used for installation.

```sh
curl -fsSL https://raw.githubusercontent.com/rcmake/rcmake/main/scripts/install.sh | bash -s -- --uninstall
```

The script removes `bin/rcmake`, `bin/rcmake-lsp`, any `cmake`, `ctest`, and `cpack` links pointing to `rcmake`, `share/cmake-4.5`, and the `rcmake` completions and man page. It leaves the probe and FetchContent cache untouched. Delete the cache directory below if you want to reclaim that space.

| System | Cache directory |
| --- | --- |
| macOS | `~/Library/Caches/rcmake` |
| Linux | `$XDG_CACHE_HOME/rcmake`, or `~/.cache/rcmake` |
| Windows | `%LOCALAPPDATA%\rcmake\cache` |

`RCMAKE_CACHE_DIR` overrides these paths. On Windows, delete the extracted folder and remove its `bin` folder from `PATH`.

## Usage

The binary selects its mode from the executable name (`argv[0]`).

| Executable name | Mode |
| --- | --- |
| `cmake`, `ctest`, `cpack` | Compatibility mode. Output, diagnostics, and exit codes match CMake 4.5.1, subject to the [intentional differences](#intentional-differences-and-limits). |
| `rcmake` | Native mode. Enables the probe cache, incremental replay, `explain`, `--profile`, `--hermetic`, `--fetch-lock`, and rich diagnostics. |

Set `RCMAKE_ORACLE_MODE=1` to force compatibility mode when invoking `rcmake`.

### Use rcmake as a drop-in CMake

Install with `--alias-cmake`, or use `target/stage/bin` from a source build. Put that `bin` directory first on `PATH`, then run the normal CMake workflow.

```sh
cmake -S . -B build -G Ninja
cmake --build build
ctest --test-dir build
cmake --install build --prefix install
cpack --config build/CPackConfig.cmake
```

### Native extensions

Run these commands under the `rcmake` name to profile, audit inputs, or inspect target provenance.

```sh
rcmake -S . -B build -G Ninja --profile=profile-out
rcmake -S . -B build -G Ninja --hermetic=strict
rcmake explain app -B build --json
```

Each `--profile` run writes the latest `flamegraph.html`, `trace.json`, and `stacks.folded` directly into the profile directory, while archiving each run in `run-<id>/` and recording the latest run name in `current`. On macOS, open the flame graph with this command.

```sh
open profile-out/flamegraph.html
```

## Troubleshooting

| Problem | Cause and fix |
| --- | --- |
| `cmake --version` prints `CMake suite maintained and supported by Kitware` | Another `cmake` appears earlier on `PATH`. Run `type -a cmake` to inspect the order, then place the `rcmake` `bin` directory first. |
| `Could not find CMAKE_ROOT` | `rcmake` cannot locate its bundled CMake modules. Keep `bin/rcmake` and `share/cmake-4.5` in the same installation tree, or set `RCMAKE_VENDORED_DATA_ROOT` to `share/cmake-4.5`. |
| `Unknown argument --profile` (or another native flag) | You invoked the binary as `cmake`. Native flags work only when invoked as `rcmake`. |
| A result looks different from CMake 4.5.1 | Run again with `RCMAKE_ORACLE_MODE=1` to disable native extensions. If the difference persists, report it with the [parity bug form](https://github.com/rcmake/rcmake/issues/new?template=bug_report.yml). |
| You suspect a stale compiler check result | Set `RCMAKE_PROBE_CACHE=verify` to check each hit against a fresh probe, or set `RCMAKE_NO_PROBE_CACHE=1` to disable the cache. |
| Probe cache stays at 0 entries | Run `rcmake doctor` to inspect the cache path and mode. On macOS, a stripped `PATH` without `/usr/sbin` blocks host fingerprinting, so include `/usr/sbin` on `PATH`. Unmodeled `CC` or `CXX` wrapper scripts also bypass publication safely rather than store an unverified entry. |
| `-G Ninja --hermetic=strict` rejects `ninja` | When `ninja` lives outside default system roots, set `RCMAKE_HERMETIC_ALLOW_PROGRAMS="$(command -v ninja)"`. |
| Editor hover shows no evaluated values | The language server was built without the `eval` feature. Rebuild it with `--features eval,stdio`. |
| `install.sh` cannot download `SHA256SUMS.txt` | No release is published yet, or the network blocks GitHub. Clone the repository and run `scripts/install.sh --from-source`. |

## Intentional differences and limits

- **Reproducible `tar` archives.** `cmake -E tar` and CPack archive generators write uid 0 and gid 0 instead of copying host user IDs so archive bytes stay deterministic across machines.
- **No DAP debugger.** `cmake --debugger` prints `CMake was not built with support for --debugger`, and `cmake -E capabilities` reports `"debugger":false`. Debugger server support was cut from the `v0.1.0` scope.
- **No interactive front ends.** `cmake-gui` and `ccmake` were cut from the `v0.1.0` scope, so `edit_cache` prints `No interactive CMake dialog available.`.
- **No legacy extra generators.** CodeBlocks, CodeLite, Eclipse CDT4, Kate, and Sublime Text 2 were omitted for `v0.1.0`, leaving `extraGenerators` empty.
- **CPack platform installers.** Archive generators (`TGZ`, `TXZ`, `TZST`, `TBZ2`, `TZ`, `TAR`, `STGZ`, `ZIP`, `7Z`), Linux `DEB` and `RPM`, `External`, and macOS `Bundle` and `DragNDrop` are implemented. `NSIS`, `WIX`, `productbuild`, and `IFW` were cut from the `v0.1.0` scope.
- **Legacy and niche generators.** `NMake Makefiles`, `NMake Makefiles JOM`, `MSYS Makefiles`, `MinGW Makefiles`, `Borland Makefiles`, `Watcom WMake`, `Green Hills MULTI`, `FASTBuild`, and Visual Studio 14, 15, and 16 were cut from `v0.1.0` to concentrate on Ninja, Unix Makefiles, Xcode, and Visual Studio 17 and 18.
- **Partial CUDA, HIP, and Swift coverage.** CUDA has one end-to-end fixture in the ecosystem lane, while HIP and Swift have rule wiring in `rcmake-resolve` without full toolchain builds because standard CI runners lack GPU hardware and those extra toolchains.
- **`--version` text.** Line 1 prints `cmake version 4.5.1`, and line 2 prints `rcmake 0.1.0` instead of the Kitware maintainer line so the output never misattributes the binary to Kitware.
- **`try_compile` runs in-process.** Probes run through the `rcmake` build executor instead of spawning Ninja, Make, MSBuild, or xcodebuild, which keeps probes fast and fully traceable under `JournalHost`. Across the tested probe suites, pass and fail outcomes and feature checks match CMake 4.5.1, while build tool lines in `OUTPUT_VARIABLE` differ and settings read only by MSBuild or Xcode (`CMAKE_GENERATOR_TOOLSET`, `CMAKE_XCODE_ATTRIBUTE_*`) have no effect on probes.
- **Incremental replay guard.** Native-mode replay disables itself when a toolchain file (including `vcpkg`), a configure preset, `-C`, or `PreLoad.cmake` is active. That is a soundness guard because global setup hooks can change configure state outside directory-scoped tracking. [`docs/ENV_VARS.md`](docs/ENV_VARS.md#incremental-replay) lists every condition.
- **Large-project option subsets.** The 10 large projects run unmodified source trees at pinned tags with reduced option sets in `conformance/corpus/large_projects.toml` so they fit CI runner build times and installed dependencies. For example, OpenCV builds 3 modules with CUDA off, and LLVM builds only `clang` for the host target. L2 compares only the listed targets.
- **Windows ZIP aliases.** Extracting the Windows ZIP archive puts `cmake.exe`, `ctest.exe`, and `cpack.exe` in `bin`, so placing that folder first on `PATH` hides any existing CMake install. Both `install.sh` and `install.ps1` create those aliases only when you pass `--alias-cmake` or `-AliasCmake`.
- **`CMAKE_STRIP` install guard.** `cmake --install --strip` writes a strip rule only when `CMAKE_STRIP` is not a CMake false value (`OFF`, `NOTFOUND`, `strip-NOTFOUND`), preventing install scripts from calling a missing `strip-NOTFOUND` binary.

## Reproduce verification locally

Download the pinned CMake 4.5.1 reference binary and stage a release build once.

```sh
source env/env.sh
scripts/fetch-oracle.sh
eval "$(scripts/fetch-oracle.sh --print-env)"
RCMAKE_STAGE_PROFILE=release scripts/stage.sh
```

Run any verification suite with one command.

```sh
# Licence scan, rustfmt, Clippy, Rustdoc, workspace tests, and recorded CLI and script cases
scripts/gate.sh

# Upstream Tests/RunCMake (use --profile linux or --profile windows on those OSes)
python3 tools/measure/runcmake_eval.py --profile all-macos --all --json runcmake.json

# Independent 20-scenario black-box suite (679 checks)
python3 tools/measure/independent_verify.py --stage target/stage --json independent.json

# Silent PATH swap check
python3 tools/measure/path_swap_eval.py --rcmake-bin target/stage/bin/rcmake --out path-swap.json

# Cold configure (empty probe cache), same-tree rerun, and peak memory
python3 tools/measure/bench_eval.py --include-corpus --json bench.json --markdown bench.md
# New build tree with shared probe cache, and leaf edit (add --leaf-edit compile-definition to change targets)
python3 tools/measure/bench_configure.py --rcmake target/stage/bin/rcmake --oracle "$RCMAKE_ORACLE_CMAKE" --scenario all --json bench-configure.json --markdown bench-configure.md

# WebAssembly and WASI smoke checks
python3 tools/measure/wasm_smoke.py --require-wasi --json wasm-smoke.json

# Seeded generative fuzzing, not coverage-guided (one of 14 targets)
cargo run --profile fuzz --bin rcmake-fuzz -- syntax --iterations 1000000

# Upstream CMake release and policy tracking
python3 tools/measure/upstream_track.py --out-json upstream.json --out-md upstream.md
```

To run every local scorecard lane and write the receipts that `tools/measure/assemble_release_receipts.py` consumes, run `scripts/reproduce-scorecard.sh --receipts-dir <dir>`. Pass `--dry-run` to print each command and receipt path first, `--lane <name>` to run one lane, or `--help` to list all lanes. Receipts for large projects, `vcpkg`, the sealed holdout, fuzzing, and CI jobs come from CI or dedicated hosts rather than this script.

The [engineering write-up](docs/BUILD_WRITEUP.md#7-one-command-for-each-claim) has the complete command table covering large C++ projects, the sealed holdout, Conan and `vcpkg`, probe-cache soundness, and mutation replay.

## Repository layout

```text
crates/         Rust workspace (35 crates, #![forbid(unsafe_code)])
modules/        Bundled CMake 4.5.1 Modules/, Templates/, and Help/ (BSD-3-Clause)
conformance/    Differential test cases, baselines, corpus manifests, ecosystem fixtures
tools/          Python measurement, corpus evaluation, and code generation tools
scripts/        Staging, installation, distribution, quality gate, and oracle scripts
env/            Shell environment for local builds and the CMake 4.5.1 oracle (env.sh)
assets/         Vector brand assets
site/           Static website, browser playground, and compatibility dashboard
third_party/    Licence texts for bundled third-party code
docs/           FAQ, build write-up, architecture, ADRs, environment variables, prior art
.github/        CI workflows, issue forms, and the pull request template
```

## Documentation

- [`docs/FAQ.md`](docs/FAQ.md) answers common questions on CMake 4.5.1 compatibility, generators, caching, native tools, installation, and limits.
- [`site/llms.txt`](site/llms.txt) provides the machine-readable index for LLMs and developer questions, also hosted at [`llms.txt`](https://rcmake.github.io/rcmake/llms.txt) and [`llms-full.txt`](https://rcmake.github.io/rcmake/llms-full.txt).
- [`docs/BUILD_WRITEUP.md`](docs/BUILD_WRITEUP.md) covers differential testing against CMake 4.5.1, bugs that testing exposed, limits, and one command for every claim.
- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) explains the execution pipeline, the 35 crates, the `Host` trait, and the verification layers.
- [`docs/ADR.md`](docs/ADR.md) collects 38 architecture decision records.
- [`docs/ENV_VARS.md`](docs/ENV_VARS.md) lists all environment variables.
- [`docs/PRIOR_ART.md`](docs/PRIOR_ART.md) logs external dependencies and licences.
- [`crates/rcmake-api/README.md`](crates/rcmake-api/README.md) shows how to embed configure and `explain` in a Rust program.
- [`crates/rcmake-lsp/README.md`](crates/rcmake-lsp/README.md) covers language server setup across editors.
- [`completions/README.md`](completions/README.md) and [`docs/man/rcmake.1`](docs/man/rcmake.1) document shell completions for Bash, Zsh, Fish, and PowerShell along with the `rcmake(1)` man page.
- [`CONTRIBUTING.md`](CONTRIBUTING.md) explains first contributions, implementation rules, engineering standards, and checks.
- [`CONTRIBUTORS.md`](CONTRIBUTORS.md) lists maintainers and attributions.
- [`SECURITY.md`](SECURITY.md) describes how to report vulnerabilities.

## Licence

The CMake-derived files listed in [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md) section 1 contain text and tables derived from CMake 4.5.1. Those five files use `BSD-3-Clause`.

- `crates/rcmake-diag/src/generated_policies.rs`
- `crates/rcmake-diag/src/policy_warnings.rs`
- `crates/rcmake-lsp/src/generated_catalog.rs`
- `crates/rcmake-commands/src/targets/generated_init.rs`
- `crates/rcmake-cli/help/usage.txt`

All other code in `crates/` and `tools/` is dual-licensed under either the Apache License, Version 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE)) or the MIT License ([`LICENSE-MIT`](LICENSE-MIT)), at your option.

`rcmake` bundles the CMake 4.5.1 `Modules/`, `Templates/`, and rendered `Help/` files in `modules/`. Kitware, Inc. and the CMake contributors license them under `BSD-3-Clause` ([`third_party/licences/CMake-BSD-3-Clause.txt`](third_party/licences/CMake-BSD-3-Clause.txt)), and all copyright notices remain intact. [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md) lists all bundled code and Rust dependency licences.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in `rcmake` is dual-licensed as above, without any additional terms or conditions.
