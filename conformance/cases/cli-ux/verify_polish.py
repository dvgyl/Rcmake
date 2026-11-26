#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Acceptance verifier for the rcmake CLI UX polish criteria."""

from __future__ import annotations

import argparse
from dataclasses import asdict, dataclass, field
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
from typing import Any

ANSI_RE = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")
EM_DASH = "\u2014"
ELLIPSIS = "\u2026"
GUTTER_ARROW = "\u256d\u2500\u25b6"
GUTTER_BAR = "\u2502"

REQUIRED_CTEST_HELP_OPTIONS = (
    "--test-dir",
    "-R",
    "-E",
    "-L",
    "-j",
    "--output-on-failure",
    "--preset",
    "--rerun-failed",
)


@dataclass
class SubCheck:
    id: str
    passed: bool
    detail: str


@dataclass
class CheckResult:
    id: str
    name: str
    subchecks: list[SubCheck] = field(default_factory=list)

    @property
    def passed(self) -> bool:
        return bool(self.subchecks) and all(item.passed for item in self.subchecks)

    @property
    def failures(self) -> list[str]:
        return [f"{item.id}: {item.detail}" for item in self.subchecks if not item.passed]

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "name": self.name,
            "passed": self.passed,
            "failures": self.failures,
            "subchecks": [asdict(item) for item in self.subchecks],
        }


@dataclass
class CommandCapture:
    returncode: int | None
    stdout: str
    stderr: str
    elapsed_seconds: float = 0.0
    timed_out: bool = False

    @property
    def combined(self) -> str:
        return f"{self.stdout}\n{self.stderr}"


def strip_ansi(text: str) -> str:
    return ANSI_RE.sub("", text)


def _select_generator() -> str:
    if shutil.which("ninja"):
        return "Ninja"
    return "Unix Makefiles"


def _clean_env(rcmake: Path, cache_dir: Path) -> dict[str, str]:
    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith("RCMAKE_") or key == "RCMAKE_VENDORED_DATA_ROOT"
    }
    if "RCMAKE_VENDORED_DATA_ROOT" in env:
        env["RCMAKE_VENDORED_DATA_ROOT"] = str(Path(env["RCMAKE_VENDORED_DATA_ROOT"]).resolve())
    elif os.name == "nt":
        env["RCMAKE_VENDORED_DATA_ROOT"] = str(rcmake.parent.parent / "share" / "cmake-4.5")
    env["RCMAKE_CACHE_DIR"] = str(cache_dir)
    env["NO_COLOR"] = "1"
    return env


def _run_cmd(
    argv: list[str | Path],
    *,
    cwd: Path,
    env: dict[str, str],
    timeout: float = 45.0,
) -> CommandCapture:
    start = time.monotonic()
    try:
        proc = subprocess.run(
            [str(part) for part in argv],
            cwd=cwd,
            env=env,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=timeout,
            check=False,
        )
        return CommandCapture(
            returncode=proc.returncode,
            stdout=proc.stdout,
            stderr=proc.stderr,
            elapsed_seconds=time.monotonic() - start,
            timed_out=False,
        )
    except subprocess.TimeoutExpired as exc:
        out = exc.stdout.decode("utf-8", errors="replace") if isinstance(exc.stdout, bytes) else (exc.stdout or "")
        err = exc.stderr.decode("utf-8", errors="replace") if isinstance(exc.stderr, bytes) else (exc.stderr or "")
        return CommandCapture(
            returncode=None,
            stdout=out,
            stderr=err,
            elapsed_seconds=time.monotonic() - start,
            timed_out=True,
        )


def _run_with_open_stdin(
    argv: list[str | Path],
    *,
    cwd: Path,
    env: dict[str, str],
    timeout: float = 2.0,
) -> CommandCapture:
    """Run a process while keeping stdin open so stdin-blocking hangs are detected."""
    start = time.monotonic()
    proc = subprocess.Popen(
        [str(part) for part in argv],
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    try:
        try:
            proc.wait(timeout=timeout)
            timed_out = False
        except subprocess.TimeoutExpired:
            timed_out = True
            proc.kill()
            proc.wait(timeout=5.0)
        elapsed = time.monotonic() - start
        stdout_bytes = proc.stdout.read() if proc.stdout else b""
        stderr_bytes = proc.stderr.read() if proc.stderr else b""
        return CommandCapture(
            returncode=None if timed_out else proc.returncode,
            stdout=stdout_bytes.decode("utf-8", errors="replace"),
            stderr=stderr_bytes.decode("utf-8", errors="replace"),
            elapsed_seconds=elapsed,
            timed_out=timed_out,
        )
    finally:
        for stream in (proc.stdin, proc.stdout, proc.stderr):
            if stream is not None:
                try:
                    stream.close()
                except OSError:
                    pass
        if proc.poll() is None:
            proc.kill()
            proc.wait(timeout=5.0)


def _parse_css_brightness(value: str) -> float | None:
    text = value.strip().lower()
    hex_match = re.fullmatch(r"#([0-9a-f]{3}|[0-9a-f]{6}|[0-9a-f]{8})", text)
    if hex_match:
        raw = hex_match.group(1)
        if len(raw) == 3:
            r, g, b = (int(ch * 2, 16) for ch in raw)
        else:
            r, g, b = int(raw[0:2], 16), int(raw[2:4], 16), int(raw[4:6], 16)
        return 0.299 * r + 0.587 * g + 0.114 * b
    rgb_match = re.fullmatch(
        r"rgba?\(\s*([0-9]+)\s*[,\s]\s*([0-9]+)\s*[,\s]\s*([0-9]+)(?:\s*[,/]\s*[0-9.]+%?)?\s*\)",
        text,
    )
    if rgb_match:
        r, g, b = (int(rgb_match.group(i)) for i in (1, 2, 3))
        return 0.299 * r + 0.587 * g + 0.114 * b
    return None


def _extract_dark_blocks(css: str) -> list[str]:
    blocks: list[str] = []
    for pattern in (
        r'@media\s*\(\s*prefers-color-scheme\s*:\s*dark\s*\)\s*\{([^{}]*(?:\{[^{}]*\}[^{}]*)*)\}',
        r':root\[data-theme\s*=\s*"?dark"?\]\s*\{([^}]*)\}',
    ):
        blocks.extend(re.findall(pattern, css, flags=re.IGNORECASE | re.DOTALL))
    return blocks


# ---------------------------------------------------------------------------
# Pure evaluators (unit-tested directly in tools/measure/test_ux_polish_harness.py)
# ---------------------------------------------------------------------------

def eval_hermetic(
    strict_clean: CommandCapture,
    warn_violations: CommandCapture,
    *,
    env_var_name: str = "RCMAKE_UX_POLISH_UNDECLARED_ENV",
    forbidden_file_hint: str = "/etc/hosts",
) -> CheckResult:
    result = CheckResult(id="1-hermetic", name="Hermetic strict/warn usability")

    strict_ok = not strict_clean.timed_out and strict_clean.returncode == 0
    strict_detail = (
        "exit 0 on clean C/C++ project"
        if strict_ok
        else f"expected exit 0, got rc={strict_clean.returncode}; stderr={strict_clean.stderr.strip()[:240]!r}"
    )
    result.subchecks.append(SubCheck("1a-strict-clean-ccxx", strict_ok, strict_detail))

    warn_text = strip_ansi(warn_violations.combined)
    warn_ok_rc = not warn_violations.timed_out and warn_violations.returncode == 0
    has_file_read = (
        warn_ok_rc
        and forbidden_file_hint in warn_text
        and bool(re.search(r"HERMETIC|outside the allowed|undeclared", warn_text, re.IGNORECASE))
    )
    result.subchecks.append(
        SubCheck(
            "1b-warn-reports-file-read",
            has_file_read,
            f"reported {forbidden_file_hint} violation"
            if has_file_read
            else f"missing {forbidden_file_hint} hermetic warning (rc={warn_violations.returncode})",
        )
    )

    has_env_read = warn_ok_rc and env_var_name in warn_text
    result.subchecks.append(
        SubCheck(
            "1c-warn-reports-env-read",
            has_env_read,
            f"reported {env_var_name} violation"
            if has_env_read
            else f"missing {env_var_name} hermetic warning (rc={warn_violations.returncode})",
        )
    )

    bundled_spam = bool(
        re.search(r"RCMAKE_HERMETIC_READ[^\n]*(?:share/cmake-[0-9.]+/Modules|/bin/(?:r?cmake))", warn_text)
    )
    omitted_match = re.search(r"Additional hermetic violations omitted:\s*([0-9]+)", warn_text)
    heavy_omission = omitted_match is not None and int(omitted_match.group(1)) > 20
    no_spam = warn_ok_rc and not bundled_spam and not heavy_omission
    spam_reasons: list[str] = []
    if bundled_spam:
        spam_reasons.append("reported bundled Modules/binary as RCMAKE_HERMETIC_READ")
    if heavy_omission and omitted_match is not None:
        spam_reasons.append(f"omitted {omitted_match.group(1)} warnings")
    if not warn_ok_rc:
        spam_reasons.append(f"rc={warn_violations.returncode}")
    result.subchecks.append(
        SubCheck(
            "1d-warn-no-bundled-spam",
            no_spam,
            "no bundled-module violation spam" if no_spam else "; ".join(spam_reasons),
        )
    )
    return result


def eval_ctest_help(capture: CommandCapture) -> CheckResult:
    result = CheckResult(id="2-ctest-help", name="ctest --help option table")
    exit_ok = not capture.timed_out and capture.returncode == 0
    result.subchecks.append(
        SubCheck(
            "2a-exit-zero",
            exit_ok,
            "ctest --help exited 0" if exit_ok else f"expected exit 0, got rc={capture.returncode}",
        )
    )
    text = strip_ansi(capture.stdout)
    missing = [
        opt
        for opt in REQUIRED_CTEST_HELP_OPTIONS
        if not re.search(rf"(?<![A-Za-z0-9_-]){re.escape(opt)}(?![A-Za-z0-9_-])", text)
    ]
    opts_ok = exit_ok and not missing
    result.subchecks.append(
        SubCheck(
            "2b-lists-options",
            opts_ok,
            "all required ctest options listed"
            if opts_ok
            else f"missing options in ctest --help: {', '.join(missing)}",
        )
    )
    return result


def eval_lsp_cli(help_cap: CommandCapture, version_cap: CommandCapture, *, max_seconds: float = 2.0) -> CheckResult:
    result = CheckResult(id="3-lsp-cli", name="rcmake-lsp --help and --version")
    help_text = strip_ansi(help_cap.stdout).strip()
    help_ok = (
        not help_cap.timed_out
        and help_cap.returncode == 0
        and help_cap.elapsed_seconds < max_seconds
        and bool(help_text)
        and bool(re.search(r"usage|options|rcmake-lsp", help_text, re.IGNORECASE))
    )
    help_detail = (
        f"exited 0 in {help_cap.elapsed_seconds:.2f}s with help text"
        if help_ok
        else (
            f"timed_out={help_cap.timed_out} rc={help_cap.returncode} "
            f"elapsed={help_cap.elapsed_seconds:.2f}s stdout={help_text[:120]!r}"
        )
    )
    result.subchecks.append(SubCheck("3a-lsp-help", help_ok, help_detail))

    ver_text = strip_ansi(version_cap.stdout).strip()
    ver_ok = (
        not version_cap.timed_out
        and version_cap.returncode == 0
        and version_cap.elapsed_seconds < max_seconds
        and bool(ver_text)
        and bool(re.search(r"rcmake-lsp|[0-9]+\.[0-9]+", ver_text, re.IGNORECASE))
    )
    ver_detail = (
        f"exited 0 in {version_cap.elapsed_seconds:.2f}s with version text"
        if ver_ok
        else (
            f"timed_out={version_cap.timed_out} rc={version_cap.returncode} "
            f"elapsed={version_cap.elapsed_seconds:.2f}s stdout={ver_text[:120]!r}"
        )
    )
    result.subchecks.append(SubCheck("3b-lsp-version", ver_ok, ver_detail))
    return result


def eval_explain(
    target_cap: CommandCapture,
    filter_cap: CommandCapture,
    typo_cap: CommandCapture,
    cli_var_cap: CommandCapture,
    cache_var_cap: CommandCapture,
    *,
    cache_line: int = 3,
) -> CheckResult:
    result = CheckResult(id="4-explain", name="rcmake explain signal-to-noise and accuracy")

    target_combined = strip_ansi(target_cap.combined)
    target_out = strip_ansi(target_cap.stdout)
    no_false_partial = (
        target_cap.returncode == 0
        and "Partial configure" not in target_combined
        and not re.search(r"denied\s+[0-9]+\s+subprocess", target_combined, re.IGNORECASE)
    )
    result.subchecks.append(
        SubCheck(
            "4a-no-false-partial",
            no_false_partial,
            "no false Partial configure or denied subprocess warning"
            if no_false_partial
            else f"rc={target_cap.returncode}; unexpected warning in output: {target_combined[:220]!r}",
        )
    )

    filter_out = strip_ansi(filter_cap.stdout)
    filter_ok = (
        filter_cap.returncode == 0
        and "-O2" in filter_out
        and "CMAKE_CXX_FLAGS_RELEASE" in filter_out
        and "No match for" not in filter_out
    )
    result.subchecks.append(
        SubCheck(
            "4b-filter-flag-token",
            filter_ok,
            "--filter -O2 matched inside CMAKE_CXX_FLAGS_RELEASE"
            if filter_ok
            else f"rc={filter_cap.returncode}; stdout={filter_out.strip()[:200]!r}",
        )
    )

    typo_combined = strip_ansi(typo_cap.combined)
    typo_ok = (
        typo_cap.returncode not in (None, 0)
        and bool(re.search(r"\bapp\b", typo_combined))
        and bool(re.search(r"did you mean|similar", typo_combined, re.IGNORECASE))
    )
    result.subchecks.append(
        SubCheck(
            "4c-typo-suggestion",
            typo_ok,
            "typo 'apq' suggested 'app'"
            if typo_ok
            else f"rc={typo_cap.returncode}; output={typo_combined.strip()[:200]!r}",
        )
    )

    # Check single-hop chain duplication: consecutive normalized lines with "app -> PRIVATE helper"
    hop_pattern = re.compile(r"^app\s*->\s*PRIVATE\s+helper$")
    non_empty = [line.strip() for line in target_out.splitlines() if line.strip()]
    has_dup_hop = any(
        hop_pattern.match(non_empty[idx]) and hop_pattern.match(non_empty[idx + 1])
        for idx in range(len(non_empty) - 1)
    )
    hop_present = any(hop_pattern.match(line) for line in non_empty)
    no_dup_hop = target_cap.returncode == 0 and hop_present and not has_dup_hop
    result.subchecks.append(
        SubCheck(
            "4d-no-duplicate-single-hop",
            no_dup_hop,
            "single-hop chain printed once"
            if no_dup_hop
            else (
                "duplicate consecutive single-hop line 'app -> PRIVATE helper'"
                if has_dup_hop
                else "expected single-hop chain 'app -> PRIVATE helper' not found"
            ),
        )
    )

    # Check synthetic line-0 "Called from CMakeLists.txt" (with no :line suffix)
    has_line0_called_from = bool(
        re.search(r"Called from\s+[^\n]*CMakeLists\.txt(?:\s|$)", target_out)
    )
    no_line0 = target_cap.returncode == 0 and not has_line0_called_from
    result.subchecks.append(
        SubCheck(
            "4e-no-line0-called-from",
            no_line0,
            "line-0 Called from CMakeLists.txt suppressed by default"
            if no_line0
            else "found redundant line-0 'Called from CMakeLists.txt' in default output",
        )
    )

    cli_out = strip_ansi(cli_var_cap.stdout)
    cache_out = strip_ansi(cache_var_cap.stdout)
    origins_errors: list[str] = []
    if cli_var_cap.returncode != 0:
        origins_errors.append(f"CMAKE_BUILD_TYPE rc={cli_var_cap.returncode}")
    if cache_var_cap.returncode != 0:
        origins_errors.append(f"MY_CACHE_OPT rc={cache_var_cap.returncode}")
    if "Source location unavailable." in cli_out:
        origins_errors.append("CMAKE_BUILD_TYPE prints 'Source location unavailable.'")
    if "Source location unavailable." in cache_out:
        origins_errors.append("MY_CACHE_OPT prints 'Source location unavailable.'")
    if not re.search(rf"CMakeLists\.txt:{cache_line}\b", cache_out):
        origins_errors.append(f"MY_CACHE_OPT missing CMakeLists.txt:{cache_line} origin")
    if not re.search(r"command[- ]line|-D|initial cache|cache", cli_out, re.IGNORECASE):
        origins_errors.append("CMAKE_BUILD_TYPE missing command-line/cache origin")
    origins_ok = not origins_errors
    result.subchecks.append(
        SubCheck(
            "4f-cache-origins",
            origins_ok,
            "-D and set(CACHE) report real origins without unavailable placeholders"
            if origins_ok
            else "; ".join(origins_errors),
        )
    )
    return result


def eval_rich_diagnostics(
    missing_paren: CommandCapture,
    unterm_string: CommandCapture,
    gutter_typo: CommandCapture,
    missing_target: CommandCapture,
    send_error: CommandCapture,
    policy_version: CommandCapture,
) -> CheckResult:
    result = CheckResult(id="5-rich-diagnostics", name="Rich diagnostics snippets, alignment, codes, and hints")

    paren_err = strip_ansi(missing_paren.stderr)
    paren_ok = (
        missing_paren.returncode not in (None, 0)
        and bool(re.search(rf"\b3\s+{re.escape(GUTTER_BAR)}\s+if\(TRUE", paren_err))
    )
    result.subchecks.append(
        SubCheck(
            "5a-parse-missing-paren-snippet",
            paren_ok,
            "missing ')' parse error renders source line snippet"
            if paren_ok
            else f"missing source line snippet for if(TRUE; stderr={paren_err.strip()[:220]!r}",
        )
    )

    str_err = strip_ansi(unterm_string.stderr)
    has_stray_quote = bool(re.search(r'^\s*"\.\s*$', str_err, re.MULTILINE))
    has_str_snippet = bool(re.search(rf"{re.escape(GUTTER_BAR)}\s+.*hello", str_err))
    str_ok = unterm_string.returncode not in (None, 0) and not has_stray_quote and has_str_snippet
    str_reasons: list[str] = []
    if unterm_string.returncode in (None, 0):
        str_reasons.append(f"rc={unterm_string.returncode}")
    if has_stray_quote:
        str_reasons.append('found broken stray `".` line')
    if not has_str_snippet:
        str_reasons.append("missing source snippet for unterminated string")
    result.subchecks.append(
        SubCheck(
            "5b-unterminated-string-clean",
            str_ok,
            "unterminated string renders cleanly with source snippet" if str_ok else "; ".join(str_reasons),
        )
    )

    typo_err = strip_ansi(gutter_typo.stderr)
    typo_lines = typo_err.splitlines()
    checked_pairs = 0
    misaligned: list[str] = []
    for idx, line in enumerate(typo_lines):
        if GUTTER_ARROW in line:
            arrow_col = line.index(GUTTER_ARROW)
            for next_line in typo_lines[idx + 1 : idx + 4]:
                if GUTTER_BAR in next_line:
                    bar_col = next_line.index(GUTTER_BAR)
                    checked_pairs += 1
                    if arrow_col != bar_col:
                        misaligned.append(f"line {idx + 1}: arrow col {arrow_col} != bar col {bar_col}")
                    break
    align_ok = gutter_typo.returncode not in (None, 0) and checked_pairs > 0 and not misaligned
    result.subchecks.append(
        SubCheck(
            "5c-gutter-alignment",
            align_ok,
            f"gutter arrow aligned with bar across {checked_pairs} diagnostic(s)"
            if align_ok
            else ("; ".join(misaligned) if misaligned else "no rich gutter found to verify alignment"),
        )
    )

    mt_err = strip_ansi(missing_target.stderr)
    mt_is_rich = GUTTER_ARROW in mt_err or bool(re.search(r"\berror\[", mt_err))
    mt_has_hint = bool(re.search(r"find_package\s*\(\s*fmt\b", mt_err, re.IGNORECASE))
    mt_ok = missing_target.returncode not in (None, 0) and mt_is_rich and mt_has_hint
    mt_reasons: list[str] = []
    if missing_target.returncode in (None, 0):
        mt_reasons.append(f"rc={missing_target.returncode}")
    if not mt_is_rich:
        mt_reasons.append("plain CMake output instead of rich diagnostic")
    if not mt_has_hint:
        mt_reasons.append("missing find_package(fmt ...) hint")
    result.subchecks.append(
        SubCheck(
            "5d-missing-target-rich-hint",
            mt_ok,
            "missing fmt::fmt renders rich diagnostic with find_package hint"
            if mt_ok
            else "; ".join(mt_reasons),
        )
    )

    se_err = strip_ansi(send_error.stderr)
    se_ok = (
        send_error.returncode not in (None, 0)
        and "non-fatal send error" in se_err
        and bool(re.search(r"\berror\[", se_err))
        and "CMake.FatalError" not in se_err
    )
    result.subchecks.append(
        SubCheck(
            "5e-send-error-code",
            se_ok,
            "message(SEND_ERROR) uses non-fatal diagnostic code"
            if se_ok
            else (
                "message(SEND_ERROR) is mislabeled as CMake.FatalError"
                if "CMake.FatalError" in se_err
                else f"unexpected SEND_ERROR output: {se_err.strip()[:200]!r}"
            ),
        )
    )

    pv_err = strip_ansi(policy_version.stderr)
    pv_ok = (
        policy_version.returncode not in (None, 0)
        and bool(re.search(r"\bhelp:", pv_err))
        and bool(re.search(r"CMAKE_POLICY_VERSION_MINIMUM|\.\.\.|version range|3\.5", pv_err, re.IGNORECASE))
    )
    result.subchecks.append(
        SubCheck(
            "5f-policy-version-help",
            pv_ok,
            "policy-version error includes help: hint"
            if pv_ok
            else f"missing help: hint on policy-version error; stderr={pv_err.strip()[:220]!r}",
        )
    )
    return result


def eval_fetch_lock(update_cap: CommandCapture, frozen_cap: CommandCapture) -> CheckResult:
    result = CheckResult(id="6-fetch-lock", name="FetchContent lockfile diagnostics")

    frozen_text = strip_ansi(frozen_cap.combined)
    frozen_reasons: list[str] = []
    if frozen_cap.returncode in (None, 0):
        frozen_reasons.append(f"expected non-zero exit, got rc={frozen_cap.returncode}")
    if "FETCHCONTENT_SOURCE_DIR_DEP" not in frozen_text:
        frozen_reasons.append("missing real dependency name FETCHCONTENT_SOURCE_DIR_DEP")
    if "<source>" in frozen_text:
        frozen_reasons.append("contains literal <source> placeholder")
    if "<NAME>" in frozen_text:
        frozen_reasons.append("contains literal <NAME> placeholder")
    frozen_ok = not frozen_reasons
    result.subchecks.append(
        SubCheck(
            "6a-frozen-no-placeholders",
            frozen_ok,
            "frozen error names FETCHCONTENT_SOURCE_DIR_DEP with zero placeholders"
            if frozen_ok
            else "; ".join(frozen_reasons),
        )
    )

    update_text = strip_ansi(update_cap.combined)
    update_ok = (
        update_cap.returncode == 0
        and bool(re.search(r"\bdep\b", update_text, re.IGNORECASE))
        and bool(
            re.search(
                r"not pinned|unpinned|cannot (?:be )?pin|could not pin|skipped|not written|no lock",
                update_text,
                re.IGNORECASE,
            )
        )
    )
    result.subchecks.append(
        SubCheck(
            "6b-update-reports-unpinned",
            update_ok,
            "--fetch-lock=update clearly reports unpinned dependency 'dep'"
            if update_ok
            else f"update did not report unpinned dependency 'dep' (rc={update_cap.returncode}, out={update_text.strip()[:200]!r})",
        )
    )
    return result


def eval_profile(
    profile_cap: CommandCapture,
    *,
    predictable_html_exists: bool,
    flamegraph_html: str | None,
) -> CheckResult:
    result = CheckResult(id="7-profile", name="Profile layout, terminal table, and flamegraph HTML")

    path_ok = profile_cap.returncode == 0 and predictable_html_exists
    result.subchecks.append(
        SubCheck(
            "7a-predictable-flamegraph-path",
            path_ok,
            "exposes <dir>/flamegraph.html or <dir>/latest/flamegraph.html"
            if path_ok
            else "neither <dir>/flamegraph.html nor <dir>/latest/flamegraph.html exists",
        )
    )

    prof_err = strip_ansi(profile_cap.stderr)
    dash_col = re.escape(EM_DASH)
    dead_phase_rows = re.findall(
        rf"^(?:\s*{dash_col}\s+{dash_col}\s+{dash_col}\s+{dash_col}\s+(?:parse|probes|resolve|eval|generate)\b"
        rf"|\s*(?:parse|probes|resolve|eval|generate)\s+{dash_col}\s+{dash_col})",
        prof_err,
        flags=re.MULTILINE,
    )
    no_dead_rows = profile_cap.returncode == 0 and not dead_phase_rows
    result.subchecks.append(
        SubCheck(
            "7b-no-dead-dash-phase-rows",
            no_dead_rows,
            "no dead em-dash phase rows in profile summary"
            if no_dead_rows
            else f"found {len(dead_phase_rows)} unmeasured dash phase row(s)",
        )
    )

    if flamegraph_html is None:
        for sub_id in (
            "7c-flamegraph-dark-dim-contrast",
            "7d-flamegraph-legend-and-walltime",
            "7e-flamegraph-no-mangled-labels",
        ):
            result.subchecks.append(SubCheck(sub_id, False, "flamegraph.html not generated"))
        return result

    dark_blocks = _extract_dark_blocks(flamegraph_html)
    dark_dims: list[tuple[str, float]] = []
    dark_bars: list[float] = []
    for block in dark_blocks:
        for dim_val in re.findall(r"--dim\s*:\s*([^;}\n]+)", block):
            brightness = _parse_css_brightness(dim_val)
            if brightness is not None:
                dark_dims.append((dim_val.strip(), brightness))
        for bar_val in re.findall(r"--bar\s*:\s*([^;}\n]+)", block):
            brightness = _parse_css_brightness(bar_val)
            if brightness is not None:
                dark_bars.append(brightness)
    bar_min = min(dark_bars) if dark_bars else 255.0
    dim_ok = bool(dark_dims) and all(b < 128.0 and b < bar_min for _, b in dark_dims)
    dim_detail = (
        f"dark --dim colors have dark contrast ({', '.join(v for v, _ in dark_dims)})"
        if dim_ok
        else (
            f"dark --dim too bright: {', '.join(f'{v} (luma={b:.0f})' for v, b in dark_dims)}"
            if dark_dims
            else "no dark-mode --dim CSS variable found"
        )
    )
    result.subchecks.append(SubCheck("7c-flamegraph-dark-dim-contrast", dim_ok, dim_detail))

    html_body = re.sub(
        r"<(?:script|style)\b[^>]*>.*?</(?:script|style)>",
        "",
        flamegraph_html,
        flags=re.IGNORECASE | re.DOTALL,
    )
    legend_terms = sum(
        1 for term in ("phase", "listfile", "builtin", "macro", "function")
        if re.search(rf"\b{term}\b", html_body, re.IGNORECASE)
    )
    has_legend = bool(re.search(r"legend", html_body, re.IGNORECASE)) or legend_terms >= 3
    header_match = re.search(r"<header\b[^>]*>(.*?)</header>", html_body, flags=re.IGNORECASE | re.DOTALL)
    header_html = header_match.group(1) if header_match else ""
    has_walltime_header = bool(
        re.search(r"wall|summary|meta|total|[0-9]+(?:\.[0-9]+)?\s*(?:ms|s|\u00b5s|ns)\b", header_html, re.IGNORECASE)
    ) or bool(
        re.search(
            r"byId\(['\"][^'\"]*(?:summary|wall|header|meta|total)[^'\"]*['\"]\)",
            flamegraph_html,
            re.IGNORECASE,
        )
    )
    legend_wall_ok = has_legend and has_walltime_header
    lw_missing: list[str] = []
    if not has_legend:
        lw_missing.append("missing color legend")
    if not has_walltime_header:
        lw_missing.append("missing wall-time header")
    result.subchecks.append(
        SubCheck(
            "7d-flamegraph-legend-and-walltime",
            legend_wall_ok,
            "flamegraph.html includes color legend and wall-time header"
            if legend_wall_ok
            else "; ".join(lw_missing),
        )
    )

    # Detect unconditional middle-truncation on command names (produces f...am / f<ellipsis>am).
    has_middle_concat = bool(
        re.search(
            rf"(?:\+\s*['\"](?:{re.escape(ELLIPSIS)}|\.\.\.)['\"]\s*\+\s*suffix)",
            flamegraph_html,
        )
    )
    has_kind_or_path_guard = bool(
        re.search(
            r"node\.kind\s*===?\s*['\"]listfile['\"]|includes\(['\"][\\/]['\"]\)|indexOf\(['\"][\\/]['\"]\)",
            flamegraph_html,
        )
    )
    old_width_math = "Math.floor((width - 10) / 8)" in flamegraph_html
    labels_ok = not (has_middle_concat and not has_kind_or_path_guard) and not old_width_math
    label_reasons: list[str] = []
    if has_middle_concat and not has_kind_or_path_guard:
        label_reasons.append("unconditional middle truncation mangles command labels")
    if old_width_math:
        label_reasons.append("uses overly conservative (width - 10) / 8 label capacity")
    result.subchecks.append(
        SubCheck(
            "7e-flamegraph-no-mangled-labels",
            labels_ok,
            "flamegraph labels avoid unconditional middle truncation on commands"
            if labels_ok
            else "; ".join(label_reasons),
        )
    )
    return result


def eval_branding(
    help_cap: CommandCapture,
    profile_cap: CommandCapture,
    flamegraph_html: str | None,
    explain_help_cap: CommandCapture,
    explain_run_cap: CommandCapture,
    missing_source_cap: CommandCapture,
    empty_source_cap: CommandCapture,
) -> CheckResult:
    result = CheckResult(id="8-branding", name="Consistent lowercase rcmake branding and zero CMake GUI text")

    help_text = strip_ansi(help_cap.stdout)
    usage_errors: list[str] = []
    if help_cap.returncode != 0:
        usage_errors.append(f"rcmake --help rc={help_cap.returncode}")
    if "Rcmake" in help_text:
        usage_errors.append("found capitalized 'Rcmake' in rcmake --help")
    if "rcmake [options]" not in help_text:
        usage_errors.append("rcmake --help usage line does not show 'rcmake [options]'")
    result.subchecks.append(
        SubCheck(
            "8a-usage-branding",
            not usage_errors,
            "rcmake --help uses lowercase rcmake branding" if not usage_errors else "; ".join(usage_errors),
        )
    )

    prof_err = strip_ansi(profile_cap.stderr)
    prof_errors: list[str] = []
    if "Rcmake" in prof_err:
        prof_errors.append("found capitalized 'Rcmake' in --profile terminal output")
    if flamegraph_html is not None and re.search(r"<(?:title|h1)>[^<]*Rcmake", flamegraph_html):
        prof_errors.append("found capitalized 'Rcmake' in flamegraph.html title/heading")
    result.subchecks.append(
        SubCheck(
            "8b-profile-branding",
            not prof_errors,
            "profile terminal and HTML use lowercase rcmake" if not prof_errors else "; ".join(prof_errors),
        )
    )

    explain_text = strip_ansi(explain_help_cap.combined) + "\n" + strip_ansi(explain_run_cap.combined)
    explain_ok = "Rcmake" not in explain_text
    result.subchecks.append(
        SubCheck(
            "8c-explain-branding",
            explain_ok,
            "explain output uses lowercase rcmake"
            if explain_ok
            else "found capitalized 'Rcmake' in rcmake explain output",
        )
    )

    gui_errors: list[str] = []
    for label, cap in (("missing source dir", missing_source_cap), ("empty source dir", empty_source_cap)):
        combined = strip_ansi(cap.combined)
        if cap.returncode in (None, 0):
            gui_errors.append(f"{label} unexpectedly exited 0")
        if "CMake GUI" in combined:
            gui_errors.append(f"{label} mentions 'CMake GUI'")
    result.subchecks.append(
        SubCheck(
            "8d-no-cmake-gui-in-native-errors",
            not gui_errors,
            "native rcmake error messages omit 'CMake GUI'" if not gui_errors else "; ".join(gui_errors),
        )
    )
    return result


# ---------------------------------------------------------------------------
# Live runners executing real binaries in a TemporaryDirectory
# ---------------------------------------------------------------------------

def _make_alias(rcmake: Path, alias_dir: Path, name: str) -> Path:
    alias = alias_dir / f"{name}{rcmake.suffix}"
    if alias.exists() or alias.is_symlink():
        return alias
    try:
        alias.symlink_to(rcmake.resolve())
    except OSError:
        if os.name != "nt":
            raise
        shutil.copy2(rcmake, alias)
    return alias


def _read_flamegraph(prof_dir: Path) -> tuple[bool, str | None]:
    for candidate in (prof_dir / "flamegraph.html", prof_dir / "latest" / "flamegraph.html"):
        if candidate.is_file():
            return True, candidate.read_text(encoding="utf-8", errors="replace")
    current_file = prof_dir / "current"
    if current_file.is_file():
        run_name = current_file.read_text(encoding="utf-8", errors="replace").strip()
        fallback = prof_dir / run_name / "flamegraph.html"
        if fallback.is_file():
            return False, fallback.read_text(encoding="utf-8", errors="replace")
    return False, None


def run_all_checks(rcmake: Path, lsp: Path | None) -> dict[str, Any]:
    generator = _select_generator()
    checks: list[CheckResult] = []

    with tempfile.TemporaryDirectory(prefix="rcmake-ux-polish-") as tmp_str:
        root = Path(tmp_str).resolve()
        env = _clean_env(rcmake, root / "cache")

        # --- Check 1: Hermetic mode ---
        herm_clean_src = root / "herm_clean"
        herm_clean_src.mkdir()
        (herm_clean_src / "main.cpp").write_text(
            'extern "C" int helper_val(void);\nint main() { return helper_val(); }\n',
            encoding="utf-8",
        )
        (herm_clean_src / "helper.c").write_text(
            "int helper_val(void) { return 0; }\n",
            encoding="utf-8",
        )
        (herm_clean_src / "CMakeLists.txt").write_text(
            "cmake_minimum_required(VERSION 4.5)\n"
            "project(HermeticClean LANGUAGES C CXX)\n"
            "add_executable(hello main.cpp helper.c)\n",
            encoding="utf-8",
        )
        strict_cap = _run_cmd(
            [rcmake, "-S", herm_clean_src, "-B", root / "herm_clean_build", "-G", generator, "--hermetic=strict"],
            cwd=root,
            env=env,
        )

        herm_warn_src = root / "herm_warn"
        herm_warn_src.mkdir()
        (herm_warn_src / "main.cpp").write_text("int main() { return 0; }\n", encoding="utf-8")
        forbidden_file = "/etc/hosts"
        if not Path(forbidden_file).is_file():
            ext_file = root / "outside-secret.txt"
            ext_file.write_text("external\n", encoding="utf-8")
            forbidden_file = ext_file.as_posix()
        (herm_warn_src / "CMakeLists.txt").write_text(
            "cmake_minimum_required(VERSION 4.5)\n"
            "project(HermeticWarn LANGUAGES C CXX)\n"
            'set(_secret "$ENV{RCMAKE_UX_POLISH_UNDECLARED_ENV}")\n'
            f'file(READ "{forbidden_file}" _hosts LIMIT 16)\n'
            "add_executable(hello main.cpp)\n",
            encoding="utf-8",
        )
        warn_env = env | {"RCMAKE_UX_POLISH_UNDECLARED_ENV": "sentinel_value"}
        warn_cap = _run_cmd(
            [rcmake, "-S", herm_warn_src, "-B", root / "herm_warn_build", "-G", generator, "--hermetic=warn"],
            cwd=root,
            env=warn_env,
        )
        checks.append(eval_hermetic(strict_cap, warn_cap, forbidden_file_hint=forbidden_file))

        # --- Check 2: ctest --help ---
        alias_dir = root / "aliases"
        alias_dir.mkdir()
        ctest_bin = _make_alias(rcmake, alias_dir, "ctest")
        ctest_help_cap = _run_cmd([ctest_bin, "--help"], cwd=root, env=env)
        checks.append(eval_ctest_help(ctest_help_cap))

        # --- Check 3: rcmake-lsp --help and --version ---
        if lsp is not None and lsp.is_file():
            lsp_help_cap = _run_with_open_stdin([lsp, "--help"], cwd=root, env=env, timeout=2.0)
            lsp_ver_cap = _run_with_open_stdin([lsp, "--version"], cwd=root, env=env, timeout=2.0)
            checks.append(eval_lsp_cli(lsp_help_cap, lsp_ver_cap))
        else:
            missing_msg = f"rcmake-lsp binary not found (path={lsp})"
            checks.append(
                CheckResult(
                    id="3-lsp-cli",
                    name="rcmake-lsp --help and --version",
                    subchecks=[
                        SubCheck("3a-lsp-help", False, missing_msg),
                        SubCheck("3b-lsp-version", False, missing_msg),
                    ],
                )
            )

        # --- Check 4: rcmake explain ---
        exp_src = root / "explain_proj"
        exp_src.mkdir()
        (exp_src / "helper.cpp").write_text("int helper_fn() { return HELPER_DEF; }\n", encoding="utf-8")
        (exp_src / "main.cpp").write_text("int helper_fn();\nint main() { return helper_fn(); }\n", encoding="utf-8")
        (exp_src / "CMakeLists.txt").write_text(
            "cmake_minimum_required(VERSION 4.5)\n"
            "project(ExplainPolish LANGUAGES CXX)\n"
            'set(MY_CACHE_OPT "enabled" CACHE STRING "Polish test cache option")\n'
            'set(CMAKE_CXX_FLAGS_RELEASE "-O2 -DNDEBUG")\n'
            "add_library(helper STATIC helper.cpp)\n"
            "target_compile_definitions(helper PUBLIC HELPER_DEF=1)\n"
            "add_executable(app main.cpp)\n"
            "target_link_libraries(app PRIVATE helper)\n",
            encoding="utf-8",
        )
        exp_build = root / "explain_build"
        _run_cmd(
            [rcmake, "-S", exp_src, "-B", exp_build, "-G", generator, "-DCMAKE_BUILD_TYPE=Release"],
            cwd=root,
            env=env,
        )
        exp_target_cap = _run_cmd([rcmake, "explain", "app", "-B", exp_build, "--color=never"], cwd=root, env=env)
        exp_filter_cap = _run_cmd(
            [rcmake, "explain", "app", "-B", exp_build, "--filter", "-O2", "--color=never"],
            cwd=root,
            env=env,
        )
        exp_typo_cap = _run_cmd([rcmake, "explain", "apq", "-B", exp_build, "--color=never"], cwd=root, env=env)
        exp_cli_var_cap = _run_cmd(
            [rcmake, "explain", "CMAKE_BUILD_TYPE", "-B", exp_build, "--var", "--color=never"],
            cwd=root,
            env=env,
        )
        exp_cache_var_cap = _run_cmd(
            [rcmake, "explain", "MY_CACHE_OPT", "-B", exp_build, "--var", "--color=never"],
            cwd=root,
            env=env,
        )
        checks.append(
            eval_explain(
                exp_target_cap,
                exp_filter_cap,
                exp_typo_cap,
                exp_cli_var_cap,
                exp_cache_var_cap,
                cache_line=3,
            )
        )

        # --- Check 5: --diagnostics=rich ---
        def run_diag_case(name: str, cmakelists: str, extra_files: dict[str, str] | None = None) -> CommandCapture:
            src = root / f"diag_{name}"
            src.mkdir()
            (src / "CMakeLists.txt").write_text(cmakelists, encoding="utf-8")
            for rel, content in (extra_files or {}).items():
                (src / rel).write_text(content, encoding="utf-8")
            return _run_cmd(
                [rcmake, "-S", src, "-B", root / f"diag_{name}_b", "-G", generator, "--diagnostics=rich"],
                cwd=root,
                env=env,
            )

        diag_missing_paren = run_diag_case(
            "paren",
            "cmake_minimum_required(VERSION 4.5)\nproject(DiagParen LANGUAGES NONE)\nif(TRUE\n",
        )
        diag_unterm_str = run_diag_case(
            "unterm",
            'cmake_minimum_required(VERSION 4.5)\nproject(DiagUnterm LANGUAGES NONE)\nmessage(STATUS "hello)\n',
        )
        diag_gutter = run_diag_case(
            "gutter",
            "cmake_minimum_required(VERSION 4.5)\nproject(DiagGutter LANGUAGES NONE)\nadd_executabel(typo_app main.cpp)\n",
        )
        diag_missing_target = run_diag_case(
            "target",
            "cmake_minimum_required(VERSION 4.5)\n"
            "project(DiagTarget LANGUAGES C)\n"
            "add_executable(app main.c)\n"
            "target_link_libraries(app PRIVATE fmt::fmt)\n",
            {"main.c": "int main(void) { return 0; }\n"},
        )
        diag_send_error = run_diag_case(
            "send",
            "cmake_minimum_required(VERSION 4.5)\n"
            "project(DiagSend LANGUAGES NONE)\n"
            'message(SEND_ERROR "non-fatal send error")\n',
        )
        diag_policy = run_diag_case(
            "policy",
            "cmake_minimum_required(VERSION 3.1)\nproject(DiagPolicy LANGUAGES NONE)\n",
        )
        checks.append(
            eval_rich_diagnostics(
                diag_missing_paren,
                diag_unterm_str,
                diag_gutter,
                diag_missing_target,
                diag_send_error,
                diag_policy,
            )
        )

        # --- Check 6: --fetch-lock=update & --fetch-lock=frozen ---
        dep_tar = root / "dep.tar.gz"
        with tarfile.open(dep_tar, "w:gz") as tf:
            dep_cmake = b"add_library(dep INTERFACE)\n"
            info = tarfile.TarInfo(name="dep/CMakeLists.txt")
            info.size = len(dep_cmake)
            tf.addfile(info, io.BytesIO(dep_cmake))
        fetch_src = root / "fetch_proj"
        fetch_src.mkdir()
        (fetch_src / "CMakeLists.txt").write_text(
            "cmake_minimum_required(VERSION 4.5)\n"
            "project(FetchPolish LANGUAGES NONE)\n"
            "include(FetchContent)\n"
            f'FetchContent_Declare(dep URL "file://{dep_tar.as_posix()}" DOWNLOAD_COMMAND "")\n'
            "FetchContent_MakeAvailable(dep)\n",
            encoding="utf-8",
        )
        fetch_update_cap = _run_cmd(
            [rcmake, "-S", fetch_src, "-B", root / "fetch_b_update", "-G", generator, "--fetch-lock=update"],
            cwd=root,
            env=env,
        )
        fetch_frozen_cap = _run_cmd(
            [rcmake, "-S", fetch_src, "-B", root / "fetch_b_frozen", "-G", generator, "--fetch-lock=frozen"],
            cwd=root,
            env=env,
        )
        checks.append(eval_fetch_lock(fetch_update_cap, fetch_frozen_cap))

        # --- Check 7: --profile=<dir> ---
        prof_dir = root / "prof_out"
        prof_cap = _run_cmd(
            [rcmake, "-S", exp_src, "-B", root / "prof_build", "-G", generator, f"--profile={prof_dir}"],
            cwd=root,
            env=env,
        )
        predictable_exists, flamegraph_html = _read_flamegraph(prof_dir)
        checks.append(
            eval_profile(
                prof_cap,
                predictable_html_exists=predictable_exists,
                flamegraph_html=flamegraph_html,
            )
        )

        # --- Check 8: Lowercase rcmake branding & zero CMake GUI text ---
        help_cap = _run_cmd([rcmake, "--help"], cwd=root, env=env)
        explain_help_cap = _run_cmd([rcmake, "explain", "--help"], cwd=root, env=env)
        empty_src = root / "empty_src"
        empty_src.mkdir()
        missing_src_cap = _run_cmd([rcmake, "-S", root / "does_not_exist", "-B", root / "b_missing"], cwd=root, env=env)
        empty_src_cap = _run_cmd([rcmake, "-S", empty_src, "-B", root / "b_empty"], cwd=root, env=env)
        checks.append(
            eval_branding(
                help_cap,
                prof_cap,
                flamegraph_html,
                explain_help_cap,
                exp_target_cap,
                missing_src_cap,
                empty_src_cap,
            )
        )

    passed_checks = sum(1 for item in checks if item.passed)
    total_subchecks = sum(len(item.subchecks) for item in checks)
    passed_subchecks = sum(1 for item in checks for sub in item.subchecks if sub.passed)
    return {
        "schema": "rcmake-ux-polish-v1",
        "rcmake": str(rcmake),
        "lsp": str(lsp) if lsp is not None else None,
        "generator": generator,
        "total_checks": len(checks),
        "passed_checks": passed_checks,
        "failed_checks": len(checks) - passed_checks,
        "total_subchecks": total_subchecks,
        "passed_subchecks": passed_subchecks,
        "failed_subchecks": total_subchecks - passed_subchecks,
        "all_passed": passed_checks == len(checks),
        "checks": [item.to_dict() for item in checks],
    }


def _discover_default_lsp(rcmake: Path) -> Path | None:
    suffix = ".exe" if os.name == "nt" else ""
    name = f"rcmake-lsp{suffix}"
    for candidate in (
        rcmake.parent / name,
        rcmake.parent.parent / name,
        rcmake.parent.parent.parent / name,
    ):
        if candidate.is_file():
            return candidate
    return None


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rcmake", type=Path, required=True, help="Path to the native rcmake executable.")
    parser.add_argument("--lsp", type=Path, default=None, help="Path to the rcmake-lsp executable.")
    parser.add_argument("--out-json", type=Path, default=None, help="Optional path to write the JSON report.")
    args = parser.parse_args(argv)

    rcmake = args.rcmake.absolute()
    if rcmake.name != ("rcmake.exe" if os.name == "nt" else "rcmake"):
        parser.error("--rcmake must name the native rcmake executable.")
    if not rcmake.is_file():
        parser.error(f"--rcmake executable not found: {rcmake}")

    lsp = args.lsp.absolute() if args.lsp is not None else _discover_default_lsp(rcmake)
    report = run_all_checks(rcmake, lsp)

    if args.out_json is not None:
        args.out_json.parent.mkdir(parents=True, exist_ok=True)
        args.out_json.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    for check in report["checks"]:
        status = "PASS" if check["passed"] else "FAIL"
        sub_passed = sum(1 for sub in check["subchecks"] if sub["passed"])
        sub_total = len(check["subchecks"])
        print(f"[{status}] {check['id']} ({sub_passed}/{sub_total}): {check['name']}")
        for failure in check["failures"]:
            print(f"       - {failure}")

    print(
        f"\nSummary: {report['passed_checks']}/{report['total_checks']} checks passed, "
        f"{report['passed_subchecks']}/{report['total_subchecks']} subchecks passed."
    )
    return 0 if report["all_passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
