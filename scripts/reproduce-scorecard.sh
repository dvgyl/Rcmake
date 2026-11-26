#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Run the scorecard measurement lanes against one staged rcmake binary.
# Each lane writes one receipt with the file name that
# tools/measure/assemble_release_receipts.py expects in --receipts-dir.
# Every receipt records the release commit and the binary SHA-256.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PYTHON="${PYTHON:-python3}"

ALL_LANES="binary-size bench-eval bench-configure bench-giants probe-cache mutation determinism wasm diff-fuzz blackbox cli-ux path-swap ecosystem runcmake assemble"
ORACLE_LANES="bench-eval bench-configure bench-giants probe-cache determinism diff-fuzz blackbox cli-ux path-swap ecosystem runcmake"

usage() {
  cat <<EOF
Usage: scripts/reproduce-scorecard.sh [options]

Run scorecard lanes against one staged rcmake binary. Write each receipt with
the standardized file name that tools/measure/assemble_release_receipts.py reads.

Options:
  --rcmake <path>        rcmake binary (default: target/stage/bin/rcmake)
  --lsp <path>           rcmake-lsp binary (default: rcmake-lsp next to --rcmake)
  --receipts-dir <dir>   receipt output directory (default: /tmp/rcmake-scorecard-receipts)
  --commit <sha>         release commit to record (default: git rev-parse HEAD)
  --lane <name>          lane to run; repeat for more lanes, or use "all" (default: all)
  --runs <n>             benchmark runs (default: 5 for synthetic, 3 for giants)
  --dry-run              print each command and receipt path; run nothing
  -h, --help             show this help

Lanes and receipts:
  binary-size       binary-size.json
  bench-eval        bench-eval.json (paired with bench-configure.json)
  bench-configure   bench-configure.json
  bench-giants      bench-giants.json
  probe-cache       probe-cache.json
  mutation          mutation-replay.json
  determinism       determinism.json
  wasm              wasi.json
  diff-fuzz         differential-fuzz.json
  blackbox          independent-scenarios.json
  cli-ux            cli-ux.json
  path-swap         path-swap.json
  ecosystem         conan.json
  runcmake          runcmake-macos.json, runcmake-linux.json, or runcmake-windows.json
  assemble          assemble_release_receipts.py --status (no receipt)

Other required lanes come from CI or from other hosts: the other two RunCMake
hosts, large-projects, vcpkg, unseen-holdout, fuzz, and ci-jobs.
Strict assembly also needs dist.sh tarball fields in the dist lane.

The wasm lane builds the WebAssembly example with cargo.
Logs and Markdown reports go to <receipts-dir>/raw/.

Environment:
  PYTHON                 Python interpreter for the lane tools (default: python3)
EOF
}

die() {
  printf 'reproduce-scorecard: %s\n' "$1" >&2
  exit 2
}

contains() {
  case " $1 " in *" $2 "*) return 0 ;; esac
  return 1
}

absolute_path() {
  case "$1" in
    /*) printf '%s\n' "$1" ;;
    *) printf '%s/%s\n' "$(pwd)" "$1" ;;
  esac
}

RCMAKE="$ROOT/target/stage/bin/rcmake"
LSP=""
RECEIPTS_DIR="/tmp/rcmake-scorecard-receipts"
COMMIT=""
LANES=""
RUNS=""
DRY_RUN=0

while [ $# -gt 0 ]; do
  case "$1" in
    --rcmake|--lsp|--receipts-dir|--commit|--lane|--runs)
      [ $# -ge 2 ] || die "$1 needs a value."
      case "$1" in
        --rcmake) RCMAKE="$2" ;;
        --lsp) LSP="$2" ;;
        --receipts-dir) RECEIPTS_DIR="$2" ;;
        --commit) COMMIT="$2" ;;
        --lane) LANES="$LANES $2" ;;
        --runs) RUNS="$2" ;;
      esac
      shift 2
      ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown option $1. Run with --help." ;;
  esac
done

SELECTED=""
for lane in ${LANES:-all}; do
  if [ "$lane" = all ]; then
    SELECTED=" $ALL_LANES"
  elif contains "$ALL_LANES" "$lane"; then
    contains "$SELECTED" "$lane" || SELECTED="$SELECTED $lane"
  else
    die "unknown lane $lane. Lanes: $ALL_LANES, all."
  fi
done

if [ -n "$RUNS" ]; then
  case "$RUNS" in ''|*[!0-9]*|0) die "--runs must be a positive integer." ;; esac
  SYNTHETIC_RUNS="$RUNS"
  GIANTS_RUNS="$RUNS"
else
  SYNTHETIC_RUNS=5
  GIANTS_RUNS=3
fi

RCMAKE="$(absolute_path "$RCMAKE")"
LSP="$(absolute_path "${LSP:-$(dirname "$RCMAKE")/rcmake-lsp}")"
RECEIPTS_DIR="$(absolute_path "$RECEIPTS_DIR")"
RAW_DIR="$RECEIPTS_DIR/raw"
BIN_DIR="$(dirname "$RCMAKE")"
STAGE_DIR="$(dirname "$BIN_DIR")"

if [ -z "$COMMIT" ]; then
  COMMIT="$(git -C "$ROOT" rev-parse HEAD 2>/dev/null)" || die "git rev-parse HEAD failed. Pass --commit."
fi
COMMIT="$(printf '%s' "$COMMIT" | tr 'A-F' 'a-f')"
case "$COMMIT" in *[!0-9a-f]*) die "--commit must be a hex SHA, got $COMMIT." ;; esac
[ "${#COMMIT}" -ge 7 ] && [ "${#COMMIT}" -le 40 ] || die "--commit must have 7 to 40 hex characters."

if [ -f "$ROOT/env/env.sh" ]; then
  set +u
  # shellcheck disable=SC1091
  source "$ROOT/env/env.sh"
  set -u
fi

case "$(uname -s)" in
  Darwin) RUNCMAKE_PROFILE=all-macos; RUNCMAKE_OS=macos ;;
  Linux) RUNCMAKE_PROFILE=linux; RUNCMAKE_OS=linux ;;
  MINGW*|MSYS*|CYGWIN*) RUNCMAKE_PROFILE=windows; RUNCMAKE_OS=windows ;;
  *) RUNCMAKE_PROFILE=""; RUNCMAKE_OS="" ;;
esac

ORACLE="${RCMAKE_ORACLE_CMAKE:-}"
if [ "$DRY_RUN" -eq 0 ]; then
  [ -x "$RCMAKE" ] || die "rcmake binary not found or not executable: $RCMAKE. Run scripts/stage.sh or pass --rcmake."
  for lane in $SELECTED; do
    if contains "$ORACLE_LANES" "$lane" && [ ! -x "$ORACLE" ]; then
      die "lane $lane needs the CMake 4.5.1 oracle. Run scripts/fetch-oracle.sh, then source env/env.sh."
    fi
  done
  if contains "$SELECTED" runcmake && [ -z "$RUNCMAKE_PROFILE" ]; then
    die "lane runcmake has no profile for host $(uname -s)."
  fi
  mkdir -p "$RAW_DIR"
else
  ORACLE="${ORACLE:-\$RCMAKE_ORACLE_CMAKE}"
fi

# One Python program does the JSON work: hashing, identity stamps, and lane summaries.
helper() {
  "$PYTHON" - "$ROOT" "$@" <<'PY'
import hashlib
import json
import os
from pathlib import Path
import platform
import sys
import tempfile

root, command, args = Path(sys.argv[1]), sys.argv[2], sys.argv[3:]
sys.path.insert(0, str(root / "tools" / "measure"))
from build_dashboard_json import REPORT_DIGEST_KEYS, REPORT_SHA_KEYS  # noqa: E402


def fail(message):
    sys.exit(f"reproduce-scorecard: {message}")


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as stream:
        for block in iter(lambda: stream.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def write(path, payload):
    path = Path(path)
    with tempfile.NamedTemporaryFile("w", encoding="utf-8", dir=path.parent, delete=False) as stream:
        stream.write(json.dumps(payload, indent=2) + "\n")
    os.chmod(stream.name, 0o644)
    os.replace(stream.name, path)


def stamp(path, commit, digest):
    """Record one commit and one binary digest; refuse a receipt that names others."""
    try:
        payload = json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        fail(f"{path}: cannot read JSON receipt ({error})")
    if not isinstance(payload, dict):
        fail(f"{path}: receipt is not a JSON object")
    for container in (payload, payload.get("receipt"), payload.get("meta")):
        if not isinstance(container, dict):
            continue
        for key in REPORT_SHA_KEYS:
            value = container.get(key)
            if isinstance(value, str) and value.strip():
                found = value.strip().lower()
                if not (commit.startswith(found) or found.startswith(commit)):
                    fail(f"{path}: {key} is {found}, expected {commit}")
                container[key] = commit
        for key in REPORT_DIGEST_KEYS:
            value = container.get(key)
            if isinstance(value, str) and value.strip():
                if value.strip().lower() != digest:
                    fail(f"{path}: {key} is {value.strip().lower()}, expected {digest}")
                container[key] = digest
    payload["release_commit"] = commit
    payload["binary_sha256"] = digest
    write(path, payload)


if command == "sha256":
    print(sha256(args[0]))
elif command == "stamp":
    stamp(*args)
elif command == "binary-size":
    out, binary, commit = args
    write(out, {
        "release_commit": commit,
        "binary_path": binary,
        "binary_size_bytes": os.path.getsize(binary),
        "binary_sha256": sha256(binary),
        "host": {"os": platform.system(), "arch": platform.machine()},
    })
elif command == "determinism":
    out, exit_code, log = args
    write(out, {
        "status": "pass" if exit_code == "0" else "fail",
        "exit_code": int(exit_code),
        "log": os.path.relpath(log, Path(out).parent),
    })
elif command == "cli-ux":
    out, verify_code, verify_log, polish_report = args
    try:
        polish = json.loads(Path(polish_report).read_text(encoding="utf-8"))
        polish_checks = polish["checks"]
    except (OSError, ValueError, KeyError) as error:
        fail(f"{polish_report}: cannot read the verify_polish.py report ({error})")
    checks = [{"check": "verify.py", "status": "pass" if verify_code == "0" else "fail",
               "log": os.path.relpath(verify_log, Path(out).parent)}]
    checks += [{"check": f"verify_polish.py:{row['id']}", "status": "pass" if row["passed"] else "fail"}
               for row in polish_checks]
    passed = sum(row["status"] == "pass" for row in checks)
    write(out, {
        "status": "pass" if passed == len(checks) else "fail",
        "passed": passed,
        "total": len(checks),
        "failures": len(checks) - passed,
        "checks": checks,
        "polish_report": os.path.relpath(polish_report, Path(out).parent),
    })
else:
    fail(f"unknown helper command {command}")
PY
}

if [ -f "$RCMAKE" ]; then
  BINARY_SHA256="$(helper sha256 "$RCMAKE")"
elif [ "$DRY_RUN" -eq 1 ]; then
  BINARY_SHA256="<sha256-of-rcmake>"
else
  die "rcmake binary not found: $RCMAKE"
fi

LANE=""
RECEIPT=""

# Print the receipt path. Remove an old receipt so a failed tool cannot leave a stale one.
receipt() {
  RECEIPT="$RECEIPTS_DIR/$1"
  printf '[%s] receipt: %s\n' "$LANE" "$RECEIPT"
  if [ "$DRY_RUN" -eq 0 ]; then
    rm -f "$RECEIPT"
  fi
}

# Print one command, then run it from the repository root. Output also goes to raw/<lane>.log.
run() {
  printf '[%s] $' "$LANE"
  printf ' %q' "$@"
  printf '\n'
  [ "$DRY_RUN" -eq 1 ] && return 0
  (cd "$ROOT" && "$@") 2>&1 | tee -a "$RAW_DIR/$LANE.log"
}

note() {
  printf '[%s] # %s\n' "$LANE" "$1"
}

# Stamp the commit and binary digest into the receipt. Fail when the tool wrote no receipt.
finish() {
  local status="$1"
  note "record release_commit and binary_sha256 in $(basename "$RECEIPT")"
  [ "$DRY_RUN" -eq 1 ] && return 0
  if [ ! -f "$RECEIPT" ]; then
    printf '[%s] no receipt was written (exit %s)\n' "$LANE" "$status" >&2
    return 1
  fi
  helper stamp "$RECEIPT" "$COMMIT" "$BINARY_SHA256" || return 1
  return "$status"
}

lane_binary_size() {
  receipt binary-size.json
  note "write binary path, byte size, SHA-256, and commit"
  [ "$DRY_RUN" -eq 1 ] && return 0
  helper binary-size "$RECEIPT" "$RCMAKE" "$COMMIT"
}

lane_bench_eval() {
  local status=0
  receipt bench-eval.json
  run "$PYTHON" tools/measure/bench_eval.py --rcmake "$RCMAKE" --runs "$SYNTHETIC_RUNS" --include-corpus \
    --json "$RECEIPT" --markdown "$RAW_DIR/bench-eval.md" || status=$?
  finish "$status"
}

lane_bench_configure() {
  local status=0
  receipt bench-configure.json
  run "$PYTHON" tools/measure/bench_configure.py --rcmake "$RCMAKE" --oracle "$ORACLE" --runs "$SYNTHETIC_RUNS" \
    --scenario all --json "$RECEIPT" --markdown "$RAW_DIR/bench-configure.md" || status=$?
  finish "$status"
}

lane_bench_giants() {
  local status=0
  receipt bench-giants.json
  run "$PYTHON" tools/measure/bench_giants.py --rcmake "$RCMAKE" --oracle "$ORACLE" --runs "$GIANTS_RUNS" \
    --rcmake-sha "$COMMIT" --binary-sha256 "$BINARY_SHA256" \
    --json "$RECEIPT" --markdown "$RAW_DIR/bench-giants.md" || status=$?
  finish "$status"
}

lane_probe_cache() {
  local status=0
  receipt probe-cache.json
  run "$PYTHON" tools/measure/probe_cache_eval.py --rcmake "$RCMAKE" --oracle "$ORACLE" \
    --out-json "$RECEIPT" --out-md "$RAW_DIR/probe-cache.md" || status=$?
  finish "$status"
}

lane_mutation() {
  local status=0
  receipt mutation-replay.json
  run "$PYTHON" tools/measure/mutation_eval.py --fixture --rcmake "$RCMAKE" \
    --out-json "$RECEIPT" --out-md "$RAW_DIR/mutation-replay.md" || status=$?
  finish "$status"
}

lane_determinism() {
  local status=0
  receipt determinism.json
  run "$PYTHON" tools/measure/determinism.py --rcmake "$RCMAKE" --baseline "$ORACLE" || status=$?
  note "write status and exit code from determinism.py to determinism.json"
  [ "$DRY_RUN" -eq 1 ] && return 0
  helper determinism "$RECEIPT" "$status" "$RAW_DIR/determinism.log" || return 1
  finish "$status"
}

lane_wasm() {
  local status=0
  receipt wasi.json
  run "$PYTHON" tools/measure/wasm_smoke.py --require-wasi --json "$RECEIPT" --markdown "$RAW_DIR/wasi.md" || status=$?
  finish "$status"
}

lane_diff_fuzz() {
  local status=0
  receipt differential-fuzz.json
  run "$PYTHON" tools/measure/oracle_prop_test.py --rcmake-bin "$RCMAKE" --rcmake-sha "$COMMIT" \
    --binary-sha256 "$BINARY_SHA256" --out "$RECEIPT" || status=$?
  finish "$status"
}

lane_blackbox() {
  local status=0
  receipt independent-scenarios.json
  run "$PYTHON" tools/measure/independent_verify.py --stage "$STAGE_DIR" --oracle "$ORACLE" --json "$RECEIPT" || status=$?
  finish "$status"
}

lane_cli_ux() {
  local verify_status=0 polish_status=0
  receipt cli-ux.json
  run "$PYTHON" conformance/cases/cli-ux/verify.py --rcmake "$RCMAKE" --lsp "$LSP" --oracle "$ORACLE" || verify_status=$?
  run "$PYTHON" conformance/cases/cli-ux/verify_polish.py --rcmake "$RCMAKE" --lsp "$LSP" \
    --out-json "$RAW_DIR/cli-ux-polish.json" || polish_status=$?
  note "merge verify.py and verify_polish.py results into cli-ux.json"
  [ "$DRY_RUN" -eq 1 ] && return 0
  helper cli-ux "$RECEIPT" "$verify_status" "$RAW_DIR/cli-ux.log" "$RAW_DIR/cli-ux-polish.json" || return 1
  if [ "$verify_status" -ne 0 ]; then
    finish "$verify_status"
  else
    finish "$polish_status"
  fi
}

lane_path_swap() {
  local status=0
  receipt path-swap.json
  run "$PYTHON" tools/measure/path_swap_eval.py --rcmake-bin "$RCMAKE" --out "$RECEIPT" || status=$?
  finish "$status"
}

lane_ecosystem() {
  local status=0
  receipt conan.json
  run "$PYTHON" tools/corpus/ecosystem_eval.py --stage "$RCMAKE" --oracle "$ORACLE" \
    --out-json "$RECEIPT" --out-md "$RAW_DIR/conan.md" || status=$?
  finish "$status"
}

lane_runcmake() {
  local status=0
  receipt "runcmake-${RUNCMAKE_OS:-unknown}.json"
  run "$PYTHON" tools/measure/runcmake_eval.py --profile "${RUNCMAKE_PROFILE:-unknown}" --stage "$BIN_DIR" \
    --json "$RECEIPT" --min-pass-rate 99.0 || status=$?
  finish "$status"
}

lane_assemble() {
  run "$PYTHON" tools/measure/assemble_release_receipts.py --receipts-dir "$RECEIPTS_DIR" --commit "$COMMIT" --status
}

printf 'rcmake:       %s\n' "$RCMAKE"
printf 'commit:       %s\n' "$COMMIT"
printf 'binary:       %s\n' "$BINARY_SHA256"
printf 'receipts dir: %s\n' "$RECEIPTS_DIR"
[ "$DRY_RUN" -eq 1 ] && printf 'mode:         dry run (no command runs)\n'

FAILED=""
for LANE in $SELECTED; do
  printf '\n== %s\n' "$LANE"
  if ! "lane_$(printf '%s' "$LANE" | tr '-' '_')"; then
    FAILED="$FAILED $LANE"
  fi
done

printf '\n'
if [ "$DRY_RUN" -eq 1 ]; then
  printf 'Dry run: planned lanes:%s\n' "$SELECTED"
  exit 0
fi
if [ -n "$FAILED" ]; then
  printf 'Lanes that failed or are incomplete:%s\n' "$FAILED"
  printf 'Logs: %s\n' "$RAW_DIR"
  exit 1
fi
printf 'All selected lanes completed.\n'
