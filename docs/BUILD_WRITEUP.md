# Building rcmake, an independent CMake 4.5.1 reimplementation in Rust

`rcmake` is an independent reimplementation of CMake 4.5.1 in Rust. It contains no copied or translated CMake C++ source code, and it never calls an upstream `cmake` binary at runtime. One multi-call binary provides `cmake`, `ctest`, `cpack`, and the Language Server Protocol server (`rcmake-lsp` symlink or `rcmake lsp`), selecting its tool from `argv[0]`.

The goal is drop-in compatibility. A project should configure, build, install, test, and package with `rcmake` first on `PATH` and produce the same outputs as CMake 4.5.1.

## 1. Motivation, scope, and implementation rules

### Why target one exact release

CMake has no formal language specification. Its observable behavior is the specification, defined by 227 policies (`CMP0000` to `CMP0226`), subtle `if()` auto-dereference rules, `CMakeCache.txt` serialization details, Ninja quoting rules, and the upstream `Tests/RunCMake` suite. Real build trees depend on those details. A partial CMake-like engine breaks as soon as a project inspects `CMakeCache.txt`, parses `try_compile` output with a regular expression, or reads File API JSON replies.

In CMake 4.x, `CMP0000` to `CMP0065` always use NEW behavior, while `CMP0066` to `CMP0226` are the 161 policies that a project can still set to OLD. The 35 upstream `RunCMake` policy suites exercise policies in this active range, and other suites cover additional policies indirectly.

Pinning one exact release, CMake 4.5.1, gives every design question a single testable answer. Whenever behavior is unclear, we run CMake 4.5.1 and `rcmake` side by side on a small probe and compare their outputs. Fixing the target version keeps that answer stable across the entire project.

The implementation covers these subsystems.

- **`cmake`.** Configure and generate, `-P` script mode, `-E` command-line utilities, `--build`, `--install`, `--workflow`, `--find-package`, presets (schema version 13), and SARIF 2.1.0 diagnostic output.
- **`ctest`.** Parallel test scheduling, fixtures, resource specifications, labels, repeat modes, JUnit and dashboard XML output, `discover_tests()`, and `-S` scripts.
- **`cpack`.** Archive generators (`TGZ`, `TXZ`, `TZST`, `TBZ2`, `TZ`, `TAR`, `STGZ`, `ZIP`, `7Z`, and their compression variants), Linux `DEB` and `RPM`, `External`, and macOS `Bundle` and `DragNDrop`.
- **CMake 4.5.1 interfaces.** Policies `CMP0000` to `CMP0226` (OLD behavior removed for `CMP0000` to `CMP0065`), File API `codemodel` v2.12, `cache` v2, `toolchains` v1, `cmakeFiles` v1, `configureLog` v1, Common Package Specification (CPS) 0.15.0, SPDX 3.0.1 SBOM generation, Instrumentation API v1, `compile_commands.json`, and C++20 and C++23 named modules with Ninja `dyndep`.
- **Native extensions under the `rcmake` name.** Persistent compiler probe cache, incremental configure replay, `rcmake explain`, `--profile`, `--diagnostics=rich`, `--hermetic`, `--fetch-lock`, the in-process `rcmake-api` crate (including WebAssembly targets), and `rcmake-lsp`.

### Why `#![forbid(unsafe_code)]`

A build system parses untrusted inputs, including third-party `CMakeLists.txt` scripts, cache files, presets, downloaded archives, binary headers, and compiler output. Memory corruption on any of those paths becomes a security vulnerability.

The workspace sets `unsafe_code = "forbid"` in [`Cargo.toml`](../Cargo.toml), and every crate root repeats `#![forbid(unsafe_code)]`. Workspace Clippy lints also reject `unwrap_used`, `expect_used`, `panic`, `todo`, `unimplemented`, `indexing_slicing`, `string_slice`, `iter_over_hash_type`, and disallowed `std` file, environment, and process methods. Reachable input paths return structured diagnostics instead of panicking, bounded by explicit limits in `rcmake_base::Limits` on recursion depth, macro expansion, loop iterations, regular expression steps, and file sizes.

This rule governs all 35 `rcmake` crates. External Rust dependencies such as `rustix` and `memchr` use `unsafe` internally, and the native `rcmake` binary statically links two C codecs, zstd 1.5.7 and liblzma 5.8 from XZ Utils. [`docs/PRIOR_ART.md`](PRIOR_ART.md) documents each external dependency.

Rust 2024 marks `std::env::set_var` as `unsafe`, yet CMake scripts mutate `ENV{...}` constantly during configure. Rather than allowing unsafe code, `NativeHost` snapshots the process environment once at startup, tracks script mutations in an in-memory overlay, and passes the merged map explicitly to each child process. As a result, the parent process environment never changes.

### Implementation boundary and licensing

The Rust code in `crates/` and the Python tooling in `tools/` use `MIT OR Apache-2.0`. A small set of tables generated from CMake 4.5.1 help and policy data, such as `crates/rcmake-diag/src/generated_policies.rs`, stays under `BSD-3-Clause` and is listed in [`THIRD_PARTY_NOTICES.md`](../THIRD_PARTY_NOTICES.md) section 1.

A large share of CMake behavior lives in CMake-language standard library modules such as `FindThreads.cmake` and `CMakeDetermineCCompiler.cmake` under `Modules/` and `Templates/`. Projects include those files directly and rely on their internal helper macros and cache variables. Rewriting hundreds of platform modules would create a second, incompatible CMake. Because CMake is licensed under `BSD-3-Clause`, `rcmake` bundles the CMake 4.5.1 `Modules/`, `Templates/`, and rendered `Help/` files verbatim under `modules/cmake-4.5/`, keeps every copyright header intact, and records full attribution in [`THIRD_PARTY_NOTICES.md`](../THIRD_PARTY_NOTICES.md) and [`third_party/licences/CMake-BSD-3-Clause.txt`](../third_party/licences/CMake-BSD-3-Clause.txt).

Three rules define the implementation boundary. First, we never copy or translate CMake C++ code into Rust. Second, `rcmake` never invokes an upstream `cmake` binary at runtime, and only `rcmake-harness` and the measurement scripts under `tools/` read `RCMAKE_ORACLE_CMAKE`. Third, diagnostics match CMake message text, which carries the same `BSD-3-Clause` licence as the bundled modules. Whenever a differential test uncovers an undocumented edge case, we probe CMake's behavior, design the Rust implementation from scratch, and verify it against the oracle binary. [`tools/measure/licence_scan.py`](../tools/measure/licence_scan.py) checks dependency licences against an allowlist, rejects `unsafe` blocks and `unsafe_code` allow attributes in `crates/`, and rejects oracle environment references outside `rcmake-harness`. The script `scripts/gate.sh` runs it on every gate, and `--check-hygiene` adds checks for Git file modes, tracked macOS metadata, and developer home paths.

We do not call this work clean-room in the strict legal sense. Coding models wrote most of the code, and their training data probably includes the public CMake source. When this write-up cites an upstream C++ function name, such as `cmComputeLinkInformation::AddLibraryRuntimeInfo`, it only identifies the behavior that a live probe against CMake 4.5.1 confirmed.

## 2. Architecture across 35 crates and one `Host` trait

The workspace spans 35 safe-Rust crates (`crates/rcmake-*`, including `rcmake-wasm` for the browser playground) and 38 Architecture Decision Records in [`docs/ADR.md`](ADR.md). The dependency graph is acyclic and mirrors CMake's execution stages.

```text
argv[0], presets          rcmake-cli, rcmake-presets
        |
        v
listfiles --> rcmake-syntax --> rcmake-expr --> rcmake-eval + rcmake-commands
        |                      (${}, if())      (scopes, policies, commands)
        v
rcmake-model               ProjectModel: cache, directories, targets, tests
        |
        v
rcmake-genex <--GenexHost--> rcmake-resolve
($<...>)                     ResolvedTarget, usage closure, BuildGraph
        |
        +--> rcmake-gen-ninja, rcmake-gen-make, rcmake-gen-xcode, rcmake-gen-vs
        +--> rcmake-gen-common (File API, install scripts, CTest files, CPS, SBOM)
        +--> rcmake-build (in-process try_compile), rcmake-ctest, rcmake-cpack
```

[`docs/ARCHITECTURE.md`](ARCHITECTURE.md) documents every crate. Five design choices carry most of the architecture.

1. **Byte strings end to end.** CMake variables, paths, and listfiles are raw byte sequences rather than guaranteed UTF-8. `rcmake-base` defines `BString`, CMake list splitting (`;` with `\;` escapes and bracket awareness), CMake truthiness (`is_on` and `is_off`), lexical path helpers, and `Limits`. `rcmake-syntax` preserves raw bytes, argument kinds (quoted, unquoted, bracket), and source spans.
2. **An independent CMake regex engine.** CMake uses the KWSys regular expression dialect, which has no non-greedy quantifiers, distinct anchor rules, and `CMAKE_MATCH_<n>` side effects. `rcmake-regex` implements that dialect in pure Rust with an explicit step budget instead of wrapping the Rust `regex` crate.
3. **Three resolution layers and an acyclic generator-expression seam.** Configure produces an arena-based `ProjectModel` (`rcmake-model`) where values carry backtraces. `rcmake-resolve` evaluates generator expressions per configuration into typed `ResolvedTarget` records (read by File API and export writers) and lowers those targets into a `BuildGraph` of compile, link, and custom-command steps. Because generator expressions such as `$<TARGET_PROPERTY:...>` query transitive usage requirements during resolution, `rcmake-genex` defines a `GenexHost` trait and `rcmake-resolve` implements it, keeping the crate graph acyclic.
4. **Generators only format output.** Ninja, Ninja Multi-Config, Unix Makefiles, Xcode, and Visual Studio 17 and 18 all read the same `BuildGraph`. Compile flags, link lines, precompiled headers, unity sources, runtime paths, and install rules are resolved once in `rcmake-resolve`, so a single resolution fix repairs every generator.
5. **All operating-system access behind `rcmake_host::Host`.** `clippy.toml` forbids engine crates from calling `std::fs`, `std::env`, `std::process`, or the system clock directly. Every file read, write, directory listing, symlink query, file lock, environment lookup, process spawn, stream write, and timestamp goes through `Host`.

### How the `Host` trait paid off, including `rcmake.wasm`

`rcmake-host` provides two implementations verified against the same conformance suite (`rcmake_host::conformance::run_all`).

- `NativeHost` calls the real operating system using `rustix` on Unix and job objects on Windows for process groups and timeouts.
- `PureHost` provides an in-memory file system with symlink resolution, virtual timestamps, scripted subprocesses, and an ordered stdout and stderr log.

Putting every system boundary behind one trait turned six higher-level features into small wrappers instead of invasive rewrites.

- **Deterministic unit tests.** Most engine tests configure projects inside `PureHost` with no temporary directories, no host compiler, and stable error text (`HostError` keeps the raw OS message without Rust's `(os error N)` suffix).
- **Hermetic configure (`--hermetic=warn|strict`).** A host wrapper audits environment reads, file reads, subprocess spawns, and network access at the single boundary the engine can reach.
- **Persistent compiler probe cache.** `JournalHost` wraps `Host` during `try_compile` probes, records every file read, search directory, environment key, and child process, and writes verified stamps to `rcmake-store`.
- **Incremental configure replay.** The same input-recording layer validates unchanged directory regions before replaying them from a previous configure snapshot.
- **Editor buffer isolation.** `rcmake-lsp` evaluates unsaved buffers through a read overlay, routes writes to a scratch overlay, and blocks subprocesses.
- **Real WebAssembly compilation (`rcmake.wasm`).** With `--no-default-features`, `rcmake-api` drops the native archive and disk-store crates (`rcmake-store`, `rcmake-archive`, `zstd`, `liblzma`) and runs on `PureHost`. The configure engine then compiles for both `wasm32-unknown-unknown` and `wasm32-wasip1`. [`tools/measure/wasm_smoke.py`](../tools/measure/wasm_smoke.py) verifies that the Cargo dependency tree contains no native C crates, compiles for `wasm32-unknown-unknown`, and builds `wasm_smoke.wasm` for `wasm32-wasip1`. It then runs that module in Wasmtime to check three configured targets, provenance queries, cancellation, and both permitted and denied `try_compile` calls. [`crates/rcmake-wasm`](../crates/rcmake-wasm) and [`scripts/build-wasm.sh`](../scripts/build-wasm.sh) build this runtime, with no `unsafe` and no `wasm-bindgen`, into `site/wasm/rcmake_wasm.{js,wasm}`. The [browser playground](https://rcmake.github.io/rcmake/playground.html) runs `rcmake` configure, Ninja generation, diagnostics, and `rcmake explain` in the browser with no server.

## 3. Timeline and multi-agent division of work

One human maintainer set the goals, implementation rules, and review gates, and coordinated coding agents across four calendar days.

- **2026-11-23.** Initial specification, workspace bootstrap, `Host` and `rcmake-syntax` contracts, oracle tooling, `rcmake-regex`, `rcmake-expr`, `rcmake-eval`, presets v13, and `-P` script parity.
- **2026-11-24.** Full configure and resolution pipeline, Ninja, Unix Makefiles, Xcode, Visual Studio, File API, `ctest`, `cpack`, CPS, SBOM, C++20 modules, Fortran, CUDA, Qt `AUTOGEN`, persistent probe cache, incremental replay, FetchContent lockfiles, `rcmake-api`, `rcmake-lsp`, and safe-Rust fuzzing.
- **2026-11-25.** Large-project corpus parity, 20 independent black-box scenarios, cross-platform Windows and Linux CI recovery, diagnostic formatting and File API v2.12 alignment, WebAssembly verification, brand assets, static site, and public documentation polish.
- **2026-11-26.** `rcmake-json` replacing `serde_json`, probe and evaluation performance work, shell completions and man page, release receipts, licence notices, and site and documentation polish.

The repository history starts on 2026-11-23 at 08:54 UTC. On 2026-11-26, it holds over 520 commits and `crates/` contains 377,001 lines of Rust across 958 files. The workspace-size command in [section 7](#7-one-command-for-each-claim) prints the live count.

A human did not read most of this code line by line. Cross-model review, the differential oracle, and the test suites in [section 4](#4-the-differential-oracle-method) verified it instead. Treat the measured results as the evidence, not a claim of manual line-by-line inspection.

### How work was divided across models

- **Gemini 4 Neon** acted as coordinator and systems planner, owning shared contracts (`Cargo.toml`, `rcmake-base`, `rcmake-syntax`, `rcmake-diag`, `rcmake-model` public types, and ADRs), triaging differential failures into disjoint crate slices, auditing diffs against the architecture, and authoring documentation and frontend tooling.
- **Opus 5.5** handled deep architectural planning, tricky semantic subsystems, and high-stakes code reviews on large resolution and caching changes.
- **GPT 6.2 Sol** executed most parallel crate implementations, parity sweeps across CMake commands and generators, measurement scripts, and routine diff reviews.
- **Deep-reasoning technical consults** reviewed hard soundness and specification design problems, including probe-cache invalidation invariants, incremental replay boundaries, and cross-platform runtime path semantics.

### Engineering habits that mattered

- **Disjoint worktrees.** Parallel implementation batches touched non-overlapping crates or command families inside separate git worktrees, avoiding merge conflicts while 3 to 5 agents worked concurrently.
- **Cross-model review.** No agent reviewed its own code. Opus 5.5 reviewed large GPT 6.2 Sol architectural changes, GPT 6.2 Sol reviewed Opus 5.5 slices, and CI ran oracle checks on every pull request and every push to `main`.
- **Resource limits on a 16 GB laptop.** Local Cargo runs shared at most two build jobs (`CARGO_BUILD_JOBS=2` and `RUST_TEST_THREADS=3` in `env/env.sh`). Read-only triage, specification checks, code reviews, Python verification scripts, and documentation ran in parallel without competing for a Cargo build slot.
- **Probes over debate.** Agents, like humans, write confident explanations for wrong hypotheses. Every disagreement about CMake semantics became a short script executed against CMake 4.5.1. For example, one early proposal suggested relaxing our export-file comparison to accept quoted `add_library("Demo::...")` names. Running the oracle showed that CMake never quotes those target names, so the bug was in `rcmake`'s export writer and the test stayed strict.
- **Treating test scripts as code under test.** Our large-project CI job originally truncated per-file diffs at 500 lines. On `llvm-project` and `vtk`, the first 500 lines contained only oracle entries, hiding every `rcmake` row. Commit `f28b50e` raised the cap to 2,000 lines and added per-field summaries with counts and samples from both sides.

## 4. The differential oracle method

Seven verification layers run against the pinned CMake 4.5.1 oracle binary.

| Layer | Suite | Scope | What it catches |
| --- | --- | --- | --- |
| 1 | Unit and integration tests | `cargo test --workspace` on `PureHost` and `NativeHost` | Logic bugs, panics, host-contract drift |
| 2 | Recorded CLI and script cases | `rcmake-harness` over `conformance/cases/` | Byte-exact `-P`, `-E`, CLI, presets, SARIF, and `configure_file` parity |
| 3 | Upstream `Tests/RunCMake` | 371 macOS arm64, 362 Linux, 367 Windows suites run (from 463 upstream entries) | Diagnostic text, policy transitions, generator corner cases |
| 4 | Independent black-box suite | 20 scenarios, 679 checks (`independent_verify.py`) | End-to-end build, install, RPATH, `tar`, `ctest`, `cpack`, and File API parity |
| 5 | Large C++ projects | 10 open-source projects at pinned tags, with reduced option sets (`large_projects.toml`) | Real module scripts, configure stderr (L1), Ninja build-graph identity (L2) |
| 6 | Fuzzing and ecosystem lanes | Seeded generative fuzzing (not coverage-guided) of 14 targets at 1,000,000 inputs each, plus 64-shard `vcpkg`, Conan, and Qt 6 | Panics, resource exhaustion, broad package-manager compatibility |
| 7 | Sealed unseen holdout | 240 repositories never opened during development | Overfitting to known test suites |

[`ARCHITECTURE.md`](ARCHITECTURE.md#verification) uses the same layer numbers. L1 and L2 are parity levels inside layers 5 and 7, not layer numbers. L1 checks configure success and standard error, while L2 also compares the Ninja build graph, `CMakeCache.txt`, and File API replies.

The RunCMake counts show the suites executed on each operating system. `tools/measure/runcmake_eval.py` lists every excluded suite in `LINUX_ONLY`, `WINDOWS_ONLY`, `POSIX_ONLY`, and `EXTERNAL_SUITES`. Some exclusions stem from runner toolchain limits rather than OS differences. Specifically, `EXTERNAL_SUITES` holds suites that require tools absent on standard CI runners, such as `CUDA_architectures`, `Swift`, `Autogen_2` to `Autogen_7`, and `FindOpenSSL`. The runs also pass `CMake_TEST_Qt5=OFF`, `CMake_TEST_Qt6=OFF`, and `CMAKE_Fortran_COMPILER=NOTFOUND`. Fortran is verified by the independent suite and the ecosystem lane, and Qt is verified by `qtbase` and `kcoreaddons`.

### Why independent black-box tests were essential

CMake's upstream `Tests/RunCMake` suite guards against regressions that CMake authors expect to break in CMake. It rarely tests invariants that CMake itself never broke, such as whether an installed macOS binary actually launches with `@rpath` load commands or whether `cpack` archive listings match byte structure across generators.

We wrote 20 independent black-box scenarios (`tools/measure/independent_verify.py` and `tools/measure/verify/`) covering 679 checks across C, C++, Objective-C++, Fortran, presets, multi-generator builds, install trees, Mach-O and ELF runtime paths, archive listings, and File API replies. A scenario passes only when every explicit assertion passes on both binaries and all normalized observations match. Running the suite with `--oracle-only` first verifies that every assertion holds on CMake 4.5.1 itself.

### Large-project corpus and sealed unseen holdout

The 10 large projects pinned in [`conformance/corpus/large_projects.toml`](../conformance/corpus/large_projects.toml) are `duckdb` v1.3.2, `opencv` 4.12.0, `vtk` v9.5.0, `qtbase` v6.8.3, `llvm-project` 20.1.8, `pytorch` v2.7.1, `kcoreaddons` v6.15.0, `boost` 1.88.0, `rclcpp` 28.1.8, and `blender` v4.5.0. L1 checks configure success and stderr parity, and L2 compares `CMakeCache.txt`, normalized Ninja dry-run commands, Ninja graph edges, and File API `codemodel` replies.

Every project uses unmodified upstream sources at its pinned tag, configured with a reduced option set from `large_projects.toml` to fit CI runner build times and installed dependencies. For example, OpenCV builds 3 modules with about 14 `WITH_*` options off, LLVM builds only `clang` for the host target, `qtbase` turns off GUI, widgets, and SQL, and Blender runs headless without Python, Cycles, or USD. L2 compares the Ninja dry-run only for the targets listed in `l2_targets`.

Separately, the repository holds a manifest of 240 unseen repositories. No agent may open that manifest during development, and `licence_scan.py --check-hygiene` skips it. It runs once against the frozen release binary, measuring parity on projects that no fix was ever tuned for.

### Documented normalizers only

Differential comparison requires narrow normalizers for machine paths, timestamps, and intentional differences, and every normalizer carries the risk of masking a real bug. Each normalizer in `tools/corpus/normalize.py` and `tools/measure/verify/` carries a written rationale checked against CMake 4.5.1, audited in commits `f28b50e` and `fb13e49`.

- `tar tvf` owner and group columns are masked because `rcmake` writes uid 0 and gid 0 for reproducible archives.
- `OPENCV_TIMESTAMP` holds wall-clock configure time, so only its value is masked.
- File API `backtraces` index arrays are dropped where `backtraceGraph` is compared as resolved frame chains, and `index-*.json` objects pass through the same documented canonicalizer as the reply files (`fb13e49`).
- `cmake -E capabilities` normalizes the `debugger` field (`rcmake` reports `"debugger":false`) and empty `extraGenerators` lists while keeping `version`, `fileApi`, `instrumentation`, `serverMode`, and `tls` strict.
- `Boost.Locale` is excluded on both engines in `large_projects.toml` because Boost 1.88 fails `source_group(TREE)` prefix validation on CMake 4.5.1 as well.

## 5. Hard bugs differential testing exposed

### 5.1 Listfile replay deduplication (`200ed8c`)

`Interp::record_list_file` in `crates/rcmake-eval/src/interp/listfile.rs` and `configure_file` in `crates/rcmake-commands/src/policy_misc/files.rs` both checked whether a path already existed in `dir.listfiles` before pushing it. That looked tidy, yet it broke parity whenever a project included the same `.cmake` file or configured the same template more than once inside a directory. CMake records every read in order, and File API `cmakeFiles` replies expose that exact sequence. Removing the deduplication check in `200ed8c` preserved repeated listfile reads and matched CMake's recorded listfile stream.

### 5.2 CMake diagnostic paragraph spacing (`ef841e3`, `200ed8c`)

CMake formats error and warning bodies through `cmDocumentationFormatter`, which wraps paragraphs, inserts blank separator lines between them, and treats indented lines as preformatted blocks. Earlier in development, emitters such as generator-expression errors and missing-link-target diagnostics manually inserted double newlines (`\n\n`). When tests failed on extra blank lines, `crates/rcmake-diag/src/render.rs` grew prefix-matching special cases (`format_with_added_separators`) that stripped `\n\n` back out for recognized strings.

Commits `ef841e3` and `200ed8c` fixed the root cause. Every diagnostic producer now emits the single-newline structure `cmDocumentationFormatter` expects, as in `validate_link_items` in `crates/rcmake-resolve/src/pipeline.rs`, and `format_diagnostic_body` collapsed to a single call to `format_paragraphs`. Deleting 25 lines of special-case logic fixed both known cases and future diagnostics.

### 5.3 Darwin RPATH resolution and cross-platform path parsing (`cfacf0b`, `186d525`)

Two bugs lived in the same function, `rpath_entries` in `crates/rcmake-resolve/src/lower/link/rpath.rs`.

1. **Missing build-tree `@rpath` entries (`cfacf0b`).** Black-box scenarios s01, s15, and s18 install a shared library and link a consumer against it, including transitively through a static library. On macOS, the built consumer failed at runtime with `Library not loaded: @rpath/libdemo_shared.1.dylib ... Reason: no LC_RPATH's found` across all four generators. `rcmake` had restricted build RPATH entries to libraries inside the build tree or a sysroot. CMake's actual rule (`cmComputeLinkInformation::AddLibraryRuntimeInfo`) skips only implicit link directories and, on Apple platforms, checks whether the library install name uses `@rpath`, reading `IMPORTED_SONAME` or inspecting the Mach-O header in `rpath_macho.rs`. Because Ninja, Ninja Multi-Config, Unix Makefiles, and Xcode all call `rpath_entries`, fixing that one function repaired all four generators.
2. **POSIX-rooted Darwin paths parsed on Windows (`186d525`).** Immediately afterward, Windows CI failed unit tests resolving Darwin RPATHs for `/opt/.../libfoo.dylib`. Because `Lexical::absolute()` applies host path rules, a leading `/` without a drive letter was not treated as absolute on Windows and got dropped from the runtime path set. Accepting `path.starts_with(b"/") || file.absolute()`, along with the matching check for `libraries.user_dirs`, restored cross-platform Darwin resolution on Windows hosts.

### 5.4 File API `codemodel` v2.12 parity (`5b51b47`, `fb13e49`)

IDE tooling relies on CMake File API `codemodel` v2.12 and `cmakeFiles` v1 replies. Early replies matched small projects but drifted on large C++ targets with header file sets, generated sources, utility targets, and mixed compile languages. Commit `5b51b47` split target reply generation into dedicated modules (`crates/rcmake-gen-common/src/fileapi/codemodel/{sources,dependencies,file_sets,targets}.rs`) to match CMake's grouping of compile flags, file-set membership, provenance traces, direct target dependency entries, and `cmakeFiles` rules. Under those `cmakeFiles` rules, `isCMake` is true only for paths under `<CMAKE_ROOT>/Modules`, which remain absolute while project files stay relative to the source directory. Commit `fb13e49` closed the remaining verification gap by passing File API `index-*.json` objects through the same documented canonicalizer as target replies.

### 5.5 Probe-cache invalidation soundness (`b9bded2`, `3d6a2a4`)

Compiler probes (`try_compile`, `check_c_source_compiles`, `check_symbol_exists`) take most of the wall time in a cold configure. Reusing probe results across build directories and projects removes that repeated work from later configures, yet one stale cache hit can silently corrupt a build.

Commits `b9bded2` and `3d6a2a4` hardened the cache boundary across `rcmake-commands` and `rcmake-store`. Every probe executes under `JournalHost` (`crates/rcmake-commands/src/configure/try_compile/journal.rs`), which records all file reads, compiler search paths, depfiles, environment reads, and child processes. If a probe touches state that cannot be re-verified safely, the journal marks an `ImpureReason` (`ExternalWrite`, `SharedState`, `NondeterministicInput`, `UnverifiedProcess`, `ChangedInput`, or `JournalLimit`) and refuses to publish the entry. On lookup, `rcmake-store` validates file and directory-tree input stamps (`crates/rcmake-store/src/stamp/tree.rs`) before accepting a hit. [`tools/measure/probe_cache_eval.py`](../tools/measure/probe_cache_eval.py) runs 16 baseline checks. Twelve checks verify invalidation triggers, including `CPATH`, `__has_include`, header shadowing, `LANG`, compiler wrapper swaps, transitive includes, `DEVELOPER_DIR`, and archive replacement. The remaining four checks test cache control (seed, reuse, and bypass), eight concurrent configures sharing one store, recovery after a `SIGKILL` during publication, and recovery from a corrupted entry. `--extended` and `--prelude` add further cases.

#### What the probe key covers and does not cover

The probe key captures the probe recipe, the toolchain, and the child process environment. On lookup, `rcmake` verifies every recorded input file by its SHA-256 content digest, including the compiler executable itself, so upgrading a compiler in place at the same path triggers a miss. Toolchain stamps also cover clang configuration files (`clang.cfg` and `<triple>.cfg`) and the GCC `specs` file.

The cache skips storage whenever the environment sets a variable that alters compiler behavior in ways `rcmake` does not model, such as `CCC_OVERRIDE_OPTIONS`, `COMPILER_PATH`, `GCC_EXEC_PREFIX`, `LD_PRELOAD`, `LD_LIBRARY_PATH`, and `DYLD_*`. Wrapper scripts or custom binaries passed through `CC` or `CXX` that fall outside modeled compiler layouts, as well as sanitized macOS environments whose `PATH` omits `/usr/sbin` system tools needed for host fingerprinting, also bypass publication safely rather than risk an unsound hit. Other compiler-internal files that `rcmake` does not model are outside the key. In CI and release builds, set `RCMAKE_PROBE_CACHE=verify` to compare every hit against a fresh probe and use the fresh result.

#### Parallel and speculative probes

By default, `rcmake` runs up to four compiler probes concurrently (`RCMAKE_PROBE_JOBS`), whereas CMake runs them serially, which accounts for part of the cold configure speedup. Speculative lookahead considers only literal compile checks (`check_include_file`, `check_c_compiler_flag`, `check_cxx_compiler_flag`, `check_symbol_exists`, and `try_compile` with `SOURCE_FROM_CONTENT`). The worker pool rejects `try_run`, and `execute_process` is never queued. When evaluation reaches the actual command, it recomputes the probe key and accepts the speculative result only when the key matches and validation succeeds. Otherwise, it runs the probe in the foreground.

#### Replay and FetchContent prefetch

A directory region replays only when its entry context and every recorded input match. The entry context includes the `rcmake` executable, policy and diagnostic state, configure options, generator, and toolchain. Matching regions reapply their recorded variable, cache, and property changes in the original order. Any operation without a replay contract forces the region to re-execute, including unverified subprocesses, clock or random reads, and state mutations outside recorded mutation APIs.

In native mode, prefetch can start a FetchContent download before the script populates it, provided `FetchContent_Declare()` has already run and points to a pinned source, either an archive with an expected hash or a Git repository with a full commit ID. Prefetch stays off in frozen lock mode, in offline mode, for package lookups, and when `FETCHCONTENT_SOURCE_DIR_<name>` overrides the source. Compatibility mode never prefetches.

### 5.6 Large-project and archive parity edge cases (`c4e32bb`, `8f5188f`, `2043177`)

- **`qtbase` `try_compile` header (`c4e32bb`).** `QtSeparateDebugInfo.cmake` extracts a binary path from `try_compile` `OUTPUT_VARIABLE` using `string(REGEX REPLACE ".+###(.+)###.+" "\\1" ...)`. Because `rcmake` ran `try_compile` in-process and returned only raw tool output (`###<path>###\n`), nothing preceded `###` and `.+` failed to match. CMake prepends `Change Dir: '<bindir>'` and `Run Build Command(s): ...` before the tool output, so emitting that same header fixed Qt and any other project parsing `OUTPUT_VARIABLE`.
- **ROS 2 `rclcpp` re-inclusion guard (`c4e32bb`).** `ament_cmake_coreConfig.cmake` checks `if(_ament_cmake_core_CONFIG_INCLUDED)` and sets `<Pkg>_FOUND` to `FALSE` if `<Pkg>_FOUND` is undefined. `rcmake` previously unset `<Pkg>_FOUND` unconditionally before reading a package config file, breaking every second `find_package()`. CMake unsets `<Pkg>_FOUND` only when it is already defined and evaluates to false.
- **PyTorch ONNX export-set creation (`c4e32bb`).** `third_party/onnx/CMakeLists.txt` calls `install(EXPORT ONNXTargets ...)` before any `install(TARGETS ... EXPORT ONNXTargets)`. CMake's `GetExportSets()[name]` uses C++ `std::map::operator[]`, which inserts an empty export set on first lookup, whereas `export(EXPORT)` stays strict. Creating empty export sets on `install(EXPORT)`, `EXPORT_ANDROID_MK`, `PACKAGE_INFO`, and `SBOM` while keeping `export(EXPORT)` strict matched CMake.
- **Bounded ZIP scanning and LZW `.Z` clear codes (`8f5188f`).** The original ZIP end-of-central-directory reader scanned backward across the whole archive in 64 KiB chunks, risking quadratic work on truncated files containing false `PK\x05\x06` signatures. Because the end record is 22 bytes with at most a 65,535-byte comment, reading the last 65,557 bytes once eliminates the scan. In the same commit, the Unix `compress` (`.Z`) decoder stopped comparing against the fixed 16-byte array length `group.len()` instead of the active byte count `len`, which had rejected valid 12-bit clear codes.
- **Cross-compiling `try_run` and CPS alignment (`2043177`).** `try_compile` now resolves imported targets via `CMAKE_TRY_COMPILE_CONFIGURATION` rather than `CMAKE_BUILD_TYPE` passed in `CMAKE_FLAGS`, `file(INSTALL)` prints `Installing:` for directory symlinks, and CPS definitions preserve alphabetical ordering and case-insensitive language mapping.

## 6. Scorecard and limitations

### Frozen release scorecard

`tools/measure/stamp_metrics.py` stamps every cell in the "Frozen release binary" column from one frozen-binary verification run. Unreleased numbers are never estimated.

| Area | Scope | Frozen release binary |
| --- | --- | --- |
| Release commit | Git SHA and date | <!-- METRIC:release_commit -->`8d9182957c82`<!-- /METRIC --> (<!-- METRIC:release_date -->`2026-11-26`<!-- /METRIC -->) |
| 3-OS CI and corpus smoke | `ubuntu-24.04`, `macos-15`, `windows-2025` | <!-- METRIC:ci_jobs -->6 of 6 jobs green<!-- /METRIC --> |
| Upstream `Tests/RunCMake`, macOS arm64 | 371 suites run (`all-macos`) | <!-- METRIC:runcmake_macos -->pending frozen release run<!-- /METRIC --> |
| Upstream `Tests/RunCMake`, Linux | 362 suites run (`linux`) | <!-- METRIC:runcmake_linux -->99.35% (352 / 362 suites, 9,312 / 9,373 cases)<!-- /METRIC --> |
| Upstream `Tests/RunCMake`, Windows | 367 suites run (`windows`) | <!-- METRIC:runcmake_windows -->pending frozen release run<!-- /METRIC --> |
| Upstream Core-62 conformance | 62 core suites (`core`) | <!-- METRIC:runcmake_core62 -->98.80% (58 / 62 suites at 100%, 3,702 / 3,747 cases)<!-- /METRIC --> |
| Upstream policies | 35 policy suites for the 161 policies `CMP0066` to `CMP0226` | <!-- METRIC:runcmake_policies -->100.00% (32 / 35 suites, 401 / 401 cases)<!-- /METRIC --> |
| Independent black-box suite | 20 scenarios, 679 checks | <!-- METRIC:independent_scenarios -->20 / 20 scenarios (679 / 679 checks passed)<!-- /METRIC --> |
| Large C++ projects (L1) | 10 projects, configure stderr parity | <!-- METRIC:large_projects_l1 -->0 / 10 passed<!-- /METRIC --> |
| Large C++ projects (L2) | 10 projects, Ninja graph and File API parity for the listed targets | <!-- METRIC:large_projects_l2 -->1 / 10 passed<!-- /METRIC --> |
| Sealed unseen holdout | 240 repositories, L1 strict parity on oracle-green repos | <!-- METRIC:unseen_holdout -->0.00% (0 / 74 repositories)<!-- /METRIC --> |
| `vcpkg` ecosystem matrix | 64 shards on Linux | <!-- METRIC:vcpkg_matrix -->96.08% (319 / 332 scored CMake ports across 16 / 64 shards). 656 ports inspected<!-- /METRIC --> |
| Conan and ecosystem fixtures | `ecosystem_eval.py` | <!-- METRIC:conan_ecosystem -->0 / 2 scenarios passed<!-- /METRIC --> |
| Silent `PATH` swap | `path_swap_eval.py` | <!-- METRIC:path_swap_eval -->pass (0 differences)<!-- /METRIC --> |
| Probe-cache soundness | 16 baseline checks (12 invalidation triggers, cache control, concurrent configures, crash and corruption recovery) | <!-- METRIC:probe_cache_soundness -->16 / 16 checks passed<!-- /METRIC --> |
| Mutation replay soundness | 20 seeded leaf edits, replay equivalence | <!-- METRIC:mutation_replay_soundness -->20 / 20 edits verified<!-- /METRIC --> |
| Seeded generative fuzzing | 14 targets, 1,000,000 inputs each, not coverage-guided | <!-- METRIC:fuzz_failures -->14,000,000 inputs, 0 failures<!-- /METRIC --> |
| WebAssembly and WASI checks | `wasm32-unknown-unknown` and `wasm32-wasip1` | <!-- METRIC:wasm_smoke -->6 of 6 checks passed<!-- /METRIC --> |
| Cold configure | Synthetic C project with 500 static libraries, new build tree, empty probe cache, median of 5 runs, speedup over CMake 4.5.1 | <!-- METRIC:bench_cold_configure -->2.03x<!-- /METRIC --> |
| New build tree with shared probe cache | Synthetic 24-probe project speedup over a CMake new build tree (CMake has no cache across build trees) | <!-- METRIC:bench_warm_probe -->5.80x<!-- /METRIC --> |
| Leaf edit reconfigure | Synthetic 64-directory project, p95 wall time of 5 runs | <!-- METRIC:bench_incremental_leaf_p95 -->110.0 ms<!-- /METRIC --> |
| Peak memory (synthetic 500-library cold configure) | Resident set size (MiB) | <!-- METRIC:bench_peak_rss -->132.7 MiB<!-- /METRIC --> |
| Release binary size | `rcmake` on macOS arm64 | <!-- METRIC:binary_size_macos -->14.32 MiB (15,018,896 bytes)<!-- /METRIC --> |

Across the five real-world C++ giant benchmarks (`duckdb`, `opencv`, `protobuf`, `qtbase`, and `llvm-project`), the geometric mean speedups on `8d9182957c82` are `1.02x` cold, `1.41x` warm probe-cache (`1.91x` on `llvm-project`, `1.48x` on `protobuf`, `1.45x` on `opencv`), and `1.21x` on leaf edits (`1.80x` on `opencv`, `1.45x` on `qtbase`).

For known parity gaps from the frozen release run, the verification receipt reports <!-- METRIC:known_parity_gaps -->Full RunCMake on macOS and Windows and 48 of 64 vcpkg shards were still running on GitHub Actions at the v0.1.0 cutoff. Linux Full RunCMake passed 99.35 percent across 362 suites and 16 completed vcpkg shards are recorded. Synthetic incremental leaf reconfigure recorded 110.05 ms p95 under host load average 3.94 against the 100 ms target, while staying 1.46x faster by median (106.89 ms vs 155.66 ms) and 2.47x faster at p95 (110.05 ms vs 272.34 ms) than CMake 4.5.1. Conan fixtures passed all 6 stages including build and ctest with byte-identical package config and targets files, while the test runner failed the strict compile_commands.json path diff because CONAN_HOME was not normalized. All 10 large C++ projects configured cleanly, while strict L1 and L2 diffing failed on absent ccmake, codemodel abstractTargets ordering, Ninja phony listfile grouping, and cmake_symlink_library spacing. The sealed 240-repository unseen holdout on macOS arm64 recorded 132 oracle-red repositories, 33 timeouts, 2 clone errors, and 0 of 74 strict L1 matches due to the same absent ccmake and Ninja phony grouping differences in unseen_eval.py. The Linux CLI strict-mode check rejected /lib32 on Ubuntu while macOS CLI checks passed 31 of 31 subchecks, and the macOS probe cache requires /usr/sbin on PATH and bypasses unmodeled CC or CXX wrappers.<!-- /METRIC -->.

Each benchmark report records N, the generator, configure flags, host CPU, memory, OS version, and power source, and the OS file cache stays warm across runs. The new-build-tree speedup reflects cross-tree reuse that CMake lacks, whereas a CMake rerun in the same build tree also skips checks (reported separately by `bench_eval.py`). The default leaf edit does not alter any target, while `--leaf-edit compile-definition` measures an edit that changes target flags.

### What `rcmake` does not support

- **Interactive front ends.** `cmake-gui` and `ccmake` were cut from the `v0.1.0` scope, so the `edit_cache` target prints `No interactive CMake dialog available.`, matching CMake when `ccmake` is absent.
- **DAP debugger.** Debugger server support was cut from `v0.1.0`, so `cmake --debugger` fails with `CMake was not built with support for --debugger` and `cmake -E capabilities` reports `"debugger":false`.
- **Swift, CUDA, and HIP verification.** Basic Swift properties and rule exclusions exist in `rcmake-resolve`, CUDA has one end-to-end fixture in the ecosystem lane, and HIP has rule wiring only. Standard CI runners lack GPU hardware and Swift or HIP toolchains, so CI does not exercise full builds for those languages yet.
- **CPack platform installers.** Archive generators (`TGZ`, `TXZ`, `TZST`, `TBZ2`, `TZ`, `TAR`, `STGZ`, `ZIP`, `7Z`, and their compression variants), Linux `DEB` and `RPM`, `External`, and macOS `Bundle` and `DragNDrop` packages are implemented. `NSIS`, `WIX`, `productbuild`, and `IFW` were cut from `v0.1.0`.
- **Legacy and niche generators.** Supported generators are `Ninja`, `Ninja Multi-Config`, `Unix Makefiles`, `Xcode`, `Visual Studio 17 2022`, and `Visual Studio 18 2026`. Legacy extra generators (`CodeBlocks`, `CodeLite`, `Eclipse CDT4`, `Kate`, `Sublime Text 2`) and older or niche generators (`NMake Makefiles`, `NMake Makefiles JOM`, `MSYS Makefiles`, `MinGW Makefiles`, `Borland Makefiles`, `Watcom WMake`, `Green Hills MULTI`, `FASTBuild`, and Visual Studio 14, 15, and 16) were omitted for `v0.1.0`.
- **Reproducible `tar` ownership.** `cmake -E tar` and CPack archive generators write uid 0 and gid 0 instead of host user IDs so archive bytes remain deterministic across machines.
- **`--version` text.** Line 1 prints `cmake version 4.5.1`, and line 2 prints `rcmake 0.1.0` instead of the Kitware line so the output never misattributes the binary to Kitware.
- **`try_compile` runs in-process.** Probes run through the `rcmake` executor instead of Ninja, Make, MSBuild, or xcodebuild so they stay fast and traceable under `JournalHost`. Pass and fail outcomes and feature checks match CMake 4.5.1 across the tested probe suites, while build tool lines in `OUTPUT_VARIABLE` differ (beyond the shared `Change Dir:` and `Run Build Command(s):` header in section 5.6) and MSBuild-only or Xcode-only settings (`CMAKE_GENERATOR_TOOLSET`, `CMAKE_XCODE_ATTRIBUTE_*`) do not apply to probes.
- **Incremental replay guard.** Native-mode replay disables itself when a toolchain file (including `vcpkg`), a configure preset, `-C`, or `PreLoad.cmake` is active, acting as a soundness guard against global setup hooks outside directory-scoped tracking. [`ENV_VARS.md`](ENV_VARS.md#incremental-replay) lists every condition.
- **Large-project option subsets.** The 10 large projects run reduced option sets and listed L2 targets to fit CI runner build times and installed dependencies (section 4).
- **Windows ZIP aliases.** Manual extraction of the Windows ZIP archive puts `cmake.exe`, `ctest.exe`, and `cpack.exe` in `bin`, so putting that folder first on `PATH` hides another CMake install. Both `install.sh` and `install.ps1` create those aliases only when asked via `--alias-cmake` or `-AliasCmake`.

## 7. One command for each claim

Prepare the pinned CMake 4.5.1 oracle and a staged `rcmake` installation once from the repository root.

```sh
source env/env.sh
scripts/fetch-oracle.sh
eval "$(scripts/fetch-oracle.sh --print-env)"
RCMAKE_STAGE_PROFILE=release scripts/stage.sh
```

| Claim | Single command |
| --- | --- |
| Dependency licences, zero `unsafe` in `rcmake` crates, and Git hygiene | `python3 tools/measure/licence_scan.py --check-hygiene` |
| Rust workspace size (crates, files, lines) | `python3 -c 'import pathlib; f=list(pathlib.Path("crates").rglob("*.rs")); print(len(list(pathlib.Path("crates").iterdir())), "crates,", len(f), "files,", sum(len(p.read_text(errors="ignore").splitlines()) for p in f), "lines")'` |
| 3-OS `Tests/RunCMake` (replace `all-macos` with `linux` or `windows`) | `python3 tools/measure/runcmake_eval.py --profile all-macos --all --json runcmake.json` |
| Independent 20-scenario black-box suite (679 checks) | `python3 tools/measure/independent_verify.py --stage target/stage --json independent.json` |
| 10 large C++ projects (5 local macOS projects, full 10 in `ecosystem.yml`) | `python3 tools/corpus/large_projects_local.py --project all --rcmake-stage target/stage --oracle "$RCMAKE_ORACLE_CMAKE" --mode parity --run-dir /tmp/rcmake-large` |
| Sealed 240-repository unseen holdout (L2) | `python3 tools/corpus/unseen_eval.py --split holdout --stage target/stage --oracle "$RCMAKE_ORACLE_CMAKE" --level L2 --out-json unseen.json --out-md unseen.md` |
| Conan and ecosystem fixtures (`gh workflow run ecosystem.yml -f lane=vcpkg` for 64-shard `vcpkg`) | `python3 tools/corpus/ecosystem_eval.py --stage target/stage --oracle "$RCMAKE_ORACLE_CMAKE" --out-json ecosystem.json --out-md ecosystem.md` |
| Silent `PATH` swap (`cmake`, `ctest`, `cpack` on `PATH`) | `python3 tools/measure/path_swap_eval.py --rcmake-bin target/stage/bin/rcmake --out path-swap.json` |
| Cold configure (empty probe cache), same-tree rerun, and peak RSS compared to CMake 4.5.1 (add `--probe-jobs 1` for serial probes) | `python3 tools/measure/bench_eval.py --include-corpus --json bench.json --markdown bench.md` |
| New build tree with shared probe cache, and leaf edit (add `--leaf-edit compile-definition` for an edit that changes targets) | `python3 tools/measure/bench_configure.py --rcmake target/stage/bin/rcmake --oracle "$RCMAKE_ORACLE_CMAKE" --scenario all --json bench-configure.json --markdown bench-configure.md` |
| Probe-cache invalidation and crash-recovery soundness | `python3 tools/measure/probe_cache_eval.py --rcmake target/stage/bin/rcmake --out-json probe.json --out-md probe.md` |
| Incremental leaf-edit replay soundness | `python3 tools/measure/mutation_eval.py --fixture --rcmake target/stage/bin/rcmake --out-json mutation.json --out-md mutation.md` |
| Target and variable provenance (`rcmake explain`) | `target/stage/bin/rcmake explain app -B build --json` |
| Configure flame graph and trace (`--profile`) | `target/stage/bin/rcmake -S . -B build -G Ninja --profile=profile-out` |
| Hermetic input auditing (`--hermetic`) | `target/stage/bin/rcmake -S . -B build -G Ninja --hermetic=strict` |
| Build the `rcmake-lsp` language server | `cargo build --locked --release -p rcmake-lsp --features eval,stdio` |
| WebAssembly dependency audit and WASI `PureHost` smoke (`rcmake.wasm`) | `python3 tools/measure/wasm_smoke.py --require-wasi --json wasm-smoke.json` |
| Seeded generative fuzzing, not coverage-guided (all 14 targets, 1,000,000 inputs each) | `for s in syntax expr regex genex json presets cps archive path tolerant binfmt diagnostics fortran store-frame; do cargo run --profile fuzz --bin rcmake-fuzz -- "$s" --iterations 1000000; done` |
| Upstream CMake release and policy drift tracker | `python3 tools/measure/upstream_track.py --out-json upstream.json --out-md upstream.md` |

## 8. Lessons that held up

- **Pin one oracle and settle every question with a probe.** Fixing CMake 4.5.1 turned vague specification arguments into five-line test scripts with clear answers.
- **Hide all operating-system access behind one trait on day one.** Hermetic auditing, probe-cache journaling, incremental replay, LSP unsaved-buffer overlays, deterministic tests, and WebAssembly all grew out of `rcmake_host::Host` without changing the evaluator core.
- **Keep generators simple.** Computing compile flags, link closures, and runtime paths once in `rcmake-resolve` meant fixing a bug once for Ninja, Makefiles, Xcode, and Visual Studio together.
- **Write independent black-box tests alongside upstream suites.** Upstream tests catch what CMake authors expect to regress, while end-to-end black-box checks catch what a brand-new implementation misses.
- **Delete special cases when you find the real rule.** Removing `format_with_added_separators` in `rcmake-diag` and listfile deduplication in `rcmake-eval` shrank the codebase while raising parity.
- **Audit normalizers and test scripts as strictly as production code.** A truncated diff or an overly broad normalizer makes a broken build look green.
