#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
set -euo pipefail

mode=release
archive=""
alias_cmake=false
force=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --from-source|--archive)
      if [[ "$mode" != release ]]; then
        printf 'Select one install source.\n' >&2
        exit 2
      fi
      if [[ "$1" == --archive ]]; then
        if [[ $# -lt 2 || -z "$2" || "$2" == --* ]]; then
          printf '%s requires a path.\n' "$1" >&2
          exit 2
        fi
        mode=archive
        archive="$2"
        shift
      else
        mode=source
      fi
      ;;
    --alias-cmake) alias_cmake=true ;;
    --force) force=true ;;
    -h|--help)
      printf 'Usage: %s [--from-source | --archive <path>] [--alias-cmake] [--force]\n' "$0"
      printf "Install into PREFIX, if set. The default is \$HOME/.local.\n"
      printf 'The default source is the latest GitHub Release.\n'
      exit 0
      ;;
    *) printf 'Unknown option %s.\n' "$1" >&2; exit 2 ;;
  esac
  shift
done

work="$(mktemp -d "${TMPDIR:-/tmp}/rcmake-install.XXXXXXXX")"
trap 'rm -rf "$work"' EXIT
checksum=""
case "$mode" in
  source)
    root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
    "$root/scripts/dist.sh" --out-dir "$work"
    archives=("$work"/*.tar.gz)
    archive="${archives[0]}"
    checksum="$archive.sha256"
    ;;
  archive)
    if [[ -f "$archive.sha256" ]]; then
      checksum="$archive.sha256"
    fi
    ;;
  release)
    case "$(uname -s):$(uname -m)" in
      Linux:x86_64) target=x86_64-unknown-linux-gnu ;;
      Linux:aarch64|Linux:arm64) target=aarch64-unknown-linux-gnu ;;
      Darwin:arm64) target=aarch64-apple-darwin ;;
      *) printf 'No release tarball for this host. Use --from-source on Linux or macOS.\n' >&2; exit 1 ;;
    esac
    releases=https://github.com/dvgyl/rcmake/releases
    checksum="$work/SHA256SUMS.txt"
    curl -fsSL --retry 3 --connect-timeout 20 --max-time 600 \
      "$releases/latest/download/SHA256SUMS.txt" -o "$checksum"
    name="$(awk -v suffix="-$target.tar.gz" '
      substr($2, length($2) - length(suffix) + 1) == suffix { print $2 }
    ' "$checksum")"
    if [[ ! "$name" =~ ^rcmake-[0-9]+\.[0-9]+\.[0-9]+([-+][A-Za-z0-9.+-]+)?-${target}\.tar\.gz$ ]]; then
      printf 'No unique release archive for %s.\n' "$target" >&2
      exit 1
    fi
    archive="$work/$name"
    curl -fsSL --retry 3 --connect-timeout 20 --max-time 600 \
      "$releases/latest/download/$name" -o "$archive"
    ;;
esac

python3 - "$archive" "$checksum" "${PREFIX:-$HOME/.local}" "$alias_cmake" "$force" <<'PY'
import hashlib
import os
from pathlib import Path, PurePosixPath
import shutil
import subprocess
import sys
import tarfile
import tempfile

archive, checksum, prefix, aliases, force = sys.argv[1:]
archive, prefix = Path(archive), Path(prefix)
tools = ("cmake", "ctest", "cpack")
data_dir = Path("share/cmake-4.5")

try:
    if checksum:
        lines = Path(checksum).read_text(encoding="ascii").splitlines()
        expected = [fields[0] for line in lines if len(fields := line.split(maxsplit=1)) == 2
                    and fields[1].lstrip("*") == archive.name]
        if expected != [hashlib.sha256(archive.read_bytes()).hexdigest()]:
            raise ValueError(f"SHA-256 mismatch for {archive.name}.")

    prefix.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=prefix, prefix=".rcmake-install-") as temporary:
        work = Path(temporary)
        top = None
        with tarfile.open(archive, "r:gz") as tar:
            for member in tar:
                path = PurePosixPath(member.name)
                if path.is_absolute() or ".." in path.parts or not path.parts:
                    raise ValueError(f"Invalid archive path {member.name!r}.")
                if top is None:
                    top = path.parts[0]
                if path.parts[0] != top:
                    raise ValueError("The archive must have one top-level directory.")
                relative = Path(*path.parts[1:])
                if relative == Path("bin/rcmake") or relative.is_relative_to(data_dir):
                    output = work / relative
                    if member.isdir():
                        output.mkdir(parents=True, exist_ok=True)
                    elif member.isfile():
                        output.parent.mkdir(parents=True, exist_ok=True)
                        with tar.extractfile(member) as source, output.open("xb") as destination:
                            shutil.copyfileobj(source, destination)
                        output.chmod(0o755 if member.mode & 0o111 else 0o644)
                    else:
                        raise ValueError(f"Invalid archive entry {member.name!r}.")

        staged_bin = work / "bin/rcmake"
        staged_bin.chmod(0o755)
        for required in ("Modules/CMake.cmake", "Templates/CPackConfig.cmake.in"):
            if not (work / data_dir / required).is_file():
                raise ValueError(f"Missing {required} in the archive.")
        subprocess.run([staged_bin, "--version"], check=True, stdout=subprocess.DEVNULL, timeout=30)

        bin_dir = prefix / "bin"
        bin_dir.mkdir(exist_ok=True)
        installed_data = prefix / data_dir
        installed_data.parent.mkdir(exist_ok=True)
        backup = work / "previous-data"
        if os.path.lexists(installed_data):
            installed_data.replace(backup)
        try:
            (work / data_dir).replace(installed_data)
            staged_bin.replace(bin_dir / "rcmake")
        except OSError:
            if installed_data.exists():
                installed_data.replace(work / data_dir)
            if os.path.lexists(backup):
                backup.replace(installed_data)
            raise

        if aliases == "true":
            for tool in tools:
                alias = bin_dir / tool
                if alias.is_symlink() and os.readlink(alias) == "rcmake":
                    continue
                if os.path.lexists(alias) and (force != "true" or (alias.is_dir() and not alias.is_symlink())):
                    print(f"Keep existing {alias}.", file=sys.stderr)
                    continue
                new_alias = work / tool
                new_alias.symlink_to("rcmake")
                new_alias.replace(alias)
        subprocess.run([bin_dir.resolve() / "rcmake", "--version"], check=True, timeout=30)
except (OSError, ValueError, tarfile.TarError, subprocess.SubprocessError) as error:
    sys.exit(f"Install failed. {error}")
PY
