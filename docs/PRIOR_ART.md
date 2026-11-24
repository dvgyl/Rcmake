# Prior Art and Dependency Licence Log

This log records the licences and purpose of the vendored assets and direct workspace dependencies. The crate versions and licence expressions come from the locked Cargo graph.

## Vendored Third-Party Assets

| Asset | Upstream version | Licence | Location | Purpose |
| --- | --- | --- | --- | --- |
| CMake standard modules and templates | CMake 4.5.1 | `BSD-3-Clause` | `modules/cmake-4.5/` | Supply the standard CMake module library and templates. The original copyright notices stay with the files. |

## Direct Third-Party Dependencies

The licence expression lists the upstream terms. Rcmake uses an option on the scanner's allowlist when the expression offers a choice. [Third-party notices](../THIRD_PARTY_NOTICES.md)

| Crate | Locked version | SPDX licence | Purpose |
| --- | --- | --- | --- |
| `bstr` | `1.14.0` | `MIT OR Apache-2.0` | Store and handle CMake strings that can contain invalid UTF-8. |
| `bzip2` | `0.6.1` | `MIT OR Apache-2.0` | Read and write bzip2 streams with the Rust `libbz2-rs-sys` backend. |
| `flate2` | `1.1.11` | `MIT OR Apache-2.0` | Read and write gzip streams with the Rust `miniz_oxide` backend. |
| `indexmap` | `2.14.2` | `Apache-2.0 OR MIT` | Keep map entries in insertion order. |
| `jiff` | `0.2.46` | `Unlicense OR MIT` | Read the local time zone and format timestamps. |
| `liblzma` | `0.4.9` | `MIT OR Apache-2.0` | Read and write xz streams with the bundled `liblzma` library. |
| `md-5` | `0.10.6` | `MIT OR Apache-2.0` | Calculate MD5 hashes and generate stable Visual Studio identifiers. |
| `memchr` | `2.8.3` | `Unlicense OR MIT` | Find bytes for list parsing and other byte operations. |
| `rustix` | `1.1.6` | `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | Give the host safe access to POSIX system calls, including `uname` and process-group operations. |
| `serde` | `1.0.229` | `MIT OR Apache-2.0` | Define the test harness data that can be read from or written to a file. |
| `serde_json` | `1.0.154` | `MIT OR Apache-2.0` | Read and write JSON manifests in the test harness. |
| `sha1` | `0.10.7` | `MIT OR Apache-2.0` | Calculate SHA-1 hashes for CMake commands. |
| `sha2` | `0.10.9` | `MIT OR Apache-2.0` | Calculate SHA-2 hashes and generate stable Xcode identifiers. |
| `sha3` | `0.10.9` | `MIT OR Apache-2.0` | Calculate SHA-3 hashes for CMake commands. |
| `similar` | `2.7.0` | `Apache-2.0` | Show text differences when test harness output differs from CMake output. |
| `tar` | `0.4.46` | `MIT OR Apache-2.0` | Read and write tar archives. |
| `tempfile` | `3.27.0` | `MIT OR Apache-2.0` | Create isolated temporary directories for the test harness. |
| `thiserror` | `2.0.21` | `MIT OR Apache-2.0` | Derive Rust error implementations for structured error types. |
| `zip` | `2.6.1` | `MIT` | Read and write ZIP archives with Deflate support. |
| `zstd` | `0.14.4` | `BSD-3-Clause` | Read and write Zstandard streams with the bundled Zstandard library. |

## Archive Codec Dependencies

| Component | Locked version | SPDX licence | Purpose |
| --- | --- | --- | --- |
| `zstd-safe` | `8.3.0` | `BSD-3-Clause` | Give `zstd` safe access to the Zstandard library. |
| `zstd-sys` | `2.1.3+zstd.1.5.7` | `BSD-3-Clause AND BSD-2-Clause` | Compile and bind Zstandard 1.5.7. Rcmake uses the C library under its BSD option. |
| `liblzma-sys` | `0.4.9` | `MIT OR Apache-2.0` | Compile and bind the XZ Utils `liblzma` library. |
| XZ Utils `liblzma` C sources | `5.8` | `0BSD` | Supply the xz codec in `src/liblzma` and `src/common/tuklib_*`. |
| `libbz2-rs-sys` | `0.2.7` | `bzip2-1.0.6` | Supply the Rust port of libbzip2. |
| `cc`, `jobserver`, `find-msvc-tools`, `shlex`, `pkg-config` | `1.11.3`, `0.1.35`, `0.1.15`, `2.0.1`, `0.3.34` | `MIT OR Apache-2.0` | Configure and compile the bundled C codec sources. |

## Internal Workspace Dependencies

The direct path dependencies in the root manifest are Rcmake crates. Each uses `MIT OR Apache-2.0`.

| Crate | Purpose |
| --- | --- |
| `rcmake-archive` | Create, list, and extract archives. |
| `rcmake-base` | Provide shared primitives for strings, lists, versions, hashes, and resource limits. |
| `rcmake-build` | Run build graphs in process for compiler probes and build fallbacks. |
| `rcmake-commands` | Implement built-in CMake commands. |
| `rcmake-cpack` | Generate archives and macOS packages. |
| `rcmake-cps` | Read, validate, and write Common Package Specification files. |
| `rcmake-ctest` | Discover and run tests. |
| `rcmake-cxxmodules` | Collect C++20 named module dependencies and generate module maps. |
| `rcmake-diag` | Report diagnostics and track CMake policies. |
| `rcmake-eval` | Run CMake scripts and manage their scopes and control flow. |
| `rcmake-expr` | Expand variables and evaluate conditions. |
| `rcmake-gen-common` | Write outputs shared by the build system generators. |
| `rcmake-gen-make` | Generate Unix Makefiles. |
| `rcmake-gen-ninja` | Generate Ninja and Ninja Multi-Config build files. |
| `rcmake-gen-vs` | Generate Visual Studio projects and solutions. |
| `rcmake-gen-xcode` | Generate Xcode projects and schemes. |
| `rcmake-genex` | Parse and evaluate generator expressions. |
| `rcmake-host` | Give the engine access to files, processes, the environment, and time. |
| `rcmake-instrument` | Collect and report CMake instrumentation data. |
| `rcmake-json` | Parse and write JSON with JsonCpp-compatible behavior. |
| `rcmake-model` | Store project state, target properties, and cache entries. |
| `rcmake-presets` | Load and resolve CMake presets. |
| `rcmake-regex` | Match CMake regular expressions and convert glob patterns. |
| `rcmake-resolve` | Resolve target dependencies and construct build graphs. |
| `rcmake-syntax` | Parse CMake listfiles and preserve their input bytes. |
| `rcmake-tool-e` | Run `cmake -E` utilities. |
