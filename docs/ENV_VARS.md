# Rcmake environment variables

Set these variables before you start Rcmake. Configure speed settings apply
to the Native persona, which you select with the executable name `rcmake`.
A supported command-line option takes precedence over its environment setting.

## Configure and probe cache

| Variable | Effect |
| --- | --- |
| `RCMAKE_NO_INCREMENTAL` | Set to `1` to evaluate all listfiles. The default permits replay of eligible listfiles. |
| `RCMAKE_PROBE_CACHE` | Select `readwrite` (default), `readonly`, `verify`, or `off`. `1` and `on` select `readwrite`. `0` selects `off`. |
| `RCMAKE_NO_PROBE_CACHE` | Set to `1` to disable the probe cache. Set to `0` to use the mode from `RCMAKE_PROBE_CACHE`. |
| `RCMAKE_PROBE_CACHE_DIR` | Override the probe cache directory. Relative paths start at the invocation directory. |
| `RCMAKE_PROBE_JOBS` | Set the number of probe workers to a positive integer. Use `1` for serial probes. |
| `RCMAKE_CACHE_DIR` | Override the system cache directory. Relative paths start at the invocation directory. |
| `RCMAKE_CACHE_MAX_SIZE` | Set the cache size limit in bytes. The default is 2 GiB. |
| `RCMAKE_MEASURE_REPLAY` | Set to `1` to write replay measurements to `CMakeFiles/rcmake/replay-result.v1.json` in the build directory. |

An invalid probe cache mode or disable value turns off the probe cache and
prints a warning. An invalid job count uses the runtime default and prints
a warning.

Presets, toolchain files, and preload scripts disable replay because they can
change configure inputs before listfile replay starts. Profiling, tracing,
`--fresh`, and options that change diagnostic output also disable replay.
The two `--diagnostics` formats support replay.

## Remote probe cache

| Variable | Effect |
| --- | --- |
| `RCMAKE_CACHE_REMOTE` | Set the HTTPS endpoint. An empty or absent value disables the remote cache. |
| `RCMAKE_CACHE_REMOTE_WRITE`, `RCMAKE_CACHE_REMOTE_EXPORT_CAPTURED_OUTPUT` | Uploads require both values to be `1`. |
| `RCMAKE_CACHE_REMOTE_EXPORT_TIMEOUT` | Set the upload time limit in seconds. |
| `RCMAKE_CACHE_REMOTE_CURL` | Select the curl executable. |
| `RCMAKE_CACHE_REMOTE_PRIVATE_DIR` | Select the private directory for remote cache transfer files. |
| `RCMAKE_CACHE_REMOTE_AUTH` | Select `anonymous` (default), `bearer`, `header`, or `s3` authentication. |
| `RCMAKE_CACHE_REMOTE_TOKEN`, `RCMAKE_CACHE_REMOTE_HEADER` | Supply the bearer token or custom header value, and the custom header name. |
| `RCMAKE_OFFLINE` | Set to `1` to disable the remote probe cache. |

## Fetch and hermetic input checks

| Variable | Effect |
| --- | --- |
| `RCMAKE_FETCH_LOCK` | Select `auto` (default), `off`, `update`, or `frozen`. An invalid value prints a warning and disables the fetch lock. |
| `RCMAKE_FETCH_LOCK_FILE` | Select a lockfile path. |
| `RCMAKE_FETCH_OFFLINE` | Set to `1` to disable fetch network access and remote probe cache access. |
| `RCMAKE_FETCH_PREFETCH` | Set to `0` to disable fetch prefetch. |
| `RCMAKE_FETCH_JOBS` | Set the number of fetch workers. The default is 4. The range is 1 to 32. |
| `RCMAKE_FETCH_GIT_READ_ONLY` | Set to `1` to use an immutable Git mirror in offline mode. |
| `RCMAKE_FETCH_GIT_MAX_SIZE` | Set the Git mirror cache size limit in bytes. |
| `RCMAKE_HERMETIC` | Select `off` (default), `warn`, or `strict`. `0` selects `off`. `1` selects `warn`. Strict mode rejects undeclared inputs. |
| `RCMAKE_HERMETIC_ALLOW_ENV` | Set a semicolon-separated list of permitted environment variable names. |
| `RCMAKE_HERMETIC_ALLOW_PATHS` | Set a semicolon-separated list of permitted absolute paths. |
| `RCMAKE_HERMETIC_ALLOW_PROGRAMS` | Set a semicolon-separated list of permitted absolute executable paths. |

## Command-line output

| Variable | Effect |
| --- | --- |
| `RCMAKE_DIAGNOSTICS` | Select `cmake` or `rich`. By default, Native mode uses rich output when standard error goes to a terminal. |
| `RCMAKE_PROFILE` | Set the output directory for a Native profile. Relative paths start at the invocation directory. |
| `RCMAKE_EXEC_LOG` | Set the path of a JSON Lines invocation log. Each record contains the tool and effective persona. |
| `RCMAKE_ORACLE_MODE` | Set to `1` to use the Compat persona, including when the executable name is `rcmake`. |
| `RCMAKE_VENDORED_DATA_ROOT` | Select the directory that contains the bundled `Modules` directory. |
