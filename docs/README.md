# rcmake documentation

Use this index to find guides, flag references, architecture notes, and verification details for `rcmake` 0.2.1.

## Contents

- [Getting started](#getting-started)
- [Native tooling and workflows](#native-tooling-and-workflows)
- [Compatibility and caching](#compatibility-and-caching)
- [Architecture and design](#architecture-and-design)
- [Verification and contributing](#verification-and-contributing)
- [Website and references](#website-and-references)

## Getting started

- [README](../README.md). Overview, quick start, release scorecard, and install options.
  - [Quick start](../README.md#quick-start)
  - [Install](../README.md#install)
  - [Modes and troubleshooting](../README.md#modes-and-troubleshooting)
  - [Intentional differences and limits](../README.md#intentional-differences-and-limits)
- [FAQ](FAQ.md). Direct answers on CMake 4.5.1 parity, generators, package managers, caching, native tools, and limits.
  - [Compatibility mode versus native mode](FAQ.md#what-is-the-difference-between-compatibility-mode-and-native-mode)
  - [Generators, languages, and interfaces](FAQ.md#which-generators-languages-and-cmake-451-interfaces-are-supported)
  - [Intentional differences and limits](FAQ.md#what-is-intentionally-different-or-not-implemented-in-rcmake)
- [Shell completions](../completions/README.md). Setup for Bash, Zsh, Fish, and PowerShell.
- Manual pages for [rcmake(1)](man/rcmake.1) and [ccmake(1)](man/ccmake.1).

## Native tooling and workflows

- [Native developer tooling](TOOLING.md). Complete guide to opt-in `rcmake` workflow commands.
  - [`rcmake explain`](TOOLING.md#provenance-and-inspection-with-rcmake-explain) for flags, definitions, includes, dependency paths, `find_package`, and generator expressions
  - [`rcmake diff-config`](TOOLING.md#build-tree-comparison-with-rcmake-diff-config) for build directory and preset comparison
  - [`rcmake fmt` and `rcmake lint`](TOOLING.md#lossless-formatting-and-static-linting) for lossless formatting, 8 lint rules, and `.rcmake.toml`
  - [`rcmake verify`](TOOLING.md#side-by-side-parity-checks-with-rcmake-verify) for isolated parity checks with `rcmake-verify`
  - [Multi-configuration compile databases](TOOLING.md#multi-configuration-compile-databases)
  - [`ccmake` and `--debugger` DAP server](TOOLING.md#interactive-cache-editor-and-dap-debugger)
  - [Profiling, hermetic audits, and FetchContent lockfiles](TOOLING.md#profiling-hermetic-audits-fetchcontent-lockfiles-and-diagnostics)
- [Language server setup](../crates/rcmake-lsp/README.md). Editor guides for VS Code, Neovim, Helix, Zed, Emacs, and CLion.
- [Embedded Rust and WebAssembly API](../crates/rcmake-api/README.md). Embed configure and `explain` inside your own Rust program.

## Compatibility and caching

- [Caching, replay, and drift detection](CACHING.md). Portable probe caches, sound incremental replay, and stale cache tracking.
  - [Probe cache archives](CACHING.md#probe-cache-archives) for `rcmake cache export` and `rcmake cache import`
  - [CI recipe with `actions/cache`](CACHING.md#ci-recipe-with-actionscache)
  - [Incremental configure replay](CACHING.md#incremental-configure-replay) under toolchains, presets, `-C`, and `PreLoad.cmake`
  - [Drift checks](CACHING.md#drift-checks) for `rcmake doctor -B <dir>` and warm-configure warnings
- [Environment variables](ENV_VARS.md). Complete `RCMAKE_*` reference and precedence rules.
  - [Configure, probe cache, and incremental replay](ENV_VARS.md#configure-probe-cache-and-incremental-replay)
  - [Remote probe cache](ENV_VARS.md#remote-probe-cache)
  - [FetchContent, hermetic checks, and archive extraction](ENV_VARS.md#fetchcontent-hermetic-checks-and-archive-extraction)
  - [Command-line output, CPack, and data discovery](ENV_VARS.md#command-line-output-cpack-and-data-discovery)
  - [Parity verification and conformance harness](ENV_VARS.md#parity-verification-and-conformance-harness)

## Architecture and design

- [Architecture](ARCHITECTURE.md). Execution pipeline, resolution layers, all 40 crates, `Host` trait, and native stack guard.
  - [Execution pipeline](ARCHITECTURE.md#execution-pipeline)
  - [Workspace crate map](ARCHITECTURE.md#workspace-crate-map)
  - [The `Host` abstraction](ARCHITECTURE.md#the-host-abstraction)
  - [CMake 4.5.1 invariants](ARCHITECTURE.md#cmake-451-invariants)
  - [Native stack use](ARCHITECTURE.md#native-stack-use)
- [Architecture decision records](ADR.md). 48 ADRs documenting storage, generators, host isolation, replay checkpoints, `ccmake`, DAP, Windows launcher shims, and native tooling.
- [Prior art and dependency licence log](PRIOR_ART.md). Vendored CMake 4.5.1 assets, direct Rust dependencies, archive codecs, and Windows libraries.
  - [Vendored third-party assets](PRIOR_ART.md#vendored-third-party-assets)
  - [Direct third-party dependencies](PRIOR_ART.md#direct-third-party-dependencies)
  - [Internal workspace crates](PRIOR_ART.md#internal-workspace-crates)

## Verification and contributing

- [Build write-up](BUILD_WRITEUP.md). Differential oracle methodology, hard bugs uncovered, frozen scorecard, and reproduction commands.
  - [The differential oracle method](BUILD_WRITEUP.md#the-differential-oracle-method)
  - [Hard bugs differential testing exposed](BUILD_WRITEUP.md#hard-bugs-differential-testing-exposed)
  - [Scorecard and limitations](BUILD_WRITEUP.md#scorecard-and-limitations)
  - [One command for each claim](BUILD_WRITEUP.md#one-command-for-each-claim)
- [Contributing](../CONTRIBUTING.md). Coding standards, clean-room boundary, and local gate commands.
- [Contributors](../CONTRIBUTORS.md). Maintainers and attributions.
- [Security policy](../SECURITY.md). Vulnerability reporting process.
- [Third-party notices](../THIRD_PARTY_NOTICES.md). Full attribution and licence texts.

## Website and references

- [Website overview](https://rcmake.github.io/rcmake/). Product summary, CMake 4.5.1 comparison, and common questions.
- [Browser playground](https://rcmake.github.io/rcmake/playground.html). Run configure, Ninja generation, diagnostics, and `rcmake explain` in WebAssembly.
- [Compatibility report](https://rcmake.github.io/rcmake/dashboard.html). Interactive results across `Tests/RunCMake`, large C++ projects, `vcpkg`, holdout, and benchmarks.
- Machine-readable references at [site/llms.txt](../site/llms.txt), [llms.txt](https://rcmake.github.io/rcmake/llms.txt), and [llms-full.txt](https://rcmake.github.io/rcmake/llms-full.txt).
