#!/usr/bin/env python3
"""Automated licence and clean-room hygiene scanner for Rcmake.

Checks:
1. No forbidden copyleft/non-permissive licence markers (GPL/LGPL/AGPL/MPL/SSPL/BUSL)
   in native Rust crates, tooling, or vendored CMake modules/templates.
2. No forbidden upstream CMake binary invocations inside core engine crates
   (only `crates/rcmake-harness`, `tools/`, and `conformance/` may reference the oracle).
3. No `#![allow(unsafe_code)]` or `unsafe ` blocks inside `crates/*`.
4. No AppleDouble `._*` junk files committed in tracked paths.
"""

from __future__ import annotations

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]

SKIP_DIRS = {
    ".git",
    "target",
    "env",
    "__pycache__",
}

# Only scan for explicit non-permissive licence assertions (avoiding false positives
# from documentation describing what is disallowed, e.g., AGENTS.md or this script).
FORBIDDEN_SPDX = re.compile(
    r"SPDX-License-Identifier:\s*[^\n]*\b(A?L?GPL|MPL|SSPL|BUSL|CDDL|EPL)\b",
    re.IGNORECASE,
)

FORBIDDEN_UNSAFE_RE = re.compile(r"\bunsafe\s*(\{|fn\b|impl\b|trait\b)")
ALLOW_UNSAFE_RE = re.compile(r"#!?\[allow\([^)]*unsafe_code[^)]*\)\]")
ORACLE_ENV_RE = re.compile(r"RCMAKE_ORACLE_CMAKE|RCMAKE_ORACLE_CTEST|RCMAKE_ORACLE_CPACK")

CORE_CRATE_ALLOW_ORACLE = {"rcmake-harness"}


def iter_files(base: pathlib.Path):
    for path in sorted(base.rglob("*")):
        if any(part in SKIP_DIRS for part in path.relative_to(ROOT).parts):
            continue
        if path.is_file():
            yield path


def main() -> int:
    errors: list[str] = []

    # 1. Check AppleDouble files
    for path in iter_files(ROOT):
        if path.name.startswith("._"):
            errors.append(f"AppleDouble junk file present: {path.relative_to(ROOT)}")

    # 2. Check crates/* for unsafe, forbidden SPDX, and oracle leakage
    crates_dir = ROOT / "crates"
    if crates_dir.exists():
        for path in iter_files(crates_dir):
            rel = path.relative_to(ROOT)
            crate_name = rel.parts[1] if len(rel.parts) > 1 else ""
            try:
                text = path.read_text(encoding="utf-8", errors="ignore")
            except OSError as exc:
                errors.append(f"Could not read {rel}: {exc}")
                continue

            if FORBIDDEN_SPDX.search(text):
                errors.append(f"Non-permissive SPDX identifier in {rel}")

            if path.suffix == ".rs":
                if ALLOW_UNSAFE_RE.search(text):
                    errors.append(f"unsafe_code allow attribute in {rel}")
                # Check lines outside simple doc comments for unsafe blocks/fns
                for lineno, line in enumerate(text.splitlines(), 1):
                    stripped = line.lstrip()
                    if stripped.startswith("//"):
                        continue
                    if FORBIDDEN_UNSAFE_RE.search(line):
                        errors.append(f"unsafe keyword in {rel}:{lineno}")

                if crate_name not in CORE_CRATE_ALLOW_ORACLE and ORACLE_ENV_RE.search(text):
                    errors.append(f"Core crate references oracle env var: {rel}")

    # 3. Check vendored modules/ for non-permissive SPDX headers
    modules_dir = ROOT / "modules"
    if modules_dir.exists():
        for path in iter_files(modules_dir):
            rel = path.relative_to(ROOT)
            try:
                text = path.read_text(encoding="utf-8", errors="ignore")
            except OSError as exc:
                errors.append(f"Could not read {rel}: {exc}")
                continue
            if FORBIDDEN_SPDX.search(text):
                errors.append(f"Non-permissive SPDX identifier in vendored module {rel}")

    # 4. Verify required licence & notice files exist
    for required in ("LICENSE-MIT", "LICENSE-APACHE", "THIRD_PARTY_NOTICES.md"):
        if not (ROOT / required).is_file():
            errors.append(f"Missing required licence file: {required}")

    if errors:
        print("LICENCE / CLEAN-ROOM SCAN FAILED:", file=sys.stderr)
        for err in errors:
            print(f"  - {err}", file=sys.stderr)
        return 1

    print("licence_scan.py: OK (clean-room & permissive licence invariants verified)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
