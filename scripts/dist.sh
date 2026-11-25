#!/usr/bin/env bash
set -euo pipefail

verify_extracted() {
  local python=python3
  case "$(uname -s)" in MINGW*|MSYS*) python=python ;; esac
  MSYS2_ARG_CONV_EXCL='*' "$python" - "$@" <<'PYVERIFY'
import os
from pathlib import Path
import platform
import re
import subprocess
import sys
from tempfile import TemporaryDirectory
from zipfile import ZipFile

package, target, *roots = sys.argv[1:]
package = Path(package).resolve(strict=True)
windows = target.endswith("-windows-msvc")
suffix = ".exe" if windows else ""
bin_dir = package / "bin"
tools = [bin_dir / (tool + suffix) for tool in ("rcmake", "cmake", "ctest", "cpack")]
if set(bin_dir.iterdir()) != set(tools) or not all(tool.is_file() for tool in tools):
    sys.exit("Expected four CLI entry points in the extracted package.")

paths = set()
for entry in roots:
    category, _, raw = entry.partition("=")
    directory = Path(raw)
    for path in (raw, str(directory.absolute()), str(directory.resolve())):
        if Path(path) == Path(Path(path).anchor):
            continue
        path = path.rstrip("/\\" if windows else "/")
        paths.add((category, path))
        if windows and path.startswith(("\\\\", "//")) and len(path) > 2 and path[2] not in "?.":
            paths.add((category, "\\\\?\\UNC\\" + path[2:]))

for tool in tools:
    contents = tool.read_bytes()
    if windows:
        decoded = [("UTF-8", contents.decode("utf-8", errors="replace"))]
        decoded.extend(("UTF-16LE", contents[offset:].decode("utf-16le", errors="replace"))
                       for offset in (0, 1))
    for category, path in sorted(paths):
        if windows:
            pattern = r"[\\/]".join(re.escape(part) for part in re.split(r"[\\/]", path))
            matches = (encoding for encoding, data in decoded if re.search(pattern, data, re.IGNORECASE))
        else:
            matches = (encoding for encoding, data in (
                ("filesystem bytes", os.fsencode(path)), ("UTF-16LE", path.encode("utf-16le", "surrogatepass")),
            ) if data in contents)
        if encoding := next(matches, None):
            sys.exit(f"{tool.name} contains a {category} path in {encoding}.")

system = platform.system()
machine = platform.machine().lower()
machine = {"arm64": "aarch64", "amd64": "x86_64"}.get(machine, machine)
native_suffix = {"Linux": "unknown-linux-gnu", "Darwin": "apple-darwin", "Windows": "pc-windows-msvc"}[system]
if target != f"{machine}-{native_suffix}":
    print(f"Verified release paths for {target}. Smoke tests require a native build.")
    sys.exit(0)

env = os.environ.copy()
for variable in ("RCMAKE_VENDORED_DATA_ROOT", "DESTDIR", "CMAKE_INSTALL_MODE"):
    env.pop(variable, None)
_, cmake, _, cpack = tools
for tool, expected in zip(tools, ("cmake", "cmake", "ctest", "cpack")):
    version = subprocess.check_output([tool, "--version"], env=env, text=True, timeout=15)
    if not version.startswith(f"{expected} version ") or "Rcmake" not in version:
        sys.exit(f"Unexpected version output from {tool.name}. Found {version}")
    print(version, end="", flush=True)
subprocess.run([cmake, "--help-command", "install"], env=env, check=True, timeout=15,
               stdout=subprocess.DEVNULL)

with TemporaryDirectory(prefix="rcmake-release-smoke-") as temporary:
    source = Path(temporary)
    build = source / "build"
    (source / "payload.txt").write_bytes(b"Rcmake release smoke\n")
    (source / "CMakeLists.txt").write_text('''cmake_minimum_required(VERSION 4.5)
project(ReleaseSmoke VERSION 1.0 LANGUAGES NONE)
file(WRITE "${CMAKE_CURRENT_BINARY_DIR}/cmake-root.txt" "${CMAKE_ROOT}")
install(FILES payload.txt DESTINATION share)
set(CPACK_PACKAGE_FILE_NAME release-smoke)
set(CPACK_GENERATOR ZIP)
include(CPack)
''', encoding="utf-8")
    subprocess.run([cmake, "-S", source, "-B", build, "-G", "Ninja"],
                   env=env, check=True, timeout=30)
    cmake_root = Path((build / "cmake-root.txt").read_text(encoding="utf-8"))
    if not cmake_root.is_absolute() or cmake_root.resolve() != (package / "share/cmake-4.5").resolve():
        sys.exit(f"Expected the bundled CMAKE_ROOT. Found {cmake_root}.")
    subprocess.run([cpack, "--config", build / "CPackConfig.cmake", "-B", build],
                   env=env, check=True, timeout=30)
    with ZipFile(build / "release-smoke.zip") as archive:
        members = [name for name in archive.namelist()
                   if name == "share/payload.txt" or name.endswith("/share/payload.txt")]
        if len(members) != 1 or archive.read(members[0]) != b"Rcmake release smoke\n":
            sys.exit("The CPack archive does not contain the expected payload.")
PYVERIFY
}

if [[ "${1:-}" == --verify-extracted ]]; then
  shift
  verify_extracted "$@"
  exit
fi

root_logical="$(cd "$(dirname "$0")/.." && pwd -L)"
root="$(cd "$root_logical" && pwd -P)"
out_dir="$root/dist"
target=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --target|--out-dir)
      if [[ $# -lt 2 || -z "$2" || "$2" == --* ]]; then
        printf '%s requires a value.\n' "$1" >&2
        exit 2
      fi
      if [[ "$1" == --target ]]; then target="$2"; else out_dir="$2"; fi
      shift 2
      ;;
    -h|--help)
      printf 'Usage: %s [--target <triple>] [--out-dir <dir>]\n' "$0"
      exit 0
      ;;
    *) printf 'Unknown option %s.\n' "$1" >&2; exit 2 ;;
  esac
done

mkdir -p "$out_dir"
out_dir="$(cd "$out_dir" && pwd -P)"
cd "$root"
# shellcheck disable=SC1091
source "$root/env/env.sh"
if [[ -z "$target" ]]; then
  target="$(rustc -vV | awk '$1 == "host:" { print $2 }')"
fi
case "$target" in
  x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu|x86_64-apple-darwin|aarch64-apple-darwin) ;;
  *) printf 'Unsupported tarball target %s.\n' "$target" >&2; exit 2 ;;
esac

version="$(awk '
  /^\[workspace\.package\]$/ { workspace = 1; next }
  /^\[/ { workspace = 0 }
  workspace && /^version[[:space:]]*=/ { split($0, fields, "\""); print fields[2]; exit }
' Cargo.toml)"
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][A-Za-z0-9.+-]+)?$ ]]; then
  printf 'Invalid version in Cargo.toml.\n' >&2
  exit 1
fi

cargo_dir="${CARGO_HOME:-$HOME/.cargo}"
rustup_dir="${RUSTUP_HOME:-$HOME/.rustup}"
target_dir="${CARGO_TARGET_DIR:-$root/target}"
case "$cargo_dir" in /*) ;; *) cargo_dir="$root/$cargo_dir" ;; esac
case "$rustup_dir" in /*) ;; *) rustup_dir="$root/$rustup_dir" ;; esac
case "$target_dir" in /*) ;; *) target_dir="$root/$target_dir" ;; esac
mkdir -p "$target_dir"
target_dir_logical="$target_dir"
target_dir="$(cd "$target_dir" && pwd -P)"
remap=(
  "--remap-path-prefix=$HOME=/rust-home"
  "--remap-path-prefix=$root=/rcmake"
  "--remap-path-prefix=$root_logical=/rcmake"
  "--remap-path-prefix=$target_dir=/target"
  "--remap-path-prefix=$target_dir_logical=/target"
  "--remap-path-prefix=$cargo_dir=/cargo"
  "--remap-path-prefix=$rustup_dir=/rustup"
)
# Cargo uses the encoded value when the variable exists, even if it is empty.
separator=$'\x1f'
if [[ ${CARGO_ENCODED_RUSTFLAGS+x} ]]; then
  flags="$CARGO_ENCODED_RUSTFLAGS"
else
  flags="$(python3 - <<'PYFLAGS'
import os
import re
import sys

words = re.findall(r"[^ \t\n\r\v\f]+", os.environ.get("RUSTFLAGS", ""))
sys.stdout.write("\x1f".join(words))
PYFLAGS
)"
fi
for flag in "${remap[@]}"; do
  flags="${flags:+$flags$separator}$flag"
done
export CARGO_ENCODED_RUSTFLAGS="$flags"
unset RUSTFLAGS

cargo build --locked --release --bin rcmake --target "$target" --target-dir "$target_dir"

name="rcmake-$version-$target"
work="$(mktemp -d "$out_dir/.rcmake-dist.XXXXXXXX")"
trap 'rm -rf "$work"' EXIT
python3 - "$root" "$work" "$name" "$target_dir/$target/release/rcmake" <<'PY'
import gzip
import hashlib
from pathlib import Path
import shutil
import sys
import tarfile

root, work, name, binary = sys.argv[1:]
root, work, binary = map(Path, (root, work, binary))
package = work / name
bin_dir = package / "bin"
bin_dir.mkdir(parents=True)
shutil.copyfile(binary, bin_dir / "rcmake")
for tool in ("cmake", "ctest", "cpack"):
    (bin_dir / tool).symlink_to("rcmake")
ignore = shutil.ignore_patterns("._*", ".DS_Store")
shutil.copytree(root / "modules/cmake-4.5", package / "share/cmake-4.5", ignore=ignore)
shutil.copytree(root / "third_party/licences", package / "third_party/licences", ignore=ignore)
for filename in ("LICENSE-MIT", "LICENSE-APACHE", "THIRD_PARTY_NOTICES.md"):
    shutil.copyfile(root / filename, package / filename)

def normalize(member):
    member.uid = member.gid = member.mtime = 0
    member.uname = member.gname = ""
    if member.issym():
        member.mode = 0o777
    elif member.isdir() or member.name == f"{name}/bin/rcmake":
        member.mode = 0o755
    else:
        member.mode = 0o644
    return member

archive = work / f"{name}.tar.gz"
with archive.open("wb") as stream, gzip.GzipFile(filename="", mode="wb", fileobj=stream, mtime=0) as compressed:
    with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as tar:
        tar.add(package, arcname=name, filter=normalize)

digest = hashlib.sha256(archive.read_bytes()).hexdigest()
archive.with_suffix(".gz.sha256").write_text(f"{digest}  {archive.name}\n", encoding="ascii")
PY

mkdir "$work/extracted"
tar -xzf "$work/$name.tar.gz" -C "$work/extracted"
verify_extracted "$work/extracted/$name" "$target" "home=$HOME" \
  "repository=$root_logical" "repository=$root" "repository=${GITHUB_WORKSPACE:-$root}" \
  "target directory=$target_dir_logical" "Cargo home=$cargo_dir" "Rustup home=$rustup_dir"
mv -f "$work/$name.tar.gz" "$work/$name.tar.gz.sha256" "$out_dir/"
printf '%s\n' "$out_dir/$name.tar.gz"
