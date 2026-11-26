# SPDX-License-Identifier: MIT OR Apache-2.0
"""Unit tests for scripts/install.sh and scripts/install.ps1."""

from __future__ import annotations

import hashlib
import io
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import tarfile
from tempfile import TemporaryDirectory
import unittest

ROOT = Path(__file__).resolve().parents[2]
INSTALL_SH = ROOT / "scripts" / "install.sh"
INSTALL_PS1 = ROOT / "scripts" / "install.ps1"
EXTRA_SHARE_FILES = (
    "share/man/man1/rcmake.1",
    "share/bash-completion/completions/rcmake",
    "share/zsh/site-functions/_rcmake",
    "share/fish/vendor_completions.d/rcmake.fish",
    "share/rcmake/completions/rcmake.ps1",
)


def _make_fake_archive(
    dest_dir: Path,
    *,
    version: str = "0.1.0",
    archive_name: str | None = None,
    lsp_mode: str = "standalone",
    lsp_link_target: str = "rcmake",
    rcmake_is_symlink: bool = False,
    sidecar: str = "valid",
    extra_marker: str = "v1",
    include_completions_and_man: bool = False,
    symlink_extra_path: str | None = None,
) -> Path:
    """Create a synthetic release .tar.gz archive and optional .sha256 sidecar."""
    top = f"rcmake-{version}-test"
    archive_path = dest_dir / (archive_name or f"{top}.tar.gz")

    rcmake_script = (
        "#!/bin/sh\n"
        'if [ "$1" = "--version" ]; then\n'
        f'  printf "cmake version 4.5.1\\nRcmake {version}\\n"\n'
        "  exit 0\n"
        "fi\n"
        f'printf "rcmake-fake {version} {extra_marker}\\n"\n'
    )
    lsp_script = f'#!/bin/sh\nprintf "standalone-lsp {version} {extra_marker}\\n"\n'

    def add_bytes(tar: tarfile.TarFile, relpath: str, data: bytes, mode: int) -> None:
        info = tarfile.TarInfo(name=f"{top}/{relpath}")
        info.size = len(data)
        info.mode = mode
        tar.addfile(info, io.BytesIO(data))

    def add_symlink(tar: tarfile.TarFile, relpath: str, target: str) -> None:
        info = tarfile.TarInfo(name=f"{top}/{relpath}")
        info.type = tarfile.SYMTYPE
        info.linkname = target
        info.mode = 0o777
        tar.addfile(info)

    with tarfile.open(archive_path, "w:gz") as tar:
        if rcmake_is_symlink:
            add_symlink(tar, "bin/rcmake", "other")
        else:
            add_bytes(tar, "bin/rcmake", rcmake_script.encode("utf-8"), 0o755)

        if lsp_mode == "standalone":
            add_bytes(tar, "bin/rcmake-lsp", lsp_script.encode("utf-8"), 0o755)
        elif lsp_mode == "symlink":
            add_symlink(tar, "bin/rcmake-lsp", lsp_link_target)
        elif lsp_mode != "absent":
            raise ValueError(f"Unsupported lsp_mode: {lsp_mode}")

        # Release archives also include cmake/ctest/cpack symlinks; install.sh must
        # only install them when --alias-cmake is passed.
        for tool in ("cmake", "ctest", "cpack"):
            add_symlink(tar, f"bin/{tool}", "rcmake")

        add_bytes(
            tar,
            "share/cmake-4.5/Modules/CMake.cmake",
            f"# fake CMake.cmake {version} {extra_marker}\n".encode("utf-8"),
            0o644,
        )
        add_bytes(
            tar,
            "share/cmake-4.5/Templates/CPackConfig.cmake.in",
            f"# fake CPackConfig {version} {extra_marker}\n".encode("utf-8"),
            0o644,
        )
        if include_completions_and_man:
            for relpath in EXTRA_SHARE_FILES:
                add_bytes(
                    tar,
                    relpath,
                    f"# fake {relpath} {version} {extra_marker}\n".encode("utf-8"),
                    0o644,
                )
        if symlink_extra_path is not None:
            add_symlink(tar, symlink_extra_path, "/etc/passwd")

    sidecar_path = Path(f"{archive_path}.sha256")
    if sidecar == "valid":
        digest = hashlib.sha256(archive_path.read_bytes()).hexdigest()
        sidecar_path.write_text(f"{digest}  {archive_path.name}\n", encoding="ascii")
    elif sidecar == "bad":
        sidecar_path.write_text(f"{'0' * 64}  {archive_path.name}\n", encoding="ascii")
    elif sidecar != "missing":
        raise ValueError(f"Unsupported sidecar mode: {sidecar}")

    return archive_path


def _check_powershell_delimiters(source: str) -> None:
    """Verify balanced (), [], {} outside PowerShell comments, strings, and here-strings."""
    lines = source.splitlines()
    stack: list[tuple[str, int]] = []
    closing = {")": "(", "]": "[", "}": "{"}
    in_block_comment = False
    in_here_string: str | None = None  # "'" or '"'

    for lineno, line in enumerate(lines, 1):
        if in_here_string is not None:
            if line.startswith(f"{in_here_string}@"):
                in_here_string = None
                i = 2
            else:
                continue
        else:
            i = 0
        length = len(line)
        in_single = False
        in_double = False
        while i < length:
            ch = line[i]
            nxt = line[i + 1] if i + 1 < length else ""

            if in_block_comment:
                if ch == "#" and nxt == ">":
                    in_block_comment = False
                    i += 2
                    continue
                i += 1
                continue

            if in_single:
                if ch == "'" and nxt == "'":
                    i += 2
                    continue
                if ch == "'":
                    in_single = False
                i += 1
                continue

            if in_double:
                if ch == "`" and i + 1 < length:
                    i += 2
                    continue
                if ch == '"':
                    in_double = False
                i += 1
                continue

            if ch == "<" and nxt == "#":
                in_block_comment = True
                i += 2
                continue
            if ch == "#":
                break
            if ch == "@" and nxt in ("'", '"') and i + 2 == length:
                in_here_string = nxt
                break
            if ch == "'":
                in_single = True
                i += 1
                continue
            if ch == '"':
                in_double = True
                i += 1
                continue
            if ch in "([{":
                stack.append((ch, lineno))
            elif ch in ")]}":
                if not stack or stack[-1][0] != closing[ch]:
                    raise AssertionError(f"Unmatched {ch!r} at line {lineno}")
                stack.pop()
            i += 1

        if in_single or in_double:
            raise AssertionError(f"Unclosed string literal at line {lineno}")

    if in_block_comment:
        raise AssertionError("Unclosed block comment")
    if in_here_string is not None:
        raise AssertionError("Unclosed here-string")
    if stack:
        opener, line = stack[-1]
        raise AssertionError(f"Unclosed {opener!r} opened at line {line}")


class InstallShTests(unittest.TestCase):
    def setUp(self) -> None:
        tmp = TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.archives = self.root / "archives"
        self.archives.mkdir()
        self.prefix = self.root / "prefix"
        self.fake_home = self.root / "fake-home"
        self.fake_home.mkdir()
        self.tmp_dir = self.root / "tmp"
        self.tmp_dir.mkdir()

    def run_install(
        self,
        *args: str,
        path_entries: list[str] | None = None,
    ) -> subprocess.CompletedProcess[str]:
        python_dir = str(Path(sys.executable).resolve().parent)
        base_path = [python_dir, "/usr/bin", "/bin"]
        if path_entries:
            base_path = [*path_entries, *base_path]
        env = {
            "HOME": str(self.fake_home),
            "PREFIX": str(self.prefix),
            "RCMAKE_CACHE_DIR": str(self.root / "cache-unused"),
            "TMPDIR": str(self.tmp_dir),
            "PATH": os.pathsep.join(base_path),
            "LC_ALL": "C",
        }
        return subprocess.run(
            ["/bin/bash", str(INSTALL_SH), *args],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=30,
        )

    def test_bash_syntax_check(self) -> None:
        result = subprocess.run(
            ["/bin/bash", "-n", str(INSTALL_SH)],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=15,
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_standalone_lsp_install_reinstall_and_uninstall(self) -> None:
        v1 = _make_fake_archive(self.archives, version="0.1.0", lsp_mode="standalone", extra_marker="v1")
        res = self.run_install("--archive", str(v1))
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertIn("Verified the SHA-256 checksum", res.stdout)
        self.assertIn("Installed rcmake 0.1.0 into", res.stdout)

        rcmake_bin = self.prefix / "bin" / "rcmake"
        lsp_bin = self.prefix / "bin" / "rcmake-lsp"
        data_file = self.prefix / "share" / "cmake-4.5" / "Modules" / "CMake.cmake"

        self.assertTrue(rcmake_bin.is_file() and not rcmake_bin.is_symlink())
        self.assertTrue(lsp_bin.is_file() and not lsp_bin.is_symlink())
        self.assertTrue(stat.S_IMODE(rcmake_bin.stat().st_mode) & 0o111)
        self.assertTrue(stat.S_IMODE(lsp_bin.stat().st_mode) & 0o111)
        self.assertIn("standalone-lsp 0.1.0 v1", lsp_bin.read_text(encoding="utf-8"))
        self.assertIn("0.1.0 v1", data_file.read_text(encoding="utf-8"))
        for tool in ("cmake", "ctest", "cpack"):
            self.assertFalse(os.path.lexists(self.prefix / "bin" / tool))

        # Reinstall with a newer standalone build replaces binary, LSP, and share data.
        v2 = _make_fake_archive(self.archives, version="0.2.0", lsp_mode="standalone", extra_marker="v2")
        res = self.run_install("--archive", str(v2))
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertIn("Installed rcmake 0.2.0 into", res.stdout)
        self.assertTrue(lsp_bin.is_file() and not lsp_bin.is_symlink())
        self.assertIn("standalone-lsp 0.2.0 v2", lsp_bin.read_text(encoding="utf-8"))
        self.assertIn("0.2.0 v2", data_file.read_text(encoding="utf-8"))

        # Reinstall switching from standalone binary to multi-call symlink and back.
        v3_sym = _make_fake_archive(
            self.archives,
            version="0.3.0",
            lsp_mode="symlink",
            lsp_link_target="rcmake",
            extra_marker="v3",
        )
        res = self.run_install("--archive", str(v3_sym))
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertTrue(lsp_bin.is_symlink())
        self.assertEqual(os.readlink(lsp_bin), "rcmake")
        self.assertIn(f"{lsp_bin} -> rcmake", res.stdout)

        res = self.run_install("--archive", str(v2))
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertTrue(lsp_bin.is_file() and not lsp_bin.is_symlink())
        self.assertIn("standalone-lsp 0.2.0 v2", lsp_bin.read_text(encoding="utf-8"))

        # Uninstall removes rcmake, standalone rcmake-lsp, and share/cmake-4.5.
        res = self.run_install("--uninstall")
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertFalse(os.path.lexists(rcmake_bin))
        self.assertFalse(os.path.lexists(lsp_bin))
        self.assertFalse(os.path.lexists(self.prefix / "share" / "cmake-4.5"))

    def test_symlink_lsp_install_and_uninstall(self) -> None:
        for link_target in ("rcmake", "./rcmake"):
            with self.subTest(link_target=link_target):
                archive = _make_fake_archive(
                    self.archives,
                    version="0.1.0",
                    archive_name=f"rcmake-sym-{link_target.replace('/', '_')}.tar.gz",
                    lsp_mode="symlink",
                    lsp_link_target=link_target,
                )
                res = self.run_install("--archive", str(archive))
                self.assertEqual(res.returncode, 0, res.stderr)

                rcmake_bin = self.prefix / "bin" / "rcmake"
                lsp_bin = self.prefix / "bin" / "rcmake-lsp"
                self.assertTrue(rcmake_bin.is_file() and not rcmake_bin.is_symlink())
                self.assertTrue(lsp_bin.is_symlink())
                self.assertEqual(os.readlink(lsp_bin), "rcmake")
                self.assertIn(f"{lsp_bin} -> rcmake", res.stdout)

                # Invoking the symlink resolves to rcmake.
                probe = subprocess.run(
                    [str(lsp_bin), "--version"],
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                    timeout=15,
                )
                self.assertEqual(probe.returncode, 0, probe.stderr)
                self.assertIn("Rcmake 0.1.0", probe.stdout)

                # Uninstall removes both rcmake and the symlink rcmake-lsp cleanly.
                res = self.run_install("--uninstall")
                self.assertEqual(res.returncode, 0, res.stderr)
                self.assertIn(str(rcmake_bin), res.stdout)
                self.assertIn(str(lsp_bin), res.stdout)
                self.assertFalse(os.path.lexists(rcmake_bin))
                self.assertFalse(os.path.lexists(lsp_bin))
                self.assertFalse(os.path.lexists(self.prefix / "share" / "cmake-4.5"))

    def test_symlink_lsp_rejects_unsafe_targets_and_symlink_rcmake(self) -> None:
        bad_targets = ("../bin/rcmake", "/bin/sh", "../../etc/passwd", "sub/rcmake", "rcmake/", "other", "")
        for idx, target in enumerate(bad_targets):
            with self.subTest(target=target):
                archive = _make_fake_archive(
                    self.archives,
                    archive_name=f"bad-link-{idx}.tar.gz",
                    lsp_mode="symlink",
                    lsp_link_target=target,
                )
                res = self.run_install("--archive", str(archive))
                self.assertNotEqual(res.returncode, 0)
                self.assertIn("Invalid symlink target", res.stderr)
                self.assertFalse(os.path.lexists(self.prefix / "bin" / "rcmake-lsp"))

        sym_rcmake = _make_fake_archive(
            self.archives,
            archive_name="sym-rcmake.tar.gz",
            rcmake_is_symlink=True,
        )
        res = self.run_install("--archive", str(sym_rcmake))
        self.assertNotEqual(res.returncode, 0)
        self.assertIn("Invalid archive entry", res.stderr)
        self.assertFalse(os.path.lexists(self.prefix / "bin" / "rcmake"))

    def test_bad_sha256_rejected_and_missing_sidecar_warns(self) -> None:
        bad_archive = _make_fake_archive(self.archives, archive_name="bad.tar.gz", sidecar="bad")
        res = self.run_install("--archive", str(bad_archive))
        self.assertNotEqual(res.returncode, 0)
        self.assertIn("SHA-256 mismatch", res.stderr)
        self.assertFalse(self.prefix.exists())

        no_sidecar = _make_fake_archive(self.archives, archive_name="no-sidecar.tar.gz", sidecar="missing")
        res = self.run_install("--archive", str(no_sidecar))
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertIn("Warning:", res.stderr)
        self.assertIn("cannot verify the SHA-256 checksum", res.stderr)
        self.assertTrue((self.prefix / "bin" / "rcmake").is_file())

    def test_alias_cmake_collision_precheck_and_force_overwrite(self) -> None:
        bin_dir = self.prefix / "bin"
        bin_dir.mkdir(parents=True)
        foreign_cmake = bin_dir / "cmake"
        foreign_cpack = bin_dir / "cpack"
        foreign_cmake.write_text("#!/bin/sh\necho foreign-cmake\n", encoding="utf-8")
        foreign_cpack.write_text("#!/bin/sh\necho foreign-cpack\n", encoding="utf-8")

        archive = _make_fake_archive(self.archives, archive_name="alias.tar.gz", lsp_mode="symlink")

        # Without --force, pre-check fails before writing anything.
        res = self.run_install("--archive", str(archive), "--alias-cmake")
        self.assertNotEqual(res.returncode, 0)
        self.assertIn("--force", res.stderr)
        self.assertIn(str(foreign_cmake), res.stderr)
        self.assertIn(str(foreign_cpack), res.stderr)
        self.assertIn("The installer changed nothing.", res.stderr)
        self.assertEqual(foreign_cmake.read_text(encoding="utf-8"), "#!/bin/sh\necho foreign-cmake\n")
        self.assertEqual(foreign_cpack.read_text(encoding="utf-8"), "#!/bin/sh\necho foreign-cpack\n")
        self.assertFalse(os.path.lexists(bin_dir / "rcmake"))
        self.assertFalse(os.path.lexists(bin_dir / "ctest"))
        self.assertFalse(os.path.lexists(self.prefix / "share" / "cmake-4.5"))

        # With --force, all three aliases become symlinks to rcmake.
        res = self.run_install("--archive", str(archive), "--alias-cmake", "--force")
        self.assertEqual(res.returncode, 0, res.stderr)
        for tool in ("cmake", "ctest", "cpack"):
            alias = bin_dir / tool
            self.assertTrue(alias.is_symlink())
            self.assertEqual(os.readlink(alias), "rcmake")

        # Re-running --alias-cmake without --force succeeds once rcmake owns the links.
        res = self.run_install("--archive", str(archive), "--alias-cmake")
        self.assertEqual(res.returncode, 0, res.stderr)

        # Uninstall cleans up all rcmake aliases as well.
        res = self.run_install("--uninstall")
        self.assertEqual(res.returncode, 0, res.stderr)
        for name in ("rcmake", "rcmake-lsp", "cmake", "ctest", "cpack"):
            self.assertFalse(os.path.lexists(bin_dir / name))
        self.assertFalse(os.path.lexists(self.prefix / "share" / "cmake-4.5"))

    def test_uninstall_preserves_share_data_when_foreign_cmake_exists(self) -> None:
        archive = _make_fake_archive(self.archives, archive_name="foreign-keep.tar.gz", lsp_mode="standalone")
        res = self.run_install("--archive", str(archive))
        self.assertEqual(res.returncode, 0, res.stderr)

        foreign_cmake = self.prefix / "bin" / "cmake"
        foreign_cmake.write_text("#!/bin/sh\necho kitware-cmake\n", encoding="utf-8")

        res = self.run_install("--uninstall")
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertFalse(os.path.lexists(self.prefix / "bin" / "rcmake"))
        self.assertFalse(os.path.lexists(self.prefix / "bin" / "rcmake-lsp"))
        self.assertTrue(foreign_cmake.is_file())
        self.assertEqual(foreign_cmake.read_text(encoding="utf-8"), "#!/bin/sh\necho kitware-cmake\n")
        self.assertTrue((self.prefix / "share" / "cmake-4.5" / "Modules" / "CMake.cmake").is_file())
        self.assertIn("Kept", res.stdout)
        self.assertIn("another CMake installation", res.stdout)

    def test_install_and_uninstall_with_completions_and_man_page(self) -> None:
        neighbors = {
            "share/man/man1/other.1": "other man page\n",
            "share/bash-completion/completions/other": "other bash completion\n",
            "share/zsh/site-functions/_other": "other zsh completion\n",
            "share/fish/vendor_completions.d/other.fish": "other fish completion\n",
        }
        for relpath, text in neighbors.items():
            target = self.prefix / relpath
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(text, encoding="utf-8")

        v1 = _make_fake_archive(
            self.archives,
            version="0.1.0",
            archive_name="rcmake-with-extras-v1.tar.gz",
            lsp_mode="symlink",
            extra_marker="v1",
            include_completions_and_man=True,
        )
        res = self.run_install("--archive", str(v1))
        self.assertEqual(res.returncode, 0, res.stderr)
        for relpath in EXTRA_SHARE_FILES:
            installed = self.prefix / relpath
            self.assertTrue(installed.is_file() and not installed.is_symlink(), relpath)
            self.assertEqual(stat.S_IMODE(installed.stat().st_mode), 0o644, relpath)
            self.assertIn(f"{relpath} 0.1.0 v1", installed.read_text(encoding="utf-8"))
            self.assertIn(str(installed), res.stdout)

        # Reinstall with a newer archive updates all five completion and man files.
        v2 = _make_fake_archive(
            self.archives,
            version="0.2.0",
            archive_name="rcmake-with-extras-v2.tar.gz",
            lsp_mode="symlink",
            extra_marker="v2",
            include_completions_and_man=True,
        )
        res = self.run_install("--archive", str(v2))
        self.assertEqual(res.returncode, 0, res.stderr)
        for relpath in EXTRA_SHARE_FILES:
            installed = self.prefix / relpath
            self.assertIn(f"{relpath} 0.2.0 v2", installed.read_text(encoding="utf-8"))

        # Uninstall removes only rcmake-owned files and preserves unrelated neighbors.
        res = self.run_install("--uninstall")
        self.assertEqual(res.returncode, 0, res.stderr)
        for relpath in EXTRA_SHARE_FILES:
            installed = self.prefix / relpath
            self.assertFalse(os.path.lexists(installed), relpath)
            self.assertIn(str(installed), res.stdout)
        self.assertFalse(os.path.lexists(self.prefix / "share" / "rcmake"))
        for relpath, text in neighbors.items():
            neighbor = self.prefix / relpath
            self.assertTrue(neighbor.is_file(), relpath)
            self.assertEqual(neighbor.read_text(encoding="utf-8"), text)

    def test_install_and_uninstall_without_completions_and_man_page(self) -> None:
        legacy = _make_fake_archive(
            self.archives,
            version="0.1.0",
            archive_name="rcmake-legacy-no-extras.tar.gz",
            lsp_mode="symlink",
            include_completions_and_man=False,
        )
        res = self.run_install("--archive", str(legacy))
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertTrue((self.prefix / "bin" / "rcmake").is_file())
        for relpath in EXTRA_SHARE_FILES:
            self.assertFalse(os.path.lexists(self.prefix / relpath), relpath)

        res = self.run_install("--uninstall")
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertFalse(os.path.lexists(self.prefix / "bin" / "rcmake"))
        self.assertFalse(os.path.lexists(self.prefix / "bin" / "rcmake-lsp"))
        self.assertFalse(os.path.lexists(self.prefix / "share" / "cmake-4.5"))

    def test_completions_and_man_reject_symlinks_and_uninstall_with_foreign_cmake(self) -> None:
        bad = _make_fake_archive(
            self.archives,
            archive_name="bad-extra-symlink.tar.gz",
            lsp_mode="symlink",
            symlink_extra_path="share/man/man1/rcmake.1",
        )
        res = self.run_install("--archive", str(bad))
        self.assertNotEqual(res.returncode, 0)
        self.assertIn("Invalid archive entry", res.stderr)
        self.assertFalse(os.path.lexists(self.prefix / "share" / "man" / "man1" / "rcmake.1"))

        archive = _make_fake_archive(
            self.archives,
            archive_name="with-extras-foreign-cmake.tar.gz",
            lsp_mode="symlink",
            include_completions_and_man=True,
        )
        res = self.run_install("--archive", str(archive))
        self.assertEqual(res.returncode, 0, res.stderr)

        foreign_cmake = self.prefix / "bin" / "cmake"
        foreign_cmake.write_text("#!/bin/sh\necho kitware-cmake\n", encoding="utf-8")
        custom_rcmake_note = self.prefix / "share" / "rcmake" / "custom.txt"
        custom_rcmake_note.write_text("keep me\n", encoding="utf-8")

        res = self.run_install("--uninstall")
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertTrue((self.prefix / "share" / "cmake-4.5" / "Modules" / "CMake.cmake").is_file())
        for relpath in EXTRA_SHARE_FILES:
            self.assertFalse(os.path.lexists(self.prefix / relpath), relpath)
        self.assertTrue(custom_rcmake_note.is_file())
        self.assertEqual(custom_rcmake_note.read_text(encoding="utf-8"), "keep me\n")


class InstallPs1Tests(unittest.TestCase):
    def test_spdx_flags_structure_and_safety(self) -> None:
        self.assertTrue(INSTALL_PS1.is_file(), f"Missing {INSTALL_PS1}")
        content = INSTALL_PS1.read_text(encoding="utf-8")
        lines = content.splitlines()
        self.assertGreater(len(lines), 50)
        self.assertEqual(lines[0], "# SPDX-License-Identifier: MIT OR Apache-2.0")
        self.assertFalse(content.startswith("#!"), "PowerShell script must not start with a hashbang")
        self.assertNotIn("\u2014", content, "Avoid em dashes in scripts/install.ps1")
        self.assertNotRegex(content, r"/(?:Users|home)/", "No developer home paths allowed")

        # Validate balanced PowerShell syntax delimiters.
        _check_powershell_delimiters(content)

        # Required parameters in param(...) block.
        param_match = re.search(r"param\s*\((.*?)\n\)", content, re.DOTALL)
        self.assertIsNotNone(param_match, "Expected top-level param(...) block")
        param_block = param_match.group(1)
        for required_param in (
            "$Version",
            "$Prefix",
            "$Archive",
            "$AliasCmake",
            "$Force",
            "$Uninstall",
            "$Help",
        ):
            self.assertIn(required_param, param_block)
        self.assertIn("'latest'", param_block)
        self.assertIn("$env:LOCALAPPDATA\\rcmake", param_block)

        # Required download, verification, extraction, PATH, and uninstall behaviors.
        for needle in (
            "https://github.com/dvgyl/rcmake/releases",
            "x86_64-pc-windows-msvc",
            "Get-FileHash -Algorithm SHA256",
            "System.IO.Compression.FileSystem",
            "[IO.Compression.ZipFile]::OpenRead",
            "$part -eq '..'",
            "bin/rcmake.exe",
            "bin/rcmake-lsp.exe",
            "share/cmake-4.5",
            "share/rcmake/completions/rcmake.ps1",
            "scripts/stage.sh",
            "--from-source",
            "Run again with -Force",
            "The installer changed nothing.",
            "[Environment]::SetEnvironmentVariable",
            "another CMake installation",
            "-UseBasicParsing",
            "Tls12",
        ):
            self.assertIn(needle, content, f"Expected {needle!r} in scripts/install.ps1")

        # Pre-check for alias collisions must occur before downloading or extracting.
        precheck_pos = content.index("Pre-check all alias collisions")
        download_pos = content.index("Invoke-Download -Uri")
        expand_pos = content.index("Expand-RcmakeArchive -ArchivePath")
        self.assertLess(precheck_pos, download_pos)
        self.assertLess(precheck_pos, expand_pos)

        # Stay compatible with Windows PowerShell 5.1 (.NET Framework).
        self.assertNotIn("GetRelativePath(", content)
        self.assertNotRegex(content, r"\[IO\.Path\]::GetFullPath\([^,)]+,")

    def test_pwsh_syntax_if_available(self) -> None:
        pwsh = shutil.which("pwsh")
        if pwsh is None:
            self.skipTest("pwsh is not installed")

        check_cmd = (
            "$tokens = $null; $errors = $null; "
            f"$ast = [System.Management.Automation.Language.Parser]::ParseFile('{INSTALL_PS1}', [ref]$tokens, [ref]$errors); "
            "if ($errors.Count -gt 0) { $errors | ForEach-Object { [Console]::Error.WriteLine($_.ToString()) }; exit 1 }; "
            "$names = @($ast.ParamBlock.Parameters | ForEach-Object { $_.Name.VariablePath.UserPath }); "
            "Write-Output ($names -join ',')"
        )
        result = subprocess.run(
            [pwsh, "-NoProfile", "-NonInteractive", "-Command", check_cmd],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        params = set(result.stdout.strip().split(","))
        self.assertTrue(
            {"Version", "Prefix", "Archive", "AliasCmake", "Force", "Uninstall", "Help"}.issubset(params),
            f"Unexpected parsed parameters: {params}",
        )


if __name__ == "__main__":
    unittest.main()
