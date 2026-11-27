#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Live PTY differential oracle harness for `ccmake`.

Drives CMake 4.5.1's `ccmake` and Rcmake's `ccmake` symlink inside real POSIX
pseudo-terminals, comparing normalized `CMakeCache.txt` outputs, VT100-parsed
status-line text, configure-vs-generate phase separation, child
`execute_process()` stdio capture & non-interactive stdin isolation, and
restored PTY `termios` flags across normal, small-window, error, and Ctrl-C
exits.
"""

from __future__ import annotations

import argparse
import difflib
import fcntl
import os
from pathlib import Path
import pty
import select
import shutil
import struct
import subprocess
import sys
import tempfile
import termios
import time

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT / "tools" / "corpus"))
import normalize as corpus_norm  # noqa: E402

CASE_TIMEOUT_SECS = 20.0


class VtScreen:
    """Minimal VT100 screen tracker to verify rendered rows and status lines."""

    def __init__(self, cols: int, rows: int) -> None:
        self.cols = cols
        self.rows = rows
        self.grid = [[" "] * cols for _ in range(rows)]
        self.cur_row = 0
        self.cur_col = 0
        self.last_ch = " "
        self.state = "ground"
        self.params = bytearray()

    def _put_char(self, ch: str) -> None:
        if 0 <= self.cur_row < self.rows and 0 <= self.cur_col < self.cols:
            self.grid[self.cur_row][self.cur_col] = ch
            self.cur_col += 1
        self.last_ch = ch

    def feed(self, data: bytes) -> None:
        for b in data:
            if self.state == "ground":
                if b == 0x1B:
                    self.state = "esc"
                    self.params.clear()
                elif b == 0x0D:
                    self.cur_col = 0
                elif b == 0x0A:
                    if self.cur_row + 1 < self.rows:
                        self.cur_row += 1
                elif 0x20 <= b <= 0x7E:
                    self._put_char(chr(b))
            elif self.state == "esc":
                if b == ord("["):
                    self.state = "csi"
                    self.params.clear()
                elif b == ord("("):
                    self.state = "charset"
                else:
                    self.state = "ground"
            elif self.state == "charset":
                self.state = "ground"
            elif self.state == "csi":
                if 0x40 <= b <= 0x7E:
                    self._exec_csi(chr(b))
                    self.state = "ground"
                    self.params.clear()
                elif len(self.params) < 64:
                    self.params.append(b)

    def _exec_csi(self, final: str) -> None:
        raw = self.params.decode("ascii", errors="ignore")
        if raw.startswith("?"):
            return
        if final in ("H", "f"):
            parts = raw.split(";") if raw else []
            row = int(parts[0]) - 1 if len(parts) >= 1 and parts[0].isdigit() else 0
            col = int(parts[1]) - 1 if len(parts) >= 2 and parts[1].isdigit() else 0
            self.cur_row = max(0, min(self.rows - 1, row))
            self.cur_col = max(0, min(self.cols - 1, col))
        elif final == "J":
            if raw in ("", "2"):
                self.grid = [[" "] * self.cols for _ in range(self.rows)]
        elif final == "K":
            if 0 <= self.cur_row < self.rows:
                for c in range(self.cur_col, self.cols):
                    self.grid[self.cur_row][c] = " "
        elif final == "A":
            n = int(raw) if raw.isdigit() else 1
            self.cur_row = max(0, self.cur_row - n)
        elif final == "B":
            n = int(raw) if raw.isdigit() else 1
            self.cur_row = min(self.rows - 1, self.cur_row + n)
        elif final == "C":
            n = int(raw) if raw.isdigit() else 1
            self.cur_col = min(self.cols - 1, self.cur_col + n)
        elif final == "D":
            n = int(raw) if raw.isdigit() else 1
            self.cur_col = max(0, self.cur_col - n)
        elif final == "G":
            col = int(raw) - 1 if raw.isdigit() else 0
            self.cur_col = max(0, min(self.cols - 1, col))
        elif final == "d":
            row = int(raw) - 1 if raw.isdigit() else 0
            self.cur_row = max(0, min(self.rows - 1, row))
        elif final == "b":
            n = int(raw) if raw.isdigit() else 1
            for _ in range(min(n, self.cols)):
                self._put_char(self.last_ch)
        elif final == "X":
            n = int(raw) if raw.isdigit() else 1
            if 0 <= self.cur_row < self.rows:
                for c in range(self.cur_col, min(self.cols, self.cur_col + n)):
                    self.grid[self.cur_row][c] = " "

    def row(self, idx: int) -> str:
        if 0 <= idx < self.rows:
            return "".join(self.grid[idx])
        return ""

    def text(self) -> str:
        return "\n".join("".join(r).rstrip() for r in self.grid)


class PtySession:
    """Managed PTY child process with window size, output drain, and termios checks."""

    def __init__(
        self,
        argv: list[str],
        *,
        cwd: Path,
        env: dict[str, str],
        cols: int = 80,
        rows: int = 24,
    ) -> None:
        self.cols = cols
        self.rows = rows
        self.screen = VtScreen(cols, rows)
        self.raw_output = bytearray()
        self.master, self.slave = pty.openpty()
        fcntl.ioctl(
            self.slave,
            termios.TIOCSWINSZ,
            struct.pack("HHHH", rows, cols, 0, 0),
        )
        self.initial_lflag = termios.tcgetattr(self.slave)[3]
        self.proc = subprocess.Popen(
            argv,
            cwd=str(cwd),
            env=env,
            stdin=self.slave,
            stdout=self.slave,
            stderr=self.slave,
            close_fds=True,
        )

    def drain(self, duration: float = 0.4, wait_for: str | None = None) -> None:
        deadline = time.monotonic() + duration
        while time.monotonic() < deadline:
            remaining = max(0.01, deadline - time.monotonic())
            ready, _, _ = select.select([self.master], [], [], min(0.05, remaining))
            if ready:
                try:
                    chunk = os.read(self.master, 65536)
                except OSError:
                    break
                if not chunk:
                    break
                self.raw_output.extend(chunk)
                self.screen.feed(chunk)
                if wait_for is not None and wait_for in self.screen.text():
                    # Allow one short settling read so the full frame finishes writing.
                    deadline = min(deadline, time.monotonic() + 0.08)
            elif self.proc.poll() is not None:
                break

    def send(self, data: bytes) -> None:
        os.write(self.master, data)

    def wait_exit(self, timeout: float = CASE_TIMEOUT_SECS) -> tuple[int, int]:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.drain(0.1)
            code = self.proc.poll()
            if code is not None:
                self.drain(0.1)
                final_lflag = termios.tcgetattr(self.slave)[3]
                self.close()
                return code, final_lflag
        self.proc.kill()
        self.proc.wait(timeout=5)
        final_lflag = termios.tcgetattr(self.slave)[3]
        self.close()
        raise TimeoutError(
            f"ccmake process timed out after {timeout}s.\nLast screen:\n{self.screen.text()}"
        )

    def close(self) -> None:
        for attr in ("master", "slave"):
            fd = getattr(self, attr, None)
            if fd is not None:
                try:
                    os.close(fd)
                except OSError:
                    pass
                setattr(self, attr, None)


def assert_termios_restored(label: str, initial_lflag: int, final_lflag: int) -> None:
    for name, bit in (("ECHO", termios.ECHO), ("ICANON", termios.ICANON)):
        if bool(initial_lflag & bit) != bool(final_lflag & bit):
            raise AssertionError(
                f"{label}: PTY termios flag {name} was NOT restored "
                f"(initial={initial_lflag:#x}, final={final_lflag:#x})"
            )


def normalize_cache_for_diff(
    cache_path: Path,
    *,
    src: Path,
    bld: Path,
    tools: dict[str, set[str]],
) -> list[str]:
    raw = cache_path.read_bytes()
    norm_bytes, _ = corpus_norm._normalize_cache(raw, tools, None, omit_editor=True)
    text = norm_bytes.decode("utf-8", errors="replace")
    for orig, repl in (
        (str(bld.resolve()), "<build>"),
        (str(bld), "<build>"),
        (str(src.resolve()), "<src>"),
        (str(src), "<src>"),
    ):
        text = text.replace(orig, repl)
    # Upstream CMake 4.5.1 CLI `cmake` writes empty `CMAKE_SYSTEM_ENVIRONMENT_ID:INTERNAL=`
    # in `cmake::Run()`, whereas `cmCursesMainForm::Configure()` skips `cmake::Run()`.
    # Rcmake runs the unified configure session for both `cmake` and `ccmake`.
    return [
        line
        for line in text.splitlines()
        if line != "CMAKE_SYSTEM_ENVIRONMENT_ID:INTERNAL="
    ]


def make_rcmake_bin_dir(rcmake_bin: Path, tmp_root: Path) -> dict[str, Path]:
    bin_dir = tmp_root / "rcmake-bin"
    bin_dir.mkdir(parents=True, exist_ok=True)
    target = bin_dir / "rcmake"
    try:
        os.link(rcmake_bin.resolve(), target)
    except OSError:
        shutil.copy2(rcmake_bin.resolve(), target)
    paths = {"rcmake": target}
    for name in ("cmake", "ctest", "cpack", "ccmake"):
        link = bin_dir / name
        link.symlink_to("rcmake")
        paths[name] = link
    return paths


def base_env(extra: dict[str, str] | None = None) -> dict[str, str]:
    env = os.environ.copy()
    env["TERM"] = "xterm-256color"
    env.pop("RCMAKE_EXEC_LOG", None)
    if extra:
        env.update(extra)
    return env


def run_case_parity(oracle_ccmake: Path, rcmake_tools: dict[str, Path], tmp_root: Path) -> None:
    print("[case 1] Oracle differential parity (c -> edit -> c -> g + phase check + termios)...")
    cmakelists = """\
cmake_minimum_required(VERSION 4.5)
project(CcmakeDiff NONE)
option(OPT_A "Feature A" OFF)
option(OPT_B "Feature B" ON)
set(MY_MODE "Alpha" CACHE STRING "Selected mode")
set_property(CACHE MY_MODE PROPERTY STRINGS "Alpha" "Beta" "Gamma")
set(COUNTER_FILE "${CMAKE_CURRENT_BINARY_DIR}/configure_count.txt")
if(EXISTS "${COUNTER_FILE}")
  file(READ "${COUNTER_FILE}" _cnt)
  math(EXPR _cnt "${_cnt} + 1")
else()
  set(_cnt 1)
endif()
file(WRITE "${COUNTER_FILE}" "${_cnt}")
"""
    results: dict[str, tuple[list[str], list[str], int]] = {}
    oracle_bin = oracle_ccmake.parent

    for label, ccmake_exe, tool_map in (
        (
            "oracle",
            oracle_ccmake,
            {
                "cmake": {str(oracle_bin / "cmake"), str((oracle_bin / "cmake").resolve())},
                "ctest": {str(oracle_bin / "ctest"), str((oracle_bin / "ctest").resolve())},
                "cpack": {str(oracle_bin / "cpack"), str((oracle_bin / "cpack").resolve())},
            },
        ),
        (
            "rcmake",
            rcmake_tools["ccmake"],
            {
                "cmake": {
                    str(rcmake_tools["cmake"]),
                    str(rcmake_tools["cmake"].resolve()),
                    str(rcmake_tools["rcmake"]),
                    str(rcmake_tools["rcmake"].resolve()),
                },
                "ctest": {
                    str(rcmake_tools["ctest"]),
                    str(rcmake_tools["ctest"].resolve()),
                },
                "cpack": {
                    str(rcmake_tools["cpack"]),
                    str(rcmake_tools["cpack"].resolve()),
                },
            },
        ),
    ):
        case_dir = tmp_root / f"case1-{label}"
        src = case_dir / "src"
        bld = case_dir / "bld"
        src.mkdir(parents=True)
        bld.mkdir(parents=True)
        (src / "CMakeLists.txt").write_text(cmakelists, encoding="utf-8")

        env = base_env(
            {"RCMAKE_VENDORED_DATA_ROOT": str(REPO_ROOT / "modules" / "cmake-4.5")}
            if label == "rcmake"
            else None
        )
        sess = PtySession(
            [str(ccmake_exe), "-S", str(src), "-B", str(bld), "-G", "Ninja"],
            cwd=case_dir,
            env=env,
        )
        try:
            sess.drain(1.5, wait_for="EMPTY CACHE")
            # 1. First configure: must populate cache WITHOUT writing build.ninja.
            sess.send(b"c")
            sess.drain(3.0, wait_for="OPT_A")
            if (bld / "build.ninja").exists():
                raise AssertionError(f"{label}: build.ninja must NOT exist after first `c`")
            if not (bld / "CMakeCache.txt").is_file():
                raise AssertionError(f"{label}: CMakeCache.txt missing after first `c`")

            # 2. Search and toggle OPT_A (OFF -> ON), then cycle MY_MODE (Alpha -> Beta).
            sess.send(b"/OPT_A\n")
            sess.drain(0.6, wait_for="OPT_A:")
            sess.send(b"\r")
            sess.drain(0.5)
            sess.send(b"/MY_MODE\n")
            sess.drain(0.6, wait_for="MY_MODE:")
            sess.send(b"\r")
            sess.drain(0.5)

            # 3. Second configure: no new entries -> `[g] Generate` appears, STILL no build.ninja!
            sess.send(b"c")
            sess.drain(3.0, wait_for="[g] Generate")
            if (bld / "build.ninja").exists():
                raise AssertionError(f"{label}: build.ninja must NOT exist after second `c`")
            count_before_g = int((bld / "configure_count.txt").read_text(encoding="utf-8").strip())
            if count_before_g != 2:
                raise AssertionError(f"{label}: expected 2 configures before `g`, got {count_before_g}")

            bottom_four = [sess.screen.row(r).rstrip() for r in range(20, 24)]

            # 4. Generate (`g`): writes build.ninja WITHOUT re-running configure!
            sess.send(b"g")
            code, final_lflag = sess.wait_exit(CASE_TIMEOUT_SECS)
        except Exception:
            sess.close()
            raise

        if code != 0:
            raise AssertionError(f"{label}: expected exit 0 after `g`, got {code}")
        assert_termios_restored(f"case1-{label}", sess.initial_lflag, final_lflag)
        if not (bld / "build.ninja").is_file():
            raise AssertionError(f"{label}: build.ninja missing after `g`")
        count_after_g = int((bld / "configure_count.txt").read_text(encoding="utf-8").strip())
        if count_after_g != count_before_g:
            raise AssertionError(
                f"{label}: `g` re-executed configure (count {count_before_g} -> {count_after_g})"
            )

        if label == "rcmake":
            raw_cache = (bld / "CMakeCache.txt").read_text(encoding="utf-8", errors="replace")
            expected_edit = f"CMAKE_EDIT_COMMAND:INTERNAL={rcmake_tools['ccmake'].parent.resolve() / 'ccmake'}"
            if expected_edit not in raw_cache:
                raise AssertionError(
                    f"rcmake: expected {expected_edit!r} in CMakeCache.txt"
                )

        norm_cache = normalize_cache_for_diff(
            bld / "CMakeCache.txt", src=src, bld=bld, tools=tool_map
        )
        results[label] = (norm_cache, bottom_four, count_after_g)

    oracle_cache, oracle_status, _ = results["oracle"]
    rcmake_cache, rcmake_status, _ = results["rcmake"]

    if oracle_cache != rcmake_cache:
        diff = "\n".join(
            difflib.unified_diff(
                oracle_cache, rcmake_cache, fromfile="oracle CMakeCache.txt", tofile="rcmake CMakeCache.txt"
            )
        )
        raise AssertionError(f"Normalized CMakeCache.txt differs from oracle:\n{diff}")

    # Compare the 4 bottom status/key rows.
    for row_idx, (o_row, r_row) in enumerate(zip(oracle_status, rcmake_status)):
        if o_row != r_row:
            raise AssertionError(
                f"Status row {row_idx} differs from oracle:\n  oracle: {o_row!r}\n  rcmake: {r_row!r}"
            )
    print("  PASS: CMakeCache.txt, bottom 4 status rows, phase separation, and termios match oracle!")


def run_case_child_stdio_isolation(rcmake_tools: dict[str, Path], tmp_root: Path) -> None:
    print("[case 2] Child execute_process() inherited stdio capture & non-interactive stdin...")
    case_dir = tmp_root / "case2-child-stdio"
    src = case_dir / "src"
    bld = case_dir / "bld"
    src.mkdir(parents=True)
    bld.mkdir(parents=True)
    stolen_marker = bld / "stolen_stdin.txt"
    (src / "CMakeLists.txt").write_text(
        f"""\
cmake_minimum_required(VERSION 4.5)
project(ChildStdio NONE)
execute_process(
  COMMAND sh -c "echo CHILD_STDOUT_MARKER_42; echo CHILD_STDERR_MARKER_99 >&2; if IFS= read -r line; then printf '%s' \\"$line\\" > '{stolen_marker}'; fi"
)
""",
        encoding="utf-8",
    )
    env = base_env({"RCMAKE_VENDORED_DATA_ROOT": str(REPO_ROOT / "modules" / "cmake-4.5")})
    sess = PtySession(
        [str(rcmake_tools["ccmake"]), "-S", str(src), "-B", str(bld), "-G", "Ninja"],
        cwd=case_dir,
        env=env,
    )
    try:
        sess.drain(1.5, wait_for="EMPTY CACHE")
        sess.send(b"c")
        sess.drain(3.0, wait_for="Press [e] to exit screen")
        log_screen = sess.screen.text()
        if "CHILD_STDOUT_MARKER_42" not in log_screen:
            raise AssertionError(f"Missing child stdout marker in TUI log:\n{log_screen}")
        if "CHILD_STDERR_MARKER_99" not in log_screen:
            raise AssertionError(f"Missing child stderr marker in TUI log:\n{log_screen}")
        if stolen_marker.exists():
            raise AssertionError("Child process stole stdin from TUI!")
        sess.send(b"e")
        sess.drain(1.0, wait_for="Keys: [enter]")
        browse_screen = sess.screen.text()
        if "Keys: [enter] Edit an entry [d] Delete an entry" not in browse_screen:
            raise AssertionError(f"Browse screen corrupted after dismissing log:\n{browse_screen}")
        sess.send(b"q")
        code, final_lflag = sess.wait_exit(CASE_TIMEOUT_SECS)
    except Exception:
        sess.close()
        raise

    if code != 0:
        raise AssertionError(f"case 2: expected exit 0, got {code}")
    assert_termios_restored("case2-child-stdio", sess.initial_lflag, final_lflag)
    print("  PASS: child stdout & stderr captured in TUI log, stdin isolated, termios restored!")


def run_case_terminal_restoration(rcmake_tools: dict[str, Path], tmp_root: Path) -> None:
    print("[case 3] Terminal restoration on small-window, configure error, and Ctrl-C...")
    env = base_env({"RCMAKE_VENDORED_DATA_ROOT": str(REPO_ROOT / "modules" / "cmake-4.5")})

    # 3a: Small window (60 x 20)
    case_dir = tmp_root / "case3-small"
    case_dir.mkdir(parents=True)
    sess = PtySession(
        [str(rcmake_tools["ccmake"]), str(case_dir)],
        cwd=case_dir,
        env=env,
        cols=60,
        rows=20,
    )
    code, final_lflag = sess.wait_exit(CASE_TIMEOUT_SECS)
    output_text = sess.raw_output.decode("utf-8", errors="replace")
    if code != 1 or "Window is too small. A size of at least 65 x 6 is required to run ccmake." not in output_text:
        raise AssertionError(f"case 3a: unexpected code={code} output={output_text!r}")
    assert_termios_restored("case3a-small-window", sess.initial_lflag, final_lflag)

    # 3b: Configure error -> log view -> dismiss -> quit
    err_dir = tmp_root / "case3-error"
    src = err_dir / "src"
    bld = err_dir / "bld"
    src.mkdir(parents=True)
    bld.mkdir(parents=True)
    (src / "CMakeLists.txt").write_text(
        'cmake_minimum_required(VERSION 4.5)\nproject(Broken NONE)\nmessage(FATAL_ERROR "BROKEN_CONFIG_TEST")\n',
        encoding="utf-8",
    )
    sess_err = PtySession(
        [str(rcmake_tools["ccmake"]), "-S", str(src), "-B", str(bld), "-G", "Ninja"],
        cwd=err_dir,
        env=env,
    )
    try:
        sess_err.drain(1.5, wait_for="EMPTY CACHE")
        sess_err.send(b"c")
        sess_err.drain(3.0, wait_for="BROKEN_CONFIG_TEST")
        if "BROKEN_CONFIG_TEST" not in sess_err.screen.text():
            raise AssertionError(f"case 3b: missing error marker:\n{sess_err.screen.text()}")
        sess_err.send(b"e")
        sess_err.drain(1.0, wait_for="Keys: [enter]")
        # `[g] Generate` must remain hidden after configure error.
        if "[g] Generate" in sess_err.screen.text():
            raise AssertionError("case 3b: [g] Generate must stay hidden after configure failure")
        sess_err.send(b"q")
        code, final_lflag = sess_err.wait_exit(CASE_TIMEOUT_SECS)
    except Exception:
        sess_err.close()
        raise
    if code != 0:
        raise AssertionError(f"case 3b: expected exit 0 on q, got {code}")
    assert_termios_restored("case3b-configure-error", sess_err.initial_lflag, final_lflag)

    # 3c: Ctrl-C exits 0 and restores termios
    sess_cc = PtySession(
        [str(rcmake_tools["ccmake"]), "-S", str(src), "-B", str(bld), "-G", "Ninja"],
        cwd=err_dir,
        env=env,
    )
    try:
        sess_cc.drain(1.5, wait_for="Keys: [enter]")
        sess_cc.send(b"\x03")
        code, final_lflag = sess_cc.wait_exit(CASE_TIMEOUT_SECS)
    except Exception:
        sess_cc.close()
        raise
    if code != 0:
        raise AssertionError(f"case 3c: expected exit 0 on Ctrl-C, got {code}")
    assert_termios_restored("case3c-ctrl-c", sess_cc.initial_lflag, final_lflag)
    print("  PASS: small-window, configure error, and Ctrl-C all exit cleanly and restore termios!")


def run_case_non_tty_and_cli(rcmake_tools: dict[str, Path], tmp_root: Path) -> None:
    print("[case 4] Non-TTY failure and pipe-safe --version/--help/no-args...")
    env = base_env({"RCMAKE_VENDORED_DATA_ROOT": str(REPO_ROOT / "modules" / "cmake-4.5")})
    ccmake = str(rcmake_tools["ccmake"])

    ver = subprocess.run([ccmake, "--version"], env=env, capture_output=True, text=True, timeout=10)
    if ver.returncode != 0 or not ver.stdout.startswith("ccmake version 4.5.1\nrcmake "):
        raise AssertionError(f"unexpected --version: rc={ver.returncode} out={ver.stdout!r}")

    no_arg = subprocess.run([ccmake], env=env, capture_output=True, text=True, timeout=10)
    if no_arg.returncode != 0 or "Run 'ccmake --help' for more information." not in no_arg.stdout:
        raise AssertionError(f"unexpected no-arg usage: rc={no_arg.returncode} out={no_arg.stdout!r}")

    non_tty = subprocess.run(
        [ccmake, str(tmp_root)],
        env=env,
        stdin=subprocess.DEVNULL,
        capture_output=True,
        text=True,
        timeout=10,
    )
    if non_tty.returncode != 1 or non_tty.stderr != "Error: ncurses initialization failed\n":
        raise AssertionError(
            f"unexpected non-tty result: rc={non_tty.returncode} out={non_tty.stdout!r} err={non_tty.stderr!r}"
        )
    print("  PASS: non-TTY guard and pipe-safe CLI flags verified!")


def main() -> int:
    parser = argparse.ArgumentParser(description="Live PTY differential harness for ccmake.")
    parser.add_argument(
        "--rcmake",
        type=Path,
        default=REPO_ROOT / "target" / "debug" / "rcmake",
        help="Path to built rcmake binary.",
    )
    parser.add_argument(
        "--oracle-ccmake",
        type=Path,
        default=None,
        help="Path to upstream CMake 4.5.1 ccmake binary (defaults to sibling of $RCMAKE_ORACLE_CMAKE).",
    )
    args = parser.parse_args()

    rcmake_bin = args.rcmake.resolve()
    if not rcmake_bin.is_file():
        sys.exit(f"rcmake binary not found: {rcmake_bin}")

    oracle_ccmake = args.oracle_ccmake
    if oracle_ccmake is None:
        oracle_cmake = os.environ.get("RCMAKE_ORACLE_CMAKE")
        if not oracle_cmake:
            sys.exit("Set RCMAKE_ORACLE_CMAKE (source env/env.sh) or pass --oracle-ccmake.")
        oracle_ccmake = Path(oracle_cmake).parent / "ccmake"
    oracle_ccmake = oracle_ccmake.resolve()
    if not oracle_ccmake.is_file():
        sys.exit(f"oracle ccmake binary not found: {oracle_ccmake}")

    if shutil.which("ninja") is None:
        sys.exit("ninja must be on PATH (source env/env.sh).")

    with tempfile.TemporaryDirectory(prefix="rcmake-ccmake-pty-") as tmp:
        tmp_root = Path(tmp).resolve()
        rcmake_tools = make_rcmake_bin_dir(rcmake_bin, tmp_root)
        run_case_parity(oracle_ccmake, rcmake_tools, tmp_root)
        run_case_child_stdio_isolation(rcmake_tools, tmp_root)
        run_case_terminal_restoration(rcmake_tools, tmp_root)
        run_case_non_tty_and_cli(rcmake_tools, tmp_root)

    print("ALL PTY DIFFERENTIAL CASES PASSED.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
