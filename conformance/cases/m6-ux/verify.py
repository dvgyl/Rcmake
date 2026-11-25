#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Check the staged Rcmake and language server entry points."""

import argparse
import json
import os
from pathlib import Path
import queue
import re
import subprocess
import tempfile
import threading
import time


PROJECT = """cmake_minimum_required(VERSION 4.5)
project(DailyUx LANGUAGES NONE)
include(settings.cmake)
message(STATUS "configure output belongs outside the protocol")
add_library(component INTERFACE)
add_library(Demo::component ALIAS component)
set(component visible)
get_property(result CACHE EDITABLE PROPERTY STRINGS)
file(WRITE "${CMAKE_BINARY_DIR}/marker.txt" "configure")
"""
SETTINGS = """string(CONCAT EVALUATED "from-" "disk")
set(EDITABLE saved CACHE STRING "Choice for the fixture")
mark_as_advanced(EDITABLE)
set_property(CACHE EDITABLE PROPERTY STRINGS first second)
"""


def check_cli(native, oracle, source, root, env):
    done = root / "configure"
    log = root / "exec.jsonl"
    options = ["-S", source, "-B", done, "-G", "Ninja"]
    def run(binary, *args, check=True, **changes):
        return subprocess.run([binary, *args], cwd=root, env=env | changes,
                              capture_output=True, check=check, timeout=30)

    run(native, *options, RCMAKE_EXEC_LOG=str(log))
    version_records = []
    for name in ("cmake", "ctest", "cpack"):
        alias = native.with_name(name)
        if not alias.exists():
            alias.symlink_to(native.name)
        run(alias, "--version", RCMAKE_EXEC_LOG=str(log))
        version_records.append(name)
    assert {entry["persona"] for entry in map(json.loads, log.read_text().splitlines())} == set(version_records)
    assert b"rcmake explain" in run(native, "--help").stdout
    assert b"Rcmake extensions" not in run(native.with_name("cmake"), "--help").stdout

    script = root / "args.cmake"
    script.write_text('message("${CMAKE_ARGV3}|${CMAKE_ARGV4}|${CMAKE_ARGV5}")\n', encoding="utf-8")
    arguments = ["-P", script, "--", "--profile", "with spaces"]
    assert run(native, *arguments).stderr == run(oracle, *arguments).stderr
    assert run(native.with_name("cmake"), *arguments).stderr == run(oracle, *arguments).stderr

    explained_target = run(native, "explain", "Demo::component", "-B", done,
                           "--target", "--json", "--filter", "missing")
    target = json.loads(explained_target.stdout)
    assert target["schema"] == "rcmake-explain-v1", target
    assert target["name"] == "Demo::component" and target["details"]["target"] == "component", target
    assert target["details"]["alias_definition"]["path"] == str(source / "CMakeLists.txt"), target
    variable = json.loads(run(native, "explain", "EDITABLE", "-B", done, "--var", "--json").stdout)
    cache = variable["details"]["cache"]
    assert cache["advanced"] is True and cache["help"] == "Choice for the fixture", cache
    assert cache["strings"] == ["first", "second"], cache
    ambiguous = run(native, "explain", "component", "-B", done, "--json", check=False)
    assert ambiguous.returncode == 1 and b"ambiguous" in ambiguous.stdout, ambiguous
    source.joinpath("CMakeLists.txt").write_text(
        PROJECT + "execute_process(COMMAND never-run-me RESULT_VARIABLE result)\n",
        encoding="utf-8",
    )
    partial = run(native, "explain", "Demo::component", "-B", done, "--target", "--json")
    def denials(result):
        count = re.search(rb"Rcmake denied ([0-9]+) subprocess", result.stderr)
        return int(count.group(1)) if count else 0

    assert denials(partial) == denials(explained_target) + 1, partial
    assert json.loads(partial.stdout)["details"]["provisional"] is True, partial
    assert source.joinpath("settings.cmake").read_text(encoding="utf-8") == SETTINGS
    assert done.joinpath("marker.txt").read_text(encoding="utf-8") == "configure"

    profiles = root / "native-profile"
    run(native, *options, RCMAKE_PROFILE=str(profiles))
    profile = json.loads(profiles.joinpath("trace.json").read_bytes())
    assert profiles.joinpath("flamegraph.html").is_file() and profile
    assert b"invocation wall time" in run(native, *options, "--profile").stderr
    source.joinpath("CMakeLists.txt").write_text(PROJECT + "messgae(WARNING typo)\n", encoding="utf-8")
    rich = run(native, *options, "--diagnostics=rich", check=False, CLICOLOR_FORCE="1")
    assert rich.returncode and b"\x1b[" in rich.stderr and b"Use `message`" in rich.stderr, rich
    compat = run(native, *options, "--diagnostics=rich", check=False, RCMAKE_ORACLE_MODE="1")
    assert compat.returncode and b"CMake Error" in compat.stderr and b"\x1b[" not in compat.stderr, compat
    source.joinpath("CMakeLists.txt").write_text(PROJECT, encoding="utf-8")



class Protocol:
    def __init__(self, process):
        self.process = process
        self.incoming = queue.Queue()
        self.next_id = 0
        threading.Thread(target=self.read, daemon=True).start()

    def read(self):
        try:
            while True:
                header = self.process.stdout.readline()
                if not header:
                    break
                prefix = b"Content-Length: "
                assert header.startswith(prefix), header
                assert self.process.stdout.readline() == b"\r\n"
                payload = self.process.stdout.read(int(header[len(prefix):]))
                self.incoming.put(json.loads(payload))
            self.incoming.put(EOFError("The server closed stdout."))
        except Exception as error:
            self.incoming.put(error)

    def send(self, method, params=None, request=False):
        data = {"jsonrpc": "2.0", "method": method, "params": params}
        if request:
            self.next_id += 1
            data["id"] = self.next_id
        payload = json.dumps(data).encode()
        self.process.stdin.write(f"Content-Length: {len(payload)}\r\n\r\n".encode() + payload)
        self.process.stdin.flush()

    def receive(self, matches, description):
        deadline = time.monotonic() + 10
        while (remaining := deadline - time.monotonic()) > 0:
            try:
                message = self.incoming.get(timeout=remaining)
            except queue.Empty:
                break
            if isinstance(message, Exception):
                raise message
            if matches(message):
                return message
        raise TimeoutError(description)

    def request(self, method, params=None):
        self.send(method, params, request=True)
        return self.receive(lambda message: message.get("id") == self.next_id, method)["result"]


def check_lsp(binary, source, env):
    settings = source / "settings.cmake"
    top = source / "CMakeLists.txt"
    lists = PROJECT + 'message(STATUS "${EVALUATED}")\n'
    top.write_text(lists, encoding="utf-8")
    with subprocess.Popen([binary], cwd=source, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE) as process:
        try:
            client = Protocol(process)
            client.request("initialize", {"rootUri": source.as_uri(), "capabilities": {}})
            client.send("initialized", {})
            def open_doc(path, text):
                client.send("textDocument/didOpen", {"textDocument": {
                    "uri": path.as_uri(), "version": 1, "languageId": "cmake", "text": text}})
            def change(version, text):
                client.send("textDocument/didChange", {"textDocument": {
                    "uri": settings.as_uri(), "version": version}, "contentChanges": [{"text": text}]})
            def query(method, line, character):
                return client.request(f"textDocument/{method}", {"textDocument": {
                    "uri": top.as_uri()}, "position": {"line": line, "character": character}})
            def hover():
                return json.dumps(query("hover", len(lists.splitlines()) - 1, 20) or {})
            def wait_for(value):
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline:
                    current = hover()
                    assert "from-obsolete" not in current, current
                    if value in current:
                        return
                    time.sleep(0.02)
                raise TimeoutError(value)

            open_doc(top, lists)
            assert "message" in json.dumps(query("hover", 3, 2))
            wait_for("from-disk")
            open_doc(settings, SETTINGS)
            # The loop keeps a configure active while the previous snapshot is available.
            change(2, "foreach(item RANGE 250000)\nendforeach()\n" + SETTINGS.replace('"disk"', '"obsolete"'))
            stale = hover().lower()
            assert "provisional" in stale and "from-disk" in stale, stale
            client.send("$/cancelRequest", {"id": client.next_id})
            updated = "# shifted definition\n" + SETTINGS.replace('"disk"', '"unsaved"')
            change(3, updated)
            wait_for("from-unsaved")
            definition = query("definition", len(lists.splitlines()) - 1, 20)
            if isinstance(definition, list):
                definition = next(iter(definition), None)
            assert definition and definition["uri"] == settings.as_uri(), definition
            assert definition["range"]["start"]["line"] == 1, definition
            assert settings.read_text(encoding="utf-8") == SETTINGS
            assert not source.joinpath(".rcmake-lsp").exists()

            diagnostic_line = len(updated.splitlines())
            change(4, updated + "messgae(STATUS typo)\n")
            client.receive(lambda message: message.get("method") == "textDocument/publishDiagnostics"
                           and message["params"].get("uri") == settings.as_uri()
                           and message["params"].get("version") == 4
                           and "Unknown CMake command" in json.dumps(message["params"].get("diagnostics")),
                           "evaluated command diagnostic")
            actions = client.request("textDocument/codeAction", {
                "textDocument": {"uri": settings.as_uri()},
                "range": {"start": {"line": diagnostic_line, "character": 0},
                          "end": {"line": diagnostic_line, "character": 7}},
                "context": {"diagnostics": []},
            })
            assert any(action.get("isPreferred") and '"newText": "message"' in json.dumps(action)
                       for action in actions), actions
            client.request("shutdown")
            client.send("exit")
            assert process.wait(timeout=10) == 0
        finally:
            if process.poll() is None:
                process.kill()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rcmake", type=Path, required=True)
    parser.add_argument("--lsp", type=Path, required=True)
    parser.add_argument("--oracle", type=Path, required=True)
    args = parser.parse_args()
    env = {key: value for key, value in os.environ.items()
           if not key.startswith("RCMAKE_") or key == "RCMAKE_VENDORED_DATA_ROOT"}
    with tempfile.TemporaryDirectory(prefix="rcmake-m6-ux-") as temporary:
        root = Path(temporary).resolve()
        source = root / "src"
        source.mkdir()
        env["RCMAKE_CACHE_DIR"] = str(root / "cache")
        source.joinpath("CMakeLists.txt").write_text(PROJECT, encoding="utf-8")
        source.joinpath("settings.cmake").write_text(SETTINGS, encoding="utf-8")
        check_cli(args.rcmake, args.oracle, source, root, env)
        print("PASS Native CLI, Explain, profiles, diagnostic modes, and execution logging.")
        check_lsp(args.lsp, source, env)
        print("PASS LSP framing, evaluated hover and definition, stale results, cancellation on a new edit, diagnostics, quick fixes, and overlay isolation.")


if __name__ == "__main__":
    main()
