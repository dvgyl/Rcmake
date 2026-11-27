# Caching, replay, and drift detection

In native mode, `rcmake` accelerates repeated configures and catches stale build trees using three connected mechanisms. Compatibility tools (`cmake`, `ccmake`, `ctest`, `cpack`) and `RCMAKE_ORACLE_MODE=1` disable all three and match CMake 4.5.1 directly.

## Contents

- [Probe cache archives](#probe-cache-archives)
  - [Export](#export)
  - [Import](#import)
  - [CI recipe with `actions/cache`](#ci-recipe-with-actionscache)
- [Incremental configure replay](#incremental-configure-replay)
- [Drift checks](#drift-checks)

## Probe cache archives

A native configure stores verified compiler check results (`try_compile`, `check_include_file`, `check_symbol_exists`, and related probes) in a content-addressed probe cache. Each entry records its inputs, including absolute paths and SHA-256 content digests of the compiler binary, headers, and configuration files. A later configure reuses an entry only when every recorded input still matches locally.

Use `rcmake cache status`, `rcmake cache dir`, and `rcmake cache clear` to inspect or clean the cache. Use `rcmake cache export` and `rcmake cache import` to transfer verified probe entries between machines or CI runners. Override the probe root separately with `RCMAKE_PROBE_CACHE_DIR` or jointly with `RCMAKE_CACHE_DIR`.

### Export

```sh
rcmake cache export probes.tar.gz
```

Place `--` before any filename starting with `-`, such as `rcmake cache export -- -probes.tar.gz`. The command performs seven steps.

1. Reads only probe entries and their referenced blobs, skipping locks, temporary files, FetchContent data, and credentials.
2. Exports an entry only when its frame, probe record, recorded inputs, and blobs are currently valid, counting stale, invalid, incomplete, or oversized entries as skipped.
3. Enforces the same size budgets used on import so every exported archive passes import checks.
4. Writes a deterministic gzip tar archive with fixed names, modes, owners, and timestamps so identical cache content produces identical bytes.
5. Runs full import validation on the finished archive before changing the destination.
6. Writes to a unique temporary file in the destination directory and renames it atomically, refusing destinations inside the cache directory.
7. Prints the SHA-256 digest of the finished archive.

Because the archive stores compiler paths, compiler flags, and captured compiler output, treat it like build logs from the same machine. Remote HTTPS uploads keep a separate opt-in rule requiring `RCMAKE_CACHE_REMOTE_EXPORT_CAPTURED_OUTPUT=1`.

### Import

```sh
rcmake cache import probes.tar.gz --sha256 <64-hex-digest>
```

Import reads one private copy of the archive, calculates its SHA-256 digest, and validates the complete file before writing anything to disk.

- The file must be one gzip stream containing one tar stream with no trailing bytes.
- Every member must be a regular file named `manifest.json`, `entries/<64-hex-digits>`, or `blobs/<64-hex-digits>`. Links, directories, devices, extension headers, and duplicate names fail validation.
- The manifest must list only probe entries, and the archive must contain exactly those entries and their blobs.
- Every entry and blob must match its digest, record format, key, and size limit. Only compiled checks may carry an artifact blob, and that artifact must be the sole blob on the entry.

Import enforces strict pre-allocation caps of 256 MiB for the archive, 272 MiB for the decompressed tar stream, 64 MiB per member, 16 MiB for the manifest, 256 MiB for all members combined, 65,536 members, 1 GiB for decoded blobs, and 512 MiB for decoded entries. Each entry declares its decoded size in its frame header so the budget triggers before decompression. On any validation failure, import reports the reason and changes nothing.

The `--sha256` flag controls trust isolation.

- **With `--sha256`.** The 64-hex-digit digest must match the archive and come from a trusted producer such as a protected CI job. Import acquires the cache collection lock, writes blobs before their referencing entries, and publishes entries into the live probe cache without overwriting valid local entries.
- **Without `--sha256`.** An archive digest alone proves completeness, not origin. Import stages entries outside the live probe cache as untrusted candidates. Candidates never trigger cache hits and can only repair a missing or corrupted blob belonging to an existing matching local entry. On a cold runner, configure runs probes live.

An entry counts as unchanged when the local entry matches and all local blobs verify. A missing or damaged blob replaced from the archive counts as recovered. Re-importing the same archive is idempotent. If a write fails mid-import, completed entries stay intact and re-running the import finishes the rest. Garbage collection removes unused candidates after 7 days and evicts candidates first when the cache exceeds `RCMAKE_CACHE_MAX_SIZE`. `rcmake cache clear` removes candidates immediately.

Because recorded inputs include absolute paths and SHA-256 digests, an imported entry hits only when the target machine has the same compiler bytes at the same path. Any compiler upgrade or path change misses safely and runs the check live.

### CI recipe with `actions/cache`

This workflow exports probes on protected `main` pushes and passes the archive digest via a workflow artifact from the same run to downstream consumer jobs.

```yaml
on:
  push:
    branches: [main]

env:
  RCMAKE_PROBE_CACHE_DIR: ${{ github.workspace }}/.rcmake-probes

jobs:
  produce:
    runs-on: ubuntu-24.04
    outputs:
      key: ${{ steps.key.outputs.key }}
    steps:
      - uses: actions/checkout@v4
      - name: Cache key
        id: key
        run: |
          compiler=$(cc --version | head -n 1 | sha256sum | cut -c1-16)
          echo "key=rcmake-probes-v1-${{ runner.os }}-${{ runner.arch }}-$compiler-${{ hashFiles('CMakeLists.txt', 'cmake/**') }}" >> "$GITHUB_OUTPUT"
      - run: rcmake -S . -B build -G Ninja
      - name: Export probes
        run: |
          rcmake cache export probes.tar.gz
          sha256sum probes.tar.gz | cut -d ' ' -f 1 > probes.sha256
      - uses: actions/upload-artifact@v4
        with:
          name: probes-digest
          path: probes.sha256
      - uses: actions/cache/save@v4
        with:
          path: probes.tar.gz
          key: ${{ steps.key.outputs.key }}-${{ github.run_id }}

  consume:
    needs: produce
    runs-on: ubuntu-24.04
    strategy:
      matrix:
        shard: [1, 2, 3]
    steps:
      - uses: actions/checkout@v4
      - uses: actions/download-artifact@v4
        with:
          name: probes-digest
      - uses: actions/cache/restore@v4
        with:
          path: probes.tar.gz
          key: ${{ needs.produce.outputs.key }}-${{ github.run_id }}
      - name: Import probes
        if: hashFiles('probes.tar.gz') != ''
        run: rcmake cache import probes.tar.gz --sha256 "$(cat probes.sha256)"
      - run: rcmake -S . -B build -G Ninja
```

Always read the trusted digest from the producer artifact rather than from the cache itself so a cache writer cannot alter both archive and digest. Pull request jobs restoring a `main` archive should skip `--sha256` or skip import. On a cold run, configure runs probes and populates the export. On a matching runner, imported probes hit directly. After a compiler update, the cache key and compiler digest both miss safely.

## Incremental configure replay

When you reconfigure an existing native build tree after editing a file, `rcmake` re-executes edited directory regions and replays unchanged subtrees from `CMakeFiles/rcmake/configure.v1.bin`. A region replays only when its entry context (binary identity, policy and diagnostic state, options, generator, toolchain) and every recorded input match, reapplying recorded variable, cache, and property edits in original order.

Starting in `0.2.0`, incremental replay also works with toolchain files (including `vcpkg`), configure presets, `-C` initial cache scripts, and `PreLoad.cmake`. Setup hooks always execute live on every configure. When each outermost setup span finishes, `rcmake` hashes observable interpreter state (effective cache entries, variables, environment reads, and scripted command overrides) into a setup checkpoint and compares it with the previous configure. Any checkpoint mismatch disables replay hits for the rest of that configure while preserving earlier valid hits, and leaf replay invalidates regions if `if(COMMAND ...)` sees a scripted override.

Set `RCMAKE_REPLAY_SETUP=0` to disable replay when setup hooks are present, or set `RCMAKE_NO_INCREMENTAL=1` to disable replay altogether. See [ENV_VARS.md](ENV_VARS.md#incremental-replay) for every condition that turns replay off.

## Drift checks

`CMakeCache.txt` and `CMakeFiles/<version>/CMake<LANG>Compiler.cmake` store compiler identification results from the first configure. If a compiler binary, toolchain file, launcher, or prefix path changes afterward, a warm reconfigure can silently reuse stale toolchain facts.

Every native configure that completes generation writes a drift receipt at `CMakeFiles/rcmake/toolchain-inputs-v1.json`. That receipt records `CMAKE_TOOLCHAIN_FILE` (cache value, resolved file, and SHA-256 digest), ordered cache and environment `CMAKE_PREFIX_PATH` entries (distinguishing unset from empty), and each enabled language's compiler cache value, resolved binary, SHA-256 digest, launcher, ID, and version.

On subsequent native configures, `rcmake` compares current values and file digests against the receipt and prints one warning per changed field with the recovery command. The receipt baseline advances only on fresh configures or when `enable_language()` writes a fresh native identification record, so replacing a compiler in place keeps warning on every warm configure until you reconfigure fresh. Failed configures, `-N`, and incomplete generations leave the receipt unchanged, and `rcmake` restores the previous receipt if cache cleanup removed it before a failed run.

Inspect drift at any time without running compilers or project scripts.

```sh
rcmake doctor -B build
```

`rcmake doctor -B <dir>` reads `CMakeCache.txt`, the drift receipt, and the referenced tool files, reporting missing build directories, unavailable or invalid receipts, missing tool files, changed fields, or a clean tree. It exits with code 0 when no drift or missing tool files are found (including a build tree that has no drift receipt yet while its tool files exist), code 1 on missing build directories, invalid receipts, missing tool files, or drift, and code 2 on invalid arguments, and it never modifies the cache.

To recover from drift, run `rcmake --fresh -S "<source>" -B "<build>"` (passing your `-D` flags again) or configure a new build directory.
