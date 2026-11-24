#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Compare incremental and cache-off reconfigures after seeded leaf edits.

Source env/env.sh before use. Set --fresh-clean to recreate the reference
build before each reference reconfigure. Use --cmake-arg=-DNAME=VALUE
for project options.
Set --model-dump to compare a model dump emitted by the selected binary.
"""

from __future__ import annotations

import argparse
from collections import Counter, defaultdict
from dataclasses import dataclass
import difflib
import hashlib
import json
import os
from pathlib import Path
import random
import re
import shutil
import sys
import tempfile

from oracle_prop_test import clean_environment, executable, run_command, write_json


API = Path(".cmake/api/v1")
QUERIES = ("codemodel-v2", "cache-v2", "cmakeFiles-v1", "toolchains-v1")
TARGET_TYPES = {"EXECUTABLE", "STATIC_LIBRARY", "SHARED_LIBRARY", "MODULE_LIBRARY", "OBJECT_LIBRARY"}
TARGET_NAME = re.compile(r"[A-Za-z0-9_.+-]+")
DURATION = re.compile(rb"(?m)^(-- (?:Configuring|Generating) done) \([0-9.]+s\)$")
FIXTURE = {
    "CMakeLists.txt": "cmake_minimum_required(VERSION 4.5)\nproject(M6Mutation C)\n"
                      "add_subdirectory(core)\nadd_subdirectory(helper)\n",
    "core/CMakeLists.txt": "add_library(core STATIC core.c)\n",
    "core/core.c": "int core(void) { return 1; }\n",
    "helper/CMakeLists.txt": "add_library(helper STATIC helper.c)\n",
    "helper/helper.c": "int helper(void) { return 2; }\n",
}


@dataclass(frozen=True)
class Leaf:
    """Identify a configured leaf and the C or C++ targets it owns."""

    lists: Path
    targets: tuple[tuple[str, str], ...]


@dataclass(frozen=True)
class Edit:
    """Record one reversible change to source files."""

    kind: str
    files: dict[Path, tuple[bytes | None, bytes | None]]

    def apply(self, source: Path) -> None:
        """Apply the edit only if the source still matches its recorded state."""
        for relative, (before, _) in self.files.items():
            path = source / relative
            if path.resolve() != path or read_optional(path) != before:
                raise ValueError(f"Source changed before the edit. File {relative}.")
        for relative, (_, after) in self.files.items():
            path = source / relative
            if after is None:
                path.unlink()
            else:
                path.write_bytes(after)

    def reverse(self) -> Edit:
        """Return an edit that restores the previous file contents."""
        return Edit("remove source", {name: (after, before)
                                      for name, (before, after) in self.files.items()})


def read_optional(path: Path) -> bytes | None:
    """Read a file and keep a missing file distinct from an empty file."""
    return path.read_bytes() if path.is_file() else None


def normalize(data: bytes, source: Path, build: Path) -> bytes:
    """Replace isolated tree paths and configure timings."""
    roots = ((source, b"<SOURCE>"), (build, b"<BUILD>"))
    for path, token in sorted(roots, key=lambda pair: len(str(pair[0])), reverse=True):
        data = data.replace(os.fsencode(path), token)
    return DURATION.sub(rb"\1", data)


def json_bytes(value) -> bytes:
    """Serialize JSON consistently while preserving array order."""
    return (json.dumps(value, sort_keys=True, indent=2) + "\n").encode()


def normalize_json(value, source: Path, build: Path, names: dict[str, str]):
    """Replace paths and File API references in JSON strings."""
    if isinstance(value, str):
        return normalize(names.get(value, value).encode(), source, build).decode()
    if isinstance(value, list):
        return [normalize_json(item, source, build, names) for item in value]
    if isinstance(value, dict):
        if "jsonFile" in value and value["jsonFile"] not in names:
            raise ValueError(f"Missing File API reply {value['jsonFile']}.")
        return {key: normalize_json(item, source, build, names) for key, item in value.items()}
    return value


def file_api(build: Path, source: Path, successful: bool) -> dict[str, bytes]:
    """Read all File API replies and normalize generated names and references."""
    reply = build / API / "reply"
    files = {path.name: json.loads(path.read_bytes()) for path in reply.glob("*.json")}
    indices = sorted(name for name in files if name.startswith("index-"))
    if successful and (len(indices) != 1 or not set(QUERIES) <= files[indices[0]]["reply"].keys()):
        raise ValueError(f"Missing or ambiguous File API index in {reply}.")
    stems = {name: "index.json" if name in indices else
             re.sub(r"-[a-f0-9]{20}(?=\.json$)", "", name) for name in files}
    counts = Counter(stems.values())
    names = {}
    for name, stem in stems.items():
        if counts[stem] > 1:
            # Use normalized content to distinguish replies with the same stem.
            raw = normalize(json_bytes(files[name]), source, build)
            digest = hashlib.sha256(raw).hexdigest()
            stem = f"{stem.removesuffix('.json')}-{digest}.json"
        names[name] = stem
    if len(set(names.values())) != len(names):
        raise ValueError(f"Duplicate File API replies in {reply}.")
    return {f"{API}/reply/{names[name]}": json_bytes(normalize_json(value, source, build, names))
            for name, value in files.items()}


def discover_leaves(source: Path, build: Path) -> list[Leaf]:
    """Find active leaf directories and their buildable targets."""
    reply = build / API / "reply"
    index = json.loads(next(reply.glob("index-*.json")).read_bytes())
    name = index["reply"]["codemodel-v2"]["jsonFile"]
    codemodel = json.loads((reply / name).read_bytes())
    leaves = defaultdict(set)
    for config in codemodel["configurations"]:
        directories = config["directories"]
        for directory in directories:
            if directory.get("childIndexes"):
                continue
            lists = source / directory["source"] / "CMakeLists.txt"
            if lists.is_relative_to(source) and lists.resolve() == lists and lists.is_file():
                leaves[lists.relative_to(source)]
        for target in config["targets"]:
            lists = Path(directories[target["directoryIndex"]]["source"]) / "CMakeLists.txt"
            if lists not in leaves or not TARGET_NAME.fullmatch(target["name"]):
                continue
            entry = json.loads((reply / target["jsonFile"]).read_bytes())
            languages = {group["language"] for group in entry.get("compileGroups", [])}
            if entry["type"] in TARGET_TYPES and languages & {"C", "CXX"}:
                leaves[lists].add((target["name"], "c" if "C" in languages else "cpp"))
    return [Leaf(path, tuple(sorted(targets))) for path, targets in sorted(leaves.items())]


def make_edit(kind: str, leaf: Leaf, source: Path, step: int, rng: random.Random) -> Edit:
    """Prepare a comment, a source change, or a target or directory setting."""
    lists = (source / leaf.lists).read_bytes()
    marker = f"RCMAKE_M6_{step}"
    files = {}
    if kind == "comment":
        new = lists + f"\n# Mutation {step}.\n".encode()
    elif kind == "directory definition":
        new = lists + f"\nadd_compile_definitions({marker}=1)\n".encode()
    else:
        target, extension = rng.choice(leaf.targets)
        if kind == "add source":
            name = f"rcmake_m6_mutation_{step}.{extension}"
            relative = leaf.lists.with_name(name)
            files[relative] = (None, f"int rcmake_m6_mutation_{step}(void) {{ return {step}; }}\n".encode())
            command = f'target_sources({target} PRIVATE "{name}")'
        elif kind == "toggle definition":
            command = f"target_compile_definitions({target} PRIVATE {marker}=1)"
        else:
            command = f"set_property(TARGET {target} PROPERTY POSITION_INDEPENDENT_CODE ON)"
        new = lists + f"\n{command}\n".encode()
    files[leaf.lists] = (lists, new)
    return Edit(kind, files)


def configure(args, source: Path, build: Path, env: dict[str, str]) -> dict:
    """Request File API replies and configure one source copy."""
    query = build / API / "query"
    query.mkdir(parents=True, exist_ok=True)
    for kind in QUERIES:
        (query / kind).touch()
    return run_command(
        [args.rcmake, "-S", source, "-B", build, "-G", "Ninja",
         "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON", *args.cmake_arg],
        source, args.timeout, env,
    )


def snapshot(result: dict, source: Path, build: Path, args) -> dict[str, bytes | None]:
    """Collect configure output and generated artifacts."""
    outputs = {name: normalize(result[name].encode(), source, build)
               for name in ("stdout", "stderr")}
    outputs["exit status"] = str(result["returncode"]).encode() + b"\n"
    outputs["execution error"] = (result["error"] or "").encode()
    successful = result["returncode"] == 0 and not result["error"]
    artifacts = {"CMakeCache.txt", "build.ninja", "compile_commands.json", "CMakeFiles/rules.ninja"}
    artifacts.update(str(path.relative_to(build)) for path in build.rglob("*.ninja")
                     if not {"CMakeScratch", "CMakeTmp"} & set(path.relative_to(build).parts))
    if args.model_dump:
        artifacts.add(args.model_dump)
    required = {"CMakeCache.txt", "build.ninja"}
    if args.model_dump:
        required.add(args.model_dump)
    for name in artifacts:
        raw = read_optional(build / name)
        if raw is None and successful and name in required:
            raise ValueError(f"Missing generated file {build / name}.")
        outputs[name] = normalize(raw, source, build) if raw is not None else None
    outputs.update(file_api(build, source, successful))
    return outputs


def diff_bytes(before: bytes | None, after: bytes | None, left: str, right: str) -> bytes:
    """Create a unified diff and distinguish an empty file from a missing file."""
    if before == after:
        return b""
    if b"\0" in (before or b"") or b"\0" in (after or b""):
        return f"Binary data differs for {left} and {right}.\n".encode()
    diff = difflib.diff_bytes(
        difflib.unified_diff,
        (before or b"").splitlines(keepends=True), (after or b"").splitlines(keepends=True),
        os.fsencode(left if before is not None else "/dev/null"),
        os.fsencode(right if after is not None else "/dev/null"),
    )
    return b"".join(line if line.endswith(b"\n") else line + b"\n\\ No newline at end of file\n"
                    for line in diff) or f"File presence differs for {left} and {right}.\n".encode()


def compare(args, report: dict, sources, builds, envs, step: int, edit: Edit | None) -> None:
    """Stop at the first divergence and save the edit and output differences."""
    left_result = configure(args, sources[0], builds[0], envs[0])
    right_result = configure(args, sources[1], builds[1], envs[1])
    if step and args.fresh_clean and right_result["returncode"] == 0 and not right_result["error"]:
        right_result = configure(args, sources[1], builds[1], envs[1])
    results = (left_result, right_result)
    row = {"step": step, "edit": edit.kind if edit else "initial configure",
           "files": [str(path) for path in edit.files] if edit else [],
           "exit_status": [result["returncode"] for result in results]}
    report["steps"].append(row)
    edit_diff = b""
    if edit:
        edit_diff = b"".join(diff_bytes(before, after, f"a/{path}", f"b/{path}")
                             for path, (before, after) in edit.files.items())
        row["patch"] = edit_diff.decode(errors="replace")
    differences = []
    try:
        left, right = [snapshot(result, source, build, args)
                       for result, source, build in zip(results, sources, builds)]
        for name in sorted(left.keys() | right.keys()):
            differences.append(diff_bytes(left.get(name), right.get(name),
                                          f"incremental/{name}", f"clean/{name}"))
        if any(result["error"] for result in results) or (
            (step == 0 or args.smoke) and left_result["returncode"]
        ):
            for lane, result in zip(("incremental", "clean"), results):
                differences.append(f"{lane}: {result}\n".encode())
    except (OSError, ValueError, KeyError) as error:
        differences.append(f"{error}\n".encode())
    difference = b"".join(differences)
    row["passed"] = not difference
    if difference:
        repro = args.out_json.with_suffix(".repro.diff")
        if repro.absolute() in {args.out_json.absolute(), args.out_md.absolute()}:
            repro = args.out_json.with_name(args.out_json.name + ".repro.diff")
        repro.parent.mkdir(parents=True, exist_ok=True)
        repro.write_bytes(f"Seed {args.seed}. Step {step}. {row['edit']}.\n".encode()
                          + edit_diff + b"\n" + difference)
        report["repro_diff"] = str(repro)
        raise ValueError(f"Configure comparison failed at step {step}. See {repro}.")


def evaluate(args, report: dict, root: Path) -> None:
    """Copy the project and apply the same sequence of edits to both copies."""
    original = args.project
    if args.smoke:
        original = root / "fixture"
        for name, contents in FIXTURE.items():
            path = original / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(contents, encoding="utf-8")
    lanes = [root / "incremental", root / "clean"]
    sources = [lane / "source" for lane in lanes]
    builds = [lane / "build" for lane in lanes]
    envs = []
    for lane, source in zip(lanes, sources):
        shutil.copytree(original, source, symlinks=True,
                        ignore=shutil.ignore_patterns(".git", "._*"))
        env = clean_environment()
        env.pop("RCMAKE_NO_INCREMENTAL", None)
        env["RCMAKE_CACHE_DIR"] = str(lane / "cache")
        env["RCMAKE_PROBE_CACHE_DIR"] = str(lane / "cache")
        envs.append(env)
    envs[1]["RCMAKE_NO_INCREMENTAL"] = "1"
    compare(args, report, sources, builds, envs, 0, None)
    leaves = discover_leaves(sources[0], builds[0])
    if not leaves:
        raise ValueError("The configured project has no editable leaf CMakeLists.txt.")
    target_leaves = [leaf for leaf in leaves if leaf.targets]
    rng = random.Random(args.seed)
    pending = None
    kinds = []
    for step in range(1, args.edits + 1):
        if pending:
            edit, pending = pending, None
        else:
            if not kinds:
                kinds = ["comment", "directory definition"]
                if target_leaves:
                    kinds += ["add source", "toggle definition", "target property"]
                rng.shuffle(kinds)
            kind = kinds.pop()
            leaf = rng.choice(leaves if kind in {"comment", "directory definition"} else target_leaves)
            edit = make_edit(kind, leaf, sources[0], step, rng)
            if kind == "add source":
                pending = edit.reverse()
            elif kind == "toggle definition":
                path = leaf.lists
                before = edit.files[path][1]
                marker = f"RCMAKE_M6_{step}=".encode()
                after = before.removesuffix(marker + b"1)\n") + marker + b"0)\n"
                pending = Edit(kind, {path: (before, after)})
        for source in sources:
            edit.apply(source)
        if args.fresh_clean:
            shutil.rmtree(builds[1])
        compare(args, report, sources, builds, envs, step, edit)
    if args.smoke and read_optional(builds[0] / "compile_commands.json") is None:
        raise ValueError("The smoke fixture did not generate a compilation database.")


def main() -> int:
    """Run the comparison and write JSON and Markdown reports."""
    parser = argparse.ArgumentParser(description=__doc__)
    project = parser.add_mutually_exclusive_group(required=True)
    project.add_argument("--project", type=Path)
    project.add_argument("--smoke", action="store_true")
    parser.add_argument("--rcmake", required=True)
    parser.add_argument("--edits", type=int, default=20)
    parser.add_argument("--seed", type=int, default=0)
    parser.add_argument("--out-json", type=Path, required=True)
    parser.add_argument("--out-md", type=Path, required=True)
    parser.add_argument("--fresh-clean", action="store_true")
    parser.add_argument("--cmake-arg", action="append", default=[])
    parser.add_argument("--model-dump")
    parser.add_argument("--timeout", type=int, default=120)
    args = parser.parse_args()
    if args.out_json.resolve() == args.out_md.resolve():
        parser.error("Use different paths for the JSON and Markdown reports.")
    if args.edits < 1 or args.timeout < 1:
        parser.error("Use positive values for --edits and --timeout.")
    if args.model_dump and (Path(args.model_dump).is_absolute() or ".." in Path(args.model_dump).parts):
        parser.error("Use a build-relative path for --model-dump.")
    report = {"gate": "G7", "project": str(args.project) if args.project else "smoke",
              "rcmake": args.rcmake, "seed": args.seed, "edits": args.edits,
              "mode": "smoke" if args.smoke else "gate", "fresh_clean": args.fresh_clean,
              "model_dump": args.model_dump, "cmake_args": args.cmake_arg, "steps": []}
    try:
        args.rcmake = executable(args.rcmake)
        if args.project:
            args.project = args.project.resolve()
            if not (args.project / "CMakeLists.txt").is_file():
                raise ValueError(f"No CMakeLists.txt at {args.project}.")
        with tempfile.TemporaryDirectory(prefix="rcmake-m6-mutation-") as directory:
            evaluate(args, report, Path(directory).resolve())
    except (OSError, RuntimeError, ValueError, KeyError) as error:
        report["error"] = str(error)
        print(error, file=sys.stderr)
    report["passed"] = "error" not in report
    write_json(args.out_json, report)
    rows = ["# M6 leaf-edit comparison", "", f"Seed {args.seed}. Mode {report['mode']}.",
            "", "| Step | Edit | Result |", "| --- | --- | --- |"]
    rows.extend(f"| {row['step']} | {row['edit']} | {'Pass' if row.get('passed') else 'Fail'} |"
                for row in report["steps"])
    if "error" in report:
        rows.extend(("", report["error"]))
    args.out_md.parent.mkdir(parents=True, exist_ok=True)
    args.out_md.write_text("\n".join(rows) + "\n", encoding="utf-8")
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
