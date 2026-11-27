#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Mock Swift compiler (`swiftc`) for differential testing against CMake 4.5.1.

Use --install-mock-swift DIRECTORY to create bin/swiftc.
Supports:
  - `-version` (Apple Swift version 6.0.3)
  - `-print-target-info` (triple, moduleTriple, pointerWidthInBytes)
  - `-wmo main.swift lib.in -###` (new or old driver probe via MOCK_SWIFTC_OLD_DRIVER)
  - `@rsp` response files (bounded depth and size)
  - Strict `-output-file-map <json>` validation against every Swift source
  - Real host C object compilation (via host `cc`) + real static/shared/exe linking
  - Import resolution checking `import <Mod>` against `-I <dir>` `.swiftmodule` artifacts
  - Content-hashed `.swiftmodule` emission + optional `-emit-objc-header-path`
  - JSONL invocation logging via MOCK_SWIFTC_LOG
"""

from __future__ import annotations

from dataclasses import dataclass, field
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shlex
import shutil
import subprocess
import sys
import tempfile

TIMEOUT = 120
MAX_RSP_DEPTH = 8
MAX_RSP_BYTES = 1024 * 1024
MAX_RSP_ARGS = 32768
MAX_OUTPUT_MAP_BYTES = 4 * 1024 * 1024
DEFAULT_VERSION = "6.0.3"
IMPORT_RE = re.compile(r"^\s*import\s+([A-Za-z_][A-Za-z0-9_]*)\b", re.MULTILINE)
EXPORT_RE = re.compile(r"//\s*MOCK_EXPORT\s+([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(-?\d+)")
CALL_RE = re.compile(r"//\s*MOCK_CALL\s+([A-Za-z_][A-Za-z0-9_]*)")
EXPECT_RE = re.compile(r"//\s*MOCK_EXPECT\s+(-?\d+)")
EXIT_TOTAL_RE = re.compile(r"//\s*MOCK_EXIT_TOTAL\b")
MAIN_RE = re.compile(r"//\s*MOCK_MAIN\b")


def _is_self(candidate: str) -> bool:
    try:
        self_real = Path(__file__).resolve()
        cand_path = Path(candidate)
        return cand_path.resolve() == self_real or cand_path.parent.resolve() == self_real.parent
    except OSError:
        return False


def _filtered_path() -> str:
    self_dirs = {
        str(Path(__file__).resolve().parent),
        str(Path(sys.argv[0]).absolute().parent),
    }
    entries = []
    for entry in os.environ.get("PATH", "").split(os.pathsep):
        if not entry:
            continue
        try:
            resolved = str(Path(entry).resolve())
        except OSError:
            resolved = entry
        if entry in self_dirs or resolved in self_dirs:
            continue
        entries.append(entry)
    return os.pathsep.join(entries)


def host_cc() -> str:
    clean_path = _filtered_path()
    preferred = os.environ.get("MOCK_SWIFTC_CC", "")
    if preferred:
        found = shutil.which(preferred, path=clean_path) or (preferred if Path(preferred).is_file() else None)
        if found and not _is_self(found):
            return found
        raise ValueError(f"Host C compiler unavailable: {preferred}")
    saved = Path(sys.argv[0]).absolute().parent.parent / "mock" / "host_cc.txt"
    if saved.is_file():
        recorded = saved.read_text(encoding="utf-8").strip()
        if recorded and Path(recorded).is_file() and not _is_self(recorded):
            return recorded
    for name in ("clang", "cc", "gcc"):
        if (found := shutil.which(name, path=clean_path)) and not _is_self(found):
            return found
    raise ValueError("Install clang, cc, or gcc.")


def target_triples() -> tuple[str, str]:
    arch = "arm64" if platform.machine() in {"arm64", "aarch64"} else "x86_64"
    if sys.platform == "darwin":
        return f"{arch}-apple-macosx15.0", f"{arch}-apple-macos"
    return f"{arch}-unknown-linux-gnu", f"{arch}-unknown-linux-gnu"


def _read_bounded_text(path: Path, max_bytes: int, label: str) -> tuple[str, int]:
    with open(path, "rb") as stream:
        st = os.fstat(stream.fileno())
        if st.st_size > max_bytes:
            raise ValueError(f"{label} exceeds {max_bytes} bytes: {path}")
        raw = stream.read(max_bytes + 1)
    if len(raw) > max_bytes:
        raise ValueError(f"{label} exceeds {max_bytes} bytes: {path}")
    return raw.decode("utf-8"), len(raw)


@dataclass
class _RspBudget:
    remaining_bytes: int = MAX_RSP_BYTES
    remaining_args: int = MAX_RSP_ARGS

    def consume_arg(self, source: str) -> None:
        if self.remaining_args <= 0:
            raise ValueError(f"Swift response file argument budget exceeded: {source}")
        self.remaining_args -= 1

    def read_tokens(self, path: Path) -> list[str]:
        if self.remaining_bytes <= 0:
            raise ValueError(f"Swift response file byte budget exceeded: {path}")
        content, nbytes = _read_bounded_text(path, self.remaining_bytes, "Swift response file")
        self.remaining_bytes -= nbytes
        tokens = shlex.split(content)
        if len(tokens) > self.remaining_args:
            raise ValueError(f"Swift response file argument budget exceeded: {path}")
        return tokens


def expand_response_files(
    argv: list[str],
    depth: int = 0,
    budget: _RspBudget | None = None,
) -> list[str]:
    if depth >= MAX_RSP_DEPTH:
        raise ValueError("Swift response file nesting limit exceeded.")
    if budget is None:
        budget = _RspBudget()
    out: list[str] = []
    for arg in argv:
        if arg.startswith("@") and len(arg) > 1:
            tokens = budget.read_tokens(Path(arg[1:]))
            out.extend(expand_response_files(tokens, depth + 1, budget))
        else:
            budget.consume_arg(arg)
            out.append(arg)
    return out


@dataclass
class SwiftInvocation:
    sources: list[Path] = field(default_factory=list)
    objects_and_libs: list[str] = field(default_factory=list)
    include_dirs: list[Path] = field(default_factory=list)
    defines: list[str] = field(default_factory=list)
    link_dirs: list[str] = field(default_factory=list)
    link_libs: list[str] = field(default_factory=list)
    linker_flags: list[str] = field(default_factory=list)
    output: Path | None = None
    output_file_map: Path | None = None
    module_name: str = ""
    module_link_name: str = ""
    module_path: Path | None = None
    objc_header_path: Path | None = None
    package_name: str = ""
    swift_version: str = ""
    mode: str = ""
    num_threads: str = ""
    compile_only: bool = False
    emit_module: bool = False
    emit_executable: bool = False
    emit_library: bool = False
    emit_dependencies: bool = False
    emit_objc_header: bool = False
    parse_as_library: bool = False
    static_lib: bool = False
    dry_run: bool = False
    used_rsp: bool = False


def parse_args(raw_argv: list[str]) -> SwiftInvocation:
    used_rsp = any(a.startswith("@") and len(a) > 1 for a in raw_argv)
    argv = expand_response_files(raw_argv)
    inv = SwiftInvocation(used_rsp=used_rsp)
    args = iter(argv)
    for arg in args:
        if arg == "-###":
            inv.dry_run = True
        elif arg == "-c":
            inv.compile_only = True
        elif arg == "-emit-module":
            inv.emit_module = True
        elif arg == "-emit-executable":
            inv.emit_executable = True
        elif arg == "-emit-library":
            inv.emit_library = True
        elif arg == "-emit-dependencies":
            inv.emit_dependencies = True
        elif arg == "-emit-objc-header":
            inv.emit_objc_header = True
        elif arg == "-parse-as-library":
            inv.parse_as_library = True
        elif arg == "-static":
            inv.static_lib = True
        elif arg == "-incremental":
            inv.mode = "incremental"
        elif arg in {"-wmo", "-whole-module-optimization"}:
            inv.mode = "wholemodule"
        elif arg == "-o":
            inv.output = Path(next(args, ""))
        elif arg == "-output-file-map":
            inv.output_file_map = Path(next(args, ""))
        elif arg == "-module-name":
            inv.module_name = next(args, "")
        elif arg == "-module-link-name":
            inv.module_link_name = next(args, "")
        elif arg == "-emit-module-path":
            inv.module_path = Path(next(args, ""))
        elif arg == "-emit-objc-header-path":
            inv.objc_header_path = Path(next(args, ""))
        elif arg == "-package-name":
            inv.package_name = next(args, "")
        elif arg == "-swift-version":
            inv.swift_version = next(args, "")
        elif arg == "-num-threads":
            inv.num_threads = next(args, "")
        elif arg in {"-j", "-target", "-tools-directory", "-sdk", "-libc",
                     "-serialize-diagnostics-path", "-emit-dependencies-path", "-F"}:
            _ = next(args, "")
        elif arg == "-I":
            inv.include_dirs.append(Path(next(args, "")))
        elif arg.startswith("-I") and len(arg) > 2:
            inv.include_dirs.append(Path(arg[2:]))
        elif arg == "-D":
            inv.defines.append(next(args, ""))
        elif arg.startswith("-D") and len(arg) > 2:
            inv.defines.append(arg[2:])
        elif arg == "-L":
            inv.link_dirs.append(next(args, ""))
        elif arg.startswith("-L") and len(arg) > 2:
            inv.link_dirs.append(arg[2:])
        elif arg == "-l":
            inv.link_libs.append(next(args, ""))
        elif arg.startswith("-l") and len(arg) > 2:
            inv.link_libs.append(arg[2:])
        elif arg == "-Xlinker":
            inv.linker_flags.append(next(args, ""))
        elif arg.startswith("-Xlinker="):
            inv.linker_flags.append(arg.split("=", 1)[1])
        elif arg.startswith("-"):
            # Ignore benign compiler flags like -Onone, -O, -g, -color-diagnostics.
            continue
        elif arg.endswith(".swift"):
            inv.sources.append(Path(arg))
        else:
            inv.objects_and_libs.append(arg)
    return inv


def resolve_imported_modules(sources: list[Path], include_dirs: list[Path]) -> list[tuple[str, bytes]]:
    _, mod_triple = target_triples()
    imported: list[tuple[str, bytes]] = []
    seen: set[str] = set()
    for src in sources:
        text = src.read_text(encoding="utf-8")
        for mod in IMPORT_RE.findall(text):
            if mod in {"Swift", "Foundation", "Darwin", "Glibc"} or mod in seen:
                continue
            seen.add(mod)
            found: Path | None = None
            for inc in include_dirs:
                for candidate in (
                    inc / f"{mod}.swiftmodule" / f"{mod_triple}.swiftmodule",
                    inc / f"{mod}.swiftmodule",
                ):
                    if candidate.is_file():
                        found = candidate
                        break
                if found is not None:
                    break
            if found is None:
                searched = ", ".join(str(d) for d in include_dirs) or "<none>"
                raise ValueError(
                    f"Cannot find Swift module '{mod}' imported by {src} (searched: {searched})"
                )
            imported.append((mod, found.read_bytes()))
    return imported


def sanitize_symbol(text: str) -> str:
    return re.sub(r"[^A-Za-z0-9_]", "_", text)


def compile_swift_source_to_object(
    cc: str,
    sources: list[Path],
    output_obj: Path,
    module_name: str,
    defines: list[str],
    is_executable: bool,
) -> None:
    lines: list[str] = ["#include <stdio.h>"]
    calls: list[str] = []
    expected: int | None = None
    exit_total = False
    has_main = False
    exported: set[str] = set()
    for idx, src in enumerate(sources):
        text = src.read_text(encoding="utf-8")
        # Unique per-source marker symbol so duplicate object outputs fail loudly.
        digest = hashlib.sha256(str(src.resolve()).encode("utf-8")).hexdigest()[:10]
        marker = f"rcmake_swift_unit_{sanitize_symbol(module_name or 'main')}_{digest}_{idx}"
        lines.append(f"int {marker}(void) {{ return 1; }}")
        for sym, val in EXPORT_RE.findall(text):
            if sym not in exported:
                exported.add(sym)
                lines.append(f"int {sym}(void) {{ return {val}; }}")
        for sym in CALL_RE.findall(text):
            calls.append(sym)
        if m := EXPECT_RE.search(text):
            expected = int(m.group(1))
        if EXIT_TOTAL_RE.search(text):
            exit_total = True
        if MAIN_RE.search(text):
            has_main = True

    if has_main or (is_executable and not exported and not calls):
        for sym in dict.fromkeys(calls):
            if sym not in exported:
                lines.append(f"extern int {sym}(void);")
        body = " + ".join(f"{sym}()" for sym in calls) if calls else "0"
        if exit_total:
            lines.append(f"int main(void) {{ return {body}; }}")
        elif expected is not None:
            lines.append(
                f"int main(void) {{ int total = {body}; return total == {expected} ? 0 : 1; }}"
            )
        else:
            lines.append(f"int main(void) {{ (void)({body}); return 0; }}")

    output_obj.parent.mkdir(parents=True, exist_ok=True)
    c_code = ("\n".join(lines) + "\n").encode("utf-8")
    cmd = [cc, "-fPIC", "-x", "c", "-c", "-", "-o", str(output_obj)]
    for define in defines:
        cmd.append(f"-D{define}")
    subprocess.run(cmd, input=c_code, check=True, timeout=TIMEOUT)


def write_if_changed(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.is_file() and path.read_bytes() == data:
        return
    path.write_bytes(data)


def emit_swiftmodule_and_aux(
    inv: SwiftInvocation,
    imported: list[tuple[str, bytes]],
    output_map: dict[str, dict[str, str]] | None,
) -> list[str]:
    emitted: list[str] = []
    h = hashlib.sha256()
    h.update((inv.module_name + "\n").encode("utf-8"))
    h.update((inv.package_name + "\n").encode("utf-8"))
    h.update((inv.swift_version + "\n").encode("utf-8"))
    for d in sorted(inv.defines):
        h.update((d + "\n").encode("utf-8"))
    for src in inv.sources:
        h.update(src.read_bytes())
    for mod, mod_bytes in imported:
        h.update(mod.encode("utf-8"))
        h.update(mod_bytes)
    mod_payload = (
        f"MOCK_SWIFTMODULE {inv.module_name} pkg={inv.package_name} "
        f"ver={inv.swift_version} sha={h.hexdigest()}\n"
    ).encode("utf-8")

    if inv.emit_module and inv.module_path is not None:
        write_if_changed(inv.module_path, mod_payload)
        emitted.append(str(inv.module_path.resolve()))
    if inv.emit_objc_header and inv.objc_header_path is not None:
        hdr = f"/* Mock ObjC header for {inv.module_name} */\n#pragma once\n".encode("utf-8")
        write_if_changed(inv.objc_header_path, hdr)
        emitted.append(str(inv.objc_header_path.resolve()))
    if output_map is not None:
        top = output_map.get("", {})
        for key in ("swift-dependencies", "dependencies", "diagnostics"):
            if out_path := top.get(key):
                write_if_changed(Path(out_path), f"# {key} {inv.module_name}\n".encode("utf-8"))
        for src in inv.sources:
            entry = lookup_map_entry(output_map, src)
            for key in ("swift-dependencies", "dependencies", "diagnostics", "swiftmodule"):
                if out_path := entry.get(key):
                    payload = mod_payload if key == "swiftmodule" else f"# {key} {src.name}\n".encode("utf-8")
                    write_if_changed(Path(out_path), payload)
    return emitted


def lookup_map_entry(output_map: dict[str, dict[str, str]], src: Path) -> dict[str, str]:
    for candidate in (str(src), str(src.resolve()), os.path.abspath(str(src))):
        if candidate in output_map:
            return output_map[candidate]
    raise ValueError(f"Source {src} missing from output-file-map keys: {sorted(output_map.keys())}")


def log_event(entry: dict) -> int:
    if path := os.environ.get("MOCK_SWIFTC_LOG"):
        with open(path, "a", encoding="utf-8") as stream:
            stream.write(json.dumps(entry) + "\n")
    return int(entry["exit_code"])


def install(root: Path) -> None:
    root.mkdir(parents=True, exist_ok=True)
    if any(root.iterdir()):
        raise ValueError(f"Use an empty directory for the mock Swift toolchain: {root}")
    bin_dir = root / "bin"
    bin_dir.mkdir(parents=True)
    (root / "mock").mkdir(parents=True)
    (root / "mock" / "host_cc.txt").write_text(host_cc() + "\n", encoding="utf-8")
    swiftc = bin_dir / "swiftc"
    shutil.copyfile(Path(__file__).resolve(), swiftc)
    swiftc.chmod(0o755)


def link_outputs(cc: str, inv: SwiftInvocation, extra_objects: list[Path]) -> int:
    if inv.output is None:
        raise ValueError("Swift link invocation requires -o <output>.")
    inv.output.parent.mkdir(parents=True, exist_ok=True)
    all_inputs = [str(p) for p in extra_objects] + inv.objects_and_libs
    if inv.emit_library and inv.static_lib:
        if inv.output.exists():
            inv.output.unlink()
        return subprocess.run(["ar", "crs", str(inv.output), *all_inputs], timeout=TIMEOUT).returncode

    cmd = [cc]
    if inv.emit_library:
        cmd.append("-shared")
    cmd.extend(["-o", str(inv.output)])
    cmd.extend(all_inputs)
    for ldir in inv.link_dirs:
        cmd.extend(["-L", ldir])
    for lib in inv.link_libs:
        cmd.append(f"-l{lib}")
    for flag in inv.linker_flags:
        cmd.extend(["-Xlinker", flag])
    return subprocess.run(cmd, timeout=TIMEOUT).returncode


def run(argv: list[str]) -> int:
    if argv == ["--help"]:
        print(__doc__)
        return 0
    if os.name != "posix":
        raise ValueError("The mock Swift toolchain requires POSIX host tools.")
    depth = int(os.environ.get("MOCK_SWIFTC_DEPTH", "0"))
    if depth >= 2:
        raise ValueError(f"Recursive mock swiftc invocation detected (depth={depth}).")
    os.environ["MOCK_SWIFTC_DEPTH"] = str(depth + 1)
    if argv[:1] == ["--install-mock-swift"]:
        if len(argv) != 2:
            raise ValueError("Use --install-mock-swift DIRECTORY.")
        install(Path(argv[1]).resolve())
        return 0

    version = os.environ.get("MOCK_SWIFTC_VERSION", DEFAULT_VERSION)
    triple, mod_triple = target_triples()
    if any(arg in {"-version", "--version"} for arg in argv):
        print(f"Apple Swift version {version} (mock-swiftc)")
        print(f"Target: {triple}")
        return 0
    if "-print-target-info" in argv:
        info = {
            "compilerVersion": f"Apple Swift version {version} (mock-swiftc)",
            "target": {
                "triple": triple,
                "unversionedTriple": mod_triple,
                "moduleTriple": mod_triple,
                "pointerWidthInBytes": 8,
            },
        }
        print(json.dumps(info))
        return 0

    inv = parse_args(argv)
    if inv.dry_run:
        if os.environ.get("MOCK_SWIFTC_OLD_DRIVER") == "1":
            print('"/mock/swift-frontend" -frontend -c main.swift lib.in -o main.o')
        else:
            print('"/mock/swift-frontend" -frontend -c main.swift -module-name main -o main.o')
            print('"/mock/ld" main.o lib.in -o main')
        return 0

    cc = host_cc()
    output_map: dict[str, dict[str, str]] | None = None
    if inv.output_file_map is not None:
        if not inv.output_file_map.is_file():
            raise ValueError(f"Missing output-file-map: {inv.output_file_map}")
        map_text, _ = _read_bounded_text(
            inv.output_file_map, MAX_OUTPUT_MAP_BYTES, "output-file-map"
        )
        output_map = json.loads(map_text)
        if not isinstance(output_map, dict):
            raise ValueError("output-file-map must be a JSON object.")

    imported = resolve_imported_modules(inv.sources, inv.include_dirs) if inv.sources else []

    # 1. Pure emit-module phase (CMP0215 separate module action).
    if inv.emit_module and not inv.compile_only and not inv.emit_executable and not inv.emit_library:
        if inv.module_path is None:
            raise ValueError("-emit-module requires -emit-module-path.")
        emitted = emit_swiftmodule_and_aux(inv, imported, output_map)
        return log_event({
            "phase": "emit-module",
            "argv": argv,
            "module_name": inv.module_name,
            "package_name": inv.package_name,
            "swift_version": inv.swift_version,
            "mode": inv.mode or "singlefile",
            "used_rsp": inv.used_rsp,
            "sources": [str(s.resolve()) for s in inv.sources],
            "outputs": emitted,
            "exit_code": 0,
        })

    # 2. Split compile phase (-c, optionally combined with -emit-module).
    if inv.compile_only and not inv.emit_executable and not inv.emit_library:
        if not inv.sources:
            raise ValueError("Swift compile invocation (-c) requires at least one .swift source.")
        emitted: list[str] = []
        is_exe = not inv.parse_as_library
        if inv.output is not None and output_map is None:
            compile_swift_source_to_object(
                cc, inv.sources, inv.output, inv.module_name, inv.defines, is_exe
            )
            emitted.append(str(inv.output.resolve()))
        elif output_map is not None:
            seen_objects: set[str] = set()
            for src in inv.sources:
                entry = lookup_map_entry(output_map, src)
                obj = entry.get("object")
                if not obj:
                    raise ValueError(f"Missing 'object' mapping for {src} in {inv.output_file_map}")
                obj_path = Path(obj)
                norm = str(obj_path.resolve())
                if norm in seen_objects:
                    raise ValueError(f"Duplicate object path in output-file-map: {obj_path}")
                seen_objects.add(norm)
                compile_swift_source_to_object(
                    cc, [src], obj_path, inv.module_name, inv.defines, is_exe
                )
                emitted.append(norm)
        else:
            raise ValueError("Swift -c requires either -o <object> or -output-file-map <json>.")

        emitted.extend(emit_swiftmodule_and_aux(inv, imported, output_map))
        return log_event({
            "phase": "compile-combined" if inv.emit_module else "compile",
            "argv": argv,
            "module_name": inv.module_name,
            "package_name": inv.package_name,
            "swift_version": inv.swift_version,
            "mode": inv.mode or "singlefile",
            "used_rsp": inv.used_rsp,
            "sources": [str(s.resolve()) for s in inv.sources],
            "outputs": emitted,
            "exit_code": 0,
        })

    # 3. Link phase (split link with .o inputs, OR legacy CMP0157 OLD compile+link with .swift inputs).
    if inv.emit_executable or inv.emit_library:
        with tempfile.TemporaryDirectory(prefix="mock-swift-link-") as tmp:
            temp_objs: list[Path] = []
            if inv.sources:
                is_exe = inv.emit_executable and not inv.parse_as_library
                for idx, src in enumerate(inv.sources):
                    obj = Path(tmp) / f"src_{idx}.o"
                    compile_swift_source_to_object(
                        cc, [src], obj, inv.module_name, inv.defines, is_exe
                    )
                    temp_objs.append(obj)
                emit_swiftmodule_and_aux(inv, imported, output_map)
            exit_code = link_outputs(cc, inv, temp_objs)
        return log_event({
            "phase": "legacy-compile-link" if inv.sources else "link",
            "argv": argv,
            "module_name": inv.module_name,
            "package_name": inv.package_name,
            "swift_version": inv.swift_version,
            "mode": inv.mode or "singlefile",
            "used_rsp": inv.used_rsp,
            "sources": [str(s.resolve()) for s in inv.sources],
            "outputs": [str(inv.output.resolve())] if inv.output else [],
            "exit_code": exit_code,
        })

    # Fallback: compile single file (e.g., CompilerId or simple probe).
    if inv.sources:
        out = inv.output or Path("a.out")
        with tempfile.TemporaryDirectory(prefix="mock-swift-probe-") as tmp:
            obj = Path(tmp) / "probe.o"
            compile_swift_source_to_object(cc, inv.sources, obj, inv.module_name, inv.defines, True)
            exit_code = subprocess.run([cc, "-o", str(out), str(obj)], timeout=TIMEOUT).returncode
        return log_event({
            "phase": "probe",
            "argv": argv,
            "outputs": [str(out.resolve())],
            "exit_code": exit_code,
        })

    raise ValueError(f"Unsupported mock swiftc invocation: {argv}")


if __name__ == "__main__":
    try:
        raise SystemExit(run(sys.argv[1:]))
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"mock swiftc: {error}", file=sys.stderr)
        raise SystemExit(1)
