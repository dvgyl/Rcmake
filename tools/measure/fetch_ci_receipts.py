#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Fetch CI and Ecosystem receipts for one commit into a release receipts directory.

The script finds the latest completed CI (ci.yml) and Ecosystem parity
(ecosystem.yml) runs for --commit. It refuses any run whose headSha is not
--commit. It writes these receipts with the names that
assemble_release_receipts.py expects:

  ci-jobs.json            jobs of the CI run (gh run view --json jobs)
  runcmake-windows.json   artifact runcmake-windows
  runcmake-linux.json     artifact runcmake-linux-full
  runcmake-macos.json     artifact runcmake-macos-full
  large-projects/<name>/  artifacts large-projects-<name> (result.json)
  vcpkg/<index>/          artifacts vcpkg-shard-<index> (shard.json, results.jsonl)
  conan.json              artifact conan (when the run uploads one)
  oracle-prop-test.json   artifact conformance (oracle-prop-test.json or oracle-prop.json)
  cli-ux.json             artifact conformance (when the artifact contains it)

Each JSON receipt gets release_commit set to --commit. With --binary-sha256,
each receipt also gets binary_sha256. A receipt that records a different commit
or digest stops the fetch before the script writes a file. A lane that
already has a local receipt (any alias name) is kept unless --overwrite is set.
At the end, the script prints assemble_release_receipts.py --status.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import json
from pathlib import Path
import re
import shutil
import subprocess
import sys
from tempfile import TemporaryDirectory
from typing import Any, Callable

if __package__:
    from . import assemble_release_receipts as assembler
    from . import build_dashboard_json as builder
    from . import verify_release_receipts as verifier
else:
    import assemble_release_receipts as assembler
    import build_dashboard_json as builder
    import verify_release_receipts as verifier


TOOLS_DIR = Path(__file__).resolve().parent
ROOT = TOOLS_DIR.parents[1]
ASSEMBLER_SCRIPT = TOOLS_DIR / "assemble_release_receipts.py"
FULL_SHA_RE = re.compile(r"^[0-9a-f]{40}$")
RUN_LIST_FIELDS = "databaseId,headSha,status,conclusion,event,url"
RUN_VIEW_FIELDS = "databaseId,headSha,status,conclusion,event,url,workflowName,jobs"
LANE_SPECS = {lane.key: lane for lane in assembler.LANES}


class FetchError(Exception):
    """A fetch step failed. The script writes no receipt after this error."""


@dataclass(frozen=True)
class Workflow:
    label: str
    file: str
    name: str


CI = Workflow("CI", "ci.yml", "CI")
ECOSYSTEM = Workflow("Ecosystem", "ecosystem.yml", "Ecosystem parity")


@dataclass(frozen=True)
class EcosystemLane:
    """Map Ecosystem artifacts to one receipt lane in --receipts-dir."""

    lane_key: str
    target: str
    artifact: str | None = None
    artifact_prefix: str | None = None
    member_files: tuple[str, ...] = ()
    required_member: str | None = None
    # File names to look for in a shared artifact, in order. The lane is skipped when none exists.
    sources: tuple[str, ...] = ()

    def artifacts(self, names: list[str]) -> list[str]:
        if self.artifact is not None:
            return [name for name in names if name == self.artifact]
        assert self.artifact_prefix is not None
        return sorted(
            (name for name in names if name.startswith(self.artifact_prefix) and len(name) > len(self.artifact_prefix)),
            key=_natural_key,
        )


ECOSYSTEM_LANES: tuple[EcosystemLane, ...] = (
    EcosystemLane("runcmake_windows", "runcmake-windows.json", artifact="runcmake-windows"),
    EcosystemLane("runcmake_linux", "runcmake-linux.json", artifact="runcmake-linux-full"),
    EcosystemLane("runcmake_macos", "runcmake-macos.json", artifact="runcmake-macos-full"),
    EcosystemLane(
        "large_projects",
        "large-projects",
        artifact_prefix="large-projects-",
        member_files=("result.json",),
        required_member="result.json",
    ),
    EcosystemLane(
        "vcpkg",
        "vcpkg",
        artifact_prefix="vcpkg-shard-",
        member_files=("shard.json", "results.jsonl"),
        required_member="shard.json",
    ),
    EcosystemLane("conan", "conan.json", artifact="conan"),
    EcosystemLane(
        "differential_fuzz",
        "oracle-prop-test.json",
        artifact="conformance",
        sources=("oracle-prop-test.json", "oracle-prop.json"),
    ),
    EcosystemLane("cli_ux", "cli-ux.json", artifact="conformance", sources=("cli-ux.json",)),
)


@dataclass(frozen=True)
class Run:
    workflow: Workflow
    run_id: int
    status: str
    conclusion: str
    url: str
    jobs: tuple[dict[str, Any], ...]
    event: str = ""


@dataclass(frozen=True)
class Planned:
    lane_key: str
    target: str
    source: str
    artifacts: tuple[str, ...]
    existing: tuple[str, ...]
    keep: bool


def _natural_key(name: str) -> tuple[Any, ...]:
    return tuple(int(part) if part.isdigit() else part for part in re.split(r"(\d+)", name))


class GhCli:
    """Run the GitHub CLI (gh) for one repository."""

    def __init__(self, repo: str | None) -> None:
        self.repo = repo or self._default_repo()

    @staticmethod
    def _call(args: list[str]) -> str:
        try:
            result = subprocess.run(args, cwd=ROOT, capture_output=True, text=True, timeout=600, check=False)
        except (OSError, subprocess.SubprocessError) as error:
            raise FetchError(f"Could not run {' '.join(args[:3])}: {error}") from None
        if result.returncode != 0:
            raise FetchError(f"{' '.join(args[:3])} failed: {result.stderr.strip() or result.stdout.strip()}")
        return result.stdout

    @classmethod
    def _default_repo(cls) -> str:
        repo = cls._call(["gh", "repo", "view", "--json", "nameWithOwner", "--jq", ".nameWithOwner"]).strip()
        if not repo:
            raise FetchError("gh repo view returned no repository; pass --repo owner/name.")
        return repo

    def list_runs(self, workflow: Workflow, commit: str) -> list[dict[str, Any]]:
        out = self._call([
            "gh", "run", "list", "--repo", self.repo, "--workflow", workflow.file, "--commit", commit,
            "--status", "completed", "--limit", "20", "--json", RUN_LIST_FIELDS,
        ])
        return json.loads(out or "[]")

    def view_run(self, run_id: int) -> dict[str, Any]:
        out = self._call(["gh", "run", "view", str(run_id), "--repo", self.repo, "--json", RUN_VIEW_FIELDS])
        return json.loads(out)

    def artifact_names(self, run_id: int) -> list[str]:
        out = self._call([
            "gh", "api", "--paginate", f"repos/{self.repo}/actions/runs/{run_id}/artifacts",
            "--jq", ".artifacts[] | select(.expired | not) | .name",
        ])
        return [line.strip() for line in out.splitlines() if line.strip()]

    def download(self, run_id: int, names: list[str], dest: Path) -> None:
        # A --pattern download always puts each artifact in its own directory.
        args = ["gh", "run", "download", str(run_id), "--repo", self.repo, "--dir", str(dest)]
        for name in names:
            args.extend(["--pattern", name])
        self._call(args)


def resolve_commit(value: str | None) -> str:
    """Return a full 40-character lowercase commit SHA."""
    raw = (value or "HEAD").strip()
    if FULL_SHA_RE.fullmatch(raw.lower()):
        return raw.lower()
    try:
        result = subprocess.run(
            ["git", "rev-parse", "--verify", f"{raw}^{{commit}}"],
            cwd=ROOT, capture_output=True, text=True, timeout=10, check=False,
        )
    except (OSError, subprocess.SubprocessError):
        result = None
    resolved = result.stdout.strip().lower() if result is not None and result.returncode == 0 else ""
    if not FULL_SHA_RE.fullmatch(resolved):
        raise FetchError(f"Could not resolve {raw!r} to a full commit SHA; pass the 40-character SHA.")
    return resolved


def _require_head_sha(workflow: Workflow, run_id: Any, head_sha: Any, commit: str) -> None:
    if not isinstance(head_sha, str) or head_sha.strip().lower() != commit:
        raise FetchError(
            f"Refusing {workflow.label} run {run_id}: headSha {head_sha!r} is not --commit {commit}."
        )


def validate_ci_run(run: Run) -> str | None:
    """Return an error message if a CI run is not a green push/dispatch run with all 6 distinct jobs passing."""
    if run.status.strip().lower() != "completed":
        return f"status is {run.status!r}, not 'completed'."
    if run.conclusion.strip().lower() != "success":
        return f"conclusion is {run.conclusion!r}, not 'success'."
    if run.event.strip().lower() not in verifier.CI_ALLOWED_EVENTS:
        return f"event is {run.event!r}, expected 'push' or 'workflow_dispatch'."
    if not run.jobs:
        return "run has no jobs."
    missing_or_failed, duplicate_required, extra_failed, _ = verifier.ci_jobs_issues(
        list(run.jobs), require_head_sha=False,
    )
    issues = [*missing_or_failed, *duplicate_required, *extra_failed]
    if issues:
        return f"all 6 distinct CI jobs must succeed ({'; '.join(issues)})."
    return None


def _build_run(workflow: Workflow, run_id: int, view: dict[str, Any], commit: str) -> Run:
    _require_head_sha(workflow, run_id, view.get("headSha"), commit)
    if view.get("workflowName") not in (None, workflow.name):
        raise FetchError(
            f"Refusing run {run_id}: it is a {view.get('workflowName')!r} run, not {workflow.name!r}."
        )
    if str(view.get("status") or "").strip().lower() != "completed":
        raise FetchError(f"Refusing {workflow.label} run {run_id}: status is {view.get('status')!r}, not 'completed'.")
    return Run(
        workflow=workflow,
        run_id=int(view.get("databaseId", run_id)),
        status=str(view["status"]),
        conclusion=str(view.get("conclusion") or ""),
        url=str(view.get("url") or ""),
        jobs=tuple(view.get("jobs") or ()),
        event=str(view.get("event") or ""),
    )


def resolve_run(gh: Any, workflow: Workflow, run_id: int | None, commit: str) -> Run | None:
    """Find the run to fetch and check that it ran on --commit."""
    if run_id is None:
        listed = gh.list_runs(workflow, commit)
        for row in listed:
            _require_head_sha(workflow, row.get("databaseId"), row.get("headSha"), commit)
        completed = [row for row in listed if str(row.get("status") or "").strip().lower() == "completed"]
        if not completed:
            return None
        if workflow == CI:
            candidates = [
                row for row in completed
                if str(row.get("conclusion") or "").strip().lower() == "success"
                and str(row.get("event") or "").strip().lower() in verifier.CI_ALLOWED_EVENTS
            ]
            for row in candidates:
                candidate_id = int(row["databaseId"])
                run = _build_run(workflow, candidate_id, gh.view_run(candidate_id), commit)
                if validate_ci_run(run) is None:
                    return run
            return None
        run_id = int(completed[0]["databaseId"])

    run = _build_run(workflow, run_id, gh.view_run(run_id), commit)
    if workflow == CI:
        reason = validate_ci_run(run)
        if reason is not None:
            raise FetchError(f"Refusing CI run {run.run_id}: {reason}")
    return run


def build_ci_jobs(run: Run, commit: str, binary: str | None) -> dict[str, Any]:
    """Convert CI run jobs into the workflow_runs receipt that build_dashboard_json.py reads."""
    reason = validate_ci_run(run)
    if reason is not None:
        raise FetchError(f"CI run {run.run_id}: {reason}")
    receipt: dict[str, Any] = {
        "release_commit": commit,
        "workflow": run.workflow.name,
        "run_id": run.run_id,
        "run_url": run.url,
        "run_conclusion": run.conclusion.strip().lower(),
        "workflow_runs": [
            {
                "name": job.get("name"),
                "status": str(job.get("status") or "").lower(),
                "conclusion": str(job.get("conclusion") or "").lower(),
                "head_sha": commit,
                "job_id": job.get("databaseId"),
                "url": job.get("url"),
            }
            for job in run.jobs
        ],
    }
    if run.event:
        receipt["event"] = run.event.strip().lower()
    if binary is not None:
        receipt["binary_sha256"] = binary
    return receipt


def stamp_receipt(
    payload: Any, commit: str, binary: str | None, source: str, *, project_pin: bool = False,
) -> dict[str, Any]:
    """Check that a receipt records only --commit and --binary-sha256, then stamp both."""
    if not isinstance(payload, dict):
        raise FetchError(f"{source}: expected a JSON object.")
    if project_pin and isinstance(payload.get("project"), str) and "commit" in payload:
        # Ecosystem large-project results store the upstream project pin as "commit".
        payload["project_commit"] = payload.pop("commit")
    for value in builder.report_identity_values(payload, builder.REPORT_SHA_KEYS):
        if value != commit:
            raise FetchError(f"{source}: records commit {value!r}, expected {commit}.")
    payload["release_commit"] = commit
    if binary is not None:
        for value in builder.report_identity_values(payload, builder.REPORT_DIGEST_KEYS):
            if value != binary:
                raise FetchError(f"{source}: records binary SHA-256 {value!r}, expected {binary}.")
        payload["binary_sha256"] = binary
    return payload


def _write_json(path: Path, payload: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")


def _read_json(path: Path, source: str) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise FetchError(f"{source}: invalid JSON ({error.__class__.__name__}).") from None


def existing_aliases(receipts_dir: Path, lane_key: str) -> tuple[str, ...]:
    spec = LANE_SPECS[lane_key]
    return tuple(name for name in spec.candidates if (receipts_dir / name).exists())


def build_plan(
    receipts_dir: Path,
    ci_run: Run | None,
    eco_run: Run | None,
    eco_artifacts: list[str],
    overwrite: bool,
) -> list[Planned]:
    def planned(lane_key: str, target: str, source: str, artifacts: tuple[str, ...]) -> Planned:
        existing = existing_aliases(receipts_dir, lane_key)
        return Planned(lane_key, target, source, artifacts, existing, keep=bool(existing) and not overwrite)

    plan: list[Planned] = []
    if ci_run is not None:
        plan.append(planned("ci_jobs", "ci-jobs.json", f"CI run {ci_run.run_id} ({len(ci_run.jobs)} jobs)", ()))
    if eco_run is not None:
        for lane in ECOSYSTEM_LANES:
            names = lane.artifacts(eco_artifacts)
            if not names:
                continue
            if lane.sources:
                label = f"{names[0]}/{lane.target}"
            elif len(names) == 1:
                label = names[0]
            else:
                label = f"{len(names)} {lane.artifact_prefix}* artifacts"
            plan.append(planned(lane.lane_key, lane.target, f"Ecosystem run {eco_run.run_id}: {label}", tuple(names)))
    return plan


def stage_lane(
    item: Planned, ci_run: Run | None, download_dir: Path, stage_dir: Path, commit: str, binary: str | None,
) -> Path | None:
    """Write the normalized receipt for one lane into stage_dir and return its path.

    Return None when a shared artifact does not contain the lane receipt.
    """
    target = stage_dir / item.target
    if item.lane_key == "ci_jobs":
        assert ci_run is not None
        _write_json(target, build_ci_jobs(ci_run, commit, binary))
        return target

    lane = next(lane for lane in ECOSYSTEM_LANES if lane.lane_key == item.lane_key)
    if lane.artifact is not None:
        artifact_dir = download_dir / lane.artifact
        if lane.sources:
            found = next((artifact_dir / name for name in lane.sources if (artifact_dir / name).is_file()), None)
            if found is None:
                return None
            source = found
        else:
            source = artifact_dir / lane.target
        if not source.is_file():
            members = sorted(artifact_dir.rglob("*.json")) if artifact_dir.is_dir() else []
            if len(members) != 1:
                raise FetchError(f"Artifact {lane.artifact} does not contain {lane.target}.")
            source = members[0]
        label = f"{lane.artifact}/{source.name}"
        _write_json(target, stamp_receipt(_read_json(source, label), commit, binary, label))
        return target

    assert lane.artifact_prefix is not None and lane.required_member is not None
    for name in item.artifacts:
        member = name[len(lane.artifact_prefix):]
        artifact_dir = download_dir / name
        if not (artifact_dir / lane.required_member).is_file():
            raise FetchError(f"Artifact {name} does not contain {lane.required_member}.")
        for file_name in lane.member_files:
            source = artifact_dir / file_name
            if not source.is_file():
                continue
            destination = target / member / file_name
            if file_name.endswith(".json"):
                label = f"{name}/{file_name}"
                payload = stamp_receipt(
                    _read_json(source, label), commit, binary, label, project_pin=lane.lane_key == "large_projects",
                )
                _write_json(destination, payload)
            else:
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(source, destination)
    return target


def install_lane(item: Planned, staged: Path, receipts_dir: Path) -> None:
    for name in item.existing:
        old = receipts_dir / name
        if old.is_dir() and not old.is_symlink():
            shutil.rmtree(old)
        else:
            old.unlink()
    staged.replace(receipts_dir / item.target)


def run_status(receipts_dir: Path, commit: str) -> int:
    result = subprocess.run(
        [sys.executable, str(ASSEMBLER_SCRIPT), "--receipts-dir", str(receipts_dir), "--commit", commit, "--status"],
        cwd=ROOT, capture_output=True, text=True, timeout=120, check=False,
    )
    sys.stdout.write(result.stdout)
    sys.stderr.write(result.stderr)
    return result.returncode


def describe_run(run: Run | None, workflow: Workflow) -> str:
    if run is None:
        if workflow == CI:
            return (
                f"{workflow.label}: no completed successful push/workflow_dispatch {workflow.file} "
                "run with all 6 required jobs green for this commit; skipping its lanes."
            )
        return f"{workflow.label}: no completed {workflow.file} run for this commit; skipping its lanes."
    return f"{workflow.label}: run {run.run_id} ({run.conclusion or run.status}) {run.url}"


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--receipts-dir", type=Path, required=True, help="Release receipts directory to fill.")
    parser.add_argument("--commit", default=None, help="Release commit SHA (default: git rev-parse HEAD).")
    parser.add_argument(
        "--binary-sha256",
        default=None,
        help="SHA-256 of the frozen release binary to stamp on each fetched receipt (strict verification needs it).",
    )
    parser.add_argument("--repo", default=None, help="GitHub repository owner/name (default: gh repo view).")
    parser.add_argument("--ci-run-id", type=int, default=None, help="CI run ID (default: latest completed run).")
    parser.add_argument(
        "--ecosystem-run-id", type=int, default=None, help="Ecosystem parity run ID (default: latest completed run).",
    )
    parser.add_argument("--dry-run", action="store_true", help="Print the plan. Do not download or write files.")
    parser.add_argument("--overwrite", action="store_true", help="Replace lanes that already have a local receipt.")
    parser.add_argument("--no-status", action="store_true", help="Do not run assemble_release_receipts.py --status.")
    return parser


def main(
    argv: list[str] | None = None,
    gh_factory: Callable[[str | None], Any] = GhCli,
    status_runner: Callable[[Path, str], int] = run_status,
) -> int:
    args = build_parser().parse_args(argv)
    receipts_dir: Path = args.receipts_dir
    if receipts_dir.exists() and not receipts_dir.is_dir():
        sys.stderr.write(f"fetch_ci_receipts: --receipts-dir is not a directory: {receipts_dir}\n")
        return 2

    try:
        commit = resolve_commit(args.commit)
        try:
            binary = builder.validate_sha256(args.binary_sha256) if args.binary_sha256 is not None else None
        except ValueError as error:
            raise FetchError(str(error)) from None
        gh = gh_factory(args.repo)
        ci_run = resolve_run(gh, CI, args.ci_run_id, commit)
        eco_run = resolve_run(gh, ECOSYSTEM, args.ecosystem_run_id, commit)
        print(f"Commit: {commit}")
        print(describe_run(ci_run, CI))
        print(describe_run(eco_run, ECOSYSTEM))
        if ci_run is None and eco_run is None:
            raise FetchError("No completed CI or Ecosystem run exists for this commit.")

        eco_artifacts = gh.artifact_names(eco_run.run_id) if eco_run is not None else []
        plan = build_plan(receipts_dir, ci_run, eco_run, eco_artifacts, args.overwrite)
        if not plan:
            print("No receipt artifacts exist in these runs.")
        for item in plan:
            if item.keep:
                print(f"keep   {', '.join(item.existing)} (local receipt; pass --overwrite to replace)")
            else:
                verb = "would write" if args.dry_run else "write"
                replaces = f" (replaces {', '.join(item.existing)})" if item.existing else ""
                print(f"{verb}  {item.target} <- {item.source}{replaces}")

        todo = [item for item in plan if not item.keep]
        if not args.dry_run and todo:
            receipts_dir.mkdir(parents=True, exist_ok=True)
            with TemporaryDirectory(prefix=".fetch-ci-receipts-", dir=receipts_dir) as temp:
                download_dir = Path(temp) / "download"
                stage_dir = Path(temp) / "stage"
                download_dir.mkdir()
                stage_dir.mkdir()
                names = list(dict.fromkeys(name for item in todo for name in item.artifacts))
                if names:
                    assert eco_run is not None
                    gh.download(eco_run.run_id, names, download_dir)
                staged: list[tuple[Planned, Path]] = []
                for item in todo:
                    path = stage_lane(item, ci_run, download_dir, stage_dir, commit, binary)
                    if path is None:
                        print(f"skip   {item.target} (artifact {', '.join(item.artifacts)} does not contain it)")
                    else:
                        staged.append((item, path))
                for item, path in staged:
                    install_lane(item, path, receipts_dir)
            print(f"Fetched {len(staged)} lane(s) into {receipts_dir}.")
    except FetchError as error:
        sys.stderr.write(f"fetch_ci_receipts: {error}\n")
        return 2

    if not args.no_status and receipts_dir.is_dir():
        print()
        status_runner(receipts_dir, commit)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
