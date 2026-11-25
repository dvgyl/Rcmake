# SPDX-License-Identifier: MIT OR Apache-2.0
"""Scenario context, registry, normalization, and inspection helpers."""

from __future__ import annotations

from dataclasses import dataclass
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import tarfile
import textwrap
import time
from typing import Any, Callable, Iterable, Mapping, Sequence
import xml.etree.ElementTree as ET
import zipfile

from .sandbox import (
    DARWIN,
    DEFAULT_TIMEOUT,
    Proc,
    StepFailed,
    Toolset,
    child_env,
    run_process,
)


SINGLE_CONFIG = ("Ninja", "Unix Makefiles")
MULTI_CONFIG = ("Ninja Multi-Config", "Xcode")
TOOL_PATH_ENTRIES = {
    "CMAKE_COMMAND": "<CMAKE>",
    "CMAKE_CTEST_COMMAND": "<CTEST>",
    "CMAKE_CPACK_COMMAND": "<CPACK>",
    "CMAKE_ROOT": "<CMAKE_ROOT>",
}
FIXED_MTIME = "2001-02-03 04:05:06 UTC"
FIXED_EPOCH = 981173106


@dataclass
class Check:
    """One assertion result."""

    name: str
    ok: bool
    detail: str = ""


@dataclass
class Options:
    """Settings that every scenario can read."""

    jobs: int = 4
    timeout: float = DEFAULT_TIMEOUT
    xcode: bool = True
    archive_mib: int = 520
    keep: bool = False


class Ctx:
    """The state of one scenario run with one tool set."""

    def __init__(self, tools: Toolset, root: Path, options: Options) -> None:
        self.tools = tools
        self.root = root
        self.options = options
        self.checks: list[Check] = []
        self.skips: list[str] = []
        self.observations: dict[str, Any] = {}
        self.transcript: list[str] = []
        self.aliases: list[tuple[str, str]] = []
        root.mkdir(parents=True, exist_ok=True)

    def write(self, rel: str | Path, text: str, base: Path | None = None) -> Path:
        """Write dedented text to a file below the root and return its path."""
        path = (base or self.root) / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        content = textwrap.dedent(text)
        if content.startswith("\n"):
            content = content[1:]
        path.write_text(content, encoding="utf-8")
        return path

    def tree(self, base: Path, files: Mapping[str, str]) -> Path:
        """Write a mapping of relative paths to dedented text below base."""
        for rel, text in files.items():
            self.write(rel, text, base)
        return base

    def run(
        self, argv: Sequence[str | os.PathLike[str]], cwd: Path | None = None,
        env: Mapping[str, str | None] | None = None, timeout: float | None = None,
        stdin: str | bytes | None = None,
    ) -> Proc:
        """Run a process with the clean environment."""
        proc = run_process(
            argv, cwd or self.root, child_env(env), timeout or self.options.timeout, stdin,
        )
        self.transcript.append(f"[{proc.returncode}] {' '.join(proc.argv)} ({proc.seconds:.2f}s)")
        return proc

    def cmake(self, *args: str | os.PathLike[str], **kw: Any) -> Proc:
        """Run the cmake program of this tool set."""
        return self.run([self.tools.cmake, *args], **kw)

    def ctest(self, *args: str | os.PathLike[str], **kw: Any) -> Proc:
        """Run the ctest program of this tool set."""
        return self.run([self.tools.ctest, *args], **kw)

    def cpack(self, *args: str | os.PathLike[str], **kw: Any) -> Proc:
        """Run the cpack program of this tool set."""
        return self.run([self.tools.cpack, *args], **kw)

    def configure(self, src: Path, build: Path, generator: str, *args: str, **kw: Any) -> Proc:
        """Configure a project with a generator."""
        extra = list(args)
        if generator in SINGLE_CONFIG and not any(a.startswith("-DCMAKE_BUILD_TYPE") for a in extra):
            extra.append("-DCMAKE_BUILD_TYPE=Release")
        return self.cmake("-S", src, "-B", build, "-G", generator, *extra, **kw)

    def build(self, build: Path, generator: str, *args: str, config: str = "Release", **kw: Any) -> Proc:
        """Build a tree with the configured parallel job count."""
        extra = ["--config", config] if generator in MULTI_CONFIG else []
        return self.cmake("--build", build, "-j", str(self.options.jobs), *extra, *args, **kw)

    def check(self, name: str, ok: bool, detail: Any = "") -> bool:
        """Record one assertion."""
        self.checks.append(Check(name, bool(ok), "" if ok else str(detail)[:4000]))
        return bool(ok)

    def require(self, name: str, ok: bool, detail: Any = "") -> None:
        """Record one assertion. Stop the scenario when it fails."""
        if not self.check(name, ok, detail):
            raise StepFailed(name)

    def ok(self, name: str, proc: Proc, required: bool = True) -> bool:
        """Assert that a process exited with status 0."""
        if required:
            self.require(name, proc.ok, proc.tail())
            return True
        return self.check(name, proc.ok, proc.tail())

    def fails(self, name: str, proc: Proc) -> bool:
        """Assert that a process exited with a nonzero status."""
        return self.check(name, proc.returncode != 0 and not proc.timed_out, proc.tail())

    def skip(self, reason: str) -> None:
        """Record a sub-case that cannot run on this host."""
        self.skips.append(reason)

    def observe(self, key: str, value: Any) -> None:
        """Record a normalized value for the differential comparison."""
        self.observations[key] = self.normalize(value)

    def learn_tool_paths(self, build: Path) -> None:
        """Read the tool paths that a configured tree records and alias them.

        A tool can resolve symbolic links in its own path. The cache then
        holds a path that is not the path of the Toolset. The aliases give
        both paths the same placeholder.
        """
        entries = cache_entries(build, TOOL_PATH_ENTRIES)
        for name, placeholder in TOOL_PATH_ENTRIES.items():
            value = entries.get(name, "").partition("=")[2]
            if value and (value, placeholder) not in self.aliases:
                self.aliases.append((value, placeholder))

    def normalize(self, value: Any) -> Any:
        """Replace host-specific paths in nested values."""
        if isinstance(value, str):
            return normalize_text(value, self.root, self.tools, getattr(self, "aliases", ()))
        if isinstance(value, Mapping):
            return {self.normalize(k): self.normalize(v) for k, v in value.items()}
        if isinstance(value, (list, tuple)):
            return [self.normalize(item) for item in value]
        return value


@dataclass
class Scenario:
    """A named scenario function."""

    sid: str
    title: str
    func: Callable[[Ctx], None]
    requires: Callable[[Options], str | None] | None = None


SCENARIOS: list[Scenario] = []


def scenario(sid: str, title: str, requires: Callable[[Options], str | None] | None = None):
    """Register a scenario function."""
    def wrap(func: Callable[[Ctx], None]) -> Callable[[Ctx], None]:
        SCENARIOS.append(Scenario(sid, title, func, requires))
        SCENARIOS.sort(key=lambda sc: sc.sid)
        return func
    return wrap


def need_darwin(_options: Options) -> str | None:
    """Skip a scenario when the host is not macOS."""
    return None if DARWIN else "needs macOS"


def normalize_text(
    text: str, root: Path, tools: Toolset | None = None, aliases: Iterable[tuple[str, str]] = (),
) -> str:
    """Replace the run root, tool paths, and aliases with stable placeholders."""
    pairs: list[tuple[str, str]] = list(aliases)
    for candidate in {str(root), os.path.realpath(root)}:
        pairs.append((candidate, "<ROOT>"))
    if tools is not None:
        for path in (tools.cmake, tools.ctest, tools.cpack):
            pairs.append((str(path), f"<{path.name.upper()}>"))
    for old, new in sorted(pairs, key=lambda pair: -len(pair[0])):
        text = text.replace(old, new)
    text = re.sub(r"\(\d+(?:\.\d+)?s\)", "(<T>s)", text)
    text = re.sub(r"\d+\.\d+ sec", "<T> sec", text)
    return text.replace("\r\n", "\n")


def diff_observations(left: Mapping[str, Any], right: Mapping[str, Any]) -> list[dict[str, Any]]:
    """Compare two observation maps and return the differences."""
    out = []
    for key in sorted(set(left) | set(right)):
        if left.get(key, "<missing>") != right.get(key, "<missing>"):
            out.append({"key": key, "oracle": left.get(key, "<missing>"), "rcmake": right.get(key, "<missing>")})
    return out


def generators_available(options: Options) -> list[str]:
    """Return the CMake generators that this host can run."""
    found = []
    if shutil.which("ninja"):
        found += ["Ninja", "Ninja Multi-Config"]
    if shutil.which("make"):
        found.append("Unix Makefiles")
    if DARWIN and options.xcode and shutil.which("xcodebuild"):
        probe = subprocess.run(["xcodebuild", "-version"], capture_output=True, timeout=60)
        if probe.returncode == 0:
            found.append("Xcode")
    return found


def slug(text: str) -> str:
    """Make a short file-name-safe name."""
    return re.sub(r"[^A-Za-z0-9]+", "-", text).strip("-").lower()


def exe_path(build: Path, name: str, generator: str, config: str = "Release", subdir: str = "") -> Path:
    """Return the path of a built executable for a generator layout."""
    base = build / subdir if subdir else build
    if generator in MULTI_CONFIG:
        return base / config / name
    return base / name


def is_appledouble(name: str) -> bool:
    """Return True for macOS metadata entries such as ._file and .DS_Store."""
    base = name.rstrip("/").rsplit("/", 1)[-1]
    return base.startswith("._") or base == ".DS_Store"


def file_listing(top: Path) -> list[str]:
    """List the files and links below a directory as sorted relative paths."""
    items = []
    for path in sorted(top.rglob("*")):
        if path.name.startswith("._") or path.name == ".DS_Store":
            continue
        rel = path.relative_to(top).as_posix()
        if path.is_symlink():
            items.append(f"{rel} -> {os.readlink(path)}")
        elif path.is_file():
            items.append(rel)
    return items


def tree_digest(top: Path) -> list[str]:
    """Describe each entry below top: kind, permission bits, and content hash.

    Directory permission bits are not included because they depend on the
    umask of the process that made the directory.
    """
    items = []
    for path in sorted(top.rglob("*")):
        rel = path.relative_to(top).as_posix()
        if is_appledouble(rel):
            continue
        if path.is_symlink():
            items.append(f"{rel} -> {os.readlink(path)}")
        elif path.is_dir():
            items.append(f"{rel}/")
        else:
            mode = stat.S_IMODE(path.stat().st_mode)
            digest = hashlib.sha256(path.read_bytes()).hexdigest()[:16]
            items.append(f"{rel} {mode:o} {digest}")
    return items


def cache_entries(build: Path, names: Iterable[str]) -> dict[str, str]:
    """Read selected entries from CMakeCache.txt."""
    wanted = set(names)
    found: dict[str, str] = {}
    cache = build / "CMakeCache.txt"
    if not cache.is_file():
        return found
    for line in cache.read_text(encoding="utf-8", errors="replace").splitlines():
        match = re.match(r"^([^#/][^:=]*):([A-Z]+)=(.*)$", line)
        if match and match.group(1) in wanted:
            found[match.group(1)] = f"{match.group(2)}={match.group(3)}"
    return found


def touch_later(path: Path, text: str | None = None) -> None:
    """Rewrite a file so that its time stamp is newer than every build output.

    GNU Make 3.81 compares time stamps with one second resolution. The wait
    makes sure that each edit is seen as newer than the previous outputs.
    """
    time.sleep(1.1)
    if text is not None:
        path.write_text(text, encoding="utf-8")
    else:
        path.touch()


def no_op(proc: Proc) -> bool:
    """Return True when a build did not compile or link anything."""
    return proc.ok and not re.search(r"Building \w+ object|Linking |Generating |Building \w+ precompiled", proc.output)


def compiled(proc: Proc, pattern: str) -> bool:
    """Return True when a build output shows that it compiled a matching object."""
    return bool(re.search(r"Building \w+ object \S*" + pattern, proc.output))


def archive_names(path: Path) -> list[str]:
    """List the file members of a .tar.gz or .zip archive.

    On macOS, libarchive adds an AppleDouble "._<name>" member for each file
    with extended attributes. macOS adds com.apple.provenance to new files, so
    these members depend on the host and are not part of the comparison.
    """
    if path.suffix == ".zip":
        with zipfile.ZipFile(path) as zf:
            names = [name for name in zf.namelist() if not name.endswith("/")]
    else:
        with tarfile.open(path) as tar:
            names = [m.name for m in tar.getmembers() if not m.isdir()]
    return sorted(name for name in names if not Path(name).name.startswith("._"))


def archive_kind(path: Path) -> str:
    """Identify an archive from its first bytes.

    A plain tar file is "tar-gnu" when it has the GNU magic and "tar-posix"
    when it has the POSIX ustar magic.
    """
    try:
        head = path.read_bytes()[:512]
    except OSError:
        return "missing"
    magics = (
        (b"\x1f\x8b", "gzip"), (b"BZh", "bzip2"), (b"\xfd7zXZ\x00", "xz"),
        (b"\x28\xb5\x2f\xfd", "zstd"), (b"PK\x03\x04", "zip"), (b"7z\xbc\xaf\x27\x1c", "7zip"),
    )
    for magic, kind in magics:
        if head.startswith(magic):
            return kind
    if head[257:265] == b"ustar  \x00":
        return "tar-gnu"
    if head[257:263] == b"ustar\x00":
        return "tar-posix"
    return "unknown"


def listing_lines(text: str) -> list[str]:
    """Return archive member names from `cmake -E tar tf` output.

    The function removes macOS metadata members and the trailing slash of
    directory members.
    """
    return [line.rstrip("/") for line in text.splitlines() if line and not is_appledouble(line)]


def python_members(path: Path) -> list[tuple[str, int]]:
    """Read member names and modification times with the Python library."""
    if zipfile.is_zipfile(path):
        with zipfile.ZipFile(path) as zf:
            return [(info.filename.rstrip("/"), 0) for info in zf.infolist() if not is_appledouble(info.filename)]
    with tarfile.open(path) as tar:
        return [(m.name, int(m.mtime)) for m in tar.getmembers() if not is_appledouble(m.name)]


def write_tar_gz(path: Path, top: str, files: Mapping[str, str]) -> None:
    """Write a reproducible .tar.gz file with every file below one top directory."""
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.PAX_FORMAT) as tar:
        for rel, text in sorted(files.items()):
            data = textwrap.dedent(text).lstrip("\n").encode()
            info = tarfile.TarInfo(f"{top}/{rel}")
            info.size, info.mtime, info.mode = len(data), FIXED_EPOCH, 0o644
            tar.addfile(info, io.BytesIO(data))
    path.parent.mkdir(parents=True, exist_ok=True)
    # An empty file name and a fixed time keep the gzip header equal for every run.
    with open(path, "wb") as handle, gzip.GzipFile("", "wb", fileobj=handle, mtime=FIXED_EPOCH) as gz:
        gz.write(raw.getvalue())


def tool_output(binary: Path, *args: str) -> str:
    """Run a binary inspection tool and return its stdout, or "" on error."""
    try:
        return subprocess.run([*args, str(binary)], capture_output=True, text=True, timeout=60).stdout
    except (OSError, subprocess.SubprocessError):
        return ""


def macho_rpaths(binary: Path) -> list[str]:
    """Return the LC_RPATH entries of a Mach-O file."""
    out = subprocess.run(["otool", "-l", str(binary)], capture_output=True, text=True, timeout=60).stdout
    return re.findall(r"cmd LC_RPATH\n\s+cmdsize \d+\n\s+path (.*?) \(offset \d+\)", out)


def macho_id(binary: Path) -> str:
    """Return the install name of a Mach-O shared library."""
    out = subprocess.run(["otool", "-D", str(binary)], capture_output=True, text=True, timeout=60).stdout
    lines = out.strip().splitlines()
    return lines[1].strip() if len(lines) > 1 else ""


def macho_load_dylibs(binary: Path) -> list[str]:
    """Return the LC_LOAD_DYLIB names of a Mach-O file, without system libraries."""
    names = re.findall(r"cmd LC_LOAD_DYLIB\n\s+cmdsize \d+\n\s+name (.*?) \(offset \d+\)",
                       tool_output(binary, "otool", "-l"))
    return [name for name in names if not name.startswith(("/usr/lib/", "/System/"))]


def macho_versions(binary: Path, lib: str) -> str:
    """Return the compatibility and current version that a binary records for a library."""
    for line in tool_output(binary, "otool", "-L").splitlines()[1:]:
        if line.strip().startswith(lib):
            match = re.search(r"\((compatibility version [\d.]+, current version [\d.]+)\)", line)
            return match.group(1) if match else ""
    return ""


def elf_dynamic(binary: Path) -> dict[str, list[str]]:
    """Return the NEEDED, SONAME, RPATH, and RUNPATH entries of an ELF file."""
    out: dict[str, list[str]] = {}
    for match in re.finditer(r"\((NEEDED|SONAME|RPATH|RUNPATH)\)\s+[^\[]*\[(.*)\]",
                             tool_output(binary, "readelf", "-d")):
        out.setdefault(match.group(1), []).append(match.group(2))
    return out


def exported_symbols(binary: Path) -> list[str]:
    """Return the defined external symbol names of a shared library."""
    args = ("nm", "-gU") if DARWIN else ("nm", "-D", "--defined-only")
    names = []
    for line in tool_output(binary, *args).splitlines():
        parts = line.split()
        if len(parts) >= 3:
            names.append(parts[-1].lstrip("_") if DARWIN else parts[-1])
    return sorted(names)


def shared_names(name: str, version: str, soversion: str) -> tuple[str, str]:
    """Return the real file name and the SONAME of a versioned shared library."""
    if DARWIN:
        return f"lib{name}.{version}.dylib", f"@rpath/lib{name}.{soversion}.dylib"
    return f"lib{name}.so.{version}", f"lib{name}.so.{soversion}"


def result(proc: Proc) -> list[Any]:
    """Return the exit status, stdout, and stderr of a process for an observation."""
    return [proc.returncode, proc.stdout, proc.stderr]


def message_lines(text: str, *prefixes: str) -> list[str]:
    """Return the lines of text that start with one of the prefixes."""
    return [line for line in text.splitlines() if line.startswith(prefixes)]


def cmake_errors(text: str) -> list[str]:
    """Return each "CMake Error" or "CMake Warning" block of a configure output.

    A block starts at a "CMake Error" or "CMake Warning" line and ends before
    the next line that starts with "-- " or "CMake ". Spacing is collapsed.
    """
    blocks: list[str] = []
    current: list[str] | None = None
    for line in text.splitlines():
        if line.startswith(("CMake Error", "CMake Warning")):
            if current is not None:
                blocks.append(" ".join(" ".join(current).split()))
            current = [line]
        elif line.startswith(("-- ", "CMake ")):
            if current is not None:
                blocks.append(" ".join(" ".join(current).split()))
            current = None
        elif current is not None:
            current.append(line)
    if current is not None:
        blocks.append(" ".join(" ".join(current).split()))
    return blocks


def collapse_repeats(blocks: Sequence[str]) -> list[str]:
    """Return the diagnostic blocks with consecutive identical blocks given once.

    Some CMake errors come from internal functions that several call sites
    use, with no deduplication. For example, cmTarget::ImportedGetFullPath
    gives the same CMP0111 error one time for each caller. The number of
    copies is an implementation detail. The text and the order are not.
    """
    out: list[str] = []
    for block in blocks:
        if not out or out[-1] != block:
            out.append(block)
    return out


def junit_cases(path: Path) -> list[str]:
    """Return 'name:status' for each test case of a JUnit file.

    CTest writes status="run" for a test that passed. A failed test also has
    a <failure> element.
    """
    cases = []
    for case in ET.parse(path).getroot().iter("testcase"):
        status = case.get("status", "")
        if case.find("failure") is not None:
            status += "+failure"
        cases.append(f"{case.get('name')}:{status}")
    return sorted(cases)


def sarif_digest(path: Path) -> list[dict[str, Any]]:
    """Reduce a SARIF log to levels, messages, locations, and stack frames."""
    data = json.loads(path.read_text(encoding="utf-8"))
    digest = []
    for run in data.get("runs", []):
        for item in run.get("results", []):
            def where(location: Mapping[str, Any]) -> str:
                phys = location.get("physicalLocation", {})
                names = [loc.get("name") for loc in location.get("logicalLocations", [])]
                return (f"{phys.get('artifactLocation', {}).get('uriBaseId', '')}:"
                        f"{phys.get('artifactLocation', {}).get('uri', '')}:"
                        f"{phys.get('region', {}).get('startLine', '')}:{','.join(map(str, names))}")
            digest.append({
                "ruleId": item.get("ruleId"),
                "level": item.get("level"),
                "message": item.get("message", {}).get("text"),
                "locations": [where(loc) for loc in item.get("locations", [])],
                "stacks": [[where(frame.get("location", {})) for frame in stack.get("frames", [])]
                           for stack in item.get("stacks", [])],
            })
    return digest
