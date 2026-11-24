#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Compare a project's configure, build, test, install and package steps.

Source env/env.sh before use. The tools run by name through PATH. Each run
starts with a fresh copy of the project. The JSON report includes tool paths,
command output, test results, installed paths and archive entries.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import xml.etree.ElementTree as ET
import zipfile

from corpus_eval import (
    JOBS, PACKAGE_EXTENSIONS, install_manifest, install_tree, package_tree, row_key,
)
from oracle_prop_test import (
    DEFAULT_WORK_DIR, ORACLE_VERSION, clean_environment, executable, run_command, write_json,
)

TOOLS = ("cmake", "ctest", "cpack")


def support_path(root: Path, inherited_path: str) -> list[Path]:
    """Keep support tools on PATH and remove copies of the three CMake tools."""
    paths = []
    for index, entry in enumerate(inherited_path.split(os.pathsep)):
        directory = Path(entry).absolute()
        if not any(os.access(directory / name, os.X_OK) for name in TOOLS):
            paths.append(directory)
            continue
        filtered = root / f"support-{index}"
        filtered.mkdir()
        for tool in directory.iterdir():
            if tool.name not in TOOLS and tool.is_file() and os.access(tool, os.X_OK):
                (filtered / tool.name).symlink_to(tool)
        paths.append(filtered)
    return paths


def verify_path(bin_dir: Path, root: Path, timeout: int, env: dict[str, str]) -> dict:
    """Check that each name resolves only to the selected bin directory."""
    evidence = {}
    for name in TOOLS:
        lookup = run_command(["which", "-a", name], root, timeout, env)
        expected = str(bin_dir / name)
        if not succeeded(lookup) or lookup["stdout"].splitlines() != [expected]:
            raise ValueError(f"Expected only {expected} on PATH. which returned {lookup!r}.")
        version = run_command([name, "--version"], root, timeout, env)
        if (not succeeded(version)
                or version["stdout"].splitlines()[:1] != [f"{name} version {ORACLE_VERSION}"]):
            raise ValueError(f"Expected {name} {ORACLE_VERSION} at {expected}. Got {version!r}.")
        evidence[name] = {"which": lookup["stdout"].strip(),
                          "version": version["stdout"].splitlines()[0]}
    return evidence


def succeeded(result: dict) -> bool:
    """Check whether a command completed and returned zero."""
    return result["returncode"] == 0 and result["error"] is None


def test_results(report: Path) -> list[dict]:
    """Read test names and outcomes from the CTest JUnit report."""
    results = []
    for test in ET.parse(report).iter("testcase"):
        status = next((tag for tag in ("error", "failure", "skipped")
                       if test.find(tag) is not None), "pass")
        results.append({"name": test.attrib["name"], "status": status})
    if not results:
        raise ValueError("CTest did not report any tests.")
    return sorted(results, key=row_key)


def collect_artifacts(stages: dict, root: Path, args: argparse.Namespace) -> dict:
    """Read test outcomes, installed paths and package entries."""
    artifacts = {}
    if succeeded(stages["test"]) or (root / "ctest.xml").is_file():
        artifacts["tests"] = test_results(root / "ctest.xml")
    if succeeded(stages["install"]):
        prefix = root / "install"
        artifacts["install_tree"] = install_tree(prefix)
        artifacts["install_manifest"] = install_manifest(root / "build", prefix)
    if succeeded(stages["package"]):
        extension = PACKAGE_EXTENSIONS[args.cpack_generator]
        archives = sorted((root / "packages").glob(f"*{extension}"))
        if not archives:
            raise ValueError(f"CPack did not produce a {args.cpack_generator} archive.")
        rows = [row for archive in archives for row in
                package_tree(archive, args.cpack_generator, {root: "<work>"})]
        artifacts["package_tree"] = sorted(rows, key=row_key)
    return artifacts


def evaluate(name: str, bin_dir: Path, root: Path, source: Path,
             env: dict[str, str], args: argparse.Namespace) -> dict:
    """Run the project through the selected PATH."""
    result = {"tools": verify_path(bin_dir, root, args.timeout, env), "stages": {}}
    shutil.copytree(source, root / "source", symlinks=True)
    build_type = f"-DCMAKE_BUILD_TYPE={args.config}"
    commands = {
        "configure": ["cmake", "-S", "source", "-B", "build", "-G", args.generator,
                      build_type, *args.configure_arg],
        "build": ["cmake", "--build", "build", "--config", args.config, "--parallel", JOBS],
        "test": ["ctest", "--test-dir", "build", "-C", args.config, "--output-on-failure",
                 "--no-tests=error", "--parallel", JOBS, "--output-junit", str(root / "ctest.xml")],
        "install": ["cmake", "--install", "build", "--config", args.config,
                    "--prefix", str(root / "install")],
        "package": ["cpack", "--config", "build/CPackConfig.cmake", "-C", args.config,
                    "-G", args.cpack_generator, "-B", str(root / "packages")],
    }
    failed = False
    for stage, command in commands.items():
        if failed:
            outcome = {"returncode": None, "error": None, "skipped": True}
        else:
            print(f"[{name}] {stage}", flush=True)
            outcome = run_command(command, root, args.timeout, env)
            failed = not succeeded(outcome)
        result["stages"][stage] = dict(outcome, command=command)
    try:
        result["artifacts"] = collect_artifacts(result["stages"], root, args)
    except (KeyError, OSError, ValueError, ET.ParseError, tarfile.TarError, zipfile.BadZipFile) as error:
        result["artifact_error"] = str(error)
    return result


def compare(oracle: dict, rcmake: dict) -> list[str]:
    """List stage results and artifacts that differ."""
    differences = []
    for stage, expected in oracle["stages"].items():
        actual = rcmake["stages"][stage]
        if any(expected.get(key) != actual.get(key) for key in ("returncode", "error", "skipped")):
            differences.append(stage)
    left, right = (engine.get("artifacts", {}) for engine in (oracle, rcmake))
    differences.extend(key for key in sorted(left.keys() | right.keys()) if left.get(key) != right.get(key))
    if oracle.get("artifact_error") != rcmake.get("artifact_error"):
        differences.append("artifact_error")
    return differences


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", "--project", required=True, type=Path,
                        help="Project directory with tests, install rules and CPack configuration.")
    parser.add_argument("--rcmake-bin", required=True, type=Path, help="Path to staged Rcmake.")
    parser.add_argument("--out", required=True, type=Path, help="Path to the JSON report.")
    parser.add_argument("--work-dir", type=Path, default=DEFAULT_WORK_DIR, help="Test workspace.")
    parser.add_argument("--generator", default="Ninja", help="CMake build generator.")
    parser.add_argument("--config", default="Release", help="Build configuration.")
    parser.add_argument("--configure-arg", action="append", default=[],
                        help="Extra CMake configure argument. Repeat to pass several.")
    parser.add_argument("--cpack-generator", choices=PACKAGE_EXTENSIONS, default="TGZ")
    parser.add_argument("--timeout", type=int, default=600, help="Timeout in seconds per command.")
    args = parser.parse_args()
    if args.timeout < 1:
        parser.error("--timeout must be positive.")

    try:
        binary = executable(args.rcmake_bin)
        oracle_bin = Path(os.environ["RCMAKE_ORACLE_BIN_DIR"]).resolve()
        for name in TOOLS:
            executable(oracle_bin / name)
        source = args.source.resolve()
        if not (source / "CMakeLists.txt").is_file():
            raise ValueError(f"No CMakeLists.txt in {source}.")
        work_dir = args.work_dir.resolve()
        if work_dir.is_relative_to(source):
            raise ValueError("The test workspace must be outside the project directory.")
        work_dir.mkdir(parents=True, exist_ok=True)
        env = clean_environment()
        for key in ("DESTDIR", "CMAKE_INSTALL_MODE"):
            env.pop(key, None)
        with tempfile.TemporaryDirectory(prefix="path-swap-", dir=work_dir) as directory:
            root = Path(directory)
            shim = root / "bin"
            shim.mkdir()
            for name in TOOLS:
                (shim / name).symlink_to(binary)
            support = support_path(root, env["PATH"])
            results = {}
            for name, bin_dir in (("oracle", oracle_bin), ("rcmake", shim)):
                project = root / "project"
                project.mkdir()
                path = os.pathsep.join(map(str, (bin_dir, *support)))
                results[name] = evaluate(name, bin_dir, project, source, dict(env, PATH=path), args)
                shutil.rmtree(project)
        oracle, rcmake = results["oracle"], results["rcmake"]
        differences = compare(oracle, rcmake)
        oracle_ok = not oracle.get("artifact_error") and all(map(succeeded, oracle["stages"].values()))
        rcmake_ok = not rcmake.get("artifact_error") and all(map(succeeded, rcmake["stages"].values()))
        status = "oracle_error" if not oracle_ok else "fail" if differences or not rcmake_ok else "pass"
        write_json(args.out, {"source": str(source), "status": status,
                              "differences": differences, **results})
    except (KeyError, OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"Could not complete the PATH comparison. {error}", file=sys.stderr)
        return 2
    print(json.dumps({"status": status, "differences": differences}))
    return {"pass": 0, "fail": 1, "oracle_error": 2}[status]


if __name__ == "__main__":
    raise SystemExit(main())
