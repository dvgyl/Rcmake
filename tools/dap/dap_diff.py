#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Compare the `--debugger` DAP transcripts of CMake 4.5.1 and rcmake.

The tool runs one fixed client script for each scenario against the oracle
(`$RCMAKE_ORACLE_CMAKE`) and against rcmake. It normalizes message sequence numbers,
frame ids, variable references, and paths, and then compares the two transcripts.

Scenarios use generated `-P` scripts and projects in a temporary directory, plus the
upstream `Tests/CMakeLib/DebuggerSample` project, which runs in place (it is not
copied). The client uses Python standard library `socket.AF_UNIX` with a 30 s limit.

Usage: dap_diff.py [--rcmake PATH] [--oracle PATH] [--only NAME ...] [--show]
Exit status: 0 if every scenario matches, 1 otherwise.
"""

from __future__ import annotations

import argparse
import difflib
import json
import os
import pathlib
import shutil
import socket
import subprocess
import sys
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parents[2]
SAMPLE = ROOT / "env/oracle/cmake-4.5.1/Tests/CMakeLib/DebuggerSample"
TIMEOUT = 30.0

# Variables whose storage is a known evaluator difference, not a debugger difference:
# CMake keeps these in the cache in `-P` mode. Their rows and the counts that include
# them are left out of the comparison.
KNOWN_VALUE_DIFFERENCES = {
    "CMAKE_ARGV0", "CMAKE_COMMAND", "CMAKE_CPACK_COMMAND", "CMAKE_CTEST_COMMAND",
    "CMAKE_ROOT",
}
COUNTED = {"Locals", "CacheVariables", "Directories"}


class Client:
    """A minimal DAP client over a Unix socket."""

    def __init__(self, path: str):
        deadline = time.monotonic() + TIMEOUT
        while not os.path.exists(path):
            if time.monotonic() > deadline:
                raise TimeoutError(f"no socket at {path}")
            time.sleep(0.01)
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(TIMEOUT)
        self.sock.connect(path)
        self.buf = b""
        self.seq = 0
        self.log: list[dict] = []
        self.pending: list[dict] = []

    def send(self, command: str, arguments: dict | None = None) -> int:
        self.seq += 1
        message = {"seq": self.seq, "type": "request", "command": command}
        if arguments is not None:
            message["arguments"] = arguments
        body = json.dumps(message).encode()
        self.sock.sendall(b"Content-Length: %d\r\n\r\n" % len(body) + body)
        self.log.append({"client": message})
        return self.seq

    def recv(self) -> dict | None:
        while b"\r\n\r\n" not in self.buf:
            data = self.sock.recv(65536)
            if not data:
                return None
            self.buf += data
        header, rest = self.buf.split(b"\r\n\r\n", 1)
        length = int(header.split(b":", 1)[1])
        while len(rest) < length:
            data = self.sock.recv(65536)
            if not data:
                return None
            rest += data
        self.buf = rest[length:]
        message = json.loads(rest[:length])
        self.log.append(message)
        return message

    def until(self, predicate) -> dict | None:
        for index, message in enumerate(self.pending):
            if predicate(message):
                return self.pending.pop(index)
        while True:
            message = self.recv()
            if message is None or predicate(message):
                return message
            if stop_or_end(message):
                self.pending.append(message)

    def request(self, command: str, arguments: dict | None = None) -> dict | None:
        seq = self.send(command, arguments)
        return self.until(lambda m: m.get("type") == "response" and m.get("request_seq") == seq)

    def close(self):
        self.sock.close()


def is_event(name):
    return lambda m: m.get("type") == "event" and m.get("event") == name


def stop_or_end(m):
    return m.get("type") == "event" and m.get("event") in ("stopped", "terminated")


def inspect(client: Client, variables: bool, evaluate: list[str]):
    """Record the stack and, optionally, the scope tree and evaluations of each frame."""
    trace = client.request("stackTrace", {
        "threadId": 1, "format": {"parameters": True, "parameterValues": True, "line": True}})
    if not trace or not trace.get("success"):
        return
    frames = trace["body"]["stackFrames"]
    for frame in frames if (variables or evaluate) else []:
        for expression in evaluate:
            client.request("evaluate", {"expression": expression, "frameId": frame["id"]})
        if not variables:
            continue
        scopes = client.request("scopes", {"frameId": frame["id"]})
        for scope in (scopes or {}).get("body", {}).get("scopes", []):
            root = client.request("variables", {"variablesReference": scope["variablesReference"]})
            for row in (root or {}).get("body", {}).get("variables", []):
                if row["name"] in ("Locals", "Directories") and row["variablesReference"]:
                    client.request("variables", {"variablesReference": row["variablesReference"]})


def drive(client: Client, scenario: dict):
    client.request("initialize", {"adapterID": "dap_diff", "supportsVariableType": True})
    for path, lines in scenario.get("breakpoints", {}).items():
        client.request("setBreakpoints", {
            "source": {"path": path}, "breakpoints": [{"line": n} for n in lines]})
    client.request("setExceptionBreakpoints", {"filters": scenario.get("filters", [])})
    client.request("configurationDone")
    actions = list(scenario.get("actions", []))
    stops = 0
    while True:
        message = client.until(stop_or_end)
        if message is None or message.get("event") == "terminated":
            break
        stops += 1
        if stops > 64:
            raise RuntimeError("too many stops")
        inspect(client, scenario.get("variables", False), scenario.get("evaluate", []))
        if message["body"].get("reason") == "exception":
            client.request("exceptionInfo", {"threadId": 1})
        action = actions.pop(0) if actions else "continue"
        if action == "disconnect":
            client.request("disconnect", {})
            return
        client.request(action, {"threadId": 1})
    if message is not None:
        client.request("disconnect", {})


def run(exe: str, scenario: dict, work: pathlib.Path) -> list:
    pipe = tempfile.mktemp(prefix="dapdiff-", suffix=".sock", dir="/tmp")
    command = [exe, "--debugger", "--debugger-pipe", pipe, *scenario["args"](work)]
    process = subprocess.Popen(command, cwd=work, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, stdin=subprocess.DEVNULL)
    client = None
    try:
        client = Client(pipe)
        drive(client, scenario)
        out, err = process.communicate(timeout=TIMEOUT)
    except Exception as error:  # noqa: BLE001 - report every failure as a transcript line
        process.kill()
        out, err = process.communicate()
        if client:
            client.log.append({"client-error": repr(error)})
    finally:
        if client:
            client.close()
        if os.path.exists(pipe):
            os.unlink(pipe)
    log = client.log if client else []
    log.insert(0, {"pipe": pipe})
    log.append({"exit": process.returncode})
    log.append({"stdout": [line for line in out.decode(errors="replace").splitlines()
                           if line.startswith(("Waiting", "Debugger", "[CMake Debugger]",
                                               "Running with debugger"))]})
    return log


class Normalizer:
    def __init__(self, replacements: list[tuple[str, str]]):
        self.ids: dict[tuple[str, int], int] = {}
        self.replacements = replacements

    def id(self, kind: str, value):
        if not isinstance(value, int) or value == 0:
            return value
        key = (kind, value)
        if key not in self.ids:
            self.ids[key] = sum(1 for k in self.ids if k[0] == kind) + 1
        return f"{kind}{self.ids[key]}"

    def text(self, value):
        if isinstance(value, str):
            for old, new in self.replacements:
                value = value.replace(old, new)
        return value

    def walk(self, value, key=None):
        if isinstance(value, dict):
            out = {}
            for k, v in value.items():
                if k in ("seq", "request_seq"):
                    continue
                if k in ("frameId",) or (k == "id" and key == "frame"):
                    out[k] = self.id("frame", v)
                elif k == "variablesReference":
                    out[k] = self.id("ref", v)
                else:
                    out[k] = self.walk(v, "frame" if k == "stackFrames" else key)
            return out
        if isinstance(value, list):
            return [self.walk(v, key) for v in value]
        return self.text(value)


def filter_rows(message):
    """Drop the rows of known evaluator differences and the counts that include them."""
    body = message.get("body") if isinstance(message, dict) else None
    if not isinstance(body, dict) or "variables" not in body:
        return message
    rows = []
    for row in body["variables"]:
        if row.get("name") in KNOWN_VALUE_DIFFERENCES:
            continue
        if row.get("name") in COUNTED:
            row = dict(row, value="<count>")
        rows.append(row)
    return dict(message, body=dict(body, variables=rows))


RESUME_COMMANDS = {"configurationDone", "continue", "next", "stepIn", "stepOut"}


def canonicalize_resume_order(log: list) -> list:
    """Place a resume response ahead of an immediately racing stop/end event."""
    out = list(log)
    index = 0
    while index + 1 < len(out):
        first, second = out[index], out[index + 1]
        if (
            isinstance(first, dict)
            and isinstance(second, dict)
            and stop_or_end(first)
            and second.get("type") == "response"
            and second.get("command") in RESUME_COMMANDS
        ):
            out[index], out[index + 1] = second, first
            index += 2
        else:
            index += 1
    return out


def normalize(log: list, replacements) -> list[str]:
    normalizer = Normalizer(replacements)
    lines = []
    for message in canonicalize_resume_order(log):
        message = filter_rows(normalizer.walk(message))
        lines.append(json.dumps(message, sort_keys=True))
    return lines


def write(path: pathlib.Path, text: str):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def scenarios(work: pathlib.Path) -> dict[str, dict]:
    """Scenario name to client script. Each `args` gets the scenario directory."""
    s = {}

    def script(name: str, text: str, **extra) -> dict:
        path = work / name / "main.cmake"
        write(path, text)
        return {"args": lambda _w, p=path: ["-P", str(p)], "dir": path.parent, **extra}

    s["basic"] = script("basic", 'set(X 1)\nmessage(STATUS "a ${X}")\nset(Y 2)\n',
                        actions=["next", "continue"], variables=True, evaluate=["X", "NOPE"])
    s["basic"]["breakpoints"] = {str(work / "basic/main.cmake"): [2, 100]}

    inc = script("include", "set(A 1)\ninclude(${CMAKE_CURRENT_LIST_DIR}/inc.cmake)\nset(B 2)\n"
                 "include(${CMAKE_CURRENT_LIST_DIR}/empty.cmake)\nset(C 3)\n",
                 actions=["stepIn", "stepIn", "next", "stepOut", "stepIn", "stepIn", "continue"])
    write(work / "include/inc.cmake", "set(I1 x)\nset(I2\n  y)\nset(I3 z)\n")
    write(work / "include/empty.cmake", "# nothing\n")
    inc["breakpoints"] = {str(work / "include/main.cmake"): [2]}
    s["include"] = inc

    s["pending"] = script("pending", "include(${CMAKE_CURRENT_LIST_DIR}/inc.cmake)\n")
    write(work / "pending/inc.cmake", "set(P1 1)\n\nset(P2\n  2\n  3)\n")
    s["pending"]["breakpoints"] = {str(work / "pending/inc.cmake"): [1, 2, 4, 6, 9]}

    calls = script("calls", "function(f a b)\n  set(L ${a})\n  message(STATUS \"${b}\")\nendfunction()\n"
                   "macro(m x)\n  set(M ${x})\nendmacro()\n"
                   "f(1 2)\nm(3)\nf(4 5)\nset(END 1)\n",
                   actions=["stepIn", "stepIn", "stepOut", "next", "stepIn", "next", "next",
                            "next", "continue"], evaluate=["L", "M", "a"])
    calls["breakpoints"] = {str(work / "calls/main.cmake"): [8]}
    s["calls"] = calls

    interrupt = script("interrupt", "function(f)\n  set(IN 1)\n  set(IN2 2)\nendfunction()\nf()\nset(OUT 1)\n",
                       actions=["next", "continue", "continue"])
    interrupt["breakpoints"] = {str(work / "interrupt/main.cmake"): [3, 5]}
    s["interrupt"] = interrupt

    block = script("block", "set(V 0)\nblock()\n  set(V 1)\n  set(W 2)\nendblock()\nset(Z 3)\n",
                   actions=["next", "next", "next", "next", "continue"], evaluate=["V"])
    block["breakpoints"] = {str(work / "block/main.cmake"): [1]}
    s["block"] = block

    call = script("call", "function(g x)\n  set(G ${x})\nendfunction()\n"
                  "cmake_language(CALL g 7)\nset(AFTER 1)\n",
                  actions=["continue", "stepIn", "continue"])
    call["breakpoints"] = {str(work / "call/main.cmake"): [4]}
    s["call"] = call

    ev = script("eval", 'cmake_language(EVAL CODE "set(E1 1)\nset(E2 2)")\nset(AFTER 1)\n',
                actions=["stepIn", "next", "next", "continue"])
    ev["breakpoints"] = {str(work / "eval/main.cmake"): [1]}
    s["eval"] = ev

    s["root-step-out"] = script("root-step-out", "set(A 1)\nset(B 2)\n", actions=["stepOut"])
    s["root-step-out"]["breakpoints"] = {str(work / "root-step-out/main.cmake"): [1]}

    s["warning"] = script("warning", 'message(WARNING "careful")\nmessage(NOTICE "plain")\nset(A 1)\n',
                          filters=["WARNING", "MESSAGE"])

    s["parse-error"] = script("parse-error", "include(${CMAKE_CURRENT_LIST_DIR}/bad.cmake)\nset(A 1)\n",
                              filters=["FATAL_ERROR"])
    write(work / "parse-error/bad.cmake", "set(A\n")

    s["eval-error"] = script("eval-error", 'cmake_language(EVAL CODE "set(A")\nset(B 1)\n',
                             filters=["FATAL_ERROR"])

    s["disconnect"] = script("disconnect", "set(A 1)\nset(B 2)\nmessage(STATUS done)\n",
                             actions=["disconnect"])
    s["disconnect"]["breakpoints"] = {str(work / "disconnect/main.cmake"): [1]}

    project = work / "dirs"
    write(project / "CMakeLists.txt", "cmake_minimum_required(VERSION 3.26)\nproject(D NONE)\n"
          "set(PV parent)\nadd_subdirectory(sub)\nset(AFTER 1)\n")
    write(project / "sub/CMakeLists.txt", "set(CV child)\nset(PV shadow)\nmessage(STATUS here)\n")
    s["dirs"] = {"args": lambda w: ["-S", str(project), "-B", str(w / "dirs-build")],
                 "breakpoints": {str(project / "sub/CMakeLists.txt"): [3]},
                 "evaluate": ["PV", "CV"], "actions": ["stepOut", "continue"]}

    if SAMPLE.is_dir():
        s["sample"] = {"args": lambda w: ["-S", str(SAMPLE), "-B", str(w / "sample-build")],
                       "breakpoints": {str(SAMPLE / "CMakeLists.txt"): [3, 9]},
                       "evaluate": ["CMAKE_PROJECT_NAME"], "actions": ["next", "continue"]}
    return s


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--rcmake", default=str(ROOT / "target/debug/rcmake"))
    parser.add_argument("--oracle", default=os.environ.get("RCMAKE_ORACLE_CMAKE", ""))
    parser.add_argument("--only", nargs="*", default=[])
    parser.add_argument("--show", action="store_true", help="print each oracle transcript")
    args = parser.parse_args()
    if not args.oracle or not os.path.exists(args.oracle):
        print("dap_diff: set RCMAKE_ORACLE_CMAKE or pass --oracle", file=sys.stderr)
        return 2
    args.rcmake = str(pathlib.Path(args.rcmake).resolve())
    args.oracle = str(pathlib.Path(args.oracle).resolve())
    failures = 0
    base = pathlib.Path(tempfile.mkdtemp(prefix="dapdiff-"))
    try:
        for name in scenarios(base):
            if args.only and name not in args.only:
                continue
            logs = {}
            for label, exe in (("oracle", args.oracle), ("rcmake", args.rcmake)):
                work = base / label
                if work.exists():
                    shutil.rmtree(work)
                work.mkdir(parents=True)
                scenario = scenarios(work)[name]
                replacements = [(str(work.resolve()), "<work>"), (str(work), "<work>"),
                                (str(ROOT), "<root>"), (exe, "<cmake>")]
                log = run(exe, scenario, work)
                replacements.insert(0, (log.pop(0)["pipe"], "<pipe>"))
                logs[label] = normalize(log, replacements)
            if args.show:
                print(f"--- {name} (oracle)")
                print("\n".join(logs["oracle"]))
            if logs["oracle"] == logs["rcmake"]:
                print(f"PASS {name} ({len(logs['oracle'])} messages)")
                continue
            failures += 1
            print(f"FAIL {name}")
            sys.stdout.writelines(line + "\n" for line in difflib.unified_diff(
                logs["oracle"], logs["rcmake"], "oracle", "rcmake", lineterm="", n=2))
    finally:
        shutil.rmtree(base, ignore_errors=True)
    print(f"dap_diff: {failures} failing scenario(s)")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
