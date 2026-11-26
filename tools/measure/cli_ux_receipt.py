#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Merge the cli-ux verify.py exit code and the verify_polish.py report into cli-ux.json.

scripts/reproduce-scorecard.sh and the conformance job in ecosystem.yml both use
this script, so the local receipt and the CI receipt have the same schema.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import sys
import tempfile
from typing import Any


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def build_receipt(out: Path, verify_exit: int, verify_log: Path, polish_report: Path) -> dict[str, Any]:
    """Return the cli-ux.json payload. Paths in the payload are relative to the directory of out."""
    try:
        polish = json.loads(polish_report.read_text(encoding="utf-8"))
        polish_checks = polish["checks"]
    except (OSError, ValueError, KeyError, TypeError) as error:
        raise ValueError(f"{polish_report}: cannot read the verify_polish.py report ({error})") from None
    base = out.parent
    checks = [{"check": "verify.py", "status": "pass" if verify_exit == 0 else "fail",
               "log": os.path.relpath(verify_log, base)}]
    checks += [{"check": f"verify_polish.py:{row['id']}", "status": "pass" if row["passed"] else "fail"}
               for row in polish_checks]
    passed = sum(row["status"] == "pass" for row in checks)
    return {
        "status": "pass" if passed == len(checks) else "fail",
        "passed": passed,
        "total": len(checks),
        "failures": len(checks) - passed,
        "checks": checks,
        "polish_report": os.path.relpath(polish_report, base),
    }


def write_json(path: Path, payload: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile("w", encoding="utf-8", dir=path.parent, delete=False) as stream:
        stream.write(json.dumps(payload, indent=2) + "\n")
    os.chmod(stream.name, 0o644)
    os.replace(stream.name, path)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--out", type=Path, required=True, help="Path of cli-ux.json.")
    parser.add_argument("--verify-exit", type=int, required=True, help="Exit code of verify.py.")
    parser.add_argument("--verify-log", type=Path, required=True, help="Log file of verify.py.")
    parser.add_argument("--polish-report", type=Path, required=True, help="JSON report of verify_polish.py.")
    parser.add_argument("--commit", default=None, help="Record this commit as release_commit.")
    parser.add_argument("--binary", type=Path, default=None, help="Record the SHA-256 of this binary.")
    args = parser.parse_args(argv)
    try:
        payload = build_receipt(args.out, args.verify_exit, args.verify_log, args.polish_report)
        if args.commit:
            payload["release_commit"] = args.commit.strip().lower()
        if args.binary is not None:
            payload["binary_sha256"] = sha256_file(args.binary)
        write_json(args.out, payload)
    except (OSError, ValueError) as error:
        sys.stderr.write(f"cli_ux_receipt: {error}\n")
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
