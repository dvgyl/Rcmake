# SPDX-License-Identifier: MIT OR Apache-2.0
"""Unit tests for scripts/generate-manifests.py and checked-in packaging manifests."""

from __future__ import annotations

from contextlib import redirect_stderr, redirect_stdout
import importlib.util
import io
import json
from pathlib import Path
import shutil
import subprocess
from tempfile import TemporaryDirectory
import unittest

ROOT = Path(__file__).resolve().parents[2]
SCRIPT_PATH = ROOT / "scripts" / "generate-manifests.py"
PACKAGING_DIR = ROOT / "packaging"


def _load_generator():
    spec = importlib.util.spec_from_file_location("generate_manifests", SCRIPT_PATH)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


gen = _load_generator()

SAMPLE_VERSION = "0.2.2"
SAMPLE_HASHES = {
    "rcmake-0.2.2-aarch64-apple-darwin.tar.gz": "1" * 64,
    "rcmake-0.2.2-x86_64-unknown-linux-gnu.tar.gz": "2" * 64,
    "rcmake-0.2.2-aarch64-unknown-linux-gnu.tar.gz": "3" * 64,
    "rcmake-0.2.2-x86_64-pc-windows-msvc.zip": "a" * 63 + "f",
}


def make_checksums_text(entries: dict[str, str], *, binary_star: bool = False) -> str:
    prefix = "*" if binary_star else ""
    return "".join(f"{digest}  {prefix}{name}\n" for name, digest in sorted(entries.items()))


class GenerateManifestsTests(unittest.TestCase):
    def setUp(self) -> None:
        tmp = TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.checksums_file = self.root / "SHA256SUMS.txt"
        self.checksums_file.write_text(make_checksums_text(SAMPLE_HASHES), encoding="utf-8")
        self.out_dir = self.root / "packaging"

    def run_cli(self, *args: str) -> tuple[int, str, str]:
        stdout = io.StringIO()
        stderr = io.StringIO()
        with redirect_stdout(stdout), redirect_stderr(stderr):
            code = gen.main(list(args))
        return code, stdout.getvalue(), stderr.getvalue()

    def test_parse_checksums_accepts_text_binary_and_extra_targets(self) -> None:
        text = (
            "\n"
            + f"{'A' * 64}  rcmake-0.2.2-aarch64-apple-darwin.tar.gz\n"
            + f"{'b' * 64} *rcmake-0.2.2-x86_64-unknown-linux-gnu.tar.gz\n"
            + f"{'c' * 64}\trcmake-0.2.2-aarch64-unknown-linux-gnu.tar.gz\n"
            + f"{'d' * 64}  rcmake-0.2.2-x86_64-pc-windows-msvc.zip\n"
            + f"{'e' * 64}  rcmake-0.2.2-aarch64-pc-windows-msvc.zip\n"
        )
        parsed = gen.parse_checksums(text)
        self.assertEqual(parsed["rcmake-0.2.2-aarch64-apple-darwin.tar.gz"], "a" * 64)
        self.assertEqual(parsed["rcmake-0.2.2-x86_64-unknown-linux-gnu.tar.gz"], "b" * 64)
        self.assertEqual(parsed["rcmake-0.2.2-aarch64-unknown-linux-gnu.tar.gz"], "c" * 64)
        self.assertEqual(parsed["rcmake-0.2.2-x86_64-pc-windows-msvc.zip"], "d" * 64)
        self.assertEqual(parsed["rcmake-0.2.2-aarch64-pc-windows-msvc.zip"], "e" * 64)

    def test_parse_checksums_rejects_malformed_lines(self) -> None:
        cases = [
            "",
            "   \n\n",
            "deadbeef  rcmake-0.2.2-aarch64-apple-darwin.tar.gz\n",
            f"{'g' * 64}  rcmake-0.2.2-aarch64-apple-darwin.tar.gz\n",
            f"{'a' * 64}\n",
            f"{'a' * 64}  ../escape.tar.gz\n",
            f"{'a' * 64}  sub/rcmake-0.2.2-aarch64-apple-darwin.tar.gz\n",
            f"{'a' * 64}  rcmake-0.2.2-aarch64-apple-darwin.tar.gz extra\n",
            (
                f"{'a' * 64}  rcmake-0.2.2-aarch64-apple-darwin.tar.gz\n"
                f"{'b' * 64}  rcmake-0.2.2-aarch64-apple-darwin.tar.gz\n"
            ),
        ]
        for bad in cases:
            with self.subTest(bad=bad):
                with self.assertRaises(gen.ManifestError):
                    gen.parse_checksums(bad)

    def test_version_normalization_and_validation(self) -> None:
        self.assertEqual(gen.normalize_version("0.2.2"), "0.2.2")
        self.assertEqual(gen.normalize_version("v0.2.2"), "0.2.2")
        self.assertEqual(gen.normalize_version("v0.2.2-rc.1"), "0.2.2-rc.1")
        for bad in ("", "0.2", "vv0.2.2", "0.2.2\nEvil", "0.2.2/.."):
            with self.subTest(bad=bad):
                with self.assertRaises(gen.ManifestError):
                    gen.normalize_version(bad)

    def test_missing_target_checksum_fails_cleanly(self) -> None:
        partial = dict(SAMPLE_HASHES)
        del partial["rcmake-0.2.2-x86_64-pc-windows-msvc.zip"]
        bad_file = self.root / "partial.txt"
        bad_file.write_text(make_checksums_text(partial), encoding="utf-8")

        code, _, stderr = self.run_cli(
            "--version", SAMPLE_VERSION, "--checksums", str(bad_file), "--out-dir", str(self.out_dir)
        )
        self.assertNotEqual(code, 0)
        self.assertIn("rcmake-0.2.2-x86_64-pc-windows-msvc.zip", stderr)
        self.assertFalse(self.out_dir.exists())

    def test_missing_checksums_file_fails_cleanly(self) -> None:
        code, _, stderr = self.run_cli(
            "--version",
            SAMPLE_VERSION,
            "--checksums",
            str(self.root / "nonexistent.txt"),
            "--out-dir",
            str(self.out_dir),
        )
        self.assertNotEqual(code, 0)
        self.assertIn("does not exist", stderr)

    def test_generate_and_check_roundtrip(self) -> None:
        code, stdout, stderr = self.run_cli(
            "--version",
            f"v{SAMPLE_VERSION}",
            "--checksums",
            str(self.checksums_file),
            "--out-dir",
            str(self.out_dir),
        )
        self.assertEqual(code, 0, msg=stderr)
        self.assertIn("Generated 5 package manifests", stdout)

        # --check must pass immediately after generation.
        code, stdout, stderr = self.run_cli(
            "--version",
            SAMPLE_VERSION,
            "--checksums",
            str(self.checksums_file),
            "--out-dir",
            str(self.out_dir),
            "--check",
        )
        self.assertEqual(code, 0, msg=stderr)
        self.assertIn("Verified 5 package manifests", stdout)

        # Mutating one file must trigger --check failure with a unified diff.
        scoop_path = self.out_dir / "scoop" / "rcmake.json"
        original = scoop_path.read_text(encoding="utf-8")
        scoop_path.write_text(original.replace("0.2.2", "0.2.9"), encoding="utf-8")

        code, _, stderr = self.run_cli(
            "--version",
            SAMPLE_VERSION,
            "--checksums",
            str(self.checksums_file),
            "--out-dir",
            str(self.out_dir),
            "--check",
        )
        self.assertNotEqual(code, 0)
        self.assertIn("Manifest drift", stderr)
        self.assertIn("scoop/rcmake.json", stderr)

        # Deleting a file must also fail --check without recreating it.
        scoop_path.unlink()
        code, _, stderr = self.run_cli(
            "--version",
            SAMPLE_VERSION,
            "--checksums",
            str(self.checksums_file),
            "--out-dir",
            str(self.out_dir),
            "--check",
        )
        self.assertNotEqual(code, 0)
        self.assertIn("Missing manifest", stderr)
        self.assertFalse(scoop_path.exists())

    def test_deterministic_output(self) -> None:
        out_a = self.root / "a"
        out_b = self.root / "b"
        for target_dir in (out_a, out_b):
            code, _, stderr = self.run_cli(
                "--version",
                SAMPLE_VERSION,
                "--checksums",
                str(self.checksums_file),
                "--out-dir",
                str(target_dir),
            )
            self.assertEqual(code, 0, msg=stderr)

        files_a = sorted(p.relative_to(out_a) for p in out_a.rglob("*") if p.is_file())
        files_b = sorted(p.relative_to(out_b) for p in out_b.rglob("*") if p.is_file())
        self.assertEqual(files_a, files_b)
        for rel in files_a:
            self.assertEqual((out_a / rel).read_bytes(), (out_b / rel).read_bytes())

    def test_homebrew_formula_contents_and_ruby_syntax(self) -> None:
        code, _, stderr = self.run_cli(
            "--version",
            SAMPLE_VERSION,
            "--checksums",
            str(self.checksums_file),
            "--out-dir",
            str(self.out_dir),
        )
        self.assertEqual(code, 0, msg=stderr)
        formula = self.out_dir / "homebrew" / "rcmake.rb"
        text = formula.read_text(encoding="utf-8")

        self.assertIn('homepage "https://rcmake.github.io/rcmake/"', text)
        self.assertIn('license any_of: ["MIT", "Apache-2.0"]', text)
        self.assertIn('version "0.2.2"', text)
        self.assertIn("rcmake-0.2.2-aarch64-apple-darwin.tar.gz", text)
        self.assertIn("rcmake-0.2.2-x86_64-unknown-linux-gnu.tar.gz", text)
        self.assertIn("rcmake-0.2.2-aarch64-unknown-linux-gnu.tar.gz", text)
        self.assertIn(f'sha256 "{"1" * 64}"', text)
        self.assertIn(f'sha256 "{"2" * 64}"', text)
        self.assertIn(f'sha256 "{"3" * 64}"', text)

        # ADR-052 libexec isolation: expose only rcmake, rcmake-lsp, rcmake-verify in bin.
        self.assertIn('libexec.install Dir["*"]', text)
        self.assertIn('bin.install_symlink libexec/"bin/rcmake"', text)
        self.assertIn('bin.install_symlink libexec/"bin/rcmake-lsp"', text)
        self.assertIn('bin.install_symlink libexec/"bin/rcmake-verify"', text)
        for forbidden in ("cmake", "ctest", "cpack", "ccmake"):
            self.assertNotIn(f'bin.install_symlink libexec/"bin/{forbidden}"', text)
        self.assertNotIn('man1.install_symlink libexec/"share/man/man1/ccmake.1"', text)

        self.assertIn('#{opt_libexec}/bin:$PATH', text)
        self.assertIn("project(HomebrewSmoke LANGUAGES NONE)", text)

        if shutil.which("ruby"):
            result = subprocess.run(
                ["ruby", "-c", str(formula)],
                check=False,
                capture_output=True,
                text=True,
                timeout=15,
            )
            self.assertEqual(result.returncode, 0, msg=result.stderr)

    def test_scoop_manifest_schema(self) -> None:
        code, _, stderr = self.run_cli(
            "--version",
            SAMPLE_VERSION,
            "--checksums",
            str(self.checksums_file),
            "--out-dir",
            str(self.out_dir),
        )
        self.assertEqual(code, 0, msg=stderr)
        scoop_path = self.out_dir / "scoop" / "rcmake.json"
        data = json.loads(scoop_path.read_text(encoding="utf-8"))

        self.assertEqual(data["version"], SAMPLE_VERSION)
        self.assertEqual(data["homepage"], "https://rcmake.github.io/rcmake/")
        self.assertEqual(data["license"], "MIT OR Apache-2.0")
        arch64 = data["architecture"]["64bit"]
        self.assertEqual(
            arch64["url"],
            "https://github.com/rcmake/rcmake/releases/download/v0.2.2/rcmake-0.2.2-x86_64-pc-windows-msvc.zip",
        )
        self.assertEqual(arch64["hash"], "a" * 63 + "f")
        self.assertEqual(arch64["extract_dir"], "rcmake-0.2.2-x86_64-pc-windows-msvc")
        self.assertEqual(
            data["bin"],
            ["bin/rcmake.exe", "bin/rcmake-lsp.exe", "bin/rcmake-verify.exe"],
        )
        self.assertEqual(data["checkver"], {"github": "https://github.com/rcmake/rcmake"})
        self.assertEqual(
            data["autoupdate"]["architecture"]["64bit"]["url"],
            "https://github.com/rcmake/rcmake/releases/download/v$version/rcmake-$version-x86_64-pc-windows-msvc.zip",
        )
        self.assertEqual(data["autoupdate"]["hash"]["url"], "$baseurl/SHA256SUMS.txt")
        self.assertTrue(any("$dir\\bin" in line for line in data["notes"]))

    def test_winget_manifests_and_yaml_syntax(self) -> None:
        code, _, stderr = self.run_cli(
            "--version",
            SAMPLE_VERSION,
            "--checksums",
            str(self.checksums_file),
            "--out-dir",
            str(self.out_dir),
        )
        self.assertEqual(code, 0, msg=stderr)
        winget_dir = self.out_dir / "winget"
        version_yaml = winget_dir / "Rcmake.Rcmake.yaml"
        installer_yaml = winget_dir / "Rcmake.Rcmake.installer.yaml"
        locale_yaml = winget_dir / "Rcmake.Rcmake.locale.en-US.yaml"

        if shutil.which("ruby"):
            script = (
                "require 'yaml'; require 'json'; "
                "puts JSON.generate(ARGV.map { |p| YAML.safe_load_file(p) })"
            )
            result = subprocess.run(
                ["ruby", "-e", script, str(version_yaml), str(installer_yaml), str(locale_yaml)],
                check=False,
                capture_output=True,
                text=True,
                timeout=15,
            )
            self.assertEqual(result.returncode, 0, msg=result.stderr)
            ver_doc, inst_doc, loc_doc = json.loads(result.stdout)

            self.assertEqual(ver_doc["PackageIdentifier"], "Rcmake.Rcmake")
            self.assertEqual(ver_doc["PackageVersion"], SAMPLE_VERSION)
            self.assertEqual(ver_doc["DefaultLocale"], "en-US")
            self.assertEqual(ver_doc["ManifestType"], "version")
            self.assertEqual(ver_doc["ManifestVersion"], "1.9.0")

            self.assertEqual(inst_doc["InstallerType"], "zip")
            self.assertEqual(inst_doc["NestedInstallerType"], "portable")
            self.assertEqual(
                inst_doc["NestedInstallerFiles"],
                [
                    {
                        "RelativeFilePath": "rcmake-0.2.2-x86_64-pc-windows-msvc\\bin\\rcmake.exe",
                        "PortableCommandAlias": "rcmake",
                    },
                    {
                        "RelativeFilePath": "rcmake-0.2.2-x86_64-pc-windows-msvc\\bin\\rcmake-lsp.exe",
                        "PortableCommandAlias": "rcmake-lsp",
                    },
                    {
                        "RelativeFilePath": "rcmake-0.2.2-x86_64-pc-windows-msvc\\bin\\rcmake-verify.exe",
                        "PortableCommandAlias": "rcmake-verify",
                    },
                ],
            )
            self.assertEqual(
                inst_doc["Installers"],
                [
                    {
                        "Architecture": "x64",
                        "InstallerUrl": "https://github.com/rcmake/rcmake/releases/download/v0.2.2/rcmake-0.2.2-x86_64-pc-windows-msvc.zip",
                        "InstallerSha256": ("a" * 63 + "f").upper(),
                    }
                ],
            )

            self.assertEqual(loc_doc["PackageLocale"], "en-US")
            self.assertEqual(loc_doc["License"], "MIT OR Apache-2.0")
            self.assertEqual(loc_doc["PackageUrl"], "https://rcmake.github.io/rcmake/")
            self.assertEqual(
                loc_doc["ReleaseNotesUrl"],
                "https://github.com/rcmake/rcmake/releases/tag/v0.2.2",
            )
            self.assertEqual(loc_doc["ManifestType"], "defaultLocale")

    def test_checked_in_manifests_are_valid(self) -> None:
        homebrew = PACKAGING_DIR / "homebrew" / "rcmake.rb"
        scoop = PACKAGING_DIR / "scoop" / "rcmake.json"
        winget_files = [
            PACKAGING_DIR / "winget" / "Rcmake.Rcmake.yaml",
            PACKAGING_DIR / "winget" / "Rcmake.Rcmake.installer.yaml",
            PACKAGING_DIR / "winget" / "Rcmake.Rcmake.locale.en-US.yaml",
        ]
        for path in [homebrew, scoop, *winget_files, PACKAGING_DIR / "README.md"]:
            self.assertTrue(path.is_file(), msg=f"Missing checked-in file: {path}")

        scoop_data = json.loads(scoop.read_text(encoding="utf-8"))
        self.assertEqual(scoop_data["license"], "MIT OR Apache-2.0")
        self.assertRegex(scoop_data["architecture"]["64bit"]["hash"], r"^[0-9a-f]{64}$")

        if shutil.which("ruby"):
            ruby_check = subprocess.run(
                ["ruby", "-c", str(homebrew)],
                check=False,
                capture_output=True,
                text=True,
                timeout=15,
            )
            self.assertEqual(ruby_check.returncode, 0, msg=ruby_check.stderr)

            yaml_check = subprocess.run(
                ["ruby", "-ryaml", "-e", "ARGV.each { |f| YAML.safe_load_file(f) }", *map(str, winget_files)],
                check=False,
                capture_output=True,
                text=True,
                timeout=15,
            )
            self.assertEqual(yaml_check.returncode, 0, msg=yaml_check.stderr)


if __name__ == "__main__":
    unittest.main()
