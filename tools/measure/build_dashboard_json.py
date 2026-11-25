#!/usr/bin/env python3
"""Merge verification reports into the public compatibility dashboard.

Use RunCMake evaluator reports for each OS. The core and policy tracks use
the matching suites from those reports. Use independent_verify.py,
probe_cache_eval.py, mutation_eval.py, path_swap_eval.py, and wasm_smoke.py
JSON for their respective gates. Repeat --bench to combine bench_m6.py and
bench_eval.py reports.

Use a boss_fight_local.py run directory for --boss-dir. Use downloaded
64-shard workflow artifacts with shard.json and results.jsonl for --vcpkg-dir.
For --fuzz-dir, supply JSON files with iterations and failures per run.
Failures can be a count or a list. For --unseen-holdout, supply an aggregate
object and a categories array. Use reposTotal and reposPassed for the counts
and category for the public category label. For --ci-status-json, use the
output of gh run list --json status,conclusion or the Actions REST response.
"""

import argparse
from collections import Counter, defaultdict
from datetime import datetime, timezone
import json
import math
from pathlib import Path
import re
from stat import S_IMODE
from statistics import geometric_mean
from tempfile import NamedTemporaryFile

if __package__:
    from .runcmake_eval import SUITE_GROUPS
else:
    from runcmake_eval import SUITE_GROUPS


DATA = Path(__file__).resolve().parents[2] / "site" / "data"
PLATFORMS = {
    "macos": "macos-arm64",
    "linux": "linux-x86_64",
    "windows": "windows-x86_64",
}
JSON_TYPES = {
    "object": (dict,), "array": (list,), "string": (str,), "integer": (int,),
    "number": (int, float), "boolean": (bool,), "null": (type(None),),
}


def validate(value, rule, root, field="$"):
    """Check the JSON Schema keywords used by dashboard.schema.json."""
    if "$ref" in rule:
        target = root
        for part in rule["$ref"].removeprefix("#/").split("/"):
            target = target[part.replace("~1", "/").replace("~0", "~")]
        validate(value, target, root, field)
    types = rule.get("type", [])
    if isinstance(types, str):
        types = [types]
    if types and not any(type(value) in JSON_TYPES[kind] for kind in types):
        raise ValueError(f"{field} must have type {' or '.join(types)}.")
    if "const" in rule and (type(value) is not type(rule["const"]) or value != rule["const"]):
        raise ValueError(f"{field} must match the schema constant.")
    if "enum" in rule and value not in rule["enum"]:
        raise ValueError(f"{field} must match a value in the schema.")
    if type(value) in (int, float):
        if isinstance(value, float) and not math.isfinite(value):
            raise ValueError(f"{field} must be finite.")
        if value < rule.get("minimum", -math.inf) or value > rule.get("maximum", math.inf):
            raise ValueError(f"{field} is outside the schema limits.")
    if isinstance(value, dict):
        properties = rule.get("properties", {})
        for key in rule.get("required", []):
            if key not in value:
                raise ValueError(f"{field}.{key} is required.")
        if rule.get("additionalProperties") is False and value.keys() - properties.keys():
            raise ValueError(f"{field} contains fields outside the schema.")
        for key, property_rule in properties.items():
            if key in value:
                validate(value[key], property_rule, root, f"{field}.{key}")
    if isinstance(value, list):
        if not rule.get("minItems", 0) <= len(value) <= rule.get("maxItems", math.inf):
            raise ValueError(f"{field} has an invalid number of items.")
        if "items" in rule:
            for index, item in enumerate(value):
                validate(item, rule["items"], root, f"{field}[{index}]")


def read_json(path):
    return json.loads(path.read_text(encoding="utf-8"))


def count(value):
    if type(value) is not int or value < 0:
        raise ValueError("Report counts must be nonnegative integers.")
    return value


def flag(value):
    if type(value) is not bool:
        raise ValueError("The report must use a boolean result.")
    return value


def rate(passed, total):
    if not 0 <= passed <= total:
        raise ValueError("The pass count must be between zero and the total.")
    return round(100 * (passed / total), 2) if total else None


def combined_status(statuses):
    statuses = set(statuses)
    if "fail" in statuses:
        return "fail"
    if statuses == {"pass"}:
        return "pass"
    return "partial"


def suite_metrics(row):
    passed, total = count(row["passed"]), count(row["total"])
    drivers_ok = all(
        driver.get("complete") is True and not driver.get("driver_errors")
        for key in ("oracle_driver", "rcmake_driver")
        if (driver := row.get(key)) is not None
    )
    rate(passed, total)
    passed_suite = (0 < passed == total and drivers_ok
                    and not row.get("reference_failed") and not row.get("no_reference"))
    status = "pass" if passed_suite else "fail"
    return passed, total, status


def suite_totals(results):
    passed = sum(row[0] for row in results)
    total = sum(row[1] for row in results)
    return dict(casesPassed=passed, casesTotal=total, passRate=rate(passed, total))


def merge_runcmake(tracks, reports):
    observations = defaultdict(list)
    track = tracks["runCmakeOs"]
    for platform in track["platforms"]:
        report = reports.get(platform["id"])
        if report is None:
            continue
        suites = {name: suite_metrics(row) for name, row in report["suites"].items()}
        if not suites:
            raise ValueError("A RunCMake report must contain at least one suite.")
        for name, result in suites.items():
            observations[name].append(result)
        results = list(suites.values())
        statuses = [row[2] for row in results]
        platform.update(suite_totals(results), suitesTotal=len(results),
                        suitesPassed=statuses.count("pass"), status=combined_status(statuses),
                        note="Results for suites in the supplied OS report.")
    track.update(status=combined_status(row["status"] for row in track["platforms"]),
                 note="Results from the OS reports.")

    suites = {
        name: (sum(row[0] for row in rows), sum(row[1] for row in rows),
               combined_status(row[2] for row in rows))
        for name, rows in observations.items()
    }
    core = tracks["coreConformance"]
    policies = tracks["policies"]
    core_rows = [suites[name] for name in SUITE_GROUPS["all"] if name in suites]
    policy_rows = [
        row for name, row in suites.items()
        if re.fullmatch(r"CMP[0-9]{4}", name) and int(name[3:]) >= 66
    ]
    for section, rows, pass_key in (
        (core, core_rows, "suitesAt100"), (policies, policy_rows, "suitesPassed"),
    ):
        if rows:
            statuses = [row[2] for row in rows]
            expected = max(section["suitesTotal"], len(rows))
            if len(rows) < expected:
                statuses.append("pending")
            status = combined_status(statuses)
            section.update(suite_totals(rows), suitesTotal=expected, status=status,
                           note="Case counts include each reported OS run of a matching suite.")
            section[pass_key] = statuses.count("pass")
    for highlight in core["highlights"]:
        if highlight["suite"] in suites:
            passed, total, status = suites[highlight["suite"]]
            highlight.update(passed=passed, total=total, status=status)
            highlight.pop("note", None)


def gate(section, identifier, label, passed, total, status, detail, **metrics):
    rate(passed, total)
    row = next((row for row in section["gates"] if row["id"] == identifier), None)
    if row is None:
        row = {"id": identifier, "label": label}
        section["gates"].append(row)
    row.update(status=status, passed=passed, total=total, failures=total - passed,
               summary=f"{passed:,} / {total:,} checks passed", detail=detail, **metrics)


def merge_independent(section, report):
    if report["mode"] != "differential":
        raise ValueError("Use a differential independent verification report.")
    scenarios = report["scenarios"]
    results = Counter(row["status"] for row in scenarios)
    checks = [
        check for row in scenarios if row["status"] != "skip"
        for check in row["runs"]["rcmake"]["checks"]
    ]
    status = "fail" if results["fail"] else "partial" if results["skip"] or not checks else "pass"
    gate(section, "blackbox", "Black-Box Scenario Suite", sum(flag(row["ok"]) for row in checks),
         len(checks), status, "Scenario results also require matching observations from CMake.",
         scenariosPassed=results["pass"], scenariosTotal=len(scenarios))


def merge_soundness(section, args):
    if args.independent:
        merge_independent(section, read_json(args.independent))
    if args.probe_cache:
        report = read_json(args.probe_cache)
        cases = [row for row in report["cases"] if row["status"] != "skipped"]
        passed = sum(flag(row["passed"]) for row in cases)
        status = "fail" if not flag(report["passed"]) else (
            "partial" if report.get("mode") == "smoke" or len(cases) < len(report["cases"]) else "pass"
        )
        gate(section, "probe-cache", "Persistent Probe Cache", passed, len(cases), status,
             "Counts include the compiler probe cache checks that ran.")
    if args.mutation:
        report = read_json(args.mutation)
        steps = [row for row in report["steps"] if row["step"] > 0]
        status = "fail" if not flag(report["passed"]) else (
            "pass" if report["replay_verified"] else "partial"
        )
        gate(section, "mutation-replay", "Mutation Replay Soundness",
             sum(flag(row.get("passed", False)) for row in steps), len(steps), status,
             "Counts include the edit comparisons and exclude the initial configure.")
    if args.determinism:
        report = read_json(args.determinism)
        passed = int(report["status"] == "pass")
        gate(section, "determinism", "Path-Swap Determinism", passed, 1,
             "pass" if passed else "fail", "Compares output from isolated runs at the same project path.")
    if args.wasm:
        report = read_json(args.wasm)
        checks = [row for row in report["checks"] if row["status"] != "planned"]
        status = {"pass": "pass", "fail": "fail", "incomplete": "partial", "planned": "pending"}
        gate(section, "wasi", "WebAssembly & WASI Gate",
             sum(row["status"] == "pass" for row in checks), len(checks), status[report["status"]],
             "Checks WebAssembly dependencies, compilation, and WASI execution.")


def merge_benchmarks(metrics, paths):
    rows = [row for path in paths for row in read_json(path)["results"]]

    def record(key, value, status, note):
        metrics[key].update(value=value, status=status, note=note)

    warm = [row for row in rows if row.get("case") == "warm probe-cache configure"]
    corpus = [row for row in warm if row["project"] != "synthetic"]
    if warm:
        selected = corpus or warm
        ratios = [row["speedup"] for row in selected]
        if all(value is not None and value > 0 for value in ratios):
            speedup = round(geometric_mean(ratios), 2)
        else:
            speedup = None
        status = benchmark_status(selected, speedup)
        record("warmProbeSpeedup", speedup, status,
               "Geometric mean across reported corpus projects." if corpus
               else "Geometric mean across reported synthetic runs.")

    incremental = [row for row in rows if row.get("case") == "incremental-leaf"]
    if incremental:
        samples = [row["rcmake"]["p95_ms"] for row in incremental]
        value = None if None in samples else max(samples)
        record("incrementalLeafP95Ms", value, benchmark_status(incremental, value),
               "Highest project p95 in the report.")

    for row in rows:
        if row.get("name") == "configure 500 targets (cold)":
            speedup = row["speedup"]
            rss = row["rcmake"]["peak_rss_mib"]
            record("coldConfigureSpeedup", speedup, "pass" if speedup is not None else "fail",
                   "Median cold configure time compared with CMake.")
            record("peakRssMiB", rss, "pass" if rss is not None and row["rcmake"]["ok"] else "fail",
                   "Highest resident memory use during cold configure.")


def benchmark_status(rows, value):
    if value is None or any(row.get("gate", {}).get("status") in {"fail", "unavailable"} for row in rows):
        return "fail"
    return "pass"


def report_paths(directory, pattern):
    paths = sorted(path for path in directory.rglob(pattern) if not path.name.startswith("._"))
    if not paths:
        raise ValueError(f"No {pattern} reports in the supplied directory.")
    return paths


def merge_fuzz(section, directory):
    total = failures = 0
    for path in report_paths(directory, "*.json"):
        report = read_json(path)
        failed = report["failures"]
        failed = len(failed) if isinstance(failed, list) else count(failed)
        iterations = count(report["iterations"])
        rate(iterations - failed, iterations)
        total += iterations
        failures += failed
    status = "fail" if failures else "pass" if total else "partial"
    gate(section, "fuzz", "Adversarial Fuzzing", total - failures, total, status,
         "Counts include all inputs in the supplied fuzz reports.")


def boss_level(values):
    states = {"equal": "pass", "different": "fail", "incomplete": "partial",
              "pass": "pass", "fail": "fail"}
    return combined_status(states[value] for value in values)


def merge_boss(track, directory):
    projects = {row["name"]: row for row in track["projects"]}
    # A local run stores the same project results in the summary and per-project files.
    summaries = {}
    individuals = {}
    for path in report_paths(directory, "result.json"):
        report = read_json(path)
        if "projects" in report:
            for result in report["projects"]:
                summaries[result["project"]] = result
        else:
            individuals[report["project"]] = report
    results = {**summaries, **individuals}
    if not results or results.keys() - projects.keys():
        raise ValueError("Boss reports must match the published project list.")
    for name, result in results.items():
        levels = result.get("l1", {})
        checks = levels.values() if isinstance(levels, dict) else [levels]
        l1 = boss_level(checks)
        l2 = boss_level([result.get("l2", "incomplete")])
        if result.get("status") in {"fail", "oracle_error"} or result.get("error"):
            l1 = "fail" if l1 == "partial" else l1
            l2 = "fail" if l2 == "partial" else l2
        projects[name].update(l1=l1, l2=l2)
        projects[name].pop("note", None)
    track.update(projectsTotal=len(projects),
                 l1Passed=sum(project["l1"] == "pass" for project in projects.values()),
                 l2Passed=sum(project["l2"] == "pass" for project in projects.values()),
                 status=combined_status(project[level] for project in projects.values()
                                        for level in ("l1", "l2")),
                 note="L1 compares configure output. L2 compares Ninja dry-run commands.")


def holdout_counts(row):
    total = count(row.get("reposTotal", row.get("repos_total")))
    passed = count(row.get("reposPassed", row.get("repos_passed")))
    return dict(reposTotal=total, reposPassed=passed, passRate=rate(passed, total),
                status="pass" if passed == total else "fail")


def merge_holdout(track, path):
    # Publish only numeric metrics and reuse category labels from the base.
    try:
        report = read_json(path)
        aggregate = holdout_counts(report.get("aggregate", report))
        categories = report.get("categories", [])
        if isinstance(categories, list):
            categories = {row["category"]: row for row in categories}
        for row in track["categories"]:
            if row["category"] in categories:
                row.update(holdout_counts(categories[row["category"]]))
        track.update(aggregate, note="Repository counts from the blind holdout report.")
    except (AttributeError, KeyError, OSError, TypeError, ValueError):
        raise ValueError("The holdout report must contain valid aggregate and category counts.") from None


def shard_results(path):
    if not path.is_file():
        return Counter()
    counts = Counter()
    with path.open(encoding="utf-8") as stream:
        for line in stream:
            if line.strip():
                counts[json.loads(line)["status"]] += 1
    return counts


def merge_vcpkg(track, directory):
    shards = {row["shard"]: row for row in track["shards"]}
    seen = set()
    for path in report_paths(directory, "shard.json"):
        report = read_json(path)
        index = count(report["shard_index"])
        if count(report["shard_count"]) != track["shardsTotal"] or index not in shards or index in seen:
            raise ValueError("Each vcpkg report must identify a distinct shard in the 64-shard matrix.")
        seen.add(index)
        total = count(report["expected_ports"])
        outcomes = shard_results(path.with_name("results.jsonl"))
        passed = outcomes["pass"]
        failed = outcomes["configure-mismatch"] + outcomes["cmake-identity-mismatch"]
        recorded = outcomes.total()
        rate(recorded, total)
        if not recorded or recorded < total:
            status = "partial"
        else:
            status = "pass" if passed == total else "fail"
        shards[index].update(status=status, portsTotal=total, portsPassed=passed, portsFailed=failed,
                             durationSeconds=report.get("duration_seconds"),
                             note=f"{recorded:,} / {total:,} expected port results recorded.")

    reported = [row for row in shards.values() if row["status"] != "pending"]
    total = sum(row["portsTotal"] for row in reported)
    passed = sum(row["portsPassed"] for row in reported)
    statuses = [row["status"] for row in shards.values()]
    status = "fail" if any(row.get("portsFailed") for row in reported) else combined_status(statuses)
    track.update(shardsCompleted=sum(row["status"] in {"pass", "fail"} for row in reported),
                 shardsPassed=sum(row["status"] == "pass" for row in reported),
                 portsTotal=total, portsPassed=passed, passRate=rate(passed, total),
                 status=status,
                 note="The rate uses the expected port count in the reported shards.")


def merge_ci(snapshot, report):
    runs = report["workflow_runs"] if isinstance(report, dict) else report
    completed = [run for run in runs if run["status"] == "completed"]
    passed = sum(run["conclusion"] == "success" for run in completed)
    snapshot["statusNote"] = (f"CI reports {passed:,} of {len(completed):,} completed runs as successful. "
                              f"Open runs total {len(runs) - len(completed):,}.")


def write_snapshot(path, snapshot):
    payload = json.dumps(snapshot, indent=2, ensure_ascii=False, allow_nan=False) + "\n"
    temporary = None
    try:
        with NamedTemporaryFile(mode="w", encoding="utf-8", dir=path.parent, delete=False) as stream:
            temporary = Path(stream.name)
            stream.write(payload)
        if path.exists():
            temporary.chmod(S_IMODE(path.stat().st_mode))
        temporary.replace(path)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def main(argv=None):
    """Build and validate a dashboard snapshot before writing it."""
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--base", type=Path, default=DATA / "latest.json")
    parser.add_argument("--out", type=Path, default=DATA / "latest.json")
    parser.add_argument("--commit", help="Commit SHA for the verified binary.")
    parser.add_argument("--version", help="Version of the verified Rcmake binary.")
    for name in (
        "runcmake-macos", "runcmake-linux", "runcmake-windows", "independent", "probe-cache",
        "mutation", "determinism", "wasm", "boss-dir", "unseen-holdout", "fuzz-dir", "vcpkg-dir",
        "ci-status-json",
    ):
        parser.add_argument(f"--{name}", type=Path)
    parser.add_argument("--bench", type=Path, action="append", default=[])
    args = parser.parse_args(argv)

    try:
        schema = read_json(DATA / "dashboard.schema.json")
        snapshot = read_json(args.base)
        validate(snapshot, schema, schema)
        snapshot["generatedAt"] = datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")
        if args.commit is not None:
            snapshot["commit"] = args.commit
        if args.version is not None:
            snapshot["rcmakeVersion"] = args.version
        tracks = snapshot["tracks"]
        reports = {
            identifier: read_json(path) for os_name, identifier in PLATFORMS.items()
            if (path := getattr(args, f"runcmake_{os_name}")) is not None
        }
        if reports:
            merge_runcmake(tracks, reports)
        section = tracks["benchmarksAndGates"]
        merge_benchmarks(section["benchmarks"], args.bench)
        merge_soundness(section, args)
        if args.boss_dir:
            merge_boss(tracks["bossFight"], args.boss_dir)
        if args.unseen_holdout:
            merge_holdout(tracks["unseenHoldout"], args.unseen_holdout)
        if args.fuzz_dir:
            merge_fuzz(section, args.fuzz_dir)
        if args.vcpkg_dir:
            merge_vcpkg(tracks["vcpkgMatrix"], args.vcpkg_dir)
        if args.ci_status_json:
            merge_ci(snapshot, read_json(args.ci_status_json))
        validate(snapshot, schema, schema)
        write_snapshot(args.out, snapshot)
    except (AttributeError, KeyError, OSError, TypeError, ValueError) as error:
        parser.exit(2, f"{parser.prog}: {error}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
