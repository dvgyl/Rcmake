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

registry="${CARGO_HOME:-$HOME/.cargo}/registry/src"
target_dir="${CARGO_TARGET_DIR:-$root/target}"
case "$registry" in /*) ;; *) registry="$root/$registry" ;; esac
remap=(
  "--remap-path-prefix=$HOME=/rust-home"
  "--remap-path-prefix=$root=/rcmake"
  "--remap-path-prefix=$registry=/cargo/registry/src"
  "--remap-path-prefix=$HOME/.cargo/registry/src=/cargo/registry/src"
)
# Cargo splits RUSTFLAGS at spaces. Encoded flags keep each path in one argument.
separator=$'\x1f'
flags="${CARGO_ENCODED_RUSTFLAGS:-${RUSTFLAGS:-}}"
if [[ -z "${CARGO_ENCODED_RUSTFLAGS:-}" ]]; then
  flags="${flags// /$separator}"
fi
for flag in "${remap[@]}"; do
  flags="${flags:+$flags$separator}$flag"
done
export CARGO_ENCODED_RUSTFLAGS="$flags"
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }${remap[*]}"

cargo build --locked --release --bin rcmake --target "$target" --target-dir "$target_dir"

python3 - "$root" "$out_dir" "$version" "$target" "$target_dir/$target/release/rcmake" <<'PY'
import gzip
import hashlib
import os
from pathlib import Path
import shutil
import sys
import tarfile
import tempfile

root, out_dir, version, target, binary = sys.argv[1:]
root, out_dir, binary = map(Path, (root, out_dir, binary))
home = Path(os.environ["HOME"])
contents = binary.read_bytes()
if any(os.fsencode(path) in contents for path in {home, home.resolve()}):
    sys.exit("The release binary contains a home directory path.")

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
