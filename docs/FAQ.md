# rcmake FAQ

## Is rcmake a drop-in replacement for CMake 4.5.1 in Rust?

Yes. `rcmake` is an independent reimplementation of CMake 4.5.1 in Rust. No CMake C++ source is copied or translated, and `rcmake` never calls an upstream `cmake` binary at runtime.

One multi-call binary provides `cmake`, `ctest`, `cpack`, and `rcmake-lsp`, selecting the tool from `argv[0]`. It reads existing `CMakeLists.txt` files, CMake Presets schema v13 (`CMakePresets.json` and `CMakeUserPresets.json`), and `CMakeCache.txt`, and writes compatible build trees.

`rcmake` is not affiliated with Kitware. "CMake" is a trademark of Kitware, Inc.

## What is the difference between compatibility mode and native mode?

`rcmake` selects its mode from the executable name (`argv[0]`):

- **Compatibility mode (`cmake`, `ctest`, `cpack`, or `RCMAKE_ORACLE_MODE=1`).** Output, flags, diagnostics, and exit codes match CMake 4.5.1, subject to the [documented intentional differences](#what-is-intentionally-different-or-not-implemented-in-rcmake). Native flags such as `--profile` and `--hermetic` are rejected in this mode.
- **Native mode (`rcmake`).** Enables the persistent compiler probe cache, parallel speculative compile probes, incremental configure replay, `rcmake explain`, `--profile`, `--diagnostics=rich`, `--hermetic`, and `--fetch-lock`.

## Which generators, languages, and CMake 4.5.1 interfaces are supported?

### Generators

- `Ninja` and `Ninja Multi-Config` on macOS, Linux, and Windows
- `Unix Makefiles` on macOS and Linux
- `Xcode` on macOS
- `Visual Studio 17 2022` and `Visual Studio 18 2026` on Windows

### Languages and toolchains

- C (C90 to C23)
- C++ (including C++20 and C++23 named modules via `CXX_MODULES` file sets and Ninja `dyndep`)
- Objective-C and Objective-C++
- Fortran (with `.mod` dependency scanning)
- Assembly (`ASM`, `ASM_MASM`, and `ASM_NASM`)
- Windows resource files (`RC`)
- Qt `AUTOMOC`, `AUTOUIC`, and `AUTORCC`
- Partial or unverified: CUDA has one end-to-end fixture in the ecosystem lane, HIP has rule wiring with no toolchain test yet, and Swift is parsed with unverified toolchain builds.

### Command-line tools and CMake 4.5.1 interfaces

- `cmake`: configure and generate, `-P` scripts, `-E` utilities, `--build`, `--install`, `--workflow`, `--find-package`, `--preset`, and `--sarif-output`.
- `ctest`: parallel test scheduling, fixtures, resource specifications, labels, repeat modes, JUnit and dashboard XML output, `discover_tests()`, and `-S` scripts.
- `cpack`: archive generators (`TGZ`, `TXZ`, `TZST`, `TBZ2`, `TZ`, `TAR`, `STGZ`, `ZIP`, `7Z`, and the `ZIP_*` and `7Z_*` compression variants), Linux `DEB` and `RPM`, `External`, and macOS `Bundle` and `DragNDrop`.
- Policies `CMP0000` to `CMP0226` (`CMP0000` to `CMP0065` always use NEW behavior as in CMake 4.x; `CMP0066` to `CMP0226` are the 161 active policies).
- CMake Presets schema v13, File API (`codemodel` v2.12, `cache` v2, `toolchains` v1, `cmakeFiles` v1, `configureLog` v1), Common Package Specification (CPS) 0.15.0, SPDX 3.0.1 SBOM via `CMAKE_INSTALL_SBOM_FORMATS`, Instrumentation API v1, `cmake_diagnostic()` with SARIF 2.1.0, and `compile_commands.json`.

## Does rcmake work with vcpkg, Conan, find_package, and FetchContent?

Yes. `rcmake` bundles the CMake 4.5.1 `Modules/`, `Templates/`, and rendered `Help/` files under `share/cmake-4.5`. It works with `vcpkg` toolchain files, Conan (`CMakeToolchain` and `CMakeDeps`), `find_package()`, and `FetchContent`.

When a toolchain file (such as `vcpkg`) or a configure preset (such as Conan's preset workflow) is active, incremental configure replay disables itself automatically while configure and the compiler probe cache continue to work.

## How do the persistent probe cache and parallel compiler probes work?

In native mode, `rcmake` caches verified compiler checks (`try_compile`, `check_c_source_compiles`, `check_include_file`, `check_symbol_exists`) in a content-addressed store shared across build directories and projects.

- Every probe runs under `JournalHost`, which records file reads, search directories, depfiles, environment reads, and child processes. Probes that touch unverified or impure state are not stored.
- Before reusing an entry, `rcmake` verifies every recorded input file by SHA-256 digest, including the compiler executable itself, clang `.cfg` files, and the GCC `specs` file.
- Control the cache with `RCMAKE_PROBE_CACHE`: `readwrite` (default), `readonly`, `verify` (compares each hit with a fresh probe and uses the fresh result; recommended for CI and release builds), or `off`. Setting `RCMAKE_NO_PROBE_CACHE=1` or passing `--no-probe-cache` also turns the cache off.
- An optional HTTPS or S3 remote cache is configured via `RCMAKE_CACHE_REMOTE`. See [ENV_VARS.md](ENV_VARS.md#remote-probe-cache).

By default, `rcmake` runs up to `min(4, logical CPUs)` compile probes in parallel (`RCMAKE_PROBE_JOBS`, capped at 32 and by available memory). Speculative lookahead runs only literal compile checks (`check_include_file`, `check_c_compiler_flag`, `check_cxx_compiler_flag`, `check_symbol_exists`, and `try_compile` with `SOURCE_FROM_CONTENT`), never `try_run` or `execute_process`. Set `RCMAKE_PROBE_JOBS=1` to run probes in series.

## How does incremental configure replay work, and when is it disabled?

When you reconfigure an existing build tree after editing a file in native mode, `rcmake` re-executes edited directory regions and replays unchanged subtrees from the previous configure snapshot.

A region replays only when its entry context (executable identity, policies, diagnostics, generator, toolchain, and configure options) and all recorded inputs match. Its recorded variable, cache, and property changes are applied again in the original order.

Replay disables itself for the whole invocation when any of these are active:

- A toolchain file (`--toolchain` or `CMAKE_TOOLCHAIN_FILE`, including `vcpkg`)
- A configure preset, `-C`, or `PreLoad.cmake`
- `--fresh`, trace options, `--profile`, or `RCMAKE_PROFILE`
- `--hermetic`, `RCMAKE_PROBE_CACHE=verify`, or `RCMAKE_NO_INCREMENTAL=1`
- `--check-system-vars`, `--log-context`, `--debug-output`, or `--debug-find*`

See [ENV_VARS.md](ENV_VARS.md#incremental-replay) for the complete list.

## What do rcmake explain and --profile do?

`rcmake explain <NAME> -B <build>` traces provenance from a configured build directory:

- For a target, it shows where each compile option, compile definition, include directory, and link library came from, including transitive `INTERFACE` and `PUBLIC` usage chains.
- For a variable, it shows where assignments came from across listfiles, macros, functions, cache entries, and `-D` arguments.
- Pass `--json` to emit machine-readable `rcmake-explain-v1` JSON instead of terminal text.

```sh
rcmake explain app -B build
rcmake explain CMAKE_CXX_FLAGS -B build
rcmake explain app -B build --json
```

`rcmake -S . -B build -G Ninja --profile[=<dir>]` writes `trace.json`, `stacks.folded`, and a self-contained `flamegraph.html` into `<dir>/run-<id>` (default `<build>/rcmake-profile`), with `<dir>/current` holding the latest run directory name. In native mode, `--diagnostics=rich` (the default on a terminal) renders source spans and annotations.

## How do --hermetic and --fetch-lock work?

- **`--hermetic=warn|strict`.** Audits configure inputs. `--hermetic=warn` reports undeclared environment reads, file reads outside allowed roots, subprocess spawns, and network access. `--hermetic=strict` fails configure with a non-zero exit code on undeclared access. Configure allowlists with `RCMAKE_HERMETIC_ALLOW_ENV`, `RCMAKE_HERMETIC_ALLOW_PATHS`, and `RCMAKE_HERMETIC_ALLOW_PROGRAMS`.
- **`--fetch-lock=off|auto|update|frozen`.** Controls `FetchContent` lockfile verification and pinned reuse (default `auto`, lockfile `rcmake-fetch.lock` via `RCMAKE_FETCH_LOCK_FILE`). In native online mode with lock mode active and hermetic mode off, prefetch can start downloading an already-declared `FetchContent_Declare()` source with an archive hash or a full Git commit ID before `FetchContent_MakeAvailable()` runs. Use `RCMAKE_FETCH_OFFLINE=1` for offline builds (and `RCMAKE_FETCH_GIT_READ_ONLY=1` for an immutable Git mirror cache). Compatibility mode never prefetches.

## Does rcmake include a CMake language server (rcmake-lsp)?

Yes. The same multi-call `rcmake` binary runs a stdio Language Server Protocol server for `CMakeLists.txt` and `*.cmake` files when started via the `rcmake-lsp` symlink or `rcmake lsp` (supporting `--help` and `--version`). Release archives and both install scripts (`scripts/install.sh` and `scripts/install.ps1`) install `rcmake-lsp` next to `rcmake`.

`rcmake-lsp` provides hover, completion, go to definition, document symbols, semantic tokens, and indentation formatting. Built with `--features eval,stdio` (the release default), it evaluates unsaved editor buffers in memory via a `Host` overlay with subprocesses disabled so hover shows evaluated variables, targets, and per-configuration generator expressions. A build with `--features stdio` alone runs as a lexical-only server.

See [crates/rcmake-lsp/README.md](../crates/rcmake-lsp/README.md) for VS Code, Neovim (0.11+ and `nvim-lspconfig`), Helix, Zed, Emacs (Eglot), and CLion (LSP4IJ) setup instructions.

## Can I run rcmake in WebAssembly or embed it in Rust?

Yes. All 35 crates access files, environment variables, processes, and clocks through the `rcmake_host::Host` trait (`NativeHost` on OS targets and in-memory `PureHost` for tests and WebAssembly).

- **Browser playground.** Open [https://dvgyl.github.io/rcmake/playground.html](https://dvgyl.github.io/rcmake/playground.html) to run `rcmake` client-side as WebAssembly with no server. You can edit `CMakeLists.txt` and `CMakePresets.json`, generate `Ninja` and `Ninja Multi-Config` build files, inspect diagnostics, and run `rcmake explain`.
- **Rust embedding.** Depend on `rcmake-api` via Git (it is not published on crates.io). With `default-features = false`, `rcmake-api` compiles for `wasm32-unknown-unknown` and `wasm32-wasip1` with no C dependencies. See [crates/rcmake-api/README.md](../crates/rcmake-api/README.md).

## Does rcmake use unsafe Rust or copied CMake C++ code, and what is the licence?

- No CMake C++ source is copied or translated.
- All 35 `rcmake` crates enforce `#![forbid(unsafe_code)]`. Some Rust dependencies (such as `rustix` and `memchr`) use `unsafe` internally, and the native binary statically links two C compression codecs: zstd 1.5.7 and liblzma 5.8.
- Rust code in `crates/` and Python tools in `tools/` are dual-licensed `MIT OR Apache-2.0`.
- Bundled CMake 4.5.1 `Modules/`, `Templates/`, and rendered `Help/` files under `modules/cmake-4.5/` remain under `BSD-3-Clause` with all original Kitware and contributor copyright notices intact. See [PRIOR_ART.md](PRIOR_ART.md) and [THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md).

## How do I install rcmake and alias cmake, ctest, and cpack?

Prebuilt GitHub Releases archives target `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, and `x86_64-pc-windows-msvc`. Intel Macs (`x86_64-apple-darwin`) build from source.

### Linux and macOS

Install `rcmake`, `rcmake-lsp`, `share/cmake-4.5`, shell completions (Bash, Zsh, Fish, PowerShell), and the `rcmake(1)` man page into `~/.local` (or `$PREFIX`), checking SHA-256 sums:

```sh
curl -fsSL https://raw.githubusercontent.com/dvgyl/rcmake/main/scripts/install.sh | bash
```

To also symlink `cmake`, `ctest`, and `cpack` to `rcmake`, pass `--alias-cmake` and place `~/.local/bin` first on `PATH`:

```sh
curl -fsSL https://raw.githubusercontent.com/dvgyl/rcmake/main/scripts/install.sh | bash -s -- --alias-cmake
```

To verify GitHub build provenance for a downloaded archive or `SHA256SUMS.txt`, run:

```sh
gh attestation verify rcmake-<version>-<target>.tar.gz --repo dvgyl/rcmake
```

### Windows

Install into `%LOCALAPPDATA%\rcmake` on PowerShell 5.1 or PowerShell 7+:

```powershell
irm https://raw.githubusercontent.com/dvgyl/rcmake/main/scripts/install.ps1 | iex
```

To pass `-AliasCmake`, `-Prefix`, `-Version`, `-Archive`, `-Force`, or `-Uninstall`, download `scripts/install.ps1` and run `.\install.ps1 -AliasCmake`. You can also extract the `x86_64-pc-windows-msvc` ZIP archive from [GitHub Releases](https://github.com/dvgyl/rcmake/releases) and keep its `bin` folder next to `share\cmake-4.5`.

### From source

Build and install from a checkout (needs Rust 1.99.1 pinned in `rust-toolchain.toml`, plus Python 3):

```sh
git clone https://github.com/dvgyl/rcmake.git && cd rcmake
scripts/install.sh --from-source
```

### Uninstall

Run `scripts/install.sh --uninstall` with the same `PREFIX`. Uninstall keeps the probe and FetchContent cache until you remove it (`~/Library/Caches/rcmake` on macOS, `$XDG_CACHE_HOME/rcmake` or `~/.cache/rcmake` on Linux, `%LOCALAPPDATA%\rcmake\cache` on Windows).

## What is intentionally different or not implemented in rcmake?

- **No interactive front ends.** `cmake-gui` and `ccmake` are not implemented.
- **No DAP debugger.** `cmake --debugger` is not supported, and `cmake -E capabilities` reports `"debugger":false`.
- **No legacy extra generators.** CodeBlocks, CodeLite, Eclipse CDT4, Kate, and Sublime Text 2 are omitted.
- **No legacy or niche generators.** `NMake Makefiles`, `NMake Makefiles JOM`, `MSYS Makefiles`, `MinGW Makefiles`, `Borland Makefiles`, `Watcom WMake`, `Green Hills MULTI`, `FASTBuild`, and Visual Studio 14, 15, and 16 are not implemented.
- **CPack platform installers.** Archive generators, `External`, macOS `DragNDrop` and `Bundle`, and Linux `DEB` and `RPM` packages are implemented. `NSIS`, `WIX`, `productbuild`, and `IFW` are not implemented.
- **Reproducible archives.** `cmake -E tar` and CPack archive generators always write uid 0 and gid 0 instead of host user IDs.
- **In-process `try_compile`.** Probes execute through `rcmake-build` rather than Ninja, Make, MSBuild, or xcodebuild. Build tool output lines in `OUTPUT_VARIABLE` differ, and MSBuild-only or Xcode-only settings (`CMAKE_GENERATOR_TOOLSET`, `CMAKE_XCODE_ATTRIBUTE_*`) do not apply to probes.
- **`--version` line 2.** Line 1 prints `cmake version 4.5.1`. Line 2 prints `rcmake 0.1.0` instead of the Kitware maintainer line.
- **`CMAKE_STRIP` install guard.** `cmake --install --strip` writes a strip rule only when `CMAKE_STRIP` is not a CMake false value (`OFF`, `NOTFOUND`, `strip-NOTFOUND`).
- **Partial language verification.** Swift toolchain builds are unverified, CUDA has one ecosystem fixture, and HIP has rule wiring only.

## How do I check parity on my project or report a bug?

Run your configure with `RCMAKE_ORACLE_MODE=1` or via the `cmake` alias to disable native extensions, then compare `CMakeCache.txt`, generated build files, or File API replies against CMake 4.5.1. To check probe cache soundness, set `RCMAKE_PROBE_CACHE=verify` or `RCMAKE_NO_PROBE_CACHE=1`.

The [compatibility report](https://dvgyl.github.io/rcmake/dashboard.html) shows the latest verification results. Report any difference from CMake 4.5.1 using the [parity bug form](https://github.com/dvgyl/rcmake/issues/new?template=bug_report.yml).
