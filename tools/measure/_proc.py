# SPDX-License-Identifier: MIT OR Apache-2.0
"""Shared subprocess helpers with process-group timeout termination."""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time
from typing import Any


@dataclass
class ProcessResult:
    """Captured output, exit status, and timing for one child process."""

    argv: list[str]
    cwd: str
    returncode: int | None
    stdout: str
    stderr: str
    seconds: float
    timed_out: bool = False
    start_error: str | None = None


def kill_group(process: subprocess.Popen[Any], env: Mapping[str, str] | None = None) -> None:
    """Terminate a child process and every process in its process group."""
    if os.name == "nt":
        system_root = (env or os.environ).get("SYSTEMROOT")
        if system_root:
            taskkill = Path(system_root) / "System32/taskkill.exe"
            try:
                subprocess.run(
                    [taskkill, "/F", "/T", "/PID", str(process.pid)],
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                    timeout=5, check=False,
                )
            except (OSError, subprocess.TimeoutExpired):
                pass
        try:
            process.kill()
        except ProcessLookupError:
            pass
        return
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError):
        try:
            process.kill()
        except ProcessLookupError:
            pass


def _popen_kwargs(
    cwd: Path | str,
    env: Mapping[str, str] | None,
    *,
    stdin: Any,
    stdout: Any,
    stderr: Any,
    text: bool = False,
) -> dict[str, Any]:
    kwargs: dict[str, Any] = {
        "cwd": cwd,
        "env": dict(env) if env is not None else None,
        "stdin": stdin,
        "stdout": stdout,
        "stderr": stderr,
    }
    if text:
        kwargs.update(text=True, encoding="utf-8", errors="replace")
    if os.name == "nt":
        kwargs["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
    else:
        kwargs["start_new_session"] = True
    return kwargs


def _run_with_tempfiles(
    args: list[str],
    cwd: Path | str,
    timeout: float,
    env: Mapping[str, str] | None,
    *,
    merge_stderr: bool,
    catch_oserror: bool,
) -> ProcessResult:
    start = time.monotonic()
    timed_out = False
    returncode: int | None = None
    start_error: str | None = None
    with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
        stderr_target = subprocess.STDOUT if merge_stderr else stderr
        try:
            with subprocess.Popen(
                args,
                **_popen_kwargs(cwd, env, stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr_target),
            ) as process:
                try:
                    returncode = process.wait(timeout)
                except (subprocess.TimeoutExpired, KeyboardInterrupt) as exc:
                    kill_group(process, env)
                    returncode = process.wait()
                    if isinstance(exc, KeyboardInterrupt):
                        raise
                    timed_out = True
        except OSError as exc:
            if not catch_oserror:
                raise
            start_error = str(exc)
        stdout.seek(0)
        stderr.seek(0)
        out_text = stdout.read().decode("utf-8", errors="replace")
        err_text = "" if merge_stderr else stderr.read().decode("utf-8", errors="replace")
    return ProcessResult(
        argv=args, cwd=str(cwd), returncode=returncode,
        stdout=out_text, stderr=err_text,
        seconds=time.monotonic() - start, timed_out=timed_out, start_error=start_error,
    )


def run_process(
    argv: Sequence[str | os.PathLike[str]],
    cwd: Path | str,
    timeout: float,
    env: Mapping[str, str] | None = None,
    *,
    stdin: str | bytes | None = None,
    text: bool = False,
    merge_stderr: bool = False,
    use_tempfiles: bool = False,
    catch_oserror: bool = True,
    drain_timeout: float | None = None,
) -> ProcessResult:
    """Run a child process and kill its process group on timeout or interrupt."""
    args = [os.fspath(arg) for arg in argv]
    if use_tempfiles and stdin is None and not text:
        return _run_with_tempfiles(
            args, cwd, timeout, env, merge_stderr=merge_stderr, catch_oserror=catch_oserror,
        )
    start = time.monotonic()
    if text:
        data: str | bytes | None = stdin.decode("utf-8", errors="replace") if isinstance(stdin, bytes) else stdin
    else:
        data = stdin.encode("utf-8") if isinstance(stdin, str) else stdin
    stdin_target = subprocess.PIPE if data is not None else subprocess.DEVNULL
    stderr_target = subprocess.STDOUT if merge_stderr else subprocess.PIPE
    try:
        process = subprocess.Popen(
            args,
            **_popen_kwargs(cwd, env, stdin=stdin_target, stdout=subprocess.PIPE, stderr=stderr_target, text=text),
        )
    except OSError as exc:
        if not catch_oserror:
            raise
        return ProcessResult(
            argv=args, cwd=str(cwd), returncode=None,
            stdout="", stderr="", seconds=time.monotonic() - start, start_error=str(exc),
        )
    with process:
        try:
            raw_out, raw_err = process.communicate(data, timeout=timeout)
            timed_out = False
        except (subprocess.TimeoutExpired, KeyboardInterrupt) as exc:
            kill_group(process, env)
            if isinstance(exc, KeyboardInterrupt):
                process.communicate()
                raise
            raw_out, raw_err = process.communicate(timeout=drain_timeout)
            timed_out = True
    seconds = time.monotonic() - start
    if text:
        out_text = raw_out or ""
        err_text = "" if merge_stderr else (raw_err or "")
    else:
        out_text = (raw_out or b"").decode("utf-8", errors="replace")
        err_text = "" if merge_stderr else (raw_err or b"").decode("utf-8", errors="replace")
    return ProcessResult(
        argv=args, cwd=str(cwd), returncode=process.returncode,
        stdout=out_text, stderr=err_text, seconds=seconds, timed_out=timed_out,
    )
