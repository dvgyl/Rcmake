# Caching

This page describes two native `rcmake` features:

- Portable probe cache archives, for sharing compiler checks between machines.
- Drift checks, for build directories whose saved toolchain results are out of date.

The compatibility tools (`cmake`, `ctest`, `cpack`) and `RCMAKE_ORACLE_MODE=1` do not use these features.

## Probe cache archives

A native configure keeps the results of compiler checks (`try_compile`, `check_include_file`, and similar checks) in the probe cache. Each result records its inputs, for example the compiler path and the SHA-256 digest of the compiler file. A later configure uses a result only when all recorded inputs still match on the local machine.

`rcmake cache export` writes the probe results to one file. `rcmake cache import` reads that file into another probe cache, for example on a new CI runner.

### Export

```sh
rcmake cache export probes.tar.gz
```

A file name that starts with `-` must come after `--`, for example `rcmake cache export -- -probes.tar.gz`. Without `--`, the command reports an unknown option and writes no file.

The command:

1. Reads only probe entries and the blobs that they reference. It does not copy other cache data, lock files, temporary files, or credentials.
2. Exports an entry only when its frame, its probe record, its recorded inputs, and all of its blobs are valid now. It counts the other entries as skipped (stale, invalid, missing blobs, or too large).
3. Keeps the archive inside the import size limits. An entry that does not fit is skipped as too large.
4. Writes a gzip tar file with fixed names, modes, owners, and times. The same cache content gives the same bytes.
5. Runs the import checks on the finished archive. If a check fails, the command fails and leaves the destination unchanged.
6. Writes to a new temporary file with a unique name in the destination directory, then renames it. It refuses a destination inside the cache directory.
7. Prints the SHA-256 digest of the archive.

The archive contains compiler paths, compiler arguments, and the captured compiler output of each check. The export command is the approval to put this data in a file. Treat the archive like other build logs from the same machine. The remote cache has a separate rule: an upload also needs `RCMAKE_CACHE_REMOTE_EXPORT_CAPTURED_OUTPUT=1`.

### Import

```sh
rcmake cache import probes.tar.gz --sha256 <digest>
```

The command reads one private copy of the archive and calculates its SHA-256 digest. Then it checks the complete archive before it writes anything:

- The archive must be one gzip stream that holds one tar stream. Data after the end is an error.
- Each member must be a regular file named `manifest.json`, `entries/<64 hex digits>`, or `blobs/<64 hex digits>`. Links, devices, directories, extension headers, duplicate names, and other names are errors.
- The manifest must name only probe entries. The archive must hold exactly those entries and their blobs.
- Each entry and blob must match its digest, record format, key, and size limits.
- Each entry must hold a valid probe record. The check covers every field of the record, including each recorded input check and its digest. Only a compiled check can keep an artifact, and the artifact blob must be the only blob of the entry.

The size limits apply before memory is allocated. The limits are 256 MiB for the archive, 272 MiB for the decompressed tar stream, 64 MiB for one member, 16 MiB for the manifest, 256 MiB for all members, 65,536 members, 1 GiB for all decoded blobs, and 512 MiB for all decoded entries. Each entry frame declares its decoded size in its header. The import adds that size to the entry total as soon as it reads the entry, before it decompresses any entry. Export uses the same limits, so an archive from `rcmake cache export` always passes these checks.

If a check fails, the command prints the reason and changes nothing.

The `--sha256` option selects how the import uses the entries:

- **With `--sha256`.** The digest must match the archive. The digest must come from a source that you trust, for example a protected producer job. The import then publishes the entries under the cache collection lock. It writes the blobs of an entry before the entry, so collection cannot remove a blob that a new entry needs. A valid local entry is never replaced.
- **Without `--sha256`.** The archive digest proves only that the file is complete, not who made it. The import keeps the entries as candidates outside the live probe cache. A candidate never causes a cache hit and cannot act as its own trusted record. A candidate can only restore a missing blob of a local entry that has the same content. On a cold runner, configure runs the checks again.

An entry counts as unchanged only when the local entry has the same content and each of its blobs on disk matches its digest. The import replaces a missing or damaged blob with the checked copy from the archive and counts the entry as recovered.

A repeated import of the same archive changes nothing. The command reports the published, unchanged, recovered, staged, and conflicting entries.

If a write fails during the import, the command reports the entries that it finished before the failure and then the error. Each finished entry is complete and usable. Run the same import again to finish the rest. The second run counts the finished entries as unchanged.

Collection removes candidates that are not used for 7 days. A candidate blob stays as long as a candidate entry that uses it. Candidates also count toward the cache size limit, and collection removes them first when the cache is too large. `rcmake cache clear` removes them at once.

An imported entry is a hit only when its recorded inputs match on the new machine. The recorded inputs include absolute paths. Thus a hit needs the same compiler at the same path, with the same content. A different compiler, a compiler update, or a different install path gives a miss, and configure runs the check again.

### CI recipe with `actions/cache`

The workflow below runs on pushes to the protected `main` branch. The producer job configures, exports the probe cache, and records the archive digest as an artifact of the same run. The consumer jobs run on other runners. They restore the archive, import it with the digest from the producer, and configure.

The cache key selects which archive to download. Make the key from the cache format version, the runner OS and architecture, the compiler identity, and the dependency inputs. The local input checks still decide which entries are used.

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

The digest comes from the producer job of the same run, not from the cache. Anyone who can write a cache entry could change both the archive and a digest stored next to it. If the restored archive does not match the digest, the import stops, nothing changes, and configure runs all checks.

A pull request workflow can restore an archive from `main`, but it has no trusted digest from the same run. Import such an archive without `--sha256`, or skip the import. Without `--sha256`, the entries are only candidates, and configure runs the checks.

The three cases are:

- **Cold run.** No archive exists for the key. Configure runs all checks, and the producer exports them.
- **Hit.** A second runner with the same compiler, paths, and inputs imports the archive. Configure uses the imported results.
- **Miss after a compiler change.** A new compiler changes the key, so no archive is restored. If an old archive is imported, its recorded compiler digest does not match, and configure runs the checks again.

## Drift checks

`CMakeCache.txt` and `CMakeFiles/<version>/CMake<LANG>Compiler.cmake` keep the toolchain results of the first configure. If the toolchain changes after that, a warm configure can use results that describe the old toolchain.

A native configure that completes generation writes a receipt to `CMakeFiles/rcmake/toolchain-inputs-v1.json`. The receipt records:

- `CMAKE_TOOLCHAIN_FILE`: the value from the cache, the resolved file, and its SHA-256 digest.
- `CMAKE_PREFIX_PATH`: the cache value and the environment value, in order. An unset value and an empty value are different.
- For each enabled language: the compiler value from the cache, the resolved file, its SHA-256 digest, the launcher, and the detected compiler ID and version.

The next native configure compares the current values with the receipt. It checks the file content, not only the modification time. It prints one warning for each changed field, with the previous and current values and the recovery command. A configure that restarts prints each warning only once.

The receipt changes only when the new values come from new evidence:

- A fresh configure (`--fresh`, a new build directory, or a cache reset) records all current values.
- A compiler identification during the configure records the new compiler.
- A warm configure that loads the saved compiler results keeps the old receipt. Thus a compiler that was replaced at the same path gives a warning on each configure until a fresh configure.
- A compiler file or compiler directory that cannot be read keeps the recorded values of its language. Only a compiler file that is missing from a readable directory removes the language from the receipt.

A relative `CMAKE_TOOLCHAIN_FILE` resolves against the build directory first and then the source directory, as in CMake. A configure hashes each compiler file once. The check after generation reuses the digest when the file length, modification time, device, and inode are the same.

A failed configure, `-N`, and an incomplete generation do not change the receipt. If a cache cleanup removed the receipt and the configure then fails, `rcmake` writes the previous receipt again.

To see the drift without a configure, run:

```sh
rcmake doctor -B build
```

The report reads the cache, the receipt, and the tool files. It does not run a compiler or project code. It reports one of these states:

- No configured build directory (no `CMakeCache.txt`).
- No receipt. Paths come from `CMakeCache.txt`, but no content digest is available. The next native configure records one.
- An invalid receipt, or a receipt from another build directory.
- A missing tool file.
- Changed fields.
- No drift.

The exit code is 0 when the report finds no problem. It is 1 when the report finds a missing build directory, an invalid receipt, a missing tool, or a changed field. It is 2 for invalid arguments. `-B` accepts `-B build`, `-Bbuild`, and `-B=build`. `rcmake doctor` never removes or changes the cache.

To recover, run `rcmake --fresh -S "<source>" -B "<build>"` or configure a new build directory. `--fresh` removes `CMakeCache.txt`, so give each `-D` option again.
