# rcmake environment variables

`rcmake` has two modes. The executable name `rcmake` selects native mode, unless
`RCMAKE_ORACLE_MODE=1` selects compatibility mode. The names `cmake`, `ctest`,
and `cpack` always select compatibility mode, which matches CMake 4.5.1.
Project configure uses the speed settings below only in native mode.

An applicable command-line option takes precedence over its environment setting.

## Configure and probe cache

| Variable | Effect |
| --- | --- |
| `RCMAKE_NO_INCREMENTAL` | Set to `1` to disable configure replay. Evaluation still follows the project's control flow and cache settings. |
| `RCMAKE_PROBE_CACHE` | Select `readwrite` (default), `readonly`, `verify`, or `off`. `1` and `on` select `readwrite`. `0` selects `off`. |
| `RCMAKE_NO_PROBE_CACHE` | Set to `1` to disable the probe cache. Set to `0` to use the mode from `RCMAKE_PROBE_CACHE`. |
| `RCMAKE_PROBE_CACHE_DIR` | Override the probe storage root. An empty value uses the shared cache root. Relative paths start at the invocation directory. |
| `RCMAKE_PROBE_JOBS` | Request a positive integer number of probe jobs, including the foreground job. `1` runs probes in series. |
| `RCMAKE_CACHE_DIR` | Override the shared storage root for the probe cache and FetchContent. An empty value uses the system cache location. Relative paths start at the invocation directory. |
| `RCMAKE_CACHE_MAX_SIZE` | Set the store size limit in bytes. The default is 2 GiB. The Git mirror cache has a separate limit. |

Mode `verify` compares a cache hit with a fresh probe result. Configure uses
the fresh result.

The default probe job count is the smaller of four and the logical CPU count.
The pool applies the CPU count, a limit of 32 jobs, and a memory limit. It
reserves 2 GiB for configure and allows 1 GiB per job. If the memory size is
unknown, the pool allows one job.

An invalid probe cache mode or disable value turns off the probe cache.
An invalid job count uses the default. In native mode, `rcmake` prints a
warning for each invalid setting.

### Incremental replay

These inputs and options disable native-mode configure replay:

- A configure preset or an explicit `-C` preload script.
- A `PreLoad.cmake` file in the source or build directory.
- `--toolchain`, or `CMAKE_TOOLCHAIN_FILE` from a `-D` option, the saved cache,
  or the environment.
- `--fresh`, trace options, or an active profile. This includes `--profile`,
  `RCMAKE_PROFILE`, and CMake-format profiling.
- `--check-system-vars`, `--log-context`, `--debug-output`, `--debug-find`,
  `--debug-find-var`, or `--debug-find-pkg`.

The configure engine also disables replay when hermetic mode is active, the
probe cache mode is `verify`, or a command observer is present. Collection of
annotations or provenance and a step budget also disable replay. Both
`--diagnostics=cmake` and `--diagnostics=rich` permit replay.

## Remote probe cache

| Variable | Effect |
| --- | --- |
| `RCMAKE_CACHE_REMOTE` | Set the HTTPS endpoint. An empty or unset value disables remote access. |
| `RCMAKE_CACHE_REMOTE_WRITE` | Set to `1` to permit uploads. Uploads also require `RCMAKE_CACHE_REMOTE_EXPORT_CAPTURED_OUTPUT=1`. |
| `RCMAKE_CACHE_REMOTE_EXPORT_CAPTURED_OUTPUT` | Set to `1` to permit uploads that include captured probe output. Uploads also require `RCMAKE_CACHE_REMOTE_WRITE=1`. |
| `RCMAKE_CACHE_REMOTE_EXPORT_TIMEOUT` | Set the upload time limit to a positive integer number of seconds. The remote client default is 300 seconds. Configure also limits the upload to its remaining remote budget, which starts at 3 seconds. |
| `RCMAKE_CACHE_REMOTE_CURL` | Set the absolute path to a curl executable. The defaults are `/usr/bin/curl` and `C:/Windows/System32/curl.exe`. |
| `RCMAKE_CACHE_REMOTE_PRIVATE_DIR` | Set the absolute directory for private transfer files. On Unix, authenticated transfers require this directory. Its group and other permission bits must be clear. On Windows, the client passes the private curl configuration through standard input. |
| `RCMAKE_CACHE_REMOTE_AUTH` | Select `anonymous` (default), `bearer`, `header`, or `s3`. |
| `RCMAKE_CACHE_REMOTE_TOKEN` | Supply the bearer token or the value of the custom HTTP header. |
| `RCMAKE_CACHE_REMOTE_HEADER` | Supply the custom HTTP header name for `header` authentication. |
| `RCMAKE_OFFLINE` | Set to `1` to disable the remote probe cache. FetchContent uses `RCMAKE_FETCH_OFFLINE` for its offline setting. |

The `s3` mode uses `AWS_REGION`, `AWS_ACCESS_KEY_ID`,
`AWS_SECRET_ACCESS_KEY`, and the optional `AWS_SESSION_TOKEN`.

## Fetch and hermetic input checks

| Variable | Effect |
| --- | --- |
| `RCMAKE_FETCH_LOCK` | Select `auto` (default), `off`, `update`, or `frozen`. In native mode, an invalid value prints a warning and selects `off`. The standalone fetch parser returns an error. |
| `RCMAKE_FETCH_LOCK_FILE` | Select the lockfile path. Relative paths start at the project source directory. An empty or unset value uses `rcmake-fetch.lock`. |
| `RCMAKE_FETCH_OFFLINE` | Set to `1` to disable remote probe access and network transfers through FetchContent and ExternalProject. FetchContent can still use local sources and verified cached sources. |
| `RCMAKE_FETCH_PREFETCH` | Set to `0` to disable prefetch. Prefetch requires a fetch lock mode other than `off`, hermetic mode `off`, and online fetch. |
| `RCMAKE_FETCH_JOBS` | Request a positive integer number of fetch workers. The default is 4. The pool clamps the count to 1 through 32. In native mode, an invalid value prints a warning and selects the default. |
| `RCMAKE_FETCH_GIT_READ_ONLY` | Set to `1` with `RCMAKE_FETCH_OFFLINE=1` to use an immutable Git mirror cache. |
| `RCMAKE_FETCH_GIT_MAX_SIZE` | Set the Git mirror cache size limit in bytes. The default is 2 GiB. Offline and read-only cache instances leave mirrors intact. |
| `RCMAKE_HERMETIC` | Select `off` (default), `warn`, or `strict`. `0` selects `off`. `1` selects `warn`. Warn mode reports undeclared inputs. Strict mode rejects them. In native mode, an invalid value prints a warning and selects `off`. |
| `RCMAKE_HERMETIC_ALLOW_ENV` | Set a semicolon-separated list of permitted environment variable names. |
| `RCMAKE_HERMETIC_ALLOW_PATHS` | Set a semicolon-separated list of resolvable, permitted absolute paths. |
| `RCMAKE_HERMETIC_ALLOW_PROGRAMS` | Set a semicolon-separated list of resolvable, permitted absolute executable paths. |

`rcmake` reports invalid allowlist entries and ignores them.

## Command-line output and data discovery

| Variable | Effect |
| --- | --- |
| `RCMAKE_ORACLE_MODE` | Set to `1` to select compatibility mode, including when the executable name is `rcmake`. |
| `RCMAKE_DIAGNOSTICS` | Native mode accepts `cmake` or `rich`. An invalid or unset value selects rich output when standard error goes to a terminal and CMake formatting otherwise. Compatibility mode always uses CMake formatting. |
| `RCMAKE_PROFILE` | Set the native-mode configure profile directory. Relative paths start at the invocation directory. An empty value disables the environment setting. Each run writes `flamegraph.html`, `trace.json`, and `stacks.folded` in `<dir>`, and keeps each run in `<dir>/run-*`. The `current` file in `<dir>` holds the name of the latest run subdirectory. `--profile=<dir>` uses the same layout. Without `<dir>`, `--profile` uses `rcmake-profile` in the build directory. |
| `RCMAKE_EXEC_LOG` | Set the path of a JSON Lines invocation log. Relative paths start at the invocation directory. An empty value disables logging. Each record has a `tool` field and a `persona` field. The `persona` field holds the effective mode (`native` or `compat`). |
| `RCMAKE_VENDORED_DATA_ROOT` | Select the directory that contains the bundled CMake data. Relative paths start at the invocation directory. Discovery checks for `Modules/CMakeDetermineSystem.cmake` and falls back to installed or checkout paths if the override fails that check. The File API can also use this value as its reported root when the cache lacks `CMAKE_ROOT`. |

## CTest script process state

| Variable | Effect |
| --- | --- |
| `RCMAKE_CTEST_SCRIPT_DEPTH` | Internal depth for CTest script processes. The initial depth defaults to 0. CTest passes the next depth to child scripts and enforces a limit of 32. Leave unset for normal invocations. |

## Configure measurements

| Variable | Effect |
| --- | --- |
| `RCMAKE_MEASURE_REPLAY` | Set to `1` to record executed and replayed listfiles in `CMakeFiles/rcmake/replay-result.v1.json` after a successful native-mode configure. The path is relative to the build directory. |
| `RCMAKE_MEASURE_PROBE_INTERVALS` | Set to `1` to write intervals for verified speculative compiler runs that exit normally. The output is `CMakeFiles/rcmake-probe-intervals.txt` in the build directory. Each line has the scratch directory name, monotonic start and end times in nanoseconds, and exit code. The measurement requires active speculative workers. |
| `RCMAKE_MEASURE_ENTRY` | Set the positive, one-based cache entry publication number for the interruption test. Use with the arm and checkpoint paths. |
| `RCMAKE_MEASURE_ARM` | Set the absolute path to the regular file that enables the cache publication pause. Remove the file to resume publication. The pause lasts at most 60 seconds. |
| `RCMAKE_MEASURE_CHECKPOINT` | Set the absolute path where the interruption test creates a symbolic link to the staged cache entry. |

## Conformance harness

| Variable | Effect |
| --- | --- |
| `RCMAKE_ROOT` | Select the repository root for `rcmake-harness`. The default is the current directory. The harness uses this root for test cases, baselines, and staged tools. |
| `RCMAKE_ORACLE_BIN_DIR` | Set the directory of the CMake 4.5.1 reference tools for `rcmake-harness`. Reference runs require this setting. The harness uses saved baselines in offline mode. |

## Test settings

| Variable | Effect |
| --- | --- |
| `RCMAKE_ARCHIVE_INTEROP_CMAKE` | CMake executable for the ignored archive interoperability tests, including 7-Zip. |
| `RCMAKE_CTEST_COMPARISON` | CTest executable for the optional report comparison test. |
| `RCMAKE_FUZZ_SCALE` | Multiplier for random cases in command-line integration tests. The default is 1. The tests clamp a valid integer to 1 through 100. |
| `RCMAKE_NINJA_TEST_CMAKE` | Optional CMake executable for the native Ninja dependency scan test. |
| `RCMAKE_TEST_EXECUTABLE` | Additional CMake-compatible executable for script-mode checks of generated files. |
| `RCMAKE_TEST_INDEX_LOCK_BUILD` | Internal child-process key for the instrumentation index lock test. It contains the build directory. |
| `RCMAKE_TEST_REFERENCE_CMAKE` | CMake executable for optional differential tests in multiple crates. |
| `RCMAKE_TEST_REFERENCE_CPACK` | CPack executable for archive output comparison. The test also uses `RCMAKE_TEST_REFERENCE_CMAKE`. |
| `RCMAKE_VS_TEST_OUTPUT` | Output directory for Visual Studio generator fixtures. The test uses an in-memory host when the variable is unset. |

Test fixtures set and read `RCMAKE_HOST_CONFORMANCE` for host environment
checks, `RCMAKE_HIDDEN` and `RCMAKE_VALUE` for child process environment checks,
`RCMAKE_TEST_UNDECLARED` for the hermetic diagnostic test, and
`RCMAKE_VAR_TEST` for evaluator environment assignment checks.

The compiler wrapper verifier checks `RCMAKE_M6_*` variables that a wrapper
uses as log destinations. The determinism fixture uses `RCMAKE_M6_PROBE_LOG`.
