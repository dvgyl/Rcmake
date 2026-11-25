# SPDX-License-Identifier: MIT OR Apache-2.0
"""Unit tests for deterministic METRIC marker stamping and verification."""

from __future__ import annotations

from contextlib import redirect_stderr, redirect_stdout
import io
import json
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest

from tools.measure import stamp_metrics as stamper
from tools.measure.test_verify_release_receipts import (
    BINARY_OK,
    BINARY_STALE,
    COMMIT_OK,
    COMMIT_STALE,
    make_full_dashboard,
)


class StampMetricsTests(unittest.TestCase):
    def setUp(self):
        tmp = TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.readme = self.root / "README.md"
        self.writeup = self.root / "BUILD_WRITEUP.md"
        self.readme.write_text((stamper.ROOT / "README.md").read_text(encoding="utf-8"), encoding="utf-8")
        self.writeup.write_text(
            (stamper.ROOT / "docs" / "BUILD_WRITEUP.md").read_text(encoding="utf-8"),
            encoding="utf-8",
        )
        self.dashboard_path = self.root / "latest.json"
        self.receipts_path = self.root / "receipts.json"
        self.write_json(self.dashboard_path, make_full_dashboard())
        self.write_json(self.receipts_path, self.make_receipts_manifest())

    def write_json(self, path: Path, payload: dict) -> Path:
        path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
        return path

    def make_receipts_manifest(self, commit: str = COMMIT_OK, binary: str = BINARY_OK) -> dict:
        return {
            "schema_version": 1,
            "release_commit": commit,
            "binary_sha256": binary,
            "binary_size_macos_bytes": 15_728_640,
            "known_parity_gaps": [],
            "receipts": {
                "ci_jobs": {"release_commit": commit, "binary_sha256": binary, "passed": 6, "total": 6},
                "conan": {
                    "release_commit": commit,
                    "binary_sha256": binary,
                    "status": "pass",
                    "scenarios": [
                        {"name": "conan-consumer", "status": "pass"},
                        {"name": "conan-source-build", "status": "pass"},
                    ],
                },
                "path_swap": {
                    "release_commit": commit,
                    "binary_sha256": binary,
                    "status": "pass",
                    "differences": [],
                },
            },
        }

    def run_cli(self, *args: str) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            code = stamper.main(list(args))
        return code, out.getvalue(), err.getvalue()

    def test_worktree_markdown_markers_match_expected_25_keys(self):
        readme_keys = {m.key for m in stamper.scan_markers(self.readme.read_text(encoding="utf-8"), "README.md")}
        writeup_keys = {
            m.key for m in stamper.scan_markers(self.writeup.read_text(encoding="utf-8"), "BUILD_WRITEUP.md")
        }
        self.assertEqual(len(readme_keys), 19)
        self.assertEqual(len(writeup_keys), 25)
        self.assertEqual(readme_keys | writeup_keys, stamper.EXPECTED_KEYS)
        self.assertTrue(readme_keys.issubset(writeup_keys))

        # Running --check against the current repo latest.json (commit: null) must refuse cleanly.
        code, _, stderr = self.run_cli(
            "--dashboard", str(stamper.DEFAULT_DASHBOARD),
            "--file", str(self.readme),
            "--file", str(self.writeup),
            "--check",
        )
        self.assertEqual(code, 1)
        self.assertIn("commit: null", stderr)

    def test_valid_full_stamp_check_and_round_trip_idempotency(self):
        common = (
            "--dashboard", str(self.dashboard_path),
            "--receipts", str(self.receipts_path),
            "--file", str(self.readme),
            "--file", str(self.writeup),
        )
        # Before stamping, --check must reject pending markers.
        code, _, stderr = self.run_cli(*common, "--check")
        self.assertEqual(code, 1)
        self.assertIn("[pending]", stderr)

        # First --write stamps all 25 markers cleanly.
        code, stdout, stderr = self.run_cli(*common, "--write")
        self.assertEqual(code, 0, stderr)
        self.assertIn("OK: stamped 25 metrics", stdout)

        first_readme = self.readme.read_text(encoding="utf-8")
        first_writeup = self.writeup.read_text(encoding="utf-8")
        for label, content in (("README.md", first_readme), ("BUILD_WRITEUP.md", first_writeup)):
            for marker in stamper.scan_markers(content, label):
                self.assertNotRegex(marker.value, stamper.PENDING_VALUE_RE)
        self.assertIn(f"<!-- METRIC:release_commit -->`{COMMIT_OK[:12]}`<!-- /METRIC -->", first_readme)
        self.assertIn("<!-- METRIC:binary_size_macos -->15.00 MiB (15,728,640 bytes)<!-- /METRIC -->", first_writeup)
        self.assertIn("<!-- METRIC:known_parity_gaps -->none<!-- /METRIC -->", first_readme)

        # Immediate --check passes with zero drift.
        code, stdout, stderr = self.run_cli(*common, "--check")
        self.assertEqual(code, 0, stderr)
        self.assertIn("OK: verified 25 metrics", stdout)

        # Second --write is strictly idempotent (no changes, byte-identical output).
        code, stdout, stderr = self.run_cli(*common, "--write")
        self.assertEqual(code, 0, stderr)
        self.assertIn("no changes needed", stdout)
        self.assertEqual(self.readme.read_text(encoding="utf-8"), first_readme)
        self.assertEqual(self.writeup.read_text(encoding="utf-8"), first_writeup)

    def test_refuses_null_commit_and_leaves_files_untouched(self):
        original_readme = self.readme.read_text(encoding="utf-8")
        dash = make_full_dashboard()
        dash["commit"] = None
        self.write_json(self.dashboard_path, dash)

        code, _, stderr = self.run_cli(
            "--dashboard", str(self.dashboard_path),
            "--receipts", str(self.receipts_path),
            "--file", str(self.readme),
            "--write",
        )
        self.assertEqual(code, 1)
        self.assertIn("commit: null", stderr)
        self.assertEqual(self.readme.read_text(encoding="utf-8"), original_readme)

    def test_refuses_mismatched_and_mixed_shas_even_with_allow_partial(self):
        original_readme = self.readme.read_text(encoding="utf-8")
        manifest = self.make_receipts_manifest(commit=COMMIT_STALE)
        manifest["receipts"]["conan"]["binary_sha256"] = BINARY_STALE
        self.write_json(self.receipts_path, manifest)

        code, _, stderr = self.run_cli(
            "--dashboard", str(self.dashboard_path),
            "--receipts", str(self.receipts_path),
            "--file", str(self.readme),
            "--allow-partial",
            "--write",
        )
        self.assertEqual(code, 1)
        self.assertIn("does not match receipts manifest", stderr)
        self.assertIn("binary_sha256", stderr)
        self.assertEqual(self.readme.read_text(encoding="utf-8"), original_readme)

    def test_refuses_missing_key_without_allow_partial_and_permits_with_flag(self):
        original_writeup = self.writeup.read_text(encoding="utf-8")
        dash = make_full_dashboard()
        del dash["knownParityGaps"]
        dash["tracks"]["runCmakeOs"]["platforms"][2].update(status="pending", suitesPassed=None, passRate=None)
        self.write_json(self.dashboard_path, dash)
        manifest = self.make_receipts_manifest()
        del manifest["known_parity_gaps"]
        self.write_json(self.receipts_path, manifest)

        # Strict mode refuses and writes nothing.
        code, _, stderr = self.run_cli(
            "--dashboard", str(self.dashboard_path),
            "--receipts", str(self.receipts_path),
            "--file", str(self.writeup),
            "--write",
        )
        self.assertEqual(code, 1)
        self.assertIn("runcmake_windows", stderr)
        self.assertIn("known_parity_gaps", stderr)
        self.assertEqual(self.writeup.read_text(encoding="utf-8"), original_writeup)

        # Staging --allow-partial stamps resolved markers and preserves unresolved ones.
        code, _, stderr = self.run_cli(
            "--dashboard", str(self.dashboard_path),
            "--receipts", str(self.receipts_path),
            "--file", str(self.writeup),
            "--allow-partial",
            "--write",
        )
        self.assertEqual(code, 0, stderr)
        updated = self.writeup.read_text(encoding="utf-8")
        self.assertIn(f"<!-- METRIC:release_commit -->`{COMMIT_OK[:12]}`<!-- /METRIC -->", updated)
        self.assertIn("<!-- METRIC:runcmake_windows -->pending frozen release run<!-- /METRIC -->", updated)
        self.assertIn("<!-- METRIC:known_parity_gaps -->pending frozen release run<!-- /METRIC -->", updated)

        code, _, stderr = self.run_cli(
            "--dashboard", str(self.dashboard_path),
            "--receipts", str(self.receipts_path),
            "--file", str(self.writeup),
            "--allow-partial",
            "--check",
        )
        self.assertEqual(code, 0, stderr)

    def test_check_detects_stale_drift_after_manual_edit(self):
        common = (
            "--dashboard", str(self.dashboard_path),
            "--receipts", str(self.receipts_path),
            "--file", str(self.readme),
        )
        self.assertEqual(self.run_cli(*common, "--write")[0], 0)
        drifted = self.readme.read_text(encoding="utf-8").replace(
            "<!-- METRIC:large_projects_l1 -->10 / 10 passed<!-- /METRIC -->",
            "<!-- METRIC:large_projects_l1 -->9 / 10 passed<!-- /METRIC -->",
        )
        self.readme.write_text(drifted, encoding="utf-8")

        code, _, stderr = self.run_cli(*common, "--check")
        self.assertEqual(code, 1)
        self.assertIn("[drift]", stderr)
        self.assertIn("large_projects_l1", stderr)

    def test_rejects_unknown_metric_key_and_malformed_marker(self):
        bad_key = self.root / "bad_key.md"
        bad_key.write_text("<!-- METRIC:nonexistent_metric -->x<!-- /METRIC -->\n", encoding="utf-8")
        code, _, stderr = self.run_cli(
            "--dashboard", str(self.dashboard_path),
            "--file", str(bad_key),
            "--check",
        )
        self.assertEqual(code, 2)
        self.assertIn("unknown METRIC key 'nonexistent_metric'", stderr)

        unclosed = self.root / "unclosed.md"
        unclosed.write_text("<!-- METRIC:release_commit -->broken\n", encoding="utf-8")
        code, _, stderr = self.run_cli(
            "--dashboard", str(self.dashboard_path),
            "--file", str(unclosed),
            "--check",
        )
        self.assertEqual(code, 2)
        self.assertIn("malformed METRIC markers", stderr)


if __name__ == "__main__":
    unittest.main()
