#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Run the Milestone 6 WebAssembly acceptance checks.

Source env/env.sh before use. Supply a WASI module that checks three
generated targets, cancellation, and a partial result on a denied process.
A standalone module must exit with an error if any assertion fails.
Use --wasi-test for an exact test name in a prebuilt Rust test module.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys

from oracle_prop_test import run_command, write_json


ROOT = Path(__file__).resolve().parents[2]
BARE = "wasm32-unknown-unknown"
WASI = "wasm32-wasip1"
TARGETS = (BARE, WASI)
FORBIDDEN = {"rcmake-store", "zstd", "rcmake-archive", "liblzma", "tz-system", "tzdb-zoneinfo"}
NATIVE_PACKAGES = FORBIDDEN | {"zstd-safe", "zstd-sys", "liblzma-sys"}
NODE = re.compile(r'^\W*([\w-]+) (?:v\d|feature "([^"]+)")')


def check_tree(output: str) -> str:
    """Check package and feature names in Cargo's normal dependency tree."""
    nodes = [match.groups() for line in output.splitlines() if (match := NODE.match(line))]
    if ("rcmake-api", None) not in nodes:
        raise ValueError("Cargo did not report the rcmake-api package.")
    blocked = {name for name, _ in nodes if name in NATIVE_PACKAGES}
    blocked.update(feature for _, feature in nodes if feature in FORBIDDEN)
    if blocked:
        raise ValueError(f"Forbidden dependencies or features. {', '.join(sorted(blocked))}.")
    return "No forbidden packages or features."


def check_targets(output: str) -> str:
    """Require both Rust WebAssembly targets."""
    missing = set(TARGETS) - set(output.splitlines())
    if missing:
        raise ValueError(f"Rust target missing. {', '.join(sorted(missing))}.")
    return "Both Rust WebAssembly targets are installed."


def run_gate(report: dict, args, env: dict[str, str], name: str, command: list[str],
             check, prerequisite: str | None = None) -> bool:
    """Run one command and record its result without losing other results."""
    row = {"check": name, "command": command, "status": "planned"}
    report["checks"].append(row)
    if args.dry_run:
        return True
    if prerequisite:
        row.update(status="skipped", detail=prerequisite)
        return False
    output = ""
    try:
        result = run_command(command, ROOT, args.timeout, env)
        output = result["stdout"] + result["stderr"]
        if result["error"] or result["returncode"] != 0:
            raise ValueError(result["error"] or f"Exit status {result['returncode']}.")
        row.update(status="pass", detail=check(result["stdout"]))
        return True
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        row.update(status="fail", detail=str(error))
        if output:
            row["output"] = output[-4000:]
        return False


def find_module() -> Path | None:
    """Find the standalone smoke module in a standard Cargo target directory."""
    target_dir = ROOT / os.environ.get("CARGO_TARGET_DIR", "target")
    for profile in ("release", "debug"):
        module = target_dir / WASI / profile / "purehost_smoke.wasm"
        if module.is_file():
            return module
    return None


def check_wasi(args, report: dict, env: dict[str, str]) -> None:
    """Run the supplied module or record why the WASI smoke is incomplete."""
    module = args.wasi_bin.expanduser().absolute() if args.wasi_bin else find_module()
    command = [args.wasmtime, "run", str(module or "<purehost_smoke.wasm>")]
    if args.wasi_test:
        command.extend((args.wasi_test, "--exact"))

    if not args.dry_run:
        if module is None:
            report["checks"].append({
                "check": "WASI PureHost smoke", "command": command,
                "status": "fail" if args.require_wasi else "skipped",
                "detail": "No PureHost smoke module found. Use --wasi-bin to select a prebuilt module.",
            })
            return
        try:
            with module.open("rb") as stream:
                if stream.read(8) != b"\0asm\x01\0\0\0":
                    raise ValueError("Expected a WebAssembly module.")
        except (OSError, ValueError) as error:
            report["checks"].append({
                "check": "WASI PureHost smoke", "command": command,
                "status": "fail", "detail": str(error),
            })
            return

    def check_output(output: str) -> str:
        if args.wasi_test and not re.search(r"(?m)^test result: ok\. 1 passed; 0 failed;", output):
            raise ValueError("The selected PureHost test did not run and pass.")
        return "The PureHost smoke completed successfully under Wasmtime."

    run_gate(report, args, env, "WASI PureHost smoke", command, check_output)


def markdown(report: dict) -> str:
    """Render the gate status and any command failure output."""
    lines = ["# Milestone 6 WebAssembly checks", "", f"Result **{report['status']}**.", "",
             "| Check | Status | Detail |", "| --- | --- | --- |"]
    for row in report["checks"]:
        detail = row.get("detail", "").replace("|", r"\|")
        lines.append(f"| {row['check']} | {row['status']} | {detail} |")
    for row in report["checks"]:
        if row["status"] == "planned" or "output" in row:
            lines.extend(("", f"## {row['check']}", "", "~~~~text", shlex.join(row["command"])))
            if "output" in row:
                lines.append(row["output"].rstrip())
            lines.append("~~~~")
    return "\n".join(lines) + "\n"


def main() -> int:
    """Print JSON on standard output and Markdown on standard error."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--wasi-bin", type=Path, help="Prebuilt wasm32-wasip1 PureHost smoke module.")
    parser.add_argument("--wasi-test", help="Exact libtest name within --wasi-bin.")
    parser.add_argument("--wasmtime", default="wasmtime", help="Path or command name for Wasmtime.")
    parser.add_argument("--require-wasi", action="store_true", help="Fail if no WASI module is available.")
    parser.add_argument("--timeout", type=int, default=300, help="Seconds per command.")
    parser.add_argument("--json", type=Path, metavar="OUT", help="Also save the JSON report.")
    parser.add_argument("--markdown", type=Path, metavar="OUT", help="Also save the Markdown report.")
    parser.add_argument("--dry-run", action="store_true", help="Print the plan without executing tools.")
    args = parser.parse_args()
    if args.timeout < 1 or (args.wasi_test and not args.wasi_bin):
        parser.error("Use a positive timeout. Set --wasi-bin when using --wasi-test.")
    if args.json and args.markdown and args.json.resolve() == args.markdown.resolve():
        parser.error("Use different report paths.")

    env = dict(os.environ, LC_ALL="C", CARGO_TERM_COLOR="never")
    env.setdefault("CARGO_BUILD_JOBS", "2")
    report = {"status": "planned", "checks": []}
    installed = run_gate(report, args, env, "Installed targets",
                         ["rustup", "target", "list", "--installed"], check_targets)
    prerequisite = None if installed else "The Rust target check failed."
    for target in TARGETS:
        selection = ["-p", "rcmake-api", "--no-default-features", "--target", target, "--locked"]
        run_gate(report, args, env, f"Dependency tree for {target}",
                 ["cargo", "tree", *selection, "-e", "normal,features"], check_tree, prerequisite)
    run_gate(report, args, env, f"Compile check for {BARE}",
             ["cargo", "check", "-p", "rcmake-api", "--no-default-features", "--target", BARE, "--locked"],
             lambda _: "Compilation succeeded.", prerequisite)
    check_wasi(args, report, env)

    statuses = {row["status"] for row in report["checks"]}
    if not args.dry_run:
        report["status"] = "fail" if "fail" in statuses else "incomplete" if "skipped" in statuses else "pass"
    formatted = markdown(report)
    if args.json:
        write_json(args.json, report)
    if args.markdown:
        args.markdown.parent.mkdir(parents=True, exist_ok=True)
        args.markdown.write_text(formatted, encoding="utf-8")
    print(json.dumps(report, indent=2))
    print(formatted, end="", file=sys.stderr)
    return int(report["status"] == "fail")


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except OSError as exc:
        sys.exit(str(exc))
