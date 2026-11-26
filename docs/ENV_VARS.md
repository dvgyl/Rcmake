# rcmake environment variables

`rcmake` runs in two modes. Invoking the binary as `rcmake` selects native mode unless `RCMAKE_ORACLE_MODE=1` forces compatibility mode. Invoking it as `cmake`, `ctest`, or `cpack` always selects compatibility mode, which matches CMake 4.5.1. Project configure applies the native speed settings below only in native mode.

When a command-line option and an environment variable control the same setting, the command-line option takes precedence.

## Configure and probe cache

| Variable | Effect |
| --- | --- |
| `RCMAKE_NO_INCREMENTAL` | Set to `1` to disable configure replay. Evaluation still follows the project's control flow and cache settings. |
| `RCMAKE_PROBE_CACHE` | Select `readwrite` (the default), `readonly`, `verify`, or `off`. The values `1` and `on` select `readwrite`, and `0` selects `off`. |
| `RCMAKE_NO_PROBE_CACHE` | Set to `1` to disable the probe cache. Set to `0` to keep the mode from `RCMAKE_PROBE_CACHE`. |
| `RCMAKE_PROBE_CACHE_DIR` | Override the probe storage root. An empty value uses the shared cache root. Relative paths start at the invocation directory. |
| `RCMAKE_PROBE_JOBS` | Request a positive integer number of probe jobs, including the foreground job. Setting `1` runs probes in series. |
| `RCMAKE_CACHE_DIR` | Override the shared storage root for the probe cache and FetchContent. An empty value uses the system cache location. Relative paths start at the invocation directory. |
| `RCMAKE_CACHE_MAX_SIZE` | Set the store size limit in bytes. The default is 2 GiB. The Git mirror cache has a separate limit. |

In `verify` mode, `rcmake` compares each cache hit against a fresh probe run and uses the fresh result for configure.

The default probe job count is the smaller of 4 and the logical CPU count. The worker pool also caps the count at the logical CPU count, at 32 jobs, and by available memory. It reserves 2 GiB for configure and allows 1 GiB per job. If the system memory size is unknown, the pool allows 1 job.

An invalid probe cache mode or disable value turns off the probe cache, and an invalid job count falls back to the default. In native mode, `rcmake` prints a warning for each invalid setting.

### Incremental replay

The following inputs and options disable native-mode configure replay.

- A configure preset or an explicit `-C` preload script.
- A `PreLoad.cmake` file in the source or build directory.
- `--toolchain`, or `CMAKE_TOOLCHAIN_FILE` set from `-D`, the saved cache, or the environment.
- `--fresh`, trace options, or an active profile, including `--profile`, `RCMAKE_PROFILE`, and CMake-format profiling.
- `--check-system-vars`, `--log-context`, `--debug-output`, `--debug-find`, `--debug-find-var`, or `--debug-find-pkg`.

The configure engine also disables replay when hermetic mode is active, when the probe cache mode is `verify`, or when a command observer is present. Collecting annotations or provenance, or setting a step budget, also disables replay. Both `--diagnostics=cmake` and `--diagnostics=rich` allow replay.

## Remote probe cache

| Variable | Effect |
| --- | --- |
| `RCMAKE_CACHE_REMOTE` | Set the HTTPS endpoint. An empty or unset value disables remote access. |
| `RCMAKE_CACHE_REMOTE_WRITE` | Set to `1` to permit uploads. Uploads also require `RCMAKE_CACHE_REMOTE_EXPORT_CAPTURED_OUTPUT=1`. |
| `RCMAKE_CACHE_REMOTE_EXPORT_CAPTURED_OUTPUT` | Set to `1` to permit uploads that include captured probe output. Uploads also require `RCMAKE_CACHE_REMOTE_WRITE=1`. |
| `RCMAKE_CACHE_REMOTE_EXPORT_TIMEOUT` | Set the upload time limit to a positive integer number of seconds. The remote client defaults to 300 seconds, and configure further limits each upload to its remaining remote budget, which starts at 3 seconds. |
| `RCMAKE_CACHE_REMOTE_CURL` | Set the absolute path to a `curl` executable. The defaults are `/usr/bin/curl` and `C:/Windows/System32/curl.exe`. |
| `RCMAKE_CACHE_REMOTE_AUTH` | Select `anonymous` (the default), `bearer`, `header`, or `s3`. |
| `RCMAKE_CACHE_REMOTE_TOKEN` | Supply the bearer token or the value of the custom HTTP header. |
| `RCMAKE_CACHE_REMOTE_HEADER` | Supply the custom HTTP header name for `header` authentication. |
| `RCMAKE_OFFLINE` | Set to `1` to disable the remote probe cache. FetchContent reads `RCMAKE_FETCH_OFFLINE` for its own offline setting. |

The `s3` authentication mode reads `AWS_REGION`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, and the optional `AWS_SESSION_TOKEN`.

## Fetch and hermetic input checks

| Variable | Effect |
| --- | --- |
| `RCMAKE_FETCH_LOCK` | Select `auto` (the default), `off`, `update`, or `frozen`. In native mode, an invalid value prints a warning and selects `off`. The standalone fetch parser returns an error instead. |
| `RCMAKE_FETCH_LOCK_FILE` | Select the lockfile path. Relative paths start at the project source directory. An empty or unset value uses `rcmake-fetch.lock`. |
| `RCMAKE_FETCH_OFFLINE` | Set to `1` to disable remote probe access and network transfers through FetchContent and ExternalProject. FetchContent still uses local sources and verified cached sources. |
| `RCMAKE_FETCH_PREFETCH` | Set to `0` to disable prefetch. Prefetch runs only when the fetch lock mode is not `off`, hermetic mode is `off`, and online fetches are allowed. |
| `RCMAKE_FETCH_JOBS` | Request a positive integer number of fetch workers. The default is 4, and the pool clamps the count to 1 through 32. In native mode, an invalid value prints a warning and selects the default. |
| `RCMAKE_FETCH_GIT_READ_ONLY` | Set to `1` with `RCMAKE_FETCH_OFFLINE=1` to use an immutable Git mirror cache. |
| `RCMAKE_FETCH_GIT_MAX_SIZE` | Set the Git mirror cache size limit in bytes. The default is 2 GiB. Offline and read-only cache instances leave mirrors intact. |
| `RCMAKE_HERMETIC` | Select `off` (the default), `warn`, or `strict`. The value `0` selects `off`, and `1` selects `warn`. Warn mode reports undeclared inputs, and strict mode rejects them. In native mode, an invalid value prints a warning and selects `off`. |
| `RCMAKE_HERMETIC_ALLOW_ENV` | Set a semicolon-separated list of permitted environment variable names. |
| `RCMAKE_HERMETIC_ALLOW_PATHS` | Set a semicolon-separated list of resolvable, permitted absolute paths. |
| `RCMAKE_HERMETIC_ALLOW_PROGRAMS` | Set a semicolon-separated list of resolvable, permitted absolute executable paths. |
| `RCMAKE_EXTRACT_MAX_BYTES` | Override the archive extraction output size limit in bytes. The default is 200 times the compressed archive size, clamped between 1 GiB and 64 GiB. Any higher value clamps to the 64 GiB hard ceiling. |

`rcmake` reports invalid allowlist entries and ignores them.

## Command-line output and data discovery

| Variable | Effect |
| --- | --- |
| `RCMAKE_ORACLE_MODE` | Set to `1` to force compatibility mode, even when the executable name is `rcmake`. |
| `RCMAKE_DIAGNOSTICS` | In native mode, select `cmake` or `rich`. An invalid or unset value selects rich output when standard error goes to a terminal and CMake formatting otherwise. Compatibility mode always uses CMake formatting. |
| `RCMAKE_PROFILE` | Set the native-mode configure profile directory. Relative paths start at the invocation directory, and an empty value disables the setting. Each run writes `flamegraph.html`, `trace.json`, and `stacks.folded` in `<dir>`, stores each run in `<dir>/run-*`, and records the latest run directory name in `<dir>/current`. `--profile=<dir>` uses the same layout, while bare `--profile` uses `rcmake-profile` in the build directory. |
| `RCMAKE_EXEC_LOG` | Set the path of a JSON Lines invocation log. Relative paths start at the invocation directory, and an empty value disables logging. Each record includes a `tool` field and a `persona` field holding the effective mode (`native` or `compat`). |
| `RCMAKE_VENDORED_DATA_ROOT` | Select the directory containing the bundled CMake data. Relative paths start at the invocation directory. Discovery checks for `Modules/CMakeDetermineSystem.cmake` and falls back to installed or checkout paths if that check fails. The File API also reports this path as its root when the cache lacks `CMAKE_ROOT`. |

## CTest script process state

| Variable | Effect |
| --- | --- |
| `RCMAKE_CTEST_SCRIPT_DEPTH` | Track nesting depth for CTest script processes. The initial depth defaults to 0. CTest passes the incremented depth to child scripts and enforces a limit of 32. Leave this variable unset for normal invocations. |

## Configure measurements

| Variable | Effect |
| --- | --- |
| `RCMAKE_MEASURE_REPLAY` | Set to `1` to record executed and replayed listfiles in `CMakeFiles/rcmake/replay-result.v1.json` relative to the build directory after a successful native-mode configure. |
| `RCMAKE_MEASURE_PROBE_INTERVALS` | Set to `1` to write timing intervals for verified speculative compiler runs that exit normally into `CMakeFiles/rcmake-probe-intervals.txt` in the build directory. Each line records the scratch directory name, monotonic start and end times in nanoseconds, and exit code. This measurement requires active speculative workers. |
| `RCMAKE_MEASURE_ENTRY` | Set the positive, one-based cache entry publication number for the interruption test. Use this with `RCMAKE_MEASURE_ARM` and `RCMAKE_MEASURE_CHECKPOINT`. |
| `RCMAKE_MEASURE_ARM` | Set the absolute path to the regular file that pauses cache publication. Remove the file to resume publication. The pause lasts at most 60 seconds. |
| `RCMAKE_MEASURE_CHECKPOINT` | Set the absolute path where the interruption test creates a symbolic link to the staged cache entry. |

## Conformance harness

| Variable | Effect |
| --- | --- |
| `RCMAKE_ROOT` | Select the repository root for `rcmake-harness`. The default is the current directory. The harness uses this root to locate test cases, baselines, and staged tools. |
| `RCMAKE_ORACLE_BIN_DIR` | Set the directory containing the CMake 4.5.1 reference tools for `rcmake-harness`. Reference runs require this setting, while offline mode uses saved baselines. |

## Test settings

| Variable | Effect |
| --- | --- |
| `RCMAKE_ARCHIVE_INTEROP_CMAKE` | Set the CMake executable for the ignored archive interoperability tests, including 7-Zip. |
| `RCMAKE_CTEST_COMPARISON` | Set the CTest executable for the optional report comparison test. |
| `RCMAKE_FUZZ_SCALE` | Multiply the random case count in command-line integration tests. The default is 1, and valid integers clamp to 1 through 100. |
| `RCMAKE_NINJA_TEST_CMAKE` | Set an optional CMake executable for the native Ninja dependency scan test. |
| `RCMAKE_TEST_EXECUTABLE` | Set an extra CMake-compatible executable for script-mode checks of generated files. |
| `RCMAKE_TEST_INDEX_LOCK_BUILD` | Pass the build directory to child processes during the instrumentation index lock test. |
| `RCMAKE_TEST_REFERENCE_CMAKE` | Set the CMake executable for optional differential tests across crates. |
| `RCMAKE_TEST_REFERENCE_CPACK` | Set the CPack executable for archive output comparison. That test also reads `RCMAKE_TEST_REFERENCE_CMAKE`. |
| `RCMAKE_VS_TEST_OUTPUT` | Set an output directory for Visual Studio generator fixtures. The test uses an in-memory host when this variable is unset. |

Test fixtures also set and read `RCMAKE_HOST_CONFORMANCE` for host environment checks, `RCMAKE_HIDDEN` and `RCMAKE_VALUE` for child process environment checks, `RCMAKE_TEST_UNDECLARED` for the hermetic diagnostic test, and `RCMAKE_VAR_TEST` for evaluator environment assignment checks.

The compiler wrapper verifier checks `RCMAKE_M6_*` variables that a wrapper uses as log destinations, and the determinism fixture uses `RCMAKE_M6_PROBE_LOG`.
