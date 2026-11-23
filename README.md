# Rcmake

**Repository:** [github.com/dvgyl/Rcmake](https://github.com/dvgyl/Rcmake)

**Rcmake** is a native Rust drop-in replacement for **CMake 4.5.1**, targeting drop-in parity for `cmake`, `ctest`, and `cpack` with fast startup, safe memory handling, deterministic host abstraction, and full modern CMake 4.x compatibility.

## Target Compatibility

- **Reference oracle:** Official **CMake 4.5.1** (released November 2026)
- **Removed pre-3.5 legacy baggage:** Follows CMake 4.0+ semantics (rejects `cmake_minimum_required` below `3.5` unless overridden via `CMAKE_POLICY_VERSION_MINIMUM`; `CMP0000`–`CMP0065` OLD behaviors removed)
- **Modern 4.x features in scope from Day 1:**
  - Policies `CMP0066` through `CMP0219+`
  - Unified diagnostics (`cmake_diagnostic()`, `CMD_NON_TARGET_DIRECTIVE`, `CMD_STRICT`, `-Wstrict`, `--sarif-output`)
  - CMake Presets **schema v13** (`${configurePresetName}`, configure/build/test/package/workflow presets)
  - File-Based API (`codemodel` **v2.12**, `cache-v2`, `toolchains-v1`, `cmakeFiles-v1`, `configureLog-v1`)
  - Common Package Specification (**CPS**), `CMAKE_INSTALL_SBOM_FORMATS`, and Instrumentation API
  - C++20/23 named modules (`CXX_MODULES`, `CMAKE_CXX_STDLIB_MODULES_JSON`, Ninja `dyndep`)
  - 4.4–4.5 commands & signatures: `add_custom_rule()`, `discover_tests(BUILD_DEPENDS ...)`, `cmake_language(PRINT_PROPERTIES / PRINT_TARGETS)`, `SOURCES` file sets, `cmake_parse_arguments(PARSE_ARGN)`, callback predicates in `list(FILTER/SORT/TRANSFORM)`
  - Multi-call CLI covering `cmake` (`-P`, `-E`, configure/generate, `--build`, `--install`, `--workflow`, `--find-package`), `ctest`, and `cpack`

## Licensing Strategy (Pragmatic Permissive)

- **Native Rust crates & tooling (`crates/*`, `tools/*`):** Dual-licensed under **`MIT OR Apache-2.0`**, written cleanly from scratch in safe Rust.
- **Standard CMake module library (`Modules/`, `Templates/`):** Vendored from CMake 4.5.1 under its permissive **`BSD-3-Clause`** license with full copyright notices preserved in `THIRD_PARTY_NOTICES.md` and `third_party/licences/`.
- **Oracle & conformance:** Verified continuously against the official CMake 4.5.1 binary, CMake's `Tests/RunCMake/*` corpus, clean-room regression suites, and real-world C/C++ open-source projects.

## Planned Repository Layout

```text
Rcmake/
├── AGENTS.md                 # Core agent operating instructions & guardrails
├── README.md                 # This file
├── PROGRESS.md               # Live parity scoreboard (created in Milestone 0)
├── THIRD_PARTY_NOTICES.md    # BSD-3-Clause & crate attributions (Milestone 0)
├── Cargo.toml                # Rust workspace manifest (Milestone 0)
├── crates/                   # Disjoint Rust crates (see docs/ARCHITECTURE.md)
├── modules/                  # Vendored BSD-3-Clause CMake 4.5.1 Modules/ & Templates/
├── conformance/              # Clean-room differential cases & fixtures
├── tools/                    # Oracle harness, licence scanner, corpus runner
├── scripts/                  # Mechanical gates & git hooks
└── docs/
    ├── MODEL_ROUTING.md      # Multi-model orchestration & quota policy
    ├── ARCHITECTURE.md       # Workspace crates, contracts, CMake 4.5.1 traps & oracles
    ├── ROADMAP.md            # Milestones M0–M4 & real-world C/C++ corpus ladder
    ├── KICKOFF_PROMPT.md     # Copy-paste starter prompt for the new thread
    ├── ADR.md                # Architectural Decision Records (created in M0)
    └── PRIOR_ART.md          # Dependency & licence log (created in M0)
```

## Documentation Index

- Start here for agent rules: [`AGENTS.md`](AGENTS.md)
- Model & subscription routing: [`docs/MODEL_ROUTING.md`](docs/MODEL_ROUTING.md)
- Technical architecture & crate map: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)
- Milestones & kickoff plan: [`docs/ROADMAP.md`](docs/ROADMAP.md)
- Ready-to-paste first prompt: [`docs/KICKOFF_PROMPT.md`](docs/KICKOFF_PROMPT.md)
