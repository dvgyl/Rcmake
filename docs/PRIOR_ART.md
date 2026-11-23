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
| `zstd` / `zstd-safe` (no default features) | `0.14` / `8.3` | `BSD-3-Clause` | `rcmake-archive` | Zstandard codec for `TZST`, `file(ARCHIVE_CREATE COMPRESSION Zstd)`, and `cmake -E tar --zstd` (ADR-010) |
| `zstd-sys` (bundled Zstandard 1.5.7 C library, used under its BSD option) | `2.1` | `BSD-3-Clause AND BSD-2-Clause` | `rcmake-archive` (build) | Zstandard C sources compiled with `cc`; the GPLv2 alternative of the C library is not used |
| `liblzma` / `liblzma-sys` (`static`, no `bindgen`) | `0.4` | `MIT OR Apache-2.0` | `rcmake-archive` | xz codec for `TXZ`, `COMPRESSION XZ`, and `cmake -E tar J` (ADR-010); maintained fork of the rejected `xz2` |
| XZ Utils `liblzma` C sources (bundled in `liblzma-sys`) | `5.8` | `0BSD` | `rcmake-archive` (build) | Only `src/liblzma` and `src/common/tuklib_*` compile; the GPL/LGPL tools and scripts of XZ Utils are not built |
| `bzip2` (default backend) | `0.6` | `MIT OR Apache-2.0` | `rcmake-archive` | bzip2 codec for `TBZ2`, `COMPRESSION BZip2`, and `cmake -E tar j` (ADR-010) |
| `libbz2-rs-sys` | `0.2` | `bzip2-1.0.6` | `rcmake-archive` | Pure-Rust port of libbzip2 (no C compiler, no system `libbz2`); licence accepted by ADR-010 |
| `cc`, `jobserver`, `find-msvc-tools`, `shlex`, `pkg-config` | `1` / `0.1` / `0.1` / `2` / `0.3` | `MIT OR Apache-2.0` | Build dependencies of `zstd-sys` and `liblzma-sys` | Compile the bundled C codec sources |
