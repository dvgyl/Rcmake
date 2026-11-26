# SPDX-License-Identifier: MIT OR Apache-2.0
"""Unit tests for verify_polish.py, shell completions, and the rcmake man page."""

from __future__ import annotations

import importlib.util
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import unittest

ROOT = Path(__file__).resolve().parents[2]
VERIFY_POLISH_PATH = ROOT / "conformance" / "cases" / "cli-ux" / "verify_polish.py"
COMPLETIONS_DIR = ROOT / "completions"
MAN_PAGE = ROOT / "docs" / "man" / "rcmake.1"


def _load_verify_polish():
    spec = importlib.util.spec_from_file_location("verify_polish", VERIFY_POLISH_PATH)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


vp = _load_verify_polish()


def _check_powershell_delimiters(source: str) -> None:
    """Verify balanced (), [], {} outside PowerShell comments and quoted strings."""
    stack: list[tuple[str, int]] = []
    closing = {")": "(", "]": "[", "}": "{"}
    for lineno, line in enumerate(source.splitlines(), 1):
        i = 0
        quote: str | None = None
        while i < len(line):
            ch = line[i]
            if quote is not None:
                if quote == '"' and ch == "`" and i + 1 < len(line):
                    i += 2
                    continue
                if quote == "'" and ch == "'" and i + 1 < len(line) and line[i + 1] == "'":
                    i += 2
                    continue
                if ch == quote:
                    quote = None
                i += 1
                continue
            if ch == "#":
                break
            if ch in ("'", '"'):
                quote = ch
            elif ch in ("(", "[", "{"):
                stack.append((ch, lineno))
            elif ch in closing:
                if not stack or stack[-1][0] != closing[ch]:
                    raise AssertionError(f"Unmatched {ch!r} on line {lineno}")
                stack.pop()
            i += 1
        if quote is not None:
            raise AssertionError(f"Unterminated {quote} string on line {lineno}")
    if stack:
        opened, lineno = stack[-1]
        raise AssertionError(f"Unclosed {opened!r} opened on line {lineno}")


class VerifyPolishUnitTests(unittest.TestCase):
    def test_hermetic_detects_prefixed_failures_and_passes_clean(self) -> None:
        bad_strict = vp.CommandCapture(
            returncode=1,
            stdout="",
            stderr="/proj/CMakeLists.txt:0: RCMAKE_HERMETIC_READ: Read outside allowed roots (/bin/cmake)\n",
        )
        bad_warn = vp.CommandCapture(
            returncode=0,
            stdout="",
            stderr=(
                "CMakeLists.txt:0: RCMAKE_HERMETIC_READ: /stage/share/cmake-4.5/Modules/CMake.cmake\n"
                "Additional hermetic violations omitted: 2005.\n"
            ),
        )
        failed = vp.eval_hermetic(bad_strict, bad_warn)
        self.assertFalse(failed.passed)
        self.assertEqual(len(failed.failures), 4)

        good_strict = vp.CommandCapture(returncode=0, stdout="-- Configuring done\n", stderr="")
        good_warn = vp.CommandCapture(
            returncode=0,
            stdout="-- Configuring done\n",
            stderr=(
                "CMakeLists.txt:3: RCMAKE_HERMETIC_ENV: Undeclared env RCMAKE_UX_POLISH_UNDECLARED_ENV\n"
                "CMakeLists.txt:4: RCMAKE_HERMETIC_READ: Read outside allowed roots (/etc/hosts)\n"
            ),
        )
        passed = vp.eval_hermetic(good_strict, good_warn)
        self.assertTrue(passed.passed, passed.failures)

    def test_ctest_help_eval(self) -> None:
        stub = vp.CommandCapture(returncode=0, stdout="Usage\n\n  ctest [options]\n", stderr="")
        self.assertFalse(vp.eval_ctest_help(stub).passed)

        full = vp.CommandCapture(
            returncode=0,
            stdout=(
                "Usage\n  ctest [options]\nOptions\n"
                "  --test-dir <dir>\n  -R <regex>\n  -E <regex>\n  -L <regex>\n"
                "  -j <jobs>\n  --output-on-failure\n  --preset <name>\n  --rerun-failed\n"
            ),
            stderr="",
        )
        self.assertTrue(vp.eval_ctest_help(full).passed)

    def test_lsp_cli_eval(self) -> None:
        hung = vp.CommandCapture(returncode=None, stdout="", stderr="", elapsed_seconds=2.01, timed_out=True)
        empty = vp.CommandCapture(returncode=0, stdout="", stderr="", elapsed_seconds=0.02, timed_out=False)
        self.assertFalse(vp.eval_lsp_cli(hung, empty).passed)

        good_help = vp.CommandCapture(
            returncode=0,
            stdout="Usage: rcmake-lsp [options]\n",
            stderr="",
            elapsed_seconds=0.05,
        )
        good_ver = vp.CommandCapture(
            returncode=0,
            stdout="rcmake-lsp 0.1.0\n",
            stderr="",
            elapsed_seconds=0.04,
        )
        self.assertTrue(vp.eval_lsp_cli(good_help, good_ver).passed)

    def test_explain_eval(self) -> None:
        bad_target = vp.CommandCapture(
            returncode=0,
            stdout=(
                "app\n  Partial configure\n"
                "    Compile definition  -DHELPER_DEF=1\n"
                "      app -> PRIVATE helper\n"
                "        app -> PRIVATE helper\n"
                "          At CMakeLists.txt:8 (target_link_libraries)\n"
                "          Called from CMakeLists.txt\n"
            ),
            stderr="Partial configure. Rcmake denied 5 subprocess requests.\n",
        )
        bad_filter = vp.CommandCapture(returncode=0, stdout='No match for "-O2" in app.\n', stderr="")
        bad_typo = vp.CommandCapture(returncode=1, stdout="No match for apq.\n", stderr="")
        bad_var = vp.CommandCapture(returncode=0, stdout="CMAKE_BUILD_TYPE\n  Source location unavailable.\n", stderr="")
        failed = vp.eval_explain(bad_target, bad_filter, bad_typo, bad_var, bad_var)
        self.assertFalse(failed.passed)
        self.assertEqual(len(failed.failures), 6)

        good_target = vp.CommandCapture(
            returncode=0,
            stdout=(
                "app\n"
                "  Compile definition  -DHELPER_DEF=1\n"
                "    app -> PRIVATE helper\n"
                "      At CMakeLists.txt:8 (target_link_libraries)\n"
            ),
            stderr="",
        )
        good_filter = vp.CommandCapture(
            returncode=0,
            stdout='-O2 <- CMAKE_CXX_FLAGS_RELEASE at CMakeLists.txt:4\n',
            stderr="",
        )
        good_typo = vp.CommandCapture(
            returncode=1,
            stdout='No target or variable named "apq". Did you mean: app (target)?\n',
            stderr="",
        )
        good_cli_var = vp.CommandCapture(
            returncode=0,
            stdout="CMAKE_BUILD_TYPE\n  Set on the command line: -DCMAKE_BUILD_TYPE=Release\n",
            stderr="",
        )
        good_cache_var = vp.CommandCapture(
            returncode=0,
            stdout="MY_CACHE_OPT\n  At CMakeLists.txt:3 (set CACHE)\n",
            stderr="",
        )
        passed = vp.eval_explain(good_target, good_filter, good_typo, good_cli_var, good_cache_var)
        self.assertTrue(passed.passed, passed.failures)

    def test_rich_diagnostics_eval(self) -> None:
        arrow = vp.GUTTER_ARROW
        bar = vp.GUTTER_BAR
        bad_paren = vp.CommandCapture(
            returncode=1,
            stdout="",
            stderr=f'error[CMake.FatalError]: Parse error.\n {arrow} CMakeLists.txt:3\n',
        )
        bad_str = vp.CommandCapture(
            returncode=1,
            stdout="",
            stderr=f'error[CMake.FatalError]: Parse error.\n {arrow} CMakeLists.txt:3\n\n  ".\n',
        )
        bad_gutter = vp.CommandCapture(
            returncode=1,
            stdout="",
            stderr=f"error[unknown-command]: bad\n {arrow} CMakeLists.txt:3:1\n  {bar}\n3 {bar} add_executabel()\n",
        )
        bad_target = vp.CommandCapture(
            returncode=1,
            stdout="",
            stderr='CMake Error: Target "app" links to: fmt::fmt\n',
        )
        bad_send = vp.CommandCapture(
            returncode=1,
            stdout="",
            stderr="error[CMake.FatalError]: non-fatal send error\n",
        )
        bad_policy = vp.CommandCapture(
            returncode=1,
            stdout="",
            stderr="error[CMake.FatalError]: Compatibility with CMake < 3.5 has been removed.\n",
        )
        failed = vp.eval_rich_diagnostics(bad_paren, bad_str, bad_gutter, bad_target, bad_send, bad_policy)
        self.assertFalse(failed.passed)
        self.assertEqual(len(failed.failures), 6)

        good_paren = vp.CommandCapture(
            returncode=1,
            stdout="",
            stderr=f"error[parse]: Missing )\n  {arrow} CMakeLists.txt:3:3\n  {bar}\n3 {bar} if(TRUE\n",
        )
        good_str = vp.CommandCapture(
            returncode=1,
            stdout="",
            stderr=f'error[parse]: Unterminated string\n  {arrow} CMakeLists.txt:3:16\n  {bar}\n3 {bar} message(STATUS "hello)\n',
        )
        good_gutter = vp.CommandCapture(
            returncode=1,
            stdout="",
            stderr=f"error[unknown-command]: bad\n  {arrow} CMakeLists.txt:3:1\n  {bar}\n3 {bar} add_executabel()\n",
        )
        good_target = vp.CommandCapture(
            returncode=1,
            stdout="",
            stderr=f"error[missing-target]: missing fmt::fmt\n  {arrow} CMakeLists.txt:4:1\n  = help: add find_package(fmt REQUIRED)\n",
        )
        good_send = vp.CommandCapture(
            returncode=1,
            stdout="",
            stderr="error[CMake.SendError]: non-fatal send error\n",
        )
        good_policy = vp.CommandCapture(
            returncode=1,
            stdout="",
            stderr="error[policy-version]: removed\n  = help: use a version range like 3.5...4.5 or -DCMAKE_POLICY_VERSION_MINIMUM=3.5\n",
        )
        passed = vp.eval_rich_diagnostics(good_paren, good_str, good_gutter, good_target, good_send, good_policy)
        self.assertTrue(passed.passed, passed.failures)

    def test_fetch_lock_eval(self) -> None:
        bad_update = vp.CommandCapture(returncode=0, stdout="-- Configuring done\n", stderr="")
        bad_frozen = vp.CommandCapture(
            returncode=1,
            stdout="",
            stderr=(
                "Frozen fetch blocked for dep. Source <source>. "
                "Set FETCHCONTENT_SOURCE_DIR_<NAME> to a pre-populated directory.\n"
            ),
        )
        failed = vp.eval_fetch_lock(bad_update, bad_frozen)
        self.assertFalse(failed.passed)
        self.assertEqual(len(failed.failures), 2)

        good_update = vp.CommandCapture(
            returncode=0,
            stdout="",
            stderr="dep: not pinned (custom download command); frozen mode will reject it\n",
        )
        good_frozen = vp.CommandCapture(
            returncode=1,
            stdout="",
            stderr=(
                "Frozen fetch blocked for dep. Set FETCHCONTENT_SOURCE_DIR_DEP to a pre-populated directory.\n"
            ),
        )
        passed = vp.eval_fetch_lock(good_update, good_frozen)
        self.assertTrue(passed.passed, passed.failures)

    def test_profile_eval(self) -> None:
        em = vp.EM_DASH
        el = vp.ELLIPSIS
        bad_cap = vp.CommandCapture(
            returncode=0,
            stdout="",
            stderr=(
                "Rcmake profile: 120 ms\n"
                f"          {em}           {em}          {em}        {em}  parse\n"
                f"          {em}           {em}          {em}        {em}  probes\n"
            ),
        )
        bad_html = (
            "<style>:root[data-theme=\"dark\"] { --bar: #aecfe8; --dim: #d3dce6; }"
            ".phase rect {} .listfile rect {} .macro rect {} .function rect {}</style>"
            "<header><h1>Rcmake profile</h1></header>"
            "<script>const capacity = Math.floor((width - 10) / 8);"
            f"text.textContent = chars.slice(0, head).join('') + '{el}' + suffix.join('');</script>"
        )
        failed = vp.eval_profile(bad_cap, predictable_html_exists=False, flamegraph_html=bad_html)
        self.assertFalse(failed.passed)
        self.assertEqual(len(failed.failures), 5)
        self.assertIn("missing color legend", " ".join(failed.failures))
        self.assertIn("missing wall-time header", " ".join(failed.failures))

        good_cap = vp.CommandCapture(
            returncode=0,
            stdout="",
            stderr="rcmake profile: 120 ms invocation wall time\neval  110 ms  10 ms  1\n",
        )
        good_html = (
            "<style>:root[data-theme=\"dark\"] { --bar: #aecfe8; --dim: #24303d; }</style>"
            "<header><h1>rcmake profile</h1><span id=\"wall-time\">120 ms</span></header>"
            "<div class=\"legend\"><span>phase</span><span>listfile</span><span>builtin</span></div>"
            "<script>const capacity = Math.floor((width - 10) / 6.5);"
            f"if (node.kind === 'listfile') text.textContent = chars.slice(0, head).join('') + '{el}' + suffix.join('');</script>"
        )
        passed = vp.eval_profile(good_cap, predictable_html_exists=True, flamegraph_html=good_html)
        self.assertTrue(passed.passed, passed.failures)

    def test_branding_eval(self) -> None:
        bad_help = vp.CommandCapture(returncode=0, stdout="Usage\n  cmake [options]\nRcmake extensions\n", stderr="")
        bad_prof = vp.CommandCapture(returncode=0, stdout="", stderr="Rcmake profile: 10 ms\n")
        bad_html = "<title>Rcmake profile</title><h1>Rcmake profile</h1>"
        bad_exp_help = vp.CommandCapture(returncode=0, stdout="rcmake explain\n", stderr="")
        bad_exp_run = vp.CommandCapture(returncode=0, stdout="", stderr="Rcmake denied 2 subprocess requests.\n")
        bad_missing = vp.CommandCapture(
            returncode=1,
            stdout="",
            stderr="The source directory does not exist.\nSpecify --help, or press the help button on the CMake GUI.\n",
        )
        failed = vp.eval_branding(bad_help, bad_prof, bad_html, bad_exp_help, bad_exp_run, bad_missing, bad_missing)
        self.assertFalse(failed.passed)
        self.assertEqual(len(failed.failures), 4)

        good_help = vp.CommandCapture(returncode=0, stdout="Usage\n  rcmake [options]\nrcmake extensions\n", stderr="")
        good_prof = vp.CommandCapture(returncode=0, stdout="", stderr="rcmake profile: 10 ms\n")
        good_html = "<title>rcmake profile</title><h1>rcmake profile</h1>"
        good_exp_run = vp.CommandCapture(returncode=0, stdout="app\n", stderr="")
        good_missing = vp.CommandCapture(
            returncode=1,
            stdout="",
            stderr="The source directory does not exist. Run 'rcmake --help' for usage.\n",
        )
        passed = vp.eval_branding(good_help, good_prof, good_html, bad_exp_help, good_exp_run, good_missing, good_missing)
        self.assertTrue(passed.passed, passed.failures)


class CompletionsAndManPageTests(unittest.TestCase):
    def test_verify_polish_is_executable_and_spdx(self) -> None:
        mode = VERIFY_POLISH_PATH.stat().st_mode
        self.assertTrue(bool(mode & stat.S_IXUSR), "verify_polish.py must be executable (100755)")
        text = VERIFY_POLISH_PATH.read_text(encoding="utf-8")
        self.assertTrue(text.startswith("#!/usr/bin/env python3\n"))
        self.assertIn("SPDX-License-Identifier: MIT OR Apache-2.0", text[:200])
        self.assertNotIn(vp.EM_DASH, text)

    def test_bash_completion_syntax_and_flags(self) -> None:
        bash_file = COMPLETIONS_DIR / "rcmake.bash"
        self.assertTrue(bash_file.is_file())
        text = bash_file.read_text(encoding="utf-8")
        self.assertIn("SPDX-License-Identifier: MIT OR Apache-2.0", text)
        self.assertNotIn(vp.EM_DASH, text)
        for token in (
            "--diagnostics=cmake",
            "--diagnostics=rich",
            "--fetch-lock=off",
            "--fetch-lock=auto",
            "--fetch-lock=update",
            "--fetch-lock=frozen",
            "--hermetic=warn",
            "--hermetic=strict",
            "--no-probe-cache",
            "--profile",
            "explain",
        ):
            self.assertIn(token, text)
        # Ensure zero hallucinated flags from CMake or outdated docs.
        for bogus in ("--diagnostics=plain", "--diagnostics=auto", "--cache-dir"):
            self.assertNotIn(bogus, text)

        bash = shutil.which("bash")
        if bash is not None:
            res = subprocess.run([bash, "-n", str(bash_file)], capture_output=True, text=True, check=False)
            self.assertEqual(res.returncode, 0, res.stderr)
            # Functional completion check for both unsplit and COMP_WORDBREAKS-split '=' forms.
            probe = (
                f'source "{bash_file.as_posix()}"\n'
                'COMP_WORDS=(rcmake --diagnostics=); COMP_CWORD=1; _rcmake_completions; echo "A:${COMPREPLY[*]}"\n'
                'COMP_WORDS=(rcmake --diagnostics = ""); COMP_CWORD=3; _rcmake_completions; echo "B:${COMPREPLY[*]}"\n'
                'COMP_WORDS=(rcmake --fetch-lock = f); COMP_CWORD=3; _rcmake_completions; echo "C:${COMPREPLY[*]}"\n'
                'COMP_WORDS=(rcmake explain app --color = ""); COMP_CWORD=5; _rcmake_completions; echo "D:${COMPREPLY[*]}"\n'
            )
            func_res = subprocess.run([bash, "-c", probe], capture_output=True, text=True, check=False)
            self.assertEqual(func_res.returncode, 0, func_res.stderr)
            self.assertIn("A:--diagnostics=cmake --diagnostics=rich", func_res.stdout)
            self.assertIn("B:cmake rich", func_res.stdout)
            self.assertIn("C:frozen", func_res.stdout)
            self.assertIn("D:auto always never", func_res.stdout)

    def test_zsh_completion_syntax_and_flags(self) -> None:
        zsh_file = COMPLETIONS_DIR / "_rcmake"
        self.assertTrue(zsh_file.is_file())
        text = zsh_file.read_text(encoding="utf-8")
        self.assertTrue(text.startswith("#compdef rcmake\n"))
        self.assertIn("SPDX-License-Identifier: MIT OR Apache-2.0", text)
        self.assertNotIn(vp.EM_DASH, text)
        for token in (
            "--diagnostics=-",
            "(cmake rich)",
            "--fetch-lock=-",
            "(off auto update frozen)",
            "--hermetic=-",
            "(warn strict)",
            "--no-probe-cache",
            "--profile=-",
            "_rcmake_explain",
        ):
            self.assertIn(token, text)
        for bogus in ("--diagnostics=plain", "--diagnostics=auto", "--cache-dir"):
            self.assertNotIn(bogus, text)

        zsh = shutil.which("zsh")
        if zsh is not None:
            res = subprocess.run([zsh, "-n", str(zsh_file)], capture_output=True, text=True, check=False)
            self.assertEqual(res.returncode, 0, res.stderr)

    def test_fish_and_powershell_completions(self) -> None:
        fish_file = COMPLETIONS_DIR / "rcmake.fish"
        ps1_file = COMPLETIONS_DIR / "rcmake.ps1"
        readme_file = COMPLETIONS_DIR / "README.md"
        for path in (fish_file, ps1_file, readme_file):
            self.assertTrue(path.is_file(), f"Missing {path}")
            content = path.read_text(encoding="utf-8")
            self.assertIn("SPDX-License-Identifier: MIT OR Apache-2.0", content)
            self.assertNotIn(vp.EM_DASH, content)
            for bogus in ("--diagnostics=plain", "--diagnostics=auto", "--cache-dir"):
                self.assertNotIn(bogus, content)

        fish_text = fish_file.read_text(encoding="utf-8")
        self.assertIn("-l diagnostics", fish_text)
        self.assertIn("-l fetch-lock", fish_text)
        self.assertIn("-l hermetic", fish_text)
        self.assertIn("-l no-probe-cache", fish_text)
        self.assertIn("__rcmake_in_mode explain", fish_text)

        ps1_text = ps1_file.read_text(encoding="utf-8")
        self.assertIn("Register-ArgumentCompleter -Native -CommandName rcmake", ps1_text)
        _check_powershell_delimiters(ps1_text)

    def test_man_page_lint_and_sections(self) -> None:
        self.assertTrue(MAN_PAGE.is_file())
        text = MAN_PAGE.read_text(encoding="utf-8")
        self.assertIn("SPDX-License-Identifier: MIT OR Apache-2.0", text)
        self.assertNotIn(vp.EM_DASH, text)
        for section in (
            ".SH NAME",
            ".SH SYNOPSIS",
            ".SH DESCRIPTION",
            ".SH NATIVE EXTENSIONS",
            ".SH COMPANION TOOLS",
            ".SH ENVIRONMENT",
            ".SH EXIT STATUS",
            ".SH FILES",
        ):
            self.assertIn(section, text)
        for topic in (
            "rcmake explain",
            "diagnostics=cmake",
            "hermetic",
            "fetch-lock=off",
            "no-probe-cache",
            "RCMAKE_CACHE_DIR",
            "ctest",
            "cpack",
            "rcmake-lsp",
        ):
            self.assertIn(topic, text)
        for bogus in ("--diagnostics=plain", "--diagnostics=auto", "--cache-dir", "\\-\\-cache-dir"):
            self.assertNotIn(bogus, text)

        mandoc = shutil.which("mandoc")
        if mandoc is not None:
            res = subprocess.run([mandoc, "-T", "lint", str(MAN_PAGE)], capture_output=True, text=True, check=False)
            self.assertEqual(res.returncode, 0, res.stderr + res.stdout)
            self.assertEqual(res.stderr.strip(), "", f"mandoc lint warnings: {res.stderr}")


if __name__ == "__main__":
    unittest.main()
