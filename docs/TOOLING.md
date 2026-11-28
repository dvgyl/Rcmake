# Native developer tooling

Invoking the binary as `rcmake` unlocks opt-in diagnostic, formatting, linting, comparison, and verification commands. Compatibility mode (`cmake`, `ccmake`, `ctest`, `cpack`, or `RCMAKE_ORACLE_MODE=1`) rejects these subcommands and flags and writes no native metadata files. Build-time regenerations of a native build directory preserve the native persona automatically.

## Contents

- [Provenance and inspection with rcmake explain](#provenance-and-inspection-with-rcmake-explain)
- [Build tree comparison with rcmake diff-config](#build-tree-comparison-with-rcmake-diff-config)
- [Lossless formatting and static linting](#lossless-formatting-and-static-linting)
- [Side-by-side parity checks with rcmake verify](#side-by-side-parity-checks-with-rcmake-verify)
- [Multi-configuration compile databases](#multi-configuration-compile-databases)
- [Interactive cache editor and DAP debugger](#interactive-cache-editor-and-dap-debugger)
- [Profiling, hermetic audits, FetchContent lockfiles, and diagnostics](#profiling-hermetic-audits-fetchcontent-lockfiles-and-diagnostics)
- [Language server and Rust API](#language-server-and-rust-api)

## Provenance and inspection with `rcmake explain`

`rcmake explain` evaluates a configured build directory in memory without spawning external processes or modifying files on disk. Pass `-B <build>` if the build directory is not the current directory, `--json` for structured `rcmake-explain-v1` output, or `--color=auto`, `--color=always`, or `--color=never`.

### Target and variable provenance

Query a target to trace where its compile options, preprocessor definitions, include directories, and link libraries originate across `PRIVATE`, `PUBLIC`, and `INTERFACE` usage chains. Query a variable to trace every write across listfiles, functions, macros, cache entries, and `-D` options. Narrow a target query with one typed filter (`--flag`, `--define`, `--include`, `--dep`, or `--filter`) and optional `--config <cfg>`.

```sh
rcmake explain app -B build
rcmake explain app -B build --flag -O2 --config Release
rcmake explain app -B build --define USE_TLS
rcmake explain app -B build --include /opt/local/include
rcmake explain app -B build --dep zlib --config Release
rcmake explain CMAKE_CXX_FLAGS -B build --var
```

Use `--flag=--sysroot=...` when a flag starts with `--`. Dependency path searches (`--dep`) cover link and order-only edges and stop at 16 paths, depth 32, or 20,000 edges, reporting when the search is capped.

### Package lookup diagnosis

Inspect how `find_package()` resolved a dependency or why it rejected candidate config files.

```sh
rcmake explain --find OpenSSL -B build
rcmake explain why-not Boost -B build --all-candidates
```

When a fresh native trace exists at `CMakeFiles/rcmake/find-trace.v1.json`, `rcmake explain` reads it directly. Otherwise it re-evaluates the project in memory. Pass `--call <n>` (1-based) when a project calls `find_package()` for the same package more than once, or `--fresh` to ignore a saved trace.

### Generator expression evaluation

Evaluate an arbitrary generator expression or a target property against a configured build directory and inspect each intermediate step.

```sh
rcmake explain --eval '$<$<CONFIG:Debug>:-DTRACE>' -B build --config Debug
rcmake explain app --eval-property COMPILE_OPTIONS -B build --lang CXX --no-steps
```

Wrap expressions in single quotes so the shell does not expand `$<`, or pass `@<file>` to read up to 64 KiB from disk. Context overrides accept `--target <name>`, `--config <cfg>`, `--lang <lang>`, `--link-lang <lang>`, `--directory <dir>`, `--link`, `--command-config <cfg>`, and `--output-config <cfg>`.

## Build tree comparison with `rcmake diff-config`

`rcmake diff-config` compares two configured build directories produced by either `rcmake` or official `cmake` without invoking `cmake`. It reads CMake File API replies, `CMakeCache.txt`, `compile_commands.json`, and saved native find traces.

```sh
rcmake diff-config build-main build-pr
rcmake diff-config --preset debug --preset release -S . --only compile,link
rcmake diff-config build-a build-b --config Debug:Release --target app --json
```

Use `--only` with any comma-separated subset of `targets`, `compile`, `link`, `toolchain`, `packages`, and `cache`. Control cache noise with `--cache key` (default), `--cache all`, or `--cache none`, and keep absolute paths with `--no-normalize`.

Complete target, compile, and link diffs need CMake File API replies. Run `--prepare` before configuring to plant the four stateless File API query files (`codemodel-v2`, `cache-v2`, `toolchains-v1`, `cmakeFiles-v1`) into one or more build directories.

```sh
rcmake diff-config --prepare build-main build-pr
```

`rcmake diff-config` exits with code 0 when no differences are found and all requested sections have complete evidence, code 1 when differences exist, and code 2 on invalid arguments or incomplete File API evidence.

## Lossless formatting and static linting

`rcmake fmt` and `rcmake lint` share one concrete-syntax parser, discovery engine, and `.rcmake.toml` configuration with `rcmake-lsp`. Recursive discovery skips `.git`, `.hg`, `.svn`, `.bzr`, `CMakeFiles`, `Testing`, nested build trees, AppleDouble `._*` files, symbolic links, and project `exclude` patterns.

### Formatting with `rcmake fmt`

`rcmake fmt` edits only whitespace and optional ASCII command identifier case. Before writing any file, an independent verifier confirms AST equivalence, unchanged protected tokens, and preserved comment attachment. In-place formatting refuses to overwrite direct symbolic link operands.

```sh
rcmake fmt                         # format project in place
rcmake fmt --check                 # exit 1 if any file needs formatting
rcmake fmt --diff                  # print unified diffs without writing
rcmake fmt --stdin --stdin-filepath cmake/deps.cmake < cmake/deps.cmake
```

### Static analysis with `rcmake lint`

`rcmake lint` checks `CMakeLists.txt` and `*.cmake` files against 8 built-in static rules.

| Rule ID | Default severity | What it catches |
| --- | --- | --- |
| `undefined-variable` | `warning` | Variable reference is unset or likely misspelled |
| `glob-without-configure-depends` | `warning` | `file(GLOB)` or `file(GLOB_RECURSE)` feeding target sources omits `CONFIGURE_DEPENDS` |
| `single-config-condition` | `warning` | Condition inspects `CMAKE_BUILD_TYPE` without a single-config generator guard |
| `unguarded-build-type` | `warning` | Unconditional assignment or `FORCE` cache overwrite masks `CMAKE_BUILD_TYPE` |
| `directory-options` | `note` | Directory-wide legacy commands (`include_directories`, `link_libraries`, `add_definitions`) affect all targets in scope |
| `obsolete-command` | `warning` | Obsolete or removed commands and unsupported `cmake_minimum_required` versions |
| `old-policy` | `warning` | Policy set to `OLD` via `cmake_policy(SET CMPnnnn OLD)` |
| `fragile-if-expansion` | `warning` | Unquoted variable expansion in `if()` or `elseif()` where empty or list values break arity |

Run `rcmake lint --list-rules` to print all rules, `--deny-warnings` to exit with code 1 on warnings, and `--sarif` to emit SARIF 2.1.0 JSON for CI code scanning.

```sh
rcmake lint --deny-warnings --sarif > rcmake-lint.sarif
```

Suppress one rule on a single command with `# rcmake-lint: disable=<rule>` on the line before the command, inside its parentheses, or trailing on its closing line. Unknown or malformed directives emit `invalid-lint-directive`.

### Project configuration in `.rcmake.toml`

Both `rcmake fmt` and `rcmake lint` walk upward inside the project root for the nearest `.rcmake.toml`.

```toml
exclude = ["third_party/**", "generated/*.cmake"]

[fmt]
indent_width = 2        # 1 to 16, default 2
line_width = 100        # 20 to 1000, default 100
command_case = "lower"  # "preserve" (default), "lower", or "upper"
end_of_line = "lf"      # "preserve" (default), "lf", or "crlf"

[lint]
disable = ["directory-options"]

[lint.severity]
glob-without-configure-depends = "error"
old-policy = "note"
```

## Side-by-side parity checks with `rcmake verify`

`rcmake verify` tests whether `rcmake` matches an upstream CMake 4.5.1 installation on your project without modifying your working directory.

```sh
rcmake verify --oracle /path/to/cmake-4.5.1 -- -S . -G Ninja
rcmake verify --timeout 300 --json -- -S . -G Ninja -DCMAKE_BUILD_TYPE=Release
```

To preserve the clean-room runtime boundary, `rcmake` itself never discovers or executes upstream `cmake`. Instead, `rcmake verify` validates configure arguments and passes a typed request to the isolated sibling binary `rcmake-verify`. That companion binary resolves `--oracle` (falling back to `RCMAKE_ORACLE_CMAKE` strictly inside `rcmake-verify`), stages clean copies of the source tree into a private scratch directory, configures with both engines under a shared deadline (`--timeout` or `RCMAKE_VERIFY_TIMEOUT`, default 600 seconds), and compares configure status, File API codemodel, `CMakeCache.txt`, `compile_commands.json`, and Ninja dry-run output.

`rcmake verify` exits with code 0 on full parity, code 1 when differences appear, and code 2 on invalid arguments, missing `rcmake-verify`, unsupported oracle versions, matched configure failures, or incomplete evidence.

## Multi-configuration compile databases

Upstream CMake exports a single `compile_commands.json` only for Makefile and Ninja generators (`Ninja` and `Ninja Multi-Config`), omitting `Xcode`, `Visual Studio`, and per-configuration split databases. In native mode, passing `--export-compile-commands` or setting `CMAKE_EXPORT_COMPILE_COMMANDS=ON` generates per-configuration compile databases across `Ninja Multi-Config`, `Xcode`, and `Visual Studio`.

```sh
rcmake -S . -B build -G "Ninja Multi-Config" --export-compile-commands
```

`rcmake` writes one `compile_commands_<Config>.json` file per configuration alongside a root `compile_commands.json` matching the default configuration. It records owned file digests in `CMakeFiles/rcmake/compile-commands-v1.json` under a lock so externally edited files are never overwritten or deleted. Compatibility mode stays identical to CMake 4.5.1 and omits these native files.

## Interactive cache editor and DAP debugger

### Terminal cache editor (`ccmake`)

On macOS and Linux, `ccmake` (or `rcmake ccmake`) launches a clean-room safe-Rust terminal cache editor with no `ncurses` dependency. It runs configure (`c`) and generate (`g`) in separate phases over a retained in-process configure session, captures child output in a bounded scrollback buffer, and restores terminal raw mode on exit. See [docs/man/ccmake.1](man/ccmake.1) for all options and key bindings. Windows terminal support is deferred in `0.2.1`.

### Debug Adapter Protocol server (`--debugger`)

On macOS and Linux, `--debugger`, `--debugger-pipe=<path>`, and `--debugger-dap-log=<file>` start a CMake 4.5.1 Debug Adapter Protocol server over a Unix domain socket for configure runs and `-P` scripts. `-E capabilities` reports `"debugger": true` on Unix. Incremental replay turns off automatically whenever a debugger attaches so breakpoints always pause live execution. Windows named-pipe transport is deferred in `0.2.1`.

## Profiling, hermetic audits, FetchContent lockfiles, and diagnostics

- **Configure profiling (`--profile[=<dir>]`).** Writes `trace.json`, `stacks.folded`, and an interactive `flamegraph.html` into `<dir>` (default `<build>/rcmake-profile`) while archiving each run under `run-<id>/`.
- **Rich terminal diagnostics (`--diagnostics=rich` or `--diagnostics=cmake`).** Defaults to `rich` on a terminal in native mode, rendering source spans and suggested fixes, and falls back to `cmake` format when stderr is redirected.
- **Hermetic input auditing (`--hermetic=warn` or `--hermetic=strict`).** Reports or rejects undeclared environment reads, file reads outside project and tool roots, child processes, and network access. Configure allowlists with `RCMAKE_HERMETIC_ALLOW_ENV`, `RCMAKE_HERMETIC_ALLOW_PATHS`, and `RCMAKE_HERMETIC_ALLOW_PROGRAMS`.
- **FetchContent lockfiles (`--fetch-lock=off`, `auto`, `update`, or `frozen`).** Pins archive hashes and Git commits in `rcmake-fetch.lock`. In native online mode, prefetch can download already-declared pinned sources ahead of `FetchContent_MakeAvailable()`.
- **Cache sharing and drift checks.** Use `rcmake cache export` and `rcmake cache import` to share compiler probes across CI runners, and `rcmake doctor -B <dir>` to detect stale `CMakeCache.txt` drift without running compilers. See [docs/CACHING.md](CACHING.md) for both workflows.

## Language server and Rust API

- **Language server (`rcmake-lsp` or `rcmake lsp`).** Provides hover with in-memory evaluated values, completion, go to definition, document symbols, semantic tokens, quick fixes, lossless `.rcmake.toml` formatting, and all 8 static `rcmake lint` rules over stdio. See [crates/rcmake-lsp/README.md](../crates/rcmake-lsp/README.md) for VS Code, Neovim, Helix, Zed, Emacs, and CLion setup.
- **Embedded Rust and WebAssembly API (`rcmake-api`).** Runs configure and `explain` in process over `rcmake_host::Host` and compiles to `wasm32-unknown-unknown` and `wasm32-wasip1` with `default-features = false`. See [crates/rcmake-api/README.md](../crates/rcmake-api/README.md).
