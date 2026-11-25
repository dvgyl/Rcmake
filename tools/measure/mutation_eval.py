#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Verify the results of seeded edits and scripted replay traps.

Source env/env.sh before use. Use --project or --fixture to verify configure
replay across seeded edits, or --traps <file.toml> to run scripted replay
soundness traps. Use --smoke for short checks of the built-in project.
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
from pathlib import Path, PurePosixPath
import random
import re
import shutil
import sys
import tempfile
import tomllib

try:
    from .oracle_prop_test import clean_environment, executable, run_command, write_json
except ImportError:
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


VERIFY_GLOBS = "CMakeFiles/VerifyGlobs.cmake"


def configure(args, source: Path, build: Path, env: dict[str, str], *, fresh=False,
              extra_cmake_args: tuple[str, ...] = ()) -> dict:
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
         "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON", *args.cmake_arg, *extra_cmake_args],
        source, args.timeout, env,
    )


def snapshot(result: dict, source: Path, build: Path, args, *,
             require_build_files: bool = True,
             extra_optional_files: tuple[str, ...] = ()) -> dict[str, bytes]:
    """Collect configure output and generated artifacts."""
    outputs = {name: normalize(result[name].encode(), source, build)
               for name in ("stdout", "stderr")}
    if not require_build_files:
        cache = build / "CMakeCache.txt"
        if cache.is_file():
            outputs["CMakeCache.txt"] = normalize(cache.read_bytes(), source, build)
        for name in extra_optional_files:
            path = build / name
            if path.is_file():
                outputs[name] = normalize(path.read_bytes(), source, build)
        return outputs
    required = {"CMakeCache.txt", "build.ninja", "compile_commands.json", "CMakeFiles/rules.ninja"}
    if args.model_dump:
        required.add(args.model_dump)
    artifacts = set(required)
    artifacts.update(str(path.relative_to(build)) for path in build.rglob("*.ninja")
                     if not {"CMakeScratch", "CMakeTmp"} & set(path.relative_to(build).parts))
    for name in extra_optional_files:
        if (build / name).is_file():
            artifacts.add(name)
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


def _validate_rel_path(raw: object, label: str) -> str:
    """Validate and normalize a relative POSIX path inside a trap fixture or build tree."""
    if not isinstance(raw, str) or not raw or "\0" in raw or "\\" in raw:
        raise ValueError(f"Invalid relative path for {label}: {raw!r}.")
    posix = PurePosixPath(raw)
    if posix.is_absolute() or not posix.parts or any(part in {".", ".."} for part in posix.parts):
        raise ValueError(f"Path must stay inside its root for {label}: {raw!r}.")
    return posix.as_posix()


def _parse_str_tuple(raw: object, label: str, *, allow_empty_item: bool = False) -> tuple[str, ...]:
    """Validate a TOML list of strings."""
    if raw is None:
        return ()
    if not isinstance(raw, list) or any(not isinstance(item, str) for item in raw):
        raise ValueError(f"Expected a list of strings for {label}.")
    if not allow_empty_item and any(not item for item in raw):
        raise ValueError(f"Empty string is not allowed in {label}.")
    return tuple(raw)


def _parse_str_dict(raw: object, label: str) -> dict[str, str]:
    """Validate a TOML table mapping strings to strings."""
    if raw is None:
        return {}
    if not isinstance(raw, dict) or any(
        not isinstance(k, str) or not k or not isinstance(v, str) for k, v in raw.items()
    ):
        raise ValueError(f"Expected a string-to-string table for {label}.")
    return dict(raw)


@dataclass(frozen=True)
class FileReplace:
    """Describe one deterministic text replacement inside a trap source file."""

    path: str
    old: str
    new: str
    count: int = 1


@dataclass(frozen=True)
class TrapMutation:
    """Describe file, environment, and CLI define changes between run 0 and run 1."""

    write: dict[str, str] = field(default_factory=dict)
    replace: tuple[FileReplace, ...] = ()
    remove: tuple[str, ...] = ()
    env: dict[str, str] = field(default_factory=dict)
    unset_env: tuple[str, ...] = ()
    defines: dict[str, str] = field(default_factory=dict)
    cmake_args: tuple[str, ...] | None = None


@dataclass(frozen=True)
class ReplayTrap:
    """Describe one scripted replay soundness trap."""

    id: str
    name: str
    stage: str
    description: str
    files: dict[str, str]
    mutate: TrapMutation
    expect_executed: tuple[str, ...]
    expect_replayed: tuple[str, ...]
    expect_not_replayed: tuple[str, ...] = ()
    expect_executed_calls: tuple[str, ...] = ()
    expect_replayed_calls: tuple[str, ...] = ()
    expect_not_replayed_calls: tuple[str, ...] = ()
    initial_env: dict[str, str] = field(default_factory=dict)
    initial_defines: dict[str, str] = field(default_factory=dict)
    initial_cmake_args: tuple[str, ...] = ()
    expect_exit_code: int = 0
    expect_initial_exit_code: int | None = None
    expect_stdout_contains: tuple[str, ...] = ()
    expect_stderr_contains: tuple[str, ...] = ()
    expect_build_contains: dict[str, tuple[str, ...]] = field(default_factory=dict)
    skip_reason: str | None = None


@dataclass(frozen=True)
class ReplayReceipt:
    """Parsed contents of CMakeFiles/rcmake/replay-result.v1.json."""

    executed_listfiles: frozenset[str]
    replayed_listfiles: frozenset[str]
    executed_calls: tuple[str, ...] = ()
    replayed_calls: tuple[str, ...] = ()
    has_call_fields: bool = False


def load_traps(path: Path) -> list[ReplayTrap]:
    """Load and validate scripted replay traps from a TOML manifest."""
    raw = tomllib.loads(path.read_text(encoding="utf-8"))
    if raw.get("schema_version") != 1:
        raise ValueError(f"Unsupported replay trap schema_version in {path}: {raw.get('schema_version')!r}.")
    entries = raw.get("trap")
    if not isinstance(entries, list) or not entries:
        raise ValueError(f"Expected a non-empty [[trap]] list in {path}.")
    traps: list[ReplayTrap] = []
    seen_ids: set[str] = set()
    seen_names: set[str] = set()
    for index, entry in enumerate(entries, start=1):
        if not isinstance(entry, dict):
            raise ValueError(f"Trap entry #{index} in {path} must be a table.")
        trap_id = entry.get("id")
        name = entry.get("name")
        stage = entry.get("stage")
        if not isinstance(trap_id, str) or not trap_id.strip():
            raise ValueError(f"Trap entry #{index} is missing a non-empty 'id'.")
        if not isinstance(name, str) or not name.strip():
            raise ValueError(f"Trap {trap_id} is missing a non-empty 'name'.")
        if not isinstance(stage, str) or not stage.strip():
            raise ValueError(f"Trap {trap_id} is missing a non-empty 'stage'.")
        if trap_id.upper() in seen_ids:
            raise ValueError(f"Duplicate trap id {trap_id!r} in {path}.")
        if name in seen_names:
            raise ValueError(f"Duplicate trap name {name!r} in {path}.")
        seen_ids.add(trap_id.upper())
        seen_names.add(name)

        description = entry.get("description", "")
        if not isinstance(description, str):
            raise ValueError(f"Trap {trap_id} 'description' must be a string.")
        skip_reason = entry.get("skip_reason")
        if skip_reason is not None:
            if not isinstance(skip_reason, str) or not skip_reason.strip():
                raise ValueError(f"Trap {trap_id} 'skip_reason' must be a non-empty string when present.")
            skip_reason = skip_reason.strip()

        raw_files = entry.get("files")
        if not isinstance(raw_files, dict) or not raw_files:
            raise ValueError(f"Trap {trap_id} must define a non-empty [trap.files] table.")
        files: dict[str, str] = {}
        for rel, content in raw_files.items():
            norm = _validate_rel_path(rel, f"{trap_id}.files")
            if not isinstance(content, str):
                raise ValueError(f"Trap {trap_id} file {rel!r} content must be a string.")
            files[norm] = content
        if "CMakeLists.txt" not in files:
            raise ValueError(f"Trap {trap_id} must include a root 'CMakeLists.txt' in files.")

        raw_mutate = entry.get("mutate")
        if raw_mutate is None:
            raw_mutate = {}
        if not isinstance(raw_mutate, dict):
            raise ValueError(f"Trap {trap_id} 'mutate' must be a table.")
        mut_write: dict[str, str] = {}
        for rel, content in _parse_str_dict(raw_mutate.get("write"), f"{trap_id}.mutate.write").items():
            mut_write[_validate_rel_path(rel, f"{trap_id}.mutate.write")] = content
        raw_replaces = raw_mutate.get("replace") or []
        if not isinstance(raw_replaces, list):
            raise ValueError(f"Trap {trap_id} 'mutate.replace' must be a list of tables.")
        replaces: list[FileReplace] = []
        for rep_idx, rep in enumerate(raw_replaces, start=1):
            if not isinstance(rep, dict):
                raise ValueError(f"Trap {trap_id} mutate.replace #{rep_idx} must be a table.")
            rep_path = _validate_rel_path(rep.get("path"), f"{trap_id}.mutate.replace[{rep_idx}].path")
            old = rep.get("old")
            new = rep.get("new")
            count = rep.get("count", 1)
            if not isinstance(old, str) or not old or not isinstance(new, str):
                raise ValueError(f"Trap {trap_id} mutate.replace #{rep_idx} requires non-empty 'old' and string 'new'.")
            if type(count) is not int:
                raise ValueError(f"Trap {trap_id} mutate.replace #{rep_idx} 'count' must be an integer.")
            replaces.append(FileReplace(rep_path, old, new, count))
        mut_remove = tuple(
            _validate_rel_path(rel, f"{trap_id}.mutate.remove")
            for rel in _parse_str_tuple(raw_mutate.get("remove"), f"{trap_id}.mutate.remove")
        )
        mut_env = _parse_str_dict(raw_mutate.get("env"), f"{trap_id}.mutate.env")
        mut_unset_env = _parse_str_tuple(raw_mutate.get("unset_env"), f"{trap_id}.mutate.unset_env")
        mut_defines = _parse_str_dict(raw_mutate.get("defines"), f"{trap_id}.mutate.defines")
        raw_mut_args = raw_mutate.get("cmake_args")
        mut_cmake_args = (
            None if raw_mut_args is None
            else _parse_str_tuple(raw_mut_args, f"{trap_id}.mutate.cmake_args")
        )
        mutation = TrapMutation(
            write=mut_write,
            replace=tuple(replaces),
            remove=mut_remove,
            env=mut_env,
            unset_env=mut_unset_env,
            defines=mut_defines,
            cmake_args=mut_cmake_args,
        )

        expect_executed = tuple(
            _validate_rel_path(rel, f"{trap_id}.expect_executed")
            for rel in _parse_str_tuple(entry.get("expect_executed"), f"{trap_id}.expect_executed")
        )
        expect_replayed = tuple(
            _validate_rel_path(rel, f"{trap_id}.expect_replayed")
            for rel in _parse_str_tuple(entry.get("expect_replayed"), f"{trap_id}.expect_replayed")
        )
        expect_not_replayed = tuple(
            _validate_rel_path(rel, f"{trap_id}.expect_not_replayed")
            for rel in _parse_str_tuple(entry.get("expect_not_replayed"), f"{trap_id}.expect_not_replayed")
        )
        expect_executed_calls = _parse_str_tuple(
            entry.get("expect_executed_calls"), f"{trap_id}.expect_executed_calls"
        )
        expect_replayed_calls = _parse_str_tuple(
            entry.get("expect_replayed_calls"), f"{trap_id}.expect_replayed_calls"
        )
        expect_not_replayed_calls = _parse_str_tuple(
            entry.get("expect_not_replayed_calls"), f"{trap_id}.expect_not_replayed_calls"
        )
        expect_exit_code = entry.get("expect_exit_code", 0)
        expect_initial_exit_code = entry.get("expect_initial_exit_code")
        if type(expect_exit_code) is not int:
            raise ValueError(f"Trap {trap_id} 'expect_exit_code' must be an integer.")
        if expect_initial_exit_code is not None and type(expect_initial_exit_code) is not int:
            raise ValueError(f"Trap {trap_id} 'expect_initial_exit_code' must be an integer.")
        if expect_exit_code == 0 and not (expect_replayed or expect_replayed_calls):
            raise ValueError(
                f"Trap {trap_id} ({name}) must declare at least one control entry in "
                "'expect_replayed' or 'expect_replayed_calls' so replay is non-vacuous."
            )

        raw_build_contains = entry.get("expect_build_contains") or {}
        if not isinstance(raw_build_contains, dict):
            raise ValueError(f"Trap {trap_id} 'expect_build_contains' must be a table.")
        expect_build_contains: dict[str, tuple[str, ...]] = {}
        for rel, needles in raw_build_contains.items():
            norm = _validate_rel_path(rel, f"{trap_id}.expect_build_contains")
            expect_build_contains[norm] = _parse_str_tuple(
                needles, f"{trap_id}.expect_build_contains[{rel}]"
            )

        traps.append(
            ReplayTrap(
                id=trap_id,
                name=name,
                stage=stage,
                description=description,
                files=files,
                mutate=mutation,
                expect_executed=expect_executed,
                expect_replayed=expect_replayed,
                expect_not_replayed=expect_not_replayed,
                expect_executed_calls=expect_executed_calls,
                expect_replayed_calls=expect_replayed_calls,
                expect_not_replayed_calls=expect_not_replayed_calls,
                initial_env=_parse_str_dict(entry.get("initial_env"), f"{trap_id}.initial_env"),
                initial_defines=_parse_str_dict(
                    entry.get("initial_defines"), f"{trap_id}.initial_defines"
                ),
                initial_cmake_args=_parse_str_tuple(
                    entry.get("initial_cmake_args"), f"{trap_id}.initial_cmake_args"
                ),
                expect_exit_code=expect_exit_code,
                expect_initial_exit_code=expect_initial_exit_code,
                expect_stdout_contains=_parse_str_tuple(
                    entry.get("expect_stdout_contains"), f"{trap_id}.expect_stdout_contains"
                ),
                expect_stderr_contains=_parse_str_tuple(
                    entry.get("expect_stderr_contains"), f"{trap_id}.expect_stderr_contains"
                ),
                expect_build_contains=expect_build_contains,
                skip_reason=skip_reason,
            )
        )
    return traps


def select_traps(traps: list[ReplayTrap], filters: list[str] | None = None) -> list[ReplayTrap]:
    """Filter traps by ID (case-insensitive) or name, supporting comma-separated entries."""
    if not filters:
        return list(traps)
    by_id = {trap.id.upper(): trap for trap in traps}
    by_name = {trap.name: trap for trap in traps}
    requested: list[str] = []
    for item in filters:
        for part in item.split(","):
            token = part.strip()
            if token:
                requested.append(token)
    if not requested:
        raise ValueError("Specify at least one trap ID or name with --trap.")
    selected: list[ReplayTrap] = []
    seen: set[str] = set()
    unknown: list[str] = []
    for token in requested:
        trap = by_id.get(token.upper()) or by_name.get(token)
        if trap is None:
            unknown.append(token)
            continue
        if trap.id not in seen:
            seen.add(trap.id)
            selected.append(trap)
    if unknown:
        valid = ", ".join(f"{t.id} ({t.name})" for t in traps)
        raise ValueError(f"Unknown trap(s): {', '.join(unknown)}. Valid traps: {valid}.")
    return selected


def _resolve_trap_file(source: Path, rel: str) -> Path:
    """Resolve a validated relative path inside a trap source tree."""
    root = source.resolve()
    target = (root / rel).resolve()
    if not target.is_relative_to(root):
        raise ValueError(f"Trap file path escapes source directory: {rel!r}.")
    return target


def write_trap_files(source: Path, files: dict[str, str]) -> None:
    """Materialize a trap's initial source files into one lane's source tree."""
    source.mkdir(parents=True, exist_ok=True)
    for rel, content in files.items():
        path = _resolve_trap_file(source, rel)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")


def apply_trap_mutation(source: Path, mutation: TrapMutation) -> bytes:
    """Apply file writes, replacements, and removals to a trap source tree and return a diff."""
    diffs: list[bytes] = []
    for rel, content in mutation.write.items():
        path = _resolve_trap_file(source, rel)
        before = read_optional(path)
        after = content.encode("utf-8")
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(after)
        diffs.append(diff_bytes(before, after, f"a/{rel}", f"b/{rel}"))
    for rule in mutation.replace:
        path = _resolve_trap_file(source, rule.path)
        before = read_optional(path)
        if before is None:
            raise ValueError(f"Mutation target file is missing: {rule.path}.")
        text = before.decode("utf-8")
        occurrences = text.count(rule.old)
        if occurrences == 0 or (rule.count > 0 and occurrences < rule.count):
            raise ValueError(
                f"Mutation replacement pattern not found in {rule.path}: {rule.old!r} "
                f"(found {occurrences}, required {max(1, rule.count)})."
            )
        updated = (
            text.replace(rule.old, rule.new, rule.count)
            if rule.count > 0
            else text.replace(rule.old, rule.new)
        )
        after = updated.encode("utf-8")
        path.write_bytes(after)
        diffs.append(diff_bytes(before, after, f"a/{rule.path}", f"b/{rule.path}"))
    for rel in mutation.remove:
        path = _resolve_trap_file(source, rel)
        before = read_optional(path)
        if before is None:
            raise ValueError(f"Mutation removal target file is missing: {rel}.")
        path.unlink()
        diffs.append(diff_bytes(before, None, f"a/{rel}", f"b/{rel}"))
    return b"".join(diffs)


def _source_relative_posix(raw_path: str, source: Path, resolved_source: Path) -> str | None:
    """Convert an absolute listfile path from replay-result.v1.json into a source-relative POSIX path."""
    candidate = Path(raw_path)
    if candidate.is_relative_to(source):
        return candidate.relative_to(source).as_posix()
    resolved = candidate.resolve() if candidate.is_absolute() else candidate
    if resolved.is_relative_to(resolved_source):
        return resolved.relative_to(resolved_source).as_posix()
    return None


def replay_receipt(source: Path, build: Path) -> ReplayReceipt:
    """Read executed/replayed listfiles and optional call-level lists from replay-result.v1.json."""
    receipt_path = build / REPLAY_RESULT
    if not receipt_path.is_file():
        raise ValueError(f"Missing replay result in {receipt_path}.")
    record = json.loads(receipt_path.read_bytes())
    if not isinstance(record, dict) or record.get("version") != 1:
        raise ValueError(f"Invalid replay result in {receipt_path}.")
    resolved_source = source.resolve()

    def parse_paths(key: str) -> frozenset[str]:
        items = record.get(key)
        if not isinstance(items, list) or any(not isinstance(item, str) for item in items):
            raise ValueError(f"Invalid {key!r} list in {receipt_path}.")
        result = set()
        for item in items:
            rel = _source_relative_posix(item, source, resolved_source)
            if rel is not None:
                result.add(rel)
        return frozenset(result)

    has_call_fields = "executed_calls" in record or "replayed_calls" in record

    def parse_calls(key: str) -> tuple[str, ...]:
        if key not in record:
            return ()
        items = record[key]
        if not isinstance(items, list) or any(not isinstance(item, str) for item in items):
            raise ValueError(f"Invalid {key!r} list in {receipt_path}.")
        return tuple(items)

    return ReplayReceipt(
        executed_listfiles=parse_paths("executed"),
        replayed_listfiles=parse_paths("replayed"),
        executed_calls=parse_calls("executed_calls"),
        replayed_calls=parse_calls("replayed_calls"),
        has_call_fields=has_call_fields,
    )


def verify_trap_receipt(
    trap: ReplayTrap,
    source: Path,
    build: Path,
    replay_off_receipt: ReplayReceipt | None,
    row: dict,
) -> None:
    """Verify non-vacuous replay and expected executed/replayed listfiles and calls for a trap."""
    if trap.expect_exit_code != 0:
        receipt_path = build / REPLAY_RESULT
        if not receipt_path.is_file():
            row["replay_receipt_present"] = False
            return
        receipt = replay_receipt(source, build)
        row["replay_receipt_present"] = True
        row["executed_listfiles"] = sorted(receipt.executed_listfiles)
        row["replayed_listfiles"] = sorted(receipt.replayed_listfiles)
        if receipt.has_call_fields:
            row["executed_calls"] = list(receipt.executed_calls)
            row["replayed_calls"] = list(receipt.replayed_calls)
        replayed_calls_lower = {name.lower() for name in receipt.replayed_calls}
        for path in trap.expect_not_replayed:
            if path in receipt.replayed_listfiles:
                raise ValueError(f"Trap {trap.id} ({trap.name}): opaque listfile {path!r} was replayed.")
        if receipt.has_call_fields:
            for call in trap.expect_not_replayed_calls:
                if call.lower() in replayed_calls_lower:
                    raise ValueError(f"Trap {trap.id} ({trap.name}): opaque call {call!r} was replayed.")
        return

    require_replay_record(source, build)
    row["replay_record"] = True
    receipt = replay_receipt(source, build)
    row["executed_listfiles"] = sorted(receipt.executed_listfiles)
    row["replayed_listfiles"] = sorted(receipt.replayed_listfiles)
    row["has_call_fields"] = receipt.has_call_fields
    if receipt.has_call_fields:
        row["executed_calls"] = list(receipt.executed_calls)
        row["replayed_calls"] = list(receipt.replayed_calls)

    for path in trap.expect_executed:
        if path not in receipt.executed_listfiles:
            raise ValueError(
                f"Trap {trap.id} ({trap.name}): expected listfile {path!r} to execute. "
                f"Executed: {sorted(receipt.executed_listfiles)}."
            )
    for path in trap.expect_replayed:
        if path not in receipt.replayed_listfiles:
            raise ValueError(
                f"Trap {trap.id} ({trap.name}): expected control listfile {path!r} to replay. "
                f"Replayed: {sorted(receipt.replayed_listfiles)}."
            )
        if replay_off_receipt is not None and path not in replay_off_receipt.executed_listfiles:
            raise ValueError(
                f"Trap {trap.id} ({trap.name}): control listfile {path!r} did not execute in replay-off lane."
            )
    for path in trap.expect_not_replayed:
        if path in receipt.replayed_listfiles:
            raise ValueError(
                f"Trap {trap.id} ({trap.name}): opaque listfile {path!r} must not replay."
            )

    needs_calls = bool(trap.expect_executed_calls or trap.expect_replayed_calls)
    if needs_calls and not receipt.has_call_fields:
        raise ValueError(
            f"Trap {trap.id} ({trap.name}): replay-result.v1.json is missing "
            "'executed_calls'/'replayed_calls' required to verify call-level replay."
        )
    executed_calls_lower = {name.lower() for name in receipt.executed_calls}
    replayed_calls_lower = {name.lower() for name in receipt.replayed_calls}
    for call in trap.expect_executed_calls:
        if call.lower() not in executed_calls_lower:
            raise ValueError(
                f"Trap {trap.id} ({trap.name}): expected call {call!r} to execute. "
                f"Executed calls: {list(receipt.executed_calls)}."
            )
    for call in trap.expect_replayed_calls:
        if call.lower() not in replayed_calls_lower:
            raise ValueError(
                f"Trap {trap.id} ({trap.name}): expected call {call!r} to replay. "
                f"Replayed calls: {list(receipt.replayed_calls)}."
            )
    if receipt.has_call_fields:
        for call in trap.expect_not_replayed_calls:
            if call.lower() in replayed_calls_lower:
                raise ValueError(
                    f"Trap {trap.id} ({trap.name}): opaque call {call!r} must not replay."
                )
    if not receipt.replayed_listfiles and not receipt.replayed_calls:
        raise ValueError(
            f"Trap {trap.id} ({trap.name}): replay result is vacuous (no replayed listfiles or calls)."
        )


def _trap_cmake_args(defines: dict[str, str], extra_args: tuple[str, ...]) -> tuple[str, ...]:
    """Build deterministic CMake CLI arguments from extra args and -D defines."""
    return (*extra_args, *(f"-D{name}={value}" for name, value in sorted(defines.items())))


def _trap_lane_env(base_env: dict[str, str], overrides: dict[str, str],
                   unset: tuple[str, ...] = ()) -> dict[str, str]:
    """Return a lane environment copy with overrides applied and unset keys removed."""
    env = dict(base_env)
    for key in unset:
        env.pop(key, None)
    env.update(overrides)
    return env


def _compare_trap_snapshots(
    args,
    results: list[dict],
    sources: list[Path],
    builds: list[Path],
    *,
    expected_exit_code: int,
    step: int,
) -> tuple[list[dict[str, bytes]], list[bytes]]:
    """Compare 3-lane trap outputs and return snapshots plus any diff failures."""
    lanes = ("incremental", "replay-off", "fresh-clean")
    failures: list[bytes] = []
    for lane, result in zip(lanes, results):
        if result["error"]:
            failures.append(f"{lane} configure errored at step {step}. {result}\n".encode())
        elif result["returncode"] != expected_exit_code:
            failures.append(
                f"{lane} exited with {result['returncode']} at step {step}; "
                f"expected {expected_exit_code}. stderr:\n{result['stderr']}\n".encode()
            )
    if failures:
        return [], failures
    require_build = expected_exit_code == 0
    snapshots = [
        snapshot(
            result,
            source,
            build,
            args,
            require_build_files=require_build,
            extra_optional_files=(VERIFY_GLOBS,),
        )
        for result, source, build in zip(results, sources, builds)
    ]
    left = snapshots[0]
    for index, lane in enumerate(lanes[1:], start=1):
        right = snapshots[index]
        names = left.keys() | right.keys()
        if lane == "fresh-clean" and step > 0:
            # Cold compiler detection changes compiler-probe stdout/stderr, cmakeFiles-v1, and Ninja files.
            names -= {"stdout", "stderr", f"{API}/reply/cmakeFiles-v1.json"}
            names = {name for name in names if not name.endswith(".ninja") or name == args.model_dump}
        for name in sorted(names):
            diff = diff_bytes(left.get(name), right.get(name), f"incremental/{name}", f"{lane}/{name}")
            if diff:
                failures.append(diff)
    return snapshots, failures


def _trap_repro_path(args, trap_id: str) -> Path:
    """Choose a non-colliding repro diff path for one failing trap."""
    repro = args.out_json.with_name(f"{args.out_json.stem}.{trap_id}.repro.diff")
    if repro.absolute() in {args.out_json.absolute(), args.out_md.absolute()}:
        repro = args.out_json.with_name(f"{args.out_json.name}.{trap_id}.repro.diff")
    return repro


def run_trap(args, trap: ReplayTrap, trap_dir: Path) -> dict:
    """Execute one scripted replay trap across incremental, replay-off, and fresh-clean lanes."""
    lanes = ("incremental", "replay-off", "fresh-clean")
    lane_dirs = [trap_dir / lane for lane in lanes]
    sources = [lane / "source" for lane in lane_dirs]
    builds = [lane / "build" for lane in lane_dirs]
    base_envs: list[dict[str, str]] = []
    for index, (lane_dir, source) in enumerate(zip(lane_dirs, sources)):
        write_trap_files(source, trap.files)
        env = clean_environment()
        env.pop("RCMAKE_NO_INCREMENTAL", None)
        env.pop("RCMAKE_PROBE_CACHE_DIR", None)
        env["RCMAKE_CACHE_DIR"] = str(lane_dir / "cache")
        if index != 0:
            env["RCMAKE_NO_INCREMENTAL"] = "1"
        base_envs.append(env)

    row: dict = {
        "id": trap.id,
        "name": trap.name,
        "stage": trap.stage,
        "status": "passed",
        "passed": True,
    }
    initial_exit = (
        trap.expect_exit_code
        if trap.expect_initial_exit_code is None
        else trap.expect_initial_exit_code
    )
    env0 = [_trap_lane_env(env, trap.initial_env) for env in base_envs]
    extra0 = _trap_cmake_args(trap.initial_defines, trap.initial_cmake_args)
    mutation_diff = b""
    failures: list[bytes] = []
    try:
        results0 = [
            configure(args, source, build, env, fresh=(lane == "fresh-clean"), extra_cmake_args=extra0)
            for lane, source, build, env in zip(lanes, sources, builds, env0)
        ]
        row["initial_exit_status"] = [res["returncode"] for res in results0]
        _, step0_failures = _compare_trap_snapshots(
            args, results0, sources, builds, expected_exit_code=initial_exit, step=0
        )
        failures.extend(step0_failures)
        if not failures and initial_exit == 0:
            require_replay_record(sources[0], builds[0])
            if any((build / REPLAY_RECORD).exists() for build in builds[1:]):
                raise ValueError("A replay-disabled configure published a replay record on step 0.")

        if not failures:
            for index, source in enumerate(sources):
                diff = apply_trap_mutation(source, trap.mutate)
                if index == 0:
                    mutation_diff = diff
            if mutation_diff:
                row["patch"] = mutation_diff.decode(errors="replace")
            if builds[2].exists():
                shutil.rmtree(builds[2])
            env1 = [
                _trap_lane_env(env, trap.mutate.env, trap.mutate.unset_env)
                for env in env0
            ]
            defines1 = {**trap.initial_defines, **trap.mutate.defines}
            args1 = (
                trap.initial_cmake_args
                if trap.mutate.cmake_args is None
                else trap.mutate.cmake_args
            )
            extra1 = _trap_cmake_args(defines1, args1)
            results1 = [
                configure(args, source, build, env, fresh=(lane == "fresh-clean"), extra_cmake_args=extra1)
                for lane, source, build, env in zip(lanes, sources, builds, env1)
            ]
            row["exit_status"] = [res["returncode"] for res in results1]
            snapshots1, step1_failures = _compare_trap_snapshots(
                args, results1, sources, builds, expected_exit_code=trap.expect_exit_code, step=1
            )
            failures.extend(step1_failures)
            if not failures:
                for stream_name, needles in (
                    ("stdout", trap.expect_stdout_contains),
                    ("stderr", trap.expect_stderr_contains),
                ):
                    for lane, snap in zip(lanes, snapshots1):
                        text = snap[stream_name].decode(errors="replace")
                        for needle in needles:
                            if needle not in text:
                                failures.append(
                                    f"Trap {trap.id} ({trap.name}): {lane} {stream_name} missing {needle!r}.\n".encode()
                                )
                for rel_file, needles in trap.expect_build_contains.items():
                    raw_file = snapshots1[0].get(rel_file)
                    if raw_file is None:
                        file_path = builds[0] / rel_file
                        if file_path.is_file():
                            raw_file = normalize(file_path.read_bytes(), sources[0], builds[0])
                    if raw_file is None:
                        failures.append(
                            f"Trap {trap.id} ({trap.name}): expected build file {rel_file!r} is missing.\n".encode()
                        )
                        continue
                    text = raw_file.decode(errors="replace")
                    for needle in needles:
                        if needle not in text:
                            failures.append(
                                f"Trap {trap.id} ({trap.name}): build file {rel_file!r} missing {needle!r}.\n".encode()
                            )
                if any((build / REPLAY_RECORD).exists() for build in builds[1:]):
                    raise ValueError("A replay-disabled configure published a replay record on step 1.")
                replay_off_receipt = (
                    replay_receipt(sources[1], builds[1])
                    if trap.expect_exit_code == 0 and (builds[1] / REPLAY_RESULT).is_file()
                    else None
                )
                verify_trap_receipt(trap, sources[0], builds[0], replay_off_receipt, row)
    except (OSError, ValueError, KeyError, TypeError, AttributeError) as error:
        failures.append(f"{error}\n".encode())

    if failures:
        difference = b"".join(failures)
        repro = _trap_repro_path(args, trap.id)
        repro.parent.mkdir(parents=True, exist_ok=True)
        repro.write_bytes(
            f"Trap {trap.id} ({trap.name}) stage {trap.stage}.\n".encode()
            + mutation_diff + b"\n" + difference
        )
        row["status"] = "failed"
        row["passed"] = False
        row["repro_diff"] = str(repro)
        row["error"] = difference.decode(errors="replace").strip()
    return row


def evaluate_traps(args, report: dict, root: Path) -> None:
    """Load, filter, and run scripted replay soundness traps."""
    traps = select_traps(load_traps(args.traps), args.trap)
    force_pending = bool(args.include_pending or args.trap)
    counts = Counter({"passed": 0, "failed": 0, "skipped": 0})
    for trap in traps:
        if trap.skip_reason and not force_pending:
            row = {
                "id": trap.id,
                "name": trap.name,
                "stage": trap.stage,
                "status": "skipped",
                "passed": None,
                "skip_reason": trap.skip_reason,
            }
            report["traps"].append(row)
            counts["skipped"] += 1
            continue
        row = run_trap(args, trap, root / trap.id)
        report["traps"].append(row)
        counts[row["status"]] += 1
    report["summary"] = {
        "total": len(traps),
        "passed": counts["passed"],
        "failed": counts["failed"],
        "skipped": counts["skipped"],
    }
    report["replay_verified"] = counts["passed"] > 0 and counts["failed"] == 0


def render_traps_markdown(report: dict) -> str:
    """Render Markdown summary and diagnostics for --traps mode."""
    summary = report["summary"]
    rows = [
        "# Replay soundness traps",
        "",
        f"Manifest `{report['traps_file']}`. "
        f"Passed {summary['passed']}. Failed {summary['failed']}. "
        f"Skipped {summary['skipped']} of {summary['total']}.",
        "",
        "| ID | Name | Stage | Result | Detail |",
        "| --- | --- | --- | --- | --- |",
    ]
    for trap in report["traps"]:
        status = trap["status"].capitalize()
        if trap["status"] == "skipped":
            detail = trap.get("skip_reason", "")
        elif trap["status"] == "passed":
            replayed = trap.get("replayed_listfiles", [])
            detail = f"Replayed: {', '.join(replayed)}" if replayed else "Verified"
        else:
            detail = f"See `{trap['repro_diff']}`" if "repro_diff" in trap else "Failed"
        rows.append(f"| {trap['id']} | {trap['name']} | {trap['stage']} | {status} | {detail} |")
    for trap in report["traps"]:
        if trap["status"] == "failed" and "error" in trap:
            rows.extend(("", f"## {trap['id']} {trap['name']}", "", trap["error"]))
    if "error" in report:
        rows.extend(("", report["error"]))
    return "\n".join(rows) + "\n"


def build_parser() -> argparse.ArgumentParser:
    """Build the command-line parser for leaf-edit and scripted trap evaluation."""
    parser = argparse.ArgumentParser(description=__doc__)
    project = parser.add_mutually_exclusive_group(required=True)
    project.add_argument("--project", type=Path, help="Verify replay for a project directory.")
    project.add_argument("--smoke", action="store_true", help="Run smoke checks on the fixture.")
    project.add_argument("--fixture", action="store_true", help="Verify replay for the fixture.")
    project.add_argument("--traps", type=Path, help="Run scripted replay soundness traps from a TOML file.")
    parser.add_argument("--trap", action="append", default=None,
                        help="Run only the named trap ID(s) or name(s) with --traps. Repeat or comma-separate.")
    parser.add_argument("--include-pending", action="store_true",
                        help="Also execute traps marked with a skip_reason in --traps mode.")
    parser.add_argument("--rcmake", required=True)
    parser.add_argument("--edits", type=int, default=20)
    parser.add_argument("--seed", type=int, default=0)
    parser.add_argument("--out-json", type=Path, required=True)
    parser.add_argument("--out-md", type=Path, required=True)
    parser.add_argument("--fresh-clean", action="store_true")
    parser.add_argument("--cmake-arg", action="append", default=[])
    parser.add_argument("--model-dump")
    parser.add_argument("--timeout", type=int, default=120)
    return parser


def main(argv: list[str] | None = None) -> int:
    """Run the comparison and write JSON and Markdown reports."""
    parser = build_parser()
    args = parser.parse_args(argv)
    if (args.trap or args.include_pending) and args.traps is None:
        parser.error("--trap and --include-pending require --traps.")
    args.fresh_clean = args.fresh_clean or not args.smoke
    if args.out_json.resolve() == args.out_md.resolve():
        parser.error("Use different paths for the JSON and Markdown reports.")
    if args.edits < 1 or args.timeout < 1:
        parser.error("Use positive values for --edits and --timeout.")
    if args.model_dump is not None:
        path = Path(args.model_dump)
        if path.is_absolute() or not path.parts or ".." in path.parts:
            parser.error("Use a build-relative file path for --model-dump.")

    if args.traps is not None:
        report: dict = {
            "gate": "G7",
            "mode": "traps",
            "traps_file": str(args.traps),
            "rcmake": args.rcmake,
            "fresh_clean": True,
            "model_dump": args.model_dump,
            "cmake_args": args.cmake_arg,
            "include_pending": bool(args.include_pending),
            "trap_filter": list(args.trap or []),
            "traps": [],
            "summary": {"total": 0, "passed": 0, "failed": 0, "skipped": 0},
            "replay_verified": False,
        }
        try:
            args.rcmake = executable(args.rcmake)
            args.traps = args.traps.resolve()
            if not args.traps.is_file():
                raise ValueError(f"Trap manifest not found at {args.traps}.")
            with tempfile.TemporaryDirectory(prefix="rcmake-replay-traps-") as directory:
                evaluate_traps(args, report, Path(directory).resolve())
        except (OSError, RuntimeError, ValueError, KeyError, tomllib.TOMLDecodeError) as error:
            report["error"] = str(error)
            print(error, file=sys.stderr)
        report["passed"] = "error" not in report and report["summary"]["failed"] == 0
        write_json(args.out_json, report)
        args.out_md.parent.mkdir(parents=True, exist_ok=True)
        args.out_md.write_text(render_traps_markdown(report), encoding="utf-8")
        return 0 if report["passed"] else 1

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
    rows = ["# Leaf-edit comparison", "", f"Seed {args.seed}. Mode {report['mode']}.",
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
