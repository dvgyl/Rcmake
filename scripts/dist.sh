#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd -P)"
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
target_dir="$(cd "$target_dir" && pwd -P)"
remap=(
  "--remap-path-prefix=$HOME=/rust-home"
  "--remap-path-prefix=$root=/rcmake"
  "--remap-path-prefix=$target_dir=/target"
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

python3 - "$root" "$out_dir" "$version" "$target" "$target_dir/$target/release/rcmake" "$target_dir" "$cargo_dir" "$rustup_dir" <<'PY'
import gzip
import hashlib
import os
from pathlib import Path
import shutil
import sys
import tarfile
import tempfile

root, out_dir, version, target, binary, *build_paths = sys.argv[1:]
root, out_dir, binary = map(Path, (root, out_dir, binary))
home = Path(os.environ["HOME"])
contents = binary.read_bytes()
for directory in (home, root, *map(Path, build_paths)):
    paths = {directory, directory.resolve()}
    if any(path != Path("/") and os.fsencode(path) in contents for path in paths):
        sys.exit(f"The release binary contains the build path {directory}.")

name = f"rcmake-{version}-{target}"
with tempfile.TemporaryDirectory(dir=out_dir, prefix=".rcmake-dist-") as temporary:
    work = Path(temporary)
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
        if member.isdir() or member.mode & 0o111 or member.name == f"{name}/bin/rcmake":
            member.mode = 0o755
        else:
            member.mode = 0o644
        if member.issym():
            member.mode = 0o777
        return member

    archive = work / f"{name}.tar.gz"
    with archive.open("wb") as stream, gzip.GzipFile(filename="", mode="wb", fileobj=stream, mtime=0) as compressed:
        with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as tar:
            tar.add(package, arcname=name, filter=normalize)

    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    checksum = archive.with_suffix(".gz.sha256")
    checksum.write_text(f"{digest}  {archive.name}\n", encoding="ascii")
    archive.replace(out_dir / archive.name)
    checksum.replace(out_dir / checksum.name)
    print(out_dir / archive.name)
PY
