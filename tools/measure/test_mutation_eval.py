# SPDX-License-Identifier: MIT OR Apache-2.0
"""Unit tests for tools/measure/mutation_eval.py and replay_traps.toml."""

from __future__ import annotations

from contextlib import redirect_stderr, redirect_stdout
import hashlib
import io
import json
import os
from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest import mock

_MEASURE_DIR = Path(__file__).resolve().parent
if str(_MEASURE_DIR) not in sys.path:
    sys.path.insert(0, str(_MEASURE_DIR))

import mutation_eval as me


EXPECTED_TRAP_PAIRS = [
    ("T01", "func-body"),
    ("T02", "func-redefine"),
    ("T03", "func-env"),
    ("T04", "macro-target-prop"),
    ("T05", "parent-scope"),
    ("T06", "block-propagate"),
    ("T07", "return-propagate"),
    ("T08", "configure-depends-glob"),
    ("T09", "glob-plain"),
    ("T10", "cache-arg"),
    ("T11", "policy-at-call"),
    ("T12", "macro-double-expansion"),
    ("T13", "include-guard-global"),
    ("T14", "message-bytes"),
    ("T15", "nested-opaque"),
    ("T16", "file-read"),
    ("T17", "list-dir"),
    ("T18", "watch"),
    ("T19", "defines-command"),
    ("T20", "cmake-match"),
    ("T21", "math-error"),
    ("T22", "enumerate"),
    ("T23", "dir-prop-inherit"),
    ("T24", "parse-argv-empty"),
    ("T25", "defer"),
]


def _fake_executable(path: Path, body: str = "#!/bin/sh\nexit 0\n") -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body, encoding="utf-8")
    path.chmod(0o755)
    return path


def _write_valid_replay_record(source: Path, build: Path) -> None:
    """Write a minimal valid configure.v1.bin frame accepted by require_replay_record."""
    paths = [
        list(str(p).encode("utf-16-le", errors="surrogatepass") if os.name == "nt" else os.fsencode(p))
        for p in (source, build)
    ]
    payload = json.dumps({
        "schema": "configure.v1",
        "region": {
            "key": {"kind": "Configure", "source_dir": paths[0], "binary_dir": paths[1]},
            "payload": {"Complete": {"inputs": [], "effects": []}},
        },
        "model": {"nested": []},
    }).encode("utf-8")
    header = me.REPLAY_MAGIC + bytes([1 if os.name == "nt" else 2]) + len(payload).to_bytes(4, "little")
    frame = header + payload
    frame += hashlib.sha256(frame).digest()
    record = build / me.REPLAY_RECORD
    record.parent.mkdir(parents=True, exist_ok=True)
    record.write_bytes(frame)


def _write_fake_configure_outputs(
    source: Path,
    build: Path,
    *,
    build_ninja: str = "# ninja\nbuild all: phony\n",
    cache_text: str = "CMAKE_PROJECT_NAME:STATIC=Trap\n",
    verify_globs: str | None = None,
) -> None:
    """Populate minimal Ninja and File API artifacts for mocked configure runs."""
    build.mkdir(parents=True, exist_ok=True)
    (build / "CMakeCache.txt").write_text(cache_text, encoding="utf-8")
    (build / "build.ninja").write_text(build_ninja, encoding="utf-8")
    (build / "compile_commands.json").write_text("[]\n", encoding="utf-8")
    rules = build / "CMakeFiles/rules.ninja"
    rules.parent.mkdir(parents=True, exist_ok=True)
    rules.write_text("rule CC\n  command = cc\n", encoding="utf-8")
    if verify_globs is not None:
        (build / me.VERIFY_GLOBS).write_text(verify_globs, encoding="utf-8")

    reply = build / me.API / "reply"
    reply.mkdir(parents=True, exist_ok=True)
    index_reply = {}
    for query in me.QUERIES:
        filename = f"{query}-{'a' * 20}.json"
        index_reply[query] = {"jsonFile": filename}
        body: dict = {"kind": query, "version": {"major": 1, "minor": 0}}
        if query == "codemodel-v2":
            body["configurations"] = [{"directories": [{"source": "."}], "targets": []}]
        (reply / filename).write_text(json.dumps(body), encoding="utf-8")
    (reply / "index-0001.json").write_text(json.dumps({"reply": index_reply}), encoding="utf-8")


def _write_replay_result(
    source: Path,
    build: Path,
    *,
    executed: list[str],
    replayed: list[str],
    executed_calls: list[str] | None = None,
    replayed_calls: list[str] | None = None,
) -> None:
    payload: dict = {
        "version": 1,
        "executed": [str(source / rel) for rel in executed],
        "replayed": [str(source / rel) for rel in replayed],
    }
    if executed_calls is not None:
        payload["executed_calls"] = executed_calls
    if replayed_calls is not None:
        payload["replayed_calls"] = replayed_calls
    target = build / me.REPLAY_RESULT
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(json.dumps(payload), encoding="utf-8")


class ReplayTrapsManifestTests(unittest.TestCase):
    def test_tracked_replay_traps_toml_defines_all_25_traps_in_order(self):
        manifest = _MEASURE_DIR / "replay_traps.toml"
        traps = me.load_traps(manifest)
        self.assertEqual([(t.id, t.name) for t in traps], EXPECTED_TRAP_PAIRS)
        self.assertEqual(len(traps), 25)

        valid_stages = {
            "current",
            "HA.1", "HA.2", "HA.3", "HA.4", "HA.5", "HA.6",
            "HB.1", "HB.2", "HB.3",
        }
        active_ids = []
        pending_ids = []
        for trap in traps:
            self.assertIn(trap.stage, valid_stages, trap.id)
            self.assertIn("CMakeLists.txt", trap.files, trap.id)
            self.assertIn("control/CMakeLists.txt", trap.files, trap.id)
            self.assertTrue(trap.description.strip(), trap.id)
            if trap.expect_exit_code == 0:
                self.assertIn("control/CMakeLists.txt", trap.expect_replayed, trap.id)
            if trap.skip_reason:
                pending_ids.append(trap.id)
                self.assertTrue(trap.stage.startswith("HB."), trap.id)
            else:
                active_ids.append(trap.id)
                self.assertFalse(trap.stage.startswith("HB."), trap.id)

        self.assertEqual(active_ids, ["T06", "T07", "T08", "T09", "T13", "T18", "T20", "T23"])
        self.assertEqual(len(pending_ids), 17)

    def test_select_traps_supports_ids_names_commas_and_rejects_unknown(self):
        traps = me.load_traps(_MEASURE_DIR / "replay_traps.toml")
        picked = me.select_traps(traps, ["t01,watch", "T01", "glob-plain"])
        self.assertEqual([t.id for t in picked], ["T01", "T18", "T09"])

        with self.assertRaisesRegex(ValueError, "Unknown trap.*T99"):
            me.select_traps(traps, ["T99"])
        with self.assertRaisesRegex(ValueError, "at least one trap"):
            me.select_traps(traps, [" , "])

    def test_load_traps_rejects_vacuous_or_malformed_manifests(self):
        with TemporaryDirectory() as tmp:
            root = Path(tmp)

            vacuous = root / "vacuous.toml"
            vacuous.write_text(
                'schema_version = 1\n'
                '[[trap]]\n'
                'id = "T01"\n'
                'name = "vacuous"\n'
                'stage = "current"\n'
                'expect_executed = ["leaf/CMakeLists.txt"]\n'
                'expect_replayed = []\n'
                '[trap.files]\n'
                '"CMakeLists.txt" = "cmake_minimum_required(VERSION 4.5)\\n"\n',
                encoding="utf-8",
            )
            with self.assertRaisesRegex(ValueError, "non-vacuous"):
                me.load_traps(vacuous)

            escape = root / "escape.toml"
            escape.write_text(
                'schema_version = 1\n'
                '[[trap]]\n'
                'id = "T01"\n'
                'name = "escape"\n'
                'stage = "current"\n'
                'expect_replayed = ["control/CMakeLists.txt"]\n'
                '[trap.files]\n'
                '"CMakeLists.txt" = "cmake_minimum_required(VERSION 4.5)\\n"\n'
                '"../outside.txt" = "bad"\n',
                encoding="utf-8",
            )
            with self.assertRaisesRegex(ValueError, "stay inside its root"):
                me.load_traps(escape)

            bad_ver = root / "bad_ver.toml"
            bad_ver.write_text("schema_version = 99\n", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "Unsupported replay trap schema_version"):
                me.load_traps(bad_ver)


class TrapMutationAndReceiptTests(unittest.TestCase):
    def setUp(self):
        tmp = TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name).resolve()

    def test_apply_trap_mutation_handles_write_replace_and_remove(self):
        source = self.root / "src"
        me.write_trap_files(source, {
            "CMakeLists.txt": "set(VAL before)\n",
            "drop.cmake": "set(DROP 1)\n",
        })
        mutation = me.TrapMutation(
            write={"leaf/new.c": "int f(void) { return 1; }\n"},
            replace=(me.FileReplace("CMakeLists.txt", "before", "after"),),
            remove=("drop.cmake",),
        )
        diff = me.apply_trap_mutation(source, mutation)
        self.assertEqual((source / "CMakeLists.txt").read_text(encoding="utf-8"), "set(VAL after)\n")
        self.assertEqual((source / "leaf/new.c").read_text(encoding="utf-8"), "int f(void) { return 1; }\n")
        self.assertFalse((source / "drop.cmake").exists())
        self.assertIn(b"-set(VAL before)", diff)
        self.assertIn(b"+set(VAL after)", diff)
        self.assertIn(b"drop.cmake", diff)

        with self.assertRaisesRegex(ValueError, "pattern not found"):
            me.apply_trap_mutation(
                source,
                me.TrapMutation(replace=(me.FileReplace("CMakeLists.txt", "missing", "x"),)),
            )
        with self.assertRaisesRegex(ValueError, "removal target file is missing"):
            me.apply_trap_mutation(source, me.TrapMutation(remove=("drop.cmake",)))

    def test_verify_trap_receipt_enforces_listfiles_calls_opacity_and_non_vacuity(self):
        source = self.root / "source"
        build = self.root / "build"
        source.mkdir()
        build.mkdir()
        _write_valid_replay_record(source, build)
        _write_replay_result(
            source,
            build,
            executed=["leaf/CMakeLists.txt"],
            replayed=["control/CMakeLists.txt"],
            executed_calls=["Helper"],
            replayed_calls=["stable_call"],
        )
        trap = me.ReplayTrap(
            id="T01",
            name="func-body",
            stage="HB.1",
            description="call test",
            files={"CMakeLists.txt": ""},
            mutate=me.TrapMutation(),
            expect_executed=("leaf/CMakeLists.txt",),
            expect_replayed=("control/CMakeLists.txt",),
            expect_not_replayed=("leaf/CMakeLists.txt",),
            expect_executed_calls=("helper",),
            expect_replayed_calls=("STABLE_CALL",),
            expect_not_replayed_calls=("helper",),
        )
        off_receipt = me.ReplayReceipt(
            executed_listfiles=frozenset({"leaf/CMakeLists.txt", "control/CMakeLists.txt"}),
            replayed_listfiles=frozenset(),
        )
        row: dict = {}
        me.verify_trap_receipt(trap, source, build, off_receipt, row)
        self.assertTrue(row["replay_record"])
        self.assertEqual(row["replayed_listfiles"], ["control/CMakeLists.txt"])
        self.assertEqual(row["executed_calls"], ["Helper"])

        # Missing call fields must fail when trap expects executed_calls.
        _write_replay_result(
            source,
            build,
            executed=["leaf/CMakeLists.txt"],
            replayed=["control/CMakeLists.txt"],
        )
        with self.assertRaisesRegex(ValueError, "missing 'executed_calls'/'replayed_calls'"):
            me.verify_trap_receipt(trap, source, build, off_receipt, {})

        # Vacuous replay (control not replayed) must fail.
        _write_replay_result(
            source,
            build,
            executed=["leaf/CMakeLists.txt", "control/CMakeLists.txt"],
            replayed=[],
            executed_calls=["helper"],
            replayed_calls=[],
        )
        with self.assertRaisesRegex(ValueError, "expected control listfile"):
            me.verify_trap_receipt(trap, source, build, off_receipt, {})

        # Replaying an opaque call must fail.
        _write_replay_result(
            source,
            build,
            executed=["leaf/CMakeLists.txt"],
            replayed=["control/CMakeLists.txt"],
            executed_calls=["helper"],
            replayed_calls=["stable_call", "helper"],
        )
        with self.assertRaisesRegex(ValueError, "opaque call 'helper' must not replay"):
            me.verify_trap_receipt(trap, source, build, off_receipt, {})


class TrapsRunnerAndCliTests(unittest.TestCase):
    def setUp(self):
        tmp = TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name).resolve()

    def run_cli(self, argv: list[str]) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            try:
                code = me.main(argv)
            except SystemExit as exc:
                code = exc.code if isinstance(exc.code, int) else 1
        return code, out.getvalue(), err.getvalue()

    def test_traps_mode_skips_pending_by_default_and_runs_env_and_define_mutations(self):
        rcmake = _fake_executable(self.root / "bin/rcmake")
        manifest = self.root / "traps.toml"
        manifest.write_text(
            'schema_version = 1\n'
            '[[trap]]\n'
            'id = "T01"\n'
            'name = "active-env-define"\n'
            'stage = "current"\n'
            'expect_executed = ["leaf/CMakeLists.txt"]\n'
            'expect_replayed = ["control/CMakeLists.txt"]\n'
            'expect_stdout_contains = ["-- VAL=after:2"]\n'
            '[trap.initial_env]\n'
            'RCMAKE_TRAP = "before"\n'
            '[trap.initial_defines]\n'
            'NUM = "1"\n'
            '[trap.files]\n'
            '"CMakeLists.txt" = "cmake_minimum_required(VERSION 4.5)\\n"\n'
            '"control/CMakeLists.txt" = "add_library(control STATIC control.c)\\n"\n'
            '"leaf/CMakeLists.txt" = "add_library(leaf STATIC leaf.c)\\n"\n'
            '[trap.mutate.env]\n'
            'RCMAKE_TRAP = "after"\n'
            '[trap.mutate.defines]\n'
            'NUM = "2"\n'
            '[trap.expect_build_contains]\n'
            '"build.ninja" = ["VAL=after:2"]\n'
            '\n'
            '[[trap]]\n'
            'id = "T02"\n'
            'name = "pending-call"\n'
            'stage = "HB.1"\n'
            'skip_reason = "Pending HB.1"\n'
            'expect_executed = ["leaf/CMakeLists.txt"]\n'
            'expect_replayed = ["control/CMakeLists.txt"]\n'
            'expect_executed_calls = ["helper"]\n'
            '[trap.files]\n'
            '"CMakeLists.txt" = "cmake_minimum_required(VERSION 4.5)\\n"\n'
            '"control/CMakeLists.txt" = "add_library(control STATIC control.c)\\n"\n'
            '"leaf/CMakeLists.txt" = "add_library(leaf STATIC leaf.c)\\n"\n'
            '[trap.mutate]\n',
            encoding="utf-8",
        )

        observed_invocations: list[tuple[str, str | None, list[str]]] = []

        def fake_run_command(cmd, _cwd, _timeout, env):
            source = Path(cmd[cmd.index("-S") + 1])
            build = Path(cmd[cmd.index("-B") + 1])
            lane = build.parent.name
            extra_defines = [str(arg) for arg in cmd if str(arg).startswith("-DNUM=")]
            observed_invocations.append((lane, env.get("RCMAKE_TRAP"), extra_defines))
            num = extra_defines[0].split("=", 1)[1] if extra_defines else "0"
            trap_val = env.get("RCMAKE_TRAP", "unset")
            already_configured = (build / "CMakeCache.txt").is_file()
            _write_fake_configure_outputs(
                source,
                build,
                build_ninja=f"# VAL={trap_val}:{num}\n",
                cache_text=f"NUM:STRING={num}\n",
            )
            if env.get("RCMAKE_NO_INCREMENTAL") != "1":
                _write_valid_replay_record(source, build)
            replayed = ["control/CMakeLists.txt"] if (already_configured and env.get("RCMAKE_NO_INCREMENTAL") != "1") else []
            executed = ["leaf/CMakeLists.txt"] if replayed else ["leaf/CMakeLists.txt", "control/CMakeLists.txt"]
            _write_replay_result(source, build, executed=executed, replayed=replayed)
            return {
                "returncode": 0,
                "stdout": f"-- VAL={trap_val}:{num}\n-- Configuring done (0.1s)\n",
                "stderr": "",
                "error": None,
            }

        out_json = self.root / "traps.json"
        out_md = self.root / "traps.md"
        with mock.patch.object(me, "run_command", side_effect=fake_run_command):
            code, _, stderr = self.run_cli([
                "--traps", str(manifest),
                "--rcmake", str(rcmake),
                "--out-json", str(out_json),
                "--out-md", str(out_md),
            ])
        self.assertEqual(code, 0, stderr)
        payload = json.loads(out_json.read_text(encoding="utf-8"))
        self.assertTrue(payload["passed"])
        self.assertTrue(payload["replay_verified"])
        self.assertEqual(payload["mode"], "traps")
        self.assertEqual(payload["summary"], {"total": 2, "passed": 1, "failed": 0, "skipped": 1})
        self.assertEqual(payload["traps"][0]["status"], "passed")
        self.assertEqual(payload["traps"][1]["status"], "skipped")
        self.assertEqual(payload["traps"][1]["skip_reason"], "Pending HB.1")
        # Confirm both run 0 (before, 1) and run 1 (after, 2) ran across all 3 lanes.
        self.assertEqual(len(observed_invocations), 6)
        self.assertEqual(observed_invocations[0], ("incremental", "before", ["-DNUM=1"]))
        self.assertEqual(observed_invocations[3], ("incremental", "after", ["-DNUM=2"]))
        md = out_md.read_text(encoding="utf-8")
        self.assertIn("# Replay soundness traps", md)
        self.assertIn("| T01 | active-env-define | current | Passed |", md)
        self.assertIn("| T02 | pending-call | HB.1 | Skipped | Pending HB.1 |", md)

        # Explicitly requesting --trap T02 forces pending execution and fails when call receipts are absent.
        pending_json = self.root / "pending.json"
        pending_md = self.root / "pending.md"
        with mock.patch.object(me, "run_command", side_effect=fake_run_command):
            code, _, _ = self.run_cli([
                "--traps", str(manifest),
                "--trap", "T02",
                "--rcmake", str(rcmake),
                "--out-json", str(pending_json),
                "--out-md", str(pending_md),
            ])
        self.assertEqual(code, 1)
        pending_payload = json.loads(pending_json.read_text(encoding="utf-8"))
        self.assertFalse(pending_payload["passed"])
        self.assertEqual(pending_payload["summary"], {"total": 1, "passed": 0, "failed": 1, "skipped": 0})
        self.assertIn("executed_calls", pending_payload["traps"][0]["error"])
        self.assertTrue(Path(pending_payload["traps"][0]["repro_diff"]).is_file())

    def test_traps_mode_catches_three_lane_divergence_and_writes_repro_diff(self):
        rcmake = _fake_executable(self.root / "bin/rcmake")
        manifest = self.root / "diverge.toml"
        manifest.write_text(
            'schema_version = 1\n'
            '[[trap]]\n'
            'id = "T06"\n'
            'name = "block-propagate"\n'
            'stage = "HA.1"\n'
            'expect_executed = ["leaf/CMakeLists.txt"]\n'
            'expect_replayed = ["control/CMakeLists.txt"]\n'
            '[trap.files]\n'
            '"CMakeLists.txt" = "cmake_minimum_required(VERSION 4.5)\\n"\n'
            '"control/CMakeLists.txt" = "add_library(control STATIC control.c)\\n"\n'
            '"leaf/CMakeLists.txt" = "set(X before)\\n"\n'
            '[[trap.mutate.replace]]\n'
            'path = "leaf/CMakeLists.txt"\n'
            'old = "before"\n'
            'new = "after"\n',
            encoding="utf-8",
        )

        def fake_divergent_run(cmd, _cwd, _timeout, env):
            source = Path(cmd[cmd.index("-S") + 1])
            build = Path(cmd[cmd.index("-B") + 1])
            already = (build / "CMakeCache.txt").is_file()
            text = (source / "leaf/CMakeLists.txt").read_text(encoding="utf-8")
            # Simulate a soundness bug: incremental lane emits stale "before" after mutation.
            is_inc = env.get("RCMAKE_NO_INCREMENTAL") != "1"
            rendered = "before" if (is_inc and already) else ("after" if "after" in text else "before")
            _write_fake_configure_outputs(source, build, build_ninja=f"# X={rendered}\n")
            if is_inc:
                _write_valid_replay_record(source, build)
            _write_replay_result(
                source,
                build,
                executed=["leaf/CMakeLists.txt"],
                replayed=["control/CMakeLists.txt"] if (is_inc and already) else [],
            )
            return {"returncode": 0, "stdout": f"-- X={rendered}\n", "stderr": "", "error": None}

        out_json = self.root / "diverge.json"
        out_md = self.root / "diverge.md"
        with mock.patch.object(me, "run_command", side_effect=fake_divergent_run):
            code, _, _ = self.run_cli([
                "--traps", str(manifest),
                "--rcmake", str(rcmake),
                "--out-json", str(out_json),
                "--out-md", str(out_md),
            ])
        self.assertEqual(code, 1)
        payload = json.loads(out_json.read_text(encoding="utf-8"))
        self.assertFalse(payload["passed"])
        self.assertEqual(payload["traps"][0]["status"], "failed")
        self.assertIn("incremental/build.ninja", payload["traps"][0]["error"])
        repro = Path(payload["traps"][0]["repro_diff"])
        self.assertTrue(repro.is_file())
        self.assertIn("-# X=before", repro.read_text(encoding="utf-8"))

    def test_traps_mode_handles_expected_nonzero_exit_code(self):
        rcmake = _fake_executable(self.root / "bin/rcmake")
        manifest = self.root / "nonzero.toml"
        manifest.write_text(
            'schema_version = 1\n'
            '[[trap]]\n'
            'id = "T21"\n'
            'name = "math-error"\n'
            'stage = "HB.3"\n'
            'expect_exit_code = 1\n'
            'expect_not_replayed_calls = ["bad_math"]\n'
            'expect_stderr_contains = ["divide by zero"]\n'
            '[trap.files]\n'
            '"CMakeLists.txt" = "cmake_minimum_required(VERSION 4.5)\\n"\n'
            '[trap.mutate]\n',
            encoding="utf-8",
        )

        def fake_error_run(cmd, _cwd, _timeout, _env):
            build = Path(cmd[cmd.index("-B") + 1])
            build.mkdir(parents=True, exist_ok=True)
            (build / "CMakeCache.txt").write_text("CMAKE_PROJECT_NAME:STATIC=Trap\n", encoding="utf-8")
            return {
                "returncode": 1,
                "stdout": "-- Configuring incomplete, errors occurred!\n",
                "stderr": 'CMake Error: math cannot evaluate the expression: "1 / 0": divide by zero.\n',
                "error": None,
            }

        out_json = self.root / "nonzero.json"
        out_md = self.root / "nonzero.md"
        with mock.patch.object(me, "run_command", side_effect=fake_error_run):
            code, _, stderr = self.run_cli([
                "--traps", str(manifest),
                "--rcmake", str(rcmake),
                "--out-json", str(out_json),
                "--out-md", str(out_md),
            ])
        self.assertEqual(code, 0, stderr)
        payload = json.loads(out_json.read_text(encoding="utf-8"))
        self.assertTrue(payload["passed"])
        self.assertEqual(payload["traps"][0]["exit_status"], [1, 1, 1])

    def test_cli_rejects_trap_flags_without_traps_and_preserves_fixture_mode(self):
        rcmake = _fake_executable(self.root / "bin/rcmake")
        code, _, stderr = self.run_cli([
            "--fixture",
            "--trap", "T01",
            "--rcmake", str(rcmake),
            "--out-json", str(self.root / "a.json"),
            "--out-md", str(self.root / "a.md"),
        ])
        self.assertEqual(code, 2)
        self.assertIn("--traps", stderr)

        out_json = self.root / "fixture.json"
        out_md = self.root / "fixture.md"

        def fake_eval(_args, report, _root):
            report["steps"].append({"step": 0, "edit": "initial configure", "passed": True})
            report["effective_edits"] = 7
            report["semantic_edits"] = 6
            report["replay_verified"] = True

        with mock.patch.object(me, "evaluate", side_effect=fake_eval):
            code, _, stderr = self.run_cli([
                "--fixture",
                "--edits", "7",
                "--rcmake", str(rcmake),
                "--out-json", str(out_json),
                "--out-md", str(out_md),
            ])
        self.assertEqual(code, 0, stderr)
        payload = json.loads(out_json.read_text(encoding="utf-8"))
        self.assertTrue(payload["passed"])
        self.assertEqual(payload["mode"], "gate")
        self.assertEqual(payload["project"], "fixture")
        self.assertIn("# Leaf-edit comparison", out_md.read_text(encoding="utf-8"))

    def test_require_replay_record_accepts_utf8_string_and_byte_array_paths(self):
        source = self.root / "src"
        build = self.root / "build"
        source.mkdir()
        build.mkdir()
        _write_valid_replay_record(source, build)
        me.require_replay_record(source, build)

        class _FixedPath:
            def __init__(self, logical: str, real: Path) -> None:
                self._logical = logical
                self._real = real

            def __truediv__(self, other: str) -> Path:
                return self._real / other

            def __fspath__(self) -> str:
                return self._logical

            def __str__(self) -> str:
                return self._logical

        # Use fixed ASCII logical paths so the string-format fixture stays valid UTF-8 on every
        # platform, even when the host temp directory contains non-ASCII Windows UTF-16LE bytes.
        fixed_source = _FixedPath("/rcmake-test/src", source)
        fixed_build = _FixedPath("/rcmake-test/build", build)
        raw_paths = [
            str(p).encode("utf-16-le", errors="surrogatepass") if os.name == "nt" else os.fsencode(p)
            for p in (fixed_source, fixed_build)
        ]
        encoded = [p.decode("utf-8") for p in raw_paths]
        payload = json.dumps({
            "schema": "configure.v1",
            "replay_schema": "configure.v4",
            "region": {
                "key": {"kind": "Configure", "source_dir": encoded[0], "binary_dir": encoded[1]},
                "payload": {"Complete": {"inputs": [], "effects": []}},
            },
            "model": {"nested": []},
        }).encode("utf-8")
        header = me.REPLAY_MAGIC + bytes([1 if os.name == "nt" else 2]) + len(payload).to_bytes(4, "little")
        frame = header + payload
        frame += hashlib.sha256(frame).digest()
        (build / me.REPLAY_RECORD).write_bytes(frame)
        me.require_replay_record(fixed_source, fixed_build)
        self.assertEqual(me._decode_journal_bytes("caf\u00e9"), b"caf\xc3\xa9")
        self.assertEqual(me._decode_journal_bytes([0xE9, 0x00]), b"\xe9\x00")
        self.assertIsNone(me._decode_journal_bytes([True]))
        self.assertIsNone(me._decode_journal_bytes([256]))


if __name__ == "__main__":
    unittest.main()
