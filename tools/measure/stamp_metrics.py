#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Stamp or verify machine-readable METRIC markers in README.md and docs/BUILD_WRITEUP.md.

Reads verified values from site/data/latest.json and an optional frozen release
receipt manifest, verifies commit and binary_sha256 consistency across inputs,
and either checks (--check) or updates (--write) every <!-- METRIC:<key> -->
marker deterministically and idempotently.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import json
from pathlib import Path
import re
from stat import S_IMODE
import sys
from tempfile import NamedTemporaryFile
from typing import Any

if __package__:
    from .build_dashboard_json import validate as validate_schema
    from .verify_release_receipts import (
        check_identity,
        is_int,
        is_number,
        load_receipt_entry,
        normalize_commit,
        normalize_sha256,
        read_json_object,
    )
else:
    from build_dashboard_json import validate as validate_schema
    from verify_release_receipts import (
        check_identity,
        is_int,
        is_number,
        load_receipt_entry,
        normalize_commit,
        normalize_sha256,
        read_json_object,
    )


ROOT = Path(__file__).resolve().parents[2]
DEFAULT_DASHBOARD = ROOT / "site" / "data" / "latest.json"
DEFAULT_SCHEMA = ROOT / "site" / "data" / "dashboard.schema.json"
DEFAULT_FILES = (ROOT / "README.md", ROOT / "docs" / "BUILD_WRITEUP.md")

EXPECTED_KEYS = frozenset({
    "release_commit",
    "release_date",
    "ci_jobs",
    "runcmake_macos",
    "runcmake_linux",
    "runcmake_windows",
    "runcmake_core62",
    "runcmake_policies",
    "independent_scenarios",
    "large_projects_l1",
    "large_projects_l2",
    "unseen_holdout",
    "vcpkg_matrix",
    "conan_ecosystem",
    "path_swap_eval",
    "probe_cache_soundness",
    "mutation_replay_soundness",
    "fuzz_failures",
    "wasm_smoke",
    "bench_cold_configure",
    "bench_warm_probe",
    "bench_incremental_leaf_p95",
    "bench_peak_rss",
    "binary_size_macos",
    "known_parity_gaps",
})

MARKER_RE = re.compile(r"<!--\s*METRIC:([a-z0-9_]+)\s*-->(.*?)<!--\s*/METRIC\s*-->", re.DOTALL)
OPEN_TAG_RE = re.compile(r"<!--\s*METRIC:[^>]*-->")
CLOSE_TAG_RE = re.compile(r"<!--\s*/METRIC\s*-->")
PENDING_VALUE_RE = re.compile(r"\bpending\b|\[FINAL_METRIC\]|\bTBD\b", re.IGNORECASE)
CI_NOTE_RE = re.compile(r"CI reports\s+([\d,]+)\s+of\s+([\d,]+)\s+completed runs as successful")
ISO_DATE_RE = re.compile(r"^\d{4}-\d{2}-\d{2}")


@dataclass(frozen=True)
class MarkerOccurrence:
    key: str
    value: str
    start: int
    end: int


@dataclass(frozen=True)
class StampIssue:
    path: str
    key: str
    kind: str
    message: str


@dataclass(frozen=True)
class StampReport:
    resolved: dict[str, str]
    missing: dict[str, str]
    issues: tuple[StampIssue, ...]
    changed_files: tuple[Path, ...]

    @property
    def ok(self) -> bool:
        return not self.issues


def scan_markers(text: str, path_label: str) -> list[MarkerOccurrence]:
    """Parse every METRIC marker in a Markdown document and reject malformed tags."""
    opens = OPEN_TAG_RE.findall(text)
    closes = CLOSE_TAG_RE.findall(text)
    matches = list(MARKER_RE.finditer(text))
    if len(opens) != len(matches) or len(closes) != len(matches):
        raise ValueError(
            f"{path_label}: malformed METRIC markers "
            f"(open={len(opens)}, close={len(closes)}, matched={len(matches)})."
        )
    occurrences: list[MarkerOccurrence] = []
    for match in matches:
        key = match.group(1)
        if key not in EXPECTED_KEYS:
            raise ValueError(f"{path_label}: unknown METRIC key '{key}'.")
        occurrences.append(MarkerOccurrence(
            key=key,
            value=match.group(2),
            start=match.start(),
            end=match.end(),
        ))
    return occurrences


def _find_gate(dashboard: dict[str, Any], *ids: str) -> dict[str, Any] | None:
    gates = dashboard.get("tracks", {}).get("benchmarksAndGates", {}).get("gates", [])
    for target_id in ids:
        for row in gates:
            if isinstance(row, dict) and row.get("id") == target_id:
                return row
    return None


def _load_receipts_bundle(
    manifest_path: Path,
) -> tuple[dict[str, Any], dict[str, dict[str, Any]], list[str]]:
    """Load the receipt manifest, resolve file entries, and verify single-SHA integrity."""
    manifest = read_json_object(manifest_path)
    base_dir = manifest_path.parent
    errors: list[str] = []

    raw_commit = manifest.get("release_commit", manifest.get("commit"))
    raw_binary = manifest.get("binary_sha256", manifest.get("binarySha256"))
    manifest_commit = normalize_commit(raw_commit)
    manifest_binary = normalize_sha256(raw_binary)
    if manifest_commit is None:
        errors.append(f"receipts manifest has invalid or missing release_commit: {raw_commit!r}")
    if raw_binary is not None and manifest_binary is None:
        errors.append(f"receipts manifest has invalid binary_sha256: {raw_binary!r}")

    loaded: dict[str, dict[str, Any]] = {}
    receipts_map = manifest.get("receipts", {})
    if isinstance(receipts_map, dict):
        for key, entry in receipts_map.items():
            try:
                record = load_receipt_entry(key, "receipt", entry, base_dir)
            except (OSError, ValueError, json.JSONDecodeError) as error:
                errors.append(f"receipt '{key}' could not be loaded: {error}")
                continue
            loaded[key] = record.payload
            findings = []
            check_identity(record, manifest_commit, manifest_binary, findings)
            errors.extend(
                f"Receipt '{finding.receipt}' does not match the manifest. {finding.detail}"
                for finding in findings
            )

    return manifest, loaded, errors


def check_source_identity(
    dashboard: dict[str, Any],
    receipts_manifest: dict[str, Any] | None,
    receipt_errors: list[str],
    allow_partial: bool,
) -> list[StampIssue]:
    """Validate commit, timestamp, and cross-file SHA agreement before stamping."""
    issues: list[StampIssue] = [
        StampIssue(path="receipts", key="identity", kind="mismatch", message=msg)
        for msg in receipt_errors
    ]

    top_commit_raw = dashboard.get("commit")
    top_commit_str = top_commit_raw.strip().lower() if isinstance(top_commit_raw, str) and top_commit_raw.strip() else None
    dash_commit = normalize_commit(top_commit_str)

    top_binary_raw = dashboard.get("binarySha256")
    receipt_block = dashboard.get("receipt") if isinstance(dashboard.get("receipt"), dict) else {}
    nested_commit_raw = receipt_block.get("commit")
    nested_binary_raw = receipt_block.get("binarySha256")
    dash_binary_raw = top_binary_raw if isinstance(top_binary_raw, str) and top_binary_raw.strip() else (
        nested_binary_raw if isinstance(nested_binary_raw, str) and nested_binary_raw.strip() else None
    )
    dash_binary = normalize_sha256(dash_binary_raw)

    if top_commit_str is None:
        if not allow_partial:
            issues.append(StampIssue(
                path="dashboard",
                key="release_commit",
                kind="missing",
                message="dashboard.json has commit: null; refuse to stamp without --allow-partial.",
            ))
    elif dash_commit is None:
        issues.append(StampIssue(
            path="dashboard",
            key="release_commit",
            kind="invalid",
            message=f"dashboard.json commit is not a valid hex SHA: {top_commit_raw!r}.",
        ))

    if isinstance(nested_commit_raw, str) and nested_commit_raw.strip():
        nested_commit = normalize_commit(nested_commit_raw)
        if dash_commit is not None and nested_commit != dash_commit:
            issues.append(StampIssue(
                path="dashboard",
                key="release_commit",
                kind="mismatch",
                message=f"dashboard.receipt.commit {nested_commit_raw!r} disagrees with dashboard.commit {dash_commit}.",
            ))

    if (
        isinstance(top_binary_raw, str) and top_binary_raw.strip()
        and isinstance(nested_binary_raw, str) and nested_binary_raw.strip()
        and top_binary_raw.strip().lower() != nested_binary_raw.strip().lower()
    ):
        issues.append(StampIssue(
            path="dashboard",
            key="binary_sha256",
            kind="mismatch",
            message="dashboard.receipt.binarySha256 disagrees with dashboard.binarySha256.",
        ))

    generated_at = dashboard.get("generatedAt")
    if not isinstance(generated_at, str) or not ISO_DATE_RE.match(generated_at):
        if not allow_partial:
            issues.append(StampIssue(
                path="dashboard",
                key="release_date",
                kind="missing",
                message=f"dashboard.json generatedAt is null or not ISO-8601: {generated_at!r}.",
            ))

    if dash_binary_raw is not None and dash_binary is None:
        issues.append(StampIssue(
            path="dashboard",
            key="binary_sha256",
            kind="invalid",
            message=f"dashboard.json binarySha256 is invalid: {dash_binary_raw!r}.",
        ))

    if receipts_manifest is not None:
        man_commit = normalize_commit(receipts_manifest.get("release_commit", receipts_manifest.get("commit")))
        man_binary = normalize_sha256(receipts_manifest.get("binary_sha256", receipts_manifest.get("binarySha256")))
        if dash_commit is not None and man_commit is not None and dash_commit != man_commit:
            issues.append(StampIssue(
                path="dashboard",
                key="release_commit",
                kind="mismatch",
                message=f"dashboard commit {dash_commit} does not match receipts manifest {man_commit}.",
            ))
        if dash_binary is not None and man_binary is not None and dash_binary != man_binary:
            issues.append(StampIssue(
                path="dashboard",
                key="binary_sha256",
                kind="mismatch",
                message=f"dashboard binarySha256 {dash_binary} does not match receipts manifest {man_binary}.",
            ))

    return issues


def _format_rate_suites(
    pass_rate: float,
    suites_passed: int,
    suites_total: int,
    cases_passed: int | None,
    cases_total: int | None,
    suites_label: str = "suites",
) -> str:
    if is_int(cases_passed) and is_int(cases_total) and cases_total > 0:
        return (
            f"{pass_rate:.2f}% ({suites_passed:,} / {suites_total:,} {suites_label}, "
            f"{cases_passed:,} / {cases_total:,} cases)"
        )
    return f"{pass_rate:.2f}% ({suites_passed:,} / {suites_total:,} {suites_label})"


def _format_binary_size(raw: Any) -> str | None:
    if is_int(raw) and raw > 0:
        return f"{raw / (1024 * 1024):.2f} MiB ({raw:,} bytes)"
    if isinstance(raw, str) and raw.strip() and not PENDING_VALUE_RE.search(raw):
        return raw.strip()
    return None


def _format_known_gaps(raw: Any) -> str | None:
    if raw is None:
        return None
    if isinstance(raw, list):
        cleaned = [str(item).strip() for item in raw if str(item).strip()]
        if not cleaned:
            return "none"
        joined = "; ".join(cleaned)
        return None if PENDING_VALUE_RE.search(joined) else joined
    if isinstance(raw, str):
        cleaned = raw.strip()
        if not cleaned or PENDING_VALUE_RE.search(cleaned):
            return None
        return cleaned
    return None


def _extract_ci_jobs(
    dashboard: dict[str, Any],
    receipts_manifest: dict[str, Any] | None,
    receipts: dict[str, dict[str, Any]],
) -> str | None:
    if receipts_manifest is not None:
        ci_block = receipts.get("ci_jobs") or receipts_manifest.get("ci_jobs")
        if isinstance(ci_block, dict):
            passed = ci_block.get("passed")
            total = ci_block.get("total")
            if is_int(passed) and is_int(total) and total > 0 and 0 <= passed <= total:
                if passed == total:
                    return f"{passed:,} of {total:,} jobs green"
                return f"{passed:,} of {total:,} jobs green ({total - passed:,} failed)"
        elif isinstance(ci_block, str) and ci_block.strip() and not PENDING_VALUE_RE.search(ci_block):
            return ci_block.strip()

    gate = _find_gate(dashboard, "ci-jobs", "ci")
    if gate is not None and gate.get("status") != "pending":
        passed, total = gate.get("passed"), gate.get("total")
        if is_int(passed) and is_int(total) and total > 0:
            if passed == total:
                return f"{passed:,} of {total:,} jobs green"
            return f"{passed:,} of {total:,} jobs green ({total - passed:,} failed)"

    note = dashboard.get("statusNote")
    if isinstance(note, str):
        match = CI_NOTE_RE.search(note)
        if match:
            passed = int(match.group(1).replace(",", ""))
            total = int(match.group(2).replace(",", ""))
            if total > 0:
                if passed == total:
                    return f"{passed:,} of {total:,} jobs green"
                return f"{passed:,} of {total:,} jobs green ({total - passed:,} failed)"
    return None


def _extract_conan(
    dashboard: dict[str, Any],
    receipts: dict[str, dict[str, Any]],
) -> str | None:
    gate = _find_gate(dashboard, "conan-ecosystem", "conan", "ecosystem")
    if gate is not None and gate.get("status") != "pending":
        passed, total = gate.get("passed"), gate.get("total")
        if is_int(passed) and is_int(total) and total > 0:
            return f"{passed:,} / {total:,} scenarios passed"

    conan = receipts.get("conan") or receipts.get("conan_ecosystem") or receipts.get("ecosystem")
    if isinstance(conan, dict):
        scenarios = conan.get("scenarios")
        if isinstance(scenarios, list) and scenarios:
            executed = [row for row in scenarios if isinstance(row, dict) and row.get("status") != "skipped_missing_tool"]
            passed = sum(1 for row in executed if row.get("status") == "pass")
            if executed:
                return f"{passed:,} / {len(executed):,} scenarios passed"
        passed, total = conan.get("passed"), conan.get("total")
        if is_int(passed) and is_int(total) and total > 0:
            return f"{passed:,} / {total:,} scenarios passed"
    return None


def _extract_path_swap(
    dashboard: dict[str, Any],
    receipts: dict[str, dict[str, Any]],
) -> str | None:
    gate = _find_gate(dashboard, "path-swap", "determinism")
    if gate is not None and gate.get("status") != "pending":
        passed, total = gate.get("passed"), gate.get("total")
        if gate.get("status") == "pass" and is_int(passed) and is_int(total) and total == 1 and passed == 1:
            return "pass (0 differences)"
        if is_int(passed) and is_int(total) and total > 0:
            return f"{passed:,} / {total:,} passed"

    swap = receipts.get("path_swap")
    if isinstance(swap, dict):
        status = swap.get("status")
        diffs = swap.get("differences")
        if status == "pass" and (diffs is None or diffs == []):
            return "pass (0 differences)"
        if isinstance(status, str) and status and not PENDING_VALUE_RE.search(status):
            count_diffs = len(diffs) if isinstance(diffs, list) else 0
            return f"{status} ({count_diffs:,} differences)"
    return None


def extract_metrics(
    dashboard: dict[str, Any],
    receipts_manifest: dict[str, Any] | None = None,
    receipts: dict[str, dict[str, Any]] | None = None,
) -> tuple[dict[str, str], dict[str, str]]:
    """Derive formatted markdown strings for all 25 known METRIC keys."""
    receipts = receipts or {}
    tracks = dashboard.get("tracks", {})
    resolved: dict[str, str] = {}
    missing: dict[str, str] = {}

    def record(key: str, value: str | None, reason: str) -> None:
        if value is None:
            missing[key] = reason
            return
        if "\n" in value or "<!--" in value or "-->" in value or PENDING_VALUE_RE.search(value):
            missing[key] = f"resolved value for {key} is pending or contains forbidden markup"
            return
        resolved[key] = value

    commit = normalize_commit(dashboard.get("commit"))
    record("release_commit", f"`{commit[:12]}`" if commit else None, "dashboard.commit is null or invalid")

    generated_at = dashboard.get("generatedAt")
    date_val = f"`{generated_at[:10]}`" if isinstance(generated_at, str) and ISO_DATE_RE.match(generated_at) else None
    record("release_date", date_val, "dashboard.generatedAt is null or not ISO-8601")

    record("ci_jobs", _extract_ci_jobs(dashboard, receipts_manifest, receipts), "CI job pass counts unavailable")

    platforms = {
        row.get("id"): row
        for row in tracks.get("runCmakeOs", {}).get("platforms", [])
        if isinstance(row, dict)
    }
    for key, plat_id in (
        ("runcmake_macos", "macos-arm64"),
        ("runcmake_linux", "linux-x86_64"),
        ("runcmake_windows", "windows-x86_64"),
    ):
        row = platforms.get(plat_id, {})
        if (
            row.get("status") != "pending"
            and is_number(row.get("passRate"))
            and is_int(row.get("suitesPassed"))
            and is_int(row.get("suitesTotal"))
        ):
            text = _format_rate_suites(
                row["passRate"], row["suitesPassed"], row["suitesTotal"],
                row.get("casesPassed"), row.get("casesTotal"),
            )
            record(key, text, "")
        else:
            record(key, None, f"platform {plat_id} is pending or missing counts")

    core = tracks.get("coreConformance", {})
    if (
        core.get("status") != "pending"
        and is_number(core.get("passRate"))
        and is_int(core.get("suitesAt100"))
        and is_int(core.get("suitesTotal"))
    ):
        record(
            "runcmake_core62",
            _format_rate_suites(
                core["passRate"], core["suitesAt100"], core["suitesTotal"],
                core.get("casesPassed"), core.get("casesTotal"), "suites at 100%",
            ),
            "",
        )
    else:
        record("runcmake_core62", None, "coreConformance is pending or missing counts")

    policies = tracks.get("policies", {})
    if (
        policies.get("status") != "pending"
        and is_number(policies.get("passRate"))
        and is_int(policies.get("suitesPassed"))
        and is_int(policies.get("suitesTotal"))
    ):
        record(
            "runcmake_policies",
            _format_rate_suites(
                policies["passRate"], policies["suitesPassed"], policies["suitesTotal"],
                policies.get("casesPassed"), policies.get("casesTotal"),
            ),
            "",
        )
    else:
        record("runcmake_policies", None, "policies is pending or missing counts")

    blackbox = _find_gate(dashboard, "blackbox")
    if (
        blackbox is not None
        and blackbox.get("status") != "pending"
        and is_int(blackbox.get("scenariosPassed"))
        and is_int(blackbox.get("scenariosTotal"))
        and is_int(blackbox.get("passed"))
        and is_int(blackbox.get("total"))
    ):
        record(
            "independent_scenarios",
            f"{blackbox['scenariosPassed']:,} / {blackbox['scenariosTotal']:,} scenarios "
            f"({blackbox['passed']:,} / {blackbox['total']:,} checks passed)",
            "",
        )
    else:
        record("independent_scenarios", None, "blackbox gate is pending or missing counts")

    boss = tracks.get("bossFight", {})
    projects_total = boss.get("projectsTotal")
    if boss.get("status") != "pending" and is_int(boss.get("l1Passed")) and is_int(projects_total):
        record("large_projects_l1", f"{boss['l1Passed']:,} / {projects_total:,} passed", "")
    else:
        record("large_projects_l1", None, "bossFight L1 is pending or missing counts")

    if boss.get("status") != "pending" and is_int(boss.get("l2Passed")) and is_int(projects_total):
        record("large_projects_l2", f"{boss['l2Passed']:,} / {projects_total:,} passed", "")
    else:
        record("large_projects_l2", None, "bossFight L2 is pending or missing counts")

    holdout = tracks.get("unseenHoldout", {})
    if (
        holdout.get("sealed") is True
        and holdout.get("status") != "pending"
        and is_number(holdout.get("passRate"))
        and is_int(holdout.get("reposPassed"))
        and is_int(holdout.get("reposTotal"))
    ):
        record(
            "unseen_holdout",
            f"{holdout['passRate']:.2f}% ({holdout['reposPassed']:,} / {holdout['reposTotal']:,} repositories)",
            "",
        )
    else:
        record("unseen_holdout", None, "unseenHoldout is pending or missing aggregate counts")

    vcpkg = tracks.get("vcpkgMatrix", {})
    if (
        vcpkg.get("status") != "pending"
        and is_number(vcpkg.get("passRate"))
        and is_int(vcpkg.get("portsPassed"))
        and is_int(vcpkg.get("portsTotal"))
        and is_int(vcpkg.get("shardsCompleted"))
        and is_int(vcpkg.get("shardsTotal"))
    ):
        inspected = vcpkg.get("portsInspected")
        inspected_note = f". {inspected:,} ports inspected" if is_int(inspected) else ""
        record(
            "vcpkg_matrix",
            f"{vcpkg['passRate']:.2f}% ({vcpkg['portsPassed']:,} / {vcpkg['portsTotal']:,} scored CMake ports "
            f"across {vcpkg['shardsCompleted']:,} / {vcpkg['shardsTotal']:,} shards){inspected_note}",
            "",
        )
    else:
        record("vcpkg_matrix", None, "vcpkgMatrix is pending or missing counts")

    record("conan_ecosystem", _extract_conan(dashboard, receipts), "Conan ecosystem counts unavailable")
    record("path_swap_eval", _extract_path_swap(dashboard, receipts), "PATH swap status unavailable")

    probe = _find_gate(dashboard, "probe-cache")
    probe_text = None
    if probe is not None and probe.get("status") != "pending" and is_int(probe.get("passed")) and is_int(probe.get("total")):
        probe_text = f"{probe['passed']:,} / {probe['total']:,} checks passed"
    elif isinstance(receipts.get("probe_cache"), dict):
        cases = receipts["probe_cache"].get("cases")
        if isinstance(cases, list) and cases:
            ran = [row for row in cases if isinstance(row, dict) and row.get("status") != "skipped"]
            passed = sum(1 for row in ran if row.get("passed") is True)
            if ran:
                probe_text = f"{passed:,} / {len(ran):,} checks passed"
    record("probe_cache_soundness", probe_text, "probe-cache gate is pending or missing counts")

    mutation = _find_gate(dashboard, "mutation-replay")
    mutation_text = None
    if (
        mutation is not None
        and mutation.get("status") != "pending"
        and is_int(mutation.get("passed"))
        and is_int(mutation.get("total"))
    ):
        mutation_text = f"{mutation['passed']:,} / {mutation['total']:,} edits verified"
    elif isinstance(receipts.get("mutation_replay"), dict):
        steps = receipts["mutation_replay"].get("steps")
        if isinstance(steps, list):
            edits = [row for row in steps if isinstance(row, dict) and is_int(row.get("step")) and row["step"] > 0]
            passed = sum(1 for row in edits if row.get("passed") is True)
            if edits:
                mutation_text = f"{passed:,} / {len(edits):,} edits verified"
    record("mutation_replay_soundness", mutation_text, "mutation-replay gate is pending or missing counts")

    fuzz = _find_gate(dashboard, "fuzz")
    if (
        fuzz is not None
        and fuzz.get("status") != "pending"
        and is_int(fuzz.get("total"))
        and is_int(fuzz.get("failures"))
    ):
        record("fuzz_failures", f"{fuzz['total']:,} inputs, {fuzz['failures']:,} failures", "")
    else:
        record("fuzz_failures", None, "fuzz gate is pending or missing counts")

    wasi = _find_gate(dashboard, "wasi")
    if wasi is not None and wasi.get("status") != "pending" and is_int(wasi.get("passed")) and is_int(wasi.get("total")):
        record("wasm_smoke", f"{wasi['passed']:,} of {wasi['total']:,} checks passed", "")
    else:
        record("wasm_smoke", None, "wasi gate is pending or missing counts")

    benchmarks = tracks.get("benchmarksAndGates", {}).get("benchmarks", {})
    for key, bench_key, fmt in (
        ("bench_cold_configure", "coldConfigureSpeedup", lambda v: f"{v:.2f}x"),
        ("bench_warm_probe", "warmProbeSpeedup", lambda v: f"{v:.2f}x"),
        ("bench_incremental_leaf_p95", "incrementalLeafP95Ms", lambda v: f"{v:.1f} ms"),
        ("bench_peak_rss", "peakRssMiB", lambda v: f"{v:.1f} MiB"),
    ):
        row = benchmarks.get(bench_key, {})
        if row.get("status") != "pending" and is_number(row.get("value")):
            record(key, fmt(row["value"]), "")
        else:
            record(key, None, f"benchmark {bench_key} is pending or missing value")

    binary_size_raw = None
    if receipts_manifest is not None:
        binary_size_raw = (
            receipts_manifest.get("binary_size_macos")
            or receipts_manifest.get("binary_size_macos_bytes")
            or receipts_manifest.get("binary_size_bytes")
        )
        if binary_size_raw is None and isinstance(receipts.get("dist"), dict):
            binary_size_raw = receipts["dist"].get("binary_size_bytes")
    if binary_size_raw is None and isinstance(dashboard.get("receipt"), dict):
        binary_size_raw = dashboard["receipt"].get("binarySizeBytes")
    record("binary_size_macos", _format_binary_size(binary_size_raw), "macOS release binary size unavailable")

    gaps_raw = None
    if receipts_manifest is not None and "known_parity_gaps" in receipts_manifest:
        gaps_raw = receipts_manifest["known_parity_gaps"]
    elif "knownParityGaps" in dashboard:
        gaps_raw = dashboard["knownParityGaps"]
    record("known_parity_gaps", _format_known_gaps(gaps_raw), "known_parity_gaps not recorded in receipts or dashboard")

    return resolved, missing


def atomic_write_text(path: Path, content: str) -> None:
    temporary: Path | None = None
    try:
        with NamedTemporaryFile(mode="w", encoding="utf-8", dir=path.parent, delete=False, newline="") as stream:
            temporary = Path(stream.name)
            stream.write(content)
        if path.exists():
            temporary.chmod(S_IMODE(path.stat().st_mode))
        temporary.replace(path)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def run_stamp(
    dashboard_path: Path,
    files: tuple[Path, ...],
    mode: str,
    receipts_path: Path | None = None,
    schema_path: Path = DEFAULT_SCHEMA,
    allow_partial: bool = False,
) -> StampReport:
    """Validate inputs, inspect all target markers, and check or write updates."""
    dashboard = read_json_object(dashboard_path)
    schema = read_json_object(schema_path)
    validate_schema(dashboard, schema, schema)

    receipts_manifest: dict[str, Any] | None = None
    receipts: dict[str, dict[str, Any]] = {}
    receipt_errors: list[str] = []
    if receipts_path is not None:
        receipts_manifest, receipts, receipt_errors = _load_receipts_bundle(receipts_path)

    issues = check_source_identity(dashboard, receipts_manifest, receipt_errors, allow_partial)
    resolved, missing = extract_metrics(dashboard, receipts_manifest, receipts)

    file_contents: dict[Path, str] = {}
    file_markers: dict[Path, list[MarkerOccurrence]] = {}
    for target in files:
        text = target.read_text(encoding="utf-8")
        markers = scan_markers(text, str(target))
        file_contents[target] = text
        file_markers[target] = markers

    for target, markers in file_markers.items():
        for marker in markers:
            if marker.key not in resolved:
                if not allow_partial:
                    reason = missing.get(marker.key, "no value available")
                    issues.append(StampIssue(
                        path=str(target),
                        key=marker.key,
                        kind="missing",
                        message=f"unresolved metric ({reason}).",
                    ))
                continue
            expected = resolved[marker.key]
            if mode == "check":
                if PENDING_VALUE_RE.search(marker.value):
                    issues.append(StampIssue(
                        path=str(target),
                        key=marker.key,
                        kind="pending",
                        message=f"marker still contains pending text {marker.value!r}; expected {expected!r}.",
                    ))
                elif marker.value != expected:
                    issues.append(StampIssue(
                        path=str(target),
                        key=marker.key,
                        kind="drift",
                        message=f"expected {expected!r}, found {marker.value!r}.",
                    ))

    changed: list[Path] = []
    if mode == "write" and not issues:
        for target, text in file_contents.items():
            def replace_marker(match: re.Match[str]) -> str:
                key = match.group(1)
                if key not in resolved:
                    return match.group(0)
                return f"<!-- METRIC:{key} -->{resolved[key]}<!-- /METRIC -->"

            updated = MARKER_RE.sub(replace_marker, text)
            if updated != text:
                atomic_write_text(target, updated)
                changed.append(target)

    return StampReport(
        resolved=resolved,
        missing=missing,
        issues=tuple(issues),
        changed_files=tuple(changed),
    )


def format_stamp_report(report: StampReport, mode: str, files: tuple[Path, ...]) -> str:
    if report.ok:
        if mode == "check":
            return f"OK: verified {len(report.resolved)} metrics across {len(files)} file(s).\n"
        changed_note = (
            ", ".join(str(p) for p in report.changed_files) if report.changed_files else "no changes needed"
        )
        return f"OK: stamped {len(report.resolved)} metrics ({changed_note}).\n"

    lines = [f"FAIL: {len(report.issues)} metric stamping issue(s):"]
    for issue in report.issues:
        lines.append(f"  - [{issue.kind}] {issue.path}:{issue.key}: {issue.message}")
    return "\n".join(lines) + "\n"


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--dashboard",
        type=Path,
        default=DEFAULT_DASHBOARD,
        help="Path to site/data/latest.json.",
    )
    parser.add_argument(
        "--receipts",
        type=Path,
        default=None,
        help="Optional path to frozen release receipt manifest JSON.",
    )
    parser.add_argument(
        "--schema",
        type=Path,
        default=DEFAULT_SCHEMA,
        help="Path to dashboard.schema.json.",
    )
    parser.add_argument(
        "--file",
        dest="files",
        type=Path,
        action="append",
        help="Target Markdown file containing METRIC markers (repeatable; defaults to README.md and docs/BUILD_WRITEUP.md).",
    )
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--check", action="store_true", help="Verify all markers match with zero pending values.")
    mode.add_argument("--write", action="store_true", help="Update target Markdown files in place deterministically.")
    parser.add_argument(
        "--allow-partial",
        action="store_true",
        help="Allow null commit/timestamp and unresolved keys for staging tests (never bypasses SHA mismatches).",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    files = tuple(args.files) if args.files else DEFAULT_FILES
    mode = "check" if args.check else "write"

    try:
        report = run_stamp(
            dashboard_path=args.dashboard,
            files=files,
            mode=mode,
            receipts_path=args.receipts,
            schema_path=args.schema,
            allow_partial=args.allow_partial,
        )
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"stamp_metrics: {error}", file=sys.stderr)
        return 2

    text = format_stamp_report(report, mode, files)
    stream = sys.stdout if report.ok else sys.stderr
    stream.write(text)
    return 0 if report.ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
