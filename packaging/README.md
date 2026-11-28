<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->

# Package Manager Manifests

This directory holds generated package manager manifests for **rcmake**, produced deterministically from a release version and `SHA256SUMS.txt` by [`scripts/generate-manifests.py`](../scripts/generate-manifests.py) (ADR-052).

## Layout and non-conflicting defaults

Every package manager installs the complete release tree (including `bin/{rcmake,cmake,ctest,cpack,ccmake,rcmake-lsp,rcmake-verify}` and `share/cmake-4.5/`) together so `discover_cmake_root` resolves the bundled CMake 4.5.1 modules via sibling canonicalization.

By default, only the non-conflicting `rcmake` entry points are linked or shimmed onto `PATH`:

- `rcmake`
- `rcmake-lsp`
- `rcmake-verify`

Compatibility aliases (`cmake`, `ctest`, `cpack`, and `ccmake`) stay inside the package's private `bin/` directory so system or package-manager installations of upstream `cmake` are never conflicted. Users who want drop-in replacement can prepend that `bin/` directory to `PATH` (documented in each package manager's caveats or notes).

| Manager | Generated files | Targets covered |
| --- | --- | --- |
| Homebrew | `homebrew/rcmake.rb` | macOS Apple Silicon (`aarch64-apple-darwin`), Linux x86_64 (`x86_64-unknown-linux-gnu`), Linux ARM64 (`aarch64-unknown-linux-gnu`) |
| Scoop | `scoop/rcmake.json` | Windows x64 (`x86_64-pc-windows-msvc`) |
| WinGet | `winget/Rcmake.Rcmake.yaml`, `winget/Rcmake.Rcmake.installer.yaml`, `winget/Rcmake.Rcmake.locale.en-US.yaml` | Windows x64 (`x86_64-pc-windows-msvc`) |

## Generating and verifying manifests

Do not edit generated files by hand. After a release builds `SHA256SUMS.txt`, regenerate all manifests with:

```sh
python3 scripts/generate-manifests.py \
  --version 0.2.1 \
  --checksums dist/SHA256SUMS.txt \
  --out-dir packaging
```

Verify that checked-in manifests match `SHA256SUMS.txt` without writing files (used in CI drift checks):

```sh
python3 scripts/generate-manifests.py \
  --version 0.2.1 \
  --checksums dist/SHA256SUMS.txt \
  --out-dir packaging \
  --check
```

Validate generated syntax locally:

```sh
ruby -c packaging/homebrew/rcmake.rb
python3 -m json.tool packaging/scoop/rcmake.json >/dev/null
ruby -ryaml -e 'ARGV.each { |f| YAML.safe_load_file(f) }' packaging/winget/*.yaml
```

## Release publishing workflow

### 1. Homebrew tap (`rcmake/homebrew-tap`)

1. If `rcmake/homebrew-tap` does not yet exist, create the public repository with a `Formula/` directory:
   ```sh
   gh repo create rcmake/homebrew-tap --public --description "Homebrew tap for rcmake"
   ```
2. Copy `packaging/homebrew/rcmake.rb` to `Formula/rcmake.rb` in `rcmake/homebrew-tap`, commit, and push to `main`.
3. Verify on macOS/Linux with:
   ```sh
   brew audit --strict --formula Formula/rcmake.rb
   brew install --build-from-source ./Formula/rcmake.rb
   brew test rcmake
   ```
4. End-user installation:
   ```sh
   brew install rcmake/tap/rcmake
   ```

### 2. Scoop bucket (`rcmake/scoop-bucket`)

1. If `rcmake/scoop-bucket` does not yet exist, create the public repository with a `bucket/` directory:
   ```sh
   gh repo create rcmake/scoop-bucket --public --description "Scoop bucket for rcmake"
   ```
2. Copy `packaging/scoop/rcmake.json` to `bucket/rcmake.json` in `rcmake/scoop-bucket`, commit, and push to `main`.
   The manifest includes `checkver` and `autoupdate` pointing to GitHub Releases and `SHA256SUMS.txt`, so Scoop's excavator workflow can also keep future releases in sync.
3. End-user installation on Windows:
   ```powershell
   scoop bucket add rcmake https://github.com/rcmake/scoop-bucket
   scoop install rcmake/rcmake
   ```

### 3. WinGet (`microsoft/winget-pkgs`)

1. Copy the three files in `packaging/winget/` into a fork of `microsoft/winget-pkgs` under:
   ```text
   manifests/r/Rcmake/Rcmake/<version>/
   ```
2. On Windows, validate the manifest directory with the WinGet CLI:
   ```powershell
   winget validate --manifest packaging/winget/
   ```
3. Open a pull request against `microsoft/winget-pkgs` (or automate updates with `wingetcreate update Rcmake.Rcmake --urls https://github.com/rcmake/rcmake/releases/download/v<version>/rcmake-<version>-x86_64-pc-windows-msvc.zip --version <version> --submit`).
4. End-user installation on Windows once merged:
   ```powershell
   winget install Rcmake.Rcmake
   ```
