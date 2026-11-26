#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Assemble, verify, and stamp frozen release receipts from a standardized directory.

Scans a receipt directory for every required release lane, enforces single-commit
(and optional binary SHA-256) identity across all receipts and nested members,
invokes build_dashboard_json.py --strict-same-sha, runs verify_release_receipts.py,
and optionally stamps README.md and docs/BUILD_WRITEUP.md via stamp_metrics.py.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from datetime import datetime, timezone
import json
from pathlib import Path
import shutil
from stat import S_IMODE
import subprocess
import sys
from tempfile import NamedTemporaryFile, TemporaryDirectory
from typing import Any

if __package__:
    from . import build_dashboard_json as builder
    from . import verify_release_receipts as verifier
else:
    import build_dashboard_json as builder
    import verify_release_receipts as verifier


TOOLS_DIR = Path(__file__).resolve().parent
ROOT = TOOLS_DIR.parents[1]
DEFAULT_OUT_JSON = ROOT / "site" / "data" / "latest.json"
DEFAULT_SCHEMA = ROOT / "site" / "data" / "dashboard.schema.json"
DEFAULT_WASM_BUNDLE = ROOT / "site" / "wasm" / "rcmake_wasm.wasm"
DEFAULT_README = ROOT / "README.md"
DEFAULT_WRITEUP = ROOT / "docs" / "BUILD_WRITEUP.md"
DIST_SIZE_ALIASES = ("binary-size.json", "binary_size.json")


@dataclass(frozen=True)
class LaneSpec:
    """Metadata describing one release receipt lane in --receipts-dir."""

    key: str
    track: str
    required: bool
    canonical_name: str
    candidates: tuple[str, ...]
    builder_flag: str | None = None
    directory_glob: str | None = None
    companion_candidates: tuple[str, ...] = ()


LANES: tuple[LaneSpec, ...] = (
    LaneSpec(
        key="runcmake_macos",
        track="track1-conformance",
        required=True,
        canonical_name="runcmake-macos.json",
        candidates=("runcmake-macos.json", "runcmake_macos.json"),
        builder_flag="--runcmake-macos",
    ),
    LaneSpec(
        key="runcmake_linux",
        track="track1-conformance",
        required=True,
        canonical_name="runcmake-linux.json",
        candidates=("runcmake-linux.json", "runcmake_linux.json"),
        builder_flag="--runcmake-linux",
    ),
    LaneSpec(
        key="runcmake_windows",
        track="track1-conformance",
        required=True,
        canonical_name="runcmake-windows.json",
        candidates=("runcmake-windows.json", "runcmake_windows.json"),
        builder_flag="--runcmake-windows",
    ),
    LaneSpec(
        key="independent_scenarios",
        track="track1-conformance",
        required=True,
        canonical_name="independent-scenarios.json",
        candidates=(
            "independent-scenarios.json",
            "independent.json",
            "independent_scenarios.json",
            "blackbox.json",
        ),
        builder_flag="--independent",
    ),
    LaneSpec(
        key="large_projects",
        track="track2-ecosystem",
        required=True,
        canonical_name="large-projects/ or large-projects.json",
        candidates=("large-projects", "large-projects.json", "large_projects.json"),
        builder_flag="--large-projects-dir",
        directory_glob="result.json",
    ),
    LaneSpec(
        key="vcpkg",
        track="track2-ecosystem",
        required=True,
        canonical_name="vcpkg/ or vcpkg.json",
        candidates=("vcpkg", "vcpkg.json", "vcpkg-matrix.json", "vcpkg_matrix.json"),
        builder_flag="--vcpkg-dir",
        directory_glob="shard.json",
    ),
    LaneSpec(
        key="conan",
        track="track2-ecosystem",
        required=True,
        canonical_name="conan.json",
        candidates=("conan.json", "ecosystem.json", "conan-ecosystem.json", "conan_ecosystem.json"),
        builder_flag="--conan",
    ),
    LaneSpec(
        key="unseen_holdout",
        track="track2-ecosystem",
        required=True,
        canonical_name="unseen-holdout.json",
        candidates=("unseen-holdout.json", "unseen_holdout.json", "unseen.json", "holdout.json"),
        builder_flag="--unseen-holdout",
    ),
    LaneSpec(
        key="benchmarks_giants",
        track="track3-benchmarks",
        required=True,
        canonical_name="bench-giants.json",
        candidates=(
            "bench-giants.json",
            "bench_giants.json",
            "benchmarks-giants.json",
            "benchmarks_giants.json",
        ),
        builder_flag="--giants-benchmark",
    ),
    LaneSpec(
        key="benchmarks_synthetic",
        track="track3-benchmarks",
        required=True,
        canonical_name="benchmarks-synthetic/ or bench-configure.json",
        candidates=(
            "benchmarks-synthetic",
            "bench-synthetic.json",
            "bench_synthetic.json",
            "benchmarks-synthetic.json",
            "benchmarks_synthetic.json",
            "bench-configure.json",
            "bench.json",
        ),
        builder_flag="--bench",
        directory_glob="*.json",
        companion_candidates=("bench-eval.json", "bench_eval.json"),
    ),
    LaneSpec(
        key="probe_cache",
        track="track4-soundness",
        required=True,
        canonical_name="probe-cache.json",
        candidates=("probe-cache.json", "probe_cache.json", "probe-gate.json", "probe.json"),
        builder_flag="--probe-cache",
    ),
    LaneSpec(
        key="mutation_replay",
        track="track4-soundness",
        required=True,
        canonical_name="mutation-replay.json",
        candidates=("mutation-replay.json", "mutation_replay.json", "mutation-gate.json", "mutation.json"),
        builder_flag="--mutation",
    ),
    LaneSpec(
        key="fuzz",
        track="track4-soundness",
        required=True,
        canonical_name="fuzz/ or fuzz.json",
        candidates=("fuzz", "fuzz.json"),
        builder_flag="--fuzz-dir",
        directory_glob="*.json",
    ),
    LaneSpec(
        key="differential_fuzz",
        track="track4-soundness",
        required=True,
        canonical_name="differential-fuzz.json",
        candidates=(
            "differential-fuzz.json",
            "differential_fuzz.json",
            "diff-fuzz.json",
            "diff_fuzz.json",
            "oracle-prop-test.json",
            "oracle_prop_test.json",
        ),
        builder_flag="--differential-fuzz",
    ),
    LaneSpec(
        key="wasi",
        track="track5-wasm",
        required=True,
        canonical_name="wasi.json",
        candidates=("wasi.json", "wasm-smoke.json", "wasm_smoke.json", "wasm.json"),
        builder_flag="--wasm",
    ),
    LaneSpec(
        key="cli_ux",
        track="track6-ux-packaging",
        required=True,
        canonical_name="cli-ux.json",
        candidates=("cli-ux.json", "cli_ux.json"),
        builder_flag=None,
    ),
    LaneSpec(
        key="path_swap",
        track="track6-ux-packaging",
        required=True,
        canonical_name="path-swap.json",
        candidates=("path-swap.json", "path_swap.json", "path-swap-gate.json"),
        builder_flag="--path-swap",
    ),
    LaneSpec(
        key="dist",
        track="track6-ux-packaging",
        required=True,
        canonical_name="dist.json",
        candidates=("dist.json", "dist-tarball.json", "dist_tarball.json", *DIST_SIZE_ALIASES),
        builder_flag="--dist",
    ),
    LaneSpec(
        key="ci_jobs",
        track="track6-ux-packaging",
        required=True,
        canonical_name="ci-jobs.json",
        candidates=("ci-jobs.json", "ci_jobs.json", "ci-status.json", "ci_status.json"),
        builder_flag="--ci-status-json",
    ),
    LaneSpec(
        key="determinism",
        track="optional",
        required=False,
        canonical_name="determinism.json",
        candidates=("determinism.json",),
        builder_flag="--determinism",
    ),
    LaneSpec(
        key="ide",
        track="optional",
        required=False,
        canonical_name="ide.json",
        candidates=("ide.json", "ide-file-api.json", "ide_file_api.json"),
        builder_flag=None,
    ),
)

REQUIRED_LANES: tuple[LaneSpec, ...] = tuple(lane for lane in LANES if lane.required)


@dataclass(frozen=True)
class FileCheck:
    """Identity inspection result for one JSON receipt file."""

    rel_path: str
    path: Path
    commits: tuple[str, ...]
    binaries: tuple[str, ...]
    error: str | None = None
    is_json_object: bool = True
    binary_size_bytes: int | None = None


@dataclass(frozen=True)
class LaneInspection:
    """Readiness status for one release lane."""

    spec: LaneSpec
    state: str
    path: Path | None
    display_path: str
    companion_paths: tuple[Path, ...]
    commits: tuple[str, ...]
    binaries: tuple[str, ...]
    member_count: int
    offenders: tuple[str, ...]
    detail: str

    @property
    def ready(self) -> bool:
        return self.state == "present"


@dataclass(frozen=True)
class DirectoryScan:
    """Aggregated readiness report for an entire --receipts-dir."""

    receipts_dir: Path
    expected_commit: str
    requested_binary: str | None
    inferred_binary: str | None
    lanes: tuple[LaneInspection, ...]
    offenders: tuple[str, ...]

    @property
    def missing_required(self) -> tuple[LaneInspection, ...]:
        return tuple(lane for lane in self.lanes if lane.spec.required and lane.state == "missing")

    @property
    def present_lanes(self) -> tuple[LaneInspection, ...]:
        return tuple(lane for lane in self.lanes if lane.path is not None)

    @property
    def has_blocking_errors(self) -> bool:
        return bool(self.offenders) or any(
            lane.state not in {"present", "missing"} for lane in self.lanes
        )

    @property
    def is_complete(self) -> bool:
        return not self.has_blocking_errors and not self.missing_required


def layout_help_text() -> str:
    """Format the expected --receipts-dir layout for --help."""
    lines = [
        "Standardized --receipts-dir layout (19 required release lanes + optional lanes):",
        "",
        "Required lanes:",
    ]
    for lane in LANES:
        if not lane.required:
            continue
        aliases = ", ".join(lane.candidates)
        lines.append(f"  {lane.key:<22} {lane.canonical_name:<38} aliases: {aliases}")
    lines.append("")
    lines.append("Optional supplemental lanes (validated and included when present):")
    for lane in LANES:
        if lane.required:
            continue
        aliases = ", ".join(lane.candidates)
        lines.append(f"  {lane.key:<22} {lane.canonical_name:<38} aliases: {aliases}")
    lines.append("")
    lines.append("Companion benchmark file:")
    lines.append("  bench-eval.json / bench_eval.json is automatically paired with bench-configure.json.")
    return "\n".join(lines)


def resolve_default_commit() -> str:
    """Read current HEAD commit SHA from the repository."""
    try:
        result = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            cwd=ROOT,
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
    except (OSError, subprocess.SubprocessError) as error:
        raise ValueError(f"Could not run git rev-parse HEAD ({error}); pass --commit explicitly.") from None
    if result.returncode != 0:
        raise ValueError("git rev-parse HEAD failed; pass --commit explicitly.")
    commit = verifier.normalize_commit(result.stdout.strip())
    if commit is None:
        raise ValueError(f"git rev-parse HEAD returned an invalid SHA: {result.stdout.strip()!r}.")
    return commit


def _collect_commits_from_ci(payload: Any) -> list[str]:
    commits: list[str] = []
    if isinstance(payload, dict):
        commits.extend(builder.report_identity_values(payload, builder.REPORT_SHA_KEYS))
        runs = payload.get("workflow_runs")
    elif isinstance(payload, list):
        runs = payload
    else:
        runs = None

    if isinstance(runs, list):
        for run in runs:
            if not isinstance(run, dict):
                continue
            head = run.get("head_sha", run.get("headSha"))
            if isinstance(head, str) and head.strip():
                commits.append(head.strip().lower())
    return commits


def _inspect_json_file(path: Path, receipts_dir: Path, lane_key: str) -> FileCheck:
    rel = path.relative_to(receipts_dir).as_posix()
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        return FileCheck(rel, path, (), (), error=f"invalid JSON ({error.__class__.__name__})")

    if lane_key == "ci_jobs":
        if not isinstance(payload, (dict, list)):
            return FileCheck(rel, path, (), (), error="expected a JSON object or array")
        commits = tuple(dict.fromkeys(_collect_commits_from_ci(payload)))
        binaries = (
            tuple(dict.fromkeys(builder.report_identity_values(payload, builder.REPORT_DIGEST_KEYS)))
            if isinstance(payload, dict)
            else ()
        )
        return FileCheck(rel, path, commits, binaries, is_json_object=isinstance(payload, dict))

    if not isinstance(payload, dict):
        return FileCheck(rel, path, (), (), error="expected a JSON object")

    commits = tuple(dict.fromkeys(builder.report_identity_values(payload, builder.REPORT_SHA_KEYS)))
    binaries = tuple(dict.fromkeys(builder.report_identity_values(payload, builder.REPORT_DIGEST_KEYS)))
    size = payload.get("binary_size_bytes") if lane_key == "dist" else None
    size = size if verifier.is_int(size) and size > 0 else None
    return FileCheck(rel, path, commits, binaries, binary_size_bytes=size)


def _member_files_for_directory(directory: Path, pattern: str) -> list[Path]:
    return sorted(
        path for path in directory.rglob(pattern)
        if path.is_file() and not path.name.startswith("._") and path.name != "outcomes.json"
    )


def _discover_lane_paths(receipts_dir: Path, spec: LaneSpec) -> tuple[list[Path], list[Path]]:
    matches: list[Path] = []
    for name in spec.candidates:
        candidate = receipts_dir / name
        if candidate.exists():
            matches.append(candidate)

    companions: list[Path] = []
    if spec.key == "dist" and len(matches) == 2 and all(path.is_file() for path in matches):
        size_files = [path for path in matches if path.name in DIST_SIZE_ALIASES]
        if len(size_files) == 1:
            companions = size_files
            matches.remove(size_files[0])
    if len(matches) == 1 and matches[0].is_file() and spec.companion_candidates:
        for name in spec.companion_candidates:
            candidate = receipts_dir / name
            if candidate.is_file():
                companions.append(candidate)
    return matches, companions


def scan_receipts_dir(
    receipts_dir: Path,
    expected_commit: str,
    requested_binary: str | None = None,
) -> DirectoryScan:
    """Scan --receipts-dir and verify commit and binary SHA-256 identity across every lane."""
    norm_commit = verifier.normalize_commit(expected_commit)
    if norm_commit is None:
        raise ValueError(f"--commit must be a 7 to 40 character hex SHA, got {expected_commit!r}.")
    norm_binary: str | None = None
    if requested_binary is not None:
        norm_binary = verifier.normalize_sha256(requested_binary)
        if norm_binary is None:
            raise ValueError(
                f"--binary-sha256 must be a 64-character hex SHA-256 digest, got {requested_binary!r}."
            )

    preliminary: list[tuple[LaneSpec, list[Path], list[Path], list[FileCheck]]] = []
    all_observed_binaries: set[str] = set()
    all_files_have_binary = True
    any_files_checked = False

    for spec in LANES:
        matches, companions = _discover_lane_paths(receipts_dir, spec)
        checks: list[FileCheck] = []
        if len(matches) == 1:
            target = matches[0]
            if target.is_dir():
                if spec.directory_glob is None:
                    checks.append(
                        FileCheck(
                            target.relative_to(receipts_dir).as_posix(),
                            target,
                            (),
                            (),
                            error="expected a JSON file, found a directory",
                        )
                    )
                else:
                    members = _member_files_for_directory(target, spec.directory_glob)
                    for member in members:
                        checks.append(_inspect_json_file(member, receipts_dir, spec.key))
            else:
                checks.append(_inspect_json_file(target, receipts_dir, spec.key))
                for companion in companions:
                    checks.append(_inspect_json_file(companion, receipts_dir, spec.key))

        for item in checks:
            if item.error is not None:
                continue
            any_files_checked = True
            if item.binaries:
                all_observed_binaries.update(item.binaries)
            else:
                all_files_have_binary = False
        preliminary.append((spec, matches, companions, checks))

    inferred_binary: str | None = None
    mixed_binaries = len(all_observed_binaries) > 1
    if norm_binary is None and not mixed_binaries and len(all_observed_binaries) == 1 and all_files_have_binary and any_files_checked:
        inferred_binary = next(iter(all_observed_binaries))

    lanes_out: list[LaneInspection] = []
    global_offenders: list[str] = []

    for spec, matches, companions, checks in preliminary:
        if not matches:
            lanes_out.append(
                LaneInspection(
                    spec=spec,
                    state="missing",
                    path=None,
                    display_path=spec.canonical_name,
                    companion_paths=(),
                    commits=(),
                    binaries=(),
                    member_count=0,
                    offenders=(),
                    detail="not found",
                )
            )
            continue

        if len(matches) > 1:
            rels = [p.relative_to(receipts_dir).as_posix() for p in matches]
            msg = f"{', '.join(rels)}: multiple alias files present for lane '{spec.key}'"
            global_offenders.append(msg)
            lanes_out.append(
                LaneInspection(
                    spec=spec,
                    state="ambiguous",
                    path=matches[0],
                    display_path=", ".join(rels),
                    companion_paths=(),
                    commits=(),
                    binaries=(),
                    member_count=len(matches),
                    offenders=(msg,),
                    detail=f"ambiguous aliases ({', '.join(rels)})",
                )
            )
            continue

        target = matches[0]
        rel_target = target.relative_to(receipts_dir).as_posix()
        if target.is_dir():
            display_path = rel_target + "/"
        elif companions:
            companion_names = ", ".join(p.relative_to(receipts_dir).as_posix() for p in companions)
            display_path = f"{rel_target} (+ {companion_names})"
        else:
            display_path = rel_target

        if target.is_dir() and not checks:
            msg = f"{rel_target}/: directory contains no matching {spec.directory_glob} receipt files"
            global_offenders.append(msg)
            lanes_out.append(
                LaneInspection(
                    spec=spec,
                    state="invalid",
                    path=target,
                    display_path=display_path,
                    companion_paths=tuple(companions),
                    commits=(),
                    binaries=(),
                    member_count=0,
                    offenders=(msg,),
                    detail=f"empty directory (no {spec.directory_glob} files)",
                )
            )
            continue

        lane_commits: list[str] = []
        lane_binaries: list[str] = []
        lane_offenders: list[str] = []
        states_seen: set[str] = set()

        for item in checks:
            if item.error is not None:
                states_seen.add("invalid")
                lane_offenders.append(f"{item.rel_path}: {item.error}")
                continue

            lane_commits.extend(item.commits)
            lane_binaries.extend(item.binaries)

            if not item.commits:
                states_seen.add("missing-sha")
                lane_offenders.append(f"{item.rel_path}: missing release_commit (expected {norm_commit})")
            elif len(item.commits) > 1:
                states_seen.add("sha-mismatch")
                joined = ", ".join(item.commits)
                lane_offenders.append(
                    f"{item.rel_path}: conflicting commits [{joined}] (expected {norm_commit})"
                )
            elif item.commits[0] != norm_commit:
                states_seen.add("sha-mismatch")
                lane_offenders.append(
                    f"{item.rel_path}: release_commit {item.commits[0]!r} != expected {norm_commit!r}"
                )

            if len(item.binaries) > 1:
                states_seen.add("binary-mismatch")
                joined = ", ".join(item.binaries)
                lane_offenders.append(f"{item.rel_path}: conflicting binary_sha256 values [{joined}]")
            elif not item.binaries and (norm_binary is not None or (spec.key == "dist" and companions)):
                states_seen.add("missing-binary-sha256")
                lane_offenders.append(f"{item.rel_path}: missing binary_sha256")
            elif norm_binary is not None and item.binaries[0] != norm_binary:
                states_seen.add("binary-mismatch")
                lane_offenders.append(
                    f"{item.rel_path}: binary_sha256 {item.binaries[0]!r} != expected {norm_binary!r}"
                )
            elif mixed_binaries and item.binaries:
                states_seen.add("binary-mismatch")
                lane_offenders.append(
                    f"{item.rel_path}: mixed binary_sha256 {item.binaries[0]!r} across receipts"
                )

        if spec.key == "dist" and companions and all(item.error is None for item in checks):
            distribution, size_receipt = checks
            if (distribution.binary_size_bytes is None
                    or distribution.binary_size_bytes != size_receipt.binary_size_bytes):
                states_seen.add("invalid")
                lane_offenders.append(
                    f"{size_receipt.rel_path} and {distribution.rel_path} must record "
                    "the same positive binary_size_bytes."
                )

        unique_commits = tuple(dict.fromkeys(lane_commits))
        unique_binaries = tuple(dict.fromkeys(lane_binaries))

        if not states_seen:
            state = "present"
            suffix = f" ({len(checks)} files)" if len(checks) > 1 else ""
            detail = f"SHA match{suffix}"
        else:
            for priority in ("invalid", "sha-mismatch", "missing-sha", "binary-mismatch", "missing-binary-sha256"):
                if priority in states_seen:
                    state = priority
                    break
            else:
                state = "invalid"
            detail = f"{len(lane_offenders)} issue(s): {lane_offenders[0]}"

        global_offenders.extend(lane_offenders)
        lanes_out.append(
            LaneInspection(
                spec=spec,
                state=state,
                path=target,
                display_path=display_path,
                companion_paths=tuple(companions),
                commits=unique_commits,
                binaries=unique_binaries,
                member_count=len(checks),
                offenders=tuple(lane_offenders),
                detail=detail,
            )
        )

    return DirectoryScan(
        receipts_dir=receipts_dir,
        expected_commit=norm_commit,
        requested_binary=norm_binary,
        inferred_binary=inferred_binary,
        lanes=tuple(lanes_out),
        offenders=tuple(global_offenders),
    )


def _short_hex(values: tuple[str, ...]) -> str:
    if not values:
        return "-"
    if len(values) == 1:
        return values[0][:12]
    return f"mixed ({len(values)})"


def format_status_matrix(scan: DirectoryScan) -> str:
    """Render a deterministic Markdown/terminal readiness matrix."""
    effective_binary = scan.requested_binary or scan.inferred_binary
    if scan.requested_binary is not None:
        binary_line = f"`{scan.requested_binary[:16]}...` (explicit)"
    elif scan.inferred_binary is not None:
        binary_line = f"`{scan.inferred_binary[:16]}...` (consistent across receipts)"
    else:
        binary_line = "not specified"

    lines = [
        "Release receipt readiness",
        f"Commit: `{scan.expected_commit}`",
        f"Binary SHA-256: {binary_line}",
        f"Receipts directory: `{scan.receipts_dir}`",
        "",
        "| Lane | Track | State | File | Commit | Binary SHA-256 | Detail |",
        "| --- | --- | --- | --- | --- | --- | --- |",
    ]
    for lane in scan.lanes:
        if not lane.spec.required and lane.state == "missing":
            continue
        lines.append(
            f"| `{lane.spec.key}` | `{lane.spec.track}` | `{lane.state}` | "
            f"`{lane.display_path}` | `{_short_hex(lane.commits)}` | "
            f"`{_short_hex(lane.binaries)}` | {lane.detail} |"
        )

    required = [lane for lane in scan.lanes if lane.spec.required]
    ready_count = sum(1 for lane in required if lane.ready)
    missing_keys = [lane.spec.key for lane in required if lane.state == "missing"]
    bad_required = [lane for lane in required if lane.state not in {"present", "missing"}]
    bad_optional = [lane for lane in scan.lanes if not lane.spec.required and lane.state not in {"present", "missing"}]

    parts = [f"{ready_count} / {len(required)} required lanes ready"]
    notes = []
    if missing_keys:
        notes.append(f"{len(missing_keys)} missing")
    if bad_required or bad_optional:
        notes.append(f"{len(bad_required) + len(bad_optional)} invalid/mismatched")
    if notes:
        parts.append(f"({', '.join(notes)})")
    lines.append("")
    lines.append(f"Summary: {' '.join(parts)}.")
    if missing_keys:
        lines.append(f"Missing required lanes: {', '.join(missing_keys)}")
    if scan.offenders:
        lines.append("Offending files:")
        for offender in scan.offenders:
            lines.append(f"  - {offender}")
    if effective_binary is None and not scan.offenders and not missing_keys:
        lines.append("Note: no binary_sha256 digest was present across all receipts.")
    return "\n".join(lines) + "\n"


def _atomic_copy_file(source: Path, destination: Path) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    content = source.read_bytes()
    temporary: Path | None = None
    try:
        with NamedTemporaryFile(mode="wb", dir=destination.parent, delete=False) as stream:
            temporary = Path(stream.name)
            stream.write(content)
        if destination.exists():
            temporary.chmod(S_IMODE(destination.stat().st_mode))
        temporary.replace(destination)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def _atomic_write_json(path: Path, payload: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    encoded = json.dumps(payload, indent=2, ensure_ascii=False) + "\n"
    temporary: Path | None = None
    try:
        with NamedTemporaryFile(mode="w", encoding="utf-8", dir=path.parent, delete=False) as stream:
            temporary = Path(stream.name)
            stream.write(encoded)
        if path.exists():
            temporary.chmod(S_IMODE(path.stat().st_mode))
        temporary.replace(path)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def _stage_vcpkg_summary(summary_path: Path, stage_dir: Path) -> Path:
    """Expand a flat vcpkg.json summary receipt into shard.json + results.jsonl for build_dashboard_json.py."""
    payload = json.loads(summary_path.read_text(encoding="utf-8"))
    raw_shards = payload.get("shards")
    if not isinstance(raw_shards, list) or not raw_shards:
        raise ValueError(f"{summary_path.name}: flat vcpkg receipt must include a non-empty shards list.")
    file_commit, file_binary = verifier.extract_identity_fields(payload)
    ident: dict[str, str] = {}
    if file_commit is not None:
        ident["release_commit"] = file_commit
    if file_binary is not None:
        ident["binary_sha256"] = file_binary

    vcpkg_stage = stage_dir / "vcpkg"
    vcpkg_stage.mkdir(parents=True, exist_ok=True)
    default_shard_count = payload.get("shards_total", payload.get("shardsTotal", len(raw_shards)))
    for row in raw_shards:
        if not isinstance(row, dict):
            raise ValueError(f"{summary_path.name}: each vcpkg shard entry must be an object.")
        index = row.get("shard_index", row.get("shard"))
        shard_count = row.get("shard_count", row.get("shardCount", default_shard_count))
        ports_total = row.get("ports_total", row.get("portsTotal", 0))
        ports_scored = row.get("ports_scored", row.get("portsScored", ports_total))
        ports_expected = row.get("ports_expected", row.get("portsExpected", ports_total))
        ports_skipped = row.get("ports_skipped", row.get("portsSkipped", ports_expected - ports_scored))
        ports_passed = row.get("ports_passed", row.get("portsPassed", 0))
        ports_failed = row.get("ports_failed", row.get("portsFailed", 0))
        duration = row.get("duration_seconds", row.get("durationSeconds", 0.0))
        status = row.get("status", "pass")
        if status == "fail" and ports_failed == 0 and ports_passed < ports_scored:
            ports_failed = ports_scored - ports_passed

        shard_dir = vcpkg_stage / str(index)
        shard_dir.mkdir(parents=True, exist_ok=True)
        shard_payload = {
            **ident,
            "shard_index": index,
            "shard_count": shard_count,
            "expected_ports": ports_expected,
            "duration_seconds": duration,
        }
        (shard_dir / "shard.json").write_text(json.dumps(shard_payload) + "\n", encoding="utf-8")
        lines = [
            json.dumps({"port": f"port-{index}-{port_idx}", "status": "pass"})
            for port_idx in range(int(ports_passed))
        ]
        lines.extend(
            json.dumps({"port": f"failed-{index}-{fail_idx}", "status": "configure-mismatch"})
            for fail_idx in range(int(ports_failed))
        )
        lines.extend(
            json.dumps({"port": f"skipped-{index}-{skip_idx}", "status": "no-observed-cmake-configure"})
            for skip_idx in range(builder.count(ports_skipped))
        )
        (shard_dir / "results.jsonl").write_text(
            ("\n".join(lines) + "\n") if lines else "",
            encoding="utf-8",
        )
    return vcpkg_stage


def _stage_ci_jobs_for_builder(ci_path: Path, stage_dir: Path, expected_commit: str) -> Path:
    """Adapt a {passed, total} ci-jobs.json summary into workflow_runs if needed for build_dashboard_json.py."""
    payload = json.loads(ci_path.read_text(encoding="utf-8"))
    if isinstance(payload, list) or (isinstance(payload, dict) and "workflow_runs" in payload):
        return ci_path
    if isinstance(payload, dict):
        passed = payload.get("passed")
        total = payload.get("total")
        if verifier.is_int(passed) and verifier.is_int(total) and 0 <= passed <= total:
            file_commit, file_binary = verifier.extract_identity_fields(payload)
            run_sha = file_commit or expected_commit
            runs = [
                {"status": "completed", "conclusion": "success", "head_sha": run_sha}
                for _ in range(passed)
            ] + [
                {"status": "completed", "conclusion": "failure", "head_sha": run_sha}
                for _ in range(total - passed)
            ]
            staged = stage_dir / "ci-jobs-builder.json"
            wrapped: dict[str, Any] = {"workflow_runs": runs}
            if file_commit is not None:
                wrapped["release_commit"] = file_commit
            if file_binary is not None:
                wrapped["binary_sha256"] = file_binary
            staged.write_text(json.dumps(wrapped) + "\n", encoding="utf-8")
            return staged
    return ci_path


def _merge_paired_benchmarks_inline(
    bench_files: list[Path],
    commit: str,
    binary_sha256: str | None,
) -> dict[str, Any]:
    """Build a self-contained inline benchmarks_synthetic receipt for persisted manifests."""
    merged_results: list[Any] = []
    for bench_file in bench_files:
        payload = json.loads(bench_file.read_text(encoding="utf-8"))
        rows = payload.get("results")
        if not isinstance(rows, list):
            raise ValueError(f"{bench_file.name}: benchmark receipt must contain a results list.")
        merged_results.extend(rows)
    entry: dict[str, Any] = {
        "release_commit": commit,
        "results": merged_results,
    }
    if binary_sha256 is not None:
        entry["binary_sha256"] = binary_sha256
    return entry


def _build_dashboard_args(
    scan: DirectoryScan,
    stage_dir: Path,
    base_json: Path,
    candidate_json: Path,
    effective_binary: str | None,
    version: str | None,
    known_parity_gaps: list[str],
) -> tuple[list[str], dict[str, Any], dict[str, Any]]:
    """Construct build_dashboard_json.py arguments plus live and persisted manifest entries."""
    cmd = [
        sys.executable,
        str(TOOLS_DIR / "build_dashboard_json.py"),
        "--strict-same-sha",
        "--base",
        str(base_json),
        "--out",
        str(candidate_json),
        "--release-sha",
        scan.expected_commit,
    ]
    if effective_binary is not None:
        cmd.extend(["--binary-sha256", effective_binary])
    if version is not None:
        cmd.extend(["--version", version])
    cmd.append("--known-parity-gaps")
    cmd.extend(known_parity_gaps)

    live_entries: dict[str, Any] = {}
    persisted_entries: dict[str, Any] = {}

    for lane in scan.lanes:
        if lane.path is None or not lane.ready:
            continue
        key = lane.spec.key
        path = lane.path.resolve()

        if key == "large_projects":
            entry = {"path": str(path)}
            live_entries[key] = entry
            persisted_entries[key] = entry
            if path.is_dir():
                cmd.extend(["--large-projects-dir", str(path)])
            else:
                lp_dir = stage_dir / "large-projects"
                lp_dir.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(path, lp_dir / "result.json")
                cmd.extend(["--large-projects-dir", str(lp_dir)])
            continue

        if key == "vcpkg":
            entry = {"path": str(path)}
            live_entries[key] = entry
            persisted_entries[key] = entry
            if path.is_dir():
                cmd.extend(["--vcpkg-dir", str(path)])
            else:
                vcpkg_dir = _stage_vcpkg_summary(path, stage_dir)
                cmd.extend(["--vcpkg-dir", str(vcpkg_dir)])
            continue

        if key == "fuzz":
            entry = {"path": str(path)}
            live_entries[key] = entry
            persisted_entries[key] = entry
            if path.is_dir():
                cmd.extend(["--fuzz-dir", str(path)])
            else:
                fuzz_dir = stage_dir / "fuzz"
                fuzz_dir.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(path, fuzz_dir / path.name)
                cmd.extend(["--fuzz-dir", str(fuzz_dir)])
            continue

        if key == "benchmarks_synthetic":
            if path.is_dir():
                entry = {"path": str(path)}
                live_entries[key] = entry
                persisted_entries[key] = entry
                for member in _member_files_for_directory(path, "*.json"):
                    cmd.extend(["--bench", str(member.resolve())])
            elif lane.companion_paths:
                bench_dir = stage_dir / "benchmarks-synthetic"
                bench_dir.mkdir(parents=True, exist_ok=True)
                all_bench_files = [path, *(p.resolve() for p in lane.companion_paths)]
                for bench_file in all_bench_files:
                    shutil.copyfile(bench_file, bench_dir / bench_file.name)
                    cmd.extend(["--bench", str(bench_file)])
                live_entries[key] = {"path": str(bench_dir)}
                persisted_entries[key] = _merge_paired_benchmarks_inline(
                    all_bench_files,
                    scan.expected_commit,
                    effective_binary,
                )
            else:
                entry = {"path": str(path)}
                live_entries[key] = entry
                persisted_entries[key] = entry
                cmd.extend(["--bench", str(path)])
            continue

        if key == "ci_jobs":
            builder_ci_path = _stage_ci_jobs_for_builder(path, stage_dir, scan.expected_commit)
            cmd.extend(["--ci-status-json", str(builder_ci_path)])
            raw_ci = json.loads(path.read_text(encoding="utf-8"))
            if isinstance(raw_ci, dict):
                entry = {"path": str(path)}
                live_entries[key] = entry
                persisted_entries[key] = entry
            continue

        entry = {"path": str(path)}
        live_entries[key] = entry
        persisted_entries[key] = entry
        if lane.spec.builder_flag is not None:
            cmd.extend([lane.spec.builder_flag, str(path)])

    return cmd, live_entries, persisted_entries


def _build_manifest_payload(
    commit: str,
    binary_sha256: str | None,
    dashboard_path: Path,
    wasm_bundle: Path,
    known_parity_gaps: list[str],
    entries: dict[str, Any],
) -> dict[str, Any]:
    payload: dict[str, Any] = {
        "schema_version": 1,
        "release_commit": commit,
        "generated_at": datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z"),
        "dashboard": str(dashboard_path.resolve()),
        "wasm_bundle": str(wasm_bundle.resolve()),
        "known_parity_gaps": known_parity_gaps,
        "receipts": entries,
    }
    if binary_sha256 is not None:
        payload["binary_sha256"] = binary_sha256
    return payload


def _run_subprocess(cmd: list[str], label: str) -> subprocess.CompletedProcess[str]:
    try:
        return subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True, timeout=120, check=False)
    except (OSError, subprocess.SubprocessError) as error:
        raise RuntimeError(f"Failed to execute {label}: {error}") from None


def run_assemble(args: argparse.Namespace, scan: DirectoryScan) -> int:
    """Execute strict or partial release receipt assembly."""
    if scan.has_blocking_errors:
        sys.stderr.write(format_status_matrix(scan))
        sys.stderr.write(
            "Refusing to assemble: identity or receipt integrity errors detected.\n"
        )
        return 2

    if not args.allow_partial and scan.missing_required:
        sys.stderr.write(format_status_matrix(scan))
        missing = ", ".join(lane.spec.key for lane in scan.missing_required)
        sys.stderr.write(
            f"Refusing strict assembly: {len(scan.missing_required)} required lane(s) missing ({missing}).\n"
            "Pass --allow-partial to build a preview snapshot without strict verification.\n"
        )
        return 2

    effective_binary = scan.requested_binary or scan.inferred_binary

    with TemporaryDirectory(prefix="rcmake-assemble-") as temp_root:
        stage_dir = Path(temp_root)
        candidate_json = stage_dir / "latest.json"

        try:
            build_cmd, live_entries, persisted_entries = _build_dashboard_args(
                scan=scan,
                stage_dir=stage_dir,
                base_json=args.base_json,
                candidate_json=candidate_json,
                effective_binary=effective_binary,
                version=args.version,
                known_parity_gaps=args.known_parity_gaps,
            )
        except (OSError, ValueError, json.JSONDecodeError) as error:
            sys.stderr.write(f"assemble_release_receipts: {error}\n")
            return 2

        build_res = _run_subprocess(build_cmd, "build_dashboard_json.py")
        if build_res.returncode != 0:
            if build_res.stdout:
                sys.stdout.write(build_res.stdout)
            if build_res.stderr:
                sys.stderr.write(build_res.stderr)
            return build_res.returncode

        manifest_path = stage_dir / "manifest.json"
        manifest_payload = _build_manifest_payload(
            commit=scan.expected_commit,
            binary_sha256=effective_binary,
            dashboard_path=candidate_json,
            wasm_bundle=args.wasm_bundle,
            known_parity_gaps=args.known_parity_gaps,
            entries=live_entries,
        )
        _atomic_write_json(manifest_path, manifest_payload)

        if not args.allow_partial:
            verify_cmd = [
                sys.executable,
                str(TOOLS_DIR / "verify_release_receipts.py"),
                "--manifest",
                str(manifest_path),
                "--dashboard",
                str(candidate_json),
                "--wasm-bundle",
                str(args.wasm_bundle),
                "--schema",
                str(args.schema),
                "--expected-commit",
                scan.expected_commit,
            ]
            if effective_binary is not None:
                verify_cmd.extend(["--expected-binary-sha256", effective_binary])
            if args.verify_json_out is not None:
                verify_cmd.extend(["--json-out", str(args.verify_json_out)])

            verify_res = _run_subprocess(verify_cmd, "verify_release_receipts.py")
            if verify_res.returncode != 0:
                if verify_res.stdout:
                    sys.stdout.write(verify_res.stdout)
                if verify_res.stderr:
                    sys.stderr.write(verify_res.stderr)
                return verify_res.returncode
            if verify_res.stdout:
                sys.stdout.write(verify_res.stdout)

        _atomic_copy_file(candidate_json, args.out_json)

        final_manifest_payload = _build_manifest_payload(
            commit=scan.expected_commit,
            binary_sha256=effective_binary,
            dashboard_path=args.out_json,
            wasm_bundle=args.wasm_bundle,
            known_parity_gaps=args.known_parity_gaps,
            entries=persisted_entries,
        )
        if args.manifest_out is not None:
            _atomic_write_json(args.manifest_out, final_manifest_payload)
            stamp_manifest = args.manifest_out
        else:
            _atomic_write_json(manifest_path, final_manifest_payload)
            stamp_manifest = manifest_path

        if args.allow_partial:
            sys.stdout.write(format_status_matrix(scan))
            if scan.missing_required:
                missing = ", ".join(lane.spec.key for lane in scan.missing_required)
                sys.stdout.write(
                    f"Partial preview written to {args.out_json} "
                    f"(strict verify_release_receipts.py skipped; missing: {missing}).\n"
                )
            else:
                sys.stdout.write(
                    f"Preview written to {args.out_json} "
                    "(strict verify_release_receipts.py skipped due to --allow-partial).\n"
                )
        else:
            sys.stdout.write(f"OK: assembled and verified {args.out_json}.\n")

        if args.stamp_docs:
            stamp_common = [
                sys.executable,
                str(TOOLS_DIR / "stamp_metrics.py"),
                "--dashboard",
                str(args.out_json),
                "--schema",
                str(args.schema),
                "--file",
                str(args.readme),
                "--file",
                str(args.writeup),
            ]
            if not args.allow_partial:
                stamp_common.extend(["--receipts", str(stamp_manifest)])
            else:
                stamp_common.append("--allow-partial")

            for mode_flag in ("--write", "--check"):
                stamp_res = _run_subprocess([*stamp_common, mode_flag], f"stamp_metrics.py {mode_flag}")
                if stamp_res.stdout:
                    sys.stdout.write(stamp_res.stdout)
                if stamp_res.stderr:
                    sys.stderr.write(stamp_res.stderr)
                if stamp_res.returncode != 0:
                    return stamp_res.returncode

    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=__doc__,
        epilog=layout_help_text(),
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--receipts-dir",
        type=Path,
        required=True,
        help="Directory containing standardized frozen release receipts.",
    )
    parser.add_argument(
        "--commit",
        default=None,
        help="Expected release commit SHA (7-40 hex chars; defaults to git rev-parse HEAD).",
    )
    parser.add_argument(
        "--binary-sha256",
        default=None,
        help="Optional 64-character SHA-256 hex digest of the frozen release binary.",
    )

    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument(
        "--status",
        action="store_true",
        help="Print readiness matrix for every required lane without modifying any files (exit 0 ready, 2 incomplete).",
    )
    mode.add_argument(
        "--assemble",
        action="store_true",
        help="Verify SHAs, build dashboard JSON via build_dashboard_json.py --strict-same-sha, and run verification.",
    )

    parser.add_argument(
        "--allow-partial",
        action="store_true",
        help="With --assemble, write a preview dashboard JSON while reporting missing lanes and skipping strict verification.",
    )
    parser.add_argument(
        "--out-json",
        type=Path,
        default=DEFAULT_OUT_JSON,
        help=f"Output path for the assembled dashboard JSON (default: {DEFAULT_OUT_JSON.relative_to(ROOT)}).",
    )
    parser.add_argument(
        "--base-json",
        type=Path,
        default=DEFAULT_OUT_JSON,
        help=f"Base dashboard template passed to build_dashboard_json.py --base (default: {DEFAULT_OUT_JSON.relative_to(ROOT)}).",
    )
    parser.add_argument(
        "--schema",
        type=Path,
        default=DEFAULT_SCHEMA,
        help=f"Path to dashboard.schema.json (default: {DEFAULT_SCHEMA.relative_to(ROOT)}).",
    )
    parser.add_argument(
        "--wasm-bundle",
        type=Path,
        default=DEFAULT_WASM_BUNDLE,
        help=f"Path to browser playground WASM bundle (default: {DEFAULT_WASM_BUNDLE.relative_to(ROOT)}).",
    )
    parser.add_argument(
        "--version",
        default=None,
        help="Optional release version forwarded to build_dashboard_json.py --version.",
    )
    parser.add_argument(
        "--known-parity-gaps",
        nargs="*",
        default=[],
        help="Known parity gaps forwarded to build_dashboard_json.py (default: none).",
    )
    parser.add_argument(
        "--manifest-out",
        type=Path,
        default=None,
        help="Optional output path to persist the assembled release receipt manifest JSON.",
    )
    parser.add_argument(
        "--verify-json-out",
        type=Path,
        default=None,
        help="Optional output path forwarded to verify_release_receipts.py --json-out.",
    )
    parser.add_argument(
        "--stamp-docs",
        action="store_true",
        help="With --assemble, run stamp_metrics.py --write and --check against --readme and --writeup.",
    )
    parser.add_argument(
        "--readme",
        type=Path,
        default=DEFAULT_README,
        help=f"README path for --stamp-docs (default: {DEFAULT_README.relative_to(ROOT)}).",
    )
    parser.add_argument(
        "--writeup",
        type=Path,
        default=DEFAULT_WRITEUP,
        help=f"Build writeup path for --stamp-docs (default: {DEFAULT_WRITEUP.relative_to(ROOT)}).",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)

    if args.status and args.allow_partial:
        parser.error("--allow-partial can only be used with --assemble.")
    if args.status and args.stamp_docs:
        parser.error("--stamp-docs can only be used with --assemble.")

    if not args.receipts_dir.is_dir():
        sys.stderr.write(f"assemble_release_receipts: receipts directory does not exist: {args.receipts_dir}\n")
        return 2

    try:
        commit = args.commit if args.commit is not None else resolve_default_commit()
        scan = scan_receipts_dir(args.receipts_dir, commit, args.binary_sha256)
    except ValueError as error:
        sys.stderr.write(f"assemble_release_receipts: {error}\n")
        return 2

    if args.status:
        sys.stdout.write(format_status_matrix(scan))
        return 0 if scan.is_complete else 2

    try:
        return run_assemble(args, scan)
    except RuntimeError as error:
        sys.stderr.write(f"assemble_release_receipts: {error}\n")
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
