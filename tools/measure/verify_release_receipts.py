#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Verify a frozen release receipt manifest or receipt directory across all 6 Launch Bar tracks.

Checks that every receipt file and the compatibility dashboard snapshot share
one exact release_commit SHA and one exact binary_sha256 digest, and enforces
completeness and pass thresholds across all required tracks:

  1. 3-OS Tests/RunCMake (>= 99.0% on macOS 371 / Linux 362 / Windows 367) and
     20/20 independent black-box differential scenarios.
  2. Large C++ projects (10/10 L1 and L2), 64-shard vcpkg coverage, Conan
     ecosystem scenarios, and sealed unseen holdout aggregates (zero leaks).
  3. Real-world giant and synthetic configure/replay benchmarks.
  4. Probe cache (16/16), mutation replay (20/20), 14M adversarial fuzzing, and
     seeded differential fuzzing (oracle_prop_test.py).
  5. WASI 6/6 checks and site/wasm/rcmake_wasm.wasm bundle presence.
  6. CLI UX checks, silent PATH swap, and dist.sh tarball SHA-256 verification.
"""

from __future__ import annotations

import argparse
from collections import Counter
from dataclasses import dataclass
import hashlib
import json
import math
from pathlib import Path
import re
import sys
from typing import Any, Callable

if __package__:
    from . import build_dashboard_json as builder
else:
    import build_dashboard_json as builder

validate_schema = builder.validate


ROOT = Path(__file__).resolve().parents[2]
DEFAULT_DASHBOARD = ROOT / "site" / "data" / "latest.json"
DEFAULT_SCHEMA = ROOT / "site" / "data" / "dashboard.schema.json"
DEFAULT_WASM_BUNDLE = ROOT / "site" / "wasm" / "rcmake_wasm.wasm"

HEX_COMMIT_RE = re.compile(r"^[0-9a-f]{7,40}$")
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
WASM_V1_MAGIC = b"\x00asm\x01\x00\x00\x00"

RUNCMAKE_MIN_PASS_RATE = 99.0
RUNCMAKE_MIN_SUITES = {
    "runcmake_macos": 371,
    "runcmake_linux": 362,
    "runcmake_windows": 367,
}
INDEPENDENT_MIN_SCENARIOS = 20
LARGE_PROJECTS_MIN_COUNT = 10
VCPKG_REQUIRED_SHARDS = 64
UNSEEN_MIN_REPOS = builder.HOLDOUT_MIN_REPOS
UNSEEN_MIN_PASS_RATE = builder.HOLDOUT_MIN_PASS_RATE
PROBE_CACHE_MIN_CHECKS = 16
MUTATION_MIN_STEPS = 20
FUZZ_MIN_ITERATIONS = 14_000_000
WASI_MIN_CHECKS = 6
REQUIRED_CONAN_SCENARIOS = ("conan-consumer", "conan-source-build")

HOLDOUT_FORBIDDEN_KEYS = frozenset({
    "projects", "project", "repos", "repo", "repositories", "repository", "name",
    "repo_name", "repo_names", "repository_name", "repository_names",
    "url", "urls", "git_url", "clone_url", "github",
})
HOLDOUT_LEAK_VALUE_RE = re.compile(
    r"(?:https?://|git@|github\.com|gitlab\.com|bitbucket\.org|\.git(?:\b|/|$)"
    r"|(?<![\w./+-])[\w.-]+/[\w.-]+(?![\w./+-]))",
    re.IGNORECASE,
)

DISCOVERY_CANDIDATES: dict[str, tuple[str, ...]] = {
    "runcmake_macos": ("runcmake-macos.json", "runcmake_macos.json"),
    "runcmake_linux": ("runcmake-linux.json", "runcmake_linux.json"),
    "runcmake_windows": ("runcmake-windows.json", "runcmake_windows.json"),
    "independent_scenarios": (
        "independent.json",
        "independent-scenarios.json",
        "independent_scenarios.json",
    ),
    "large_projects": (
        "large-projects.json",
        "large_projects.json",
        "large-projects/result.json",
        "large-projects",
    ),
    "vcpkg": ("vcpkg.json", "vcpkg-matrix.json", "vcpkg_matrix.json", "vcpkg"),
    "conan": ("conan.json", "ecosystem.json", "conan-ecosystem.json", "conan_ecosystem.json"),
    "unseen_holdout": ("unseen-holdout.json", "unseen_holdout.json", "unseen.json", "holdout.json"),
    "benchmarks_giants": (
        "bench-giants.json",
        "bench_giants.json",
        "benchmarks-giants.json",
        "benchmarks_giants.json",
    ),
    "benchmarks_synthetic": (
        "bench-synthetic.json",
        "bench_synthetic.json",
        "benchmarks-synthetic.json",
        "benchmarks_synthetic.json",
        "bench-configure.json",
        "bench.json",
        "benchmarks-synthetic",
    ),
    "probe_cache": ("probe-cache.json", "probe_cache.json", "probe-gate.json", "probe.json"),
    "mutation_replay": (
        "mutation-replay.json",
        "mutation_replay.json",
        "mutation-gate.json",
        "mutation.json",
    ),
    "fuzz": ("fuzz.json", "fuzz"),
    "differential_fuzz": (
        "differential-fuzz.json",
        "differential_fuzz.json",
        "oracle-prop-test.json",
        "oracle_prop_test.json",
    ),
    "wasi": ("wasi.json", "wasm-smoke.json", "wasm_smoke.json"),
    "cli_ux": ("cli-ux.json", "cli_ux.json"),
    "path_swap": ("path-swap.json", "path_swap.json", "path-swap-gate.json"),
    "dist": ("dist.json", "dist-tarball.json", "dist_tarball.json"),
}


@dataclass(frozen=True)
class Finding:
    """One concrete verification failure with expected and observed values."""

    track: str
    receipt: str
    kind: str
    expected: str
    actual: str
    detail: str

    def to_dict(self) -> dict[str, str]:
        return {
            "track": self.track,
            "receipt": self.receipt,
            "kind": self.kind,
            "expected": self.expected,
            "actual": self.actual,
            "detail": self.detail,
        }


@dataclass(frozen=True)
class ReceiptRecord:
    """Normalized receipt loaded from a manifest entry or directory file."""

    key: str
    track: str
    source: str
    base_dir: Path
    payload: dict[str, Any]
    file_commit: str | None
    file_binary_sha256: str | None
    envelope_commit: str | None = None
    envelope_binary_sha256: str | None = None
    member_identities: tuple[tuple[str, str | None, str | None], ...] = ()


@dataclass(frozen=True)
class VerificationResult:
    """Outcome of verifying one release receipt set."""

    release_commit: str | None
    binary_sha256: str | None
    checked_receipts: tuple[str, ...]
    findings: tuple[Finding, ...]

    @property
    def ok(self) -> bool:
        return not self.findings

    def to_dict(self) -> dict[str, Any]:
        return {
            "ok": self.ok,
            "release_commit": self.release_commit,
            "binary_sha256": self.binary_sha256,
            "checked_receipts": list(self.checked_receipts),
            "findings": [item.to_dict() for item in self.findings],
        }


def is_int(value: Any) -> bool:
    return type(value) is int


def is_number(value: Any) -> bool:
    return type(value) in (int, float) and math.isfinite(value)


def normalize_commit(value: Any) -> str | None:
    if not isinstance(value, str):
        return None
    cleaned = value.strip().lower()
    return cleaned if HEX_COMMIT_RE.fullmatch(cleaned) else None


def normalize_sha256(value: Any) -> str | None:
    if not isinstance(value, str):
        return None
    cleaned = value.strip().lower()
    return cleaned if SHA256_RE.fullmatch(cleaned) else None


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(65536), b""):
            digest.update(chunk)
    return digest.hexdigest()


def read_json_object(path: Path) -> dict[str, Any]:
    data = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(data, dict):
        raise ValueError(f"{path} must contain a JSON object.")
    return data


def extract_identity_fields(mapping: dict[str, Any]) -> tuple[str | None, str | None]:
    """Read raw commit and binary_sha256 strings from top-level, receipt, or meta blocks."""
    commits = set(builder.report_identity_values(mapping, builder.REPORT_SHA_KEYS))
    digests = set(builder.report_identity_values(mapping, builder.REPORT_DIGEST_KEYS))
    if len(commits) > 1 or len(digests) > 1:
        raise ValueError("Receipt contains conflicting release commit or binary SHA-256 fields.")
    return next(iter(commits), None), next(iter(digests), None)


def resolve_path(raw: str | Path, base_dir: Path) -> Path:
    candidate = Path(raw)
    return candidate if candidate.is_absolute() else (base_dir / candidate)


def load_directory_receipt(key: str, directory: Path) -> tuple[dict[str, Any], list[tuple[str, str | None, str | None]]]:
    """Aggregate multi-file receipt directories (large_projects, vcpkg, fuzz)."""
    member_identities: list[tuple[str, str | None, str | None]] = []

    if key == "benchmarks_synthetic":
        rows = []
        for path in builder.report_paths(directory, "*.json"):
            report = read_json_object(path)
            member_identities.append((path.relative_to(directory).as_posix(), *extract_identity_fields(report)))
            if not isinstance(report.get("results"), list):
                raise ValueError(f"{path}: benchmark report must contain a results list.")
            rows.extend(report["results"])
        return {"results": rows}, member_identities

    if key == "fuzz":
        paths = sorted(p for p in directory.rglob("*.json") if not p.name.startswith("._"))
        if not paths:
            raise ValueError(f"No fuzz JSON reports in {directory}.")
        total_iterations = 0
        total_failures = 0
        targets: list[str] = []
        for path in paths:
            report = read_json_object(path)
            member_identities.append((path.relative_to(directory).as_posix(), *extract_identity_fields(report)))
            iterations = report.get("iterations")
            failed = report.get("failures")
            if not is_int(iterations) or iterations < 0:
                raise ValueError(f"{path}: iterations must be a nonnegative integer.")
            if isinstance(failed, list):
                failed_count = len(failed)
            elif is_int(failed) and failed >= 0:
                failed_count = failed
            else:
                raise ValueError(f"{path}: failures must be a nonnegative integer or list.")
            total_iterations += iterations
            total_failures += failed_count
            targets.append(str(report.get("target") or path.stem))
        return {
            "iterations": total_iterations,
            "failures": total_failures,
            "targets": targets,
        }, member_identities

    if key == "vcpkg":
        paths = sorted(p for p in directory.rglob("shard.json") if not p.name.startswith("._"))
        if not paths:
            raise ValueError(f"No shard.json reports in {directory}.")
        shards: list[dict[str, Any]] = []
        for path in paths:
            report = read_json_object(path)
            member_identities.append((path.relative_to(directory).as_posix(), *extract_identity_fields(report)))
            outcomes: Counter[str] = Counter()
            results_file = path.with_name("results.jsonl")
            if results_file.is_file():
                for line in results_file.read_text(encoding="utf-8").splitlines():
                    if line.strip():
                        outcomes[json.loads(line)["status"]] += 1
            total = report.get("expected_ports")
            passed = outcomes["pass"]
            failed = outcomes["configure-mismatch"] + outcomes["cmake-identity-mismatch"]
            recorded = sum(outcomes.values())
            if not is_int(total) or total < 0:
                raise ValueError(f"{path}: expected_ports must be a nonnegative integer.")
            status = "partial" if (recorded == 0 or recorded < total) else ("pass" if passed == total else "fail")
            shards.append({
                "shard": report.get("shard_index"),
                "shard_count": report.get("shard_count"),
                "status": status,
                "ports_total": total,
                "ports_passed": passed,
                "ports_failed": failed,
            })
        return {
            "shards_total": VCPKG_REQUIRED_SHARDS,
            "shards_completed": sum(s["status"] in {"pass", "fail"} for s in shards),
            "shards_passed": sum(s["status"] == "pass" for s in shards),
            "ports_total": sum(s["ports_total"] for s in shards),
            "ports_passed": sum(s["ports_passed"] for s in shards),
            "ports_failed": sum(s["ports_failed"] for s in shards),
            "shards": shards,
        }, member_identities

    if key == "large_projects":
        paths = sorted(p for p in directory.rglob("result.json") if not p.name.startswith("._"))
        if not paths:
            raise ValueError(f"No result.json reports in {directory}.")
        summaries: dict[str, dict[str, Any]] = {}
        individuals: dict[str, dict[str, Any]] = {}
        for path in paths:
            report = read_json_object(path)
            member_identities.append((path.relative_to(directory).as_posix(), *extract_identity_fields(report)))
            if isinstance(report.get("projects"), dict):
                for name, item in report["projects"].items():
                    summaries[name] = {**item, "project": item.get("project", name)}
            elif isinstance(report.get("projects"), list):
                for item in report["projects"]:
                    if isinstance(item, dict) and isinstance(item.get("project"), str):
                        summaries[item["project"]] = item
            elif isinstance(report.get("project"), str):
                individuals[report["project"]] = report
        merged = {**summaries, **individuals}
        return {"projects": list(merged.values())}, member_identities

    raise ValueError(f"{key}: directory receipts are not supported for this track.")


def load_receipt_entry(
    key: str,
    track: str,
    entry: Any,
    base_dir: Path,
) -> ReceiptRecord:
    """Load one receipt from a path string, wrapper dict with path, or inline dict."""
    if isinstance(entry, (str, Path)):
        path = resolve_path(entry, base_dir)
        return _load_receipt_from_path(key, track, path, None, None)

    if isinstance(entry, dict):
        env_commit, env_binary = extract_identity_fields(entry)
        if "path" in entry:
            raw_path = entry["path"]
            if not isinstance(raw_path, str) or not raw_path.strip():
                raise ValueError(f"{key}: receipt path must be a non-empty string.")
            path = resolve_path(raw_path, base_dir)
            return _load_receipt_from_path(key, track, path, env_commit, env_binary)
        return ReceiptRecord(
            key=key,
            track=track,
            source=f"inline:{key}",
            base_dir=base_dir,
            payload=entry,
            file_commit=env_commit,
            file_binary_sha256=env_binary,
        )

    raise ValueError(f"{key}: receipt entry must be a file path or JSON object.")


def _load_receipt_from_path(
    key: str,
    track: str,
    path: Path,
    env_commit: str | None,
    env_binary: str | None,
) -> ReceiptRecord:
    if not path.exists():
        raise FileNotFoundError(str(path))
    if path.is_dir():
        payload, members = load_directory_receipt(key, path)
        commits = {c for _, c, _ in members if c is not None}
        binaries = {b for _, _, b in members if b is not None}
        file_commit = next(iter(commits)) if len(commits) == 1 else None
        file_binary = next(iter(binaries)) if len(binaries) == 1 else None
        return ReceiptRecord(
            key=key,
            track=track,
            source=str(path),
            base_dir=path,
            payload=payload,
            file_commit=file_commit,
            file_binary_sha256=file_binary,
            envelope_commit=env_commit,
            envelope_binary_sha256=env_binary,
            member_identities=tuple(members),
        )
    payload = read_json_object(path)
    file_commit, file_binary = extract_identity_fields(payload)
    return ReceiptRecord(
        key=key,
        track=track,
        source=str(path),
        base_dir=path.parent,
        payload=payload,
        file_commit=file_commit,
        file_binary_sha256=file_binary,
        envelope_commit=env_commit,
        envelope_binary_sha256=env_binary,
    )


def check_identity(
    record: ReceiptRecord,
    expected_commit: str | None,
    expected_binary: str | None,
    findings: list[Finding],
) -> None:
    """Enforce one exact release_commit and binary_sha256 on every receipt and member."""
    if record.member_identities:
        for member_name, member_commit, member_binary in record.member_identities:
            _check_single_identity(
                f"{record.key}:{member_name}",
                record.track,
                member_commit,
                member_binary,
                record.envelope_commit,
                record.envelope_binary_sha256,
                expected_commit,
                expected_binary,
                findings,
            )
        return

    _check_single_identity(
        record.key,
        record.track,
        record.file_commit,
        record.file_binary_sha256,
        record.envelope_commit,
        record.envelope_binary_sha256,
        expected_commit,
        expected_binary,
        findings,
    )


def _check_single_identity(
    label: str,
    track: str,
    file_commit: str | None,
    file_binary: str | None,
    env_commit: str | None,
    env_binary: str | None,
    expected_commit: str | None,
    expected_binary: str | None,
    findings: list[Finding],
) -> None:
    if env_commit is not None and file_commit is not None and env_commit != file_commit:
        findings.append(Finding(
            track=track,
            receipt=label,
            kind="mismatch",
            expected=f"envelope release_commit == file release_commit ({env_commit})",
            actual=file_commit,
            detail="Manifest envelope commit disagrees with the receipt file commit.",
        ))
    if env_binary is not None and file_binary is not None and env_binary != file_binary:
        findings.append(Finding(
            track=track,
            receipt=label,
            kind="mismatch",
            expected=f"envelope binary_sha256 == file binary_sha256 ({env_binary})",
            actual=file_binary,
            detail="Manifest envelope binary_sha256 disagrees with the receipt file binary_sha256.",
        ))

    effective_commit = file_commit if file_commit is not None else env_commit
    effective_binary = file_binary if file_binary is not None else env_binary

    if effective_commit is None:
        findings.append(Finding(
            track=track,
            receipt=label,
            kind="missing",
            expected=expected_commit or "7-40 hex release_commit",
            actual="missing",
            detail="Receipt is missing release_commit metadata.",
        ))
    elif not normalize_commit(effective_commit):
        findings.append(Finding(
            track=track,
            receipt=label,
            kind="invalid",
            expected=expected_commit or "7-40 hex release_commit",
            actual=effective_commit,
            detail="Receipt release_commit is not a valid Git hex SHA.",
        ))
    elif expected_commit is not None and effective_commit != expected_commit:
        findings.append(Finding(
            track=track,
            receipt=label,
            kind="stale",
            expected=expected_commit,
            actual=effective_commit,
            detail="Receipt release_commit does not match the frozen release commit.",
        ))

    if effective_binary is None:
        findings.append(Finding(
            track=track,
            receipt=label,
            kind="missing",
            expected=expected_binary or "64-hex binary_sha256",
            actual="missing",
            detail="Receipt is missing binary_sha256 metadata.",
        ))
    elif not normalize_sha256(effective_binary):
        findings.append(Finding(
            track=track,
            receipt=label,
            kind="invalid",
            expected=expected_binary or "64-hex binary_sha256",
            actual=effective_binary,
            detail="Receipt binary_sha256 is not a valid 64-character SHA-256 hex digest.",
        ))
    elif expected_binary is not None and effective_binary != expected_binary:
        findings.append(Finding(
            track=track,
            receipt=label,
            kind="stale",
            expected=expected_binary,
            actual=effective_binary,
            detail="Receipt binary_sha256 does not match the frozen release binary digest.",
        ))


def check_runcmake(record: ReceiptRecord, findings: list[Finding]) -> None:
    min_suites = RUNCMAKE_MIN_SUITES[record.key]
    payload = record.payload
    if isinstance(payload.get("suites"), dict):
        suites = payload["suites"]
        suites_total = len(suites)
        cases_passed = 0
        cases_total = 0
        driver_failures: list[str] = []
        for suite_name, row in suites.items():
            if not isinstance(row, dict):
                driver_failures.append(f"{suite_name}: invalid row")
                continue
            passed = row.get("passed")
            total = row.get("total")
            if not is_int(passed) or not is_int(total) or passed < 0 or total < passed:
                driver_failures.append(f"{suite_name}: invalid passed/total")
                continue
            cases_passed += passed
            cases_total += total
            for lane in ("oracle_driver", "rcmake_driver"):
                driver = row.get(lane)
                if driver is not None and (
                    not isinstance(driver, dict)
                    or driver.get("complete") is not True
                    or bool(driver.get("driver_errors"))
                ):
                    driver_failures.append(f"{suite_name}:{lane}")
            if row.get("reference_failed") or row.get("no_reference"):
                driver_failures.append(f"{suite_name}:reference")
        if driver_failures:
            findings.append(Finding(
                track=record.track,
                receipt=record.key,
                kind="failed",
                expected="0 incomplete drivers or reference failures",
                actual=f"{len(driver_failures)} driver/reference issues ({', '.join(driver_failures[:5])})",
                detail="RunCMake receipt contains incomplete drivers or oracle reference failures.",
            ))
        pass_rate = (100.0 * cases_passed / cases_total) if cases_total > 0 else 0.0
    else:
        suites_total = payload.get("suites_total", payload.get("suitesTotal"))
        suites_passed = payload.get("suites_passed", payload.get("suitesPassed"))
        cases_passed = payload.get("cases_passed", payload.get("casesPassed"))
        cases_total = payload.get("cases_total", payload.get("casesTotal"))
        if not is_int(suites_total) or suites_total < 0:
            findings.append(Finding(
                track=record.track,
                receipt=record.key,
                kind="invalid",
                expected=f"suites_total >= {min_suites}",
                actual=str(suites_total),
                detail="RunCMake receipt must provide suites or integer suites_total.",
            ))
            return
        if is_int(cases_passed) and is_int(cases_total) and cases_total > 0 and 0 <= cases_passed <= cases_total:
            pass_rate = 100.0 * cases_passed / cases_total
        elif is_int(suites_passed) and suites_total > 0 and 0 <= suites_passed <= suites_total:
            pass_rate = 100.0 * suites_passed / suites_total
        else:
            findings.append(Finding(
                track=record.track,
                receipt=record.key,
                kind="invalid",
                expected="valid case or suite pass counts",
                actual=f"suites_passed={suites_passed}, cases={cases_passed}/{cases_total}",
                detail="RunCMake receipt lacks valid pass counts.",
            ))
            return

    if suites_total < min_suites:
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="failed",
            expected=f">= {min_suites} suites",
            actual=f"{suites_total} suites",
            detail="RunCMake receipt covers fewer suites than the full OS profile.",
        ))
    if pass_rate < RUNCMAKE_MIN_PASS_RATE:
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="failed",
            expected=f">= {RUNCMAKE_MIN_PASS_RATE:.1f}%",
            actual=f"{pass_rate:.2f}%",
            detail="RunCMake pass rate is below the 99.0% release threshold.",
        ))


def check_independent(record: ReceiptRecord, findings: list[Finding]) -> None:
    payload = record.payload
    mode = payload.get("mode")
    if mode is not None and mode != "differential":
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="failed",
            expected="mode == 'differential'",
            actual=str(mode),
            detail="Independent black-box suite must run in differential mode against CMake.",
        ))
    if isinstance(payload.get("scenarios"), list):
        scenarios = payload["scenarios"]
        total = len(scenarios)
        passed = sum(1 for row in scenarios if isinstance(row, dict) and row.get("status") == "pass")
        failed_checks = 0
        for row in scenarios:
            if not isinstance(row, dict):
                continue
            runs = row.get("runs", {})
            checks = runs.get("rcmake", {}).get("checks", []) if isinstance(runs, dict) else []
            failed_checks += sum(1 for c in checks if not (isinstance(c, dict) and c.get("ok") is True))
    else:
        total = payload.get("scenarios_total", payload.get("scenariosTotal"))
        passed = payload.get("scenarios_passed", payload.get("scenariosPassed"))
        checks_passed = payload.get("checks_passed", payload.get("passed"))
        checks_total = payload.get("checks_total", payload.get("total"))
        if not is_int(total) or not is_int(passed):
            findings.append(Finding(
                track=record.track,
                receipt=record.key,
                kind="invalid",
                expected=f"{INDEPENDENT_MIN_SCENARIOS}/{INDEPENDENT_MIN_SCENARIOS} scenarios",
                actual=f"{passed}/{total}",
                detail="Independent verification receipt lacks scenario counts.",
            ))
            return
        if is_int(checks_passed) and is_int(checks_total):
            failed_checks = max(0, checks_total - checks_passed)
        else:
            failed_checks = payload.get("failures", 0)
            if not is_int(failed_checks):
                failed_checks = 1

    if total < INDEPENDENT_MIN_SCENARIOS or passed != total:
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="failed",
            expected=f"{INDEPENDENT_MIN_SCENARIOS}/{INDEPENDENT_MIN_SCENARIOS} scenarios passed",
            actual=f"{passed}/{total} scenarios passed",
            detail="All 20 independent black-box scenarios must pass with zero skips or failures.",
        ))
    if failed_checks != 0:
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="failed",
            expected="0 failed checks",
            actual=f"{failed_checks} failed checks",
            detail="Independent black-box suite reported failing checks.",
        ))


def check_large_projects(record: ReceiptRecord, findings: list[Finding]) -> None:
    payload = record.payload
    if isinstance(payload.get("projects"), list):
        projects = payload["projects"]
        total = len(projects)
        l1_passed = 0
        l2_passed = 0
        extra_failures: list[str] = []
        for row in projects:
            if not isinstance(row, dict):
                continue
            name = str(row.get("project") or row.get("name") or "unknown")
            errored = row.get("status") in {"fail", "oracle_error", "oracle-red", "error"} or bool(row.get("error"))
            try:
                l1 = builder.extract_large_project_level(row, "l1")
                l2 = builder.extract_large_project_level(row, "l2")
                l3 = builder.extract_large_project_level(row, "l3", lane_keys=("build",))
                l4 = builder.extract_large_project_level(row, "l4", lane_keys=("test", "manifest_l4", "install"))
            except ValueError:
                extra_failures.append(f"{name}:invalid levels")
                continue
            l1_passed += not errored and l1 == "pass"
            l2_passed += not errored and l2 == "pass"
            extra_failures.extend(f"{name}:{level}" for level, status in (("l3", l3), ("l4", l4))
                                  if status == "fail")
        if extra_failures:
            findings.append(Finding(
                track=record.track,
                receipt=record.key,
                kind="failed",
                expected="0 L3/L4 failures",
                actual=", ".join(extra_failures),
                detail="Large-project build or test verification failed.",
            ))
    else:
        total = payload.get("projects_total", payload.get("projectsTotal"))
        l1_passed = payload.get("l1_passed", payload.get("l1Passed"))
        l2_passed = payload.get("l2_passed", payload.get("l2Passed"))
        if not (is_int(total) and is_int(l1_passed) and is_int(l2_passed)):
            findings.append(Finding(
                track=record.track,
                receipt=record.key,
                kind="invalid",
                expected=f"{LARGE_PROJECTS_MIN_COUNT}/{LARGE_PROJECTS_MIN_COUNT} L1 and L2",
                actual=f"total={total}, l1={l1_passed}, l2={l2_passed}",
                detail="Large-projects receipt lacks valid project counts.",
            ))
            return

    if total < LARGE_PROJECTS_MIN_COUNT or l1_passed != total or l2_passed != total:
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="failed",
            expected=f">= {LARGE_PROJECTS_MIN_COUNT} projects with 100% L1 and L2 pass",
            actual=f"L1={l1_passed}/{total}, L2={l2_passed}/{total}",
            detail="All pinned large C++ projects must pass both L1 configure and L2 build-graph parity.",
        ))


def check_vcpkg(record: ReceiptRecord, findings: list[Finding]) -> None:
    payload = record.payload
    shards_total = payload.get("shards_total", payload.get("shardsTotal"))
    shards_completed = payload.get("shards_completed", payload.get("shardsCompleted"))
    shards_passed = payload.get("shards_passed", payload.get("shardsPassed"))
    ports_total = payload.get("ports_total", payload.get("portsTotal"))
    ports_passed = payload.get("ports_passed", payload.get("portsPassed"))
    ports_failed = payload.get("ports_failed", payload.get("portsFailed", 0))

    if isinstance(payload.get("shards"), list):
        shard_rows = payload["shards"]
        indices = [row.get("shard") for row in shard_rows if isinstance(row, dict)]
        bad_counts = [
            row.get("shard_count")
            for row in shard_rows
            if isinstance(row, dict) and "shard_count" in row and row.get("shard_count") != VCPKG_REQUIRED_SHARDS
        ]
        expected_set = set(range(VCPKG_REQUIRED_SHARDS))
        if len(shard_rows) != VCPKG_REQUIRED_SHARDS or set(indices) != expected_set or bad_counts:
            findings.append(Finding(
                track=record.track,
                receipt=record.key,
                kind="failed",
                expected="64 distinct shards (0..63) with shard_count == 64",
                actual=f"{len(shard_rows)} shard rows ({len(set(indices) & expected_set)} valid unique)",
                detail="vcpkg matrix must cover all 64 distinct shards.",
            ))
        if shards_total is None:
            shards_total = len(shard_rows)
        if shards_completed is None:
            shards_completed = sum(1 for row in shard_rows if isinstance(row, dict) and row.get("status") in {"pass", "fail"})
        if shards_passed is None:
            shards_passed = sum(1 for row in shard_rows if isinstance(row, dict) and row.get("status") == "pass")

    if (
        shards_total != VCPKG_REQUIRED_SHARDS
        or shards_completed != VCPKG_REQUIRED_SHARDS
        or shards_passed != VCPKG_REQUIRED_SHARDS
    ):
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="failed",
            expected="64/64 vcpkg shards completed and passed",
            actual=f"completed={shards_completed}/{shards_total}, passed={shards_passed}/{shards_total}",
            detail="vcpkg ecosystem matrix requires full 64-shard coverage.",
        ))

    if (
        not is_int(ports_total)
        or ports_total <= 0
        or not is_int(ports_passed)
        or not is_int(ports_failed)
        or ports_failed != 0
        or ports_passed != ports_total
    ):
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="failed",
            expected="ports_total > 0, ports_passed == ports_total, ports_failed == 0",
            actual=f"passed={ports_passed}, total={ports_total}, failed={ports_failed}",
            detail="vcpkg matrix reported missing or failing port comparisons.",
        ))


def check_conan(record: ReceiptRecord, findings: list[Finding]) -> None:
    payload = record.payload
    status = payload.get("status")
    if status != "pass":
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="failed",
            expected="status == 'pass'",
            actual=str(status),
            detail="Conan and ecosystem receipt did not pass.",
        ))
    scenarios = payload.get("scenarios")
    if isinstance(scenarios, list):
        by_name = {
            row["name"]: row.get("status")
            for row in scenarios
            if isinstance(row, dict) and isinstance(row.get("name"), str)
        }
        for required in REQUIRED_CONAN_SCENARIOS:
            if by_name.get(required) != "pass":
                findings.append(Finding(
                    track=record.track,
                    receipt=record.key,
                    kind="failed",
                    expected=f"{required} == 'pass'",
                    actual=str(by_name.get(required, "missing")),
                    detail="Required Conan scenario was skipped, missing, or failed.",
                ))
        bad = [name for name, state in by_name.items() if state in {"fail", "error", "oracle_error", "harness_error"}]
        if bad:
            findings.append(Finding(
                track=record.track,
                receipt=record.key,
                kind="failed",
                expected="0 failed ecosystem scenarios",
                actual=", ".join(sorted(bad)),
                detail="Ecosystem fixture run reported failures.",
            ))
    else:
        passed = payload.get("passed")
        total = payload.get("total")
        if not (is_int(passed) and is_int(total) and total >= len(REQUIRED_CONAN_SCENARIOS) and passed == total):
            findings.append(Finding(
                track=record.track,
                receipt=record.key,
                kind="invalid",
                expected=f">= {len(REQUIRED_CONAN_SCENARIOS)} passing Conan scenarios",
                actual=f"passed={passed}, total={total}",
                detail="Conan receipt must supply scenario list or passing scenario counts.",
            ))


def find_holdout_leaks(value: Any, path: str = "$") -> list[str]:
    """Return JSON paths that expose sealed holdout repository identities or URLs."""
    leaks: list[str] = []
    if isinstance(value, dict):
        for key, item in value.items():
            child_path = f"{path}.{key}"
            if HOLDOUT_LEAK_VALUE_RE.search(key) or (
                path.endswith((".failure_categories", ".failureCategories"))
                and key not in builder.HOLDOUT_FAILURE_CATEGORIES
            ):
                leaks.append(f"{path}.<redacted>")
                continue
            if str(key).lower() in HOLDOUT_FORBIDDEN_KEYS:
                leaks.append(child_path)
                continue
            # Allow top-level manifest artifact links on wrapper fields only, not inside payload.
            if path == "$" and key in {"artifact_url", "run_url"}:
                continue
            leaks.extend(find_holdout_leaks(item, child_path))
    elif isinstance(value, list):
        for index, item in enumerate(value):
            leaks.extend(find_holdout_leaks(item, f"{path}[{index}]"))
    elif isinstance(value, str):
        if HOLDOUT_LEAK_VALUE_RE.search(value):
            leaks.append(path)
    return leaks


def check_unseen_holdout(record: ReceiptRecord, findings: list[Finding]) -> None:
    payload = record.payload
    leaks = find_holdout_leaks(payload)
    if leaks:
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="leak",
            expected="aggregate counts only (zero repository identities or URLs)",
            actual=f"forbidden fields/values at {', '.join(leaks[:5])}",
            detail="Sealed unseen holdout receipt leaked repository identifiers or URLs.",
        ))

    if payload.get("sealed") is False:
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="leak",
            expected="sealed == true",
            actual="false",
            detail="Unseen holdout receipt must be marked sealed.",
        ))

    aggregate = payload.get("aggregate")
    split = payload.get("holdout", payload)
    if isinstance(aggregate, dict):
        total = aggregate.get("reposTotal", aggregate.get("repos_total"))
        passed = aggregate.get("reposPassed", aggregate.get("repos_passed"))
    elif isinstance(split, dict) and "pass_rates" in split:
        try:
            counts, _ = builder.parse_holdout_pass_rates(split["pass_rates"])
            total, passed = counts["reposTotal"], counts["reposPassed"]
        except (KeyError, ValueError):
            total = passed = None
    else:
        total = payload.get("reposTotal", payload.get("repos_total"))
        passed = payload.get("reposPassed", payload.get("repos_passed"))

    if not (is_int(total) and is_int(passed) and total > 0 and 0 <= passed <= total):
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="invalid",
            expected=f"aggregate repos_total >= {UNSEEN_MIN_REPOS}",
            actual=f"passed={passed}, total={total}",
            detail="Unseen holdout receipt lacks valid aggregate repository counts.",
        ))
        return

    pass_rate = 100.0 * passed / total
    if total < UNSEEN_MIN_REPOS:
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="failed",
            expected=f">= {UNSEEN_MIN_REPOS} repositories",
            actual=f"{total} repositories",
            detail="Unseen holdout receipt does not cover the full 240-repository holdout.",
        ))
    if payload.get("status") in {"fail", "pending"} or pass_rate < UNSEEN_MIN_PASS_RATE:
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="failed",
            expected=f"status pass and >= {UNSEEN_MIN_PASS_RATE:.1f}%",
            actual=f"status={payload.get('status')}, pass_rate={pass_rate:.2f}% ({passed}/{total})",
            detail="Sealed unseen holdout aggregate pass rate is below threshold.",
        ))


def _check_bench_row_health(row: dict[str, Any]) -> bool:
    if row.get("status") in {"fail", "pending", "unavailable"}:
        return False
    gate = row.get("gate")
    if isinstance(gate, dict) and (gate.get("passed") is False or gate.get("status") in {"fail", "unavailable"}):
        return False
    for tool in ("rcmake", "oracle"):
        side = row.get(tool)
        if isinstance(side, dict):
            if side.get("ok") is False:
                return False
            failures = side.get("failures")
            if is_int(failures) and failures > 0:
                return False
    return True


def _giants_project_passes(row: dict[str, Any]) -> bool:
    """Check the three phases emitted by bench_giants.py."""
    if row.get("status") != "pass":
        return False
    for key in ("cold", "warm_probe", "incremental_leaf"):
        phase = row.get(key)
        if not isinstance(phase, dict):
            return False
        for tool in ("cmake", "rcmake"):
            sample = phase.get(tool)
            if not isinstance(sample, dict) or sample.get("failed") != 0:
                return False
            succeeded = sample.get("succeeded")
            median = sample.get("median_ms")
            if not is_int(succeeded) or succeeded < 1 or not is_number(median) or median <= 0:
                return False
        speedup = phase.get("speedup")
        if not is_number(speedup) or speedup <= 0:
            return False
    p95 = row["incremental_leaf"]["rcmake"].get("p95_ms")
    if not is_number(p95) or p95 <= 0:
        return False
    build = row.get("build_verify")
    return build is None or (isinstance(build, dict) and build.get("status") == "pass")


def check_benchmarks_giants(record: ReceiptRecord, findings: list[Finding]) -> None:
    payload = record.payload
    legacy_rows = payload.get("results")
    versioned_projects = not isinstance(legacy_rows, list) and payload.get("schema_version") == 1
    rows = legacy_rows if isinstance(legacy_rows, list) else payload.get("projects")
    if not isinstance(rows, list) or not rows:
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="invalid",
            expected="non-empty real-world giant benchmark results",
            actual=str(type(rows).__name__),
            detail="Real-world giant benchmark receipt must contain a non-empty results or projects list.",
        ))
        return

    giant_rows = [
        row for row in rows
        if isinstance(row, dict) and str(row.get("project") or row.get("name") or "").lower() != "synthetic"
    ]
    if not giant_rows:
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="failed",
            expected=">= 1 non-synthetic real-world giant benchmark",
            actual="0 real-world giant rows",
            detail="Real-world giant benchmark receipt contains only synthetic rows.",
        ))
        return

    failing = []
    for row in giant_rows:
        label = str(row.get("project") or row.get("name") or "unknown")
        if versioned_projects:
            if not _giants_project_passes(row):
                failing.append(label)
            continue
        has_metric = any(
            is_number(val) and val > 0
            for val in (
                row.get("speedup"),
                row.get("warm_probe_speedup"),
                row.get("cold_configure_speedup"),
                row.get("incremental_leaf_p95_ms"),
                row.get("rcmake", {}).get("p95_ms") if isinstance(row.get("rcmake"), dict) else None,
                row.get("rcmake", {}).get("median_ms") if isinstance(row.get("rcmake"), dict) else None,
            )
        )
        if not _check_bench_row_health(row) or not has_metric:
            failing.append(label)

    if failing:
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="failed",
            expected="all real-world giant benchmarks passing with positive metrics",
            actual=f"failed: {', '.join(failing)}",
            detail="One or more real-world giant benchmarks failed or missed numeric measurements.",
        ))


def check_benchmarks_synthetic(record: ReceiptRecord, findings: list[Finding]) -> None:
    payload = record.payload
    rows = payload.get("results")
    if isinstance(rows, list):
        cold_ok = False
        warm_ok = False
        leaf_ok = False
        failing: list[str] = []
        for row in rows:
            if not isinstance(row, dict):
                continue
            label = str(row.get("name") or row.get("case") or "unknown")
            if not _check_bench_row_health(row):
                failing.append(label)
            if row.get("name") == "configure 500 targets (cold)" or row.get("case") == "cold-500":
                rcmake = row.get("rcmake") if isinstance(row.get("rcmake"), dict) else {}
                rss = rcmake.get("peak_rss_mib", row.get("peak_rss_mib"))
                if is_number(row.get("speedup")) and row["speedup"] > 0 and is_number(rss) and rss > 0:
                    cold_ok = True
            if row.get("case") == "warm probe-cache configure":
                if is_number(row.get("speedup")) and row["speedup"] > 0:
                    warm_ok = True
            if row.get("case") == "incremental-leaf":
                rcmake = row.get("rcmake") if isinstance(row.get("rcmake"), dict) else {}
                p95 = rcmake.get("p95_ms", row.get("p95_ms"))
                if is_number(p95) and p95 > 0:
                    leaf_ok = True
        if failing:
            findings.append(Finding(
                track=record.track,
                receipt=record.key,
                kind="failed",
                expected="0 failing synthetic benchmark rows",
                actual=", ".join(failing),
                detail="Synthetic benchmark suite reported failures or missed gates.",
            ))
        missing_cases = [
            name for name, present in (
                ("cold 500-target configure + RSS", cold_ok),
                ("warm probe-cache configure", warm_ok),
                ("incremental-leaf p95", leaf_ok),
            ) if not present
        ]
        if missing_cases:
            findings.append(Finding(
                track=record.track,
                receipt=record.key,
                kind="missing",
                expected="cold 500-target, warm probe-cache, and incremental-leaf measurements",
                actual=f"missing {', '.join(missing_cases)}",
                detail="Synthetic benchmark receipt is missing required benchmark cases.",
            ))
        return

    required_fields = (
        "cold_configure_speedup",
        "warm_probe_speedup",
        "incremental_leaf_p95_ms",
        "peak_rss_mib",
    )
    missing = [field for field in required_fields if not (is_number(payload.get(field)) and payload[field] > 0)]
    if missing or payload.get("status") in {"fail", "pending"}:
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="failed",
            expected="positive cold, warm, leaf p95, and peak RSS metrics",
            actual=f"status={payload.get('status')}, missing={missing}",
            detail="Synthetic benchmark summary is incomplete or failing.",
        ))


def check_probe_cache(record: ReceiptRecord, findings: list[Finding]) -> None:
    payload = record.payload
    mode = payload.get("mode")
    if mode != "gate":
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="failed",
            expected="mode == 'gate'",
            actual=str(mode),
            detail="Probe-cache receipt must come from full gate mode, not smoke mode.",
        ))
    cases = payload.get("cases")
    if isinstance(cases, list):
        total = len(cases)
        passed = sum(1 for row in cases if isinstance(row, dict) and row.get("status") == "passed" and row.get("passed") is True)
        skipped = sum(1 for row in cases if isinstance(row, dict) and row.get("status") == "skipped")
    else:
        total = payload.get("total")
        passed = payload.get("passed_count") if is_int(payload.get("passed_count")) else (
            payload.get("passed") if is_int(payload.get("passed")) else None
        )
        skipped = payload.get("skipped", 0)
    if (
        payload.get("passed") is False
        or not is_int(total)
        or not is_int(passed)
        or total < PROBE_CACHE_MIN_CHECKS
        or passed != total
        or skipped != 0
    ):
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="failed",
            expected=f"{PROBE_CACHE_MIN_CHECKS}/{PROBE_CACHE_MIN_CHECKS} probe-cache checks passed (0 skipped)",
            actual=f"passed={passed}/{total}, skipped={skipped}, overall={payload.get('passed')}",
            detail="Probe-cache soundness gate requires 16/16 passing checks.",
        ))


def check_mutation_replay(record: ReceiptRecord, findings: list[Finding]) -> None:
    payload = record.payload
    if payload.get("mode") != "gate":
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="failed",
            expected="mode == 'gate'",
            actual=str(payload.get("mode")),
            detail="Mutation replay receipt must come from full gate mode, not smoke mode.",
        ))
    if payload.get("replay_verified") is not True:
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="failed",
            expected="replay_verified == true",
            actual=str(payload.get("replay_verified")),
            detail="Mutation replay receipt did not verify incremental replay.",
        ))
    steps = payload.get("steps")
    if isinstance(steps, list):
        edit_steps = [row for row in steps if isinstance(row, dict) and is_int(row.get("step")) and row["step"] > 0]
        total = len(edit_steps)
        passed = sum(1 for row in edit_steps if row.get("passed") is True)
    else:
        total = payload.get("total", payload.get("edits"))
        passed = payload.get("passed_steps", payload.get("effective_edits"))
    if (
        payload.get("passed") is False
        or not is_int(total)
        or not is_int(passed)
        or total < MUTATION_MIN_STEPS
        or passed != total
    ):
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="failed",
            expected=f"{MUTATION_MIN_STEPS}/{MUTATION_MIN_STEPS} mutation edits passed",
            actual=f"passed={passed}/{total}, overall={payload.get('passed')}",
            detail="Mutation replay soundness gate requires 20/20 passing edits.",
        ))


def check_fuzz(record: ReceiptRecord, findings: list[Finding]) -> None:
    payload = record.payload
    iterations = payload.get("iterations", payload.get("total"))
    raw_failures = payload.get("failures")
    failures = len(raw_failures) if isinstance(raw_failures, list) else raw_failures
    if not (is_int(iterations) and is_int(failures) and iterations >= FUZZ_MIN_ITERATIONS and failures == 0):
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="failed",
            expected=f">= {FUZZ_MIN_ITERATIONS:,} iterations with 0 failures",
            actual=f"iterations={iterations}, failures={failures}",
            detail="Adversarial fuzzing gate requires at least 14,000,000 inputs and 0 failures.",
        ))
    targets = payload.get("targets")
    if isinstance(targets, (list, dict)) and len(targets) < 14:
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="failed",
            expected=">= 14 fuzz targets",
            actual=f"{len(targets)} fuzz targets",
            detail="Adversarial fuzzing receipt does not cover all 14 fuzz targets.",
        ))


def check_differential_fuzz(record: ReceiptRecord, findings: list[Finding]) -> None:
    payload = record.payload
    total = payload.get("total")
    seed = payload.get("seed")
    counts = payload.get("counts")
    if not is_int(seed) or not is_int(total) or total <= 0 or not isinstance(counts, dict):
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="invalid",
            expected="oracle_prop_test.py report with integer seed, total > 0, and counts",
            actual=f"seed={seed}, total={total}, counts={type(counts).__name__}",
            detail="Differential fuzzing receipt is missing oracle_prop_test.py fields.",
        ))
        return
    passed = counts.get("pass")
    failed = counts.get("fail")
    oracle_errors = counts.get("oracle_error")
    if not (is_int(passed) and is_int(failed) and is_int(oracle_errors)):
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="invalid",
            expected="integer counts for pass, fail, oracle_error",
            actual=str(counts),
            detail="Differential fuzzing counts must be integers.",
        ))
        return
    if failed != 0 or oracle_errors != 0 or passed != total:
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="failed",
            expected=f"{total}/{total} pass, 0 fail, 0 oracle_error",
            actual=f"pass={passed}, fail={failed}, oracle_error={oracle_errors} (total={total})",
            detail="Differential fuzzing (oracle_prop_test.py) reported mismatches or oracle errors.",
        ))


def check_wasi(record: ReceiptRecord, findings: list[Finding]) -> None:
    payload = record.payload
    checks = payload.get("checks")
    if isinstance(checks, list):
        total = len(checks)
        passed = sum(1 for row in checks if isinstance(row, dict) and row.get("status") == "pass")
    else:
        total = payload.get("total")
        passed = payload.get("passed")
    if (
        payload.get("status") != "pass"
        or not is_int(total)
        or not is_int(passed)
        or total < WASI_MIN_CHECKS
        or passed != total
    ):
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="failed",
            expected=f"status == 'pass' and {WASI_MIN_CHECKS}/{WASI_MIN_CHECKS} checks passed",
            actual=f"status={payload.get('status')}, passed={passed}/{total}",
            detail="WebAssembly and WASI smoke receipt requires 6/6 passing checks.",
        ))


def check_wasm_bundle(
    wasm_path: Path,
    expected_sha256: str | None,
    findings: list[Finding],
) -> None:
    if not wasm_path.is_file():
        findings.append(Finding(
            track="track5-wasm",
            receipt="wasm_bundle",
            kind="missing",
            expected=f"non-empty WebAssembly module at {wasm_path}",
            actual="missing",
            detail="Browser playground WASM bundle is missing.",
        ))
        return
    raw = wasm_path.read_bytes()
    if len(raw) <= len(WASM_V1_MAGIC) or not raw.startswith(WASM_V1_MAGIC):
        findings.append(Finding(
            track="track5-wasm",
            receipt="wasm_bundle",
            kind="invalid",
            expected="valid non-empty WebAssembly v1 module",
            actual=f"{len(raw)} bytes",
            detail=f"{wasm_path} is empty or lacks the WebAssembly v1 magic header.",
        ))
        return
    if expected_sha256 is not None:
        actual_sha = hashlib.sha256(raw).hexdigest()
        if actual_sha != expected_sha256:
            findings.append(Finding(
                track="track5-wasm",
                receipt="wasm_bundle",
                kind="mismatch",
                expected=expected_sha256,
                actual=actual_sha,
                detail=f"{wasm_path} SHA-256 does not match the recorded wasm_sha256.",
            ))
    js_wrapper = wasm_path.with_suffix(".js")
    if not js_wrapper.is_file() or js_wrapper.stat().st_size == 0:
        findings.append(Finding(
            track="track5-wasm",
            receipt="wasm_bundle",
            kind="missing",
            expected=f"non-empty JS loader at {js_wrapper}",
            actual="missing or empty",
            detail="Browser playground WASM JS loader is missing or empty.",
        ))


def check_cli_ux(record: ReceiptRecord, findings: list[Finding]) -> None:
    payload = record.payload
    status = payload.get("status")
    checks = payload.get("checks")
    if isinstance(checks, list):
        total = len(checks)
        passed = sum(
            1 for row in checks
            if isinstance(row, dict) and (row.get("status") == "pass" or row.get("ok") is True)
        )
        failures = total - passed
    else:
        total = payload.get("total")
        passed = payload.get("passed")
        failures = payload.get("failures", 0 if (is_int(total) and is_int(passed) and total == passed) else None)
    if (
        status != "pass"
        or not is_int(total)
        or not is_int(passed)
        or not is_int(failures)
        or total <= 0
        or passed != total
        or failures != 0
    ):
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="failed",
            expected="status == 'pass', total > 0, passed == total, failures == 0",
            actual=f"status={status}, passed={passed}/{total}, failures={failures}",
            detail="CLI UX verification receipt failed or is incomplete.",
        ))


def check_path_swap(record: ReceiptRecord, findings: list[Finding]) -> None:
    payload = record.payload
    status = payload.get("status")
    diffs = payload.get("differences")
    if status != "pass" or (diffs is not None and diffs != []):
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="failed",
            expected="status == 'pass' and differences == []",
            actual=f"status={status}, differences={len(diffs) if isinstance(diffs, list) else diffs}",
            detail="Silent PATH swap verification reported differences or errors.",
        ))
    fixture = payload.get("fixture")
    if isinstance(fixture, dict):
        if fixture.get("status") != "pass" or fixture.get("differences") not in (None, []):
            findings.append(Finding(
                track=record.track,
                receipt=record.key,
                kind="failed",
                expected="fixture.status == 'pass' and fixture.differences == []",
                actual=f"status={fixture.get('status')}",
                detail="Nested PATH swap fixture check failed.",
            ))


def check_dist(record: ReceiptRecord, findings: list[Finding]) -> None:
    payload = record.payload
    tarball = payload.get("tarball")
    tarball_sha = normalize_sha256(payload.get("tarball_sha256", payload.get("tarballSha256")))
    if not isinstance(tarball, str) or not tarball.endswith((".tar.gz", ".zip")):
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="invalid",
            expected="tarball filename ending in .tar.gz or .zip",
            actual=str(tarball),
            detail="dist.sh receipt must identify the packaged release archive.",
        ))
    if tarball_sha is None:
        findings.append(Finding(
            track=record.track,
            receipt=record.key,
            kind="invalid",
            expected="64-hex tarball_sha256",
            actual=str(payload.get("tarball_sha256")),
            detail="dist.sh receipt must record a valid 64-character SHA-256 digest.",
        ))
        return

    tarball_path_raw = payload.get("tarball_path")
    if isinstance(tarball_path_raw, str) and tarball_path_raw.strip():
        tarball_path = resolve_path(tarball_path_raw, record.base_dir)
        if not tarball_path.is_file():
            findings.append(Finding(
                track=record.track,
                receipt=record.key,
                kind="missing",
                expected=f"archive file at {tarball_path}",
                actual="missing",
                detail="Referenced dist.sh tarball does not exist on disk.",
            ))
        else:
            actual_sha = sha256_file(tarball_path)
            if actual_sha != tarball_sha:
                findings.append(Finding(
                    track=record.track,
                    receipt=record.key,
                    kind="mismatch",
                    expected=tarball_sha,
                    actual=actual_sha,
                    detail="dist.sh tarball SHA-256 on disk does not match receipt tarball_sha256.",
                ))

    sha256_path_raw = payload.get("sha256_path")
    if isinstance(sha256_path_raw, str) and sha256_path_raw.strip():
        sha_path = resolve_path(sha256_path_raw, record.base_dir)
        if not sha_path.is_file():
            findings.append(Finding(
                track=record.track,
                receipt=record.key,
                kind="missing",
                expected=f"checksum file at {sha_path}",
                actual="missing",
                detail="Referenced dist.sh .sha256 checksum file does not exist.",
            ))
        else:
            line = sha_path.read_text(encoding="ascii", errors="replace").strip()
            parts = line.split()
            if len(parts) != 2 or parts[0].lower() != tarball_sha or (
                isinstance(tarball, str) and parts[1] != Path(tarball).name
            ):
                findings.append(Finding(
                    track=record.track,
                    receipt=record.key,
                    kind="mismatch",
                    expected=f"{tarball_sha}  {Path(str(tarball)).name}",
                    actual=line or "<empty>",
                    detail="dist.sh .sha256 file does not match receipt tarball_sha256 and filename.",
                ))


@dataclass(frozen=True)
class ReceiptSpec:
    key: str
    track: str
    validator: Callable[[ReceiptRecord, list[Finding]], None]


REQUIRED_RECEIPTS: tuple[ReceiptSpec, ...] = (
    ReceiptSpec("runcmake_macos", "track1-conformance", check_runcmake),
    ReceiptSpec("runcmake_linux", "track1-conformance", check_runcmake),
    ReceiptSpec("runcmake_windows", "track1-conformance", check_runcmake),
    ReceiptSpec("independent_scenarios", "track1-conformance", check_independent),
    ReceiptSpec("large_projects", "track2-ecosystem", check_large_projects),
    ReceiptSpec("vcpkg", "track2-ecosystem", check_vcpkg),
    ReceiptSpec("conan", "track2-ecosystem", check_conan),
    ReceiptSpec("unseen_holdout", "track2-ecosystem", check_unseen_holdout),
    ReceiptSpec("benchmarks_giants", "track3-benchmarks", check_benchmarks_giants),
    ReceiptSpec("benchmarks_synthetic", "track3-benchmarks", check_benchmarks_synthetic),
    ReceiptSpec("probe_cache", "track4-soundness", check_probe_cache),
    ReceiptSpec("mutation_replay", "track4-soundness", check_mutation_replay),
    ReceiptSpec("fuzz", "track4-soundness", check_fuzz),
    ReceiptSpec("differential_fuzz", "track4-soundness", check_differential_fuzz),
    ReceiptSpec("wasi", "track5-wasm", check_wasi),
    ReceiptSpec("cli_ux", "track6-ux-packaging", check_cli_ux),
    ReceiptSpec("path_swap", "track6-ux-packaging", check_path_swap),
    ReceiptSpec("dist", "track6-ux-packaging", check_dist),
)


def check_dashboard(
    dashboard_path: Path,
    schema_path: Path,
    expected_commit: str | None,
    expected_binary: str | None,
    findings: list[Finding],
) -> tuple[str | None, str | None]:
    """Validate site/data/latest.json against schema, identity, and non-pending status."""
    if not dashboard_path.is_file():
        findings.append(Finding(
            track="identity",
            receipt="dashboard",
            kind="missing",
            expected=f"dashboard JSON at {dashboard_path}",
            actual="missing",
            detail="Compatibility dashboard snapshot is missing.",
        ))
        return None, None

    try:
        dashboard = read_json_object(dashboard_path)
        schema = read_json_object(schema_path)
        validate_schema(dashboard, schema, schema)
    except (OSError, ValueError, KeyError, TypeError) as error:
        findings.append(Finding(
            track="identity",
            receipt="dashboard",
            kind="invalid",
            expected="valid dashboard JSON matching dashboard.schema.json",
            actual=str(error),
            detail="Dashboard snapshot failed schema validation.",
        ))
        return None, None

    top_commit_raw = dashboard.get("commit")
    dash_commit = top_commit_raw.strip().lower() if isinstance(top_commit_raw, str) and top_commit_raw.strip() else None
    receipt_block = dashboard.get("receipt") if isinstance(dashboard.get("receipt"), dict) else {}
    top_binary_raw = dashboard.get("binarySha256")
    nested_binary_raw = receipt_block.get("binarySha256")
    dash_binary = (
        top_binary_raw.strip().lower()
        if isinstance(top_binary_raw, str) and top_binary_raw.strip()
        else (nested_binary_raw.strip().lower() if isinstance(nested_binary_raw, str) and nested_binary_raw.strip() else None)
    )

    if dash_commit is None:
        findings.append(Finding(
            track="identity",
            receipt="dashboard",
            kind="missing",
            expected=expected_commit or "non-null commit SHA",
            actual="null",
            detail="Dashboard snapshot commit is null or missing.",
        ))
    elif not normalize_commit(dash_commit):
        findings.append(Finding(
            track="identity",
            receipt="dashboard",
            kind="invalid",
            expected=expected_commit or "7-40 hex commit SHA",
            actual=dash_commit,
            detail="Dashboard snapshot commit is not a valid hex SHA.",
        ))
    elif expected_commit is not None and dash_commit != expected_commit:
        findings.append(Finding(
            track="identity",
            receipt="dashboard",
            kind="stale",
            expected=expected_commit,
            actual=dash_commit,
            detail="Dashboard snapshot commit does not match the frozen release commit.",
        ))

    nested_commit_raw = receipt_block.get("commit")
    if isinstance(nested_commit_raw, str) and nested_commit_raw.strip():
        nested_commit = nested_commit_raw.strip().lower()
        if dash_commit is not None and nested_commit != dash_commit:
            findings.append(Finding(
                track="identity",
                receipt="dashboard.receipt",
                kind="mismatch",
                expected=dash_commit,
                actual=nested_commit,
                detail="Dashboard receipt.commit disagrees with top-level dashboard.commit.",
            ))

    if (
        isinstance(top_binary_raw, str) and top_binary_raw.strip()
        and isinstance(nested_binary_raw, str) and nested_binary_raw.strip()
        and top_binary_raw.strip().lower() != nested_binary_raw.strip().lower()
    ):
        findings.append(Finding(
            track="identity",
            receipt="dashboard.receipt",
            kind="mismatch",
            expected=top_binary_raw.strip().lower(),
            actual=nested_binary_raw.strip().lower(),
            detail="Dashboard receipt.binarySha256 disagrees with top-level dashboard.binarySha256.",
        ))

    if not isinstance(dashboard.get("generatedAt"), str) or not dashboard["generatedAt"].strip():
        findings.append(Finding(
            track="identity",
            receipt="dashboard",
            kind="missing",
            expected="non-null ISO-8601 generatedAt timestamp",
            actual=str(dashboard.get("generatedAt")),
            detail="Dashboard snapshot generatedAt timestamp is null or missing.",
        ))

    if dash_binary is None and expected_binary is not None:
        findings.append(Finding(
            track="identity",
            receipt="dashboard",
            kind="missing",
            expected=expected_binary,
            actual="missing",
            detail="Dashboard snapshot is missing the frozen release binary digest.",
        ))
    elif dash_binary is not None:
        if not normalize_sha256(dash_binary):
            findings.append(Finding(
                track="identity",
                receipt="dashboard",
                kind="invalid",
                expected=expected_binary or "64-hex binarySha256",
                actual=dash_binary,
                detail="Dashboard binarySha256 is not a valid 64-character SHA-256 hex digest.",
            ))
        elif expected_binary is not None and dash_binary != expected_binary:
            findings.append(Finding(
                track="identity",
                receipt="dashboard",
                kind="stale",
                expected=expected_binary,
                actual=dash_binary,
                detail="Dashboard binarySha256 does not match the frozen release binary digest.",
            ))

    tracks = dashboard.get("tracks", {})
    holdout = tracks.get("unseenHoldout", {})
    holdout_leaks = find_holdout_leaks(holdout)
    if holdout_leaks:
        findings.append(Finding(
            track="track2-ecosystem",
            receipt="dashboard.unseenHoldout",
            kind="leak",
            expected="aggregate counts only",
            actual=f"forbidden fields/values at {', '.join(holdout_leaks[:5])}",
            detail="Dashboard unseenHoldout section leaked repository identities or URLs.",
        ))

    for track_name in ("coreConformance", "policies", "runCmakeOs", "bossFight", "unseenHoldout", "vcpkgMatrix"):
        status = tracks.get(track_name, {}).get("status")
        if status in {"pending", "fail"}:
            findings.append(Finding(
                track="identity",
                receipt=f"dashboard.{track_name}",
                kind="stale" if status == "pending" else "failed",
                expected="measured passing track",
                actual=str(status),
                detail=f"Dashboard track {track_name} is {status}.",
            ))

    return normalize_commit(dash_commit), normalize_sha256(dash_binary)


def discover_receipts_in_dir(directory: Path) -> dict[str, Path]:
    found: dict[str, Path] = {}
    for key, candidates in DISCOVERY_CANDIDATES.items():
        for name in candidates:
            candidate = directory / name
            if candidate.exists():
                found[key] = candidate
                break
    return found


def verify_manifest(
    manifest_path: Path,
    dashboard_override: Path | None = None,
    wasm_override: Path | None = None,
    schema_path: Path = DEFAULT_SCHEMA,
) -> VerificationResult:
    """Verify a frozen release receipt manifest JSON file."""
    manifest = read_json_object(manifest_path)
    base_dir = manifest_path.parent
    findings: list[Finding] = []

    raw_commit = manifest.get("release_commit", manifest.get("commit"))
    raw_binary = manifest.get("binary_sha256", manifest.get("binarySha256"))
    expected_commit = normalize_commit(raw_commit)
    expected_binary = normalize_sha256(raw_binary)

    if expected_commit is None:
        findings.append(Finding(
            track="identity",
            receipt="manifest",
            kind="invalid" if raw_commit is not None else "missing",
            expected="7-40 hex release_commit",
            actual=str(raw_commit),
            detail="Manifest must specify a valid release_commit SHA.",
        ))
    if expected_binary is None:
        findings.append(Finding(
            track="identity",
            receipt="manifest",
            kind="invalid" if raw_binary is not None else "missing",
            expected="64-hex binary_sha256",
            actual=str(raw_binary),
            detail="Manifest must specify a valid binary_sha256 digest.",
        ))

    receipts_map = manifest.get("receipts")
    if not isinstance(receipts_map, dict):
        findings.append(Finding(
            track="identity",
            receipt="manifest",
            kind="invalid",
            expected="receipts JSON object",
            actual=type(receipts_map).__name__,
            detail="Manifest must contain a receipts mapping.",
        ))
        return VerificationResult(expected_commit, expected_binary, (), tuple(findings))

    dashboard_path = dashboard_override
    if dashboard_path is None and isinstance(manifest.get("dashboard"), str):
        dashboard_path = resolve_path(manifest["dashboard"], base_dir)
    if dashboard_path is not None:
        check_dashboard(dashboard_path, schema_path, expected_commit, expected_binary, findings)

    wasm_path = wasm_override
    if wasm_path is None:
        wasm_raw = manifest.get("wasm_bundle")
        wasm_path = resolve_path(wasm_raw, base_dir) if isinstance(wasm_raw, str) else DEFAULT_WASM_BUNDLE
    expected_wasm_sha = normalize_sha256(manifest.get("wasm_sha256"))
    if manifest.get("wasm_sha256") is not None and expected_wasm_sha is None:
        findings.append(Finding(
            track="track5-wasm",
            receipt="wasm_bundle",
            kind="invalid",
            expected="64-hex wasm_sha256",
            actual=str(manifest.get("wasm_sha256")),
            detail="Manifest wasm_sha256 is not a valid SHA-256 hex digest.",
        ))
    check_wasm_bundle(wasm_path, expected_wasm_sha, findings)

    checked = _verify_receipt_entries(receipts_map, base_dir, expected_commit, expected_binary, findings)
    return VerificationResult(expected_commit, expected_binary, tuple(checked), tuple(findings))


def verify_receipts_dir(
    receipts_dir: Path,
    dashboard_path: Path | None = DEFAULT_DASHBOARD,
    wasm_path: Path = DEFAULT_WASM_BUNDLE,
    expected_commit: str | None = None,
    expected_binary: str | None = None,
    schema_path: Path = DEFAULT_SCHEMA,
) -> VerificationResult:
    """Auto-discover receipts in a directory and verify them alongside the dashboard."""
    findings: list[Finding] = []
    if not receipts_dir.is_dir():
        findings.append(Finding(
            track="identity",
            receipt="receipts_dir",
            kind="missing",
            expected=f"directory at {receipts_dir}",
            actual="missing",
            detail="Receipts directory does not exist.",
        ))
        return VerificationResult(None, None, (), tuple(findings))

    norm_commit = normalize_commit(expected_commit) if expected_commit is not None else None
    norm_binary = normalize_sha256(expected_binary) if expected_binary is not None else None
    if expected_commit is not None and norm_commit is None:
        findings.append(Finding(
            track="identity",
            receipt="cli",
            kind="invalid",
            expected="7-40 hex --expected-commit",
            actual=expected_commit,
            detail="Provided --expected-commit is not a valid hex SHA.",
        ))
    if expected_binary is not None and norm_binary is None:
        findings.append(Finding(
            track="identity",
            receipt="cli",
            kind="invalid",
            expected="64-hex --expected-binary-sha256",
            actual=expected_binary,
            detail="Provided --expected-binary-sha256 is not a valid SHA-256 hex digest.",
        ))

    discovered = discover_receipts_in_dir(receipts_dir)

    # Establish canonical identity from explicit CLI flags, then dashboard, then first receipt.
    if dashboard_path is not None:
        dash_commit, dash_binary = check_dashboard(dashboard_path, schema_path, norm_commit, norm_binary, findings)
        if norm_commit is None:
            norm_commit = dash_commit
        if norm_binary is None:
            norm_binary = dash_binary

    if norm_commit is None or norm_binary is None:
        for spec in REQUIRED_RECEIPTS:
            path = discovered.get(spec.key)
            if path is None:
                continue
            try:
                probe = load_receipt_entry(spec.key, spec.track, path, receipts_dir)
            except (OSError, ValueError, json.JSONDecodeError):
                continue
            if norm_commit is None:
                norm_commit = normalize_commit(probe.file_commit)
            if norm_binary is None:
                norm_binary = normalize_sha256(probe.file_binary_sha256)
            if norm_commit is not None and norm_binary is not None:
                break

    check_wasm_bundle(wasm_path, None, findings)
    checked = _verify_receipt_entries(discovered, receipts_dir, norm_commit, norm_binary, findings)
    return VerificationResult(norm_commit, norm_binary, tuple(checked), tuple(findings))


def _verify_receipt_entries(
    entries: dict[str, Any],
    base_dir: Path,
    expected_commit: str | None,
    expected_binary: str | None,
    findings: list[Finding],
) -> list[str]:
    checked: list[str] = []
    for spec in REQUIRED_RECEIPTS:
        if spec.key not in entries:
            findings.append(Finding(
                track=spec.track,
                receipt=spec.key,
                kind="missing",
                expected=f"receipt entry for {spec.key}",
                actual="missing",
                detail=f"Required Launch Bar receipt {spec.key} is absent.",
            ))
            continue
        try:
            record = load_receipt_entry(spec.key, spec.track, entries[spec.key], base_dir)
        except FileNotFoundError as error:
            findings.append(Finding(
                track=spec.track,
                receipt=spec.key,
                kind="missing",
                expected="existing receipt file",
                actual=str(error),
                detail="Referenced receipt file does not exist.",
            ))
            continue
        except (OSError, ValueError, json.JSONDecodeError) as error:
            findings.append(Finding(
                track=spec.track,
                receipt=spec.key,
                kind="invalid",
                expected="valid JSON receipt",
                actual=str(error),
                detail="Could not parse receipt file.",
            ))
            continue

        checked.append(spec.key)
        check_identity(record, expected_commit, expected_binary, findings)
        spec.validator(record, findings)

    # Also validate identity on any extra optional receipts (such as ci_jobs) in the manifest.
    required_keys = {spec.key for spec in REQUIRED_RECEIPTS}
    for extra_key in sorted(set(entries) - required_keys):
        try:
            record = load_receipt_entry(extra_key, "extra", entries[extra_key], base_dir)
        except (OSError, ValueError, json.JSONDecodeError) as error:
            findings.append(Finding(
                track="extra",
                receipt=extra_key,
                kind="invalid",
                expected="valid JSON receipt",
                actual=str(error),
                detail="Could not parse optional receipt entry.",
            ))
            continue
        checked.append(extra_key)
        check_identity(record, expected_commit, expected_binary, findings)

    return checked


def format_report(result: VerificationResult) -> str:
    """Format a deterministic human-readable summary and diff of findings."""
    if result.ok:
        commit = result.release_commit or "unknown"
        binary = (result.binary_sha256 or "unknown")[:16]
        return (
            f"PASS: verified {len(result.checked_receipts)} release receipts for commit "
            f"{commit} (binary_sha256 {binary}...).\n"
        )

    lines = [
        f"FAIL: {len(result.findings)} release receipt issue(s) found "
        f"(commit={result.release_commit or 'unresolved'}, "
        f"binary_sha256={(result.binary_sha256 or 'unresolved')[:16]}):",
    ]
    by_track: dict[str, list[Finding]] = {}
    for finding in result.findings:
        by_track.setdefault(finding.track, []).append(finding)
    for track, items in by_track.items():
        lines.append(f"  [{track}]")
        for item in items:
            lines.append(
                f"    - {item.receipt} ({item.kind}): expected {item.expected}; "
                f"got {item.actual}. {item.detail}"
            )
    return "\n".join(lines) + "\n"


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--manifest", type=Path, help="Path to a frozen release receipt manifest JSON.")
    source.add_argument("--receipts-dir", type=Path, help="Directory of receipt JSON files to auto-discover.")
    parser.add_argument(
        "--dashboard",
        type=Path,
        default=None,
        help="Path to site/data/latest.json (defaults to manifest dashboard or site/data/latest.json).",
    )
    parser.add_argument(
        "--no-dashboard",
        action="store_true",
        help="Skip validating site/data/latest.json in --receipts-dir mode.",
    )
    parser.add_argument(
        "--wasm-bundle",
        type=Path,
        default=None,
        help="Path to site/wasm/rcmake_wasm.wasm.",
    )
    parser.add_argument(
        "--schema",
        type=Path,
        default=DEFAULT_SCHEMA,
        help="Path to dashboard.schema.json.",
    )
    parser.add_argument("--expected-commit", help="Require this exact release_commit SHA.")
    parser.add_argument("--expected-binary-sha256", help="Require this exact binary_sha256 digest.")
    parser.add_argument("--json-out", type=Path, help="Write structured verification output to this JSON file.")
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        if args.manifest is not None:
            result = verify_manifest(
                args.manifest,
                dashboard_override=args.dashboard,
                wasm_override=args.wasm_bundle,
                schema_path=args.schema,
            )
            if args.expected_commit or args.expected_binary_sha256:
                extra_findings = list(result.findings)
                if args.expected_commit:
                    want_commit = normalize_commit(args.expected_commit)
                    if want_commit is None or want_commit != result.release_commit:
                        extra_findings.append(Finding(
                            track="identity",
                            receipt="manifest",
                            kind="invalid" if want_commit is None else "stale",
                            expected=args.expected_commit.strip().lower(),
                            actual=str(result.release_commit),
                            detail="Manifest release_commit does not match --expected-commit.",
                        ))
                if args.expected_binary_sha256:
                    want_binary = normalize_sha256(args.expected_binary_sha256)
                    if want_binary is None or want_binary != result.binary_sha256:
                        extra_findings.append(Finding(
                            track="identity",
                            receipt="manifest",
                            kind="invalid" if want_binary is None else "stale",
                            expected=args.expected_binary_sha256.strip().lower(),
                            actual=str(result.binary_sha256),
                            detail="Manifest binary_sha256 does not match --expected-binary-sha256.",
                        ))
                result = VerificationResult(
                    result.release_commit,
                    result.binary_sha256,
                    result.checked_receipts,
                    tuple(extra_findings),
                )
        else:
            dashboard_path = None if args.no_dashboard else (args.dashboard or DEFAULT_DASHBOARD)
            result = verify_receipts_dir(
                args.receipts_dir,
                dashboard_path=dashboard_path,
                wasm_path=args.wasm_bundle or DEFAULT_WASM_BUNDLE,
                expected_commit=args.expected_commit,
                expected_binary=args.expected_binary_sha256,
                schema_path=args.schema,
            )
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"verify_release_receipts: {error}", file=sys.stderr)
        return 2

    if args.json_out is not None:
        args.json_out.parent.mkdir(parents=True, exist_ok=True)
        args.json_out.write_text(json.dumps(result.to_dict(), indent=2) + "\n", encoding="utf-8")

    output = format_report(result)
    stream = sys.stdout if result.ok else sys.stderr
    stream.write(output)
    return 0 if result.ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
