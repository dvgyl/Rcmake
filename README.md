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
  <a href="docs/ARCHITECTURE.md">Architecture</a> ·
  <a href="CONTRIBUTING.md">Contributing</a>
</p>

# rcmake

rcmake is a clean-room replacement for CMake 4.5.1, written in safe Rust. One
binary provides `cmake`, `ctest`, and `cpack`. It reads the same
`CMakeLists.txt` files, presets, and cache files as CMake 4.5.1, and writes
compatible build trees. An optional language server, `rcmake-lsp`, provides
editor support for CMake files.

The Rust engine is licensed under `MIT OR Apache-2.0`. rcmake bundles the
CMake 4.5.1 `Modules/` and `Templates/` directories under their original
`BSD-3-Clause` licence, with all copyright notices preserved.

rcmake is not affiliated with Kitware. "CMake" is a trademark of Kitware, Inc.

## Status

rcmake is pre-release (`0.1.0-dev`). Compatibility and performance numbers for
the frozen `0.1.0` release binary will be published here and on the
[compatibility report](https://dvgyl.github.io/rcmake/dashboard.html) once the
final release gate run finishes.

| Check | Current status |
| --- | --- |
| 3-OS CI (`ubuntu-24.04`, `macos-15`, `windows-2025`) + corpus smoke | 6 / 6 jobs green |
| Upstream `Tests/RunCMake`, macOS arm64 (371 suites) | Final frozen run pending |
| Upstream `Tests/RunCMake`, Linux (362 suites) | Final frozen run pending |
| Upstream `Tests/RunCMake`, Windows (367 suites) | Final frozen run pending |
| Independent black-box differential suite (20 scenarios, 679 checks) | 679 / 679 explicit checks passing; final 20/20 run pending |
| Large-project corpus (10 projects, L1 configure + L2 build-graph parity) | Final frozen run pending |
| Unseen holdout (240 repositories, Layer 2 dry-run parity) | Sealed; single final run pending |
| Adversarial fuzzing (14 input surfaces, 1,000,000 iterations each) | 14,000,000 inputs, 0 failures |
| WebAssembly WASI smoke gates (`wasm32-unknown-unknown` + `wasm32-wasip1`) | 6 / 6 passing |

Please report differences from CMake 4.5.1 as GitHub issues.

## Features

### Command-line tools

- `cmake`: configure and generate, `-P` scripts, `-E` utilities, `--build`,
  `--install`, `--workflow`, `--find-package`, and `--sarif-output`.
- `ctest`: test scheduling, fixtures, resource specifications, labels,
  repeat modes, JUnit output, and `discover_tests()`.
- `cpack`: archive generators (`TGZ`, `TXZ`, `TZST`, `TBZ2`, `TZ`, `TAR`,
  `ZIP`, `7Z`, and the `ZIP_*` and `7Z_*` compression variants), `External`,
  and the macOS `DragNDrop` and `Bundle` generators.

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
- Swift
- Windows resource files (`RC`)
- Qt `AUTOMOC`, `AUTOUIC`, and `AUTORCC`

### CMake 4.5 interfaces

- Policies `CMP0000` to `CMP0226` (OLD behavior removed for `CMP0000`–`CMP0065`, as in CMake 4.x)
- CMake Presets, schema version 13
- File API: `codemodel` v2.12, `cache` v2, `toolchains` v1, `cmakeFiles` v1,
  and `configureLog` v1
- Common Package Specification (CPS) 0.15.0, read and write
- SPDX 3.0.1 software bill of materials (SBOM) through
  `CMAKE_INSTALL_SBOM_FORMATS`
- Instrumentation API v1
- Diagnostics with `cmake_diagnostic()` and SARIF 2.1.0 output
- `compile_commands.json`

### rcmake extensions

These features are available only when running the binary under the name
`rcmake`. The names `cmake`, `ctest`, and `cpack` follow CMake 4.5.1.

- Persistent compiler probe cache shared across build directories and projects.
- Incremental configure replay for unchanged subtrees.
- `rcmake explain <NAME>` showing provenance for targets and variables.
- `--profile=<dir>` writing traces, folded stacks, and an HTML flame graph.
- `--diagnostics=rich` showing source spans and annotations.
- `--hermetic[=warn|strict]` auditing environment reads, filesystem reads,
  subprocesses, and network access during configure.
- `--fetch-lock=off|auto|update|frozen` for FetchContent lockfile pinning.
- In-process `rcmake-api` crate, also building cleanly for `wasm32-unknown-unknown`
  and `wasm32-wasip1`.

[`docs/ENV_VARS.md`](docs/ENV_VARS.md) lists all environment variables.

## Install

### Prebuilt archives

Release archives target:

- `aarch64-apple-darwin`
- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-pc-windows-msvc`

On Linux and macOS, `scripts/install.sh` downloads a release archive, verifies
its SHA-256 checksum, and installs into `$HOME/.local` (or `$PREFIX`):

```sh
scripts/install.sh
```

Pass `--alias-cmake` to also install `cmake`, `ctest`, and `cpack` links.

### Build from source

rcmake uses the Rust toolchain pinned in `rust-toolchain.toml`.

```sh
git clone https://github.com/dvgyl/rcmake.git
cd rcmake
source env/env.sh
scripts/stage.sh
```

`scripts/stage.sh` builds `rcmake` and stages it under `target/stage`:

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

Set `RCMAKE_STAGE_PROFILE=release` for an optimized build, or run
`scripts/install.sh --from-source`.

To build the language server:

```sh
cargo build --locked --release -p rcmake-lsp --features stdio
```

See [`crates/rcmake-lsp/README.md`](crates/rcmake-lsp/README.md) for editor setup.

## Usage

Put `target/stage/bin` (or your installed `bin` directory) first on `PATH`:

```sh
cmake -S . -B build -G Ninja
cmake --build build
ctest --test-dir build
cmake --install build --prefix install
cpack --config build/CPackConfig.cmake
```

The binary selects its mode from `argv[0]`:

| Name | Behavior |
| --- | --- |
| `cmake`, `ctest`, `cpack` | Compatibility mode matching CMake 4.5.1 output, diagnostics, and exit codes. |
| `rcmake` | Native mode enabling rcmake extensions and caching features. |

Set `RCMAKE_ORACLE_MODE=1` to force compatibility mode under the `rcmake` name.

## Intentional differences from CMake 4.5.1

- **Reproducible `tar` archives:** `cmake -E tar` and CPack archive generators
  write uid 0 and gid 0 so archives do not depend on the host user ID.
- **No DAP debugger:** `cmake --debugger` reports that debugger support is not
  built in, and `cmake -E capabilities` omits `debugger`.
- **No legacy extra generators:** CodeBlocks, CodeLite, Eclipse CDT4, Kate,
  and Sublime Text 2 are omitted (`extraGenerators` is empty).
- **`CMAKE_STRIP` guard:** `cmake --install --strip` writes a strip rule only
  when `CMAKE_STRIP` is not a CMake false value (`OFF`, `NOTFOUND`,
  `strip-NOTFOUND`), avoiding attempts to run a nonexistent `strip-NOTFOUND`
  executable.

`DEB`, `RPM`, `NSIS`, `WIX` CPack generators and the `ccmake` curses UI are not
implemented.

## Compatibility testing

rcmake is tested differentially against the official CMake 4.5.1 binary across
four layers:

| Layer | Scope | Pass criterion |
| --- | --- | --- |
| 1. CLI and script output | `cmake -P`, `cmake -E`, CLI flags, `configure_file`, diagnostics, presets | Byte-for-byte stdout, stderr, generated files, and exit code after path normalization |
| 2. Upstream `Tests/RunCMake` | CMake 4.5.1 conformance suites | Matching pass/fail outcome and diagnostic regexes |
| 3. Structured output | File API replies, `compile_commands.json`, `CMakeCache.txt` | Normalized structural equality |
| 4. Real projects | Configure, build, install, and test of open-source C/C++ projects | Clean builds, identical installed artifacts, matching `ctest` results |

To run the differential suites locally:

```sh
source env/env.sh
scripts/fetch-oracle.sh
eval "$(scripts/fetch-oracle.sh --print-env)"
scripts/stage.sh

cargo test --workspace
cargo run --bin rcmake-harness -- --suite all
python3 tools/measure/runcmake_eval.py --profile all-macos --all --json runcmake.json
python3 tools/measure/independent_verify.py --stage target/stage
```

## Repository layout

```text
crates/         Rust workspace (34 crates, #![forbid(unsafe_code)])
modules/        Bundled CMake 4.5.1 Modules/, Templates/, and Help/ (BSD-3-Clause)
conformance/    Differential test cases, baselines, corpus definitions, ecosystem fixtures
tools/          Python measurement, corpus evaluation, and code generation tools
scripts/        Staging, installation, distribution, quality gate, and oracle scripts
env/            Local environment setup (env.sh)
assets/         Vector brand assets
site/           Static website, browser playground, and compatibility report
third_party/    Licence texts for bundled third-party code
docs/           Architecture, ADRs, environment variables, prior art
```

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the full 34-crate map
and data-flow pipeline.

## Documentation

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md): data flow, 34-crate map, and test layers
- [`docs/ADR.md`](docs/ADR.md): architecture decision records
- [`docs/ENV_VARS.md`](docs/ENV_VARS.md): environment variables
- [`docs/PRIOR_ART.md`](docs/PRIOR_ART.md): dependency and licence log
- [`CONTRIBUTING.md`](CONTRIBUTING.md): clean-room rules, engineering standards, and checks
- [`CONTRIBUTORS.md`](CONTRIBUTORS.md): maintainers and attributions
- [`SECURITY.md`](SECURITY.md): vulnerability reporting

## Contributors

See [`CONTRIBUTORS.md`](CONTRIBUTORS.md). Contributions are welcome under
[`CONTRIBUTING.md`](CONTRIBUTING.md).

## Licence

The Rust code in `crates/` and `tools/` is dual-licensed under either:

- Apache License, Version 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE))
- MIT License ([`LICENSE-MIT`](LICENSE-MIT))

at your option.

The bundled CMake 4.5.1 files in `modules/` are licensed under `BSD-3-Clause`
by Kitware, Inc. and the CMake contributors
([`third_party/licences/CMake-BSD-3-Clause.txt`](third_party/licences/CMake-BSD-3-Clause.txt)).
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md) lists all bundled code and
Rust dependency licences.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in rcmake is dual-licensed as above, without any additional terms
or conditions.
