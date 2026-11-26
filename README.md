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

`rcmake` is a clean-room replacement for CMake 4.5.1, written in safe Rust (`#![forbid(unsafe_code)]`, 35 crates). One binary provides `cmake`, `ctest`, and `cpack`. It reads the same `CMakeLists.txt` files, presets, and cache files as CMake 4.5.1 and writes compatible build trees.

A second binary, `rcmake-lsp`, is a language server for CMake files.

`rcmake` is not affiliated with Kitware. "CMake" is a trademark of Kitware, Inc.

## Quick start

Install the latest release on Linux or macOS into `~/.local`:

```sh
curl -fsSL https://raw.githubusercontent.com/dvgyl/rcmake/main/scripts/install.sh | bash
```

Or build from source (needs Rust and Python 3):

```sh
git clone https://github.com/dvgyl/rcmake.git && cd rcmake
scripts/install.sh --from-source
```

Then use `rcmake` where you use `cmake`:

```sh
rcmake -S . -B build -G Ninja                 # configure and generate
rcmake --build build                          # build
rcmake explain app -B build                   # show where target "app" gets each flag
rcmake explain CMAKE_CXX_FLAGS -B build       # show where a variable gets its value
rcmake -S . -B build --profile                # write a flame graph of the configure run
```

To try `rcmake` with no install, open the [browser playground](https://dvgyl.github.io/rcmake/playground.html). It runs the real configure engine, Ninja generation, and `rcmake explain` as WebAssembly.

To make `rcmake` your `cmake`, `ctest`, and `cpack`, see [Use rcmake as a drop-in CMake](#use-rcmake-as-a-drop-in-cmake).

## Status

`rcmake` is at version `0.1.0`. A script stamps the release scorecard below from the frozen release binary at commit <!-- METRIC:release_commit -->pending frozen release run<!-- /METRIC --> on <!-- METRIC:release_date -->pending frozen release run<!-- /METRIC -->. The [compatibility report](https://dvgyl.github.io/rcmake/dashboard.html) shows the same data.

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
| Sealed unseen holdout (240 repositories, L2 dry-run parity) | <!-- METRIC:unseen_holdout -->pending frozen release run<!-- /METRIC --> |
| Cold 500-target configure speedup over CMake 4.5.1 | <!-- METRIC:bench_cold_configure -->pending frozen release run<!-- /METRIC --> |
| Warm probe-cache configure speedup over CMake 4.5.1 | <!-- METRIC:bench_warm_probe -->pending frozen release run<!-- /METRIC --> |
| Incremental leaf reconfigure (p95 wall time) | <!-- METRIC:bench_incremental_leaf_p95 -->pending frozen release run<!-- /METRIC --> |
| Adversarial fuzzing (14 targets, 1,000,000 inputs each) | <!-- METRIC:fuzz_failures -->pending frozen release run<!-- /METRIC --> (pre-freeze baseline: 14,000,000 inputs, 0 failures) |
| WebAssembly and WASI checks (`wasm32-unknown-unknown` and `wasm32-wasip1`) | <!-- METRIC:wasm_smoke -->pending frozen release run<!-- /METRIC --> (pre-freeze baseline: 6 of 6 passed) |

L1 parity means that configure succeeds and writes the same standard error as CMake 4.5.1. L2 parity also compares `CMakeCache.txt`, the Ninja commands and graph edges, and the File API replies.

Known gaps from the frozen release run: <!-- METRIC:known_parity_gaps -->pending frozen release run<!-- /METRIC -->. Report any other difference from CMake 4.5.1 with the [parity bug form](https://github.com/dvgyl/rcmake/issues/new?template=bug_report.yml).

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

- Policies `CMP0000` to `CMP0226` (OLD behavior is removed for `CMP0000` to `CMP0065`, as in CMake 4.x)
- CMake Presets, schema version 13
- File API: `codemodel` v2.12, `cache` v2, `toolchains` v1, `cmakeFiles` v1, and `configureLog` v1
- Common Package Specification (CPS) 0.15.0, read and write
- SPDX 3.0.1 software bill of materials (SBOM) through `CMAKE_INSTALL_SBOM_FORMATS`
- Instrumentation API v1
- Diagnostics with `cmake_diagnostic()` and SARIF 2.1.0 output
- `compile_commands.json`

### Native `rcmake` extensions

The extensions work only when the executable name is `rcmake`. See [Usage](#usage) for the two modes.

- **Persistent compiler probe cache.** Reuses verified compiler check results across build directories and projects. Before a reuse, `rcmake` checks the recorded host inputs.
- **Incremental configure replay.** Replays unchanged directory subtrees from the previous configure after it verifies their inputs.
- **`rcmake explain <NAME>`.** Shows where a target gets its build flags or where a variable gets its value, as terminal text or `--json`.
- **`--profile[=<dir>]`.** Writes `trace.json`, `stacks.folded`, and `flamegraph.html` for a configure run. The default `<dir>` is `rcmake-profile` in the build directory.
- **`--diagnostics=rich`.** Shows source spans and annotations in a terminal.
- **`--hermetic[=warn|strict]`.** Audits environment reads, file reads, subprocesses, and network access during configure.
- **`--fetch-lock=off|auto|update|frozen`.** Controls FetchContent lockfile verification and pinned reuse.
- **In-process and WebAssembly API.** [`rcmake-api`](crates/rcmake-api/README.md) runs configure inside your program. It compiles for `wasm32-unknown-unknown` and `wasm32-wasip1`.

[`docs/ENV_VARS.md`](docs/ENV_VARS.md) lists all environment variables.

## Install

### Linux and macOS

GitHub Releases has archives for these targets:

- `aarch64-apple-darwin`
- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-pc-windows-msvc`

`scripts/install.sh` downloads the latest release and verifies its SHA-256 checksum. It installs `bin/rcmake`, `bin/rcmake-lsp`, `share/cmake-4.5`, and the bundled shell completions and `rcmake(1)` man page into `$HOME/.local`, or into `$PREFIX` if you set it. The script needs `bash`, `python3`, and `curl`.

```sh
curl -fsSL https://raw.githubusercontent.com/dvgyl/rcmake/main/scripts/install.sh | bash
```

To give options to the piped script, put them after `bash -s --`. For example, this command also creates the `cmake`, `ctest`, and `cpack` links:

```sh
curl -fsSL https://raw.githubusercontent.com/dvgyl/rcmake/main/scripts/install.sh | bash -s -- --alias-cmake
```

| Option | Effect |
| --- | --- |
| `--from-source` | Build a release archive from the checkout. This needs a clone and the Rust toolchain. |
| `--archive <path>` | Install a local `.tar.gz` archive. The script checks `<path>.sha256` when that file exists and prints a warning when it does not. |
| `--alias-cmake` | Also create `cmake`, `ctest`, and `cpack` links to `rcmake`. If one of these files exists and is not a link to `rcmake`, the script stops before it changes anything. |
| `--force` | With `--alias-cmake`, replace existing `cmake`, `ctest`, and `cpack` files. |
| `--uninstall` | Remove the files that the script installed. |

At the end, the script lists the installed paths. If the `bin` directory is not on `PATH`, it prints the `export PATH=...` line to add to your shell profile.

### Windows

`scripts/install.ps1` downloads the latest `x86_64-pc-windows-msvc` release, checks its SHA-256 checksum, and installs `bin\rcmake.exe`, `bin\rcmake-lsp.exe`, and `share\cmake-4.5` into `%LOCALAPPDATA%\rcmake` (or `-Prefix <dir>`). It runs on Windows PowerShell 5.1 and PowerShell 7+:

```powershell
irm https://raw.githubusercontent.com/dvgyl/rcmake/main/scripts/install.ps1 | iex
```

To pass options such as `-AliasCmake`, `-Prefix`, `-Archive`, `-Version`, `-Force`, or `-Uninstall`, download the script and run it directly:

```powershell
Invoke-WebRequest https://raw.githubusercontent.com/dvgyl/rcmake/main/scripts/install.ps1 -OutFile install.ps1
.\install.ps1 -AliasCmake
```

If you prefer manual installation:

1. Download `rcmake-<version>-x86_64-pc-windows-msvc.zip` and `SHA256SUMS.txt` (or the `.sha256` file) from [GitHub Releases](https://github.com/dvgyl/rcmake/releases).
2. Compare the checksum with the recorded value:

   ```powershell
   Get-FileHash .\rcmake-<version>-x86_64-pc-windows-msvc.zip -Algorithm SHA256
   ```

3. Extract the archive and add its `bin` folder to your user `PATH`:

   ```powershell
   Expand-Archive .\rcmake-<version>-x86_64-pc-windows-msvc.zip -DestinationPath "$env:LOCALAPPDATA\Programs"
   $bin = "$env:LOCALAPPDATA\Programs\rcmake-<version>-x86_64-pc-windows-msvc\bin"
   [Environment]::SetEnvironmentVariable("Path", "$bin;" + [Environment]::GetEnvironmentVariable("Path", "User"), "User")
   ```

4. Open a new terminal and run `rcmake --version`.

The extracted `bin` folder also contains `cmake.exe`, `ctest.exe`, `cpack.exe`, and `rcmake-lsp.exe`. Because the folder is first on `PATH`, `cmake` runs `rcmake`. Keep the `bin` folder next to `share\cmake-4.5`, because `rcmake` finds the bundled CMake modules there.

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

Set `RCMAKE_STAGE_PROFILE=release` for an optimized stage. To install a local release build into `~/.local`, run `scripts/install.sh --from-source`.

### Language server

The release archives and `scripts/install.sh` include `rcmake-lsp`. To build it from source, use the `eval` feature for evaluated hover results:

```sh
cargo build --locked --release -p rcmake-lsp --features eval,stdio
```

A build with `--features stdio` alone gives a lexical-only server. [`crates/rcmake-lsp/README.md`](crates/rcmake-lsp/README.md) has configuration for VS Code, Neovim, Helix, Zed, Emacs, and CLion.

### Shell completions and man page

Release tarballs and `scripts/install.sh` install completions for Bash, Zsh, Fish, and PowerShell along with the `rcmake(1)` man page under `$PREFIX/share/`. See [`completions/README.md`](completions/README.md) for shell setup commands and [`docs/man/rcmake.1`](docs/man/rcmake.1) for the manual source.

### Upgrade

Run the install command again with the same options. The script replaces `rcmake`, `rcmake-lsp`, and `share/cmake-4.5`, and keeps your links. On Windows, extract the new archive and change the `PATH` entry to the new `bin` folder.

### Uninstall

```sh
curl -fsSL https://raw.githubusercontent.com/dvgyl/rcmake/main/scripts/install.sh | bash -s -- --uninstall
```

The script removes `bin/rcmake`, `bin/rcmake-lsp`, the `cmake`, `ctest`, and `cpack` links that point to `rcmake`, `share/cmake-4.5`, and the rcmake completions and man page. Use the same `PREFIX` that you used for the install. The script does not delete the probe and FetchContent cache. To free that disk space, delete the cache directory:

| System | Cache directory |
| --- | --- |
| macOS | `~/Library/Caches/rcmake` |
| Linux | `$XDG_CACHE_HOME/rcmake`, or `~/.cache/rcmake` |
| Windows | `%LOCALAPPDATA%\rcmake\cache` |

`RCMAKE_CACHE_DIR` overrides these locations. On Windows, delete the extracted folder and remove its `bin` folder from `PATH`.

## Usage

The binary selects its mode from the name that starts it (`argv[0]`):

| Executable name | Mode |
| --- | --- |
| `cmake`, `ctest`, `cpack` | Compatibility mode. Output, diagnostics, and exit codes match CMake 4.5.1. |
| `rcmake` | Native mode. Adds the probe cache, incremental replay, `explain`, `--profile`, `--hermetic`, `--fetch-lock`, and rich diagnostics. |

Set `RCMAKE_ORACLE_MODE=1` to force compatibility mode when you run `rcmake`.

### Use rcmake as a drop-in CMake

Install with `--alias-cmake`, or use `target/stage/bin` from a source build. Put that `bin` directory first on `PATH`, then run the standard CMake workflow:

```sh
cmake -S . -B build -G Ninja
cmake --build build
ctest --test-dir build
cmake --install build --prefix install
cpack --config build/CPackConfig.cmake
```

### Native extensions

```sh
rcmake -S . -B build -G Ninja --profile=profile-out
rcmake -S . -B build -G Ninja --hermetic=strict
rcmake explain app -B build --json
```

Each `--profile` run writes its files to a new `run-<id>` directory in the profile directory. The `current` file in the profile directory holds the name of the latest run directory. To open the latest flame graph on macOS:

```sh
open "profile-out/$(cat profile-out/current)/flamegraph.html"
```

## Troubleshooting

| Problem | Cause and fix |
| --- | --- |
| `cmake --version` prints `CMake suite maintained and supported by Kitware` | Another `cmake` comes first on `PATH`. Run `type -a cmake` to see the order, then put the `rcmake` `bin` directory first. |
| `Could not find CMAKE_ROOT` | `rcmake` cannot find the bundled CMake modules. Keep `bin/rcmake` and `share/cmake-4.5` in the same installation directory, or set `RCMAKE_VENDORED_DATA_ROOT` to the `share/cmake-4.5` directory. |
| `Unknown argument --profile` (or another native flag) | You ran the binary as `cmake`. Native flags work only when the executable name is `rcmake`. |
| A result looks different from CMake 4.5.1 | Run again with `RCMAKE_ORACLE_MODE=1` to turn off the native extensions. If the difference stays, report it with the [parity bug form](https://github.com/dvgyl/rcmake/issues/new?template=bug_report.yml). |
| You suspect a stale compiler check result | Set `RCMAKE_PROBE_CACHE=verify` to compare each cache hit with a fresh probe, or set `RCMAKE_NO_PROBE_CACHE=1` to turn off the cache. |
| Editor hover shows no evaluated values | The language server was built without the `eval` feature. Rebuild it with `--features eval,stdio`. |
| `install.sh` cannot download `SHA256SUMS.txt` | No release is published yet, or the network blocks GitHub. Clone the repository and run `scripts/install.sh --from-source`. |

## Intentional differences and limits

Compatibility mode tracks CMake 4.5.1 closely. These differences and scope limits are intentional:

- **Reproducible `tar` archives.** `cmake -E tar` and CPack archive generators write uid 0 and gid 0 instead of copying host user IDs.
- **No DAP debugger.** `cmake --debugger` prints `CMake was not built with support for --debugger`, and `cmake -E capabilities` omits `debugger`.
- **No interactive front ends.** `cmake-gui` and `ccmake` are not implemented. `edit_cache` prints `No interactive CMake dialog available.`.
- **No legacy extra generators.** CodeBlocks, CodeLite, Eclipse CDT4, Kate, and Sublime Text 2 are omitted (`extraGenerators` is empty).
- **CPack native installers.** `DEB`, `RPM`, `NSIS`, `WIX`, `productbuild`, and `IFW` are not implemented.
- **Legacy and niche generators.** `NMake Makefiles`, `NMake Makefiles JOM`, `MSYS Makefiles`, `MinGW Makefiles`, `Borland Makefiles`, `Watcom WMake`, `Green Hills MULTI`, `FASTBuild`, and Visual Studio 14, 15, and 16 are not implemented.
- **Swift is unverified.** Basic Swift wiring exists in `rcmake-resolve`, but no test builds Swift code with a Swift toolchain yet.
- **`CMAKE_STRIP` install guard.** `cmake --install --strip` writes a strip rule only when `CMAKE_STRIP` is not a CMake false value (`OFF`, `NOTFOUND`, `strip-NOTFOUND`). This avoids running a nonexistent `strip-NOTFOUND` binary.

The [engineering write-up](docs/BUILD_WRITEUP.md) gives full details.

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
# Rust unit tests, Clippy, Rustdoc, licence scan, and recorded CLI and script cases
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

The [engineering write-up](docs/BUILD_WRITEUP.md#7-reproduce-every-claim-in-one-command) has the complete command table. It covers the large C++ projects, the sealed holdout, Conan and `vcpkg`, probe-cache soundness, and mutation replay.

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
docs/           Build write-up, architecture, ADRs, environment variables, prior art
.github/        CI workflows, issue forms, and the pull request template
```

## Documentation

[`docs/README.md`](docs/README.md) tells you which document answers which question.

- [`docs/BUILD_WRITEUP.md`](docs/BUILD_WRITEUP.md): differential testing against CMake 4.5.1, bugs that testing found, limits, and how to reproduce every claim
- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md): execution pipeline, the map of 35 crates, the `Host` trait, and verification layers
- [`docs/ADR.md`](docs/ADR.md): 31 architecture decision records
- [`docs/ENV_VARS.md`](docs/ENV_VARS.md): environment variables
- [`docs/PRIOR_ART.md`](docs/PRIOR_ART.md): dependency and licence log
- [`crates/rcmake-api/README.md`](crates/rcmake-api/README.md): embed configure and `explain` in a Rust program
- [`crates/rcmake-lsp/README.md`](crates/rcmake-lsp/README.md): language server and editor setup
- [`completions/README.md`](completions/README.md) and [`docs/man/rcmake.1`](docs/man/rcmake.1): shell completions (Bash, Zsh, Fish, PowerShell) and the `rcmake(1)` man page
- [`CONTRIBUTING.md`](CONTRIBUTING.md): first contribution, clean-room rules, engineering standards, and checks
- [`CONTRIBUTORS.md`](CONTRIBUTORS.md): maintainers and attributions
- [`SECURITY.md`](SECURITY.md): vulnerability reporting

## Licence

The Rust code in `crates/` and tooling in `tools/` are dual-licensed under either:

- Apache License, Version 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE))
- MIT License ([`LICENSE-MIT`](LICENSE-MIT))

at your option.

`rcmake` bundles the CMake 4.5.1 `Modules/`, `Templates/`, and rendered `Help/` files in `modules/`. Kitware, Inc. and the CMake contributors license them under `BSD-3-Clause` ([`third_party/licences/CMake-BSD-3-Clause.txt`](third_party/licences/CMake-BSD-3-Clause.txt)), and all copyright notices stay in the files. [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md) lists all bundled code and Rust dependency licences.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in `rcmake` is dual-licensed as above, without any additional terms or conditions.
