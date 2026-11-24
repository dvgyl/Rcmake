# Architectural Decision Records (ADR)

All major architectural and contract decisions for Rcmake are recorded here by the primary planners (**Gemini 4 Neon** and **Opus 5.5**, with empirical verification against the pinned **CMake 4.5.1** oracle).

---

## ADR-001: Toolchain, Workspace Layout, Strict Lints & Dependency Policy

- **Status:** Accepted
- **Date:** 2026-11-23

### Decision

1. **Toolchain & Edition:** Pin `rust-toolchain.toml` to Rust `1.99.1` (`rustfmt`, `clippy`), workspace `edition = "2024"`, `rust-version = "1.99"`, `resolver = "3"`. Commit `Cargo.lock`.
2. **Workspace Crates:** Adopt the disjoint crate map from `docs/ARCHITECTURE.md` plus three focused foundation/utility crates so parallel workers never collide:
   - `rcmake-base` (byte strings, lists, truthiness, versions, paths, hashes, time, rand, limits)
   - `rcmake-json` (jsoncpp-compatible JSON parser and writers for `string(JSON)`, Presets, File-API, `-E capabilities`, and SARIF)
   - `rcmake-archive` (shared `tar`/`zip`/`gzip`/`xz`/`zstd` archive engine for `file(ARCHIVE_*)`, `cmake -E tar`, and `cpack`)
   - Plus `rcmake-model` skeleton introduced in M1 so `rcmake-eval` contracts stay stable into M2.
3. **Mechanical Enforcement via Lints & `clippy.toml`:**
   - `unsafe_code = "forbid"` in workspace lints and `#![forbid(unsafe_code)]` in every crate root.
   - `unwrap_used`, `expect_used`, `panic`, `todo`, `unimplemented`, `dbg_macro`, `print_stdout`, `print_stderr`, `exit`, `indexing_slicing`, `string_slice`, `iter_over_hash_type`, `disallowed_methods`, `missing_docs`, `unreachable_pub`, and `broken_intra_doc_links` set to `warn` in `Cargo.toml` and enforced as `-D warnings` by `scripts/gate.sh`.
   - `clippy.toml` bans direct `std::fs::*`, `std::env::*`, `std::process::Command::new`, `std::process::exit`, and `std::time::{SystemTime,Instant}::now` across all crates except explicitly `#[allow]`-annotated `rcmake-host::native`, `rcmake-cli/src/main.rs`, and `rcmake-harness`.
4. **Permissive Dependency Allow-List:** Only `MIT`, `Apache-2.0`, `Apache-2.0 WITH LLVM-exception`, `BSD-2-Clause`, `BSD-3-Clause`, `ISC`, `Zlib`, `Unicode-3.0`, and `0BSD`.
   - **Adopted for M0/M1:** `bstr` (`default-features = false, features = ["std", "alloc"]`), `indexmap`, `memchr`, `thiserror`, `jiff`, `rustix`, `md-5`, `sha1`, `sha2`, `sha3`, `flate2`, `tar`, `zip` (deflate only), `zstd` (BSD-3 option), `liblzma` (pinned `>= 5.6.2`). `serde` + `serde_json` allowed only in `rcmake-harness`/tools (never `preserve_order`).
   - **Explicitly Rejected:** `regex`/`fancy-regex` (wrong dialect), `glob`/`globset` (wrong semantics), `clap` (irregular CMake argv/error grammar), `camino` (cannot hold non-UTF-8 Unix paths), `dirs`/`directories` (`option-ext` MPL-2.0), `xz2` (unmaintained), `webpki-roots` (CDLA-Permissive-2.0), `aws-lc-rs` (OpenSSL + circular CMake build dependency). `bzip2` deferred to M3 pending separate licence ADR.
5. **16 GB RAM Parallel Slot Discipline:** Parallel workers use `CARGO_TARGET_DIR=target/slot-a` or `target/slot-b` with `CARGO_BUILD_JOBS=2` and `RUST_TEST_THREADS=3` so Cargo's target directory lock mechanically enforces the 2-concurrent-build limit.

---

## ADR-002: Byte-Oriented Value Model, Lists, Truthiness & Versions

- **Status:** Accepted
- **Date:** 2026-11-23

### Decision

1. **Byte Strings (`bstr::BString` / `BStr`):** CMake variables, source files, paths, and regex inputs can contain arbitrary non-UTF-8 bytes (empirically verified: `string(ASCII 200 x)` has `string(LENGTH)` == 1). Engine values are byte strings (`BString`), never Rust `String` sliced by UTF-8 index.
2. **CMake List Splitting (`rcmake-base::list`):**
   - Split after variable expansion, once per unquoted source argument.
   - Maintain a **signed** square-bracket depth counter (`[` increments, `]` decrements); an unescaped `;` splits only when depth `== 0`. Brackets remain in the output bytes.
   - A backslash immediately followed by `;` (`\;`) produces a literal `;` (testing the immediately following byte, not backslash parity). Other backslashes remain intact.
   - Expose two independent options: empty-element handling (`Empty::Keep` vs `Empty::Drop`) and empty-whole-input handling (e.g., `list(LENGTH)` / `foreach(IN LISTS)` yield 0 on `""`, whereas `if("" IN_LIST empty_defined)` evaluates `TRUE` as verified on CMake 4.5.1).
   - `list::join` joins with `;` and never escapes elements.
3. **Truthiness (`rcmake-base::truth`):** Keep `is_on`, `is_off`, and `if_constant` as distinct functions matching CMake 4.5.1 (`NOTFOUND` and `*-NOTFOUND` are case-sensitive; `ON`/`OFF`/`TRUE`/`FALSE`/`YES`/`NO`/`Y`/`N`/`IGNORE` are ASCII case-insensitive; numeric `strtod` parsing applies to `if_constant` literals only, not post-dereference variable values).
4. **Arbitrary-Precision Version Comparison (`rcmake-base::version`):** Empirically verified on CMake 4.5.1 that `VERSION_GREATER` compares arbitrarily many components and arbitrary-length decimal integers (`1.2.3.4.5.999999999999999999999999 > 1.2.3.4.5.999999999999999999999998`). `version::compare` compares parsed decimal digit runs (ignoring leading zeros, trailing missing components = 0) without a 4-component or `u64` cap.

---

## ADR-003: Diagnostics, Policy Registry (`CMP0000`–`CMP0226`) & SARIF 2.1.0

- **Status:** Accepted
- **Date:** 2026-11-23

### Decision

1. **Persistent Call Stacks (`Backtrace`):** Store `Backtrace` as a persistent `Option<Arc<BtNode>>` linked stack of `Frame { file, line, name /* original spelling */, kind }` so targets and deferred commands retain cheap immutable snapshots for generate-time diagnostics.
2. **Render Kind vs. Execution Effect:** Keep `MessageKind` (how the message renders) distinct from `Effect` (`None`, `SetError`, `Fatal`), because `SEND_ERROR` and `FATAL_ERROR` both render `CMake Error` while differing in whether execution continues.
3. **Exact Terminal Formatting:**
   - Empirically verified wrap width is **77 bytes** including the 2-space indent (`format_paragraphs`), preserving preformatted lines (leading space) and paragraph breaks, ending framed diagnostics with `\n\n\n` (two trailing blank lines).
   - No-location form (`Location::None`) prints `CMake Error: <msg>\n` without indentation or trailing blank lines.
   - `Call Stack (most recent call first):` is printed only when printable caller frames remain.
   - Paths under `RenderOptions::relative_base` print relative to it (matching `cmake -P` behavior).
   - Command status errors render `<lowercase-command> <text>` as fatal.
4. **Generated Policy & Diagnostic Tables:** Generate `CMP0000`–`CMP0226` metadata (`id`, `introduced`, `removed_old` for `<= 65`) and the 9 `DiagCategory` entries (`CMD_AUTHOR`, `CMD_DEPRECATED`, `CMD_EXPERIMENTAL`, `CMD_INSTALL_ABSOLUTE_DESTINATION`, `CMD_NON_TARGET_DIRECTIVE`, `CMD_POLICY`, `CMD_STRICT`, `CMD_UNINITIALIZED`, `CMD_UNUSED_CLI` with parent hierarchy and defaults) via `tools/gen/gen_policies.py` from the pinned CMake 4.5.1 oracle.
5. **SARIF 2.1.0:** Built into `rcmake-diag::DiagEngine` using `rcmake-json` so `--sarif-output` and `CMAKE_EXPORT_SARIF` share one canonical diagnostic stream.

---

## ADR-004: Deterministic `Host` Abstraction (`NativeHost` & `PureHost`)

- **Status:** Accepted
- **Date:** 2026-11-23

### Decision

1. **Trait Shape:** `Host: Send + Sync` taking `&self` (interior mutability where needed) so future parallel `ctest` and concurrent tooling can share `Arc<dyn Host>`.
2. **Absolute Paths Only:** Every filesystem path passed to `Host` must be absolute (`HostError::NotAbsolute` otherwise). Engine code never relies on or mutates process cwd.
3. **Environment Overlay (Edition 2024 Safe):** `std::env::set_var` is `unsafe` in Rust 2024 and forbidden. `NativeHost` snapshots process environment once and applies an interior-mutable overlay passed explicitly (`env_clear` + overlay) to every child process.
4. **Structured Error & Process Fidelity:**
   - `HostError::os_message` stores raw `strerror` text without Rust's `" (os error N)"` suffix so CMake error wording matches byte-for-byte on both `NativeHost` and `PureHost`.
   - `Host::run` supports command pipelines, shared stdout/stderr pipe interleaving, explicit cwd/env, and process-group timeouts (via `rustix`).
   - `PureHost` provides an in-memory `BTreeMap` filesystem with real symlink resolution, strictly advancing virtual nanosecond timestamps on writes, scripted subprocess hooks, and an ordered stdio event log.
5. **Shared Host Conformance Suite:** `rcmake_host::conformance::run_all` runs the exact same behavioral suite against both `NativeHost` and `PureHost` in CI.

---

## ADR-005: Lexer/Parser, Clean-Room Regex VM, Evaluator & Oracle Harness

- **Status:** Accepted
- **Date:** 2026-11-23

### Decision

1. **Lexer/Parser & Pre-Execution Nesting Validation (`rcmake-syntax`):**
   - Preserve raw source bytes (strip leading UTF-8 BOM before column counting; reject UTF-16/UTF-32 BOM; convert CRLF->LF for files; preserve lone CR inside quoted/bracket args).
   - Strip odd-backslash `\<LF>` quoted-arg continuations and the first `\n` after a bracket opener in the lexer (empirically verified: `${CMAKE_CURRENT_LIST_LINE}` reports the opening line after a quoted continuation). All other escapes (`\n`, `\;`, etc.) are preserved raw for `rcmake-expr`.
   - Output a flat command sequence (`ListFile`), preserving original command spelling, `ArgKind::{Unquoted, Quoted, Bracket}`, opening/closing lines, and byte spans.
   - Validate control-flow nesting (`if`/`elseif`/`else`/`endif`, `foreach`/`endforeach`, `while`/`endwhile`, `function`/`endfunction`, `macro`/`endmacro`, `block`/`endblock`) before execution begins (empirically verified: commands before an unmatched `endif()` or unclosed `if()` do NOT execute in CMake 4.5.1).
2. **Clean-Room Byte Regex VM (`rcmake-regex`):**
   - Implement a clean-room Henry-Spencer / KWSys-compatible byte regex compiler and bounded backtracking VM (no `regex` crate).
   - Support up to **31 capturing groups** (`0..=31`, 32nd fails with `RegularExpression::compile(): Too many parentheses.`, empirically verified on 4.5.1); `CMAKE_MATCH_0..9` exposes slots 0..=9 and `CMAKE_MATCH_COUNT`.
   - Support `bol_at_start` (`CMP0186` OLD vs NEW) and zero-length match retry for repeated search.
3. **Evaluator Stacks & Contracts (`rcmake-eval`):**
   - Separate stacks for variable scopes (with raise-then-set `PARENT_SCOPE`), policy stack, diagnostic stack, and execution frames.
   - Distinct `Flow::{Normal, Break, Continue, Return { propagate }}` control-transfer states.
   - Macros perform ordered textual substitution on non-bracket raw body arguments with `CMP0219` call-site backslash preservation; functions bind `ARGC`, `ARGNC` (4.5), `ARGV0..N`, `ARGV`, `ARGN`.
4. **Staged On-Disk `CMAKE_ROOT` & Version Banner:**
   - Modules remain on disk (`share/cmake-4.5/{Modules,Templates}` beside `bin/{rcmake,cmake,ctest,cpack}`), staged by `scripts/stage.sh` into `target/stage/`.
   - `--version` prints `cmake version 4.5.1\n\nRcmake native Rust drop-in replacement for CMake 4.5.1.\n` (matching the version line byte-for-byte without falsely claiming Kitware maintenance; `rcmake-harness` normalizes the attribution line).
5. **Black-Box Oracle Harness (`rcmake-harness`):**
   - Links zero core engine crates; runs staged `rcmake` and pinned `$RCMAKE_ORACLE_CMAKE` side-by-side in fresh `/tmp` sandboxes with controlled env, normalizing `<TMP>`, `<CMAKE_EXE>`, `<CMAKE_ROOT>`, and the version attribution line.
   - Uses compact txtar-style `.case` files plus recorded JSON baselines in `conformance/baselines/`.

---

## ADR-006: Generator Expression Engine (`rcmake-genex`)

- **Status:** Accepted
- **Date:** 2026-11-23

### Decision

1. **Decoupled Crate & `GenexHost` Trait:** `rcmake-genex` depends only on `rcmake-base`, `rcmake-diag`, and `rcmake-regex`. To break the mutual recursion between generator-expression evaluation (`$<TARGET_PROPERTY:...>`) and transitive usage resolution without cyclic crate dependencies, `rcmake-genex` defines a `GenexHost` trait (`find_target`, `raw_property`, `transitive_property`, `artifact`, `link_closure`) implemented by `rcmake-resolve`.
2. **Shared Pure Algorithms in `rcmake-base`:** Move the `cmake_path` lexical engine and `list` sort/transform/filter algorithms into `rcmake-base` during M2 Wave 0 so `$<PATH:...>`, `$<LIST:...>`, `cmake_path()`, and `list()` share one verified implementation.
3. **Parser & Evaluation Invariants (Verified Against CMake 4.5.1):**
   - `$<` opens an expression, `>` closes the innermost open expression, the first `:` at that nesting level splits operator from arguments, and commas split arguments at that nesting level according to each operator's arity rules (`$<UPPER_CASE:a,b:c>` -> `A,B:C`; `$<BOOL:a,b>` -> error).
   - Unmatched delimiters recover as literal text; complete `$<>` is a generation error.
   - `IF`, `AND`, `OR`, and conditional payloads evaluate lazily. `$<BOOL:00>` and `$<BOOL:0.0>` evaluate to `1`. `$<JOIN:a;;b,|>` drops empty items (`a|b`), while `$<LIST:JOIN,a;;b,|>` preserves them (`a||b`).
   - `$<GENEX_EVAL:...>` re-parses and evaluates in the same context; `$<TARGET_GENEX_EVAL:t,...>` evaluates pass 1 in consumer `head`, then pass 2 with target context switched to `t`.
4. **Separate Cycle Detection vs. Memoization:**
   - Cycle detection uses an active parent-linked chain (`DAGChecker`) keyed on `(target, property)` with CMake's exact exemptions and `"Self reference on target"` diagnostic.
   - Memoization lives in `rcmake-resolve`, keyed on `(target, property, config, language, head?, traversal)`, where `EvalFlags { head_sensitive, context_sensitive, link_language_sensitive }` controls whether `head` enters the key so diamond graphs remain polynomial.
5. **Preprocess & Configure-Time Helpers:** Provide `find_genex(bytes)`, syntax-only `genex_strip(bytes)`, and AST-preserving `strip(ast, Mode::{Build, Install})` + `rewrite_install_prefix` for `string(GENEX_STRIP)`, `install(EXPORT)`, and `export()`. Reserve `command_config` and `output_config` on `EvalContext` for `Ninja Multi-Config`.

---

## ADR-007: Arena-Based `ProjectModel`, Directory Scopes & Transitive Usage Resolution

- **Status:** Accepted
- **Date:** 2026-11-23

### Decision

1. **Arena Architecture:** Extend `rcmake-model::ProjectModel` with arenas (`DirId`/`Directory`, `TargetId`/`Target`, `CommandId`/`CustomCommand`, `InstallRule`, `ExportSet`, `TestEntry`) and delete M1 placeholder maps (`target_names`, `test_names`, `rule_names`).
2. **Backtrace-Tagged Usage Properties (`BT<BString>`):** Store `Target` usage requirements (`INCLUDE_DIRECTORIES`, `COMPILE_DEFINITIONS`, `COMPILE_OPTIONS`, `COMPILE_FEATURES`, `LINK_OPTIONS`, `LINK_DIRECTORIES`, `LINK_LIBRARIES`, and `INTERFACE_*` counterparts) and `sources` as `Vec<BT<BString>>` (carrying cheap `Backtrace` snapshots) rather than joined strings, so File-API v2.12 and diagnostics never lose origin provenance. Store `LinkItem { text: BT<BString>, lookup_dir: DirId }` for `CMP0079` cross-directory link calls.
3. **Target Lookup & Policy Snapshots:**
   - Normal targets and `IMPORTED GLOBAL` targets reside in a global map; non-global `IMPORTED` targets and their aliases are scoped to their creation `Directory` and descendants via `ProjectModel::find_target(dir, name)`.
   - Snapshot target-creation policy values (`TargetPolicySnapshot`) on target creation and `Directory::final_policies` when a directory's `CMakeLists.txt` finishes.
   - Generate the `CMAKE_<PROP>` target-property initializer table via `tools/gen/gen_target_init.py` from BSD-3-Clause CMake 4.5.1 Help `.rst` docs.
4. **Scoped Properties & `INHERITED` Chain:** Use typed `PropScope::{Global, Directory(DirId), Target(TargetId), Source { dir, path }, Test { dir, name }, Cache(BString)}`. An `INHERITED` lookup walks `Object -> Directory -> ParentDirectories -> Global`; any set value (including the empty string) stops the walk.
5. **Directory Execution in `rcmake-eval`:** Replace `Interp`'s single `dir_props`/`source_dir`/`binary_dir`/`defer` fields with the `ProjectModel` directory arena + `dir_stack: Vec<DirId>` and implement `Interp::run_subdirectory(...)` directly in `rcmake-eval` (copying parent variables on entry so `PARENT_SCOPE` updates the caller frame without leaking back into child reads, pushing policy/diagnostic/loop barriers, running the directory's deferred queue on exit, and recording `final_policies`).
6. **Transitive Link & Usage Semantics:** `PRIVATE d` on `STATIC`/`OBJECT` adds `$<LINK_ONLY:d>` to `INTERFACE_LINK_LIBRARIES` (not on `SHARED`/`MODULE`). Compile traversal skips `$<LINK_ONLY:...>`; link traversal includes it. Keep compile-usage deduplication (encounter order + `SYSTEM` classification) distinct from link-line ordering (`REORDER_MINIMALLY`, `CMP0156`, `CMP0179`, static archive multiplicity).

---

## ADR-008: Compiler Bootstrap, Vendored `Modules/` & In-Process `try_compile`

- **Status:** Accepted
- **Date:** 2026-11-23

### Decision

1. **Execute Vendored CMake 4.5.1 `Modules/` Directly:** Rather than hardcoding compiler flags or platform tables in Rust, `project()` / `enable_language()` execute the vendored `modules/cmake-4.5/Modules/` sequence (`CMakeDetermineSystem`, `CMakeUnixFindMake`/`CMakeNinjaFindMake`, `CMakeSystemSpecificInitialize`, `CMakeDetermine<LANG>Compiler`, `CMakeSystemSpecificInformation`, `CMake<LANG>Information`, `CMakeTest<LANG>Compiler`, `CMakeDetermineCompilerABI`, `Check*`, `GNUInstallDirs`) directly through `rcmake-eval`, preserving exact variable and cache types and warm-reconfigure caching via `CMakeFiles/4.5.1/CMake<LANG>Compiler.cmake`.
2. **Native Rust Primitives & `GeneratorKind`:** Provide `GeneratorKind` at configure time (name, `multi_config`, find-make module, `GENERATOR_IS_MULTI_CONFIG`), native global properties (`ENABLED_LANGUAGES`, `IN_TRY_COMPILE`, `CMAKE_C_KNOWN_FEATURES`, `CMAKE_CXX_KNOWN_FEATURES`, `PACKAGES_FOUND`, `PACKAGES_NOT_FOUND`), `file(READ_MACHO ...)` (required by Darwin ABI detection), and `CMakeConfigureLog.yaml`.
3. **CMake 4.0+ macOS Defaults:** Preserve `CMAKE_C_COMPILER=/usr/bin/cc`, `APPLE=1`, `UNIX=1`, `DARWIN` unset, `CMAKE_SYSTEM_NAME=Darwin`, and empty `CMAKE_OSX_SYSROOT` by default while compiler state records the resolved SDK path.
4. **Isolated `try_compile` with In-Process `rcmake-build` Executor:**
   - `try_compile` / `try_run` instantiate a fresh child `Interp` with a `TryCompileSeed` (reusing detected compiler state to avoid recursion, capped at depth 2) and a capturing `Host` decorator so child output routes cleanly into `OUTPUT_VARIABLE` and `CMakeConfigureLog.yaml`.
   - Deliberate M2 design choice: execute `try_compile`'s resolved `BuildGraph` via an in-process Rust executor crate (`rcmake-build`) rather than shelling out to Ninja/Make. This keeps `try_compile` fast, deterministic, independent of generator bugs, and lets compiler bootstrap (`project(x C CXX)`) pass end-to-end in Wave 2 before generators are written.

---

## ADR-009: Three-Layer Resolution IR, Generators, File-API v2.12 & Minimal `ctest`

- **Status:** Accepted
- **Date:** 2026-11-23

### Decision

1. **Three-Layer Pipeline (Zero Cyclic Crates):**
   - `ProjectModel` (raw configure-time state) -> `ResolvedTarget` (per-config typed, genex-evaluated target facts with backtraces; single source of truth for File-API codemodel and `install(EXPORT)`) -> `BuildGraph` (lowered POSIX-`sh`-escaped command invocations, placeholder-expanded rule templates `<FLAGS> <DEFINES> <INCLUDES> <OBJECT> <SOURCE> <DEP_FILE> <LINK_FLAGS> <LINK_LIBRARIES> <OBJECTS> <TARGET> <SONAME_FLAG> <TARGET_INSTALLNAME_DIR>`, link fragments with semantic roles, and `InstallManifest`).
   - Non-target metadata (cache, ordered listfile inputs for `RERUN_CMAKE` & `cmakeFiles-v1`, toolchains, configureLog) lives in `ConfigureMetadata`.
   - Acyclic crate graph: `base <- regex <- genex`, `base <- diag <- model`, `eval <- model`, `resolve <- (model, genex, diag, host)`, `build <- (resolve, host)`, `commands <- (eval, model, genex, resolve, build)`, `gen-common <- resolve`, `gen-ninja`/`gen-make <- (resolve, gen-common)`, `ctest <- (eval, host)`, `cli <- all`.
2. **Generator Rollout Order:**
   - Wave 3: `rcmake-gen-ninja` (single-config `Ninja`), `rcmake-gen-common` (`compile_commands.json`, `cmake_install.cmake`, `CTestTestfile.cmake`, `install(EXPORT)`/`export()`, `file(GENERATE)`), `cmake --build`, `cmake --install` (with native `file(INSTALL)` and macOS `install_name_tool` / Linux ELF `file(RPATH_*)`), and minimal `rcmake-ctest` (`CTestTestfile.cmake` execution with `-C`, `-R`, `-E`, `-j`, `--output-on-failure`) so Tier-1 corpus projects (`zlib`, `fmt`, `nlohmann/json`, `cJSON`, `doctest`) build, install, and pass tests end-to-end.
   - Wave 4: `rcmake-gen-make` (`Unix Makefiles` with output sync), `Ninja Multi-Config` (with `COMMAND_CONFIG`/`OUTPUT_CONFIG`), and File-API v2.12 (`codemodel` 2.12, `cache` 2.0, `toolchains` 1.1, `cmakeFiles` 1.1, `configureLog` 1.0), followed by Tier-2 corpus (`Catch2`, `libpng`, `sqlite`, `spdlog`).
3. **Behavioral Equivalence Over Byte-Identical Build Files:** Validate generated build systems against CMake 4.5.1 via normalized `compile_commands.json`, File-API JSON replies, installed file trees + `install_manifest.txt`, passing consumer projects linking against installed exports, and `ctest` results.

---

## ADR-010: Native `cpack` Engine & Archive / macOS Packaging (`rcmake-cpack`)

- **Status:** Accepted
- **Date:** 2026-11-23

### Decision

1. **Acyclic Crate Boundary:** Add `crates/rcmake-cpack` depending on `base`, `diag`, `host`, `json`, `eval`, `commands`, `archive`, and `presets` (no dependency on `rcmake-cli`; `preinstall` runs via `${CMAKE_COMMAND} --build <dir> --target preinstall` through `Host::run_process`). `rcmake-cli` dispatches the `cpack` persona directly to `rcmake_cpack::main`.
2. **Configuration & CLI Precedence (Empirically Verified Against CMake 4.5.1):**
   - Load `CPackConfig.cmake` (or `--config <file>`, or package preset via `--preset`) in a script-mode `Interp` with the full `rcmake_commands::register_all` registry.
   - Apply `-C` (`CPACK_BUILD_CONFIG`) **before** executing the config script; apply `-G`, `-P`, `-R`, `-B`, `--vendor` **after** the config script; apply `-D` overrides last.
   - For each generator in `CPACK_GENERATOR`, set the active generator, evaluate `CPACK_PROJECT_CONFIG_FILE` if set, stage `CPACK_INSTALLED_DIRECTORIES` and `CPACK_INSTALL_CMAKE_PROJECTS` quadruples (`build;project;component;subdir`, running `preinstall` unless `CPACK_SKIP_PREINSTALL` and invoking `cmake_install.cmake` in-process with `CMAKE_INSTALL_PREFIX` pointing at the staging root), plus `CPACK_INSTALL_SCRIPTS`, `CPACK_PRE_BUILD_SCRIPTS`, `CPACK_POST_BUILD_SCRIPTS`, and `CPACK_PACKAGE_CHECKSUM`.
   - Fix the `COMPONENTS` global property to derive dynamically from `ProjectModel::install_components()` (defaulting to `Unspecified`), matching CMake 4.5.1's `CPackConfig.cmake` output.
3. **Milestone 3 Generator Set & Codecs:**
   - Implement `PackageGenerator` trait with archive generators (`TGZ`, `TXZ`, `TZST`, `TBZ2`, `TZ`, `TAR`, `ZIP` including `ZIP_DEFLATE`/`ZIP_STORE`), source packaging (`CPACK_SOURCE_IGNORE_FILES`), component modes (`ONE_PER_GROUP`, `ALL_COMPONENTS_IN_ONE`, `IGNORE`), and macOS `DragNDrop` (`.dmg`) & `Bundle` via `Host::run_process` (`hdiutil`/`SetFile`/`Rez`, tested with scripted `PureHost` and live macOS runs).
   - Extend `rcmake-archive` with ADR-001 codecs `zstd` (`MIT`), `liblzma` (`MIT OR Apache-2.0`), `bzip2` (pure-Rust `libbz2-rs-sys`, `bzip2-1.0.6`), and native pure-Rust LZW for `TZ` (also enabling `-E tar J`/`j`/`--zstd`). Non-archive / non-macOS CPack generators (`DEB`, `RPM`, `NSIS`, `WIX`, `IFW`, `NuGet`, `STGZ`, `productbuild`, etc.) defer to Milestone 4.

---

## ADR-011: Full `rcmake-ctest`, `discover_tests` & `cmake --workflow`

- **Status:** Accepted
- **Date:** 2026-11-23

### Decision

1. **Full-Registry CTest Interpreter & `TEST_INCLUDE_FILES`:**
   - Move `ctest` CLI parsing from `rcmake-cli` into `crates/rcmake-ctest/src/cli.rs` and expand `rcmake-ctest` to depend on `commands`, `presets`, `json`, and `instrument`.
   - Run `CTestTestfile.cmake` and `TEST_INCLUDE_FILES` scripts with the full `rcmake_commands::register_all` registry plus ctest overrides (`add_test`, `set_tests_properties`, `subdirs`, `discover_tests`, and `-S` script commands `ctest_start`, `ctest_configure`, `ctest_build`, `ctest_test`), enabling `gtest_discover_tests` and `catch_discover_tests` in both `POST_BUILD` and `PRE_TEST` modes.
2. **Complete CLI, Discovery, Reporting & State Files:**
   - Support `-I`, `-U`, `-L`/`-LE`, `-FA`/`-FS`/`-FC`, `--rerun-failed`, `--tests-from-file`, `--exclude-from-file`, `--repeat` (`until-fail`, `until-pass`, `after-timeout`, plus CMP0224 fixture repeat mode), `--schedule-random[-seed]`, `--test-load`, `--stop-time`, `-O`, `--output-junit`, `-T Test` (`Testing/TAG` + `Testing/<tag>/Test.xml`), `--show-only=human|json-v1|json-v1-raw`, `-N`, `--out-of-date`, `--print-labels`, `--preset`, and `--instrument` (instrumentation launcher).
   - Support CMake 4.4–4.5 `discover_tests(BUILD_DEPENDS ...)` in `ProjectModel` (`TestKind::Discover`) and `CTestTestfile.cmake` (`_CMAKE_TEST_BUILD_DEPENDS`, `install_prep/*`, `test_prep/*`).
   - Implement `cmake --workflow --preset <name> [--fresh] [--list-presets[=defined]]` in `rcmake-cli/src/workflow.rs`, executing configure, build, test, and package presets in order and propagating the first failing exit code.
   - Defer CDash network `ctest_submit` and `--build-and-test` to Milestone 4.

---

## ADR-012: C++20 Named Modules (`FILE_SET TYPE CXX_MODULES`) & Ninja `dyndep`

- **Status:** Accepted
- **Date:** 2026-11-23

### Decision

1. **Leaf Module Collation Crate (`rcmake-cxxmodules`):**
   - Add `crates/rcmake-cxxmodules` depending only on `base` and `json`. Owns P1689r5 `.ddi` JSON parsing, `CXXDependInfo.json` (TDI) reading/writing, `CXXModules.json` reading/writing, module import collation & cycle diagnostics, Ninja `ninja_dyndep_version = 1.0` `.dd` emission, and compiler module-map writers (LLVM Clang `@modmap`, GCC `-fmodule-mapper=`, MSVC).
   - Expose `cmake -E cmake_ninja_dyndep --tdi=... --lang=CXX --dd=... [--modmapfmt=...] [<file.ddi> ...]` in `rcmake-tool-e` backed by `rcmake-cxxmodules`, verified byte-for-byte against official CMake 4.5.1 `cmake -E cmake_ninja_dyndep` on `.ddi` fixtures.
2. **Model, Resolver & Ninja Pipeline:**
   - Extend `FileSetKind` to `{ Headers, Sources, CxxModules }` (no `CxxModuleHeaders` in CMake 4.5.1; `SYCL`/`SYCL_HEADERS` mapped under experimental gate), add `FileSet.props` + `PropScope::FileSet(FileSetId)` + `CMP0211`, and enforce CMake 4.5.1 `CXX_MODULES` visibility/scope rules.
   - Extend `BuildStep` with `BuildStep::Scan` and `BuildStep::Collate`, plus `modmap`/`dyndep` fields on `BuildStep::Compile`. Honor `CXX_SCAN_FOR_MODULES` precedence (`file set > source > target > CMP0155`), `CMAKE_CXX_STDLIB_MODULES_JSON`, and the synthesized generator-provided `@cmake_cxx_std` target for `import std`.
   - Ninja-only pipeline (`CXX_SCAN__<t>_`, `CXX_DYNDEP__<t>_`, `CXX_COMPILER__<t>_scanned_`); `rcmake-build` executes scan -> collate -> compile in-process for `try_compile`. C++20 targets without `CXX_MODULES` file sets continue to build cleanly on AppleClang without requiring `clang-scan-deps`.

---

## ADR-013: Common Package Specification (CPS), SPDX 3.0.1 SBOM & Instrumentation API v1

- **Status:** Accepted
- **Date:** 2026-11-23

### Decision

1. **Common Package Specification (`rcmake-cps`):**
   - Add leaf crate `crates/rcmake-cps` (`base`, `json`, `diag`) implementing CPS **0.15.0** emission and validating reader (`>= 0.13 && < 0.16`), `@prefix@` path resolution, root `<Pkg>.cps`, appendix `<Pkg>-<appendix>.cps`, and install configuration supplements `<Pkg>[-<appendix>]@<lowercase-config>.cps` (`@noconfig` when empty).
   - Wire `install(PACKAGE_INFO ...)` and `export(PACKAGE_INFO ...)` in `rcmake-commands` / `rcmake-gen-common`, and CPS discovery + imported `<Pkg>::<comp>` / default `<Pkg>` target creation inside `find_package()` Config mode.
2. **Software Bill of Materials (SPDX 3.0.1 JSON-LD):**
   - Implement `install(SBOM <name> EXPORTS <set>... [DESTINATION <dir>] ...)` and `export(SBOM ...)` plus automatic per-export SBOM generation via `CMAKE_INSTALL_SBOM_FORMATS` (`spdx`, `spdx-3.0+json`, `spdx-3.0.1+json` — SPDX 3.0.1 JSON-LD only, matching CMake 4.5.1).
   - Emit `<name>-<config-or-noconfig>.spdx.json` with `@context: "https://spdx.org/rdf/3.0.1/spdx-context.jsonld"`, `CreationInfo`, `SpdxDocument`, `software_Package`, and `hasDeclaredLicense` relationships honoring target `SPDX_LICENSE` over `DEFAULT_LICENSE`.
3. **Instrumentation API v1 (`rcmake-instrument`):**
   - Add leaf crate `crates/rcmake-instrument` (`base`, `json`, `host`) implementing API v1 / data v1.2 query parsing (`<build>/.cmake/instrumentation/v1/query[/generated]/`), snippet/index/content JSON writers, hook triggers (`postGenerate`, `preBuild`, `postBuild`, `preCMakeBuild`, `postCMakeBuild`, `postCMakeInstall`, `postCMakeWorkflow`, `postCTest`), callback dispatch via `Host`, Google trace output, and the `ctest --instrument` command launcher.
   - Register `cmake_instrumentation(...)` in `rcmake-commands`, wire compile/link launcher wrapping in `rcmake-resolve`, invoke lifecycle hooks from `rcmake-cli`, and expose `"instrumentation":{"apiVersion":1,"dataVersion":{"major":1,"minor":2}}` in `cmake -E capabilities`.

---

## ADR-014: Native Xcode Generator (`rcmake-gen-xcode`)

- **Status:** Accepted
- **Date:** 2026-11-24
- **Source:** `docs/M4_DESIGN.md` sections 1.3–1.7, 2, and 3.

### Context

Xcode is the only IDE (integrated development environment) generator that this macOS machine can verify end to end with the real build tool (`xcodebuild`). The CMake 4.5.1 Xcode output is not deterministic: a second `cmake .` changed 737 lines of `project.pbxproj`. ADR-009 already chose behavioral equivalence over byte-identical build files.

### Decision

1. **Two IDE crates, not one.** `docs/ARCHITECTURE.md` names one `rcmake-gen-ide` crate. Milestone 4 uses `crates/rcmake-gen-xcode` and `crates/rcmake-gen-vs` instead, so that two workers can work in parallel without conflicts. `rcmake-gen-xcode` depends on `base`, `diag`, `model`, `resolve`, `gen-common`, `host`, and `sha2`.
2. **Module layout (frozen in Wave 0).** `plist.rs` (OpenStep writer), `ids.rs` (object IDs), `object.rs` (object graph), `scheme.rs`, `workspace.rs`, `rerun.rs` (`ReRunCMake.make`), `project.rs`, `settings.rs`, `phases.rs`, `build.rs` (`build_commands()`), and `lib.rs` (`generate()`).
3. **Shared contracts (frozen in Wave 0).**
   - `GeneratorKind::{Ninja, NinjaMultiConfig, UnixMakefiles, Xcode, VisualStudio(VsVersion)}` in `rcmake-model`. `Xcode` is multi-config. Its default `CMAKE_CONFIGURATION_TYPES` is `Debug;Release;MinSizeRel;RelWithDebInfo`. `CMAKE_CFG_INTDIR` is `$(CONFIGURATION)$(EFFECTIVE_PLATFORM_NAME)`. `-G Xcode` is available only on macOS.
   - `GeneratorSpec` and the pure parsers `parse_xcode_toolset`, `parse_vs_toolset`, and `parse_vs_platform` in `rcmake-model`. The `Display` text of `GeneratorSpecError` is the CMake 4.5.1 message text. The configure commands report the error at the first `project()` call.
   - `ResolveOptions`, `resolve_with_options`, and `ResolvedTarget::ide` (`IdeFacts` in `rcmake-resolve/src/ide.rs`). `IdeFacts` holds raw, unescaped values. In Wave 0, `ide` is always `None`, so the Ninja and Makefile output does not change.
   - `rcmake-gen-common/src/ide.rs`: `utility_targets`, `project_roots`, `targets_for_root`, `rerun_inputs`, and the pure `cmake --build` description `BuildRequest` / `BuildCommand`. Every generator crate exports `build_commands(&BuildRequest) -> Result<Vec<BuildCommand>, GenError>`. The CLI runs the commands in order and stops at the first failure.
4. **Deterministic object IDs.** ID = the first 24 uppercase hexadecimal digits of SHA-256(4-byte little-endian `PBXType` ordinal ‖ semantic key). The key never contains the build directory. Cached `<target>_GUID_CMAKE` and `PROJECT_<Name>_GUID_CMAKE` entries have priority, so a build tree that the oracle made keeps its IDs. An ID collision is a `GenError`.
5. **`try_compile` under Xcode** uses the in-process `rcmake-build` executor (ADR-008). Rcmake does not write an Xcode project for each `try_compile`.
6. **Verification.** Tier X0 (unit golden strings), X1 (structural diff of canonical forms made by `tools/measure/pbxproj_canon.py`), X2 (live `xcodebuild` build, `ctest`, and install), X3 (upstream `Tests/RunCMake/XcodeProject*`, at least 85%), and X4 (Tier-1 and Tier-2 corpus, 9/9).

### Consequences

- Rcmake Xcode projects are reproducible. A repeated configure does not change `project.pbxproj`, so Xcode does not reload the project without a reason.
- Oracle comparison uses the canonical-form tool. Byte-for-byte diffs are not used.
- One `IdeFacts` contract serves both IDE generators.
- `cmake -E capabilities` lists `Xcode` only after Wave 2, when the generator writes projects.

---

## ADR-015: Native Visual Studio Generator (`rcmake-gen-vs`)

- **Status:** Accepted
- **Date:** 2026-11-24
- **Source:** `docs/M4_DESIGN.md` sections 1.2, 1.8, and 4.

### Context

The CMake 4.5.1 oracle cannot create a Visual Studio generator on macOS. This machine has no MSBuild. Verification on macOS must be structural.

### Decision

1. **Scope.** Milestone 4 implements `Visual Studio 17 2022` (`.sln`, toolset `v143`, `ToolsVersion` `17.0`) and `Visual Studio 18 2026` (`.slnx`, toolset `v145`, `ToolsVersion` `18.0`). `VsVersion::{Vs17, Vs18}` is an enum, so older versions can be added later. `Visual Studio 15 2017` and `16 2019` wait for a later milestone.
2. **Crate.** `crates/rcmake-gen-vs` depends on `base`, `diag`, `model`, `resolve`, `gen-common`, `host`, and `md-5`. Modules (frozen in Wave 0): `guid.rs`, `xml.rs`, `sln.rs`, `slnx.rs`, `filters.rs`, `vcxproj.rs`, `rules.rs`, `flags_table.rs`, `build.rs`, and `lib.rs`.
3. **Host gate.** On Windows, both generators are available. On other hosts, `-G "Visual Studio 17 2022"` gives the oracle error `Could not create named generator <name>` and exit code 1. The `vs-cross` Cargo feature of `rcmake-model` (forwarded by `rcmake-cli/vs-cross`) makes the generators available on every host. Only the structural-test build enables this feature. The default build and the staged release binary never enable it. No environment variable changes this behavior.
4. **Deterministic GUIDs.** A cached `<name>_GUID_CMAKE` entry has priority. Otherwise the GUID is the RFC 4122 version-3 (MD5) UUID with namespace `ee30c4be-5192-4fb0-b335-722a2dffe760` over `"<CMAKE_BINARY_DIR>|<name>"`, in uppercase. This is the same rule as CMake 4.5.1, so Rcmake can match oracle GUIDs exactly.
5. **Clean-room MSVC flag table (locked).** `flags_table.rs` is hand-authored from the documented MSVC command-line options. Rcmake does not read, copy, or generate from the Microsoft MSBuild rule files (`CL.xml`, `Link.xml`, `LIB.xml`), and it does not copy the CMake `cmVS*FlagTable` tables. A flag that the table does not know goes to `<AdditionalOptions>` with `%(AdditionalOptions)`. This is valid MSBuild, and the build behavior is the same.
6. **Verification: structural only on macOS (locked).** There is no `.github/workflows/windows.yml` in Milestone 4.
   - Tier V0: unit golden strings (`guid.rs` UUID-v3 vectors, `sln.rs`, `slnx.rs`, `xml.rs`, `filters.rs`, `build_commands()`).
   - Tier V1: XML well-formedness, element order, and `ProjectReference` GUID checks with `tools/measure/vs_canon.py`.
   - Tier V2: the upstream `Tests/RunCMake/VSSolution` and `VS10Project` check scripts run with oracle `cmake -P` against the `vs-cross` output (at least 80% of the cases that do not need MSBuild).
   - Tier V3: seeded configure from hand-written `conformance/vs/seed/msvc-x64/` compiler files.

### Consequences

- Windows parity is "structural only" in Milestone 4. A live Windows tier (byte-level `.sln`/`.vcxproj` parity, MSBuild builds) needs a later decision.
- The release binary on non-Windows hosts behaves the same as the oracle: it cannot create a Visual Studio generator.
- The flag table has no Microsoft-licensed input, so the licence position of the crate stays `MIT OR Apache-2.0`.

---

## ADR-016: Benchmarks, Safe-Rust Fuzzing & Release Hardening

- **Status:** Accepted
- **Date:** 2026-11-24
- **Source:** `docs/M4_DESIGN.md` sections 1.9 and 5.

### Decision

1. **Benchmarks.** `tools/bench/bench.py` (Python standard library only) runs each case with the oracle and with the staged Rcmake binary: 2 warm-up runs, then 7 measured runs (median and 90th percentile). In exclusive mode, wall time is the primary metric. In shared mode (other agents run), CPU time and peak RSS (resident set size) are the primary metrics. Cases B1–B9 and their gates are in `docs/M4_DESIGN.md` section 5.1. The first known performance task is B2: the `-P` loop uses 2.1× the oracle CPU time.
2. **Safe-Rust fuzzing only (locked).** `crates/rcmake-fuzz` is the only fuzzer. It uses the stable toolchain, safe Rust (`#![forbid(unsafe_code)]`), and no new dependencies. Rcmake does not use `libfuzzer-sys` or `cargo-fuzz`, and has no out-of-workspace `fuzz/` directory, so the repository has no `NCSA`-licensed dependency.
   - Harness contract (frozen in Wave 0): each input runs on its own thread with an 8 MiB stack, inside `catch_unwind`, with a wall-clock budget (default 2 s). A caught panic or a timeout is a failure. The SplitMix64 generator `rcmake_fuzz::Rng` makes each failure replayable from its seed.
   - Binary: `rcmake-fuzz <surface> [--seed <s>] [--iterations <n>] [--budget-ms <ms>]`. Surfaces: `syntax`, `expr`, `regex`, `genex`, `json`, `presets`, `cps`, `archive`, `path`.
   - `rcmake_base::Limits::fuzz()` gives stricter caps for the harness only (16 MiB values and file reads, 1,000,000 regex steps, 2 s per input). Production code always uses `Limits::default()`, which never rejects input that CMake 4.5.1 accepts.
   - Gates: Wave 2 runs 1,000,000 iterations per surface with zero failures. `scripts/gate.sh` gets a 10,000-iteration smoke mode. Each crash becomes a regression test in the owning crate before the fix lands.
   - The `adversarial_fuzz.rs` integration suite stays as the in-tree regression net.
3. **Host primitives for the Milestone 3 P1 audit fixes.**
   - `Host::lock_file(path, timeout) -> Box<dyn FileLockGuard>`: `NativeHost` uses the safe, stable `std::fs::File::lock` / `try_lock` (Rust 1.89+), with no new dependency. `PureHost` uses an in-memory lock table. A lock that is not free before the timeout gives `ErrorKind::TimedOut`.
   - `Host::rename_noreplace(from, to)`: `NativeHost` uses `rustix::fs::renameat_with` with `RenameFlags::NOREPLACE` (macOS `renameatx_np(RENAME_EXCL)`, Linux `renameat2`). A filesystem without support gives `ErrorKind::Unsupported`. `PureHost` checks and renames atomically under its state lock.
   - Both methods have default bodies that return `ErrorKind::Unsupported`, so a host wrapper does not break. The shared conformance suite checks both hosts.
   - `rcmake_base::hash::IncrementalHasher` (`HashAlgo::hasher()`, `update`, `finish_hex`) gives streaming digests for CPack checksums and `cmake -E sha256sum`. The result is equal to `HashAlgo::hex_digest` over the same bytes.
4. **Worktree oracle path (locked).** `env/env.sh` exports `RCMAKE_ORACLE_DIR`. When the local `env/oracle/bin` is missing (a git worktree), it uses `env/oracle` of the main checkout: the parent directory of `git rev-parse --git-common-dir`.
5. **Release hardening.** A panic hook in `rcmake-cli/src/main.rs` prints one `rcmake: internal error: ...` line and exits with code 1. The release profile keeps `panic = "unwind"`. Two clean `scripts/stage.sh` runs must give byte-identical binaries. `tools/measure/determinism.py` checks that each generator writes the same files into two build directories after `<B>` replacement. `tools/measure/licence_scan.py` must pass.

### Consequences

- Fuzzing finds panics, stack overflows, and hangs on the stable toolchain without coverage guidance. Coverage-guided fuzzing needs a new ADR.
- Milestone 4 adds no external crate. `Cargo.lock` changes only by the three new workspace packages.
