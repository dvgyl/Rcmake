#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

if [[ -f "$ROOT/env/env.sh" ]]; then
  # shellcheck disable=SC1091
  source "$ROOT/env/env.sh"
fi

echo "==> [1/6] Licence & clean-room hygiene scan"
python3 tools/measure/licence_scan.py

echo "==> [2/6] Rust formatting check"
cargo fmt --all -- --check

echo "==> [3/6] Clippy (all targets, -D warnings)"
cargo clippy --workspace --all-targets -- -D warnings

echo "==> [4/6] Rustdoc (-D warnings)"
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

echo "==> [5/6] Workspace unit & integration tests"
cargo test --workspace -- --test-threads="${RUST_TEST_THREADS:-3}"

echo "==> [6/6] Stage & Layer 1 Oracle Differential Suites"
if [[ -x "${RCMAKE_ORACLE_CMAKE:-}" ]]; then
  ./scripts/stage.sh
  cargo run --quiet --bin rcmake-harness -- --suite all
else
  echo "WARNING: RCMAKE_ORACLE_CMAKE not found at '${RCMAKE_ORACLE_CMAKE:-}'; skipping oracle smoke." >&2
  exit 1
fi

echo "==> ALL GATES PASSED"
