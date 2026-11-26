# Architectural decision records

We verified each decision below against the pinned CMake 4.5.1 oracle.

---

## ADR-001 Toolchain, workspace layout, strict lints, and dependency policy

Accepted on 2026-11-23.

### Decision

1. **Toolchain and edition.** Pin `rust-toolchain.toml` to Rust `1.99.1` with `rustfmt` and `clippy`, set workspace `edition = "2024"`, `rust-version = "1.99"`, and `resolver = "3"`, and commit `Cargo.lock`.
2. **Workspace crates.** Adopt the crate map from `docs/ARCHITECTURE.md` along with three focused foundation and utility crates.
   - `rcmake-base` holds byte strings, lists, truthiness, versions, paths, hashes, time, random values, and resource limits.
   - `rcmake-json` provides a jsoncpp-compatible JSON parser and writers for `string(JSON)`, Presets, File API, `-E capabilities`, and SARIF.
   - `rcmake-archive` provides the shared `tar`, `zip`, `gzip`, `xz`, and `zstd` archive engine for `file(ARCHIVE_*)`, `cmake -E tar`, and `cpack`.
   - `rcmake-model` stays a separate crate so `rcmake-eval` contracts remain stable as the project model grows.
3. **Mechanical enforcement through lints and `clippy.toml`.**
   - Set `unsafe_code = "forbid"` in workspace lints and place `#![forbid(unsafe_code)]` in every crate root.
   - Set `unwrap_used`, `expect_used`, `panic`, `todo`, `unimplemented`, `dbg_macro`, `print_stdout`, `print_stderr`, `exit`, `indexing_slicing`, `string_slice`, `iter_over_hash_type`, `disallowed_methods`, `missing_docs`, `unreachable_pub`, and `broken_intra_doc_links` to `warn` in `Cargo.toml`, and enforce them with `-D warnings` in `scripts/gate.sh`.
   - Configure `clippy.toml` to ban direct `std::fs::*`, `std::env::*`, `std::process::Command::new`, `std::process::exit`, and `std::time::{SystemTime,Instant}::now` across every crate except the explicitly `#[allow]`-annotated modules in `rcmake-host::native`, `rcmake-cli/src/main.rs`, and `rcmake-harness`.
4. **Permissive dependency allowlist.** Permit only `MIT`, `Apache-2.0`, `Apache-2.0 WITH LLVM-exception`, `BSD-2-Clause`, `BSD-3-Clause`, `ISC`, `Zlib`, `Unicode-3.0`, and `0BSD`.
   - **Adopted crates.** `bstr` with `default-features = false` and `features = ["std", "alloc"]`, `indexmap`, `memchr`, `thiserror`, `jiff`, `rustix`, `md-5`, `sha1`, `sha2`, `sha3`, `flate2`, `tar`, `zip` (deflate only), `zstd` (under its BSD-3 option), and `liblzma` pinned to `>= 5.6.2`. `serde` and `serde_json` support the test harness and tools.
   - **Probe and journal records.** Reuse the locked `serde` and `serde_json` versions for internal journal and probe records with `preserve_order` disabled, while keeping `rcmake-json` for CMake-compatible JSON.
   - **Rejected crates.** Reject `regex` and `fancy-regex` for having the wrong dialect, `glob` and `globset` for having the wrong semantics, `clap` for failing to match CMake's irregular argument and error grammar, `camino` because it cannot hold non-UTF-8 Unix paths, `dirs` and `directories` because they depend on MPL-2.0 `option-ext`, `xz2` because it is unmaintained, `webpki-roots` because it uses CDLA-Permissive-2.0, and `aws-lc-rs` because it brings OpenSSL and a circular CMake build dependency. ADR-010 later accepted `bzip2` with its pure-Rust backend.
5. **Build concurrency.** `env/env.sh` sets default limits for `CARGO_BUILD_JOBS` and `RUST_TEST_THREADS` so local builds and tests fit comfortably in memory on a small machine.

---

## ADR-002 Byte-oriented value model, lists, truthiness, and versions

Accepted on 2026-11-23.

### Decision

1. **Byte strings (`bstr::BString` and `BStr`).** CMake variables, source files, paths, and regex inputs can contain arbitrary non-UTF-8 bytes. For example, `string(ASCII 200 x)` has `string(LENGTH)` equal to 1 in CMake 4.5.1. Engine values therefore use byte strings (`BString`) and never slice Rust `String` values by UTF-8 byte index.
2. **CMake list splitting (`rcmake-base::list`).**
   - Split unquoted arguments after variable expansion, once per unquoted source argument.
   - Track a signed square-bracket depth counter where `[` increments and `]` decrements, splitting on an unescaped `;` only when the depth equals 0 while keeping brackets in the output bytes.
   - Turn a backslash immediately followed by `;` (`\;`) into a literal `;` by inspecting the immediately following byte rather than counting backslash parity. Keep all other backslashes intact.
   - Expose two independent options for empty elements (`Empty::Keep` versus `Empty::Drop`) and empty whole inputs. In CMake 4.5.1, `list(LENGTH)` and `foreach(IN LISTS)` yield 0 on `""`, whereas `if("" IN_LIST empty_defined)` evaluates to `TRUE`.
   - `list::join` joins elements with `;` and never escapes them.
3. **Truthiness (`rcmake-base::truth`).** Keep `is_on`, `is_off`, and `if_constant` as distinct functions matching CMake 4.5.1. `NOTFOUND` and `*-NOTFOUND` are case-sensitive, while `ON`, `OFF`, `TRUE`, `FALSE`, `YES`, `NO`, `Y`, `N`, and `IGNORE` are ASCII case-insensitive. Numeric `strtod` parsing applies only to `if_constant` literals, not to variable values after dereference.
4. **Arbitrary-precision version comparison (`rcmake-base::version`).** CMake 4.5.1 compares arbitrarily many components and arbitrary-length decimal integers in `VERSION_GREATER`, such as `1.2.3.4.5.999999999999999999999999 > 1.2.3.4.5.999999999999999999999998`. `version::compare` compares parsed decimal digit runs without a 4-component or `u64` cap, ignoring leading zeros and treating missing trailing components as 0.

---

## ADR-003 Diagnostics, policy registry CMP0000 to CMP0226, and SARIF 2.1.0

Accepted on 2026-11-23.

### Decision

1. **Persistent call stacks (`Backtrace`).** Store `Backtrace` as a persistent `Option<Arc<BtNode>>` linked stack of `Frame { file, line, name, kind }` keeping the original command spelling. Targets and deferred commands can then retain cheap immutable snapshots for generate-time diagnostics.
2. **Render kind versus execution effect.** Keep `MessageKind`, which controls how a message renders, separate from `Effect` (`None`, `SetError`, `Fatal`), because `SEND_ERROR` and `FATAL_ERROR` both render `CMake Error` while differing in whether execution continues.
3. **Exact terminal formatting.**
   - `format_paragraphs` wraps at 77 bytes including the 2-space indent, preserves preformatted lines that begin with a space, preserves paragraph breaks, and ends framed diagnostics with `\n\n\n` (two trailing blank lines).
   - The no-location form (`Location::None`) prints `CMake Error: <msg>\n` with no indentation and no trailing blank lines.
   - `Call Stack (most recent call first):` prints only when printable caller frames remain.
   - Paths under `RenderOptions::relative_base` print relative to that base, matching `cmake -P`.
   - Command status errors render `<lowercase-command> <text>` as fatal.
4. **Generated policy and diagnostic tables.** `tools/gen/gen_policies.py` generates `CMP0000` through `CMP0226` metadata (`id`, `introduced`, and `removed_old` for policies through `CMP0065`) along with the 9 `DiagCategory` entries (`CMD_AUTHOR`, `CMD_DEPRECATED`, `CMD_EXPERIMENTAL`, `CMD_INSTALL_ABSOLUTE_DESTINATION`, `CMD_NON_TARGET_DIRECTIVE`, `CMD_POLICY`, `CMD_STRICT`, `CMD_UNINITIALIZED`, and `CMD_UNUSED_CLI` with their parent hierarchy and defaults) from the pinned CMake 4.5.1 oracle.
5. **SARIF 2.1.0.** Build SARIF 2.1.0 emission directly into `rcmake-diag::DiagEngine` using `rcmake-json` so `--sarif-output` and `CMAKE_EXPORT_SARIF` read one canonical diagnostic stream.

---

## ADR-004 Deterministic Host abstraction with NativeHost and PureHost

Accepted on 2026-11-23.

### Decision

1. **Trait shape.** Define `Host: Send + Sync` with `&self` methods and interior mutability where needed so parallel `ctest` and concurrent tools can share one `Arc<dyn Host>`.
2. **Absolute paths only.** Every file system path passed to `Host` must be absolute, returning `HostError::NotAbsolute` otherwise. Engine code never relies on or mutates the process working directory.
3. **Environment overlay safe for Rust 2024.** Because `std::env::set_var` is `unsafe` in Rust 2024 and forbidden here, `NativeHost` snapshots the process environment once and tracks updates in an interior-mutable overlay passed explicitly with `env_clear` to every child process.
4. **Structured error and process fidelity.**
   - `HostError::os_message` stores raw `strerror` text without Rust's `" (os error N)"` suffix so CMake error wording matches byte for byte on both `NativeHost` and `PureHost`.
   - `Host::run` supports command pipelines, shared standard output and standard error pipe interleaving, explicit working directories and environments, and process-group timeouts through `rustix`.
   - `PureHost` provides an in-memory `BTreeMap` file system with real symlink resolution, strictly advancing virtual nanosecond timestamps on writes, scripted subprocess hooks, and an ordered standard I/O event log.
5. **Shared host conformance suite.** `rcmake_host::conformance::run_all` runs the exact same behavioral suite against both `NativeHost` and `PureHost` in CI.

---

## ADR-005 Lexer, parser, clean-room regex VM, evaluator, and oracle harness

Accepted on 2026-11-23.

### Decision

1. **Lexer, parser, and pre-execution nesting validation (`rcmake-syntax`).**
   - Preserve raw source bytes while stripping a leading UTF-8 BOM before column counting, rejecting UTF-16 and UTF-32 BOMs, converting CRLF to LF for files, and keeping lone CR bytes inside quoted and bracket arguments.
   - Strip odd-backslash `\<LF>` quoted-argument continuations and the first `\n` after a bracket opener inside the lexer, because `${CMAKE_CURRENT_LIST_LINE}` reports the opening line after a quoted continuation in CMake 4.5.1. Leave all other escapes such as `\n` and `\;` raw for `rcmake-expr`.
   - Emit a flat command sequence (`ListFile`) preserving original command spelling, `ArgKind::{Unquoted, Quoted, Bracket}`, opening and closing lines, and byte spans.
   - Validate control-flow nesting (`if`, `elseif`, `else`, `endif`, `foreach`, `endforeach`, `while`, `endwhile`, `function`, `endfunction`, `macro`, `endmacro`, `block`, and `endblock`) before execution begins. In CMake 4.5.1, commands before an unmatched `endif()` or unclosed `if()` do not execute.
2. **Clean-room byte regex VM (`rcmake-regex`).**
   - Implement a clean-room Henry Spencer and KWSys-compatible byte regex compiler and bounded backtracking VM without using the Rust `regex` crate.
   - Support up to 31 capturing groups (`0..=31`), failing on the 32nd group with `RegularExpression::compile(): Too many parentheses.` to match CMake 4.5.1. Expose slots `0..=9` through `CMAKE_MATCH_0` through `CMAKE_MATCH_9` alongside `CMAKE_MATCH_COUNT`.
   - Support `bol_at_start` for `CMP0186` OLD versus NEW behavior and zero-length match retries during repeated searches.
3. **Evaluator stacks and contracts (`rcmake-eval`).**
   - Keep separate stacks for variable scopes (with raise-then-set `PARENT_SCOPE`), policy scopes, diagnostic scopes, and execution frames.
   - Model control transfers distinctly with `Flow::{Normal, Break, Continue, Return { propagate }}`.
   - Macros perform ordered textual substitution on non-bracket raw body arguments with `CMP0219` call-site backslash preservation, while functions bind `ARGC`, `ARGNC` (added in CMake 4.5), `ARGV0` through `ARGVN`, `ARGV`, and `ARGN`.
4. **Staged on-disk `CMAKE_ROOT` and version banner.**
   - Keep CMake modules on disk in `share/cmake-4.5/Modules` and `share/cmake-4.5/Templates` beside `bin/rcmake`, `bin/cmake`, `bin/ctest`, and `bin/cpack`, staged by `scripts/stage.sh` into `target/stage/`.
   - Print `cmake version 4.5.1\n\nRcmake native Rust drop-in replacement for CMake 4.5.1.\n` for `--version`. This matches the version line byte for byte without falsely claiming Kitware maintenance, and `rcmake-harness` normalizes the attribution line.
5. **Black-box oracle harness (`rcmake-harness`).**
   - Link no core engine crates. Run staged `rcmake` and pinned `$RCMAKE_ORACLE_CMAKE` side by side in fresh `/tmp` sandboxes with a controlled environment, normalizing `<TMP>`, `<CMAKE_EXE>`, `<CMAKE_ROOT>`, and the version attribution line.
   - Store test suites in compact txtar-style `.case` files alongside recorded JSON baselines in `conformance/baselines/`.

---

## ADR-006 Generator expression engine in rcmake-genex

Accepted on 2026-11-23.

### Decision

1. **Decoupled crate and `GenexHost` trait.** `rcmake-genex` depends only on `rcmake-base`, `rcmake-diag`, and `rcmake-regex`. To handle the mutual recursion between generator expression evaluation (such as `$<TARGET_PROPERTY:...>`) and transitive usage resolution without cyclic crate dependencies, `rcmake-genex` defines a `GenexHost` trait (`find_target`, `raw_property`, `transitive_property`, `artifact`, and `link_closure`) that `rcmake-resolve` implements.
2. **Shared pure algorithms in `rcmake-base`.** Place the `cmake_path` lexical engine and `list` sort, transform, and filter algorithms in `rcmake-base` so `$<PATH:...>`, `$<LIST:...>`, `cmake_path()`, and `list()` share one verified implementation.
3. **Parser and evaluation invariants verified against CMake 4.5.1.**
   - `$<` opens an expression, `>` closes the innermost open expression, the first `:` at that nesting level separates the operator from its arguments, and commas split arguments at that nesting level according to each operator's arity rules. For example, `$<UPPER_CASE:a,b:c>` evaluates to `A,B:C`, whereas `$<BOOL:a,b>` produces an error.
   - Unmatched delimiters recover as literal text, while a complete `$<>` produces a generation error.
   - `IF`, `AND`, `OR`, and conditional payloads evaluate lazily. `$<BOOL:00>` and `$<BOOL:0.0>` evaluate to `1`. `$<JOIN:a;;b,|>` drops empty items to produce `a|b`, while `$<LIST:JOIN,a;;b,|>` preserves them to produce `a||b`.
   - `$<GENEX_EVAL:...>` reparses and evaluates its payload in the same context. `$<TARGET_GENEX_EVAL:t,...>` evaluates pass 1 in consumer `head`, then evaluates pass 2 with the target context switched to `t`.
4. **Separate cycle detection and memoization.**
   - Cycle detection tracks an active parent-linked chain (`DAGChecker`) keyed on `(target, property)` with CMake's exact exemptions and the `"Self reference on target"` diagnostic.
   - Memoization lives in `rcmake-resolve`, keyed on `(target, property, config, language, head?, traversal)`. `EvalFlags { head_sensitive, context_sensitive, link_language_sensitive }` controls whether `head` enters the key so diamond graphs remain polynomial.
5. **Preprocess and configure-time helpers.** Provide `find_genex(bytes)`, syntax-only `genex_strip(bytes)`, AST-preserving `strip(ast, Mode::{Build, Install})`, and `rewrite_install_prefix` for `string(GENEX_STRIP)`, `install(EXPORT)`, and `export()`. Reserve `command_config` and `output_config` on `EvalContext` for `Ninja Multi-Config`.

---

## ADR-007 Arena-based ProjectModel, directory scopes, and transitive usage resolution

Accepted on 2026-11-23.

### Decision

1. **Arena architecture.** Build `rcmake-model::ProjectModel` around typed arenas (`DirId` and `Directory`, `TargetId` and `Target`, `CommandId` and `CustomCommand`, `InstallRule`, `ExportSet`, and `TestEntry`) instead of placeholder maps (`target_names`, `test_names`, and `rule_names`).
2. **Backtrace-tagged usage properties (`BT<BString>`).** Store target usage requirements (`INCLUDE_DIRECTORIES`, `COMPILE_DEFINITIONS`, `COMPILE_OPTIONS`, `COMPILE_FEATURES`, `LINK_OPTIONS`, `LINK_DIRECTORIES`, `LINK_LIBRARIES`, and their `INTERFACE_*` counterparts) and `sources` as `Vec<BT<BString>>` carrying cheap `Backtrace` snapshots instead of joined strings. That preserves origin provenance for File API v2.12 and diagnostics. Store `LinkItem { text: BT<BString>, lookup_dir: DirId }` for `CMP0079` cross-directory link calls.
3. **Target lookup and policy snapshots.**
   - Store normal targets and `IMPORTED GLOBAL` targets in a global map, while scoping non-global `IMPORTED` targets and their aliases to their creation `Directory` and its descendants through `ProjectModel::find_target(dir, name)`.
   - Snapshot target-creation policy values in `TargetPolicySnapshot` when creating a target, and snapshot `Directory::final_policies` when a directory's `CMakeLists.txt` finishes.
   - Generate the `CMAKE_<PROP>` target-property initializer table with `tools/gen/gen_target_init.py` from BSD-3-Clause CMake 4.5.1 Help `.rst` documentation.
4. **Scoped properties and the `INHERITED` chain.** Use typed scopes in `PropScope::{Global, Directory(DirId), Target(TargetId), Source { dir, path }, Test { dir, name }, Cache(BString)}`. An `INHERITED` lookup walks from the object to its directory, then parent directories, and then global scope, stopping at the first set value including the empty string.
5. **Directory execution in `rcmake-eval`.** Replace the single `dir_props`, `source_dir`, `binary_dir`, and `defer` fields on `Interp` with the `ProjectModel` directory arena and `dir_stack: Vec<DirId>`. Implement `Interp::run_subdirectory(...)` directly in `rcmake-eval` so entry copies parent variables (letting `PARENT_SCOPE` update the caller frame without leaking back into child reads), pushes policy, diagnostic, and loop barriers, runs the directory's deferred queue on exit, and records `final_policies`.
6. **Transitive link and usage semantics.** Adding `PRIVATE d` on `STATIC` or `OBJECT` targets appends `$<LINK_ONLY:d>` to `INTERFACE_LINK_LIBRARIES`, whereas `SHARED` and `MODULE` targets do not. Compile traversal skips `$<LINK_ONLY:...>`, and link traversal includes it. Keep compile-usage deduplication (encounter order plus `SYSTEM` classification) separate from link-line ordering (`REORDER_MINIMALLY`, `CMP0156`, `CMP0179`, and static archive multiplicity).

---

## ADR-008 Compiler bootstrap, vendored Modules, and in-process try_compile

Accepted on 2026-11-23.

### Decision

1. **Execute vendored CMake 4.5.1 `Modules/` directly.** Rather than hardcoding compiler flags or platform tables in Rust, `project()` and `enable_language()` run the vendored `modules/cmake-4.5/Modules/` sequence (`CMakeDetermineSystem`, `CMakeUnixFindMake` or `CMakeNinjaFindMake`, `CMakeSystemSpecificInitialize`, `CMakeDetermine<LANG>Compiler`, `CMakeSystemSpecificInformation`, `CMake<LANG>Information`, `CMakeTest<LANG>Compiler`, `CMakeDetermineCompilerABI`, `Check*`, and `GNUInstallDirs`) directly through `rcmake-eval`. This preserves exact variable and cache types as well as warm-reconfigure caching through `CMakeFiles/4.5.1/CMake<LANG>Compiler.cmake`.
2. **Native Rust engine state and `GeneratorKind`.** Provide `GeneratorKind` at configure time (name, `multi_config`, find-make module, and `GENERATOR_IS_MULTI_CONFIG`), native global properties (`ENABLED_LANGUAGES`, `IN_TRY_COMPILE`, `CMAKE_C_KNOWN_FEATURES`, `CMAKE_CXX_KNOWN_FEATURES`, `PACKAGES_FOUND`, and `PACKAGES_NOT_FOUND`), `file(READ_MACHO ...)` for Darwin ABI detection, and `CMakeConfigureLog.yaml`.
3. **CMake 4.0 and newer macOS defaults.** Keep `CMAKE_C_COMPILER=/usr/bin/cc`, `APPLE=1`, `UNIX=1`, `DARWIN` unset, `CMAKE_SYSTEM_NAME=Darwin`, and an empty `CMAKE_OSX_SYSROOT` by default while compiler state records the resolved SDK path.
4. **Isolated `try_compile` with the in-process `rcmake-build` executor.**
   - `try_compile` and `try_run` create a fresh child `Interp` seeded with `TryCompileSeed` (reusing detected compiler state to prevent recursion, capped at depth 2) and wrapped in a capturing `Host` decorator so child output routes cleanly into `OUTPUT_VARIABLE` and `CMakeConfigureLog.yaml`.
   - Run the resolved `BuildGraph` of `try_compile` with the in-process Rust executor crate `rcmake-build` instead of invoking Ninja or Make. This keeps `try_compile` fast, deterministic, and independent of generator bugs, so compiler bootstrap (`project(x C CXX)`) never depends on any generator.

---

## ADR-009 Three-layer resolution IR, generators, File API v2.12, and minimal ctest

Accepted on 2026-11-23.

### Decision

1. **Three-layer pipeline with zero cyclic crates.**
   - Configure produces `ProjectModel` for raw configure-time state. Resolution lowers `ProjectModel` to `ResolvedTarget`, which holds per-configuration typed, genex-evaluated target facts with backtraces and acts as the single source of truth for the File API codemodel and `install(EXPORT)`. Lowering then turns `ResolvedTarget` into `BuildGraph`, which holds POSIX-`sh`-escaped command invocations, placeholder-expanded rule templates (`<FLAGS>`, `<DEFINES>`, `<INCLUDES>`, `<OBJECT>`, `<SOURCE>`, `<DEP_FILE>`, `<LINK_FLAGS>`, `<LINK_LIBRARIES>`, `<OBJECTS>`, `<TARGET>`, `<SONAME_FLAG>`, and `<TARGET_INSTALLNAME_DIR>`), link fragments with semantic roles, and `InstallManifest`.
   - Store non-target metadata such as cache entries, ordered listfile inputs for `RERUN_CMAKE` and `cmakeFiles-v1`, toolchains, and configureLog in `ConfigureMetadata`.
   - Maintain an acyclic crate graph. `rcmake-genex` depends on `rcmake-regex` and `rcmake-base`. `rcmake-model` depends on `rcmake-diag` and `rcmake-base`. `rcmake-eval` depends on `rcmake-model`. `rcmake-resolve` depends on `rcmake-model`, `rcmake-genex`, `rcmake-diag`, and `rcmake-host`. `rcmake-build` depends on `rcmake-resolve` and `rcmake-host`. `rcmake-commands` depends on `rcmake-eval`, `rcmake-model`, `rcmake-genex`, `rcmake-resolve`, and `rcmake-build`. `rcmake-gen-common` depends on `rcmake-resolve`. `rcmake-gen-ninja` and `rcmake-gen-make` depend on `rcmake-resolve` and `rcmake-gen-common`. `rcmake-ctest` depends on `rcmake-eval` and `rcmake-host`. `rcmake-cli` brings them together.
2. **Generator scope.**
   - The single-configuration layer provides `rcmake-gen-ninja` (`Ninja`), `rcmake-gen-common` (`compile_commands.json`, `cmake_install.cmake`, `CTestTestfile.cmake`, `install(EXPORT)`, `export()`, and `file(GENERATE)`), `cmake --build`, `cmake --install` (including native `file(INSTALL)`, macOS `install_name_tool`, and Linux ELF `file(RPATH_*)`), and minimal `rcmake-ctest` (`CTestTestfile.cmake` execution with `-C`, `-R`, `-E`, `-j`, and `--output-on-failure`) so Tier-1 corpus projects (`zlib`, `fmt`, `nlohmann/json`, `cJSON`, and `doctest`) build, install, and pass tests end to end.
   - The multi-configuration and File API layer adds `rcmake-gen-make` (`Unix Makefiles` with output sync), `Ninja Multi-Config` (with `COMMAND_CONFIG` and `OUTPUT_CONFIG`), and File API v2.12 (`codemodel` 2.12, `cache` 2.0, `toolchains` 1.1, `cmakeFiles` 1.1, and `configureLog` 1.0), followed by Tier-2 corpus projects (`Catch2`, `libpng`, `sqlite`, and `spdlog`).
3. **Behavioral equivalence over byte-identical build files.** Validate generated build systems against CMake 4.5.1 by comparing normalized `compile_commands.json`, File API JSON replies, installed file trees with `install_manifest.txt`, consumer projects linking against installed exports, and `ctest` results.

---

## ADR-010 Native cpack engine and archive and macOS packaging in rcmake-cpack

Accepted on 2026-11-23.

### Decision

1. **Acyclic crate boundary.** Add `crates/rcmake-cpack` depending on `base`, `diag`, `host`, `json`, `eval`, `commands`, `archive`, and `presets`, with no dependency on `rcmake-cli`. `preinstall` executes `${CMAKE_COMMAND} --build <dir> --target preinstall` through `Host::run_process`, and `rcmake-cli` dispatches the `cpack` persona directly to `rcmake_cpack::main`.
2. **Configuration and CLI precedence verified against CMake 4.5.1.**
   - Load `CPackConfig.cmake`, `--config <file>`, or a package preset from `--preset` in a script-mode `Interp` with the full `rcmake_commands::register_all` registry.
   - Apply `-C` (`CPACK_BUILD_CONFIG`) before executing the config script, apply `-G`, `-P`, `-R`, `-B`, and `--vendor` after the config script, and apply `-D` overrides last.
   - For each generator in `CPACK_GENERATOR`, set the active generator, evaluate `CPACK_PROJECT_CONFIG_FILE` if set, stage `CPACK_INSTALLED_DIRECTORIES` and `CPACK_INSTALL_CMAKE_PROJECTS` quadruples (`build;project;component;subdir`, running `preinstall` unless `CPACK_SKIP_PREINSTALL` is set and running `cmake_install.cmake` in process with `CMAKE_INSTALL_PREFIX` pointing at the staging root), and run `CPACK_INSTALL_SCRIPTS`, `CPACK_PRE_BUILD_SCRIPTS`, `CPACK_POST_BUILD_SCRIPTS`, and `CPACK_PACKAGE_CHECKSUM`.
   - Derive the `COMPONENTS` global property dynamically from `ProjectModel::install_components()` with a default of `Unspecified`, matching CMake 4.5.1 `CPackConfig.cmake` output.
3. **Generator set and codecs.**
   - Implement the `PackageGenerator` trait with archive generators (`TGZ`, `TXZ`, `TZST`, `TBZ2`, `TZ`, `TAR`, and `ZIP` including `ZIP_DEFLATE` and `ZIP_STORE`), source packaging (`CPACK_SOURCE_IGNORE_FILES`), component modes (`ONE_PER_GROUP`, `ALL_COMPONENTS_IN_ONE`, and `IGNORE`), and macOS `DragNDrop` (`.dmg`) and `Bundle` through `Host::run_process` (`hdiutil`, `SetFile`, and `Rez`, tested on scripted `PureHost` and live macOS runs).
   - Extend `rcmake-archive` with the ADR-001 codecs `zstd` (`MIT`), `liblzma` (`MIT OR Apache-2.0`), `bzip2` (pure-Rust `libbz2-rs-sys`, `bzip2-1.0.6`), and pure-Rust LZW for `TZ`, which also enables `-E tar` flags `J`, `j`, and `--zstd`. `rcmake` does not implement the other CPack generators (`DEB`, `RPM`, `NSIS`, `WIX`, `IFW`, `NuGet`, `STGZ`, `productbuild`, and others) under this record. Adding them requires a new ADR.

---

## ADR-011 Full rcmake-ctest, discover_tests, and cmake --workflow

Accepted on 2026-11-23.

### Decision

1. **Full-registry CTest interpreter and `TEST_INCLUDE_FILES`.**
   - Move `ctest` CLI parsing from `rcmake-cli` into `crates/rcmake-ctest/src/cli.rs` and expand `rcmake-ctest` to depend on `commands`, `presets`, `json`, and `instrument`.
   - Run `CTestTestfile.cmake` and `TEST_INCLUDE_FILES` scripts with the full `rcmake_commands::register_all` registry plus ctest overrides (`add_test`, `set_tests_properties`, `subdirs`, `discover_tests`, and `-S` script commands `ctest_start`, `ctest_configure`, `ctest_build`, and `ctest_test`). This enables `gtest_discover_tests` and `catch_discover_tests` in both `POST_BUILD` and `PRE_TEST` modes.
2. **Complete CLI, discovery, reporting, and state files.**
   - Support `-I`, `-U`, `-L`, `-LE`, `-FA`, `-FS`, `-FC`, `--rerun-failed`, `--tests-from-file`, `--exclude-from-file`, `--repeat` (`until-fail`, `until-pass`, `after-timeout`, plus CMP0224 fixture repeat mode), `--schedule-random[-seed]`, `--test-load`, `--stop-time`, `-O`, `--output-junit`, `-T Test` (`Testing/TAG` and `Testing/<tag>/Test.xml`), `--show-only=human|json-v1|json-v1-raw`, `-N`, `--out-of-date`, `--print-labels`, `--preset`, and `--instrument` (the instrumentation launcher).
   - Support CMake 4.4 and 4.5 `discover_tests(BUILD_DEPENDS ...)` in `ProjectModel` (`TestKind::Discover`) and `CTestTestfile.cmake` (`_CMAKE_TEST_BUILD_DEPENDS`, `install_prep/*`, and `test_prep/*`).
   - Implement `cmake --workflow --preset <name> [--fresh] [--list-presets[=defined]]` in `rcmake-cli/src/workflow.rs`, executing configure, build, test, and package presets in order and returning the first failing exit code.
   - `ctest_submit` and `ctest --build-and-test` live in [`script/submit.rs`](../crates/rcmake-ctest/src/script/submit.rs) and [`build_and_test.rs`](../crates/rcmake-ctest/src/build_and_test.rs).

---

## ADR-012 C++20 named modules and Ninja dyndep

Accepted on 2026-11-23.

### Decision

1. **Leaf module collation crate (`rcmake-cxxmodules`).**
   - Add `crates/rcmake-cxxmodules` depending only on `base` and `json`. It owns P1689r5 `.ddi` JSON parsing, `CXXDependInfo.json` (TDI) reading and writing, `CXXModules.json` reading and writing, module import collation and cycle diagnostics, Ninja `ninja_dyndep_version = 1.0` `.dd` emission, and compiler module-map writers for LLVM Clang (`@modmap`), GCC (`-fmodule-mapper=`), and MSVC.
   - Expose `cmake -E cmake_ninja_dyndep --tdi=... --lang=CXX --dd=... [--modmapfmt=...] [<file.ddi> ...]` in `rcmake-tool-e` backed by `rcmake-cxxmodules`, verified byte for byte against official CMake 4.5.1 `cmake -E cmake_ninja_dyndep` on `.ddi` fixtures.
2. **Model, resolver, and Ninja pipeline.**
   - Extend `FileSetKind` to `{ Headers, Sources, CxxModules }`. CMake 4.5.1 has no `CxxModuleHeaders`, and `SYCL` and `SYCL_HEADERS` remain under the experimental gate. Add `FileSet.props`, `PropScope::FileSet(FileSetId)`, and `CMP0211`, and enforce CMake 4.5.1 `CXX_MODULES` visibility and scope rules.
   - Extend `BuildStep` with `BuildStep::Scan` and `BuildStep::Collate`, plus `modmap` and `dyndep` fields on `BuildStep::Compile`. Honor `CXX_SCAN_FOR_MODULES` precedence from file set, to source, to target, to `CMP0155`, along with `CMAKE_CXX_STDLIB_MODULES_JSON` and the generator-provided `@cmake_cxx_std` target for `import std`.
   - Emit the Ninja-only pipeline (`CXX_SCAN__<t>_`, `CXX_DYNDEP__<t>_`, and `CXX_COMPILER__<t>_scanned_`), while `rcmake-build` executes scan, collate, and compile in process for `try_compile`. C++20 targets without `CXX_MODULES` file sets continue to build cleanly on AppleClang without requiring `clang-scan-deps`.

---

## ADR-013 Common Package Specification, SPDX 3.0.1 SBOM, and Instrumentation API v1

Accepted on 2026-11-23.

### Decision

1. **Common Package Specification (`rcmake-cps`).**
   - Add the leaf crate `crates/rcmake-cps` (`base`, `json`, `diag`) implementing CPS 0.15.0 emission and a validating reader accepting versions from 0.13 up to 0.16, `@prefix@` path resolution, root `<Pkg>.cps`, appendix `<Pkg>-<appendix>.cps`, and install configuration supplements `<Pkg>[-<appendix>]@<lowercase-config>.cps` (`@noconfig` when empty).
   - Wire `install(PACKAGE_INFO ...)` and `export(PACKAGE_INFO ...)` in `rcmake-commands` and `rcmake-gen-common`, and add CPS discovery plus imported `<Pkg>::<comp>` or default `<Pkg>` target creation inside `find_package()` Config mode.
2. **Software Bill of Materials (SPDX 3.0.1 JSON-LD).**
   - Implement `install(SBOM <name> EXPORTS <set>... [DESTINATION <dir>] ...)` and `export(SBOM ...)` along with automatic per-export SBOM generation through `CMAKE_INSTALL_SBOM_FORMATS` (`spdx`, `spdx-3.0+json`, and `spdx-3.0.1+json`). As in CMake 4.5.1, the only output format is SPDX 3.0.1 JSON-LD.
   - Emit `<name>-<config-or-noconfig>.spdx.json` with `@context: "https://spdx.org/rdf/3.0.1/spdx-context.jsonld"`, `CreationInfo`, `SpdxDocument`, `software_Package`, and `hasDeclaredLicense` relationships honoring target `SPDX_LICENSE` over `DEFAULT_LICENSE`.
3. **Instrumentation API v1 (`rcmake-instrument`).**
   - Add the leaf crate `crates/rcmake-instrument` (`base`, `json`, `host`) implementing API v1 with data v1.2 query parsing (`<build>/.cmake/instrumentation/v1/query[/generated]/`), snippet, index, and content JSON writers, hook triggers (`postGenerate`, `preBuild`, `postBuild`, `preCMakeBuild`, `postCMakeBuild`, `postCMakeInstall`, `postCMakeWorkflow`, and `postCTest`), callback dispatch through `Host`, Google trace output, and the `ctest --instrument` command launcher.
   - Register `cmake_instrumentation(...)` in `rcmake-commands`, wire compile and link launcher wrapping in `rcmake-resolve`, invoke lifecycle hooks from `rcmake-cli`, and expose `"instrumentation":{"apiVersion":1,"dataVersion":{"major":1,"minor":2}}` in `cmake -E capabilities`.

---

## ADR-014 Native Xcode generator in rcmake-gen-xcode

Accepted on 2026-11-24.

### Context

Xcode is the only integrated development environment (IDE) generator that a macOS host can verify end to end with the real build tool (`xcodebuild`). CMake 4.5.1 Xcode output is not deterministic, because running `cmake .` a second time changed 737 lines of `project.pbxproj`. ADR-009 already chose behavioral equivalence over byte-identical build files.

### Decision

1. **Two IDE crates, not one.** `rcmake` uses separate crates `crates/rcmake-gen-xcode` and `crates/rcmake-gen-vs` rather than one shared IDE crate. Each crate keeps its own external dependency (`sha2` or `md-5`), its own host gate, and its own tests. `rcmake-gen-xcode` depends on `base`, `diag`, `model`, `resolve`, `gen-common`, `host`, and `sha2`.
2. **Module layout.** Organize the crate into `plist.rs` (OpenStep writer), `ids.rs` (object IDs), `object.rs` (object graph), `scheme.rs`, `workspace.rs`, `rerun.rs` (`ReRunCMake.make`), `project.rs`, `settings.rs`, `phases.rs`, `build.rs` (`build_commands()`), and `lib.rs` (`generate()`).
3. **Shared contracts.**
   - Define `GeneratorKind::{Ninja, NinjaMultiConfig, UnixMakefiles, Xcode, VisualStudio(VsVersion)}` in `rcmake-model`. `Xcode` is multi-config, defaults `CMAKE_CONFIGURATION_TYPES` to `Debug;Release;MinSizeRel;RelWithDebInfo`, sets `CMAKE_CFG_INTDIR` to `$(CONFIGURATION)$(EFFECTIVE_PLATFORM_NAME)`, and accepts `-G Xcode` only on macOS.
   - Define `GeneratorSpec` and the pure parsers `parse_xcode_toolset`, `parse_vs_toolset`, and `parse_vs_platform` in `rcmake-model`. `GeneratorSpecError` formats directly to the CMake 4.5.1 message text, and the configure commands report it at the first `project()` call.
   - Add `ResolveOptions`, `resolve_with_options`, and `ResolvedTarget::ide` (`IdeFacts` in `rcmake-resolve/src/ide.rs`). `IdeFacts` stores raw, unescaped values and computes them only when `ResolveOptions::ide` names an IDE flavor, so Ninja and Makefile outputs never depend on `IdeFacts`.
   - Provide `utility_targets`, `project_roots`, `targets_for_root`, `rerun_inputs`, and the pure `cmake --build` types `BuildRequest` and `BuildCommand` in `rcmake-gen-common/src/ide.rs`. Every generator crate exports `build_commands(&BuildRequest) -> Result<Vec<BuildCommand>, GenError>`, and the CLI runs those commands in order, stopping at the first failure.
4. **Deterministic object IDs.** Compute each object ID as the first 24 uppercase hexadecimal digits of the SHA-256 digest of the 4-byte little-endian `PBXType` ordinal concatenated with the semantic key, excluding the build directory from the key. Cached `<target>_GUID_CMAKE` and `PROJECT_<Name>_GUID_CMAKE` entries take priority so a build tree created by the oracle keeps its IDs. Any ID collision returns a `GenError`.
5. **`try_compile` under Xcode.** Run `try_compile` through the in-process `rcmake-build` executor (ADR-008) rather than generating an Xcode project for each probe.
6. **Verification.** Verify across five tiers, starting with Tier X0 unit golden strings, Tier X1 structural diffs of canonical forms produced by `tools/measure/pbxproj_canon.py`, Tier X2 live `xcodebuild` build, `ctest`, and install runs, Tier X3 upstream `Tests/RunCMake/XcodeProject*` at 85% or higher, and Tier X4 passing all 9 Tier-1 and Tier-2 corpus projects.

### Consequences

- `rcmake` Xcode projects are reproducible, so a repeated configure leaves `project.pbxproj` unchanged and Xcode does not reload unnecessarily.
- Oracle comparison uses the canonical-form tool instead of raw byte diffs.
- One `IdeFacts` contract serves both IDE generators.
- `cmake -E capabilities` lists `Xcode` only on hosts where the generator is available.

---

## ADR-015 Native Visual Studio generator in rcmake-gen-vs

Accepted on 2026-11-24.

### Context

The CMake 4.5.1 oracle cannot create a Visual Studio generator on macOS or Linux, and neither host has MSBuild, so verification on those hosts must be structural.

### Decision

1. **Scope.** Implement `Visual Studio 17 2022` (`.sln`, toolset `v143`, `ToolsVersion` `17.0`) and `Visual Studio 18 2026` (`.slnx`, toolset `v145`, `ToolsVersion` `18.0`). Use a `VsVersion::{Vs17, Vs18}` enum so older versions can be added later. `Visual Studio 15 2017` and `16 2019` remain unimplemented.
2. **Crate.** `crates/rcmake-gen-vs` depends on `base`, `diag`, `model`, `resolve`, `gen-common`, `host`, and `md-5`. Its modules are `guid.rs`, `xml.rs`, `sln.rs`, `slnx.rs`, `filters.rs`, `vcxproj.rs`, `rules.rs`, `flags_table.rs`, `build.rs`, and `lib.rs`.
3. **Host gate.** Both generators are available on Windows. On other hosts, `-G "Visual Studio 17 2022"` prints the oracle error `Could not create named generator <name>` and exits with code 1. The `vs-cross` Cargo feature of `rcmake-model` (forwarded by `rcmake-cli/vs-cross`) enables the generators on every host strictly for structural tests. The default build and the staged release binary never enable `vs-cross`, and no environment variable overrides this gate.
4. **Deterministic GUIDs.** A cached `<name>_GUID_CMAKE` entry takes priority. Otherwise compute the uppercase RFC 4122 version-3 (MD5) UUID using namespace `ee30c4be-5192-4fb0-b335-722a2dffe760` over `"<CMAKE_BINARY_DIR>|<name>"`. This matches CMake 4.5.1 so `rcmake` reproduces oracle GUIDs exactly.
5. **Clean-room MSVC flag table (locked).** Author `flags_table.rs` by hand from documented MSVC command-line options. Never read, copy, or generate from Microsoft MSBuild rule files (`CL.xml`, `Link.xml`, `LIB.xml`), and never copy CMake's `cmVS*FlagTable` tables. Any unrecognized flag goes into `<AdditionalOptions>` alongside `%(AdditionalOptions)`, which is valid MSBuild with identical build behavior.
6. **Verification.** Run the structural tiers on every host, while Windows CI also builds native C and C++ projects with MSVC through Ninja.
   - Tier V0 checks unit golden strings for `guid.rs` UUID-v3 vectors, `sln.rs`, `slnx.rs`, `xml.rs`, `filters.rs`, and `build_commands()`.
   - Tier V1 checks XML well-formedness, element order, and `ProjectReference` GUIDs with `tools/measure/vs_canon.py`.
   - Tier V2 runs upstream `Tests/RunCMake` Visual Studio solution and `VS10Project` check scripts with oracle `cmake -P` against `vs-cross` output, passing at least 80% of cases that do not require MSBuild.
   - Tier V3 runs seeded configures using hand-written compiler files in `conformance/vs/seed/msvc-x64/`.

### Consequences

- Visual Studio project parity is structural. A live tier covering byte-level `.sln` and `.vcxproj` parity and MSBuild builds requires a new ADR.
- On non-Windows hosts, the release binary matches the oracle by refusing to create a Visual Studio generator.
- Because the flag table uses no Microsoft-licensed inputs, the crate stays under `MIT OR Apache-2.0`.

---

## ADR-016 Benchmarks, safe-Rust fuzzing, and release hardening

Accepted on 2026-11-24.

### Decision

1. **Benchmarks.** `tools/measure/bench_eval.py` uses only the Python standard library and runs each benchmark with both the oracle and the staged `rcmake` binary. By default it executes 1 untimed warm-up run and 5 timed runs, reporting median and 95th-percentile (p95) wall time along with peak resident set size (RSS) for each tool. Speedup is the oracle median divided by the `rcmake` median.
2. **Safe-Rust fuzzing only (locked).** `crates/rcmake-fuzz` is the only fuzzer. It runs on the stable toolchain in safe Rust (`#![forbid(unsafe_code)]`) with no new dependencies. `rcmake` avoids `libfuzzer-sys`, `cargo-fuzz`, and any out-of-workspace `fuzz/` directory so the repository adds no `NCSA`-licensed dependency.
   - Each fuzz input executes on its own thread with an 8 MiB stack inside `catch_unwind` and a wall-clock budget defaulting to 2 seconds. Any caught panic or timeout fails the run. The SplitMix64 generator `rcmake_fuzz::Rng` lets you replay every failure from its seed.
   - The binary accepts `rcmake-fuzz <surface> [--seed <s>] [--iterations <n>] [--budget-ms <ms>]` across 14 fuzz targets, namely `syntax`, `expr`, `regex`, `genex`, `json`, `presets`, `cps`, `archive`, `path`, `tolerant`, `binfmt`, `diagnostics`, `fortran`, and `store-frame`.
   - `rcmake_base::Limits::fuzz()` applies tighter caps only in the fuzzer (16 MiB values and file reads, 1,000,000 regex steps, and 2 seconds per input). Production code always uses `Limits::default()`, which rejects only input that would exhaust memory or time (ADR-036 and ADR-037).
   - The release gate runs 1,000,000 iterations per fuzz target with zero failures, while `scripts/gate.sh` runs a 10,000-iteration smoke pass. Every crash becomes a regression test in its owning crate before the fix lands.
   - The `adversarial_fuzz.rs` integration suite remains as the in-tree regression net.
3. **Host operations for file locks, safe publication, and streaming checksums.**
   - `Host::lock_file(path, timeout) -> Box<dyn FileLockGuard>` uses the safe, stable `std::fs::File::lock` and `try_lock` APIs from Rust 1.89 in `NativeHost` with no extra dependency, while `PureHost` uses an in-memory lock table. A lock that stays busy past the timeout returns `ErrorKind::TimedOut`.
   - `Host::rename_noreplace(from, to)` uses `rustix::fs::renameat_with` with `RenameFlags::NOREPLACE` (macOS `renameatx_np(RENAME_EXCL)` and Linux `renameat2`) in `NativeHost`, returning `ErrorKind::Unsupported` if the file system lacks support. `PureHost` checks and renames atomically under its state lock.
   - Both methods provide default implementations returning `ErrorKind::Unsupported` so wrapper hosts keep compiling, and the shared conformance suite tests both hosts.
   - `rcmake_base::hash::IncrementalHasher` (`HashAlgo::hasher()`, `update`, and `finish_hex`) provides streaming digests for CPack checksums and `cmake -E sha256sum` matching `HashAlgo::hex_digest` over the same bytes.
4. **Oracle path in Git worktrees.** `env/env.sh` exports `RCMAKE_ORACLE_DIR`. When the local `env/oracle/bin` directory is missing in a Git worktree, it falls back to `env/oracle` in the main checkout, located in the parent directory of `git rev-parse --git-common-dir`.
5. **Release hardening.** A panic hook in `rcmake-cli/src/main.rs` prints one `rcmake: internal error: ...` line and exits with code 1. The release profile initially kept `panic = "unwind"` (later updated by ADR-034). Two clean `scripts/stage.sh` runs must produce byte-identical binaries. `tools/measure/determinism.py` verifies that each generator writes identical files into two build directories after `<B>` replacement, and `tools/measure/licence_scan.py` must pass.

### Consequences

- Fuzzing catches panics, stack overflows, and hangs on the stable toolchain without coverage guidance. Adding coverage-guided fuzzing requires a new ADR.
- This decision introduces no external crates.

---

## ADR-017 Windows host services and 7-Zip archives

Accepted on 2026-11-24.

Use Windows job objects for process pipelines, assigning each child to a job while suspended and holding the job handle until output capture finishes. Deliver pipe output through a bounded channel to the deadline-checking thread. Compare file identities through safe handle APIs before a copy truncates its destination, and read the Windows version through a safe registry API.

Use `sevenz-rust2` for 7-Zip headers and standard compression methods. Use `ppmd-turbo` for PPMd because the optional PPMd dependency in `sevenz-rust2` lacks an allowlisted licence. Apply the existing archive path checks across all formats, keep the Windows dependencies under the Windows target configuration, and keep unsafe code forbidden in both the host and archive crates.

---

## ADR-018 Native compiler probe cache

Accepted.

Cache eligible compiler probe results by recipe, verifying observed inputs and required artifacts before reuse and rebinding supported paths when a result moves to another build directory.

During publication, inspect the existing and candidate entries under the store lock. Keep an existing valid entry, and replace a stale entry only if the new inputs and artifacts still pass verification. Scope replacement to the individual recipe key instead of using generation markers that invalidate an entire shard of keys.

Record bounded compiler search directory listings to detect added or removed files. Inside SDK listings, follow directory links, visit each canonical directory once, and record dangling links. Verify the contents of every file the probe reads. Within one configure run, reuse a file digest only when file identity, metadata, and change time (`ctime`) match, and re-hash the file whenever `ctime` is unavailable, recent, or lacks sub-second precision.

Remote recovery uses HTTPS and requires a local pin, verifying both the remote entry and its inputs before reuse. Uploads require explicit permission both to write and to export captured output, and either offline setting disables remote probe access.

Related source files are [probe lookup and publication](../crates/rcmake-commands/src/configure/try_compile/cache.rs), [store publication](../crates/rcmake-store/src/entry_publish.rs), [input verification](../crates/rcmake-store/src/stamp.rs), and [search listings](../crates/rcmake-store/src/stamp/tree.rs).

---

## ADR-019 Verified probe lookahead

Accepted.

Scan ahead for eligible compiler probes and run them inside separate scratch directories using a bounded worker pool. Each worker verifies the compiler command and its inputs. When configure reaches the original probe command, it uses the speculative result only if the recipe matches and all recorded evidence validates. Otherwise configure runs the probe in the foreground.

Related source files are [lookahead](../crates/rcmake-commands/src/configure/lookahead.rs), [worker pool](../crates/rcmake-commands/src/configure/probe_pool.rs), and [speculative probe execution](../crates/rcmake-commands/src/configure/try_compile/active/speculate.rs).

---

## ADR-020 Incremental configure replay in creation order

Accepted.

Record reusable configure state and observed inputs in a versioned snapshot. Verify the entry context and input trace before replaying an eligible root or subdirectory region, falling back to full evaluation if that check fails. Restore targets and tests in their original creation order, validating names and identifiers before inserting anything into the model.

Publish `CMakeFiles/rcmake/configure.v1.bin` after configure completes, and disable replay whenever configure requires live observation. See the complete replay conditions in the [environment variable reference](ENV_VARS.md#incremental-replay).

Related source files are [replay engine](../crates/rcmake-eval/src/journal/replay.rs), [model restoration](../crates/rcmake-model/src/project/replay.rs), and [configure session](../crates/rcmake-commands/src/configure/session/native.rs).

---

## ADR-021 FetchContent lockfiles and frozen reuse

Accepted.

Store FetchContent archives in a content-addressed cache alongside Git mirrors, and prefetch eligible pinned sources with a bounded worker pool. Support `off`, `auto`, `update`, and `frozen` lock modes, publishing lockfile updates only after configure succeeds.

In frozen mode, verify retained archive contents against the lock pin before reuse. Each download receipt binds the pin, declaration identity, generated wrapper, and download stamp. For a reused Git checkout, verify both the top-level directory and the pinned HEAD commit. Preserve explicit source overrides and dependency provider results. Offline mode uses only local sources and verified cached sources, treating Windows drive-letter file URLs as local sources.

Related source files are [fetch storage](../crates/rcmake-fetch/src/lib.rs), [configure integration](../crates/rcmake-commands/src/fetch_observe.rs), and [frozen checks](../crates/rcmake-commands/src/fetch_observe/frozen.rs).

---

## ADR-022 Native and compatibility command lines

Accepted.

Select the active tool from the invocation name. Invoking the binary as `rcmake` selects native mode, while CMake tool names and `RCMAKE_ORACLE_MODE=1` select compatibility mode with CMake diagnostic formatting. Native mode supports `--diagnostics`, `--profile`, `--no-probe-cache`, `--fetch-lock`, `--hermetic`, and the `explain` subcommand, and explicit CLI flags take precedence over their environment equivalents.

Hermetic mode audits environment reads, file reads, subprocesses, and network access through the host. Warn mode reports violations, and strict mode rejects them. Native profiles emit a trace, folded stacks, and an HTML flamegraph. The invocation log records the tool name and effective persona in separate fields.

Related source files are [persona](../crates/rcmake-cli/src/persona.rs), [native options](../crates/rcmake-cli/src/options/extensions.rs), [profile](../crates/rcmake-cli/src/profile.rs), [invocation log](../crates/rcmake-cli/src/exec_log.rs), and [hermetic policy](../crates/rcmake-eval/src/hermetic.rs).

---

## ADR-023 In-process configure API and evaluated editor queries

Accepted.

Run the shared configure engine through `rcmake_api::run_configure`. The caller controls the host, write mode, process policy, and optional cancellation token. The result always includes the model and can also carry a workspace index, overlay changes, and provenance, allowing a denied subprocess to still produce a partial result.

For Language Server Protocol queries, evaluate unsaved editor buffers through a read overlay, isolate configure writes in a separate overlay, and deny subprocesses. Answer queries with evaluated facts from the current snapshot, cancel or discard stale evaluations, and fall back to syntax-only results when current evaluated facts are unavailable.

Related source files are [configure API](../crates/rcmake-api/src/config.rs), [configure runner](../crates/rcmake-api/src/runner.rs), and [evaluated language server](../crates/rcmake-lsp/src/eval_provider.rs).

---

## ADR-024 WebAssembly configure engine

Accepted.

Build `rcmake-api` with `--no-default-features` for `wasm32-unknown-unknown` and `wasm32-wasip1`, using `PureHost` to supply files and scripted processes. The WebAssembly target configurations omit native store, archive, and system time zone dependencies. The smoke check compiles the bare target and runs the WebAssembly System Interface (WASI) example in Wasmtime, exercising target discovery, provenance, cancellation, and process policy.

Related source files are [API features](../crates/rcmake-api/Cargo.toml), [host features](../crates/rcmake-host/Cargo.toml), and [WebAssembly smoke check](../tools/measure/wasm_smoke.py).

---

## ADR-025 CUDA, Fortran, and Qt AUTOGEN build rules

Accepted.

Build rules for CUDA, Fortran, and Qt `AUTOGEN` come from evaluated target properties inside the shared build graph. For CUDA, resolve architecture policy and runtime selection, plan device links ahead of host links, and evaluate each link in its own context. For Fortran, order compilation from module producers and consumers, using dynamic dependencies for Ninja, module stamps for Make, and the same module dependency graph for in-process compiler checks.

Plan Qt `AUTOMOC`, `AUTOUIC`, and `AUTORCC` before final source resolution and emit the metadata required by their tool drivers. Resolve staging destinations from cross-toolchain settings, retain explicit file dependencies on the link rule, and generate the Microsoft Visual C++ module definition file before linking when `WINDOWS_EXPORT_ALL_SYMBOLS` requests it.

Related source files are [resolution pipeline](../crates/rcmake-resolve/src/pipeline.rs), [Fortran scanner](../crates/rcmake-fortran/src/lib.rs), and [language fixtures](../conformance/cases/languages/verify.py).

---

## ADR-026 ELF runtime dependency scans and install runtime paths

Accepted.

On Linux and FreeBSD, read Executable and Linkable Format (ELF) dynamic metadata in `file(GET_RUNTIME_DEPENDENCIES)`. Call the configured `objdump` command when provided, and use bounded `ldconfig` queries alongside platform fallback paths to locate system libraries. Traverse dependencies by file identity while enforcing RPATH and RUNPATH search rules, filters, and conflict checks.

Compute build and install runtime paths from the evaluated target and configuration, using the staged install destination where applicable. Whenever an installed ELF binary requires a runtime path rewrite, emit guarded `file(RPATH_CHECK)` and `file(RPATH_CHANGE)` calls in the install script while honoring runtime path skip and build-with-install settings.

Related source files are [ELF dependency scanner](../crates/rcmake-commands/src/file_path/runtime_dependencies/elf.rs), [install rule generation](../crates/rcmake-resolve/src/lower/install_rpath.rs), and [ELF runtime path operations](../crates/rcmake-commands/src/file_path/elf_rpath.rs).

---

## ADR-027 File API reply reuse by content hash

Accepted.

Name each File API reply file with its prefix followed by the first 20 hexadecimal digits of the SHA-256 digest of its serialized bytes. When a regular file with that name already exists, reuse it and mark it for retention so its timestamp stays intact. Write new replies through temporary files, publish the current index, and then remove obsolete files according to the reply retention rules.

The source file is [File API reply writer](../crates/rcmake-gen-common/src/fileapi.rs).

---

## ADR-028 CMake runtime policy warning summaries

Accepted.

Store warning summaries recorded from `cmake_policy(GET_WARNING)` in CMake 4.5.1 for policies `CMP0000` through `CMP0226`, while the policy registry supplies help titles. Both tables carry CMake attribution in the [third-party notices](../THIRD_PARTY_NOTICES.md).

Related source files are [warning summaries](../crates/rcmake-diag/src/policy_warnings.rs) and [policy registry](../crates/rcmake-diag/src/generated_policies.rs).

---

## ADR-029 Stable legacy regex install order

Accepted.

For the regular expression forms of `install_files()` and `install_programs()`, sort entries in the current source directory by byte order before running matches, and apply the shared regular expression step limit. Sorting makes the generated install rules reproducible across file systems.

The source file is [legacy install commands](../crates/rcmake-commands/src/compat/install.rs).

---

## ADR-030 Xcode per-language source flags

Accepted.

A target can mix languages that share one Xcode flag family, namely C and Objective-C under `OTHER_CFLAGS` or C++ and Objective-C++ under `OTHER_CPLUSPLUSFLAGS`. Because Xcode applies one family setting to every language in that family, sharing a setting sends the wrong flags to some sources whenever those languages need different flags.

Emit target-level settings when all language groups of the target share the same include directories, the same preprocessor definitions, and the same baseline flags within each family. Otherwise emit one `RCMAKE_<LANG>_COMPILE_FLAGS` build setting per language, reference `$(RCMAKE_<LANG>_COMPILE_FLAGS)` in the compiler flags of each source file for that language, and leave the family settings containing only `$(inherited)`. In that per-language mode, omit target-level include directories and definitions from the preferred language so each language carries its own values. If any configuration of a target requires per-language mode, use it across every configuration of that target.

Related source files are [compile settings](../crates/rcmake-gen-xcode/src/settings/compile.rs), [source file flags](../crates/rcmake-gen-xcode/src/project/sources.rs), and [target selection](../crates/rcmake-gen-xcode/src/project/targets.rs).

---

## ADR-031 Install strip guard for CMAKE_STRIP

Accepted.

Emit an install strip action only for executables, shared libraries, and module libraries, and only when `CMAKE_STRIP` is not false according to `rcmake_base::truth::is_off`. Values such as `""`, `OFF`, `NOTFOUND`, or `strip-NOTFOUND` produce no strip action, so `rcmake` never writes an `execute_process()` call with an empty or missing program.

The install script invokes the strip command only when `CMAKE_INSTALL_DO_STRIP` is true and the installed file exists and is not a symbolic link. On Darwin, pass `-x` for libraries, and pass `-u -r` for executables only after a bounded probe identifies the configured program as Apple `strip`.

Related source files are [strip action](../crates/rcmake-resolve/src/lower/install/strip.rs) and [install script rendering](../crates/rcmake-gen-common/src/install_script/strip.rs).

---

## ADR-032 Probe compiler write scope

Accepted.

A classified compiler or linker may write only its certified outputs and temporary files. The probe host passes those write paths to the memo host, which invalidates any tracked root that overlaps a write path.

Whenever the probe host cannot certify the write scope, including for compiler wrappers that write telemetry outside the probe directory, the memo host invalidates all tracked roots.

---

## ADR-033 Direct dependencies without an earlier record

Accepted.

`docs/PRIOR_ART.md` lists six direct dependencies that no earlier ADR named. Each licence is on the ADR-001 allowlist or offers an allowlisted option, and ADR-017 already covers the Windows host and 7-Zip design that uses four of them.

| Crate | Version | Licence | Used by | Decision |
| --- | --- | --- | --- | --- |
| `crc32fast` | `1.5.5` | `MIT OR Apache-2.0` | `rcmake-archive` | Calculate CRC-32 checksums for ZIP entries and 7-Zip PPMd data. `flate2`, `zip`, and `sevenz-rust2` already pull in this crate, so it adds no new code to the graph. |
| `process-wrap` | `10.0.2` | `Apache-2.0 OR MIT` | `rcmake-host`, Windows only | Assign each child process to a job object while suspended and terminate the job at the pipeline deadline. Enable only the `std` and `job-object` features. |
| `similar` | `2.7.0` | `Apache-2.0` | `rcmake-harness` only | Display line differences when harness output differs from oracle output. Release binaries exclude this crate. |
| `tempfile` | `3.27.0` | `MIT OR Apache-2.0` | `rcmake-host` (Unix and Windows), `rcmake-harness` | Create anonymous scratch files and unique temporary directories with safe permissions, including isolated sandboxes in the harness. |
| `winapi-util` | `0.1.11` | `Unlicense OR MIT` | `rcmake-host`, Windows only | Read file identities and the computer name through safe wrappers under the MIT option. |
| `winreg` | `0.56.0` | `MIT` | `rcmake-host`, Windows only | Read the Windows release and build number from the registry with default features disabled. |

The Windows crates stay inside `[target.'cfg(windows)'.dependencies]` and provide safe Win32 wrappers so `unsafe_code = "forbid"` holds across every `rcmake` crate. `THIRD_PARTY_NOTICES.md` attributes all six crates.

Related source files are [host manifest](../crates/rcmake-host/Cargo.toml), [archive manifest](../crates/rcmake-archive/Cargo.toml), [harness manifest](../crates/rcmake-harness/Cargo.toml), and [prior art](PRIOR_ART.md).

---

## ADR-034 Release builds abort on panic

Accepted. This record applies from the change that sets `panic = "abort"` in `[profile.release]` and adds the abort hooks below.

Building release binaries with `panic = "abort"` removes unwind tables and landing pads, shrinking the release `rcmake` binary by 3.29 MiB (18.8%). No production path depends on unwinding for correct output, because `AGENTS.md` forbids panics on reachable input paths and every `catch_unwind` or `join` error handler guards only against internal bugs.

Set `panic = "abort"` in `[profile.release]`. `[profile.release-dev]` and `[profile.size-probe]` inherit that setting, and `[profile.wasm-release]` already uses it. Keep `panic = "unwind"` in `[profile.dev]` and `[profile.fuzz]`. Cargo always compiles test targets with unwinding, so panic-recovery tests continue to run.

Keep the panic hook in `rcmake` so it writes `rcmake: internal error: <message> (<file>:<line>)` and preserves `RUST_BACKTRACE` behavior. Under `cfg(panic = "abort")`, the hook then writes `CMake Internal Error (please report a bug)` and exits with code 1, matching the unwind path. Install the same hook in `rcmake-lsp` so a panic never terminates with a bare `SIGABRT` (exit code 134) and no diagnostic. Retain the existing `catch_unwind` wrappers so tests and embedders that unwind remain protected.

### Consequences

- An internal bug in a worker thread now terminates the whole process with an internal error instead of recovering quietly in the configure probe pool, platform query prefetch, fetch prefetch pool, install workers, or ctest scheduler. After a worker bug, `ctest` stops without printing a summary or XML report, and an editor restarts `rcmake-lsp` if its evaluation thread panics.
- File locks use OS locks (`std::fs::File::lock` and `try_lock`), which the OS releases on process exit, so an abort never leaves a stale lock blocking the next run.
- The next run cleans up probe scratch directories. Store staging files, cpack scratch directories, and running child processes can remain after an abort, just as they do after `SIGKILL`.
- Stack overflows and out-of-memory errors abort under both panic strategies.

Related source files are [workspace profiles](../Cargo.toml), [CLI entry point](../crates/rcmake-cli/src/main.rs), [LSP entry point](../crates/rcmake-lsp/src/main.rs), [probe pool](../crates/rcmake-commands/src/configure/probe_pool.rs), [platform query prefetch](../crates/rcmake-commands/src/configure/platform_queries/prefetch.rs), [fetch pool](../crates/rcmake-fetch/src/pool.rs), [ctest scheduler](../crates/rcmake-ctest/src/schedule.rs), [install workers](../crates/rcmake-cli/src/install/parallel.rs), and [native file locks](../crates/rcmake-host/src/native.rs).

---

## ADR-035 Private local cache root

Accepted.

The local store and Git mirror reuse cached data read from the cache root. If another user can write inside that root or swap out the root or an ancestor directory, that user could plant cache entries that alter configure results. Both components therefore accept a cache root only when no other user can modify or replace it.

**Path resolution.** `Store::new` and `GitMirror::new` resolve the configured root once through `canonical_path`, canonicalizing the deepest existing ancestor from fresh data using `Host::canonicalize_fresh` (bypassing `MemoHost`) and appending any missing components. On POSIX hosts the resolved path contains no symbolic links. On Windows it contains no junctions or reparse points and strips the `\\?\` prefix. Every subsequent check and file operation uses that resolved path, and `Store::root` returns it.

**POSIX checks.** Every check reads fresh metadata with `Host::metadata_fresh` so a cache such as `MemoHost` cannot mask a directory change. The validator walks the resolved path from `/` downward.

- Every ancestor above the root must be a real directory rather than a symbolic link, owned by root or by the effective user. Any ancestor writable by group or other users must carry the sticky bit. As a result, a root under root-owned `/tmp` (mode `1777`) passes, whereas a root under a sticky directory owned by another user fails.
- The root itself must be a real directory owned by the effective user with no group or world write bits (`mode & 0o022` equals zero). Mode `0755` passes.
- Any lookup error on a component refuses the root, and a POSIX host that cannot report the effective user or owner also refuses the root.

**Creation.** Creating a missing root walks from `/` in the same way, creating each missing component with `Host::create_private_dir` so it starts at mode `0700` or narrower after the umask. Each component, including one created concurrently by another process (`AlreadyExists`), must pass validation before creating anything underneath it. An existing root keeps its mode.

**Later checks.** The initial check records the root's device and inode numbers. Before every later read or write, the store fetches one fresh metadata sample of the root and verifies that the directory, owner, and non-writable permissions remain unchanged. Because every ancestor passed the initial walk, only root or the effective user can rename or replace them. If the root disappears, the store recreates it with the full walk.

**Refusal.** Reading or writing inside a refused root returns a permission error that names both the problem and the fix, such as `chmod 700 <root>`, without reading any data from the directory. The probe cache treats the refusal as a miss, and fetch reports it as a cache warning. The store never alters the permissions of an existing directory, so a refused cache makes configure slower without making it wrong.

**Git mirror.** A mutable mirror creates or validates its shared root with the same walk. An offline mirror validates an existing root and treats a missing root as an offline miss. An immutable snapshot from `GitMirror::read_only_cache` skips the walk, leaving the caller responsible for making sure no process can modify it.

**Windows.** Because the host reports no owner or access control list (ACL) and new directories inherit their parent ACL, the store accepts a root only when it resolves inside the user profile.

- The root path must not contain `.` or `..`.
- Both the resolved root and each resolved profile directory (`%LOCALAPPDATA%` and `%USERPROFILE%`) come from fresh canonicalization that follows junctions and other reparse points, and the resolved root must lie inside a resolved profile directory.
- The root must match its own resolved path so operations use the exact path that passed validation.
- During creation, every component must still resolve to itself before creating the next component below it, blocking newly inserted junctions.

This policy assumes the default profile ACL, which permits access only to the user, administrators, and the system. Because the host cannot inspect ACLs, it cannot detect a profile subtree whose ACL grants write access to other users, so do not point `RCMAKE_CACHE_DIR` at such a subtree. Roots outside the profile, such as a CI workspace on another drive, are refused, so native cache tests on Windows must use a directory inside the profile. The Windows tests run against a simulated host, so native Windows behavior is not directly exercised by the test suite.

**WebAssembly.** The WebAssembly build omits the native store and Git mirror, and its virtual host has no other users.

**Remote credentials.** The remote cache client passes its URL, authorization headers, and credentials to `curl` through `curl --config -` on standard input (`rcmake_base::curl::CurlConfig`). Because they never appear on the command line or on disk, the client needs no private directory. Any header value containing a carriage return or line feed is rejected.

**Hosts.** `Host::effective_uid`, `Host::create_private_dir`, `Host::metadata_fresh`, and `Host::canonicalize_fresh` provide default implementations so wrapper hosts outside `rcmake-host` keep compiling, while every wrapper host in the workspace forwards them. `MemoHost` reads fresh metadata and canonical paths from its base, and `OverlayHost` captures one consistent snapshot of its base per evaluation. When `effective_uid` returns the default `None`, a POSIX host refuses every private root.

Frame footers remain unkeyed SHA-256 digests, because once no other user can write inside or replace the cache root, a key adds no further protection.

Related source files are [private directory checks](../crates/rcmake-store/src/private_dir.rs), [store](../crates/rcmake-store/src/lib.rs), [Git mirror](../crates/rcmake-fetch/src/git.rs), [host trait](../crates/rcmake-host/src/lib.rs), and [memo host](../crates/rcmake-host/src/memo.rs).

---

## ADR-036 Command argument caps and live variable budget

Accepted.

Limiting each individual value to 256 MiB still leaves several ways a script could exhaust memory.

- An unquoted 256 MiB list such as `a;a;...` holds 134 million elements and expands into one argument per element, consuming roughly 8 to 12 GB.
- One command call can accumulate many large arguments that each stay under the single-value limit.
- A script can copy a large value repeatedly into many variables, enclosing scopes, macro bodies, or cache entries.

`Limits` enforces four memory and count caps.

| Cap | Default | Fuzz | Largest known real use |
| --- | --- | --- | --- |
| `max_command_arguments` | 16,777,216 elements | 1,048,576 | About 200,000 files from `file(GLOB_RECURSE)` on LLVM or Qt sources |
| `max_command_argument_bytes` | 512 MiB | 32 MiB | About 20 MB for such a glob |
| `max_live_variable_bytes` | 4 GiB (1 GiB on 32-bit targets) | 256 MiB | About 20 MB for a large configure (LLVM, Qt 6, VTK) |
| `max_cache_bytes` | 1 GiB | 64 MiB | A few MB for a large `CMakeCache.txt` |

Every cap sits more than 10 times above the largest known real workload, so valid projects are unaffected.

**Arguments.** Before copying an argument for a command call, the evaluator reserves it against a shared per-call budget covering both short literals and every element of a split list. Exceeding argument count, argument bytes, expansion value size, or expansion nesting depth returns a typed `ExpandErrorKind::ResourceLimit`. That stops the command with a fatal error such as `Command arguments expanded to more than 16777216 elements.` without expanding any later arguments. Syntax errors still continue to later arguments, matching CMake.

**Lists.** Each `foreach` plan tracks one budget with the same caps, charging `ITEMS` copies, elements of named `LISTS` and `ZIP_LISTS`, and generated `ZIP_LISTS` variable names (`<prefix>_<n>`, including undefined lists) before allocating them. A `ZIP_LISTS` loop with zero iterations saves nothing. Otherwise it saves the prior value of each distinct loop variable once and reserves those bytes in the temporary call budget before copying, releasing each reservation as the saved value moves back into its variable. Every `list()` subcommand that splits a whole list enforces the same caps, and `list(APPEND)` and `list(PREPEND)` verify the exact joined size against the value limit before growing the value.

**Variables.** The variable stack tracks an estimated byte total across all active bindings in every scope, charging name length plus value length plus 64 bytes for a set binding, and name length plus 64 bytes for an unset binding. Because this estimate omits allocator overhead, it bounds live bindings rather than total process memory. Every write checks the projected total at the storage boundary before mutating state, covering `set`, `unset`, `PARENT_SCOPE`, `block(PROPAGATE)`, `return(PROPAGATE)`, language scope propagation, and direct evaluator writes. Arithmetic uses checked operations, subtracting the replaced binding's cost before adding the new cost and rejecting overflow. Bulk writes verify their projected totals before copying values, including captured scope propagation and `PARENT_SCOPE`. Multi-name propagation in `return(PROPAGATE)` and `block(PROPAGATE)` checks the running total after each write in caller order because a later unset or overwrite can free bytes needed by an earlier name. If that preflight fails, nothing changes. Because variable watches execute between individual writes during multi-name propagation and can mutate variables after preflight, each write checks the storage boundary again and stops propagation on refusal while leaving earlier committed names intact. Any refused write reports `Total variable storage would exceed the 4294967296-byte limit.` and stops.

**Calls.** Active calls share a temporary byte reservation on the evaluator drawn from the same variable budget, so variable writes see only the remaining room. Before copying, a macro call reserves memory for five items.

- The argument values, `ARGN`, and `ARGV`, including headroom for `CMP0219` to double every backslash.
- The replacement patterns (`${<param>}` and `${ARGV<n>}`).
- The slots of the copied command array before allocation.
- Both spellings of each command name, the argument slots, and the bracket arguments for every command in the copied body.
- Each substituted argument, sized before building the substitution.

Nested macros add to the same reservation, and each call releases its share upon return after its copied body finishes running. Function calls check the projected cost of `ARGV<n>`, named parameters, `ARGV`, and `ARGN` before copying arguments, stopping at the first refused binding and requiring the joined `ARGV` and `ARGN` values to fit within the single-value limit.

**Cache.** The cache tracks an estimated byte total across all entries, counting name, value, help text, extra properties such as `STRINGS`, and 128 bytes per entry. Every script-driven cache write verifies the projected total against `max_cache_bytes` before mutating the entry, and a refusal cancels the paired variable update. `find_package()` and `project()` stop at a refused entry before updating any related normal variable. Before writing its aliases, `project()` determines whether it will store each `CMAKE_PROJECT_*` entry. In a subdirectory where an alias with the same name (for a project named `CMAKE_PROJECT`) already defines the name, no cache entry is stored and no cache space is reserved. Every stored entry reserves its full cost as a floor on its key until insertion completes so a `variable_watch()` callback triggered by an alias write cannot consume that room even if it shrinks or deletes the entry. Each reservation carries a unique ID that only its owning `project()` call can release, preventing a nested `project()` inside a watch from releasing an outer reservation, and multiple reservations on one key count only the largest floor. In a subdirectory, `project()` rechecks the name after the alias writes just as CMake does, skipping storage if a watch defined it. If a name existed before the alias writes and a watch deleted it, the unreserved entry still passes through the standard cache check afterward, which may refuse the entry while preserving `max_cache_bytes`. The guarded write paths cover three entry points.

- `Cx::insert_cache_entry`, which records history for `set(CACHE)`, `option()`, `mark_as_advanced` placeholders, `get_filename_component(... CACHE)`, and legacy `build_command(<cachevar> <makecommand>)`.
- `Cx::insert_cache_entry_unrecorded`, which applies the same check without history for `site_name()`, `project()` values, `enable_language()` results, `try_compile()` and `try_run()` results, `find_package()` `<Pkg>_DIR`, generated target GUIDs, and each entry and `STRINGS` property imported by `load_cache()`, stopping at the first refused entry.
- `set_property(CACHE ...)` for every property, including `APPEND` and `APPEND_STRING`.

Two limits remain outside these caps. They do not count directory variable snapshots or target and directory properties, and initial cache entries loaded from the command line, startup `CMakeCache.txt`, or evaluator setup use the raw cache API bounded by file-read limits.

Related source files are [limits](../crates/rcmake-base/src/limits.rs), [list budget](../crates/rcmake-base/src/list.rs), [argument expansion](../crates/rcmake-expr/src/expand.rs), [command dispatch](../crates/rcmake-eval/src/interp/dispatch.rs), [foreach](../crates/rcmake-eval/src/flow/foreach.rs), [variable stack](../crates/rcmake-eval/src/scope.rs), [function and macro calls](../crates/rcmake-eval/src/interp/calls.rs), [cache](../crates/rcmake-model/src/lib.rs), and [cache properties](../crates/rcmake-commands/src/policy_misc/properties/set.rs).

---

## ADR-037 Native stack guard

Accepted.

`CMAKE_MAXIMUM_RECURSION_DEPTH` allows up to 20,000 levels, which leaves only 3.3 KB per level on a 64 MiB worker stack. One level of `cmake_language(EVAL CODE)` recursion consumes about 13.8 KB in debug builds and 5.4 KB in release builds. A nested `try_compile` interpreter inherits its caller's depth while pushing its own frames onto the same native stack. Without an explicit stack check, a script raising the depth limit could overflow the native stack and abort the process with a signal.

`rcmake_eval::stack_guard` estimates stack consumption in safe Rust by comparing the integer address of a local variable against the address captured at the start of the active budget. Because Rust does not guarantee exact frame layouts, this measurement is an estimate. `stack_guard::with_budget` installs a lexically scoped budget for a closure, clamping any nested budget to what remains of the outer budget and restoring the outer budget when the closure returns or unwinds. Every interpreter running on the same thread shares that thread's budget.

Two counters cap recursion, and both consult the stack guard.

- **Command depth.** Each command call checks the configured depth limit first so `Maximum recursion depth of <n> exceeded` stays unchanged, and then checks the guard, which reports `Maximum recursion depth exceeded: the native stack is full at depth <n>.`
- **Structured block nesting.** A block body (`if`, `foreach`, `while`, `block`, and related structured commands) executes after its opener's command frame returns, so command depth does not count it. Block nesting tracks its own counter under the same limit and guard, reporting `Maximum nesting depth of <n> exceeded by structured blocks.`

| Thread | Stack | Budget |
| --- | --- | --- |
| `rcmake` worker | 64 MiB | 56 MiB |
| Speculative probe worker | 16 MiB | 12 MiB |
| LSP evaluation | 32 MiB | 24 MiB |
| API caller thread without a budget | caller's (2 MiB Rust default) | 1 MiB |

In the API case, the caller's thread must have at least 2 MiB of free stack, though a caller with a larger stack can pass a higher budget to `with_budget`. If the `rcmake` worker thread cannot start, `rcmake` prints an internal error and exits with code 1. The main thread never runs the evaluator because its stack size is unknown.

The headroom above each budget reserves space for native recursion between guard checks, covering variable and condition expressions (depth 256), `math` (depth 256), generator expression parsing (depth 256) and evaluation (depth 128), and JSON and syntax parsing. Those reserves are estimates, because only the recursion paths listed in `docs/ARCHITECTURE.md` are directly measured.

On `wasm32` targets a stack address does not measure the call stack, so the build omits the guard. `with_budget` simply invokes its closure and never reports a full stack. Instead, the WebAssembly entry point caps both the recursion limit and its ceiling (`Limits::max_recursion_ceiling`) at 200 levels so `CMAKE_MAXIMUM_RECURSION_DEPTH` cannot raise it, and applies the same ceiling to structured block nesting.

`HARD_RECURSION_CEILING` stays at 20,000 because it also bounds per-level heap allocations such as backtrace frames.

Related source files are [stack guard](../crates/rcmake-eval/src/stack_guard.rs), [command dispatch](../crates/rcmake-eval/src/interp/dispatch.rs), [block bodies](../crates/rcmake-eval/src/flow/mod.rs), [CLI entry point](../crates/rcmake-cli/src/main.rs), [probe pool](../crates/rcmake-commands/src/configure/probe_pool.rs), [API runner](../crates/rcmake-api/src/runner.rs), [WebAssembly entry point](../crates/rcmake-wasm/src/project.rs), and [LSP evaluation thread](../crates/rcmake-lsp/src/stdio_eval.rs).

---

## ADR-038 GNU make jobserver access

Accepted.

CTest reads GNU make tokens through `Host::open_jobserver`. Unix hosts open a FIFO or reopen inherited pipe descriptors using safe file APIs. One test uses the implicit token, and each additional test returns its pipe token upon completion. Hosts without a jobserver fall back to the local parallel limit.
