#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Compare pinned upstream projects with CMake 4.5.1 through PATH.

The evaluator runs upstream build code.
"""

import argparse
from collections import Counter
from functools import lru_cache
import json
import os
from pathlib import Path
import re
import shutil
import sys
import tempfile
import tomllib

from normalize import normalize_codemodel_abstract_target_ties
from unseen_pool import CORPUS
from unseen_select import validate_pin

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "measure"))
from ninja_canon import Normalizer, canonicalize  # noqa: E402
from oracle_prop_test import clean_environment, executable, run_command, write_json  # noqa: E402
from path_swap_eval import TOOLS, succeeded, support_path, verify_path  # noqa: E402

LEVELS = ("L1", "L2", "L3")
CACHE_ENTRY = re.compile(r'^((?:"[^"]*"|[^":]+):[A-Z]+)=(.*)$')
DRY_RUN_PREFIX = re.compile(r"^\[\d+/\d+\] ", re.MULTILINE)
DIAGNOSTIC_CATEGORIES = {
    "unknown-command": r"unknown cmake command|unknown command",
    "genex": r"generator expression",
    "find-module": r"could not find|find_package",
    "policy": r"\bCMP\d{4}\b",
}


def cache_entries(build):
    entries = {}
    for line in (build / "CMakeCache.txt").read_text(encoding="utf-8").splitlines():
        if line.startswith(("#", "//")):
            continue
        if match := CACHE_ENTRY.fullmatch(line):
            entries[match[1]] = match[2]
    return entries


def codemodel(build, normalize):
    """Resolve File API references before comparing their content."""
    reply = build / ".cmake/api/v1/reply"
    index = json.loads(next(reply.glob("index-*.json")).read_text(encoding="utf-8"))
    model = next(obj["jsonFile"] for obj in index["objects"] if obj["kind"] == "codemodel")
    active = set()

    @lru_cache(maxsize=None)
    def read_reply(name):
        if Path(name).name != name or not name.endswith(".json") or name in active:
            raise ValueError("Invalid File API reply reference.")
        active.add(name)
        try:
            data = json.loads((reply / name).read_text(encoding="utf-8"))
            return canonical(data)
        finally:
            active.remove(name)

    def canonical(value):
        if isinstance(value, dict):
            return {key: read_reply(item) if key == "jsonFile" else canonical(item)
                    for key, item in value.items()}
        if isinstance(value, list):
            return [canonical(item) for item in value]
        if isinstance(value, str):
            return normalize.text(value)
        return value

    return normalize_codemodel_abstract_target_ties(
        read_reply(model), dir_data_key="jsonFile"
    )


def capture_l1(build, source, tool_paths, configure):
    cache = cache_entries(build)
    replacements = {}
    cache_tools = {}
    # Map tool locations and module roots to stable labels. The cmake label has
    # priority when all three shim names resolve to the same executable.
    for key, name in (("CMAKE_ROOT", "modules"), ("CMAKE_CPACK_COMMAND", "cpack"),
                      ("CMAKE_CTEST_COMMAND", "ctest"), ("CMAKE_COMMAND", "cmake")):
        key += ":INTERNAL"
        label = f"<{name}>"
        if name in tool_paths:
            replacements[tool_paths[name]] = label
        path = cache.get(key, "")
        if Path(path).is_absolute():
            replacements[Path(path)] = label
            cache_tools[key] = label
    normalize = Normalizer(build, source, replacements)
    artifacts = {
        "exit": configure["returncode"],
        "stderr": normalize.text(configure["stderr"]).strip(),
        "cache": {key: cache_tools.get(key, normalize.text(value)) for key, value in cache.items()},
    }
    errors = {}
    for name, capture in (("codemodel", codemodel),
                          ("ninja", lambda build, normalize: canonicalize(normalize))):
        try:
            artifacts[name] = capture(build, normalize)
        except (KeyError, OSError, ValueError, StopIteration, RecursionError) as error:
            errors[name] = str(error) or "Required File API or Ninja data is missing."
    return artifacts, errors, normalize


def clone(project, source, timeout, env, log):
    """Fetch one commit with no history and check out that exact tree."""
    source.mkdir()
    for command in (
        ["git", "init", "--quiet"],
        ["git", "fetch", "--quiet", "--no-tags", "--depth=1", project["url"], project["commit"]],
        ["git", "checkout", "--quiet", "--detach", "FETCH_HEAD"],
        ["git", "rev-parse", "HEAD"],
    ):
        result = run_command(command, source, timeout, env)
        log.append(result)
        if not succeeded(result):
            return False
    return result["stdout"].strip() == project["commit"] and (source / "CMakeLists.txt").is_file()


def classify(result, default):
    code = result["returncode"]
    if result["error"]:
        return "timeout"
    if code < 0:
        return "crash"
    for category, pattern in DIAGNOSTIC_CATEGORIES.items():
        if re.search(pattern, result["stderr"], re.IGNORECASE):
            return category
    return default


def configure_lane(run, timeout, env, tool_paths, log):
    source, build = run / "src", run / "build"
    (run / "home").mkdir()
    query = build / ".cmake/api/v1/query"
    query.mkdir(parents=True)
    (query / "codemodel-v2").touch()
    configure = run_command(["cmake", "-S", source, "-B", build, "-G", "Ninja"],
                            run, timeout, env)
    log["L1"] = configure
    if not succeeded(configure):
        return None, {}, None
    try:
        artifacts, errors, normalize = capture_l1(build, source, tool_paths, configure)
    except (OSError, ValueError) as error:
        artifacts, errors, normalize = {}, {"cache": str(error)}, None
    if errors:
        log["artifact_errors"] = errors
    return artifacts, errors, normalize


def run_level(level, lane, run, timeout, env, log):
    commands = {"L2": ["ninja", "-C", "build", "-n", "-v"],
                "L3": ["cmake", "--build", "build", "--parallel", "2"]}
    lane.rename(run)
    try:
        result = run_command(commands[level], run, timeout, env)
        log[level] = result
        return result
    finally:
        run.rename(lane)


def dry_commands(result, normalize):
    return {normalize.text(command.rstrip("\n"))
            for command in DRY_RUN_PREFIX.split(result["stdout"])[1:]}


def evaluate(project, root, args, envs, tool_paths):
    result = {"name": project["name"], "split": project["split"], "levels": {},
              "commands": {"clone": [], "oracle": {}, "rcmake": {}}}
    levels, log = result["levels"], result["commands"]
    seed = root / "checkout"
    clone_env = {**clean_environment(), "GIT_TERMINAL_PROMPT": "0",
                 "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_SYSTEM": os.devnull}
    if not clone(project, seed, args.timeout, clone_env, log["clone"]):
        result["failure_category"] = classify(log["clone"][-1], "source")
        return result

    run = root / "run"
    run.mkdir()
    oracle_dir, rcmake_dir = root / "oracle-run", root / "rcmake-run"
    oracle_env, rcmake_env = [{**env, "HOME": str(run / "home"),
                              "XDG_CACHE_HOME": str(run / "home/.cache")}
                             for env in envs]
    shutil.copytree(seed, run / "src", symlinks=True)
    oracle, oracle_errors, oracle_norm = configure_lane(
        run, args.timeout, oracle_env, tool_paths[0], log["oracle"]
    )
    if oracle is None:
        levels["L1"] = "oracle-red"
        result["failure_category"] = "oracle-red"
        return result
    if oracle_errors:
        levels["L1"] = "unavailable"
        result["failure_category"] = "evaluation"
        return result
    run.rename(oracle_dir)
    run.mkdir()
    seed.rename(run / "src")
    candidate, candidate_errors, candidate_norm = configure_lane(
        run, args.timeout, rcmake_env, tool_paths[1], log["rcmake"]
    )
    if candidate is None:
        levels["L1"] = "fail"
        result["failure_category"] = classify(log["rcmake"]["L1"], "configure")
        return result
    differences = sorted(key for key, value in oracle.items() if candidate.get(key) != value)
    if candidate_errors or differences:
        levels["L1"] = "fail"
        result["l1_differences"] = differences
        category = "generator" if {"codemodel", "ninja"} & set(differences) else "configure"
        result["failure_category"] = classify(log["rcmake"]["L1"], category)
        return result
    levels["L1"] = "pass"
    if args.level == "L1":
        return result

    run.rename(rcmake_dir)
    for level in LEVELS[1:LEVELS.index(args.level) + 1]:
        reference = run_level(level, oracle_dir, run, args.timeout, oracle_env, log["oracle"])
        if not succeeded(reference):
            levels[level] = "oracle-red"
            result["failure_category"] = "oracle-red"
            break
        candidate = run_level(level, rcmake_dir, run, args.timeout, rcmake_env, log["rcmake"])
        if not succeeded(candidate):
            levels[level] = "fail"
            result["failure_category"] = classify(candidate, "generator" if level == "L2" else "build")
            break
        if level == "L2" and dry_commands(reference, oracle_norm) != dry_commands(candidate, candidate_norm):
            levels[level] = "fail"
            result["failure_category"] = "generator"
            break
        levels[level] = "pass"
    return result


def setup_paths(root, args):
    stage = args.stage.absolute()
    if not (stage / "cmake").is_file():
        stage /= "bin"
    oracle = executable(args.oracle)
    tool_paths = [
        {name: executable(oracle if name == "cmake" else oracle.with_name(name)) for name in TOOLS},
        {name: executable(stage / name) for name in TOOLS},
    ]
    if tool_paths[0]["cmake"].resolve() == tool_paths[1]["cmake"].resolve():
        raise ValueError("The oracle and stage must use different executables.")
    env = clean_environment()
    # Do not expose Cargo or rustc from the host PATH during project evaluation.
    path = os.pathsep.join(entry for entry in env["PATH"].split(os.pathsep) if entry
                           and not any(shutil.which(tool, path=entry) for tool in ("cargo", "rustc")))
    support = support_path(root, path)
    envs = []
    for lane, tools in zip(("oracle", "rcmake"), tool_paths):
        bin_dir = root / lane
        bin_dir.mkdir()
        for name, path in tools.items():
            (bin_dir / name).symlink_to(path)
        lane_env = {**env, "PATH": os.pathsep.join(map(str, [bin_dir, *support])),
                    "NINJA_STATUS": "[%f/%t] "}
        verify_path(bin_dir, root, args.timeout, lane_env)
        envs.append(lane_env)
        tools.update({name: bin_dir / name for name in TOOLS})
    if not shutil.which("ninja", path=envs[0]["PATH"]):
        raise ValueError("Ninja is required on PATH.")
    return envs, tool_paths


def shard(value):
    match = re.fullmatch(r"([1-9]\d*)/([1-9]\d*)", value)
    if match and (int(match[1]) <= int(match[2])):
        return int(match[1]), int(match[2])
    raise argparse.ArgumentTypeError("Use a one-based shard K/M with 1 <= K <= M.")


def select_projects(args):
    with args.manifest.open("rb") as file:
        projects = tomllib.load(file)["project"]
    names = set()
    for project in projects:
        validate_pin(project["name"], project["url"], project["commit"])
        key = project["name"].lower()
        if key in names or project["split"] not in {"dev", "holdout"}:
            raise ValueError("The manifest contains a duplicate repository or an invalid split.")
        names.add(key)
    selected = [project for project in projects
                if args.split == "all" or args.split == project["split"]]
    index, count = args.shard
    return selected[index - 1::count][:args.limit]


def summary(results, level):
    rates = {}
    for rung in LEVELS[:LEVELS.index(level) + 1]:
        counts = Counter(result["levels"].get(rung) for result in results)
        passed, eligible = counts["pass"], counts["pass"] + counts["fail"]
        rates[rung] = {"passed": passed, "eligible": eligible,
                       "rate": passed / eligible if eligible else None}
    histogram = Counter(result["failure_category"] for result in results if "failure_category" in result)
    return {"pass_rates": rates, "failure_categories": dict(sorted(histogram.items()))}


def markdown(report):
    lines = ["# Unseen corpus results", "",
             "Pass rates use the projects that reached each level and passed the oracle check.", ""]
    for split, data in report.items():
        lines.extend((f"## {split}", "", "| Level | Passed | Eligible | Pass rate |",
                      "| --- | ---: | ---: | ---: |"))
        for level, stats in data["pass_rates"].items():
            rate = "—" if stats["rate"] is None else f'{stats["rate"]:.1%}'
            lines.append(f'| {level} | {stats["passed"]} | {stats["eligible"]} | {rate} |')
        lines.extend(("", "### Failure categories", ""))
        lines.extend(f"- {category} ({count})" for category, count in data["failure_categories"].items())
        if not data["failure_categories"]:
            lines.append("None.")
        if "projects" in data:
            lines.extend(("", "### Projects", ""))
            for result in data["projects"]:
                status = ", ".join(f"{level} {state}" for level, state in result["levels"].items())
                lines.append(f'- `{result["name"]}` — {status or result["failure_category"]}')
        lines.append("")
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=CORPUS / "unseen.toml")
    parser.add_argument("--split", choices=("dev", "holdout", "all"), default="dev")
    parser.add_argument("--limit", type=int, help="Limit the number of projects after sharding.")
    parser.add_argument("--shard", type=shard, default=(1, 1))
    parser.add_argument("--stage", type=Path, required=True)
    parser.add_argument("--oracle", type=Path, required=True)
    parser.add_argument("--level", choices=LEVELS, default="L1")
    parser.add_argument("--timeout", type=int, default=120, help="Set the time limit per command in seconds.")
    parser.add_argument("--out-json", type=Path, required=True)
    parser.add_argument("--out-md", type=Path, required=True)
    parser.add_argument("--show-holdout-details", action="store_true", help="Include holdout project details.")
    args = parser.parse_args()
    if args.timeout <= 0 or (args.limit is not None and args.limit <= 0):
        parser.error("Use positive values for --timeout and --limit.")
    if args.out_json.resolve() == args.out_md.resolve():
        parser.error("Specify separate JSON and Markdown output files.")

    results = {split: [] for split in ("dev", "holdout") if args.split in (split, "all")}
    try:
        projects = select_projects(args)
        if not projects:
            raise ValueError("The selection contains no projects.")
        with tempfile.TemporaryDirectory(prefix="rcmake-unseen-tools-", dir="/tmp") as directory:
            envs, tool_paths = setup_paths(Path(directory).resolve(), args)
            for project in projects:
                split = project["split"]
                details = split == "dev" or args.show_holdout_details
                if details:
                    print(f'Evaluate {project["name"]}.', flush=True)
                with tempfile.TemporaryDirectory(prefix="rcmake-unseen-project-", dir="/tmp") as work:
                    try:
                        result = evaluate(project, Path(work).resolve(), args, envs, tool_paths)
                    except (OSError, KeyError, TypeError, ValueError) as error:
                        result = {"name": project["name"], "levels": {},
                                  "failure_category": "evaluation", "error": str(error)}
                if not details:
                    result = {key: result[key] for key in ("levels", "failure_category") if key in result}
                results[split].append(result)

        report = {split: summary(rows, args.level) for split, rows in results.items()}
        for split, rows in results.items():
            if split == "dev" or args.show_holdout_details:
                report[split]["projects"] = rows
        write_json(args.out_json, report)
        args.out_md.parent.mkdir(parents=True, exist_ok=True)
        args.out_md.write_text(markdown(report), encoding="utf-8")
        print(json.dumps({split: summary(rows, args.level) for split, rows in results.items()},
                         sort_keys=True))
    except (OSError, KeyError, TypeError, ValueError) as error:
        parser.exit(1, f"{error}\n")


if __name__ == "__main__":
    main()
