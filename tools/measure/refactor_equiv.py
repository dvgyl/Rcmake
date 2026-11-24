#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Compare generated build trees between a baseline and candidate Rcmake binary."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import sys
import tempfile

sys.path.insert(0, str(Path(__file__).resolve().parent))
import determinism  # noqa: E402

REPLY_HASH_RE = re.compile(rb"-[0-9a-f]{20}\.json")
REPLY_KEY_HASH_RE = re.compile(r"-[0-9a-f]{20}\.json$")
INDEX_KEY_RE = re.compile(r"^\.cmake/api/v1/reply/index-.*\.json$")
INDEX_REF_RE = re.compile(rb"index-[0-9T:\-\.]+\.json")

FILE_API_QUERY = {
    "requests": [
        {"kind": "codemodel", "version": 2},
        {"kind": "cache", "version": 2},
        {"kind": "cmakeFiles", "version": 1},
        {"kind": "toolchains", "version": 1},
    ]
}


def write_file_api_query(build: Path) -> None:
    query = build / ".cmake/api/v1/query/client-refactor-equiv"
    query.mkdir(parents=True, exist_ok=True)
    (query / "query.json").write_text(json.dumps(FILE_API_QUERY, indent=2) + "\n", encoding="utf-8")


def normalize_snapshot(snap: determinism.Snapshot, binary: Path) -> determinism.Snapshot:
    bin_bytes = os.fsencode(binary.resolve())
    raw_bin = os.fsencode(binary)
    out: determinism.Snapshot = {}
    for name, (kind, data) in snap.items():
        if INDEX_KEY_RE.match(name):
            key = ".cmake/api/v1/reply/index-NORMALIZED.json"
        elif name.startswith(".cmake/api/v1/reply/"):
            key = REPLY_KEY_HASH_RE.sub("-<HASH>.json", name)
        else:
            key = name
        cleaned = data.replace(bin_bytes, b"<RCMAKE>").replace(raw_bin, b"<RCMAKE>")
        if name.startswith(".cmake/api/v1/reply/"):
            cleaned = REPLY_HASH_RE.sub(b"-<HASH>.json", cleaned)
            cleaned = INDEX_REF_RE.sub(b"index-NORMALIZED.json", cleaned)
        out[key] = (kind, cleaned)
    return out


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True, help="Baseline Rcmake binary.")
    parser.add_argument("--candidate", type=Path, required=True, help="Candidate Rcmake binary.")
    parser.add_argument(
        "--generator",
        action="append",
        help="Limit comparison to specific generator(s); defaults to all available generators.",
    )
    args = parser.parse_args()
    baseline = args.baseline.absolute()
    candidate = args.candidate.absolute()

    passed = True
    checked = 0
    with tempfile.TemporaryDirectory(prefix="rcmake-refactor-equiv-") as directory:
        root = Path(directory).resolve()
        source = root / "src"
        source.mkdir()
        for name, content in determinism.PROJECT.items():
            (source / name).write_text(content, encoding="utf-8")

        generators = args.generator or list(determinism.available_generators(baseline))
        for gen in generators:
            print(f"Checking equivalence for {gen}.", flush=True)
            checked += 1
            with tempfile.TemporaryDirectory(dir=root) as case:
                b_dir = Path(case) / "baseline"
                c_dir = Path(case) / "candidate"
                for binary, build in ((baseline, b_dir), (candidate, c_dir)):
                    write_file_api_query(build)
                    determinism.run(
                        binary,
                        "-S",
                        source,
                        "-B",
                        build,
                        "-G",
                        gen,
                        "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON",
                    )
                snap_b = normalize_snapshot(determinism.snapshot(b_dir), baseline)
                snap_c = normalize_snapshot(determinism.snapshot(c_dir), candidate)
                if not determinism.compare(snap_b, snap_c):
                    passed = False

    if not checked:
        print("No generators were checked.", file=sys.stderr)
        return 1
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
