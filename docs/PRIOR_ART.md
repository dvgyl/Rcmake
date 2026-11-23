# Prior Art & Dependency Licence Log

Every external crate or vendored third-party asset adopted by Rcmake is recorded here before or at adoption time, with its exact licence, scope, and clean-room rationale.

## Vendored Third-Party Assets

| Asset | Upstream Version | Licence | Location | Purpose |
| --- | --- | --- | --- | --- |
| CMake Standard Modules & Templates (`Modules/`, `Templates/`) | CMake 4.5.1 | `BSD-3-Clause` (with Kitware/OSI copyright headers preserved) | `modules/cmake-4.5/` | Standard CMake module library (`GNUInstallDirs`, `CheckCCompilerFlag`, `Platform/*`, `Compiler/*`, `Find*`, `FetchContent`, etc.) |

## Rust Crate Dependencies

All crates conform to the permissive allow-list in ADR-001 (`MIT`, `Apache-2.0`, `Apache-2.0 WITH LLVM-exception`, `BSD-2-Clause`, `BSD-3-Clause`, `ISC`, `Zlib`, `Unicode-3.0`, `0BSD`).

| Crate | Version | Licence | Scope | Rationale |
| --- | --- | --- | --- | --- |
| `bstr` (`std`, `alloc`; no `unicode`) | `1.12` | `MIT OR Apache-2.0` | Core engine (`rcmake-base`+) | Byte-oriented CMake values without UTF-8 panics |
| `memchr` | `2` | `Unlicense OR MIT` | Core engine (`rcmake-base`+) | Fast byte scanning for list splitting & lexer |
| `indexmap` | `2` | `MIT OR Apache-2.0` | Core engine | Deterministic insertion-ordered maps |
| `thiserror` | `2` | `MIT OR Apache-2.0` | Core engine | Ergonomic structured error types |
| `jiff` | `0.2` | `Unlicense OR MIT` | `rcmake-host`, `rcmake-base` | Multithread-safe local timezone & timestamp formatting |
| `rustix` (`fs`, `process`, `system`) | `1` | `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | `rcmake-host` | Safe `uname`, process groups, and POSIX syscalls without `unsafe` in Rcmake |
| `serde` / `serde_json` (no `preserve_order`) | `1` | `MIT OR Apache-2.0` | `rcmake-harness` only | Baseline manifest serialization in isolated test harness |
| `similar` | `2` | `Apache-2.0` | `rcmake-harness` only | Readable diff output on oracle mismatches |
| `tempfile` | `3` | `MIT OR Apache-2.0` | `rcmake-harness` & tests | Isolated `/tmp` test sandboxes |
| `md-5`, `sha1`, `sha2`, `sha3` | `0.10` | `MIT OR Apache-2.0` | `rcmake-base` | Pure-Rust cryptographic hashes for `string(<HASH>)`, `file(<HASH>)`, and `cmake -E *sum` |
| `flate2` (default `miniz_oxide`) | `1` | `MIT OR Apache-2.0` | `rcmake-archive` | Pure-Rust gzip compression/decompression |
| `tar` | `0.4` | `MIT OR Apache-2.0` | `rcmake-archive` | Pure-Rust POSIX/GNU tar creation & extraction |
| `zip` (`deflate` only, no default features) | `2` | `MIT` | `rcmake-archive` | Pure-Rust ZIP archive creation & extraction without copyleft/bzip2 defaults |
