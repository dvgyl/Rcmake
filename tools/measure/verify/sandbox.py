# SPDX-License-Identifier: MIT OR Apache-2.0
"""Sandbox, toolset discovery, and subprocess primitives for verification."""

from __future__ import annotations

import atexit
from dataclasses import dataclass
import os
from pathlib import Path
import re
import shutil
import signal
import sys
import tempfile
from typing import Any, Mapping, Sequence

try:
    from .._proc import kill_group, run_process as _run_process
except ImportError:
    from _proc import kill_group, run_process as _run_process


DARWIN = sys.platform == "darwin"
ROOT = Path(__file__).resolve().parents[3]
TOOLSET_DIRS = ("oracle", "rcmake")  # Equal length keeps path budgets equal.
DEFAULT_TIMEOUT = 600.0
SCRUBBED_ENV = re.compile(
    r"^(CMAKE_[A-Z_]+|CTEST_[A-Z_]+|CPACK_[A-Z_]+|MAKEFLAGS|MFLAGS|MAKELEVEL|"
    r"NINJA_STATUS|DESTDIR|CC|CXX|OBJC|OBJCXX|FC|CFLAGS|CXXFLAGS|OBJCFLAGS|"
    r"OBJCXXFLAGS|FFLAGS|LDFLAGS|CPATH|LIBRARY_PATH|VERBOSE)$"
)
RSS_WRAPPER = (
    "import resource, subprocess, sys\n"
    "code = subprocess.run(sys.argv[1:]).returncode\n"
    "peak = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss\n"
    "scale = 1 if sys.platform == 'darwin' else 1024\n"
    "print('INDEPENDENT_VERIFY_PEAK_RSS', peak * scale, file=sys.stderr)\n"
    "sys.exit(code)\n"
)
MIB = 1024 * 1024


class ScenarioSkip(Exception):
    """Raised when a scenario cannot run on this host."""


class StepFailed(Exception):
    """Raised when a scenario cannot continue after a failed check."""


@dataclass(frozen=True)
class Toolset:
    """One CMake implementation: the cmake, ctest, and cpack programs."""

    label: str
    cmake: Path
    ctest: Path
    cpack: Path

    @classmethod
    def from_cmake(cls, label: str, cmake: Path) -> "Toolset":
        """Use the ctest and cpack programs next to a cmake program."""
        cmake = Path(cmake).absolute()
        return cls(label, cmake, cmake.with_name("ctest"), cmake.with_name("cpack"))

    @classmethod
    def from_stage(cls, stage: Path) -> "Toolset":
        """Use the bin/ programs of a staged Rcmake tree."""
        bindir = Path(stage).absolute() / "bin"
        return cls("rcmake", bindir / "cmake", bindir / "ctest", bindir / "cpack")

    def missing(self) -> list[str]:
        """Return the programs that do not exist or are not executable."""
        return [
            str(path) for path in (self.cmake, self.ctest, self.cpack)
            if not (path.is_file() and os.access(path, os.X_OK))
        ]


@dataclass
class Proc:
    """The result of one child process."""

    argv: list[str]
    cwd: str
    returncode: int
    stdout: str
    stderr: str
    seconds: float
    timed_out: bool = False

    @property
    def output(self) -> str:
        """Return stdout followed by stderr."""
        return self.stdout + self.stderr

    @property
    def ok(self) -> bool:
        """Return True when the process exited with status 0."""
        return self.returncode == 0 and not self.timed_out

    def tail(self, lines: int = 25) -> str:
        """Return the last lines of the output for a failure report."""
        text = self.output.rstrip().splitlines()[-lines:]
        cmd = " ".join(self.argv)
        return f"$ {cmd}\n[exit {self.returncode}{' timeout' if self.timed_out else ''}]\n" + "\n".join(text)


def child_env(extra: Mapping[str, str | None] | None = None) -> dict[str, str]:
    """Return a clean environment for a child process."""
    env = {key: value for key, value in os.environ.items() if not SCRUBBED_ENV.match(key)}
    env["COPYFILE_DISABLE"] = "1"
    env.setdefault("LC_ALL", "C")
    if extra:
        for key, value in extra.items():
            if value is None:
                env.pop(key, None)
            else:
                env[key] = value
    return env


def run_process(
    argv: Sequence[str | os.PathLike[str]],
    cwd: Path,
    env: Mapping[str, str],
    timeout: float,
    stdin: str | bytes | None = None,
) -> Proc:
    """Run a process with a timeout. Kill its process group on timeout."""
    completed = _run_process(argv, cwd, timeout, env, stdin=stdin)
    if completed.start_error is not None:
        return Proc(completed.argv, completed.cwd, 127, "", f"cannot start: {completed.start_error}", 0.0)
    returncode = -9 if completed.timed_out else (completed.returncode if completed.returncode is not None else -1)
    return Proc(
        completed.argv, completed.cwd, returncode,
        completed.stdout, completed.stderr, completed.seconds, completed.timed_out,
    )


def peak_rss(proc: Proc) -> int | None:
    """Read the peak resident set size that RSS_WRAPPER prints."""
    found = re.findall(r"INDEPENDENT_VERIFY_PEAK_RSS (\d+)", proc.stderr)
    return int(found[-1]) if found else None


class Sandbox:
    """A temporary directory tree under /tmp. It is removed on exit."""

    def __init__(self, base: str = "/tmp", keep: bool = False) -> None:
        base_path = Path(base)
        base_path.mkdir(parents=True, exist_ok=True)
        # Use the real path. On macOS /tmp is a symbolic link to /private/tmp.
        self.path = Path(os.path.realpath(tempfile.mkdtemp(prefix="rcmake-indep-", dir=base_path)))
        self.keep = keep
        self._removed = False
        atexit.register(self.cleanup)

    def cleanup(self) -> None:
        """Remove the sandbox unless the user asked to keep it."""
        if self._removed or self.keep:
            return
        self._removed = True
        remove_tree(self.path)

    def __enter__(self) -> "Sandbox":
        return self

    def __exit__(self, *exc: object) -> None:
        self.cleanup()


def remove_tree(path: Path) -> None:
    """Remove a directory tree. Make read-only directories writable first.

    Symbolic links are not followed, so no file outside the tree changes.
    """
    if path.is_symlink() or path.is_file():
        path.unlink(missing_ok=True)
        return
    if not path.exists():
        return
    try:
        os.chmod(path, 0o700)
    except OSError:
        pass
    for dirpath, dirnames, _files in os.walk(path):
        for name in dirnames:
            child = os.path.join(dirpath, name)
            if not os.path.islink(child):
                try:
                    os.chmod(child, 0o700)
                except OSError:
                    pass
    shutil.rmtree(path, ignore_errors=True)


def install_signal_handlers() -> None:
    """Turn SIGTERM and SIGHUP into SystemExit so that atexit cleanup runs."""
    def handler(signum: int, _frame: Any) -> None:
        raise SystemExit(128 + signum)
    for sig in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, handler)
