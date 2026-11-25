#!/usr/bin/env bash
# rcmake local environment: build limits and CMake 4.5.1 oracle paths.
# Source this file before you run cargo, tests, or the differential oracle scripts:
#   source env/env.sh

_RCMAKE_ENV_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
export RCMAKE_ROOT="$(cd "$_RCMAKE_ENV_DIR/.." && pwd)"

# Find the oracle directory. A git worktree has an empty local env/oracle.
# In that case, use env/oracle of the main checkout. The main checkout is the
# parent directory of the shared git directory (git rev-parse --git-common-dir).
_RCMAKE_ORACLE_DIR="$RCMAKE_ROOT/env/oracle"
if [[ ! -d "$_RCMAKE_ORACLE_DIR/bin" ]] && command -v git >/dev/null 2>&1; then
  _RCMAKE_COMMON_DIR="$(git -C "$RCMAKE_ROOT" rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)"
  if [[ -n "$_RCMAKE_COMMON_DIR" && -d "$(dirname "$_RCMAKE_COMMON_DIR")/env/oracle/bin" ]]; then
    _RCMAKE_ORACLE_DIR="$(cd "$(dirname "$_RCMAKE_COMMON_DIR")/env/oracle" && pwd)"
  fi
  unset _RCMAKE_COMMON_DIR
fi
export RCMAKE_ORACLE_DIR="$_RCMAKE_ORACLE_DIR"
unset _RCMAKE_ORACLE_DIR

# Put the pinned local tools (ninja), Rust, and Homebrew (if present) on PATH.
if [[ -d /opt/homebrew/bin ]]; then
  export PATH="/opt/homebrew/bin:$PATH"
fi
export PATH="$RCMAKE_ORACLE_DIR/bin:$HOME/.cargo/bin:$PATH"

# Default concurrency limits. They keep memory use low on small machines.
# Set larger values before you source this file to build faster.
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
export RUST_TEST_THREADS="${RUST_TEST_THREADS:-3}"
export COPYFILE_DISABLE=1

# Pinned CMake 4.5.1 oracle paths. scripts/fetch-oracle.sh uses the same layout.
_RCMAKE_ORACLE_EXE=""
case "$(uname -s):$(uname -m)" in
  Linux:x86_64) _RCMAKE_ORACLE_PLATFORM=linux-x86_64 ;;
  Linux:aarch64|Linux:arm64) _RCMAKE_ORACLE_PLATFORM=linux-aarch64 ;;
  MINGW*:aarch64|MSYS*:aarch64) _RCMAKE_ORACLE_PLATFORM=windows-arm64; _RCMAKE_ORACLE_EXE=.exe ;;
  MINGW*|MSYS*) _RCMAKE_ORACLE_PLATFORM=windows-x86_64; _RCMAKE_ORACLE_EXE=.exe ;;
  *) _RCMAKE_ORACLE_PLATFORM=macos-universal ;;
esac
export RCMAKE_ORACLE_ROOT="$RCMAKE_ORACLE_DIR/cmake-4.5.1-$_RCMAKE_ORACLE_PLATFORM"
if [[ "$_RCMAKE_ORACLE_PLATFORM" == macos-universal ]]; then
  RCMAKE_ORACLE_ROOT="$RCMAKE_ORACLE_ROOT/CMake.app/Contents"
fi
export RCMAKE_ORACLE_BIN_DIR="$RCMAKE_ORACLE_ROOT/bin"
export RCMAKE_ORACLE_CMAKE="$RCMAKE_ORACLE_BIN_DIR/cmake$_RCMAKE_ORACLE_EXE"
export RCMAKE_ORACLE_CTEST="$RCMAKE_ORACLE_BIN_DIR/ctest$_RCMAKE_ORACLE_EXE"
export RCMAKE_ORACLE_CPACK="$RCMAKE_ORACLE_BIN_DIR/cpack$_RCMAKE_ORACLE_EXE"
export RCMAKE_ORACLE_SRC="$RCMAKE_ORACLE_DIR/cmake-4.5.1"
export RCMAKE_ORACLE_RUNCMAKE_DIR="$RCMAKE_ORACLE_SRC/Tests/RunCMake"
unset _RCMAKE_ORACLE_PLATFORM _RCMAKE_ORACLE_EXE

# Vendored CMake 4.5.1 Modules/ and Templates/ root
export RCMAKE_VENDORED_DATA_ROOT="$RCMAKE_ROOT/modules/cmake-4.5"

unset _RCMAKE_ENV_DIR
