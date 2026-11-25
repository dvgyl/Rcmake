#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Run project commands with only the selected CMake tools on PATH."""

from __future__ import annotations

import argparse
from collections import Counter
from dataclasses import dataclass
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

from normalize import compare

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "measure"))
from ninja_canon import Normalizer, normalized_commands
from oracle_prop_test import clean_environment, executable, run_command, write_json
from path_swap_eval import TOOLS, succeeded, support_path, verify_path


@dataclass(frozen=True)
class Stage:
    """Store a shell command or argument list and the tools it must invoke."""

    name: str
    command: str | list[str]
    expect: tuple[str, ...] = ("cmake",)
    ninja_dry_run: bool = False

    def argv(self) -> list[str]:
        """Pass a shell command to sh without editing its text."""
        return ["sh", "-c", self.command] if isinstance(self.command, str) else self.command


def stage_dir(path: Path) -> Path:
    """Find the staged Rcmake executable."""
    if path.is_dir():
        path = path / "bin" / "rcmake"
    return executable(path).resolve()


def prepare_paths(root: Path, binary: Path) -> tuple[Path, dict[str, str]]:
    """Create the Rcmake tool links and filter the inherited PATH."""
    shim = root / "shim" / "bin"
    shim.mkdir(parents=True)
    for name in TOOLS:
        (shim / name).symlink_to(binary)
    env = clean_environment()
    env["NINJA_STATUS"] = "[%f/%t] "
    for key in (
        "BASH_ENV", "ENV", "DESTDIR", "CMAKE_INSTALL_MODE", "CMAKE_EXECUTABLE",
        "CMAKE_COMMAND", "CMAKE_CTEST_COMMAND", "CMAKE_CPACK_COMMAND",
        "VCPKG_FORCE_DOWNLOADED_BINARIES",
    ):
        env.pop(key, None)
    directories = dict.fromkeys(
        str(Path(entry).resolve()) for entry in env["PATH"].split(os.pathsep)
        if Path(entry).is_absolute() and Path(entry).is_dir()
    )
    support = support_path(root, os.pathsep.join(directories)) if directories else []
    env["PATH"] = os.pathsep.join(map(str, support))
    return shim, env


def check_exec_log(log: Path, offset: int, expected: tuple[str, ...]) -> dict:
    """Check the new log records for each expected CMake tool."""
    records = []
    if log.exists():
        with log.open("rb") as stream:
            stream.seek(offset)
            for line in stream:
                try:
                    record = json.loads(line)
                    if (not isinstance(record, dict) or record.get("tool") not in TOOLS
                            or record.get("persona") not in {"compat", "native"}
                            or not isinstance(record.get("argv"), list)):
                        raise ValueError("Expected an argv list, a CMake tool, and a persona.")
                    if "--version" not in record["argv"]:
                        records.append(record)
                except (ValueError, RecursionError) as error:
                    return {"status": "invalid_exec_log", "error": str(error)}
    seen = {record["tool"] for record in records}
    missing = sorted(set(expected) - seen)
    return {
        "status": "missing_exec_log" if missing else "pass",
        "count": len(records), "missing_personas": missing,
    }


def evaluate(bin_dir: Path, source: Path, project: Path, stages: list[Stage],
             env: dict[str, str], timeout: int, log: Path | None) -> dict:
    """Run the stages and record evidence from the selected tools."""
    env = dict(env, PATH=os.pathsep.join(filter(None, (str(bin_dir), env["PATH"]))))
    if "RCMAKE_ORACLE_CMAKE" in env:
        raise ValueError("Remove RCMAKE_ORACLE_CMAKE from the command environment.")
    if log is not None:
        log.write_bytes(b"")
        env["RCMAKE_EXEC_LOG"] = str(log)
    shutil.copytree(source, project, symlinks=True, ignore=shutil.ignore_patterns(".git", "._*"))
    result = {"stages": [], "oracle_env_unset": True}
    if log is not None:
        result["exec_log"] = str(log)
    try:
        result["tools"] = verify_path(bin_dir, project, timeout, env)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        result["path_error"] = str(error)

    blocked = "path_error" in result
    for stage in stages:
        row = {"name": stage.name, "command": stage.command, "expected_personas": stage.expect}
        result["stages"].append(row)
        if blocked:
            row["status"] = "skipped_prior_failure"
            continue
        print(f"[{'rcmake' if log else 'oracle'}] {stage.name}", flush=True)
        offset = log.stat().st_size if log is not None and log.exists() else 0
        try:
            row.update(run_command(stage.argv(), project, timeout, env))
        except (OSError, subprocess.SubprocessError) as error:
            row.update(returncode=None, error=str(error))
        blocked = not succeeded(row)
        row["status"] = "failed" if blocked else "pass"
        if log is not None:
            proof = check_exec_log(log, offset, stage.expect)
            row["exec_proof"] = proof
            if not blocked:
                row["status"] = proof["status"]
    return result


def tool_paths(directory: Path) -> dict[str, set[str]]:
    """Return selected tool names and their resolved executable paths."""
    return {
        name: {str(directory / name), str((directory / name).resolve())} for name in TOOLS
    }


def dry_run_commands(stage: dict, project: Path, bin_dir: Path) -> Counter[str]:
    """Count commands from verbose Ninja dry-run output."""
    normalizer = Normalizer(project, replacements={
        bin_dir / name: f"<{name}>" for name in TOOLS
    })
    return Counter(normalized_commands(stage["stdout"], normalizer))


def compare_runs(source: Path, stages: list[Stage], binary: Path, oracle: Path,
                 artifacts: Path, timeout: int, *,
                 configured_outputs: tuple[str, ...] = (),
                 oracle_env: dict[str, str] | None = None,
                 rcmake_env: dict[str, str] | None = None) -> dict:
    """Run both tools at the same absolute project path and compare their outputs."""
    for name in TOOLS:
        executable(oracle.parent / name)
    artifacts.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="rcmake-pathswap-") as directory:
        work = Path(directory).resolve()
        shim, env = prepare_paths(work, binary)
        project = work / "project"
        before = evaluate(oracle.parent, source, project, stages, env | (oracle_env or {}),
                          timeout, None)
        snapshot = work / "oracle-project"
        project.rename(snapshot)
        after = evaluate(shim, source, project, stages, env | (rcmake_env or {}), timeout,
                         artifacts / "rcmake-exec.jsonl")
        files = compare(snapshot, project, artifacts / "diffs",
                        configured_outputs=configured_outputs,
                        tools=(tool_paths(oracle.parent), tool_paths(shim)))
        dry_runs = {}
        for stage, first, second in zip(stages, before["stages"], after["stages"]):
            if stage.ninja_dry_run and all(row.get("returncode") == 0 for row in (first, second)):
                left = dry_run_commands(first, project, oracle.parent)
                right = dry_run_commands(second, project, shim)
                dry_runs[stage.name] = {
                    "oracle_only": sorted((left - right).elements()),
                    "rcmake_only": sorted((right - left).elements()),
                }
    oracles_ok = "path_error" not in before and all(
        stage["status"] == "pass" for stage in before["stages"]
    )
    rcmake_ok = "path_error" not in after and all(
        stage["status"] == "pass" for stage in after["stages"]
    )
    different = (any(counts["different"] for counts in files["classes"].values())
                 or any(any(row.values()) for row in dry_runs.values()))
    return {
        "source": str(source), "oracle_executable": str(oracle), "rcmake_executable": str(binary),
        "status": "oracle_error" if not oracles_ok else "pass" if rcmake_ok and not different else "fail",
        "oracle": before, "rcmake": after, "files": files, "ninja_dry_runs": dry_runs,
    }


def load_stages(path: Path) -> list[Stage]:
    """Read the ordered command list from a JSON manifest."""
    rows = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(rows, list) or not rows:
        raise ValueError("The command manifest must be a non-empty JSON array.")
    stages = []
    for row in rows:
        if not isinstance(row, dict) or not isinstance(row.get("name"), str) or not row["name"]:
            raise ValueError("Each stage needs a name.")
        command = row.get("command")
        if not isinstance(command, (str, list)) or not command or (
            isinstance(command, list) and not all(isinstance(arg, str) for arg in command)
        ):
            raise ValueError("Each command must be shell text or a non-empty list of strings.")
        expected = row.get("expect", ["cmake"])
        if not isinstance(expected, list) or not all(name in TOOLS for name in expected):
            raise ValueError("The expect field must list CMake tool names.")
        stages.append(Stage(row["name"], command, tuple(expected)))
    return stages


def artifact_dir(output: Path) -> Path:
    """Keep the evidence for each run in a new directory next to the report."""
    output.parent.mkdir(parents=True, exist_ok=True)
    return Path(tempfile.mkdtemp(prefix=f"{output.stem}-", dir=output.parent)).resolve()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--stage", type=Path, required=True, help="Stage directory or Rcmake executable.")
    parser.add_argument("--oracle", default=os.environ.get("RCMAKE_ORACLE_CMAKE"),
                        help="Path to the CMake 4.5.1 executable.")
    parser.add_argument("--source", required=True, type=Path, help="Project directory to copy.")
    parser.add_argument("--commands", required=True, type=Path, help="JSON array of named stages.")
    parser.add_argument("--configured-output", action="append", default=[],
                        help="Project-relative glob for configure_file outputs. Repeat for each glob.")
    parser.add_argument("--out-json", required=True, type=Path, help="Path to the JSON report.")
    parser.add_argument("--timeout", type=int, default=600, help="Time limit in seconds per command.")
    args = parser.parse_args()
    if not args.oracle or args.timeout < 1:
        parser.error("Set an oracle path and a positive timeout.")
    try:
        source = args.source.resolve(strict=True)
        if not source.is_dir() or args.out_json.resolve().is_relative_to(source):
            raise ValueError("Use a project directory and put the report outside it.")
        binary, oracle = stage_dir(args.stage), executable(args.oracle)
        stages = load_stages(args.commands)
        report = compare_runs(source, stages, binary, oracle, artifact_dir(args.out_json),
                              args.timeout, configured_outputs=tuple(args.configured_output))
        write_json(args.out_json, report)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(str(error), file=sys.stderr)
        return 2
    return {"pass": 0, "fail": 1, "oracle_error": 2}[report["status"]]


if __name__ == "__main__":
    raise SystemExit(main())
