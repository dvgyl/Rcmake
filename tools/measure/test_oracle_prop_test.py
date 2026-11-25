# SPDX-License-Identifier: MIT OR Apache-2.0
"""Unit tests for the CMake 4.5.1 differential property fuzzer."""

from __future__ import annotations

from contextlib import redirect_stderr, redirect_stdout
import io
import json
import os
from pathlib import Path
import random
import stat
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest import mock

try:
    import build_dashboard_json as builder
    import oracle_prop_test as opt
    import verify_release_receipts as verifier
except ImportError:
    from tools.measure import build_dashboard_json as builder
    from tools.measure import oracle_prop_test as opt
    from tools.measure import verify_release_receipts as verifier


TEST_COMMIT = "0123456789abcdef0123456789abcdef01234567"


def write_executable_script(path: Path, body: str) -> Path:
    """Create an executable Python script for CLI tests."""
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(f"#!{sys.executable}\n{body}", encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    return path


FAKE_CMAKE_SCRIPT = """
import pathlib
import sys

args = sys.argv[1:]
if args == ["--version"]:
    print("cmake version 4.5.1")
    sys.exit(0)

if len(args) >= 2 and args[0] == "-P":
    text = pathlib.Path(args[1]).read_text(encoding="utf-8")
    if "FAIL_ORACLE" in text and {is_oracle!r}:
        print("simulated oracle error", file=sys.stderr)
        sys.exit(1)
    if {fail_math!r} and "math(EXPR" in text:
        print("-- RESULT=[divergent]")
        sys.exit(0)
    if {fail_oracle!r}:
        print("oracle failed", file=sys.stderr)
        sys.exit(1)
    print("-- RESULT=[ok]")
    sys.exit(0)

if "-B" in args:
    build = pathlib.Path(args[args.index("-B") + 1])
    build.mkdir(parents=True, exist_ok=True)
    if {fail_oracle!r}:
        print("configure failed", file=sys.stderr)
        sys.exit(1)
    (build / "build.ninja").write_text("# fake\\n", encoding="utf-8")
    print("-- Configuring done (0.01s)")
    sys.exit(0)

print(f"unexpected args: {{args}}", file=sys.stderr)
sys.exit(2)
"""

FAKE_NINJA_SCRIPT = """
import os
import sys

assert os.environ.get("NINJA_STATUS") == "[%f/%t] "
print("[1/2] /usr/bin/cc -c main.c -o main.c.o")
print("[2/2] /usr/bin/cc main.c.o -o app")
"""


class OraclePropTestUnitTests(unittest.TestCase):
    def setUp(self):
        tmp = TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)

    def test_seeded_case_generation_is_deterministic_and_covers_all_families(self):
        first = [
            (idx, case_seed, case.family, case.configure, case.filename, case.program)
            for idx, case_seed, case in opt.generate_cases(seed=42, count=120)
        ]
        second = [
            (idx, case_seed, case.family, case.configure, case.filename, case.program)
            for idx, case_seed, case in opt.generate_cases(seed=42, count=120)
        ]
        other_seed = [
            (idx, case_seed, case.family, case.configure, case.filename, case.program)
            for idx, case_seed, case in opt.generate_cases(seed=99, count=120)
        ]
        self.assertEqual(first, second)
        self.assertNotEqual(first, other_seed)

        expected_scripts = set(opt.script_cases(random.Random(1)).keys())
        expected_configures = set(opt.configure_cases(random.Random(1)).keys())
        seen_scripts = {row[2] for row in first if not row[3]}
        seen_configures = {row[2] for row in first if row[3]}
        self.assertEqual(seen_scripts, expected_scripts)
        self.assertEqual(seen_configures, expected_configures)

        # Verify coverage across every required CMake language and generator family.
        required_families = {
            "if.auto_deref",
            "if.matches",
            "if.compare_in_list",
            "foreach.zip_lists",
            "foreach.range",
            "foreach.in_lists",
            "scope.block",
            "scope.function_macro",
            "list.sort",
            "list.filter",
            "list.transform",
            "list.sublist",
            "list.pop",
            "list.join",
            "list.insert_remove",
            "string.regex_matchall",
            "string.regex_replace",
            "string.replace",
            "string.substring",
            "string.json",
            "string.append",
            "string.compare",
            "string.configure",
            "string.genex_strip",
            "string.find_repeat",
            "math.expr",
            "cmake_path",
            "cmake_path.components",
            "configure_file.subst",
            "configure_file.copyonly",
            "policy.push_pop_block",
            "policy.cmp0140_return",
            "policy.cmp0174_parse_args",
            "policy.cmp0077_cmp0126",
            "genex.bool_logic",
            "genex.if_config",
            "genex.target_property",
            "genex.link_only",
            "genex.build_interface",
            "genex.list",
            "genex.string_version",
            "genex.path_filter",
            "usage.transitive",
            "usage.object_and_sources",
            "ninja.custom_command",
        }
        self.assertTrue(required_families.issubset(seen_scripts | seen_configures))

        for _, _, _, is_configure, filename, program in first:
            self.assertTrue(program.startswith(opt.MINIMUM + "\n"))
            if is_configure:
                self.assertEqual(filename, opt.CONFIGURE)
                self.assertIn("project(PropTest LANGUAGES C)", program)
            else:
                self.assertEqual(filename, opt.SCRIPT)
                self.assertIn(opt.OBSERVE, program)

        with self.assertRaises(ValueError):
            list(opt.generate_cases(seed=1, count=5, configure_interval=0))

    def test_normalize_replaces_workspace_and_sibling_cmake_paths(self):
        work = self.root / "work"
        work.mkdir()
        stage_bin = self.root / "stage" / "bin"
        stage_bin.mkdir(parents=True)
        rcmake_bin = write_executable_script(stage_bin / "rcmake", "")
        cmake_link = stage_bin / "cmake"
        cmake_link.symlink_to(rcmake_bin)

        raw = (
            f"{rcmake_bin} -P {work}/case.cmake   \n"
            f"{cmake_link} -E rm -f {work.resolve()}/libcore.a\n"
            f"{cmake_link.resolve()} -E touch {work}/libcore.a  \n"
        )
        normalized = opt.normalize(raw, work, rcmake_bin)
        self.assertEqual(
            normalized,
            "<cmake> -P <work>/case.cmake\n"
            "<cmake> -E rm -f <work>/libcore.a\n"
            "<cmake> -E touch <work>/libcore.a",
        )

    def test_minimizer_reduces_single_and_paired_faults_while_guarding_oracle(self):
        # Single failing unit among 8 units reduces to 1 unit.
        single_case = opt.Case("synthetic.single", tuple(f"u{i}" for i in range(8)))

        def compare_single(case: opt.Case):
            oracle = {"returncode": 0, "error": None, "stdout": "oracle", "stderr": ""}
            bad = "u5" in case.units
            rcmake = {"returncode": 0, "error": None, "stdout": "diff" if bad else "oracle", "stderr": ""}
            return oracle, rcmake

        o0, r0 = compare_single(single_case)
        reduced, left, right = opt.minimize(single_case, compare_single, o0, r0)
        self.assertEqual(reduced.units, ("u5",))
        self.assertEqual(opt.differences(left, right), ("stdout",))

        # Two interacting units among 7 units must both survive minimization.
        pair_case = opt.Case("synthetic.pair", tuple(f"s{i}" for i in range(7)))

        def compare_pair(case: opt.Case):
            oracle = {"returncode": 0, "error": None, "stdout": "same", "stderr": ""}
            has_pair = "s1" in case.units and "s4" in case.units
            rcmake = {"returncode": 0, "error": None, "stdout": "mismatch" if has_pair else "same", "stderr": ""}
            return oracle, rcmake

        o1, r1 = compare_pair(pair_case)
        reduced_pair, _, _ = opt.minimize(pair_case, compare_pair, o1, r1)
        self.assertEqual(reduced_pair.units, ("s1", "s4"))

        # Minimization must never drop a unit required for the oracle to stay green,
        # nor accept a candidate that changes the difference signature or rcmake exit code.
        guarded_case = opt.Case("synthetic.guard", ("setup", "noise1", "trigger", "noise2"))

        def compare_guarded(case: opt.Case):
            if "setup" not in case.units:
                oracle = {"returncode": 1, "error": None, "stdout": "", "stderr": "oracle err"}
                rcmake = {"returncode": 1, "error": None, "stdout": "", "stderr": "rcmake err"}
                return oracle, rcmake
            oracle = {"returncode": 0, "error": None, "stdout": "ok", "stderr": ""}
            if "trigger" in case.units:
                rcmake = {"returncode": 0, "error": None, "stdout": "bad", "stderr": ""}
            else:
                rcmake = {"returncode": 0, "error": None, "stdout": "ok", "stderr": ""}
            return oracle, rcmake

        o2, r2 = compare_guarded(guarded_case)
        reduced_guarded, _, _ = opt.minimize(guarded_case, compare_guarded, o2, r2)
        self.assertEqual(reduced_guarded.units, ("setup", "trigger"))

    def test_resolve_output_paths_and_identity_metadata(self):
        out_dir = self.root / "receipts"
        report, repro = opt.resolve_output_paths(None, out_dir)
        self.assertEqual(report, out_dir / "differential-fuzz.json")
        self.assertEqual(repro, (out_dir / "differential-fuzz.repros").absolute())

        rel_report, rel_repro = opt.resolve_output_paths(Path("custom.json"), out_dir)
        self.assertEqual(rel_report, out_dir / "custom.json")
        self.assertEqual(rel_repro, (out_dir / "custom.repros").absolute())

        abs_out = self.root / "standalone.json"
        abs_report, abs_repro = opt.resolve_output_paths(abs_out, out_dir)
        self.assertEqual(abs_report, abs_out)
        self.assertEqual(abs_repro, (out_dir / "repros").absolute())

        with self.assertRaises(ValueError):
            opt.resolve_output_paths(None, None)

        fake_bin = write_executable_script(self.root / "bin" / "rcmake", "print('ok')\n")
        digest = opt.sha256_file(fake_bin)

        with mock.patch.dict(os.environ, {}, clear=True):
            ident_bare = opt.resolve_identity(fake_bin)
            self.assertEqual(ident_bare, {"binary_sha256": digest})

            ident_explicit = opt.resolve_identity(fake_bin, TEST_COMMIT.upper(), digest.upper())
            self.assertEqual(
                ident_explicit,
                {
                    "binary_sha256": digest,
                    "rcmake_sha": TEST_COMMIT,
                    "release_commit": TEST_COMMIT,
                    "commit": TEST_COMMIT,
                },
            )

        with mock.patch.dict(os.environ, {"GITHUB_SHA": TEST_COMMIT}, clear=True):
            ident_env = opt.resolve_identity(fake_bin)
            self.assertEqual(ident_env["rcmake_sha"], TEST_COMMIT)
            self.assertEqual(ident_env["release_commit"], TEST_COMMIT)
            self.assertEqual(ident_env["commit"], TEST_COMMIT)

        with self.assertRaises(ValueError):
            opt.resolve_identity(fake_bin, "not-a-sha")
        with self.assertRaises(ValueError):
            opt.resolve_identity(fake_bin, TEST_COMMIT, "short")
        with self.assertRaises(ValueError):
            opt.resolve_identity(fake_bin, TEST_COMMIT, "f" * 64)

    def _make_fake_env(self, *, fail_math: bool = False, fail_oracle: bool = False):
        bin_dir = self.root / "fake-tools"
        oracle_bin = write_executable_script(
            bin_dir / "oracle" / "cmake",
            FAKE_CMAKE_SCRIPT.format(is_oracle=True, fail_math=False, fail_oracle=fail_oracle),
        )
        rcmake_bin = write_executable_script(
            bin_dir / "stage" / "rcmake",
            FAKE_CMAKE_SCRIPT.format(is_oracle=False, fail_math=fail_math, fail_oracle=False),
        )
        ninja_dir = bin_dir / "ninja-bin"
        write_executable_script(ninja_dir / "ninja", FAKE_NINJA_SCRIPT)
        env_patch = {
            "RCMAKE_ORACLE_CMAKE": str(oracle_bin),
            "PATH": f"{ninja_dir}{os.pathsep}{os.environ.get('PATH', '')}",
        }
        return oracle_bin, rcmake_bin, env_patch

    def test_emitted_report_integrates_with_dashboard_schema_and_release_receipts(self):
        _, rcmake_bin, env_patch = self._make_fake_env()
        binary_digest = opt.sha256_file(rcmake_bin)
        out_dir = self.root / "receipts"
        work_dir = self.root / "work"

        stdout, stderr = io.StringIO(), io.StringIO()
        with mock.patch.dict(os.environ, env_patch, clear=False):
            with redirect_stdout(stdout), redirect_stderr(stderr):
                code = opt.main([
                    "--rcmake-bin", str(rcmake_bin),
                    "--cases", "20",
                    "--seed", "7",
                    "--out-dir", str(out_dir),
                    "--work-dir", str(work_dir),
                    "--rcmake-sha", TEST_COMMIT,
                    "--binary-sha256", binary_digest,
                    "--configure-interval", "5",
                ])
        self.assertEqual(code, 0, stderr.getvalue())

        report_path = out_dir / "differential-fuzz.json"
        self.assertTrue(report_path.is_file())
        report = json.loads(report_path.read_text(encoding="utf-8"))
        self.assertEqual(report["rcmake_sha"], TEST_COMMIT)
        self.assertEqual(report["release_commit"], TEST_COMMIT)
        self.assertEqual(report["commit"], TEST_COMMIT)
        self.assertEqual(report["binary_sha256"], binary_digest)
        self.assertEqual(report["oracle_version"], opt.ORACLE_VERSION)
        self.assertEqual(report["seed"], 7)
        self.assertEqual(report["total"], 20)
        self.assertEqual(report["counts"], {"pass": 20, "fail": 0, "oracle_error": 0})
        self.assertEqual(len(report["cases"]), 20)
        self.assertEqual({row["mode"] for row in report["cases"]}, {"script", "configure"})

        # End-to-end ingestion into build_dashboard_json.py + dashboard.schema.json validation.
        dash_out = self.root / "latest.json"
        dash_code = builder.main([
            "--out", str(dash_out),
            "--release-sha", TEST_COMMIT,
            "--binary-sha256", binary_digest,
            "--strict-same-sha",
            "--differential-fuzz", str(out_dir),
        ])
        self.assertEqual(dash_code, 0)
        snapshot = builder.read_json(dash_out)
        schema = builder.read_json(builder.DATA / "dashboard.schema.json")
        builder.validate(snapshot, schema, schema)

        diff = snapshot["tracks"]["benchmarksAndGates"]["differentialFuzz"]
        self.assertEqual(diff["status"], "pass")
        self.assertEqual(diff["casesTotal"], 20)
        self.assertEqual(diff["casesPassed"], 20)
        self.assertEqual(diff["mismatches"], 0)
        self.assertEqual(diff["oracleErrors"], 0)
        self.assertEqual(diff["passRate"], 100.0)
        self.assertEqual(diff["seeds"], [7])
        self.assertEqual(diff["scriptCases"], 16)
        self.assertEqual(diff["configureCases"], 4)
        self.assertGreater(diff["families"], 1)

        # End-to-end auto-discovery and validation in verify_release_receipts.py.
        discovered = verifier.discover_receipts_in_dir(out_dir)
        self.assertEqual(discovered.get("differential_fuzz"), report_path)
        record = verifier.load_receipt_entry("differential_fuzz", "track4-soundness", report_path, out_dir)
        findings: list[verifier.Finding] = []
        verifier.check_identity(record, TEST_COMMIT, binary_digest, findings)
        verifier.check_differential_fuzz(record, findings)
        self.assertEqual(findings, [])

    def test_mismatch_and_oracle_error_reports_and_json_flag_alias(self):
        # Mismatch on math.expr exits 1, saves minimized repro, and marks dashboard/verifier fail.
        _, rcmake_bin, env_patch = self._make_fake_env(fail_math=True)
        binary_digest = opt.sha256_file(rcmake_bin)
        report_path = self.root / "mismatch-report.json"

        with mock.patch.dict(os.environ, env_patch, clear=False):
            with redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()):
                code = opt.main([
                    "--rcmake-bin", str(rcmake_bin),
                    "--cases", "50",
                    "--seed", "11",
                    "--json", str(report_path),
                    "--work-dir", str(self.root / "work-mismatch"),
                    "--rcmake-sha", TEST_COMMIT,
                ])
        self.assertEqual(code, 1)
        report = json.loads(report_path.read_text(encoding="utf-8"))
        self.assertGreaterEqual(report["counts"]["fail"], 1)
        self.assertEqual(report["counts"]["oracle_error"], 0)
        failed_rows = [row for row in report["cases"] if row["status"] == "fail"]
        self.assertTrue(all(row["family"] == "math.expr" for row in failed_rows))
        repro_dir = Path(failed_rows[0]["repro"])
        self.assertTrue((repro_dir / "case.cmake").is_file())
        self.assertTrue((repro_dir / "outcomes.json").is_file())

        dash_out = self.root / "latest-mismatch.json"
        self.assertEqual(
            builder.main([
                "--out", str(dash_out),
                "--release-sha", TEST_COMMIT,
                "--binary-sha256", binary_digest,
                "--strict-same-sha",
                "--differential-fuzz", str(report_path),
            ]),
            0,
        )
        snapshot = builder.read_json(dash_out)
        schema = builder.read_json(builder.DATA / "dashboard.schema.json")
        builder.validate(snapshot, schema, schema)
        diff = snapshot["tracks"]["benchmarksAndGates"]["differentialFuzz"]
        self.assertEqual(diff["status"], "fail")
        self.assertEqual(diff["mismatches"], report["counts"]["fail"])
        self.assertEqual(diff["oracleErrors"], 0)

        record = verifier.load_receipt_entry("differential_fuzz", "track4-soundness", report_path, self.root)
        findings: list[verifier.Finding] = []
        verifier.check_identity(record, TEST_COMMIT, binary_digest, findings)
        verifier.check_differential_fuzz(record, findings)
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].kind, "failed")

        # Oracle error exits 2 and marks dashboard fail with oracleErrors > 0.
        _, rcmake_ok, oracle_err_env = self._make_fake_env(fail_oracle=True)
        oracle_report = self.root / "oracle-err.json"
        with mock.patch.dict(os.environ, oracle_err_env, clear=False):
            with redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()):
                err_code = opt.main([
                    "--rcmake-bin", str(rcmake_ok),
                    "--cases", "3",
                    "--seed", "5",
                    "--json", str(oracle_report),
                    "--work-dir", str(self.root / "work-oracle-err"),
                ])
        self.assertEqual(err_code, 2)
        err_payload = json.loads(oracle_report.read_text(encoding="utf-8"))
        self.assertEqual(err_payload["counts"]["oracle_error"], 3)

        # Mismatched --binary-sha256 exits 2 cleanly without writing output.
        bad_digest_out = self.root / "never-written.json"
        with mock.patch.dict(os.environ, env_patch, clear=False):
            with redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()):
                bad_code = opt.main([
                    "--rcmake-bin", str(rcmake_bin),
                    "--cases", "2",
                    "--json", str(bad_digest_out),
                    "--binary-sha256", "0" * 64,
                ])
        self.assertEqual(bad_code, 2)
        self.assertFalse(bad_digest_out.exists())


if __name__ == "__main__":
    unittest.main()
