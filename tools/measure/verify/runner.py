# SPDX-License-Identifier: MIT OR Apache-2.0
"""Scenario runner, differential scorecard formatting, and CLI parsing."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import platform
import sys
import textwrap
import time
from typing import Any, Mapping, Sequence

from .context import (
    SCENARIOS,
    Ctx,
    Options,
    Scenario,
    diff_observations,
    slug,
)
from .sandbox import (
    DEFAULT_TIMEOUT,
    ROOT,
    TOOLSET_DIRS,
    Sandbox,
    ScenarioSkip,
    StepFailed,
    Toolset,
    install_signal_handlers,
    remove_tree,
)

CLI_DESCRIPTION = """\
Independent black-box differential verification suite.

The suite builds small C, C++, Objective-C++, Fortran, CMake, and Presets
projects in a sandbox under /tmp. It runs each scenario with the CMake 4.5.1
oracle and with a staged Rcmake tree. A scenario passes when every check
passes for each tool set and the normalized observations are equal.

Use --oracle-only to run the checks with the oracle only. This mode proves
that the suite itself is correct.

Examples:
  source env/env.sh
  python3 tools/measure/independent_verify.py --oracle "$RCMAKE_ORACLE_CMAKE" --oracle-only
  python3 tools/measure/independent_verify.py --stage target/stage --only s05,s10
"""


def run_one(sc: Scenario, tools: Toolset, root: Path, options: Options) -> dict[str, Any]:
    """Run a scenario with one tool set and return its record."""
    ctx = Ctx(tools, root, options)
    start = time.monotonic()
    error = ""
    try:
        sc.func(ctx)
    except StepFailed:
        pass
    except ScenarioSkip:
        raise
    except Exception as exc:  # A scenario bug must not stop the other scenarios.
        error = f"{type(exc).__name__}: {exc}"
        ctx.check("scenario raised no exception", False, error)
    return {
        "seconds": round(time.monotonic() - start, 2),
        "passed": all(check.ok for check in ctx.checks) and bool(ctx.checks),
        "checks": [check.__dict__ for check in ctx.checks],
        "skips": ctx.skips,
        "observations": ctx.observations,
        "transcript": ctx.transcript,
    }


def run_scenario(sc: Scenario, toolsets: Sequence[Toolset], sandbox: Path, options: Options) -> dict[str, Any]:
    """Run a scenario with each tool set and compare the results."""
    record: dict[str, Any] = {"id": sc.sid, "title": sc.title, "runs": {}, "differences": []}
    start = time.monotonic()
    reason = sc.requires(options) if sc.requires else None
    if reason:
        record.update(status="skip", reason=reason, seconds=0.0)
        return record
    try:
        for tools in toolsets:
            root = sandbox / sc.sid / (tools.label if tools.label in TOOLSET_DIRS else slug(tools.label))
            try:
                record["runs"][tools.label] = run_one(sc, tools, root, options)
            finally:
                if not options.keep:
                    remove_tree(root)
    except ScenarioSkip as exc:
        record.update(status="skip", reason=str(exc), seconds=round(time.monotonic() - start, 2))
        return record
    passed = all(run["passed"] for run in record["runs"].values())
    if len(toolsets) == 2:
        first, second = (record["runs"][t.label]["observations"] for t in toolsets)
        record["differences"] = diff_observations(first, second)
        passed = passed and not record["differences"]
    record["status"] = "pass" if passed else "fail"
    record["seconds"] = round(time.monotonic() - start, 2)
    return record


def print_record(record: Mapping[str, Any], verbose: bool) -> None:
    """Print one scenario result."""
    status = record["status"].upper()
    print(f"{status:4} {record['id']} {record['title']} ({record.get('seconds', 0):.1f}s)", flush=True)
    if record["status"] == "skip":
        print(f"     skipped: {record.get('reason', '')}")
        return
    for label, run in record["runs"].items():
        passed = sum(1 for check in run["checks"] if check["ok"])
        total = len(run["checks"])
        print(f"     {label}: {passed}/{total} checks")
        for note in run["skips"]:
            print(f"       skipped sub-case: {note}")
        for check in run["checks"]:
            if not check["ok"] or verbose:
                mark = "ok  " if check["ok"] else "FAIL"
                print(f"       {mark} {check['name']}")
                if check["detail"]:
                    print(textwrap.indent(check["detail"], "            "))
    for diff in record["differences"]:
        print(f"     DIFF {diff['key']}")
        print(f"       oracle: {json.dumps(diff['oracle'])[:1500]}")
        print(f"       rcmake: {json.dumps(diff['rcmake'])[:1500]}")


def select(ids: str | None) -> list[Scenario]:
    """Select scenarios by a comma-separated list of ids."""
    if not ids:
        return list(SCENARIOS)
    wanted = [part.strip() for part in ids.split(",") if part.strip()]
    known = {sc.sid: sc for sc in SCENARIOS}
    unknown = [sid for sid in wanted if sid not in known]
    if unknown:
        raise SystemExit(f"Unknown scenario id: {', '.join(unknown)}")
    return [known[sid] for sid in wanted]


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    """Parse the command line."""
    parser = argparse.ArgumentParser(
        description=CLI_DESCRIPTION,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--oracle", type=Path, default=os.environ.get("RCMAKE_ORACLE_CMAKE"),
                        help="CMake 4.5.1 cmake program (default: $RCMAKE_ORACLE_CMAKE)")
    parser.add_argument("--stage", type=Path, default=ROOT / "target/stage",
                        help="Staged Rcmake tree with bin/cmake, bin/ctest, bin/cpack")
    parser.add_argument("--oracle-only", action="store_true", help="Run the checks with the oracle only")
    parser.add_argument("--only", help="Comma-separated scenario ids, for example s01,s05")
    parser.add_argument("--list", action="store_true", help="List the scenarios and exit")
    parser.add_argument("--json", type=Path, help="Write the scorecard as JSON to this file")
    parser.add_argument("--work-root", default="/tmp", help="Parent directory of the sandbox (default: /tmp)")
    parser.add_argument("--keep", action="store_true", help="Keep the sandbox for inspection")
    parser.add_argument("--jobs", type=int, default=4, help="Parallel build jobs (default: 4)")
    parser.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT, help="Timeout of one command in seconds")
    parser.add_argument("--no-xcode", action="store_true", help="Do not use the Xcode generator")
    parser.add_argument("--archive-mib", type=int, default=520,
                        help="Decoded size of the large archive member in MiB (default: 520)")
    parser.add_argument("--verbose", action="store_true", help="Print every check")
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    """Run the suite and return the process exit status."""
    args = parse_args(argv)
    if args.list:
        for sc in SCENARIOS:
            print(f"{sc.sid}  {sc.title}")
        return 0
    if not args.oracle:
        print("Set --oracle or RCMAKE_ORACLE_CMAKE.", file=sys.stderr)
        return 2
    toolsets = [Toolset.from_cmake("oracle", args.oracle)]
    if not args.oracle_only:
        toolsets.append(Toolset.from_stage(args.stage))
    for tools in toolsets:
        missing = tools.missing()
        if missing:
            print(f"{tools.label}: missing program(s): {', '.join(missing)}", file=sys.stderr)
            return 2
    options = Options(jobs=max(1, args.jobs), timeout=args.timeout, xcode=not args.no_xcode,
                      archive_mib=args.archive_mib, keep=args.keep)
    scenarios = select(args.only)
    install_signal_handlers()
    records = []
    started = time.monotonic()
    with Sandbox(args.work_root, keep=args.keep) as sandbox:
        if args.keep:
            print(f"Sandbox: {sandbox.path}")
        for sc in scenarios:
            record = run_scenario(sc, toolsets, sandbox.path, options)
            print_record(record, args.verbose)
            records.append(record)
    summary = {status: sum(1 for r in records if r["status"] == status) for status in ("pass", "fail", "skip")}
    card = {
        "mode": "oracle-only" if args.oracle_only else "differential",
        "platform": f"{platform.system()} {platform.machine()}",
        "toolsets": {t.label: str(t.cmake) for t in toolsets},
        "seconds": round(time.monotonic() - started, 1),
        "summary": summary,
        "scenarios": records,
    }
    print(f"\nScorecard ({card['mode']}): {summary['pass']} pass, {summary['fail']} fail, "
          f"{summary['skip']} skip in {card['seconds']}s")
    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(json.dumps(card, indent=2) + "\n", encoding="utf-8")
    return 1 if summary["fail"] else 0
