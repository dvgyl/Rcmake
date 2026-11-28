# Prior art and dependency licence log

The crate versions and licence expressions below come from the locked Cargo graph for `rcmake` 0.2.1.

## Contents

- [Vendored third-party assets](#vendored-third-party-assets)
- [Direct third-party dependencies](#direct-third-party-dependencies)
- [Archive codec dependencies](#archive-codec-dependencies)
- [Internal workspace crates](#internal-workspace-crates)
- [Windows host and 7-Zip support](#windows-host-and-7-zip-support)

## Vendored third-party assets

| Asset | Upstream version | Licence | Location | Purpose |
| --- | --- | --- | --- | --- |
| CMake standard modules and templates | CMake 4.5.1 | `BSD-3-Clause` | `modules/cmake-4.5/Modules/`, `modules/cmake-4.5/Templates/` | Supply standard CMake modules and templates with original copyright notices intact |
| Rendered command-line help and reference manuals | CMake 4.5.1 | `BSD-3-Clause` | `crates/rcmake-cli/help/usage.txt`, `modules/cmake-4.5/Help/reference.zst` | Supply bundled CMake help for the installed tool |
| Policy warning summaries | CMake 4.5.1 | `BSD-3-Clause` | `crates/rcmake-diag/src/policy_warnings.rs` | Supply runtime warning text for `cmake_policy(GET_WARNING)` |
| Policy registry | CMake 4.5.1 | `BSD-3-Clause` | `crates/rcmake-diag/src/generated_policies.rs` | Supply policy metadata and titles from CMake help output |
| Language server catalog | CMake 4.5.1 | `BSD-3-Clause` | `crates/rcmake-lsp/src/generated_catalog.rs` | Supply summaries and signatures from CMake help and module docs for editor queries |
| Target property initializer table | CMake 4.5.1 | `BSD-3-Clause` | `crates/rcmake-commands/src/targets/generated_init.rs` | Supply target property and initializer variable pairs from CMake help |

The [third-party notices](../THIRD_PARTY_NOTICES.md#1-cmake-451-modules-templates-help-and-derived-tables-bsd-3-clause) list the CMake licence and contributor files.

## Direct third-party dependencies

Each licence expression records the upstream terms. When an expression offers a choice, `rcmake` selects an option on the licence scanner allowlist. Full licence texts live in [THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md).

| Crate | Locked version | SPDX licence | Purpose |
| --- | --- | --- | --- |
| `bstr` | `1.14.0` | `MIT OR Apache-2.0` | Store and manipulate CMake byte strings that may contain non-UTF-8 bytes |
| `bzip2` | `0.6.1` | `MIT OR Apache-2.0` | Read and write bzip2 streams via the pure-Rust `libbz2-rs-sys` backend |
| `flate2` | `1.1.11` | `MIT OR Apache-2.0` | Read and write gzip streams via the Rust `miniz_oxide` backend |
| `indexmap` | `2.14.2` | `Apache-2.0 OR MIT` | Preserve insertion order for maps |
| `jiff` | `0.2.46` | `Unlicense OR MIT` | Read local time zones and format timestamps using the system zone database |
| `liblzma` | `0.4.9` | `MIT OR Apache-2.0` | Read and write xz streams via bundled `liblzma` |
| `md-5` | `0.10.6` | `MIT OR Apache-2.0` | Compute MD5 digests and stable Visual Studio GUIDs |
| `memchr` | `2.8.3` | `Unlicense OR MIT` | Fast byte search for list parsing and scanning |
| `rustix` | `1.1.6` | `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | Safe POSIX syscalls for filesystem, `uname`, process groups, and `termios` raw mode in `ccmake` |
| `serde` | `1.0.229` | `MIT OR Apache-2.0` | Serialize structured records for the test harness, evaluator journal, and probe store |
| `serde_json` | `1.0.154` | `MIT OR Apache-2.0` | Read and write test harness manifests and evaluator journal records |
| `sha1` | `0.10.7` | `MIT OR Apache-2.0` | Compute SHA-1 hashes for CMake commands |
| `sha2` | `0.10.9` | `MIT OR Apache-2.0` | Compute SHA-2 hashes, input digests, and stable Xcode identifiers |
| `sha3` | `0.10.9` | `MIT OR Apache-2.0` | Compute SHA-3 hashes for CMake commands |
| `similar` | `2.7.0` | `Apache-2.0` | Render unified diffs in `rcmake fmt --diff`, `rcmake-harness`, and `rcmake-verify` |
| `tar` | `0.4.46` | `MIT OR Apache-2.0` | Read and write tar archives with extended-attribute default features disabled |
| `tempfile` | `3.27.0` | `MIT OR Apache-2.0` | Create isolated temporary directories for `rcmake-harness` and `rcmake-verify` |
| `thiserror` | `2.0.21` | `MIT OR Apache-2.0` | Derive structured error implementations |
| `zip` | `2.6.1` | `MIT` | Read and write ZIP archives with the `flate2` Deflate backend |
| `zstd` | `0.14.4` | `BSD-3-Clause` | Read and write Zstandard streams via bundled Zstandard |

Archive feature choices omit `zopfli`, `bumpalo`, and `xattr` from the lockfile. On non-MSVC AArch64 targets, `sha2` enables `sha2-asm` (0.6.4, `MIT OR Apache-2.0`) for ARMv8 SHA-2 instructions.

## Archive codec dependencies

| Component | Locked version | SPDX licence | Purpose |
| --- | --- | --- | --- |
| `zstd-safe` | `8.3.0` | `BSD-3-Clause` | Safe Rust wrapper around `zstd-sys` |
| `zstd-sys` | `2.1.3+zstd.1.5.7` | `BSD-3-Clause AND BSD-2-Clause` | Compile and bind Zstandard 1.5.7 under its BSD licence |
| `liblzma-sys` | `0.4.9` | `MIT OR Apache-2.0` | Compile and bind XZ Utils `liblzma` |
| XZ Utils `liblzma` C sources | `5.8` | `0BSD` | Supply xz codec in `src/liblzma` and `src/common/tuklib_*` |
| `libbz2-rs-sys` | `0.2.7` | `bzip2-1.0.6` | Rust port of libbzip2 |
| `cc`, `jobserver`, `find-msvc-tools`, `shlex`, `pkg-config` | `1.11.3`, `0.1.35`, `0.1.15`, `2.0.1`, `0.3.34` | `MIT OR Apache-2.0` | Build helpers for bundled C codec sources |

## Internal workspace crates

All 40 workspace crates use `MIT OR Apache-2.0` and enforce `#![forbid(unsafe_code)]`. The 35 shared library crates appear in `[workspace.dependencies]` of root `Cargo.toml`, alongside five leaf crates (`rcmake-cli`, `rcmake-shim`, `rcmake-harness`, `rcmake-fuzz`, and `rcmake-wasm`).

| Crate | Purpose |
| --- | --- |
| `rcmake-api` | In-process configure, workspace index, provenance, and `explain` queries |
| `rcmake-archive` | Create, list, extract, and validate archives and portable cache tarballs |
| `rcmake-base` | Shared byte strings, lists, versions, hashes, lexical paths, and limits |
| `rcmake-build` | In-process build-graph execution for probes and build fallback |
| `rcmake-buildinfo` | Bounded File API, cache, compile-database, and find-trace readers and comparator |
| `rcmake-ccmake` | Clean-room Unix terminal cache editor (`ccmake`) with no `ncurses` |
| `rcmake-commands` | Built-in CMake commands, `project()`, `try_compile()`, probe cache, and replay |
| `rcmake-cpack` | Archives, Linux `DEB` and `RPM`, macOS `Bundle`, `DragNDrop`, `productbuild`, `WIX`, `NSIS`, `NSIS64`, and `IFW` |
| `rcmake-cps` | Common Package Specification 0.15.0 reader, validator, and writer |
| `rcmake-ctest` | Test discovery, parallel scheduling, jobserver access, and reporting |
| `rcmake-cxxmodules` | C++20 named module dependency scanning, `dyndep`, and module maps |
| `rcmake-dap` | CMake 4.5.1 `--debugger` Debug Adapter Protocol server on Unix sockets and Windows named pipes |
| `rcmake-diag` | Diagnostics, backtraces, SARIF 2.1.0, and policy registry |
| `rcmake-eval` | Script interpreter, scopes, control flow, replay checkpoints, and debug hooks |
| `rcmake-expr` | Variable expansion, escape decoding, and `if()` condition evaluation |
| `rcmake-fetch` | FetchContent archive cache, Git mirrors, and lockfiles |
| `rcmake-fortran` | Pure-Rust Fortran source scanning and `.mod` dependency collation with no disk access |
| `rcmake-gen-common` | Shared File API, multi-config `compile_commands.json`, install, CTest, and export outputs |
| `rcmake-gen-make` | `Unix Makefiles` and `NMake Makefiles` generators |
| `rcmake-gen-ninja` | `Ninja` and `Ninja Multi-Config` generators |
| `rcmake-gen-vs` | `Visual Studio 17 2022` and `18 2026` generators |
| `rcmake-gen-xcode` | `Xcode` project and scheme generator |
| `rcmake-genex` | Generator expression parser and evaluator |
| `rcmake-host` | `Host` trait with `NativeHost` and `PureHost` backends |
| `rcmake-instrument` | Instrumentation API v1 queries, snippets, hooks, and traces |
| `rcmake-json` | JsonCpp-compatible JSON parser and writers |
| `rcmake-language-tools` | `.rcmake.toml` loader, lossless formatter (`fmt`), and 8-rule static linter (`lint`) |
| `rcmake-lsp` | Language Server Protocol server for CMake listfiles |
| `rcmake-model` | Arena-based project state, targets, properties, and cache |
| `rcmake-presets` | CMake Presets schema v13 loader and resolver |
| `rcmake-regex` | KWSys regular expression engine and glob converter |
| `rcmake-resolve` | Target resolution, transitive usage graphs, CUDA, HIP, Swift, RPATH, and `BuildGraph` lowering |
| `rcmake-shim` | Tiny dependency-free safe-Rust Windows launcher shim forwarding `cmake`, `ctest`, `cpack`, and `rcmake-lsp` to `rcmake.exe` |
| `rcmake-store` | Content-addressed probe store, input stamps, GC, and portable exchange |
| `rcmake-syntax` | Raw-byte listfile parser and lossless concrete token stream |
| `rcmake-tool-e` | Built-in `cmake -E` utilities and Fortran and Qt tool drivers |

## Windows host and 7-Zip support

| Dependency | Locked version | Licence | Use |
| --- | --- | --- | --- |
| `winapi-util` | `1.1.11` | `Unlicense OR MIT` | Read file identity and computer name through safe Windows APIs |
| `winreg` | `0.56.0` | `MIT` | Read Windows release and build numbers from the registry |
| `process-wrap` | `10.0.2` | `Apache-2.0 OR MIT` | Attach suspended child processes to Windows job objects for clean timeout termination |
| `sevenz-rust2` | `0.25.0` | `Apache-2.0` | Read 7-Zip headers and encode or decode standard compression methods |
| `ppmd-turbo` | `0.2.2` | `Apache-2.0` | Encode and decode PPMd blocks in 7-Zip archives |
| `crc32fast` | `1.5.5` | `MIT OR Apache-2.0` | Compute CRC-32 checksums in 7-Zip PPMd archives |
| `tokio` | `1.48.0` | `MIT` | Safe Windows `--debugger` named-pipe transport (ADR-050) and Windows `Ctrl+C`/`Ctrl+Break` instrumentation interrupt handling |
| `mio` | `1.2.5` | `MIT` | Run the I/O completion port of the `tokio` named-pipe wrapper (ADR-050) |
| `socket2` | `0.6.5` | `MIT OR Apache-2.0` | Transitive Windows networking helper pulled by `tokio` `net` feature (ADR-050) |
| `signal-hook-registry` | `1.4.8` | `Apache-2.0 OR MIT` | Transitive signal registry dependency of `tokio` `signal` feature |

See the [7-Zip format specification](https://github.com/ip7z/7zip/blob/main/DOC/7zFormat.txt) for PPMd container headers.
