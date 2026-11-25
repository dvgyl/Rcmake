#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
set -euo pipefail

print_env=false
case "${1:-}" in
  "") ;;
  --print-env) print_env=true; shift ;;
  -h|--help)
    printf 'Usage: %s [--print-env]\n' "$0"
    printf 'Download CMake 4.5.1. Use --print-env to print Bash exports for the oracle tools.\n'
    exit 0
    ;;
  *) printf 'Unknown option %s.\n' "$1" >&2; exit 2 ;;
esac
if [[ $# -ne 0 ]]; then
  printf 'Unexpected argument %s.\n' "$1" >&2
  exit 2
fi

root="$(cd "$(dirname "$0")/.." && pwd -P)"
oracle="$root/env/oracle"
release=cmake-4.5.1
base_url=https://github.com/Kitware/CMake/releases/download/v4.5.1
manifest="$release-SHA-256.txt"
manifest_hash=3ee8e6c91ca39eeaf78b99008198ddde4e266f8f608ebd73fe8e92476dfeaf08
archive_ext=.tar.gz
exe=""
case "$(uname -s):$(uname -m)" in
  Linux:x86_64) platform=linux-x86_64 ;;
  Linux:aarch64|Linux:arm64) platform=linux-aarch64 ;;
  Darwin:*) platform=macos-universal ;;
  MINGW*:x86_64|MSYS*:x86_64)
    platform=windows-x86_64
    archive_ext=.zip
    exe=.exe
    ;;
  MINGW*:aarch64|MSYS*:aarch64)
    platform=windows-arm64
    archive_ext=.zip
    exe=.exe
    ;;
  *) printf 'No CMake 4.5.1 archive for this host.\n' >&2; exit 1 ;;
esac
install_root="$oracle/$release-$platform"
if [[ "$platform" == macos-universal ]]; then
  install_root="$install_root/CMake.app/Contents"
fi
bin_dir="$install_root/bin"
source_dir="$oracle/$release"

mkdir -p "$oracle"
work="$(mktemp -d "$oracle/.fetch.XXXXXXXX")"
trap 'rm -rf "$work"' EXIT

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum < "$1"
  else
    shasum -a 256 < "$1"
  fi
}

download() {
  local name="$1" hash="$2" actual
  if [[ -f "$oracle/$name" ]]; then
    actual="$(sha256 "$oracle/$name")"
    if [[ "${actual%% *}" == "$hash" ]]; then
      return
    fi
  fi
  printf 'Download %s.\n' "$name" >&2
  curl -fsSL --retry 3 --connect-timeout 20 --max-time 600 "$base_url/$name" -o "$work/$name"
  actual="$(sha256 "$work/$name")"
  if [[ "${actual%% *}" != "$hash" ]]; then
    printf 'SHA-256 mismatch for %s.\n' "$name" >&2
    exit 1
  fi
  mv -f "$work/$name" "$oracle/$name"
}

fetch_archive() {
  local name="$1" extension="$2" hash
  hash="$(awk -v file="$name$extension" '$2 == file { print $1 }' "$oracle/$manifest")"
  if [[ ! "$hash" =~ ^[0-9a-f]{64}$ ]]; then
    printf 'Missing SHA-256 for %s.\n' "$name$extension" >&2
    exit 1
  fi
  download "$name$extension" "$hash"
  if [[ ! -d "$oracle/$name" ]]; then
    if [[ "$extension" == .zip ]]; then
      unzip -q "$oracle/$name$extension" -d "$work"
    else
      tar -xzf "$oracle/$name$extension" -C "$work"
    fi
    mv "$work/$name" "$oracle/"
  fi
}

download "$manifest" "$manifest_hash"
fetch_archive "$release-$platform" "$archive_ext"
fetch_archive "$release" .tar.gz
for tool in cmake ctest cpack; do
  output="$("$bin_dir/$tool$exe" --version)"
  first_line="${output%%$'\n'*}"
  if [[ "${first_line%$'\r'}" != "$tool version 4.5.1" ]]; then
    printf 'Invalid version for %s.\n' "$bin_dir/$tool$exe" >&2
    exit 1
  fi
done
test -f "$source_dir/Tests/RunCMake/RunCMake.cmake"

if "$print_env"; then
  printf 'export RCMAKE_ORACLE_DIR=%q\n' "$oracle"
  printf 'export RCMAKE_ORACLE_ROOT=%q\n' "$install_root"
  printf 'export RCMAKE_ORACLE_BIN_DIR=%q\n' "$bin_dir"
  printf 'export RCMAKE_ORACLE_CMAKE=%q\n' "$bin_dir/cmake$exe"
  printf 'export RCMAKE_ORACLE_CTEST=%q\n' "$bin_dir/ctest$exe"
  printf 'export RCMAKE_ORACLE_CPACK=%q\n' "$bin_dir/cpack$exe"
  printf 'export RCMAKE_ORACLE_SRC=%q\n' "$source_dir"
  printf 'export RCMAKE_ORACLE_RUNCMAKE_DIR=%q\n' "$source_dir/Tests/RunCMake"
else
  printf 'CMake 4.5.1 oracle is in %s.\n' "$oracle"
  printf "Run eval \"\$(scripts/fetch-oracle.sh --print-env)\" after you source env/env.sh.\n"
fi
