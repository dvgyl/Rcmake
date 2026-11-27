# rcmake FAQ

Direct answers on CMake 4.5.1 parity, native developer tooling, caching, installation, and limits in `rcmake` 0.2.0.

## Contents

- [Is rcmake a drop-in replacement for CMake 4.5.1 in Rust?](#is-rcmake-a-drop-in-replacement-for-cmake-451-in-rust)
- [What is the difference between compatibility mode and native mode?](#what-is-the-difference-between-compatibility-mode-and-native-mode)
- [Which generators, languages, and CMake 4.5.1 interfaces are supported?](#which-generators-languages-and-cmake-451-interfaces-are-supported)
- [Does rcmake work with vcpkg, Conan, find_package, and FetchContent?](#does-rcmake-work-with-vcpkg-conan-find_package-and-fetchcontent)
- [How do the probe cache, portable archives, and drift checks work?](#how-do-the-probe-cache-portable-archives-and-drift-checks-work)
- [How does incremental configure replay work in 0.2.0?](#how-does-incremental-configure-replay-work-in-020)
- [What native developer tools does rcmake include?](#what-native-developer-tools-does-rcmake-include)
- [Does rcmake include a language server, WebAssembly build, and Rust API?](#does-rcmake-include-a-language-server-webassembly-build-and-rust-api)
- [Does rcmake use unsafe Rust or copied CMake C++ code, and what is the licence?](#does-rcmake-use-unsafe-rust-or-copied-cmake-c-code-and-what-is-the-licence)
- [How do I install rcmake and alias cmake, ctest, cpack, and ccmake?](#how-do-i-install-rcmake-and-alias-cmake-ctest-cpack-and-ccmake)
- [What is intentionally different or not implemented in rcmake?](#what-is-intentionally-different-or-not-implemented-in-rcmake)
- [How do I check parity on my project or report a bug?](#how-do-i-check-parity-on-my-project-or-report-a-bug)

## Is rcmake a drop-in replacement for CMake 4.5.1 in Rust?

Yes. `rcmake` is an independent reimplementation of CMake 4.5.1 in safe Rust with no copied or translated CMake C++ code, and the core engine never calls an upstream `cmake` binary at runtime. One multi-call binary provides `cmake`, `ccmake` on Unix, `ctest`, `cpack`, and `rcmake-lsp`, selecting its tool from `argv[0]`, while the isolated sibling binary `rcmake-verify` runs explicit side-by-side comparisons when you invoke `rcmake verify`.

`rcmake` reads existing `CMakeLists.txt` files, Presets v13 (`CMakePresets.json` and `CMakeUserPresets.json`), and `CMakeCache.txt`, then writes compatible build trees. `rcmake` is not affiliated with Kitware, and CMake is a trademark of Kitware, Inc.

## What is the difference between compatibility mode and native mode?

`rcmake` chooses its mode from `argv[0]`.

- **Compatibility mode (`cmake`, `ccmake`, `ctest`, `cpack`, or `RCMAKE_ORACLE_MODE=1`).** Flags, output, diagnostics, and exit codes match CMake 4.5.1, subject to the [documented limits](#what-is-intentionally-different-or-not-implemented-in-rcmake). Native subcommands and flags are rejected, and no native metadata files are written.
- **Native mode (`rcmake`).** Enables the persistent probe cache, parallel probes, sound incremental replay, drift checks, multi-config `compile_commands.json` export, `explain`, `diff-config`, `fmt`, `lint`, `verify`, `doctor`, `cache`, `--profile`, `--diagnostics=rich`, `--hermetic`, and `--fetch-lock`. Build-time regenerations of a native build directory preserve native mode automatically.

## Which generators, languages, and CMake 4.5.1 interfaces are supported?

- **Generators.** `Ninja` and `Ninja Multi-Config` on macOS, Linux, and Windows, `Unix Makefiles` on macOS and Linux, `Xcode` on macOS, and `Visual Studio 17 2022` and `Visual Studio 18 2026` on Windows.
- **Languages.** C (C90 to C23), C++ (including C++20 and C++23 named modules with `CXX_MODULES` and Ninja `dyndep`), Objective-C, Objective-C++, Fortran with `.mod` scanning, CUDA (NVCC and Clang, separable compilation, device linking), HIP (AMD ROCm, SPIR-V, and NVIDIA), Swift (`CMP0157` split compilation, `CMP0195`, `CMP0215`, `CMP0216`), `ASM`, `ASM_MASM`, `ASM_NASM`, Windows `RC`, and Qt `AUTOMOC`, `AUTOUIC`, and `AUTORCC`.
- **Command-line tools.** `cmake` (configure, generate, `-P`, `-E`, `--build`, `--install`, `--workflow`, `--find-package`, `--preset`, `--sarif-output`, and `--debugger` DAP server on Unix), `ccmake` (clean-room Rust TUI on macOS and Linux), `ctest` (parallel runs, GNU make jobserver, fixtures, resources, labels, repeat modes, JUnit and dashboard XML, `-S` scripts), and `cpack` (archive formats `TGZ`, `TXZ`, `TZST`, `TBZ2`, `TZ`, `TAR`, `STGZ`, `ZIP`, `7Z`, Linux `DEB` and `RPM`, `External`, macOS `Bundle`, `DragNDrop`, and `productbuild`, Windows `WIX` v3 and v4, and cross-platform `NSIS`, `NSIS64`, and `IFW`).
- **Interfaces.** Policies `CMP0000` to `CMP0226` (`CMP0000` to `CMP0065` always NEW, 161 active policies from `CMP0066` to `CMP0226`), Presets v13, File API (`codemodel` v2.12, `cache` v2, `toolchains` v1, `cmakeFiles` v1, `configureLog` v1), CPS 0.15.0, SPDX 3.0.1 SBOM, Instrumentation API v1, `cmake_diagnostic()` with SARIF 2.1.0, and `compile_commands.json`.

## Does rcmake work with vcpkg, Conan, find_package, and FetchContent?

Yes. `rcmake` bundles CMake 4.5.1 `Modules/`, `Templates/`, and rendered `Help/` under `share/cmake-4.5` and runs `vcpkg` toolchain files, Conan (`CMakeToolchain` and `CMakeDeps`), `find_package()`, and `FetchContent` directly. In `0.2.0`, sound incremental replay also stays active with `vcpkg` toolchains and Conan configure presets by running setup hooks live and validating setup checkpoints.

## How do the probe cache, portable archives, and drift checks work?

In native mode, `rcmake` stores verified compiler checks (`try_compile`, `check_include_file`, `check_symbol_exists`) in a shared content-addressed cache under `JournalHost`. Before reusing an entry, it checks the SHA-256 digest of every recorded input file, including the compiler executable, clang `.cfg` files, and GCC `specs`. By default it runs up to `min(4, logical CPUs)` literal compile probes concurrently (`RCMAKE_PROBE_JOBS`, never `try_run` or `execute_process`). Set `RCMAKE_PROBE_CACHE=verify` in CI to check hits against fresh probes, or configure an optional HTTPS or S3 remote cache via `RCMAKE_CACHE_REMOTE`.

To share probes across CI runners without a remote server, run `rcmake cache export probes.tar.gz` and import on downstream jobs with `rcmake cache import probes.tar.gz --sha256 <digest>` (trusted archives publish live, while imports without `--sha256` stage as untrusted candidates that never trigger hits). To detect stale `CMakeCache.txt` drift when compilers, toolchain files, launchers, or `CMAKE_PREFIX_PATH` change, native configures automatically warn on drift and `rcmake doctor -B <build>` reports drift offline without running compilers. See [CACHING.md](CACHING.md) for full details.

## How does incremental configure replay work in 0.2.0?

On a warm native reconfigure, `rcmake` re-executes edited directory regions and replays unchanged subtrees from `CMakeFiles/rcmake/configure.v1.bin` whenever entry context and recorded inputs match. Starting in `0.2.0`, toolchain files (`vcpkg`), configure presets, `-C` initial cache scripts, and `PreLoad.cmake` no longer disable replay globally. Instead, setup spans run live on every configure and hash observable interpreter state into setup checkpoints so any setup change disables replay hits safely.

Replay turns off for the invocation when `--fresh`, `--debugger`, `--trace*`, `--profile`, `--hermetic`, `RCMAKE_PROBE_CACHE=verify`, `RCMAKE_NO_INCREMENTAL=1`, or debug flags are active, or when `RCMAKE_REPLAY_SETUP=0` is set with setup hooks. See [ENV_VARS.md](ENV_VARS.md#incremental-replay).

## What native developer tools does rcmake include?

See [TOOLING.md](TOOLING.md) for examples and every flag.

- **`rcmake explain`.** Traces target flags (`--flag`), definitions (`--define`), includes (`--include`), dependency paths (`--dep`), variable writes (`--var`), `find_package()` rejections (`--find`, `why-not`), and generator expressions (`--eval`, `--eval-property`) in memory with optional `--json`.
- **`rcmake diff-config <A> <B>`.** Compares any two `rcmake` or official `cmake` build directories (or presets) across targets, compile, link, toolchain, packages, and cache without running `cmake`.
- **`rcmake fmt` and `rcmake lint`.** Lossless concrete-syntax formatter (`--check`, `--diff`, `--stdin`) and 8-rule static linter (`--sarif`, `--list-rules`, `--deny-warnings`, `# rcmake-lint: disable=<rule>`), configured by `.rcmake.toml` and shared with `rcmake-lsp`.
- **`rcmake verify`.** Runs `rcmake` and a user-supplied CMake 4.5.1 binary side by side inside the isolated `rcmake-verify` companion on staged copies of the project.
- **Multi-config `compile_commands.json`.** `--export-compile-commands` or `CMAKE_EXPORT_COMPILE_COMMANDS=ON` in native mode writes `compile_commands.json` plus per-config `compile_commands_<Config>.json` on `Ninja Multi-Config`, `Xcode`, and `Visual Studio`.
- **`--profile`, `--hermetic`, `--fetch-lock`, and `--diagnostics=rich`.** Configure flame graphs, input audits, FetchContent lockfiles, and source-span terminal errors.

## Does rcmake include a language server, WebAssembly build, and Rust API?

Yes.

- **Language server (`rcmake-lsp` or `rcmake lsp`).** Runs over stdio with hover showing in-memory evaluated values (in `--features eval,stdio` release builds), completion, go to definition, document symbols, semantic tokens, quick fixes, lossless `.rcmake.toml` formatting, and all 8 static `rcmake lint` rules. See [crates/rcmake-lsp/README.md](../crates/rcmake-lsp/README.md) for VS Code, Neovim, Helix, Zed, Emacs, and CLion setup.
- **Browser playground.** [https://rcmake.github.io/rcmake/playground.html](https://rcmake.github.io/rcmake/playground.html) runs `rcmake` as WebAssembly on `PureHost` with no backend server.
- **Embedded Rust API.** `rcmake-api` embeds configure and `explain` in Rust applications and compiles to `wasm32-unknown-unknown` and `wasm32-wasip1` with `default-features = false` (no C dependencies). See [crates/rcmake-api/README.md](../crates/rcmake-api/README.md).

## Does rcmake use unsafe Rust or copied CMake C++ code, and what is the licence?

`rcmake` contains no copied or translated CMake C++ code, and all 39 workspace crates enforce `#![forbid(unsafe_code)]`. External Rust dependencies such as `rustix` and `memchr` use `unsafe` internally, and the native binary statically links zstd 1.5.7 and liblzma 5.8. Rust and Python code in `crates/` and `tools/` is dual-licensed `MIT OR Apache-2.0`. Bundled CMake 4.5.1 `Modules/`, `Templates/`, `Help/`, and five derived tables stay under `BSD-3-Clause` with original copyright notices intact. See [PRIOR_ART.md](PRIOR_ART.md) and [THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md).

## How do I install rcmake and alias cmake, ctest, cpack, and ccmake?

Prebuilt releases cover `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, and `x86_64-pc-windows-msvc`. Intel Macs build from source.

- **Linux and macOS.** Run `curl -fsSL https://raw.githubusercontent.com/rcmake/rcmake/main/scripts/install.sh | bash` to install `rcmake`, `rcmake-lsp`, `rcmake-verify`, `share/cmake-4.5`, shell completions, and `rcmake(1)` plus `ccmake(1)` into `~/.local`. Add `-s -- --alias-cmake` after `bash` to symlink `cmake`, `ctest`, `cpack`, and `ccmake` to `rcmake`, and place `~/.local/bin` first on `PATH`. Verify archive provenance with `gh attestation verify rcmake-<version>-<target>.tar.gz --repo rcmake/rcmake`.
- **Windows.** Run `irm https://raw.githubusercontent.com/rcmake/rcmake/main/scripts/install.ps1 | iex` on PowerShell 5.1 or newer, or download `install.ps1` and run `.\install.ps1 -AliasCmake` (for `cmake`, `ctest`, and `cpack`).
- **From source.** Run `git clone https://github.com/rcmake/rcmake.git && cd rcmake && scripts/install.sh --from-source` with Rust 1.99.1 and Python 3.
- **Uninstall.** Run `scripts/install.sh --uninstall` (or `install.ps1 -Uninstall`). Delete the local cache directory separately if you want to reclaim cache space.

## What is intentionally different or not implemented in rcmake?

- **Windows `ccmake` and Windows `--debugger`.** Both `ccmake` and `--debugger` DAP server work on macOS and Linux in `0.2.0`. Windows terminal UI and Windows named-pipe DAP transport are deferred, and Windows builds report `"debugger": false` in `-E capabilities`.
- **No `cmake-gui`.** Graphical desktop setup is not included.
- **External tools for CPack installers and `rcmake verify`.** CPack installer generators invoke the host packaging binary (`pkgbuild` and `productbuild`, `makensis`, `wix` or `candle` and `light`, `binarycreator`), and `rcmake verify` requires a user-supplied CMake 4.5.1 binary.
- **Omitted legacy and niche generators.** Legacy extra generators (CodeBlocks, CodeLite, Eclipse CDT4, Kate, Sublime Text 2), older Makefile variants (`NMake`, `MSYS`, `MinGW`, `Borland`, `Watcom`), `Green Hills MULTI`, `FASTBuild`, and Visual Studio 14 to 16 are omitted.
- **GPU and Swift CI coverage.** CUDA, HIP, and Swift target lowering and build-graph generation are implemented, while standard CI runners lack GPU hardware and full Swift or HIP toolchains.
- **Reproducible archives.** `cmake -E tar` and CPack archive writers record uid 0 and gid 0 for deterministic output.
- **In-process `try_compile`.** Probes run through `rcmake-build` instead of Ninja, Make, MSBuild, or xcodebuild, so MSBuild-only and Xcode-only attributes (`CMAKE_GENERATOR_TOOLSET`, `CMAKE_XCODE_ATTRIBUTE_*`) do not affect probes.
- **`--version` line 2.** Line 1 prints `<tool> version 4.5.1` and line 2 prints `rcmake 0.2.0` instead of Kitware's maintainer line.
- **`CMAKE_STRIP` install guard.** `cmake --install --strip` emits a strip step only when `CMAKE_STRIP` is not a CMake false value.

## How do I check parity on my project or report a bug?

Run `rcmake verify --oracle /path/to/cmake-4.5.1 -- -S . -G Ninja` for an automated staged comparison of configure status, File API codemodel, `CMakeCache.txt`, `compile_commands.json`, and Ninja dry-run output. You can also compare two existing trees with `rcmake diff-config`, isolate native extensions with `RCMAKE_ORACLE_MODE=1`, or verify probe cache hits with `RCMAKE_PROBE_CACHE=verify`.

Check the [compatibility report](https://rcmake.github.io/rcmake/dashboard.html) for tracked suites, and report any difference with the [parity bug form](https://github.com/rcmake/rcmake/issues/new?template=bug_report.yml).
