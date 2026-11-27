# rcmake environment variables

`rcmake` selects its mode from `argv[0]`. Running as `rcmake` enables native mode unless `RCMAKE_ORACLE_MODE=1` forces compatibility mode. Running as `cmake`, `ccmake`, `ctest`, or `cpack` always selects CMake 4.5.1 compatibility mode and ignores native speed settings. When both a command-line option and an environment variable control the same setting, the command-line option wins.

## Contents

- [Configure, probe cache, and incremental replay](#configure-probe-cache-and-incremental-replay)
  - [Incremental replay](#incremental-replay)
- [Remote probe cache](#remote-probe-cache)
- [FetchContent, hermetic checks, and archive extraction](#fetchcontent-hermetic-checks-and-archive-extraction)
- [Command-line output, CPack, and data discovery](#command-line-output-cpack-and-data-discovery)
- [Parity verification and conformance harness](#parity-verification-and-conformance-harness)
- [CTest script state, measurements, and test settings](#ctest-script-state-measurements-and-test-settings)

## Configure, probe cache, and incremental replay

| Variable | Effect |
| --- | --- |
| `RCMAKE_PROBE_CACHE` | Select `readwrite` (default), `readonly`, `verify`, or `off`. The values `1` and `on` map to `readwrite`, and `0` maps to `off`. In `verify` mode, each hit is compared against a fresh probe and configure uses the fresh result. |
| `RCMAKE_NO_PROBE_CACHE` | Set to `1` to disable the probe cache, or `0` to keep `RCMAKE_PROBE_CACHE`. |
| `RCMAKE_CACHE_DIR` | Override the shared storage root for the probe cache and FetchContent. Relative paths resolve from the invocation directory, and an empty value uses the OS cache directory. |
| `RCMAKE_PROBE_CACHE_DIR` | Override the probe storage root separately from `RCMAKE_CACHE_DIR`. Relative paths resolve from the invocation directory. |
| `RCMAKE_CACHE_MAX_SIZE` | Set the shared store size limit in bytes (default 2 GiB). The Git mirror cache has its own cap. |
| `RCMAKE_PROBE_JOBS` | Request a positive integer number of concurrent probe jobs including the foreground job (`1` runs serially). Defaults to the smaller of 4 and logical CPUs, capped at logical CPUs, 32, and available memory (reserving 2 GiB for configure plus 1 GiB per job, or 1 job if RAM size is unknown). |
| `RCMAKE_NO_INCREMENTAL` | Set to `1` to disable incremental configure replay. |
| `RCMAKE_REPLAY_SETUP` | Set to `0` to disable incremental replay when toolchain files, configure presets, `-C` scripts, or `PreLoad.cmake` are active. By default (`0.2.0` and later), setup checkpoints allow sound replay with those inputs. |

An invalid probe cache mode turns the probe cache off, and an invalid job count falls back to the default, printing a warning in native mode.

### Incremental replay

Native-mode incremental replay turns off for the invocation when any of these conditions hold.

- `--fresh`, `--debugger`, `--trace*`, `--profile`, `RCMAKE_PROFILE`, or CMake-format profiling is active.
- `--check-system-vars`, `--log-context`, `--debug-output`, or `--debug-find*` is active.
- Hermetic mode is active, `RCMAKE_PROBE_CACHE=verify`, `RCMAKE_NO_INCREMENTAL=1`, or a command observer, annotation collector, provenance query, or step budget is active.
- `RCMAKE_REPLAY_SETUP=0` is set and a toolchain file, configure preset, `-C` script, or `PreLoad.cmake` is active.

Both `--diagnostics=cmake` and `--diagnostics=rich` allow replay.

## Remote probe cache

| Variable | Effect |
| --- | --- |
| `RCMAKE_CACHE_REMOTE` | Set the HTTPS endpoint URL. Empty or unset disables remote access. |
| `RCMAKE_CACHE_REMOTE_AUTH` | Select `anonymous` (default), `bearer`, `header`, or `s3`. The `s3` mode reads `AWS_REGION`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, and optional `AWS_SESSION_TOKEN`. |
| `RCMAKE_CACHE_REMOTE_TOKEN` | Supply the bearer token or custom HTTP header value. |
| `RCMAKE_CACHE_REMOTE_HEADER` | Supply the custom HTTP header name for `header` authentication. |
| `RCMAKE_CACHE_REMOTE_WRITE` | Set to `1` to allow uploads. Uploads also require `RCMAKE_CACHE_REMOTE_EXPORT_CAPTURED_OUTPUT=1`. |
| `RCMAKE_CACHE_REMOTE_EXPORT_CAPTURED_OUTPUT` | Set to `1` alongside `RCMAKE_CACHE_REMOTE_WRITE=1` to authorize uploading captured compiler output. |
| `RCMAKE_CACHE_REMOTE_EXPORT_TIMEOUT` | Set the upload timeout in positive seconds (client default 300 seconds, further bounded by configure's 3-second remote budget). |
| `RCMAKE_CACHE_REMOTE_CURL` | Set the absolute path to `curl` (defaults `/usr/bin/curl` and `C:/Windows/System32/curl.exe`). |
| `RCMAKE_OFFLINE` | Set to `1` to disable remote probe cache access. |

## FetchContent, hermetic checks, and archive extraction

| Variable | Effect |
| --- | --- |
| `RCMAKE_FETCH_LOCK` | Select `auto` (default), `off`, `update`, or `frozen`. Invalid values warn and select `off` in native mode. |
| `RCMAKE_FETCH_LOCK_FILE` | Override the lockfile path (default `rcmake-fetch.lock` in the source directory). |
| `RCMAKE_FETCH_OFFLINE` | Set to `1` to disable remote probe access and FetchContent or ExternalProject network transfers while keeping local and cached sources. |
| `RCMAKE_FETCH_PREFETCH` | Set to `0` to disable native prefetch. Prefetch runs only when lock mode is not `off`, hermetic mode is `off`, and online fetches are permitted. |
| `RCMAKE_FETCH_JOBS` | Request positive fetch worker count (default 4, clamped to 1 through 32). |
| `RCMAKE_FETCH_GIT_READ_ONLY` | Set to `1` with `RCMAKE_FETCH_OFFLINE=1` to treat the Git mirror cache as immutable. |
| `RCMAKE_FETCH_GIT_MAX_SIZE` | Set the Git mirror cache size limit in bytes (default 2 GiB). |
| `RCMAKE_HERMETIC` | Select `off` (default, or `0`), `warn` (or `1`), or `strict`. |
| `RCMAKE_HERMETIC_ALLOW_ENV` | Semicolon-separated list of permitted environment variable names in hermetic mode. |
| `RCMAKE_HERMETIC_ALLOW_PATHS` | Semicolon-separated list of resolvable permitted absolute file or directory paths. |
| `RCMAKE_HERMETIC_ALLOW_PROGRAMS` | Semicolon-separated list of resolvable permitted absolute executable paths. |
| `RCMAKE_EXTRACT_MAX_BYTES` | Override the archive extraction output size cap in bytes (default 200x compressed size clamped between 1 GiB and 64 GiB, hard ceiling 64 GiB). |

Invalid hermetic allowlist entries print a warning and are ignored.

## Command-line output, CPack, and data discovery

| Variable | Effect |
| --- | --- |
| `RCMAKE_ORACLE_MODE` | Set to `1` to force compatibility mode even when invoked as `rcmake`. |
| `RCMAKE_DIAGNOSTICS` | In native mode, select `cmake` or `rich`. Defaults to `rich` on a terminal stderr and `cmake` otherwise. |
| `NO_COLOR`, `CLICOLOR_FORCE` | Non-empty `NO_COLOR` disables ANSI color. `CLICOLOR_FORCE=1` forces color in rich mode and overrides `NO_COLOR`. |
| `RCMAKE_NO_HINTS` | Set to `1` to suppress trailing native `find_package()` hint lines on stderr after failed lookups. |
| `RCMAKE_PROFILE` | Set the native configure profile output directory (writes `flamegraph.html`, `trace.json`, `stacks.folded`, `run-*`, and `current`). |
| `RCMAKE_EXEC_LOG` | Write JSON Lines invocation records (`tool` and effective `persona`) to this path. |
| `RCMAKE_VENDORED_DATA_ROOT` | Override the bundled CMake 4.5.1 data root (`Modules/`, `Templates/`, `Help/`). Verified by checking `Modules/CMakeDetermineSystem.cmake`. |
| `RCMAKE_CPACK_TOOL_TIMEOUT` | Optional timeout in seconds for external CPack installer tools (`pkgbuild`, `productbuild`, `makensis`, `wix`, `candle`, `light`, `binarycreator`). Unset matches CMake with no timeout. |

## Parity verification and conformance harness

The core `rcmake` binary never reads `RCMAKE_ORACLE_CMAKE` or executes upstream `cmake`. `RCMAKE_VERIFY_TIMEOUT` sets the deadline forwarded by `rcmake verify`, while the remaining variables below apply strictly to the isolated `rcmake-verify` companion binary and `rcmake-harness`.

| Variable | Effect |
| --- | --- |
| `RCMAKE_ORACLE_CMAKE` | Companion-only default path to a reference CMake 4.5.1 executable read by `rcmake-verify` when `--oracle` is omitted, and by `rcmake-harness` and measurement scripts. |
| `RCMAKE_VERIFY_TIMEOUT` | Default overall wall-clock timeout in seconds for `rcmake verify` (default 600, overridden by `--timeout`). |
| `RCMAKE_VERIFY_NINJA` | Absolute path to an independent trusted `ninja` executable outside the source and scratch trees for `rcmake-verify` dry-run checks. |
| `RCMAKE_ROOT` | Repository root used by `rcmake-harness` to locate test cases, baselines, and staged tools (default current directory). |
| `RCMAKE_ORACLE_BIN_DIR` | Directory containing reference CMake 4.5.1 binaries for `rcmake-harness` oracle runs. |

## CTest script state, measurements, and test settings

| Variable | Effect |
| --- | --- |
| `RCMAKE_CTEST_SCRIPT_DEPTH` | Internal nesting counter passed to child CTest script processes (cap 32). Leave unset for normal runs. |
| `RCMAKE_MEASURE_REPLAY` | Set to `1` to record executed and replayed listfiles in `CMakeFiles/rcmake/replay-result.v1.json` after a native configure. |
| `RCMAKE_MEASURE_PROBE_INTERVALS` | Set to `1` to log verified speculative probe timing intervals to `CMakeFiles/rcmake-probe-intervals.txt`. |
| `RCMAKE_MEASURE_ENTRY`, `RCMAKE_MEASURE_ARM`, `RCMAKE_MEASURE_CHECKPOINT` | Interruption-test hooks that pause a numbered cache entry publication for up to 60 seconds behind an arm file and checkpoint symlink. |
| `RCMAKE_FUZZ_SCALE` | Multiply random CLI integration test case count (default 1, clamped 1 to 100). |
| `RCMAKE_ARCHIVE_INTEROP_CMAKE`, `RCMAKE_CTEST_COMPARISON`, `RCMAKE_NINJA_TEST_CMAKE`, `RCMAKE_TEST_EXECUTABLE`, `RCMAKE_TEST_REFERENCE_CMAKE`, `RCMAKE_TEST_REFERENCE_CPACK` | Optional reference executables used only by ignored or optional Rust integration tests. |
| `RCMAKE_TEST_INDEX_LOCK_BUILD`, `RCMAKE_VS_TEST_OUTPUT` | Internal test paths for instrumentation index locking and Visual Studio fixture output. |

Internal unit tests also read fixture-scoped variables (`RCMAKE_HOST_CONFORMANCE`, `RCMAKE_HIDDEN`, `RCMAKE_VALUE`, `RCMAKE_TEST_UNDECLARED`, `RCMAKE_VAR_TEST`, and `RCMAKE_M6_*` wrapper logs).
