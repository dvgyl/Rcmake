# SPDX-License-Identifier: MIT OR Apache-2.0
"""Unit tests for scripts/reproduce-scorecard.sh (zero Cargo builds)."""

from __future__ import annotations

from hashlib import sha256
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import sys
from tempfile import TemporaryDirectory
import unittest

from tools.measure import assemble_release_receipts as assembler


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "reproduce-scorecard.sh"
ASSEMBLER_SCRIPT = ROOT / "tools" / "measure" / "assemble_release_receipts.py"
COMMIT = "0123456789abcdef0123456789abcdef01234567"
RUNCMAKE_OS = {"Darwin": "macos", "Linux": "linux"}.get(platform.system(), "windows")

# Each lane, the tool that it runs, and the assembler lane key of its receipt.
LANES = {
    "binary-size": (None, "dist"),
    "bench-eval": ("tools/measure/bench_eval.py", "benchmarks_synthetic"),
    "bench-configure": ("tools/measure/bench_configure.py", "benchmarks_synthetic"),
    "bench-giants": ("tools/measure/bench_giants.py", "benchmarks_giants"),
    "probe-cache": ("tools/measure/probe_cache_eval.py", "probe_cache"),
    "mutation": ("tools/measure/mutation_eval.py", "mutation_replay"),
    "determinism": ("tools/measure/determinism.py", "determinism"),
    "wasm": ("tools/measure/wasm_smoke.py", "wasi"),
    "diff-fuzz": ("tools/measure/oracle_prop_test.py", "differential_fuzz"),
    "blackbox": ("tools/measure/independent_verify.py", "independent_scenarios"),
    "cli-ux": ("conformance/cases/cli-ux/verify_polish.py", "cli_ux"),
    "path-swap": ("tools/measure/path_swap_eval.py", "path_swap"),
    "ecosystem": ("tools/corpus/ecosystem_eval.py", "conan"),
    "runcmake": ("tools/measure/runcmake_eval.py", f"runcmake_{RUNCMAKE_OS}"),
}

# A stand-in interpreter: it answers for one lane tool and runs everything else with Python.
FAKE_PYTHON = """\
import json, os, sys
if len(sys.argv) > 1 and sys.argv[1].endswith("mutation_eval.py"):
    args = sys.argv[2:]
    for flag in ("--out-json", "--out-md"):
        path = args[args.index(flag) + 1]
        with open(path, "w", encoding="utf-8") as stream:
            stream.write(os.environ["FAKE_RECEIPT"])
    sys.exit(int(os.environ.get("FAKE_EXIT", "0")))
os.execv(sys.executable, [sys.executable, *sys.argv[1:]])
"""


def run_script(*args: str, env: dict[str, str] | None = None) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["bash", str(SCRIPT), *args],
        cwd=ROOT,
        capture_output=True,
        text=True,
        timeout=120,
        check=False,
        env=env,
    )


def lane_by_receipt_name() -> dict[str, str]:
    """Map every file name that the assembler accepts to its lane key."""
    names: dict[str, str] = {}
    for spec in assembler.LANES:
        for name in (*spec.candidates, *spec.companion_candidates):
            names[name] = spec.key
    return names


def dry_run_receipts(output: str) -> dict[str, Path]:
    """Return {lane: receipt path} from the "[lane] receipt: path" lines of a dry run."""
    return {
        match.group(1): Path(match.group(2))
        for match in re.finditer(r"^\[([a-z-]+)\] receipt: (.+)$", output, re.MULTILINE)
    }


class ReproduceScorecardTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = TemporaryDirectory(prefix="rcmake-scorecard-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.receipts = self.root / "receipts"
        self.binary = self.root / "stage" / "bin" / "rcmake"
        self.binary.parent.mkdir(parents=True)
        self.binary.write_bytes(b"fake rcmake release binary\n")
        self.binary.chmod(0o755)

    def dry_run_all(self) -> subprocess.CompletedProcess[str]:
        result = run_script(
            "--dry-run", "--lane", "all", "--rcmake", str(self.binary),
            "--receipts-dir", str(self.receipts), "--commit", COMMIT,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        return result

    def status(self) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(ASSEMBLER_SCRIPT), "--receipts-dir", str(self.receipts),
             "--commit", COMMIT, "--status"],
            cwd=ROOT, capture_output=True, text=True, timeout=60, check=False,
        )

    def fake_python_env(self, receipt: dict, exit_code: int = 0) -> dict[str, str]:
        fake = self.root / "fake-python"
        fake.write_text(f"#!{sys.executable}\n{FAKE_PYTHON}", encoding="utf-8")
        fake.chmod(0o755)
        return {**os.environ, "PYTHON": str(fake), "FAKE_RECEIPT": json.dumps(receipt),
                "FAKE_EXIT": str(exit_code)}

    def test_script_syntax_mode_and_help(self) -> None:
        syntax = subprocess.run(["bash", "-n", str(SCRIPT)], capture_output=True, text=True, check=False)
        self.assertEqual(syntax.returncode, 0, syntax.stderr)
        self.assertTrue(os.access(SCRIPT, os.X_OK))
        self.assertIn("SPDX-License-Identifier: MIT OR Apache-2.0", SCRIPT.read_text(encoding="utf-8"))

        result = run_script("--help")
        self.assertEqual(result.returncode, 0, result.stderr)
        for lane in (*LANES, "assemble"):
            self.assertIn(lane, result.stdout)
        for flag in ("--rcmake", "--lsp", "--receipts-dir", "--commit", "--lane", "--runs", "--dry-run"):
            self.assertIn(flag, result.stdout)

    def test_dry_run_all_prints_every_lane_command_and_runs_nothing(self) -> None:
        result = self.dry_run_all()
        commands = [line for line in result.stdout.splitlines() if "] $ " in line]
        for lane, (tool, _) in LANES.items():
            self.assertIn(f"\n== {lane}\n", result.stdout)
            if tool is not None:
                self.assertTrue(
                    any(line.startswith(f"[{lane}] $ ") and tool in line for line in commands),
                    f"no {tool} command for lane {lane}",
                )
        self.assertTrue(any("assemble_release_receipts.py" in line and "--status" in line for line in commands))
        self.assertIn("--runs 5", next(line for line in commands if "bench_configure.py" in line))
        self.assertIn("--runs 3", next(line for line in commands if "bench_giants.py" in line))
        self.assertFalse(self.receipts.exists(), "a dry run must not create the receipts directory")

    def test_runs_option_applies_to_every_benchmark(self) -> None:
        result = run_script("--dry-run", "--lane", "bench-configure", "--lane", "bench-giants", "--runs", "7",
                            "--rcmake", str(self.binary), "--receipts-dir", str(self.receipts), "--commit", COMMIT)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.count("--runs 7"), 2)
        self.assertNotIn("== bench-eval", result.stdout)

    def test_every_dry_run_receipt_maps_to_one_assembler_lane(self) -> None:
        receipts = dry_run_receipts(self.dry_run_all().stdout)
        self.assertEqual(set(receipts), set(LANES))
        accepted = lane_by_receipt_name()
        seen: dict[str, str] = {}
        for lane, path in receipts.items():
            self.assertEqual(path.parent, self.receipts, f"{lane} receipt is not in --receipts-dir")
            self.assertTrue(path.name in accepted, f"{path.name} is not a file name that the assembler reads")
            key = accepted[path.name]
            self.assertEqual(key, LANES[lane][1], f"{path.name} maps to {key}")
            spec = next(spec for spec in assembler.LANES if spec.key == key)
            if path.name in spec.candidates:
                self.assertNotIn(key, seen, f"{path.name} and {seen.get(key)} are aliases of lane {key}")
                seen[key] = path.name
        self.assertEqual(seen["benchmarks_synthetic"], "bench-configure.json")
        self.assertEqual(receipts["bench-eval"].name, "bench-eval.json")

    def test_unknown_lane_and_bad_commit_exit_2(self) -> None:
        self.assertEqual(run_script("--dry-run", "--lane", "nope").returncode, 2)
        self.assertEqual(run_script("--dry-run", "--commit", "xyz").returncode, 2)
        self.assertEqual(run_script("--bogus").returncode, 2)

    def test_binary_size_receipt_is_accepted_by_assembler_status(self) -> None:
        result = run_script("--lane", "binary-size", "--rcmake", str(self.binary),
                            "--receipts-dir", str(self.receipts), "--commit", COMMIT)
        self.assertEqual(result.returncode, 0, result.stderr)

        receipt_path = self.receipts / "binary-size.json"
        receipt = json.loads(receipt_path.read_text(encoding="utf-8"))
        digest = sha256(self.binary.read_bytes()).hexdigest()
        self.assertEqual(receipt["release_commit"], COMMIT)
        self.assertEqual(receipt["binary_sha256"], digest)
        self.assertEqual(receipt["binary_size_bytes"], self.binary.stat().st_size)
        self.assertEqual(receipt["binary_path"], str(self.binary))
        self.assertEqual(receipt_path.stat().st_mode & 0o777, 0o644)

        scan = assembler.scan_receipts_dir(self.receipts, COMMIT)
        dist = next(lane for lane in scan.lanes if lane.spec.key == "dist")
        self.assertEqual(dist.state, "present")
        self.assertEqual(dist.binaries, (digest,))
        self.assertEqual(scan.inferred_binary, digest)
        self.assertEqual(scan.offenders, ())

        status = self.status()
        self.assertEqual(status.returncode, 2, "18 other required lanes are still missing")
        self.assertRegex(status.stdout, r"\| `dist` \| `track6-ux-packaging` \| `present` \| `binary-size\.json` \|")
        self.assertIn("1 / 19 required lanes ready", status.stdout)

    def test_lane_receipt_gets_full_commit_and_binary_digest(self) -> None:
        env = self.fake_python_env({"passed": True, "replay_verified": True, "steps": [], "rcmake_sha": COMMIT[:7]})
        result = run_script("--lane", "mutation", "--rcmake", str(self.binary),
                            "--receipts-dir", str(self.receipts), "--commit", COMMIT, env=env)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

        receipt = json.loads((self.receipts / "mutation-replay.json").read_text(encoding="utf-8"))
        self.assertEqual(receipt["rcmake_sha"], COMMIT)
        self.assertEqual(receipt["release_commit"], COMMIT)
        self.assertEqual(receipt["binary_sha256"], sha256(self.binary.read_bytes()).hexdigest())
        self.assertRegex(self.status().stdout, r"\| `mutation_replay` \| `track4-soundness` \| `present` \|")

    def test_lane_fails_on_foreign_commit_or_tool_failure(self) -> None:
        foreign = self.fake_python_env({"passed": True, "steps": [], "release_commit": "f" * 40})
        result = run_script("--lane", "mutation", "--rcmake", str(self.binary),
                            "--receipts-dir", str(self.receipts), "--commit", COMMIT, env=foreign)
        self.assertEqual(result.returncode, 1)
        self.assertIn("expected " + COMMIT, result.stderr)
        self.assertIn("failed or are incomplete: mutation", result.stdout)

        failing = self.fake_python_env({"passed": False, "steps": []}, exit_code=1)
        result = run_script("--lane", "mutation", "--rcmake", str(self.binary),
                            "--receipts-dir", str(self.receipts), "--commit", COMMIT, env=failing)
        self.assertEqual(result.returncode, 1)
        receipt = json.loads((self.receipts / "mutation-replay.json").read_text(encoding="utf-8"))
        self.assertEqual(receipt["release_commit"], COMMIT, "a failed run still records its identity")


if __name__ == "__main__":
    unittest.main()
