# SPDX-License-Identifier: MIT OR Apache-2.0
"""Unit tests for tools/measure/cli_ux_receipt.py (zero Cargo builds)."""

from __future__ import annotations

from contextlib import redirect_stderr
import hashlib
import io
import json
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest

from tools.measure import cli_ux_receipt
from tools.measure import verify_release_receipts as verifier


COMMIT = "0123456789abcdef0123456789abcdef01234567"


class CliUxReceiptTests(unittest.TestCase):
    def setUp(self) -> None:
        temp = TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.report = Path(temp.name) / "conformance-report"
        self.report.mkdir()
        self.out = self.report / "cli-ux.json"
        self.log = self.report / "cli-ux.log"
        self.polish = self.report / "cli-ux-polish.json"
        self.binary = Path(temp.name) / "rcmake"
        self.binary.write_bytes(b"fake rcmake\n")

    def write_polish(self, *passed: bool) -> None:
        checks = [{"id": f"check-{index}", "passed": value} for index, value in enumerate(passed)]
        self.polish.write_text(json.dumps({"checks": checks}), encoding="utf-8")

    def run_main(self, verify_exit: int) -> tuple[int, str]:
        err = io.StringIO()
        with redirect_stderr(err):
            code = cli_ux_receipt.main([
                "--out", str(self.out), "--verify-exit", str(verify_exit), "--verify-log", str(self.log),
                "--polish-report", str(self.polish), "--commit", COMMIT.upper(), "--binary", str(self.binary),
            ])
        return code, err.getvalue()

    def check(self, payload: dict) -> list[verifier.Finding]:
        self.out.write_text(json.dumps(payload), encoding="utf-8")
        record = verifier.load_receipt_entry("cli_ux", "track6-ux-packaging", {"path": str(self.out)}, self.report)
        findings: list[verifier.Finding] = []
        verifier.check_cli_ux(record, findings)
        return findings

    def test_passing_run_writes_a_stamped_receipt_that_the_verifier_accepts(self) -> None:
        self.write_polish(True, True)
        code, err = self.run_main(0)
        self.assertEqual(code, 0, err)
        payload = json.loads(self.out.read_text(encoding="utf-8"))
        self.assertEqual((payload["status"], payload["passed"], payload["total"], payload["failures"]),
                         ("pass", 3, 3, 0))
        self.assertEqual(payload["release_commit"], COMMIT)
        self.assertEqual(payload["binary_sha256"], hashlib.sha256(b"fake rcmake\n").hexdigest())
        self.assertEqual(payload["checks"][0], {"check": "verify.py", "status": "pass", "log": "cli-ux.log"})
        self.assertEqual(payload["polish_report"], "cli-ux-polish.json")
        self.assertEqual(self.check(payload), [])

    def test_failed_verify_or_polish_check_writes_a_failing_receipt(self) -> None:
        for verify_exit, polish in ((1, (True,)), (0, (True, False))):
            with self.subTest(verify_exit=verify_exit, polish=polish):
                self.write_polish(*polish)
                code, err = self.run_main(verify_exit)
                self.assertEqual(code, 0, err)
                payload = json.loads(self.out.read_text(encoding="utf-8"))
                self.assertEqual((payload["status"], payload["failures"]), ("fail", 1))
                self.assertNotEqual(self.check(payload), [])

    def test_missing_polish_report_writes_nothing(self) -> None:
        code, err = self.run_main(0)
        self.assertEqual(code, 2)
        self.assertIn("cannot read the verify_polish.py report", err)
        self.assertFalse(self.out.exists())


if __name__ == "__main__":
    unittest.main()
