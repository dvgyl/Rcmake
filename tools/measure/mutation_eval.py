#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Verify the results of seeded edits to configured leaf directories.

Source env/env.sh before use. Use --project or --fixture to verify configure
replay. Use --smoke for short checks of the built-in project.
Set --fresh-clean to compare stable outputs with a separate clean configure.
Use --cmake-arg=-DNAME=VALUE for project options. Use --model-dump to
include a model dump that the selected binary generates.
"""

from __future__ import annotations

import argparse
from collections import Counter, defaultdict
from dataclasses import dataclass, field
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
REPLAY_RECORD = Path("CMakeFiles/rcmake/configure.v1.bin")
REPLAY_MAGIC = b"RCMAKE-CONFIGURE\0\x01"
REPLAY_LIMIT = 16 * 1024 * 1024
REPLAY_RESULT = Path("CMakeFiles/rcmake/replay-result.v1.json")
EDIT_GROUPS = (("comment",), ("directory definition",), ("add source", "remove source"),
               ("set target definition", "toggle definition"), ("target property",))
CYCLE_SIZE = sum(map(len, EDIT_GROUPS))
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
class Effect:
    """Identify one observable change in a generated target."""

    target: str
    field: str
    value: str
    added: bool


@dataclass(frozen=True)
class Edit:
    """Record one reversible change to source files."""

    kind: str
    leaf: Path
    files: dict[Path, tuple[bytes | None, bytes | None]]
    effects: tuple[Effect, ...] = ()

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
        files = {name: (after, before) for name, (before, after) in self.files.items()}
        return Edit("remove source", self.leaf, files,
                    tuple(Effect(e.target, e.field, e.value, not e.added) for e in self.effects))


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


def file_api(build: Path, source: Path) -> dict[str, bytes]:
    """Require a complete set of File API replies and normalize its names."""
    reply = build / API / "reply"
    files = {path.name: json.loads(path.read_bytes()) for path in reply.glob("*.json")}
    indices = [name for name in files if name.startswith("index-")]
    if len(indices) != 1:
        raise ValueError(f"Missing or ambiguous File API index in {reply}.")
    index = files[indices[0]]["reply"]
    for query in QUERIES:
        if query not in index or index[query].get("jsonFile") not in files:
            raise ValueError(f"Missing File API reply for {query} in {reply}.")
    stems = {name: "index.json" if name.startswith("index-") else
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
    indices = list(reply.glob("index-*.json"))
    if len(indices) != 1:
        raise ValueError(f"Missing or ambiguous File API index in {reply}.")
    index = json.loads(indices[0].read_bytes())
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
    effects = ()
    if kind == "comment":
        new = lists + f"\n# Mutation {step}.\n".encode()
    else:
        target, extension = rng.choice(leaf.targets)
        if kind == "add source":
            name = f"rcmake_m6_mutation_{step}.{extension}"
            relative = leaf.lists.with_name(name)
            files[relative] = (None, f"int rcmake_m6_mutation_{step}(void) {{ return {step}; }}\n".encode())
            command = f'target_sources({target} PRIVATE "{name}")'
            effects = (Effect(target, "source", relative.as_posix(), True),)
        elif kind == "directory definition":
            command = f"add_compile_definitions({marker}=1)"
            effects = (Effect(target, "definition", f"{marker}=1", True),)
        elif kind == "set target definition":
            command = f"target_compile_definitions({target} PRIVATE {marker}=1)"
            effects = (Effect(target, "definition", f"{marker}=1", True),)
        else:
            command = f"set_property(TARGET {target} APPEND PROPERTY COMPILE_OPTIONS -D{marker}=1)"
            effects = (Effect(target, "flag", f"-D{marker}=1", True),)
        new = lists + f"\n{command}\n".encode()
    files[leaf.lists] = (lists, new)
    return Edit(kind, leaf.lists, files, effects)


def configure(args, source: Path, build: Path, env: dict[str, str], *, fresh=False) -> dict:
    """Request File API replies and configure one source copy."""
    query = build / API / "query"
    query.mkdir(parents=True, exist_ok=True)
    for kind in QUERIES:
        (query / kind).touch()
    reply = build / API / "reply"
    if reply.exists():
        shutil.rmtree(reply)
    if args.model_dump:
        (build / args.model_dump).unlink(missing_ok=True)
    if not args.smoke:
        (build / REPLAY_RESULT).unlink(missing_ok=True)
        env = {**env, "RCMAKE_MEASURE_REPLAY": "1"}
    flags = ["--fresh"] if fresh else []
    return run_command(
        [args.rcmake, *flags, "-S", source, "-B", build, "-G", "Ninja",
         "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON", *args.cmake_arg],
        source, args.timeout, env,
    )


def snapshot(result: dict, source: Path, build: Path, args) -> dict[str, bytes]:
    """Collect configure output and generated artifacts."""
    outputs = {name: normalize(result[name].encode(), source, build)
               for name in ("stdout", "stderr")}
    required = {"CMakeCache.txt", "build.ninja", "compile_commands.json", "CMakeFiles/rules.ninja"}
    if args.model_dump:
        required.add(args.model_dump)
    artifacts = set(required)
    artifacts.update(str(path.relative_to(build)) for path in build.rglob("*.ninja")
                     if not {"CMakeScratch", "CMakeTmp"} & set(path.relative_to(build).parts))
    for name in artifacts:
        raw = (build / name).read_bytes()
        if name in required and not raw:
            raise ValueError(f"Empty generated file {build / name}.")
        if name == "compile_commands.json" and not isinstance(json.loads(raw), list):
            raise ValueError(f"Invalid compilation database in {build / name}.")
        outputs[name] = normalize(raw, source, build)
    outputs.update(file_api(build, source))
    return outputs


def target_values(outputs: dict[str, bytes], target: str) -> dict[str, set[str]]:
    """Read compiled sources, definitions, and options for one target."""
    reply = f"{API}/reply"
    index = json.loads(outputs[f"{reply}/index.json"])
    name = index["reply"]["codemodel-v2"]["jsonFile"]
    codemodel = json.loads(outputs[f"{reply}/{name}"])
    values = {"source": set(), "definition": set(), "flag": set()}
    for config in codemodel["configurations"]:
        for entry in config["targets"]:
            if entry["name"] != target:
                continue
            model = json.loads(outputs[f"{reply}/{entry['jsonFile']}"])
            values["source"].update(item["path"] for item in model["sources"]
                                    if "compileGroupIndex" in item)
            for group in model["compileGroups"]:
                values["definition"].update(item["define"] for item in group.get("defines", []))
                for item in group.get("compileCommandFragments", []):
                    values["flag"].update(item["fragment"].split())
            return values  # Ninja generates one configuration.
    raise ValueError(f"Target {target} is missing from the codemodel.")


def verify_effects(edit: Edit, before: dict[str, bytes], after: dict[str, bytes]) -> None:
    """Check each requested change against the previous and current codemodels."""
    if not edit.effects:
        return
    target = edit.effects[0].target
    old = target_values(before, target)
    new = target_values(after, target)
    for effect in edit.effects:
        was_present = effect.value in old[effect.field]
        is_present = effect.value in new[effect.field]
        if was_present == effect.added or is_present != effect.added:
            action = "addition" if effect.added else "removal"
            raise ValueError(f"Target {target} did not show the {action} of {effect.value}.")


def replay_leaves(source: Path, build: Path) -> tuple[set[Path], set[Path]]:
    """Read the listfiles evaluated and replayed in this run."""
    record = json.loads((build / REPLAY_RESULT).read_bytes())
    if record.get("version") != 1:
        raise ValueError(f"Invalid replay result in {build / REPLAY_RESULT}.")
    def leaves(key: str) -> set[Path]:
        paths = (Path(name) for name in record[key])
        return {path.relative_to(source) for path in paths
                if path.name == "CMakeLists.txt" and path.is_relative_to(source)}
    return leaves("executed"), leaves("replayed")


def require_replay_record(source: Path, build: Path) -> None:
    """Check the current configure journal frame and its tree identity."""
    path = build / REPLAY_RECORD
    if not path.is_file():
        raise ValueError(f"No configure replay record in {path}.")
    header_size = len(REPLAY_MAGIC) + 5
    limit = header_size + REPLAY_LIMIT + 32
    with path.open("rb") as stream:
        record = stream.read(limit + 1)
    header = REPLAY_MAGIC + bytes([1 if os.name == "nt" else 2])
    size = int.from_bytes(record[len(header):header_size], "little")
    if (len(record) != header_size + size + 32 or size > REPLAY_LIMIT
            or not record.startswith(header) or hashlib.sha256(record[:-32]).digest() != record[-32:]):
        raise ValueError(f"Invalid configure replay record in {path}.")
    payload = json.loads(record[header_size:-32])
    region = payload.get("region", {})
    key = region.get("key", {})
    paths = [str(p).encode("utf-16-le", errors="surrogatepass") if os.name == "nt" else os.fsencode(p)
             for p in (source, build)]
    complete = "Complete" in region.get("payload", {})
    nested = payload.get("model", {}).get("nested", [])
    has_nested = any(isinstance(item, list) and len(item) == 2
                     and isinstance(item[0], dict) and "Complete" in item[0].get("payload", {})
                     for item in nested)
    if (payload.get("schema") != "configure.v1" or "model" not in payload
            or not (complete or has_nested) or key.get("kind") != "Configure"
            or [key.get("source_dir"), key.get("binary_dir")] != [list(p) for p in paths]):
        raise ValueError(f"Configure replay record does not match the configure in {path}.")


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


@dataclass(frozen=True)
class Observation:
    """Keep the output and the leaf state for one configure."""

    outputs: dict[str, bytes]
    executed: set[Path]
    leaf_digests: dict[Path, str]
    replayed: set[Path] = field(default_factory=set)


def observe(args, result: dict, source: Path, build: Path) -> Observation:
    """Collect fresh outputs and the leaf state for one lane."""
    outputs = snapshot(result, source, build, args)
    if args.smoke:
        return Observation(outputs, set(), {})
    executed, replayed = replay_leaves(source, build)
    digests = {leaf.lists: hashlib.sha256((source / leaf.lists).read_bytes()).hexdigest()
               for leaf in discover_leaves(source, build)}
    return Observation(outputs, executed, digests, replayed)


def verify_replay(row: dict, sources, builds, current: list[Observation],
                  previous: list[Observation], edit: Edit | None) -> None:
    """Require execution of the changed leaf and replay of an unchanged leaf."""
    require_replay_record(sources[0], builds[0])
    row["replay_record"] = True
    if any((build / REPLAY_RECORD).exists() for build in builds[1:]):
        raise ValueError("A replay-disabled configure published a replay record.")
    left, right = current[:2]
    if edit is None:
        if len(left.executed & right.executed & left.leaf_digests.keys()) < 2:
            raise ValueError("The initial runs must evaluate two configured leaf directories.")
        return
    changed = edit.leaf
    if changed not in left.executed or changed not in right.executed:
        raise ValueError(f"Both runs must evaluate the changed leaf {changed}.")
    row["changed_leaf_executed"] = str(changed)
    demonstrated = previous[0].executed | previous[0].replayed
    controls = right.executed & previous[1].executed & demonstrated
    controls = (controls & left.replayed) - {changed}
    replayed = sorted(path for path in controls if path in left.leaf_digests
                      and left.leaf_digests[path] == previous[0].leaf_digests.get(path)
                      == right.leaf_digests.get(path) == previous[1].leaf_digests.get(path))
    if not replayed:
        raise ValueError("The replay result shows no reuse of an unchanged configured leaf.")
    row["replayed_leaves"] = [str(path) for path in replayed]


def compare(args, report: dict, sources, builds, envs, step: int, edit: Edit | None,
            previous: list[Observation]) -> list[Observation]:
    """Stop at the first divergence and save the edit and output differences."""
    lanes = ("incremental", "replay-off", "fresh-clean")
    results = [configure(args, source, build, env, fresh=lane == "fresh-clean")
               for lane, source, build, env in zip(lanes, sources, builds, envs)]
    row = {"step": step, "edit": edit.kind if edit else "initial configure",
           "files": [str(path) for path in edit.files] if edit else [],
           "exit_status": [result["returncode"] for result in results[:2]]}
    report["steps"].append(row)
    if args.fresh_clean:
        fresh = results[2]
        row["fresh_configure"] = {
            **fresh,
            **{name: normalize(fresh[name].encode(), sources[2], builds[2]).decode()
               for name in ("stdout", "stderr")},
        }
    edit_diff = b""
    if edit:
        edit_diff = b"".join(diff_bytes(before, after, f"a/{path}", f"b/{path}")
                             for path, (before, after) in edit.files.items())
        row["patch"] = edit_diff.decode(errors="replace")
    failures = [f"{lane} configure failed. {result}\n".encode()
                for lane, result in zip(lanes, results)
                if result["returncode"] != 0 or result["error"]]
    current = []
    try:
        if not failures:
            current = [observe(args, result, source, build)
                       for result, source, build in zip(results, sources, builds)]
            left = current[0].outputs
            for index, lane in enumerate(lanes[1:len(current)], start=1):
                right = current[index].outputs
                names = left.keys() | right.keys()
                if lane == "fresh-clean" and step:
                    # Cold compiler detection changes the input list and Ninja files.
                    names -= {"stdout", "stderr", f"{API}/reply/cmakeFiles-v1.json"}
                    names = {name for name in names if not name.endswith(".ninja")
                             or name == args.model_dump}
                failures.extend(diff_bytes(left.get(name), right.get(name),
                                           f"incremental/{name}", f"{lane}/{name}")
                                for name in sorted(names))
            if edit:
                for before, after in zip(previous, current):
                    verify_effects(edit, before.outputs, after.outputs)
            if not args.smoke:
                verify_replay(row, sources, builds, current, previous, edit)
    except (OSError, ValueError, KeyError, TypeError, AttributeError) as error:
        failures.append(f"{error}\n".encode())
    difference = b"".join(failures)
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
    return current


def edit_kinds(count: int, rng: random.Random):
    """Generate complete edit groups up to the requested count."""
    while count:
        groups = list(EDIT_GROUPS)
        rng.shuffle(groups)
        for group in groups:
            if len(group) <= count:
                yield from group
                count -= len(group)


def toggle_definition(edit: Edit) -> Edit:
    """Change the target definition from one to zero."""
    effect = edit.effects[0]
    before = edit.files[edit.leaf][1]
    after = before.removesuffix(b"=1)\n") + b"=0)\n"
    return Edit("toggle definition", edit.leaf, {edit.leaf: (before, after)}, (
        Effect(effect.target, effect.field, effect.value, False),
        Effect(effect.target, effect.field, effect.value.removesuffix("1") + "0", True),
    ))


def evaluate(args, report: dict, root: Path) -> None:
    """Copy the project and apply the same sequence of edits to each copy."""
    if not args.smoke and args.edits < CYCLE_SIZE:
        raise ValueError(f"Gate mode requires at least {CYCLE_SIZE} edits for one complete cycle.")
    original = args.project
    if original is None:
        original = root / "fixture"
        for name, contents in FIXTURE.items():
            path = original / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(contents, encoding="utf-8")
    lanes = [root / "incremental", root / "replay-off"]
    if args.fresh_clean:
        lanes.append(root / "fresh-clean")
    sources = [lane / "source" for lane in lanes]
    builds = [lane / "build" for lane in lanes]
    envs = []
    for lane, source in zip(lanes, sources):
        shutil.copytree(original, source, symlinks=True,
                        ignore=shutil.ignore_patterns(".git", "._*"))
        env = clean_environment()
        env.pop("RCMAKE_NO_INCREMENTAL", None)
        env.pop("RCMAKE_PROBE_CACHE_DIR", None)
        env["RCMAKE_CACHE_DIR"] = str(lane / "cache")
        if lane != lanes[0]:
            env["RCMAKE_NO_INCREMENTAL"] = "1"
        envs.append(env)
    previous = compare(args, report, sources, builds, envs, 0, None, [])
    rng = random.Random(args.seed)
    pending = None
    for step, kind in enumerate(edit_kinds(args.edits, rng), start=1):
        if pending is not None:
            edit, pending = pending, None
        else:
            leaves = discover_leaves(sources[0], builds[0])
            eligible = [leaf for leaf in leaves if kind == "comment" or leaf.targets]
            if not eligible:
                raise ValueError(f"No configured leaf can accept a {kind} edit.")
            leaf = rng.choice(eligible)
            edit = make_edit(kind, leaf, sources[0], step, rng)
            if kind == "add source":
                pending = edit.reverse()
            elif kind == "set target definition":
                pending = toggle_definition(edit)
        for source in sources:
            edit.apply(source)
        if args.fresh_clean:
            shutil.rmtree(builds[2])
        previous = compare(args, report, sources, builds, envs, step, edit, previous)
        report["effective_edits"] += 1
        if edit.effects:
            report["semantic_edits"] += 1
    report["replay_verified"] = not args.smoke


def main() -> int:
    """Run the comparison and write JSON and Markdown reports."""
    parser = argparse.ArgumentParser(description=__doc__)
    project = parser.add_mutually_exclusive_group(required=True)
    project.add_argument("--project", type=Path, help="Verify replay for a project directory.")
    project.add_argument("--smoke", action="store_true", help="Run smoke checks on the fixture.")
    project.add_argument("--fixture", action="store_true", help="Verify replay for the fixture.")
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
    args.fresh_clean = args.fresh_clean or not args.smoke
    if args.out_json.resolve() == args.out_md.resolve():
        parser.error("Use different paths for the JSON and Markdown reports.")
    if args.edits < 1 or args.timeout < 1:
        parser.error("Use positive values for --edits and --timeout.")
    if args.model_dump is not None:
        path = Path(args.model_dump)
        if path.is_absolute() or not path.parts or ".." in path.parts:
            parser.error("Use a build-relative file path for --model-dump.")
    project_name = "smoke" if args.smoke else "fixture"
    if args.project:
        project_name = str(args.project)
    report = {"gate": "G7", "project": project_name,
              "rcmake": args.rcmake, "seed": args.seed, "edits": args.edits,
              "mode": "smoke" if args.smoke else "gate", "fresh_clean": args.fresh_clean,
              "model_dump": args.model_dump, "cmake_args": args.cmake_arg, "steps": [],
              "effective_edits": 0, "semantic_edits": 0, "replay_verified": False}
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
            f"Verified edits {report['effective_edits']} of {args.edits}. "
            f"Semantic edits {report['semantic_edits']}.",
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
