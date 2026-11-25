<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/rcmake-wordmark-dark.svg">
    <source media="(prefers-color-scheme: light)" srcset="assets/brand/rcmake-wordmark-light.svg">
    <img alt="rcmake" src="assets/brand/rcmake-wordmark-light.svg" width="360">
  </picture>
</p>

<p align="center">
  <a href="https://dvgyl.github.io/rcmake/">Website</a> ·
  <a href="https://dvgyl.github.io/rcmake/playground.html">Playground</a> ·
  <a href="https://dvgyl.github.io/rcmake/dashboard.html">Compatibility</a> ·
  <a href="docs/BUILD_WRITEUP.md">Build write-up</a> ·
  <a href="docs/ARCHITECTURE.md">Architecture</a>
</p>

# rcmake

`rcmake` is a clean-room replacement for CMake 4.5.1 written in safe Rust (`#![forbid(unsafe_code)]`, 34 crates). One binary provides `cmake`, `ctest`, and `cpack`. It reads the same `CMakeLists.txt` files, presets, and cache files as CMake 4.5.1 and writes compatible build trees. A companion binary, `rcmake-lsp`, provides Language Server Protocol support for CMake files.

The Rust engine in `crates/` and tooling in `tools/` are licensed under `MIT OR Apache-2.0`. `rcmake` bundles the CMake 4.5.1 `Modules/`, `Templates/`, and rendered `Help/` files under their original `BSD-3-Clause` licence with all copyright notices preserved.

`rcmake` is not affiliated with Kitware. "CMake" is a trademark of Kitware, Inc.

## Status

`rcmake` is at version `0.1.0`. The release scorecard below is stamped mechanically from the frozen release binary at commit <!-- METRIC:release_commit -->pending frozen release run<!-- /METRIC --> on <!-- METRIC:release_date -->pending frozen release run<!-- /METRIC --> and mirrored on the [compatibility report](https://dvgyl.github.io/rcmake/dashboard.html).

| Check | Result |
| --- | --- |
| 3-OS CI (`ubuntu-24.04`, `macos-15`, `windows-2025`) and corpus smoke (6 jobs) | <!-- METRIC:ci_jobs -->pending frozen release run<!-- /METRIC --> (pre-freeze baseline: 6 of 6 green) |
| Upstream `Tests/RunCMake`, macOS arm64 (371 suites) | <!-- METRIC:runcmake_macos -->pending frozen release run<!-- /METRIC --> |
| Upstream `Tests/RunCMake`, Linux (362 suites) | <!-- METRIC:runcmake_linux -->pending frozen release run<!-- /METRIC --> |
| Upstream `Tests/RunCMake`, Windows (367 suites) | <!-- METRIC:runcmake_windows -->pending frozen release run<!-- /METRIC --> |
| Independent black-box differential suite (20 scenarios, 679 checks) | <!-- METRIC:independent_scenarios -->pending frozen release run<!-- /METRIC --> |
| Large C++ projects, L1 configure parity (10 projects) | <!-- METRIC:large_projects_l1 -->pending frozen release run<!-- /METRIC --> |
| Large C++ projects, L2 Ninja build-graph and File API parity (10 projects) | <!-- METRIC:large_projects_l2 -->pending frozen release run<!-- /METRIC --> |
| `vcpkg` ecosystem matrix (64 shards) | <!-- METRIC:vcpkg_matrix -->pending frozen release run<!-- /METRIC --> |
| Conan and ecosystem fixtures | <!-- METRIC:conan_ecosystem -->pending frozen release run<!-- /METRIC --> |
| Silent `PATH` swap verification | <!-- METRIC:path_swap_eval -->pending frozen release run<!-- /METRIC --> |
| Sealed unseen holdout (240 repositories, Layer 2 dry-run parity) | <!-- METRIC:unseen_holdout -->pending frozen release run<!-- /METRIC --> |
| Cold 500-target configure speedup over CMake 4.5.1 | <!-- METRIC:bench_cold_configure -->pending frozen release run<!-- /METRIC --> |
| Warm probe-cache configure speedup over CMake 4.5.1 | <!-- METRIC:bench_warm_probe -->pending frozen release run<!-- /METRIC --> |
| Incremental leaf reconfigure (p95 wall time) | <!-- METRIC:bench_incremental_leaf_p95 -->pending frozen release run<!-- /METRIC --> |
| Adversarial fuzzing (14 targets, 1,000,000 inputs each) | <!-- METRIC:fuzz_failures -->pending frozen release run<!-- /METRIC --> (pre-freeze baseline: 14,000,000 inputs, 0 failures) |
| WebAssembly and WASI checks (`wasm32-unknown-unknown` and `wasm32-wasip1`) | <!-- METRIC:wasm_smoke -->pending frozen release run<!-- /METRIC --> (pre-freeze baseline: 6 of 6 passed) |

Known gaps from the frozen release run: <!-- METRIC:known_parity_gaps -->pending frozen release run<!-- /METRIC -->. Report any other difference from CMake 4.5.1 as a [GitHub issue](https://github.com/dvgyl/rcmake/issues).

## Features

### Command-line tools

- `cmake`: configure and generate, `-P` scripts, `-E` utilities, `--build`, `--install`, `--workflow`, `--find-package`, and `--sarif-output`.
- `ctest`: parallel test scheduling, fixtures, resource specifications, labels, repeat modes, JUnit and dashboard XML output, `discover_tests()`, and `-S` scripts.
- `cpack`: archive generators (`TGZ`, `TXZ`, `TZST`, `TBZ2`, `TZ`, `TAR`, `ZIP`, `7Z`, and the `ZIP_*` and `7Z_*` compression variants), `External`, and macOS `DragNDrop` and `Bundle`.

### Generators

- `Ninja` and `Ninja Multi-Config`
- `Unix Makefiles`
- `Xcode` (on macOS)
- `Visual Studio 17 2022` and `Visual Studio 18 2026` (on Windows)

### Languages

- C and C++, including C++20 and C++23 named modules with Ninja `dyndep`
- Objective-C and Objective-C++
- Assembly: `ASM`, `ASM_MASM`, and `ASM_NASM`
- CUDA and HIP
- Fortran, with module dependency scanning
- Windows resource files (`RC`)
- Qt `AUTOMOC`, `AUTOUIC`, and `AUTORCC`
- Swift (basic property and rule wiring exists, but real Swift toolchain builds are unverified)

### CMake 4.5.1 interfaces

- Policies `CMP0000` to `CMP0226` (OLD behavior is removed for `CMP0000` to `CMP0065`, matching CMake 4.x)
- CMake Presets, schema version 13
- File API: `codemodel` v2.12, `cache` v2, `toolchains` v1, `cmakeFiles` v1, and `configureLog` v1
- Common Package Specification (CPS) 0.15.0, read and write
- SPDX 3.0.1 software bill of materials (SBOM) through `CMAKE_INSTALL_SBOM_FORMATS`
- Instrumentation API v1
- Diagnostics with `cmake_diagnostic()` and SARIF 2.1.0 output
- `compile_commands.json`

### Native `rcmake` extensions

These extensions activate only when you run the binary under the name `rcmake`. Invoking `cmake`, `ctest`, or `cpack` runs in strict CMake 4.5.1 compatibility mode.

- **Persistent compiler probe cache.** Reuses verified compiler check results across build directories and projects after validating recorded host inputs.
- **Incremental configure replay.** Replays unchanged directory subtrees from the prior configure after verifying their inputs.
- **`rcmake explain <NAME>`.** Shows where a target gets its build flags or where a variable gets its value (terminal or `--json`).
- **`--profile[=<dir>]`.** Writes `trace.json`, `stacks.folded`, and an HTML `flamegraph.html` for configure runs.
- **`--diagnostics=rich`.** Renders source spans and annotations on terminals.
- **`--hermetic[=warn|strict]`.** Audits environment reads, file reads, subprocesses, and network access during configure.
- **`--fetch-lock=off|auto|update|frozen`.** Controls FetchContent lockfile verification and pinned reuse.
- **In-process and WebAssembly API (`rcmake-api`).** Runs configure in-process over `Host`, compiling cleanly for `wasm32-unknown-unknown` and `wasm32-wasip1` on `PureHost`.

See [`docs/ENV_VARS.md`](docs/ENV_VARS.md) for all environment variables.

## Install

### Prebuilt archives

GitHub Releases publishes archives for:

- `aarch64-apple-darwin`
- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-pc-windows-msvc`

On Linux and macOS, `scripts/install.sh` downloads the latest release, verifies its SHA-256 checksum, and installs into `$HOME/.local` (or `$PREFIX`):

```sh
scripts/install.sh
```

Pass `--alias-cmake` to install `cmake`, `ctest`, and `cpack` links alongside `rcmake`.

### Build from source

`rcmake` builds with the Rust toolchain pinned in [`rust-toolchain.toml`](rust-toolchain.toml) (Rust 1.99.1).

```sh
git clone https://github.com/dvgyl/rcmake.git
cd rcmake
source env/env.sh
scripts/stage.sh
```

`scripts/stage.sh` builds `rcmake` and writes a staged tree under `target/stage`:

```text
target/stage/
├── bin/
│   ├── rcmake
│   ├── cmake  -> rcmake
│   ├── ctest  -> rcmake
│   └── cpack  -> rcmake
└── share/cmake-4.5/
    ├── Modules/
    ├── Templates/
    └── Help/
```

Set `RCMAKE_STAGE_PROFILE=release` for an optimized stage, or run `scripts/install.sh --from-source` to install a local release build.

To build the language server:

```sh
cargo build --locked --release -p rcmake-lsp --features stdio
```

See [`crates/rcmake-lsp/README.md`](crates/rcmake-lsp/README.md) for editor configuration.

## Usage

Put `target/stage/bin` (or your installed `bin` directory) first on `PATH`, then run standard CMake workflows:

```sh
cmake -S . -B build -G Ninja
cmake --build build
ctest --test-dir build
cmake --install build --prefix install
cpack --config build/CPackConfig.cmake
```

The binary selects its mode from `argv[0]`:

| Executable name | Mode |
| --- | --- |
| `cmake`, `ctest`, `cpack` | Compatibility mode matching CMake 4.5.1 output, diagnostics, and exit codes. |
| `rcmake` | Native mode enabling probe caching, incremental replay, `explain`, `--profile`, `--hermetic`, and rich diagnostics. |

Set `RCMAKE_ORACLE_MODE=1` to force compatibility mode when running `rcmake`.

Native extension examples:

```sh
rcmake -S . -B build -G Ninja --profile=profile-out
rcmake -S . -B build -G Ninja --hermetic=strict
rcmake explain app -B build --json
```

## Intentional differences and honest limits

Compatibility mode tracks CMake 4.5.1 closely. These differences and scope limits are intentional:

- **Reproducible `tar` archives.** `cmake -E tar` and CPack archive generators write uid 0 and gid 0 instead of copying host user IDs.
- **No DAP debugger.** `cmake --debugger` prints `CMake was not built with support for --debugger`, and `cmake -E capabilities` omits `debugger`.
- **No interactive front ends.** `cmake-gui` and `ccmake` are not implemented. `edit_cache` prints `No interactive CMake dialog available.`.
- **No legacy extra generators.** CodeBlocks, CodeLite, Eclipse CDT4, Kate, and Sublime Text 2 are omitted (`extraGenerators` is empty).
- **CPack native installers.** `DEB`, `RPM`, `NSIS`, `WIX`, `productbuild`, and `IFW` are not implemented.
- **Legacy and niche generators.** `NMake Makefiles`, `NMake Makefiles JOM`, `MSYS Makefiles`, `MinGW Makefiles`, `Borland Makefiles`, `Watcom WMake`, `Green Hills MULTI`, `FASTBuild`, and Visual Studio 14, 15, and 16 are not implemented.
- **Swift is unverified.** Basic Swift wiring exists in `rcmake-resolve`, but Swift builds are not tested against a Swift toolchain yet.
- **`CMAKE_STRIP` install guard.** `cmake --install --strip` writes a strip rule only when `CMAKE_STRIP` is not a CMake false value (`OFF`, `NOTFOUND`, `strip-NOTFOUND`). This avoids running a nonexistent `strip-NOTFOUND` binary.

See the [engineering write-up](docs/BUILD_WRITEUP.md) for full details.

## Reproduce verification locally

Download the pinned CMake 4.5.1 reference binary and stage a release build once:

```sh
source env/env.sh
scripts/fetch-oracle.sh
eval "$(scripts/fetch-oracle.sh --print-env)"
RCMAKE_STAGE_PROFILE=release scripts/stage.sh
```

Run any verification suite with one command:

```sh
# Rust unit tests, Clippy, Rustdoc, licence scan, and Layer 1 oracle suites
scripts/gate.sh

# Upstream Tests/RunCMake (use --profile linux or --profile windows on those OSes)
python3 tools/measure/runcmake_eval.py --profile all-macos --all --json runcmake.json

# Independent 20-scenario black-box suite (679 checks)
python3 tools/measure/independent_verify.py --stage target/stage --json independent.json

# Silent PATH swap check
python3 tools/measure/path_swap_eval.py --rcmake-bin target/stage/bin/rcmake --out path-swap.json

# Cold, warm probe-cache, and incremental leaf-edit benchmarks
python3 tools/measure/bench_eval.py --include-corpus --json bench.json --markdown bench.md
python3 tools/measure/bench_configure.py --rcmake target/stage/bin/rcmake --oracle "$RCMAKE_ORACLE_CMAKE" --scenario all --json bench-configure.json --markdown bench-configure.md

# WebAssembly and WASI smoke checks
python3 tools/measure/wasm_smoke.py --require-wasi --json wasm-smoke.json

# Safe-Rust adversarial fuzzing (one of 14 targets)
cargo run --release --bin rcmake-fuzz -- syntax --iterations 1000000

# Upstream CMake release and policy tracking
python3 tools/measure/upstream_track.py --out-json upstream.json --out-md upstream.md
```

The [engineering write-up](docs/BUILD_WRITEUP.md#7-reproduce-every-claim-in-one-command) includes the complete one-command table for large C++ projects, the sealed holdout, Conan and `vcpkg`, probe-cache soundness, and mutation replay.

## Repository layout

```text
crates/         Rust workspace (34 crates, #![forbid(unsafe_code)])
modules/        Bundled CMake 4.5.1 Modules/, Templates/, and Help/ (BSD-3-Clause)
conformance/    Differential test cases, baselines, corpus manifests, ecosystem fixtures
tools/          Python measurement, corpus evaluation, and code generation tools
scripts/        Staging, installation, distribution, quality gate, and oracle scripts
env/            Local environment setup (env.sh)
assets/         Vector brand assets
site/           Static website, browser playground, and compatibility dashboard
third_party/    Licence texts for bundled third-party code
docs/           Build write-up, architecture, ADRs, environment variables, prior art
```

## Documentation

- [`docs/BUILD_WRITEUP.md`](docs/BUILD_WRITEUP.md): how `rcmake` was built, multi-agent workflow, real bugs solved, limitations, and one-command reproducibility
- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md): execution pipeline, 34-crate map, `Host` trait, and verification layers
- [`docs/ADR.md`](docs/ADR.md): 31 architecture decision records
- [`docs/ENV_VARS.md`](docs/ENV_VARS.md): environment variables
- [`docs/PRIOR_ART.md`](docs/PRIOR_ART.md): dependency and licence log
- [`CONTRIBUTING.md`](CONTRIBUTING.md): clean-room rules, engineering standards, and checks
- [`CONTRIBUTORS.md`](CONTRIBUTORS.md): maintainers and attributions
- [`SECURITY.md`](SECURITY.md): vulnerability reporting

## Licence

The Rust code in `crates/` and tooling in `tools/` are dual-licensed under either:

- Apache License, Version 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE))
- MIT License ([`LICENSE-MIT`](LICENSE-MIT))

at your option.

The bundled CMake 4.5.1 files in `modules/` are licensed under `BSD-3-Clause` by Kitware, Inc. and the CMake contributors ([`third_party/licences/CMake-BSD-3-Clause.txt`](third_party/licences/CMake-BSD-3-Clause.txt)). [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md) lists all bundled code and Rust dependency licences.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in `rcmake` is dual-licensed as above, without any additional terms or conditions.
