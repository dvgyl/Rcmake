# SPDX-License-Identifier: MIT OR Apache-2.0
"""Test the probe cache isolation and host data of tools/measure/bench_eval.py."""

from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import unittest

from tools.measure import bench_eval as be


class IsolationTests(unittest.TestCase):
    def setUp(self):
        temporary = TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def context(self, name="rcmake", base=None, probe_jobs=None):
        scratch = self.root / name
        scratch.mkdir(exist_ok=True)
        env = be.tool_env(base or {"PATH": "/usr/bin"}, scratch, probe_jobs)
        return be.Context(name, Path(sys.executable), scratch, "Ninja", env)

    def test_env_removes_rcmake_settings_but_keeps_vendored_data_root(self):
        env = be.isolated_env({
            "PATH": "/usr/bin",
            "HOME": "/srv/user",
            "RCMAKE_CACHE_DIR": "/srv/user/.cache/rcmake",
            "RCMAKE_PROBE_CACHE": "readonly",
            "RCMAKE_PROBE_CACHE_DIR": "/shared",
            "RCMAKE_PROBE_JOBS": "8",
            "RCMAKE_VENDORED_DATA_ROOT": "/opt/rcmake/share",
            "CMAKE_GENERATOR": "Xcode",
            "CTEST_OUTPUT_ON_FAILURE": "1",
            "MAKEFLAGS": "-j8",
        })
        self.assertEqual(env, {
            "PATH": "/usr/bin",
            "HOME": "/srv/user",
            "RCMAKE_VENDORED_DATA_ROOT": "/opt/rcmake/share",
            "COPYFILE_DISABLE": "1",
        })

    def test_each_tool_has_a_private_store(self):
        rcmake = self.context("rcmake", probe_jobs=1)
        oracle = self.context("oracle")
        self.assertEqual(rcmake.env["RCMAKE_CACHE_DIR"], str(self.root / "rcmake" / "store"))
        self.assertEqual(oracle.env["RCMAKE_CACHE_DIR"], str(self.root / "oracle" / "store"))
        self.assertEqual(rcmake.env["RCMAKE_PROBE_JOBS"], "1")
        self.assertNotIn("RCMAKE_PROBE_JOBS", oracle.env)

    def test_cold_prepare_empties_the_probe_store(self):
        benches = be.project_benches(self.root / "shared", [3])
        cold = next(bench for bench in benches if bench.name.endswith("(cold)"))
        warm = next(bench for bench in benches if bench.name.endswith("(warm)"))
        self.assertTrue(cold.empty_probe_cache)
        self.assertFalse(warm.empty_probe_cache)
        self.assertEqual(cold.notes, "new build tree, empty probe cache, includes compiler checks")
        cx = self.context()
        store = Path(cx.env["RCMAKE_CACHE_DIR"])
        entry = store / "entries" / "probe-v1" / "ab" / "cd.entry"
        entry.parent.mkdir(parents=True)
        entry.write_text("cached", encoding="utf-8")
        build = cx.scratch / "cold3"
        build.mkdir()
        cold.prepare(cx)
        self.assertFalse(store.exists())
        self.assertFalse(build.exists())

    def test_probe_hits_reads_both_log_formats(self):
        self.assertEqual(be.probe_hits("-- probe cache: 4 hits, 2 misses"), 4)
        self.assertEqual(be.probe_hits("probe_hits=0 probe_misses=7"), 0)
        self.assertIsNone(be.probe_hits("-- Configuring done"))

    def test_cold_run_with_probe_hits_fails(self):
        cx = self.context()
        for output, error in (("probe_hits=3", "a cold run reported 3 probe cache hits"),
                              ("probe_hits=0", None), ("no probe data", None)):
            with self.subTest(output=output):
                bench = be.Bench(
                    "cold", "project",
                    lambda cx, output=output: ([sys.executable, "-c", f"print({output!r})"], cx.scratch),
                    empty_probe_cache=True,
                )
                result = be.run_bench(bench, cx, runs=2, warmup=1, timeout=60, method="wait4")
                self.assertEqual(result.error, error)
                self.assertEqual(len(result.samples), 0 if error else 2)


class HardwareTests(unittest.TestCase):
    def test_macos_hardware(self):
        outputs = {
            ("sysctl", "-n", "machdep.cpu.brand_string"): "Apple M1 Pro",
            ("sysctl", "-n", "hw.memsize"): str(32 * be.GIB),
            ("sw_vers", "-productName"): "macOS",
            ("sw_vers", "-productVersion"): "15.1",
            ("sw_vers", "-buildVersion"): "24B83",
            ("pmset", "-g", "batt"): "Now drawing from 'Battery Power'\n -InternalBattery-0 80%",
        }
        info = be.host_hardware("Darwin", lambda command: outputs.get(tuple(command)))
        self.assertEqual(info, {"cpu_model": "Apple M1 Pro", "memory_gib": 32.0,
                                "os_version": "macOS 15.1 (24B83)",
                                "power_source": "Battery Power"})
        self.assertEqual(be.describe_hardware(info),
                         "Apple M1 Pro, 32 GiB RAM, macOS 15.1 (24B83), power: Battery Power")

    def test_linux_hardware(self):
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            files = {
                "proc/cpuinfo": "processor\t: 0\nmodel name\t: AMD EPYC 7763 64-Core Processor\n",
                "proc/meminfo": "MemTotal:       16384000 kB\nMemFree: 1 kB\n",
                "etc/os-release": 'NAME="Ubuntu"\nPRETTY_NAME="Ubuntu 24.04.1 LTS"\n',
                "sys/class/power_supply/AC/type": "Mains\n",
                "sys/class/power_supply/AC/online": "1\n",
                "sys/class/power_supply/BAT0/type": "Battery\n",
            }
            for name, text in files.items():
                (root / name).parent.mkdir(parents=True, exist_ok=True)
                (root / name).write_text(text, encoding="utf-8")
            info = be.host_hardware("Linux", lambda command: None, root)
        self.assertEqual(info, {"cpu_model": "AMD EPYC 7763 64-Core Processor",
                                "memory_gib": 15.6, "os_version": "Ubuntu 24.04.1 LTS",
                                "power_source": "AC Power"})

    def test_missing_data_is_none(self):
        with TemporaryDirectory() as temporary:
            info = be.host_hardware("Linux", lambda command: None, Path(temporary))
        self.assertEqual(info, dict.fromkeys(("cpu_model", "memory_gib", "os_version",
                                              "power_source")))
        self.assertEqual(be.describe_hardware(info),
                         "unknown CPU, unknown RAM, unknown OS version, power: unknown")
        self.assertEqual(be.host_hardware("Windows", lambda command: "x")["cpu_model"], None)


if __name__ == "__main__":
    unittest.main()
