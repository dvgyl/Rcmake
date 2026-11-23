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
