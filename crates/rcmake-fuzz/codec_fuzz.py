#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Exercise codecs through the staged Rcmake and language server executables."""

import argparse
import ctypes
from ctypes.util import find_library
import hashlib
import json
import os
from pathlib import Path
import random
import subprocess
import tempfile


SURFACES = ("replay-journal", "probe-cache", "fetch-lock", "lsp-frame", "cxx-modules", "cmake-cache")


def mutate(seed, rng, index):
    offset = rng.randrange(len(seed))
    if index % 4 == 0:
        return seed[:offset]
    if index % 4 == 1:
        return seed[:offset] + bytes([seed[offset] ^ (1 << rng.randrange(8))]) + seed[offset + 1:]
    if index % 4 == 2:
        return seed[:offset] + rng.randbytes(rng.randrange(1, 16)) + seed[offset:]
    return seed[:offset] + seed[offset + 1:]


def checksum(body):
    return body + hashlib.sha256(body).digest()


def replay_mutation(seed, rng, index):
    if index % 2 == 0:
        return mutate(seed, rng, index)
    prefix = len(b"RCMAKE-CONFIGURE\0\x01") + 1
    payload = mutate(seed[prefix + 4:-32], rng, index)
    return checksum(seed[:prefix] + len(payload).to_bytes(4, "little") + payload)


class ProbeFrame:
    """Build valid Store frames around changed probe payloads."""

    def __init__(self):
        library = find_library("zstd")
        if library is None:
            raise ValueError("The probe-cache surface requires libzstd.")
        lib = ctypes.CDLL(library)
        lib.ZSTD_decompress.argtypes = (
            ctypes.c_void_p, ctypes.c_size_t, ctypes.c_char_p, ctypes.c_size_t)
        lib.ZSTD_decompress.restype = ctypes.c_size_t
        lib.ZSTD_compress.argtypes = (*lib.ZSTD_decompress.argtypes, ctypes.c_int)
        lib.ZSTD_compress.restype = ctypes.c_size_t
        lib.ZSTD_compressBound.argtypes = (ctypes.c_size_t,)
        lib.ZSTD_compressBound.restype = ctypes.c_size_t
        self.lib = lib

    def mutate(self, seed, rng, index):
        if index % 2 == 0:
            return mutate(seed, rng, index)
        size = int.from_bytes(seed[9:17], "little")
        if not 0 < size <= 16 << 20 or not seed.startswith(b"RCMENTRY\x01"):
            raise ValueError("Invalid seed probe frame.")
        output = ctypes.create_string_buffer(size)
        compressed = seed[17:-32]
        if self.lib.ZSTD_decompress(output, size, compressed, len(compressed)) != size:
            raise ValueError("Cannot decode the seed probe frame.")
        payload = mutate(output.raw, rng, index)
        limit = self.lib.ZSTD_compressBound(len(payload))
        output = ctypes.create_string_buffer(limit)
        size = self.lib.ZSTD_compress(output, limit, payload, len(payload), 3)
        if size > limit:
            raise ValueError("Cannot encode the changed probe frame.")
        return checksum(seed[:9] + len(payload).to_bytes(8, "little") + output.raw[:size])


def run(command, cwd, env, data=None, *, control=False):
    result = subprocess.run(command, cwd=cwd, env=env, input=data, capture_output=True, timeout=30)
    if result.returncode not in ({0} if control else {0, 1}) or b"panicked at" in result.stderr:
        raise ValueError(f"{command} exited with {result.returncode}. {result.stderr.decode(errors='replace')}")
    return result


def fuzz_file(path, exercise, options, *, mutation=mutate, protected=False):
    seed = path.read_bytes()
    rng = random.Random(options.seed)
    for index in range(options.iterations):
        changed = mutation(seed, rng, index)
        path.write_bytes(changed)
        try:
            exercise()
        except (ValueError, OSError, subprocess.SubprocessError) as error:
            raise ValueError(f"{path.name}, seed {options.seed}, iteration {index}. {error}") from error
        if protected and index % 2 == 0 and path.is_file() and path.read_bytes() == changed:
            raise ValueError(f"{path.name} kept a changed frame at iteration {index}.")
    path.write_bytes(seed)


def configure(options, root, env, content, *flags):
    src = root / "src"
    src.mkdir()
    (src / "CMakeLists.txt").write_text(content, encoding="utf-8")
    build = root / "build"
    command = [options.rcmake, "-S", src, "-B", build, "-G", "Ninja", *flags]
    run(command, root, env, control=True)
    return src, build, command


def project_surface(surface, options, root, env):
    content = "cmake_minimum_required(VERSION 4.5)\nproject(Codec NONE)\n"
    if surface == "cmake-cache":
        _, build, _ = configure(options, root, env, content)
        fuzz_file(build / "CMakeCache.txt", lambda: run([options.rcmake, "-N", "-B", build], root, env), options)
    elif surface == "replay-journal":
        _, build, command = configure(options, root, env, content + "add_library(codec INTERFACE)\n")
        snapshot = build / "CMakeFiles/rcmake/configure.v1.bin"
        fuzz_file(snapshot, lambda: run(command, root, env, control=True), options,
                  mutation=replay_mutation, protected=True)
    elif surface == "probe-cache":
        content = content.replace("Codec NONE", "Codec C")
        content += 'try_compile(OK SOURCE_FROM_CONTENT probe.c "int main(void) { return 0; }")\n'
        content += 'if(NOT OK)\nmessage(FATAL_ERROR "The probe failed.")\nendif()\n'
        _, _, command = configure(options, root, env, content)
        entries = list((Path(env["RCMAKE_CACHE_DIR"]) / "entries/probe-v1").rglob("*.entry"))
        if not entries:
            raise ValueError("The compiler produced no verified probe entry.")
        frame = ProbeFrame()
        for entry in entries:
            fuzz_file(entry, lambda: run(command, root, env, control=True), options,
                      mutation=frame.mutate)
    else:
        archive = root / "seed.bin"
        archive.write_bytes(b"fetch lock fuzz input")
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        content += f'''include(FetchContent)
FetchContent_Declare(codec URL "{archive.as_uri()}" URL_HASH SHA256={digest} DOWNLOAD_NO_EXTRACT TRUE)
FetchContent_MakeAvailable(codec)
'''
        src, _, command = configure(options, root, env, content, "--fetch-lock=update")
        command[-1] = "--fetch-lock=frozen"
        run(command, root, env, control=True)
        fuzz_file(src / "rcmake-fetch.lock", lambda: run(command, root, env), options)


def modules(options, root, env):
    tdi = root / "tdi.json"
    ddi = root / "scan.ddi"
    tdi.write_text(json.dumps({
        **{key: root.as_posix() for key in ("dir-cur-bld", "dir-cur-src", "dir-top-bld", "dir-top-src")},
        "module-dir": "bmi",
        "cxx-modules": {"a.o": {"type": "CXX_MODULES", "visibility": "PUBLIC"}},
    }), encoding="utf-8")
    ddi.write_text(json.dumps({
        "version": 1,
        "rules": [{"primary-output": "a.o", "provides": [{"logical-name": "A"}]}],
    }), encoding="utf-8")
    command = [options.rcmake, "-E", "cmake_ninja_dyndep", "--tdi=tdi.json", "--lang=CXX",
               "--dd=out.dd", "--modmapfmt=clang", "scan.ddi"]
    run(command, root, env, control=True)
    for fixture in (tdi, ddi):
        fuzz_file(fixture, lambda: run(command, root, env), options)


def lsp(options, root, env):
    message = b'{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}'
    header = f"Content-Length: {len(message)}\r\n\r\n".encode()
    command = [options.lsp]
    result = run(command, root, env, header + message, control=True)
    if b'"result"' not in result.stdout:
        raise ValueError("The language server did not initialize.")
    rng = random.Random(options.seed)
    for index in range(options.iterations):
        if index % 2:
            body = mutate(message, rng, index)
            frame = f"Content-Length: {len(body)}\r\n\r\n".encode() + body
        else:
            frame = mutate(header, rng, index) + message
        run(command, root, env, frame)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rcmake", type=lambda path: Path(path).resolve(), required=True)
    parser.add_argument("--lsp", type=lambda path: Path(path).resolve())
    parser.add_argument("--surface", choices=SURFACES, action="append")
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("--iterations", type=int, default=16)
    options = parser.parse_args()
    if options.iterations < 1:
        parser.error("Use a positive iteration count.")
    for surface in options.surface or SURFACES:
        if surface == "lsp-frame" and options.lsp is None:
            parser.error("Pass --lsp for the lsp-frame surface.")
        with tempfile.TemporaryDirectory(prefix=f"rcmake-fuzz-{surface}-") as directory:
            root = Path(directory).resolve()
            env = {key: value for key, value in os.environ.items()
                   if not key.startswith(("RCMAKE_", "CMAKE_")) or key == "RCMAKE_VENDORED_DATA_ROOT"}
            env.update(RCMAKE_CACHE_DIR=str(root / "store"), RCMAKE_PROBE_JOBS="1")
            if surface == "fetch-lock":
                env["RCMAKE_FETCH_OFFLINE"] = "1"
            if surface == "probe-cache":
                env["RCMAKE_NO_INCREMENTAL"] = "1"
            if surface == "lsp-frame":
                lsp(options, root, env)
            elif surface == "cxx-modules":
                modules(options, root, env)
            else:
                project_surface(surface, options, root, env)
        print(f"PASS {surface} with seed {options.seed} and {options.iterations} iterations.")


if __name__ == "__main__":
    main()
