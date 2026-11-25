#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

if [[ -f "$ROOT/env/env.sh" ]]; then
  # shellcheck disable=SC1091
  source "$ROOT/env/env.sh"
fi

TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
case "$TARGET_DIR" in /*) ;; *) TARGET_DIR="$ROOT/$TARGET_DIR" ;; esac
PROFILE="${RCMAKE_STAGE_PROFILE:-debug}"
if [[ "$PROFILE" == "release" ]]; then
  cargo build --locked --release --bin rcmake --target-dir "$TARGET_DIR"
  BIN_SRC="$TARGET_DIR/release/rcmake"
else
  cargo build --locked --bin rcmake --target-dir "$TARGET_DIR"
  BIN_SRC="$TARGET_DIR/debug/rcmake"
fi

STAGE="$ROOT/target/stage"
rm -rf "$STAGE"
mkdir -p "$STAGE/bin" "$STAGE/share/cmake-4.5"

cp "$BIN_SRC" "$STAGE/bin/rcmake"
ln -sf rcmake "$STAGE/bin/cmake"
ln -sf rcmake "$STAGE/bin/ctest"
ln -sf rcmake "$STAGE/bin/cpack"

# Symlink vendored CMake 4.5.1 Modules/ and Templates/ so CMAKE_ROOT resolves cleanly on disk.
ln -s "$ROOT/modules/cmake-4.5/Modules" "$STAGE/share/cmake-4.5/Modules"
ln -s "$ROOT/modules/cmake-4.5/Templates" "$STAGE/share/cmake-4.5/Templates"

echo "Staged Rcmake at $STAGE/bin/{rcmake,cmake,ctest,cpack} with share/cmake-4.5"
