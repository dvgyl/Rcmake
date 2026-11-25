#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Compare generated build trees and File API replies from two Rcmake binaries."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

import determinism
from mutation_eval import API, QUERIES
from ninja_canon import Normalizer
from oracle_prop_test import executable

CONFIGURE_TIMEOUT = 600


def available_generators(binary: Path, root: Path, env: dict[str, str]) -> list[str]:
    """Return generators from the determinism suite that can run on this host."""
    stdout, _ = determinism.run([binary, "-E", "capabilities"], root, env, 30)
    advertised = {generator["name"] for generator in json.loads(stdout)["generators"]}
    requirements = {"Ninja": "ninja", "Ninja Multi-Config": "ninja", "Unix Makefiles": "make"}
    return [
        name for name in determinism.GENERATORS
        if name in advertised
        and (name not in requirements or shutil.which(requirements[name], path=env.get("PATH")))
        and (name != "Xcode" or sys.platform == "darwin")
    ]


def capture(binary: Path, source: Path, build: Path, generator: str,
            definitions: list[str], env: dict[str, str]) -> dict[str, bytes]:
    """Configure a clean tree and read its generated files and File API replies."""
    query = build / API / "query"
    query.mkdir(parents=True)
    for name in QUERIES:
        (query / name).touch()

    determinism.run([binary, "-S", source, "-B", build, "-G", generator,
                     "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON",
                     *(f"-D{definition}" for definition in definitions)],
                    build.parent, env, CONFIGURE_TIMEOUT)

    suffix = ".exe" if os.name == "nt" else ""
    paths = determinism.ninja_paths(Normalizer(build, replacements={
        binary.with_name(f"cmake{suffix}"): "<RCMAKE>",
        binary.with_name(f"ctest{suffix}"): "<CTEST>",
        binary.with_name(f"cpack{suffix}"): "<CPACK>",
        binary.parent.parent / "share/cmake-4.5": "<CMAKE_DATA>",
        binary: "<RCMAKE>",
    }))
    reply_prefix = (API / "reply").as_posix() + "/"
    generated = {
        name: determinism.normalized(data, paths)
        for name, data in determinism.generator_snapshot(build).items()
        if not name.replace(os.sep, "/").startswith(reply_prefix)
    }
    if "CMakeCache.txt" not in generated:
        raise ValueError(f"No CMake cache in {build}.")
    return {**generated, **determinism.file_api(build, paths)}


def main(argv: list[str] | None = None) -> int:
    """Compare the synthetic fixture and each requested project."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True, help="Baseline Rcmake binary.")
    parser.add_argument("--candidate", type=Path, required=True, help="Candidate Rcmake binary.")
    parser.add_argument("--source", action="append", type=Path, default=[],
                        help="Project directory to copy and check. Repeat for more projects.")
    parser.add_argument("--define", action="append", default=[], metavar="KEY=VAL",
                        help="Set a CMake cache entry for each configure. Repeat for more entries.")
    parser.add_argument("--generator", action="append",
                        help="Generator to check. Repeat to select more generators.")
    args = parser.parse_args(argv)

    sources = [source.resolve() for source in args.source]
    for source in sources:
        if not (source / "CMakeLists.txt").is_file():
            parser.error(f"No CMakeLists.txt in {source}.")
    for definition in args.define:
        key, separator, _ = definition.partition("=")
        if not key or not separator:
            parser.error(f"Expected KEY=VAL for --define. Got {definition!r}.")

    env = {**os.environ, "RCMAKE_NO_INCREMENTAL": "1", "RCMAKE_NO_PROBE_CACHE": "1"}
    try:
        baseline, candidate = map(executable, (args.baseline, args.candidate))
        with tempfile.TemporaryDirectory(prefix="rcmake-refactor-equiv-") as directory:
            root = Path(directory).resolve()
            generators = args.generator or available_generators(baseline, root, env)
            if not generators:
                raise ValueError("No generators were available.")

            fixture = root / "src"
            fixture.mkdir()
            for name, content in determinism.GENERATOR_FIXTURE.items():
                (fixture / name).write_text(content, encoding="utf-8")

            failures = 0
            for source in (fixture, *sources):
                for generator in generators:
                    label = f"{'synthetic fixture' if source == fixture else source} ({generator})"
                    print(f"Checking {label}.", flush=True)
                    try:
                        with tempfile.TemporaryDirectory(dir=root) as case:
                            # Reuse both paths and restore inputs that configure can change.
                            checkout = Path(case) / "source" / source.name
                            build = Path(case) / "build"
                            shutil.copytree(source, checkout, symlinks=True)
                            expected = capture(baseline, checkout, build, generator, args.define, env)
                            shutil.rmtree(checkout)
                            shutil.rmtree(build)
                            shutil.copytree(source, checkout, symlinks=True)
                            actual = capture(candidate, checkout, build, generator, args.define, env)
                            determinism.compare(expected, actual, label)
                    except (OSError, ValueError, subprocess.SubprocessError) as error:
                        print(f"FAIL {label}. {error}", file=sys.stderr)
                        failures += 1
            return int(bool(failures))
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"Equivalence check failed. {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
