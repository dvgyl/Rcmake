#!/usr/bin/env bash
# Rcmake local environment configuration (16 GB RAM safe defaults + CMake 4.5.1 oracle).
# Source this file before running cargo, tests, or differential oracle scripts:
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

# Put Rust, Homebrew, and the pinned local tools (ninja) on PATH.
export PATH="$RCMAKE_ORACLE_DIR/bin:$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"

# 16 GB RAM macOS arm64 concurrency caps (AGENTS.md)
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
export RUST_TEST_THREADS="${RUST_TEST_THREADS:-3}"
export COPYFILE_DISABLE=1

# Pinned CMake 4.5.1 oracle paths
export RCMAKE_ORACLE_ROOT="$RCMAKE_ORACLE_DIR/cmake-4.5.1-macos-universal/CMake.app/Contents"
export RCMAKE_ORACLE_BIN_DIR="$RCMAKE_ORACLE_ROOT/bin"
export RCMAKE_ORACLE_CMAKE="$RCMAKE_ORACLE_BIN_DIR/cmake"
export RCMAKE_ORACLE_CTEST="$RCMAKE_ORACLE_BIN_DIR/ctest"
export RCMAKE_ORACLE_CPACK="$RCMAKE_ORACLE_BIN_DIR/cpack"
export RCMAKE_ORACLE_SRC="$RCMAKE_ORACLE_DIR/cmake-4.5.1"
export RCMAKE_ORACLE_RUNCMAKE_DIR="$RCMAKE_ORACLE_SRC/Tests/RunCMake"

# Vendored CMake 4.5.1 Modules/ & Templates/ root
export RCMAKE_VENDORED_DATA_ROOT="$RCMAKE_ROOT/modules/cmake-4.5"

unset _RCMAKE_ENV_DIR
