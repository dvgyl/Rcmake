#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Run the Milestone 6 WebAssembly acceptance checks.

Source env/env.sh before use. The rcmake-api wasm_smoke example checks three
generated targets, provenance, cancellation, try_compile, and a partial result
on a denied process.
Use --wasi-bin to run a prebuilt copy of that example.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import shlex
import shutil
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


def check_smoke(output: str) -> str:
    """Require the success report that the pinned example emits after verification."""
    result = json.loads(output)
    if not isinstance(result, dict) or result.get("schema") != "rcmake-wasm-smoke-v1":
        raise ValueError("Expected the wasm_smoke JSON report.")
    if (result.get("ok") is not True or result.get("targets") != ["core", "util", "app"]
            or result.get("provenance") != "-DCORE_API=1"
            or type(result.get("denied_processes")) is not int or result["denied_processes"] != 1
            or result.get("cancelled") is not True
            or result.get("complete") is not True or result.get("provisional") is not True
            or result.get("try_compile") != "passed"):
        raise ValueError("The wasm_smoke report lacks evidence of all required checks.")
    # The example reports a denial only after it verifies the partial result and index.
    return "Verified three targets, provenance, the partial result, cancellation, and try_compile."


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
    """Find the pinned prebuilt example in a standard Cargo target directory."""
    target_dir = ROOT / os.environ.get("CARGO_TARGET_DIR", "target")
    for profile in ("release", "debug"):
        module = target_dir / WASI / profile / "examples" / "wasm_smoke.wasm"
        if module.is_file():
            return module
    return None


def selection(target: str) -> list[str]:
    return ["-p", "rcmake-api", "--no-default-features", "--target", target, "--locked"]


def check_wasi(args, report: dict, env: dict[str, str], prerequisite: str | None) -> None:
    """Build and run the pinned example, or use a prebuilt copy."""
    module = args.wasi_bin.expanduser().absolute() if args.wasi_bin else None
    if module is None and prerequisite and not args.dry_run:
        module = find_module()
    if module is None:
        def built_example(output: str) -> str:
            nonlocal module
            for line in output.splitlines():
                message = json.loads(line)
                if not isinstance(message, dict) or message.get("reason") != "compiler-artifact":
                    continue
                target = message.get("target")
                if (not isinstance(target, dict) or target.get("name") != "wasm_smoke"
                        or target.get("kind") != ["example"]):
                    continue
                filenames = message.get("filenames")
                if not isinstance(filenames, list):
                    continue
                for filename in filenames:
                    if not isinstance(filename, str):
                        continue
                    path = ROOT / filename
                    if (path.name == "wasm_smoke.wasm" and path.parent.name == "examples"
                            and len(path.parents) > 2 and path.parents[2].name == WASI
                            and path.is_file()):
                        module = path
                        return "Built the wasm_smoke example."
            raise ValueError("Cargo did not report the wasm_smoke.wasm example artifact.")

        run_gate(report, args, env, "Build WASI example",
                 ["cargo", "build", *selection(WASI), "--example", "wasm_smoke",
                  "--message-format=json-render-diagnostics"], built_example, prerequisite)

    name = "WASI PureHost smoke"
    command = [args.wasmtime, "run", str(module or f"target/{WASI}/debug/examples/wasm_smoke.wasm")]
    detail = None
    if not args.dry_run:
        if module is None:
            detail = "The wasm_smoke WASI example is unavailable."
        else:
            try:
                with module.open("rb") as stream:
                    if stream.read(8) != b"\0asm\x01\0\0\0":
                        raise ValueError("Expected a WebAssembly module.")
            except (OSError, ValueError) as error:
                report["checks"].append({
                    "check": name, "command": command, "status": "fail", "detail": str(error),
                })
                return
            if not shutil.which(args.wasmtime):
                detail = f"Cannot execute {args.wasmtime}. Install Wasmtime or set --wasmtime."
    run_gate(report, args, env, name, command, check_smoke, detail)
    if detail and args.require_wasi:
        report["checks"][-1]["status"] = "fail"


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
    parser.add_argument("--wasi-bin", type=Path, help="Prebuilt wasm32-wasip1 wasm_smoke example.")
    parser.add_argument("--wasmtime", default="wasmtime", help="Path or command name for Wasmtime.")
    parser.add_argument("--require-wasi", action="store_true", help="Treat a missing WASI run as a failure.")
    parser.add_argument("--allow-incomplete", action="store_true", help="Permit skipped checks for diagnostics.")
    parser.add_argument("--timeout", type=int, default=300, help="Seconds per command.")
    parser.add_argument("--json", type=Path, metavar="OUT", help="Also save the JSON report.")
    parser.add_argument("--markdown", type=Path, metavar="OUT", help="Also save the Markdown report.")
    parser.add_argument("--dry-run", action="store_true", help="Print the plan without executing tools.")
    args = parser.parse_args()
    if args.timeout < 1:
        parser.error("Use a positive timeout.")
    if args.json and args.markdown and args.json.resolve() == args.markdown.resolve():
        parser.error("Use different report paths.")

    env = dict(os.environ, LC_ALL="C", CARGO_TERM_COLOR="never")
    env.setdefault("CARGO_BUILD_JOBS", "2")
    report = {"status": "planned", "checks": []}
    installed = set()

    def record_installed(output: str) -> str:
        installed.update(output.split())
        return "Installed WebAssembly targets. " + (", ".join(sorted(installed & set(TARGETS))) or "None.")

    checked = run_gate(report, args, env, "Installed targets",
                       ["rustup", "target", "list", "--installed"], record_installed,
                       None if shutil.which("rustup") else "Cannot execute rustup.")
    prerequisites = {
        target: (None if target in installed else f"Rust target {target} is missing.")
        if checked else f"Could not verify Rust target {target}."
        for target in TARGETS
    }
    for target in TARGETS:
        run_gate(report, args, env, f"Dependency tree for {target}",
                 ["cargo", "tree", *selection(target), "-e", "normal,features"],
                 check_tree, prerequisites[target])
    run_gate(report, args, env, f"Compile check for {BARE}",
             ["cargo", "check", *selection(BARE), "--example", "wasm_smoke"],
             lambda _: "The wasm_smoke example compiled.", prerequisites[BARE])
    check_wasi(args, report, env, prerequisites[WASI])

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
    return int(report["status"] == "fail"
               or (report["status"] == "incomplete" and not args.allow_incomplete))


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except OSError as exc:
        sys.exit(str(exc))
