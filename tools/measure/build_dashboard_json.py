#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Merge verification reports into the public compatibility dashboard.

Use RunCMake evaluator reports for each OS. The core and policy tracks use
the matching suites from those reports. Use independent_verify.py,
probe_cache_eval.py, mutation_eval.py, path_swap_eval.py, and wasm_smoke.py
JSON for their respective gates. Repeat --bench to combine reports from
bench_configure.py and bench_eval.py.

Use a large_projects_local.py or ecosystem workflow result.json directory for
--large-projects-dir (L1 configure, L2 dry-run, L3 build, and L4 test/install).
Use a bench_giants.py JSON report for --giants-benchmark.
Use downloaded 64-shard workflow artifacts with shard.json and results.jsonl
for --vcpkg-dir.
For --fuzz-dir, supply JSON files with iterations and failures per run.
Failures can be a count or a list. For --differential-fuzz, supply one or more
oracle_prop_test.py JSON reports (or a directory of reports) with total, counts,
and cases. For --unseen-holdout / --holdout, supply either unseen_eval.py JSON
(holdout.pass_rates and holdout.failure_categories) or an aggregate/categories
report. For --ci-status-json, use the output of gh run list --json
status,conclusion,headSha or the Actions REST response.

For --strict-same-sha, each measurement report must record its release commit.
If you set --binary-sha256, each measurement report must also record its digest.
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
    from .runcmake_eval import SUITE_GROUPS, strict_case_issue
else:
    from runcmake_eval import SUITE_GROUPS, strict_case_issue


DATA = Path(__file__).resolve().parents[2] / "site" / "data"
PLATFORMS = {
    "macos": "macos-arm64",
    "linux": "linux-x86_64",
    "windows": "windows-x86_64",
}
EXPECTED_OS_SUITES = {
    "macos-arm64": 371,
    "linux-x86_64": 362,
    "windows-x86_64": 367,
}
# Distinct CMP0066+ suites in the union of the CMake 4.5.1 OS profiles.
EXPECTED_POLICY_SUITES = 35
RUNCMAKE_MIN_PASS_RATE = 99.0
HOLDOUT_MIN_REPOS = 240
HOLDOUT_MIN_PASS_RATE = 95.0
SHA_HEX = re.compile(r"^[0-9a-fA-F]{7,40}$")
SHA256_HEX = re.compile(r"^[0-9a-fA-F]{64}$")
COLD_ISOLATION_NOTE = "empty probe cache"
DRAFT_TOKEN = "[FINAL_METRIC]"
# Draft text from older snapshots. An empty replacement removes a note.
FUZZ_LABEL = "Seeded generative fuzzing"
FUZZ_DETAIL = ("Seeded generative inputs (not coverage-guided) for 14 parser and format targets. "
               "Each counted input ran with no panic, timeout, or limit violation.")
DRAFT_TEXT = {
    "10 Boss-Fight Giants": "Large projects",
    "Measured across real-world corpus on idle hardware.": "",
    "Adversarial fuzzing": FUZZ_LABEL,
    "Safe-Rust property and adversarial corpus across 14 parser and format surfaces "
    "with zero panics or crashes.": FUZZ_DETAIL,
}
DIFF_FUZZ_STATUSES = frozenset({"pass", "fail", "oracle_error"})
HOLDOUT_LEVELS = ("L1", "L2", "L3", "L4")
HOLDOUT_FAILURE_CATEGORIES = frozenset({
    "build", "configure", "crash", "evaluation", "find-module", "generator",
    "genex", "oracle-red", "other", "policy", "source", "timeout",
    "unknown-command",
})
LARGE_PROJECT_STATES = {
    "equal": "pass",
    "pass": "pass",
    "different": "fail",
    "fail": "fail",
    "oracle_error": "fail",
    "oracle-red": "fail",
    "error": "fail",
    "incomplete": "partial",
    "partial": "partial",
    "pending": "pending",
    "not_requested": "pending",
    "not-requested": "pending",
    "skipped": "pending",
    "skip": "pending",
}
REPORT_SHA_KEYS = (
    "release_commit", "commit", "git_sha", "release_sha", "releaseSha", "rcmake_sha", "rcmake_commit",
)
REPORT_DIGEST_KEYS = ("binary_sha256", "binarySha256", "rcmake_sha256", "rcmakeBinarySha256")
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
    if isinstance(value, str) and "pattern" in rule and not re.search(rule["pattern"], value):
        raise ValueError(f"{field} must match the schema pattern.")
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
        extra_properties = value.keys() - properties.keys()
        additional_rule = rule.get("additionalProperties")
        if additional_rule is False and extra_properties:
            raise ValueError(f"{field} contains fields outside the schema.")
        if isinstance(additional_rule, dict):
            for key in extra_properties:
                validate(value[key], additional_rule, root, f"{field}.{key}")
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


def sha_matches(left, right):
    left, right = left.strip().lower(), right.strip().lower()
    if left == right:
        return True
    return min(len(left), len(right)) >= 7 and (left.startswith(right) or right.startswith(left))


def report_identity_values(report, keys):
    if not isinstance(report, dict):
        return []
    containers = [report, report.get("receipt"), report.get("meta")]
    return [
        container[key].strip().lower()
        for container in containers if isinstance(container, dict)
        for key in keys
        if isinstance(container.get(key), str) and container[key].strip()
    ]


def verify_report_identity(report, expected_sha, expected_digest, strict, source):
    if expected_sha is None and expected_digest is None:
        return
    if expected_sha is not None:
        commits = report_identity_values(report, REPORT_SHA_KEYS)
        if strict and not commits:
            raise ValueError(f"{source} is missing a release commit.")
        for value in commits:
            matches = value == expected_sha.lower() if strict else sha_matches(value, expected_sha)
            if not matches:
                raise ValueError(f"{source} reports commit {value!r}, expected {expected_sha!r}.")
    if expected_digest is not None:
        digests = report_identity_values(report, REPORT_DIGEST_KEYS)
        if strict and not digests:
            raise ValueError(f"{source} is missing a binary SHA-256 digest.")
        for value in digests:
            if value != expected_digest.lower():
                raise ValueError(f"{source} reports binary SHA-256 {value!r}, expected {expected_digest!r}.")


def read_report(path, identity=(None, None, False)):
    report = read_json(path)
    verify_report_identity(report, *identity, str(path))
    return report


def validate_sha(value, flag_name):
    cleaned = value.strip()
    if not cleaned or not SHA_HEX.fullmatch(cleaned):
        raise ValueError(f"{flag_name} must be a 7 to 40 character hexadecimal commit SHA.")
    return cleaned


def validate_sha256(value):
    cleaned = value.strip().lower()
    if not SHA256_HEX.fullmatch(cleaned):
        raise ValueError("--binary-sha256 must be a 64-character hexadecimal SHA-256 digest.")
    return cleaned


def clean_draft_text(node, key=None):
    """Replace draft labels and remove draft tokens from a snapshot.

    A base snapshot can hold placeholder text such as "[FINAL_METRIC]". The
    published file must not show it. A note that becomes empty is removed.
    """
    if isinstance(node, dict):
        cleaned = {name: clean_draft_text(value, name) for name, value in node.items()}
        return {name: value for name, value in cleaned.items()
                if not (name == "note" and value == "")}
    if isinstance(node, list):
        return [clean_draft_text(value, key) for value in node]
    if isinstance(node, str):
        text = node
        if DRAFT_TOKEN in text:
            text = " ".join(text.replace(DRAFT_TOKEN, " ").split())
        return DRAFT_TEXT.get(text, text)
    return node


def reset_tracks_to_pending(snapshot):
    pending_note = "Pending verification for this commit."
    snapshot.pop("binarySha256", None)
    snapshot.pop("releaseSha", None)
    snapshot.pop("receipt", None)
    snapshot.pop("knownParityGaps", None)
    snapshot["statusNote"] = "Verification metrics reset to pending for the updated commit."
    tracks = snapshot["tracks"]

    core = tracks["coreConformance"]
    core.update(status="pending", suitesAt100=None, casesPassed=None, casesTotal=None,
                passRate=None, note=pending_note)
    for highlight in core["highlights"]:
        highlight.update(passed=None, total=None, status="pending")
        highlight.pop("note", None)

    policies = tracks["policies"]
    policies.update(status="pending", suitesTotal=EXPECTED_POLICY_SUITES, suitesPassed=None,
                    casesPassed=None, casesTotal=None, passRate=None, note=pending_note)

    run_cmake = tracks["runCmakeOs"]
    run_cmake.update(status="pending", note=pending_note)
    for platform in run_cmake["platforms"]:
        expected = EXPECTED_OS_SUITES.get(platform["id"], platform["suitesTotal"])
        platform.update(suitesTotal=expected, suitesPassed=None, casesPassed=None,
                        casesTotal=None, passRate=None, status="pending", note=pending_note)
        for key in ("partial", "expectedSuites", "completedSuites"):
            platform.pop(key, None)

    boss = tracks["bossFight"]
    boss.update(status="pending", projectsTotal=len(boss["projects"]), l1Passed=None,
                l2Passed=None, note=pending_note)
    boss.pop("l3Passed", None)
    boss.pop("l4Passed", None)
    for project in boss["projects"]:
        project.update(l1="pending", l2="pending")
        for key in ("l3", "l4", "note"):
            project.pop(key, None)

    holdout = tracks["unseenHoldout"]
    holdout.update(sealed=True, status="pending", reposPassed=None, passRate=None, note=pending_note)
    holdout.pop("levelPassRates", None)
    holdout.pop("failureCategories", None)
    for row in holdout["categories"]:
        row.update(reposTotal=None, reposPassed=None, passRate=None, status="pending")

    vcpkg = tracks["vcpkgMatrix"]
    vcpkg.update(status="pending", shardsCompleted=0, shardsPassed=None, portsTotal=None,
                 portsPassed=None, passRate=None, note=pending_note)
    for shard in vcpkg["shards"]:
        shard.update(status="pending", portsTotal=None, portsPassed=None, portsFailed=None,
                     durationSeconds=None, note=pending_note)
    for row in (vcpkg, *vcpkg["shards"]):
        row.pop("portsInspected", None)
        row.pop("portsExpected", None)

    bench_section = tracks["benchmarksAndGates"]
    for metric in bench_section["benchmarks"].values():
        metric.update(value=None, status="pending", note=pending_note)
    for row in bench_section["gates"]:
        row.update(status="pending", passed=None, failures=None, summary=pending_note)
        for key in ("preFreeze", "commit", "measuredAt"):
            row.pop(key, None)
        if "scenariosPassed" in row:
            row["scenariosPassed"] = None
    bench_section.pop("giantsBenchmark", None)
    bench_section.pop("realGiants", None)
    bench_section.pop("differentialFuzz", None)


def resolve_release_identity(snapshot, args):
    if args.strict_same_sha and args.allow_stale_base:
        raise ValueError("--strict-same-sha cannot be combined with --allow-stale-base.")
    commit_arg = args.commit.strip() if args.commit is not None else None
    if args.commit is not None and not commit_arg:
        raise ValueError("--commit must not be empty.")
    release_sha = validate_sha(args.release_sha, "--release-sha") if args.release_sha is not None else None
    if commit_arg is not None and release_sha is not None and not sha_matches(commit_arg, release_sha):
        raise ValueError("--commit and --release-sha must match when both are provided.")
    requested_sha = release_sha or commit_arg
    requested_digest = validate_sha256(args.binary_sha256) if args.binary_sha256 is not None else None

    base_sha = snapshot.get("commit") or snapshot.get("releaseSha")
    base_digest = snapshot.get("binarySha256")
    effective_sha = requested_sha or base_sha
    effective_digest = requested_digest or base_digest

    if args.strict_same_sha and not effective_sha:
        raise ValueError("--strict-same-sha requires --release-sha or --commit.")
    if args.strict_same_sha:
        validate_sha(effective_sha, "--strict-same-sha commit")

    sha_differs = requested_sha is not None and (
        base_sha is None or (
            base_sha.lower() != requested_sha.lower() if args.strict_same_sha
            else not sha_matches(base_sha, requested_sha)
        )
    )
    digest_differs = requested_digest is not None and (
        base_digest is not None and base_digest.lower() != requested_digest.lower()
    )
    strict_unverified_digest = (
        args.strict_same_sha and requested_digest is not None and base_digest is None
    )

    if (sha_differs or digest_differs or strict_unverified_digest) and not args.allow_stale_base:
        reset_tracks_to_pending(snapshot)
        effective_digest = requested_digest
    elif sha_differs:
        if requested_digest is None:
            snapshot.pop("binarySha256", None)
            effective_digest = None
        if release_sha is None:
            snapshot.pop("releaseSha", None)

    if requested_sha is not None:
        snapshot["commit"] = requested_sha
    if release_sha is not None:
        snapshot["releaseSha"] = release_sha
    if requested_digest is not None:
        snapshot["binarySha256"] = requested_digest

    return effective_sha, effective_digest, args.strict_same_sha


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


def suite_metrics(name, row):
    passed, total = count(row["passed"]), count(row["total"])
    drivers_ok = all(
        driver.get("complete") is True and not driver.get("driver_errors")
        for key in ("oracle_driver", "rcmake_driver")
        if (driver := row.get(key)) is not None
    )
    rate(passed, total)
    gates_ok = drivers_ok and strict_case_issue(name, row) is None
    passed_suite = 0 < passed == total and gates_ok
    status = "pass" if passed_suite else "fail"
    return passed, total, status, gates_ok


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
        suites = {name: suite_metrics(name, row) for name, row in report["suites"].items()}
        if not suites:
            raise ValueError("A RunCMake report must contain at least one suite.")
        for name, result in suites.items():
            observations[name].append(result)
        results = list(suites.values())
        statuses = [row[2] for row in results]
        totals = suite_totals(results)
        expected_suites = EXPECTED_OS_SUITES[platform["id"]]
        reported_suites = len(results)
        driver_complete_suites = sum(
            1
            for row in report["suites"].values()
            if all(
                driver.get("complete") is True and not driver.get("driver_errors")
                for key in ("oracle_driver", "rcmake_driver")
                if (driver := row.get(key)) is not None
            )
        )
        incomplete_count = reported_suites - driver_complete_suites
        is_partial = reported_suites < expected_suites
        all_gates_ok = all(row[3] for row in results)
        if is_partial:
            platform_status = "fail" if "fail" in statuses else "partial"
            if incomplete_count > 0:
                note = (
                    f"Partial OS report: {reported_suites:,} of {expected_suites:,} expected suites "
                    f"reported ({driver_complete_suites:,} complete, {incomplete_count:,} incomplete)."
                )
            else:
                note = f"Partial OS report: {reported_suites:,} of {expected_suites:,} expected suites completed."
        elif all_gates_ok and totals["passRate"] is not None and totals["passRate"] >= RUNCMAKE_MIN_PASS_RATE:
            platform_status = "pass"
            note = f"Full OS report: {driver_complete_suites:,} suites completed (minimum {expected_suites:,})."
        else:
            platform_status = "fail"
            note = f"Full OS report: {driver_complete_suites:,} suites completed (minimum {expected_suites:,})."
        platform.update(
            totals,
            suitesTotal=max(expected_suites, reported_suites),
            suitesPassed=statuses.count("pass"),
            expectedSuites=expected_suites,
            completedSuites=driver_complete_suites,
            reportedSuites=reported_suites,
            partial=is_partial,
            status=platform_status,
            note=note,
        )
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
    for section, rows, minimum, pass_key in (
        (core, core_rows, core["suitesTotal"], "suitesAt100"),
        (policies, policy_rows, EXPECTED_POLICY_SUITES, "suitesPassed"),
    ):
        if rows:
            statuses = [row[2] for row in rows]
            expected = max(minimum, len(rows))
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


def gate(section, identifier, label, passed, total, status, detail, summary=None, **metrics):
    rate(passed, total)
    row = next((row for row in section["gates"] if row["id"] == identifier), None)
    if row is None:
        row = {"id": identifier, "label": label}
        section["gates"].append(row)
    for key in ("preFreeze", "commit", "measuredAt"):
        row.pop(key, None)
    row.update(status=status, passed=passed, total=total, failures=total - passed,
               summary=summary or f"{passed:,} / {total:,} checks passed", detail=detail, **metrics)


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


def merge_soundness(section, args, identity=(None, None, False)):
    if args.independent:
        merge_independent(section, read_report(args.independent, identity))
    if args.probe_cache:
        report = read_report(args.probe_cache, identity)
        cases = [row for row in report["cases"] if row["status"] != "skipped"]
        passed = sum(flag(row["passed"]) for row in cases)
        status = "fail" if not flag(report["passed"]) else (
            "partial" if report.get("mode") == "smoke" or len(cases) < len(report["cases"]) else "pass"
        )
        gate(section, "probe-cache", "Persistent Probe Cache", passed, len(cases), status,
             "Counts include the compiler probe cache checks that ran.")
    if args.mutation:
        report = read_report(args.mutation, identity)
        steps = [row for row in report["steps"] if row["step"] > 0]
        status = "fail" if not flag(report["passed"]) else (
            "pass" if report["replay_verified"] else "partial"
        )
        gate(section, "mutation-replay", "Mutation Replay Soundness",
             sum(flag(row.get("passed", False)) for row in steps), len(steps), status,
             "Counts include the edit comparisons and exclude the initial configure.")
    if args.determinism:
        report = read_report(args.determinism, identity)
        passed = int(report["status"] == "pass")
        gate(section, "determinism", "Path-Swap Determinism", passed, 1,
             "pass" if passed else "fail", "Compares output from isolated runs at the same project path.")
    if args.path_swap:
        report = read_report(args.path_swap, identity)
        comparisons = [report]
        if isinstance(report.get("fixture"), dict):
            comparisons.append(report["fixture"])
        passed = int(all(row.get("status") == "pass" and row.get("differences") in (None, [])
                         for row in comparisons))
        gate(section, "path-swap", "Silent PATH Swap", passed, 1,
             "pass" if passed else "fail", "Compares artifacts after a silent PATH swap.")
    if args.wasm:
        report = read_report(args.wasm, identity)
        checks = [row for row in report["checks"] if row["status"] != "planned"]
        status = {"pass": "pass", "fail": "fail", "incomplete": "partial", "planned": "pending"}
        gate(section, "wasi", "WebAssembly & WASI Gate",
             sum(row["status"] == "pass" for row in checks), len(checks), status[report["status"]],
             "Checks WebAssembly dependencies, compilation, and WASI execution.")


def merge_conan(section, report):
    if isinstance(report.get("scenarios"), list):
        states = Counter(row["status"] for row in report["scenarios"])
        total = states.total() - states["skipped_missing_tool"]
        passed = states["pass"]
    else:
        passed, total = count(report["passed"]), count(report["total"])
    rate(passed, total)
    status = "pass" if report["status"] == "pass" and 0 < passed == total else "fail"
    gate(section, "conan-ecosystem", "Conan Ecosystem", passed, total, status,
         "Counts include the ecosystem scenarios that ran.")


def merge_dist(snapshot, report):
    binary_size = count(report["binary_size_bytes"])
    if not binary_size:
        raise ValueError("The distribution report must record a positive binary size.")
    snapshot.setdefault("receipt", {}).update(
        commit=snapshot.get("commit"),
        binarySha256=snapshot.get("binarySha256"),
        binarySizeBytes=binary_size,
    )


def merge_benchmarks(metrics, paths, identity=(None, None, False)):
    rows = [row for path in paths for row in read_report(path, identity)["results"]]

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
        projects = ", ".join(sorted({str(row["project"]) for row in selected}))
        record("warmProbeSpeedup", speedup, status,
               f"Geometric mean across {projects}. Both tools configure a new build tree. "
               "rcmake uses its shared probe cache. CMake 4.5.1 has no cache across build trees.")

    incremental = [row for row in rows if row.get("case") == "incremental-leaf"]
    if incremental:
        samples = [row["rcmake"]["p95_ms"] for row in incremental]
        value = None if None in samples else max(samples)
        record("incrementalLeafP95Ms", value, benchmark_status(incremental, value),
               "Highest project p95 in the report.")

    for row in rows:
        if row.get("name") == "configure 500 targets (cold)":
            if COLD_ISOLATION_NOTE not in str(row.get("notes") or ""):
                raise ValueError("The cold 500-target row must come from a bench_eval.py run "
                                 f"that records '{COLD_ISOLATION_NOTE}'.")
            speedup = row["speedup"]
            rss = row["rcmake"]["peak_rss_mib"]
            record("coldConfigureSpeedup", speedup, "pass" if speedup is not None else "fail",
                   "Median cold configure of a synthetic C project with 500 static libraries. "
                   "New build tree and empty probe cache for each run.")
            record("peakRssMiB", rss, "pass" if rss is not None and row["rcmake"]["ok"] else "fail",
                   "Highest resident memory use during the synthetic 500-library cold configure.")


def benchmark_status(rows, value):
    if value is None or any(row.get("gate", {}).get("status") in {"fail", "unavailable"} for row in rows):
        return "fail"
    return "pass"


def optional_number(value, *, positive=False):
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ValueError("Benchmark metric values must be numbers or null.")
    if not math.isfinite(value):
        raise ValueError("Benchmark metric values must be finite.")
    if positive and value <= 0:
        raise ValueError("Positive benchmark metric values must exceed zero.")
    if not positive and value < 0:
        raise ValueError("Benchmark metric values must be nonnegative.")
    return round(float(value), 2)


def merge_giants_bench(section, path, identity=(None, None, False)):
    report = read_report(path, identity)
    if report.get("schema_version") != 1:
        raise ValueError("Giants benchmark report must use schema_version 1.")
    raw_projects = report.get("projects")
    if not isinstance(raw_projects, list) or not raw_projects:
        raise ValueError("Giants benchmark report must include a non-empty projects list.")
    meta = report.get("meta") or {}
    runs = count(meta.get("runs", 0))
    if runs < 1:
        raise ValueError("Giants benchmark report must record a positive run count.")
    reproducer = report.get("reproducer")
    if not isinstance(reproducer, str) or not reproducer.strip():
        raise ValueError("Giants benchmark report must include a reproducer command.")

    status_map = {"pass": "pass", "fail": "fail", "unavailable": "pending"}
    projects_out = []
    cold_ratios = []
    warm_ratios = []
    inc_ratios = []
    inc_p95s = []
    rcmake_rss = []
    cmake_rss = []

    for item in raw_projects:
        name = item["name"]
        raw_status = item["status"]
        if raw_status not in status_map:
            raise ValueError(f"Unknown giants benchmark status {raw_status!r} for {name}.")
        mapped_status = status_map[raw_status]
        entry = {
            "name": name,
            "repository": item.get("repository", ""),
            "version": item.get("version", ""),
            "status": mapped_status,
        }
        if mapped_status == "pending":
            if item.get("unavailable_reason"):
                entry["note"] = str(item["unavailable_reason"])
            projects_out.append(entry)
            continue

        cold = item["cold"]
        warm = item["warm_probe"]
        inc = item["incremental_leaf"]
        cold_out = {
            "cmakeMedianMs": optional_number(cold["cmake"].get("median_ms")),
            "rcmakeMedianMs": optional_number(cold["rcmake"].get("median_ms")),
            "speedup": optional_number(cold.get("speedup"), positive=True),
            "cmakePeakRssMiB": optional_number(cold["cmake"].get("peak_rss_mib")),
            "rcmakePeakRssMiB": optional_number(cold["rcmake"].get("peak_rss_mib")),
        }
        probe = warm.get("probe_cache") or {}
        warm_out = {
            "cmakeMedianMs": optional_number(warm["cmake"].get("median_ms")),
            "rcmakeMedianMs": optional_number(warm["rcmake"].get("median_ms")),
            "speedup": optional_number(warm.get("speedup"), positive=True),
            "cmakePeakRssMiB": optional_number(warm["cmake"].get("peak_rss_mib")),
            "rcmakePeakRssMiB": optional_number(warm["rcmake"].get("peak_rss_mib")),
            "probeCacheHits": count(probe["hits"]) if probe.get("hits") is not None else None,
            "probeCacheMisses": count(probe["misses"]) if probe.get("misses") is not None else None,
        }
        inc_out = {
            "cmakeMedianMs": optional_number(inc["cmake"].get("median_ms")),
            "rcmakeMedianMs": optional_number(inc["rcmake"].get("median_ms")),
            "rcmakeP95Ms": optional_number(inc["rcmake"].get("p95_ms")),
            "speedup": optional_number(inc.get("speedup"), positive=True),
            "cmakePeakRssMiB": optional_number(inc["cmake"].get("peak_rss_mib")),
            "rcmakePeakRssMiB": optional_number(inc["rcmake"].get("peak_rss_mib")),
            "leafFile": inc.get("leaf_file"),
        }
        entry.update(cold=cold_out, warmProbe=warm_out, incrementalLeaf=inc_out)

        bv = item.get("build_verify")
        if isinstance(bv, dict):
            dry = bv.get("dry_run") or {}
            comp = bv.get("compile") or {}
            entry["buildVerify"] = {
                "status": status_map.get(bv.get("status", "fail"), "fail"),
                "dryRunStepCountMatch": bool(dry.get("step_count_match")) if dry else None,
                "target": comp.get("target") if comp else None,
                "cmakeBuildMs": optional_number((comp.get("cmake") or {}).get("wall_ms")) if comp else None,
                "rcmakeBuildMs": optional_number((comp.get("rcmake") or {}).get("wall_ms")) if comp else None,
                "buildSpeedup": optional_number(comp.get("build_speedup"), positive=True) if comp else None,
            }

        for value, bucket in (
            (cold_out["speedup"], cold_ratios),
            (warm_out["speedup"], warm_ratios),
            (inc_out["speedup"], inc_ratios),
        ):
            if value is not None:
                bucket.append(value)
        if inc_out["rcmakeP95Ms"] is not None:
            inc_p95s.append(inc_out["rcmakeP95Ms"])
        for phase_out in (cold_out, warm_out, inc_out):
            if phase_out["rcmakePeakRssMiB"] is not None:
                rcmake_rss.append(phase_out["rcmakePeakRssMiB"])
            if phase_out["cmakePeakRssMiB"] is not None:
                cmake_rss.append(phase_out["cmakePeakRssMiB"])
        projects_out.append(entry)

    def geomean(values):
        return round(geometric_mean(values), 2) if values else None

    section["giantsBenchmark"] = {
        "label": "Real-World Giants Configure Benchmark",
        "status": combined_status(row["status"] for row in projects_out),
        "runs": runs,
        "reproducer": reproducer,
        "generatedAt": report.get("generated_at"),
        "summary": {
            "projectsTotal": len(projects_out),
            "projectsPassed": sum(row["status"] == "pass" for row in projects_out),
            "coldSpeedupGeomean": geomean(cold_ratios),
            "warmProbeSpeedupGeomean": geomean(warm_ratios),
            "incrementalLeafSpeedupGeomean": geomean(inc_ratios),
            "incrementalLeafMaxP95Ms": max(inc_p95s) if inc_p95s else None,
            "rcmakeMaxPeakRssMiB": max(rcmake_rss) if rcmake_rss else None,
            "cmakeMaxPeakRssMiB": max(cmake_rss) if cmake_rss else None,
        },
        "projects": projects_out,
    }


def report_paths(directory, pattern):
    paths = sorted(path for path in directory.rglob(pattern) if not path.name.startswith("._"))
    if not paths:
        raise ValueError(f"No {pattern} reports in the supplied directory.")
    return paths


def merge_fuzz(section, directory, identity=(None, None, False)):
    total = failures = 0
    for path in report_paths(directory, "*.json"):
        report = read_report(path, identity)
        failed = report["failures"]
        failed = len(failed) if isinstance(failed, list) else count(failed)
        iterations = count(report["iterations"])
        rate(iterations - failed, iterations)
        total += iterations
        failures += failed
    status = "fail" if failures else "pass" if total else "partial"
    gate(section, "fuzz", FUZZ_LABEL, total - failures, total, status, FUZZ_DETAIL,
         summary=f"{total:,} inputs, {failures:,} failures")


def differential_fuzz_paths(inputs):
    paths = []
    for item in inputs:
        if item.is_dir():
            found = sorted(
                path for path in item.rglob("*.json")
                if not path.name.startswith("._") and path.name != "outcomes.json"
            )
            if not found:
                raise ValueError("No differential fuzz reports in the supplied directory.")
            paths.extend(found)
        else:
            paths.append(item)
    return paths


def merge_differential_fuzz(section, inputs, identity=(None, None, False)):
    total = passed = mismatches = oracle_errors = 0
    script_cases = configure_cases = 0
    saw_cases = False
    families = set()
    seeds = []
    reports = differential_fuzz_paths(inputs)
    for path in reports:
        report = read_report(path, identity)
        if not isinstance(report, dict) or ("counts" not in report and "cases" not in report):
            raise ValueError("Differential fuzz reports must include counts or cases.")
        raw_cases = report.get("cases")
        if raw_cases is not None and not isinstance(raw_cases, list):
            raise ValueError("Differential fuzz cases must be a list.")

        case_counts = Counter()
        if raw_cases:
            saw_cases = True
            for case in raw_cases:
                if not isinstance(case, dict):
                    raise ValueError("Each differential fuzz case must be an object.")
                status_value = case.get("status")
                if status_value not in DIFF_FUZZ_STATUSES:
                    raise ValueError(f"Unknown differential fuzz case status {status_value!r}.")
                case_counts[status_value] += 1
                mode = case.get("mode")
                if mode == "script":
                    script_cases += 1
                elif mode == "configure":
                    configure_cases += 1
                family = case.get("family")
                if isinstance(family, str) and family:
                    families.add(family)

        raw_counts = report.get("counts")
        if raw_counts is not None:
            if not isinstance(raw_counts, dict):
                raise ValueError("Differential fuzz counts must be an object.")
            unknown = set(raw_counts) - DIFF_FUZZ_STATUSES
            if unknown:
                raise ValueError(f"Unknown differential fuzz count keys: {', '.join(sorted(unknown))}.")
            report_passed = count(raw_counts.get("pass", 0))
            report_mismatches = count(raw_counts.get("fail", 0))
            report_oracle_errors = count(raw_counts.get("oracle_error", 0))
            if raw_cases and (
                case_counts["pass"] != report_passed
                or case_counts["fail"] != report_mismatches
                or case_counts["oracle_error"] != report_oracle_errors
            ):
                raise ValueError("Differential fuzz cases do not match counts.")
        else:
            report_passed = case_counts["pass"]
            report_mismatches = case_counts["fail"]
            report_oracle_errors = case_counts["oracle_error"]

        summed = report_passed + report_mismatches + report_oracle_errors
        report_total = count(report["total"]) if "total" in report else summed
        if report_total < 1:
            raise ValueError("Differential fuzz report must contain at least one case.")
        if summed != report_total:
            raise ValueError("Differential fuzz total must equal pass, fail, and oracle_error counts.")

        if "seed" in report and report["seed"] is not None:
            seed = report["seed"]
            if type(seed) is not int:
                raise ValueError("Differential fuzz seed must be an integer.")
            if seed not in seeds:
                seeds.append(seed)

        total += report_total
        passed += report_passed
        mismatches += report_mismatches
        oracle_errors += report_oracle_errors

    status = "fail" if (mismatches or oracle_errors) else "pass"
    entry = {
        "label": "Oracle Differential Fuzzing",
        "status": status,
        "casesTotal": total,
        "casesPassed": passed,
        "mismatches": mismatches,
        "oracleErrors": oracle_errors,
        "passRate": rate(passed, total),
        "reportsCount": len(reports),
        "seeds": seeds,
        "summary": (
            f"{passed:,} / {total:,} differential cases passed "
            f"({mismatches:,} mismatches, {oracle_errors:,} oracle errors)"
        ),
        "note": "Seeded CMake script and configure differential runs against CMake 4.5.1.",
    }
    if saw_cases:
        entry["scriptCases"] = script_cases
        entry["configureCases"] = configure_cases
        entry["families"] = len(families)
    section["differentialFuzz"] = entry


def map_large_project_state(value):
    if isinstance(value, bool):
        return "pass" if value else "fail"
    if isinstance(value, str):
        key = value.strip().lower()
        if key in LARGE_PROJECT_STATES:
            return LARGE_PROJECT_STATES[key]
    raise ValueError(f"Unknown large-project level state {value!r}.")


def collect_large_project_states(raw):
    if isinstance(raw, (bool, str)):
        return [map_large_project_state(raw)]
    if isinstance(raw, dict):
        if not raw:
            return []
        if "status" in raw or "checks" in raw:
            states = []
            if raw.get("status") is not None:
                states.append(map_large_project_state(raw["status"]))
            checks = raw.get("checks")
            if isinstance(checks, dict):
                for item in checks.values():
                    states.extend(collect_large_project_states(item))
            elif isinstance(checks, list):
                for item in checks:
                    states.extend(collect_large_project_states(item))
            elif checks is not None:
                states.extend(collect_large_project_states(checks))
            return states
        states = []
        for item in raw.values():
            states.extend(collect_large_project_states(item))
        return states
    if isinstance(raw, list):
        states = []
        for item in raw:
            states.extend(collect_large_project_states(item))
        return states
    raise ValueError(f"Invalid large-project level value {raw!r}.")


def large_project_level(values):
    mapped = []
    for item in values:
        mapped.extend(collect_large_project_states(item))
    if not mapped:
        return "partial"
    statuses = set(mapped)
    if "fail" in statuses:
        return "fail"
    if statuses == {"pass"}:
        return "pass"
    if statuses == {"pending"}:
        return "pending"
    return "partial"


def extract_large_project_level(result, level_key, *, lane_keys=(), default="partial"):
    levels = result.get("levels") if isinstance(result.get("levels"), dict) else {}
    for container in (result, levels):
        for candidate in (level_key, level_key.upper()):
            if candidate in container and container[candidate] is not None:
                return large_project_level([container[candidate]])
    lanes = result.get("lanes")
    if lane_keys and isinstance(lanes, dict) and lanes:
        observed = [
            lane[key]
            for lane in lanes.values()
            if isinstance(lane, dict)
            for key in lane_keys
            if key in lane and lane[key] is not None
        ]
        if observed:
            return large_project_level(observed)
    return default


def large_project_name(result):
    name = result.get("project") or result.get("name")
    if not isinstance(name, str) or not name:
        raise ValueError("Large-project result must include a project name.")
    return name


def merge_large_projects(track, directory, identity=(None, None, False)):
    projects = {row["name"]: row for row in track["projects"]}
    # A local run stores the same project results in the summary and per-project files.
    summaries = {}
    individuals = {}
    for path in report_paths(directory, "result.json"):
        report = read_report(path, identity)
        if "projects" in report:
            raw_projects = report["projects"]
            if isinstance(raw_projects, dict):
                for name, result in raw_projects.items():
                    summaries[name] = {**result, "project": result.get("project", name)}
            elif isinstance(raw_projects, list):
                for result in raw_projects:
                    summaries[large_project_name(result)] = result
            else:
                raise ValueError("Large-project summary projects field must be a list or object.")
        else:
            individuals[large_project_name(report)] = report
    results = {**summaries, **individuals}
    if not results or results.keys() - projects.keys():
        raise ValueError("Large-project reports must match the published project list.")
    for name, result in results.items():
        l1 = extract_large_project_level(result, "l1", default="partial")
        l2 = extract_large_project_level(result, "l2", default="partial")
        l3 = extract_large_project_level(result, "l3", lane_keys=("build",), default="pending")
        l4 = extract_large_project_level(
            result, "l4", lane_keys=("test", "manifest_l4", "install"), default="pending"
        )
        failed_run = result.get("status") in {"fail", "oracle_error", "oracle-red", "error"} or bool(
            result.get("error")
        )
        if failed_run:
            l1 = "fail" if l1 == "partial" else l1
            l2 = "fail" if l2 == "partial" else l2
            l3 = "fail" if l3 == "partial" else l3
            l4 = "fail" if l4 == "partial" else l4
        projects[name].update(l1=l1, l2=l2, l3=l3, l4=l4)
        projects[name].pop("note", None)

    l3_reported = any(project.get("l3", "pending") != "pending" for project in projects.values())
    l4_reported = any(project.get("l4", "pending") != "pending" for project in projects.values())
    checked_levels = ["l1", "l2"]
    if l3_reported:
        checked_levels.append("l3")
    if l4_reported:
        checked_levels.append("l4")
    track.update(
        projectsTotal=len(projects),
        l1Passed=sum(project["l1"] == "pass" for project in projects.values()),
        l2Passed=sum(project["l2"] == "pass" for project in projects.values()),
        l3Passed=sum(project.get("l3") == "pass" for project in projects.values()) if l3_reported else None,
        l4Passed=sum(project.get("l4") == "pass" for project in projects.values()) if l4_reported else None,
        status=combined_status(
            project.get(level, "pending")
            for project in projects.values()
            for level in checked_levels
        ),
        note="L1 compares configure output. L2 compares Ninja dry-run commands. L3 builds targets. L4 runs tests and install checks.",
    )


def holdout_counts(row):
    total = count(row.get("reposTotal", row.get("repos_total")))
    passed = count(row.get("reposPassed", row.get("repos_passed")))
    return dict(reposTotal=total, reposPassed=passed, passRate=rate(passed, total),
                status="pass" if total > 0 and passed == total else "fail")


def parse_holdout_pass_rates(raw_rates):
    if not isinstance(raw_rates, dict) or not raw_rates:
        raise ValueError("Holdout pass_rates must be a non-empty object.")
    unknown = set(raw_rates) - set(HOLDOUT_LEVELS)
    if unknown:
        raise ValueError("Holdout pass_rates contains unsupported level keys.")
    level_rates = {}
    ordered = [level for level in HOLDOUT_LEVELS if level in raw_rates]
    for level in ordered:
        stats = raw_rates[level]
        if not isinstance(stats, dict):
            raise ValueError("Each holdout pass_rates entry must be an object.")
        passed = count(stats["passed"])
        eligible = count(stats["eligible"])
        level_rates[level] = {
            "passed": passed,
            "eligible": eligible,
            "passRate": rate(passed, eligible),
        }
    failed_total = sum(row["eligible"] - row["passed"] for row in level_rates.values())
    deepest_passed = level_rates[ordered[-1]]["passed"]
    repos_total = deepest_passed + failed_total
    aggregate = {
        "reposTotal": repos_total,
        "reposPassed": deepest_passed,
        "passRate": rate(deepest_passed, repos_total),
        "status": "pass" if repos_total > 0 and deepest_passed == repos_total else "fail",
    }
    return aggregate, level_rates


def sanitize_holdout_failure_categories(raw_failures):
    if not isinstance(raw_failures, dict):
        raise ValueError("Holdout failure_categories must be an object.")
    cleaned = Counter()
    for raw_key, raw_count in raw_failures.items():
        if not isinstance(raw_key, str):
            raise ValueError("Holdout failure category keys must be strings.")
        bucket = raw_key if raw_key in HOLDOUT_FAILURE_CATEGORIES else "other"
        cleaned[bucket] += count(raw_count)
    return dict(sorted(cleaned.items()))


def merge_holdout(track, path, identity=(None, None, False)):
    # Publish only numeric metrics and fixed public labels so no repo names or URLs can leak.
    report = read_report(path, identity)
    try:
        if not isinstance(report, dict):
            raise ValueError("The holdout report must be an object.")
        if "holdout" in report:
            payload = report["holdout"]
            if not isinstance(payload, dict):
                raise ValueError("The holdout split must be an object.")
        elif "dev" in report and not any(
            key in report for key in ("pass_rates", "aggregate", "reposTotal", "repos_total")
        ):
            raise ValueError("The holdout report is missing the holdout split.")
        else:
            payload = report

        level_rates = None
        if "pass_rates" in payload:
            aggregate, level_rates = parse_holdout_pass_rates(payload["pass_rates"])
        else:
            source = payload.get("aggregate", payload)
            if not isinstance(source, dict):
                raise ValueError("Holdout aggregate counts must be an object.")
            aggregate = holdout_counts(source)

        failure_categories = None
        raw_failures = payload.get("failure_categories", report.get("failure_categories"))
        if raw_failures is not None:
            failure_categories = sanitize_holdout_failure_categories(raw_failures)

        raw_categories = payload.get("categories", report.get("categories", []))
        if isinstance(raw_categories, list):
            categories = {
                row["category"]: row for row in raw_categories
                if isinstance(row, dict) and isinstance(row.get("category"), str)
            }
        elif isinstance(raw_categories, dict):
            categories = raw_categories
        else:
            raise ValueError("Holdout categories must be an array or object.")

        for row in track["categories"]:
            if row["category"] in categories:
                row.update(holdout_counts(categories[row["category"]]))

        if level_rates is not None:
            track["levelPassRates"] = level_rates
        else:
            track.pop("levelPassRates", None)
        if failure_categories is not None:
            track["failureCategories"] = failure_categories
        else:
            track.pop("failureCategories", None)

        if (aggregate["reposTotal"] >= HOLDOUT_MIN_REPOS
                and 100 * aggregate["reposPassed"] >= HOLDOUT_MIN_PASS_RATE * aggregate["reposTotal"]):
            aggregate["status"] = "pass"
        if payload.get("status", report.get("status")) == "fail" or report.get("sealed") is False:
            aggregate["status"] = "fail"
        track.update(
            aggregate,
            sealed=True,
            policy=(
                "Blind holdout corpus. Holdout entries are never inspected during development, "
                "and dashboard reports and receipts publish only aggregate pass rates and failure category counts."
            ),
            note="Repository counts from the blind holdout report.",
        )
    except (AttributeError, KeyError, OSError, TypeError, ValueError) as error:
        raise ValueError("The holdout report must contain valid aggregate and category counts.") from None


def score_vcpkg_shard(path, expected):
    """Return the shard status and the passed, failed, and skipped port counts."""
    expected = count(expected)
    passed = failed = skipped = 0
    if path.is_file():
        with path.open(encoding="utf-8") as stream:
            for line in stream:
                if not line.strip():
                    continue
                row = json.loads(line)
                status = row.get("status") if isinstance(row, dict) else None
                if status == "pass":
                    passed += 1
                elif status in ("no-observed-cmake-configure", "oracle-blocked-before-configure"):
                    skipped += 1
                else:
                    failed += 1
    if expected == 0 or passed + failed + skipped != expected:
        status = "partial"
    else:
        status = "pass" if passed > 0 and failed == 0 else "fail"
    return status, passed, failed, skipped


def merge_vcpkg(track, directory, identity=(None, None, False)):
    shards = {row["shard"]: row for row in track["shards"]}
    seen = set()
    for path in report_paths(directory, "shard.json"):
        report = read_report(path, identity)
        index = count(report["shard_index"])
        if count(report["shard_count"]) != track["shardsTotal"] or index not in shards or index in seen:
            raise ValueError("Each vcpkg report must identify a distinct shard in the 64-shard matrix.")
        seen.add(index)
        expected = report["expected_ports"]
        status, passed, failed, skipped = score_vcpkg_shard(path.with_name("results.jsonl"), expected)
        scored = passed + failed
        inspected = scored + skipped
        shards[index].update(status=status, portsTotal=scored, portsPassed=passed, portsFailed=failed,
                             portsInspected=inspected, portsExpected=expected,
                             durationSeconds=report.get("duration_seconds"),
                             note=f"{scored:,} scored CMake ports. "
                                  f"{inspected:,} / {expected:,} expected ports inspected.")

    reported = [row for row in shards.values() if row["status"] != "pending"]
    scored = sum(row["portsTotal"] for row in reported)
    passed = sum(row["portsPassed"] for row in reported)
    statuses = [row["status"] for row in shards.values()]
    status = "fail" if any(row.get("portsFailed") for row in reported) else combined_status(statuses)
    note = f"The pass rate uses {scored:,} scored CMake ports."
    if all("portsInspected" in row and "portsExpected" in row for row in reported):
        inspected = sum(row["portsInspected"] for row in reported)
        expected = sum(row["portsExpected"] for row in reported)
        track.update(portsInspected=inspected, portsExpected=expected)
        note += f" {inspected:,} / {expected:,} expected ports inspected in the reported shards."
    else:
        track.pop("portsInspected", None)
        track.pop("portsExpected", None)
    track.update(shardsCompleted=sum(row["status"] in {"pass", "fail"} for row in reported),
                 shardsPassed=sum(row["status"] == "pass" for row in reported),
                 portsTotal=scored, portsPassed=passed, passRate=rate(passed, scored),
                 status=status, note=note)


def merge_ci(snapshot, report, identity, source):
    runs = report["workflow_runs"] if isinstance(report, dict) else report
    expected_sha, _, strict = identity
    if expected_sha is not None:
        commits = report_identity_values(report, REPORT_SHA_KEYS)
        if commits:
            verify_report_identity(report, expected_sha, None, strict, source)
        for index, run in enumerate(runs):
            head = run.get("head_sha", run.get("headSha"))
            verify_report_identity(
                {"commit": head}, expected_sha, None, strict and not commits, f"{source} run {index}",
            )
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
    parser.add_argument("--release-sha", help="Exact Git commit SHA for the verified release binary.")
    parser.add_argument("--binary-sha256", help="SHA-256 hex digest of the verified release binary.")
    parser.add_argument("--strict-same-sha", action="store_true",
                        help="Require matching receipt identity and reset stale base tracks.")
    parser.add_argument(
        "--allow-stale-base",
        action="store_true",
        help="Explicitly allow carrying untouched base tracks across commit changes.",
    )
    parser.add_argument("--version", help="Version of the verified Rcmake binary.")
    parser.add_argument("--known-parity-gaps", nargs="*", help="Known parity gaps. Pass no values for zero gaps.")
    for name in (
        "runcmake-macos", "runcmake-linux", "runcmake-windows", "independent", "probe-cache",
        "mutation", "determinism", "path-swap", "wasm", "large-projects-dir",
        "fuzz-dir", "vcpkg-dir", "ci-status-json", "conan", "dist",
    ):
        parser.add_argument(f"--{name}", type=Path)
    parser.add_argument("--giants-benchmark", "--giants-bench", dest="giants_bench", type=Path)
    parser.add_argument("--unseen-holdout", "--holdout", dest="unseen_holdout", type=Path)
    parser.add_argument("--differential-fuzz", type=Path, action="append", default=[])
    parser.add_argument("--bench", type=Path, action="append", default=[])
    args = parser.parse_args(argv)

    try:
        schema = read_json(DATA / "dashboard.schema.json")
        snapshot = clean_draft_text(read_json(args.base))
        validate(snapshot, schema, schema)
        identity = resolve_release_identity(snapshot, args)
        snapshot["generatedAt"] = datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")
        if args.version is not None:
            snapshot["rcmakeVersion"] = args.version
        if args.known_parity_gaps is not None:
            snapshot["knownParityGaps"] = args.known_parity_gaps
        tracks = snapshot["tracks"]
        reports = {
            identifier: read_report(path, identity) for os_name, identifier in PLATFORMS.items()
            if (path := getattr(args, f"runcmake_{os_name}")) is not None
        }
        if reports:
            merge_runcmake(tracks, reports)
        section = tracks["benchmarksAndGates"]
        merge_benchmarks(section["benchmarks"], args.bench, identity)
        if args.giants_bench:
            merge_giants_bench(section, args.giants_bench, identity)
        merge_soundness(section, args, identity)
        if args.conan:
            merge_conan(section, read_report(args.conan, identity))
        if args.dist:
            merge_dist(snapshot, read_report(args.dist, identity))
        if args.large_projects_dir:
            merge_large_projects(tracks["bossFight"], args.large_projects_dir, identity)
        if args.unseen_holdout:
            merge_holdout(tracks["unseenHoldout"], args.unseen_holdout, identity)
        if args.fuzz_dir:
            merge_fuzz(section, args.fuzz_dir, identity)
        if args.differential_fuzz:
            merge_differential_fuzz(section, args.differential_fuzz, identity)
        if args.vcpkg_dir:
            merge_vcpkg(tracks["vcpkgMatrix"], args.vcpkg_dir, identity)
        if args.ci_status_json:
            merge_ci(snapshot, read_json(args.ci_status_json), identity, str(args.ci_status_json))
        validate(snapshot, schema, schema)
        write_snapshot(args.out, snapshot)
    except (AttributeError, KeyError, OSError, TypeError, ValueError) as error:
        parser.exit(2, f"{parser.prog}: {error}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
