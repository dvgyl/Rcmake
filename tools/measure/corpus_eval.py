#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Build the pinned C/C++ corpus and compare CMake and Rcmake results."""

from __future__ import annotations

import argparse
from collections import Counter
import hashlib
from http.client import HTTPException
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import signal
import stat
import subprocess
import sys
import tarfile
import tempfile
import time
import tomllib
from urllib.request import urlopen


ROOT = Path(__file__).resolve().parents[2]
MANIFEST = ROOT / "conformance/corpus/manifest.toml"
CACHE = ROOT / "env/corpus/cache"
STAGES = ("configure", "build", "install", "test")
JOBS = "2"
ORACLE_VERSION = "4.5.1"
DOWNLOAD_TIMEOUT = 1800
MAX_ARCHIVE_BYTES = 512 * 1024 * 1024


def cached_archive(pin: dict) -> Path:
    """Return a cache entry only after checking its SHA-256 digest."""
    sha256 = pin["sha256"]
    if not re.fullmatch(r"[0-9a-f]{64}", sha256):
        raise ValueError(f"Invalid SHA-256 for {pin['url']}")
    CACHE.mkdir(parents=True, exist_ok=True)
    # Use the repository's existing ignore rule for temporary files.
    archive = CACHE / f"{sha256}.tar.tmp"
    if archive.is_file():
        with archive.open("rb") as source:
            if hashlib.file_digest(source, "sha256").hexdigest() == sha256:
                return archive

    print(f"Downloading {pin['url']}", flush=True)
    with tempfile.NamedTemporaryFile(dir=CACHE, suffix=".tmp", delete=False) as target:
        partial = Path(target.name)
    try:
        digest = hashlib.sha256()
        deadline = time.monotonic() + DOWNLOAD_TIMEOUT
        with urlopen(pin["url"], timeout=60) as source, partial.open("wb") as target:
            while chunk := source.read1(1024 * 1024):
                if time.monotonic() >= deadline:
                    raise TimeoutError(f"Download timed out for {pin['url']}")
                if target.tell() + len(chunk) > MAX_ARCHIVE_BYTES:
                    raise ValueError(f"Download is too large for {pin['url']}")
                target.write(chunk)
                digest.update(chunk)
        if digest.hexdigest() != sha256:
            raise ValueError(f"SHA-256 mismatch for {pin['url']}: got {digest.hexdigest()}")
        partial.replace(archive)
    finally:
        partial.unlink(missing_ok=True)
    return archive


def extract(archive: Path, destination: Path) -> Path:
    """Extract a verified tarball and return its one top-level directory."""
    destination.mkdir()
    with tarfile.open(archive) as source:
        source.extractall(destination, filter="data")
    roots = list(destination.iterdir())
    if len(roots) != 1 or roots[0].is_symlink() or not roots[0].is_dir():
        raise ValueError(f"Expected one source directory in {archive.name}")
    return roots[0]


def run_process(command: list[str], cwd: Path, timeout: int, env: dict[str, str]) -> dict:
    """Run one command and stop its process group if the timeout expires."""
    start = time.monotonic()
    result = {"returncode": None}
    try:
        with subprocess.Popen(
            command, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            text=True, encoding="utf-8", errors="replace", start_new_session=True,
        ) as process:
            try:
                output, _ = process.communicate(timeout=timeout)
                result["returncode"] = process.returncode
            except (subprocess.TimeoutExpired, KeyboardInterrupt) as error:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                output, _ = process.communicate()
                if isinstance(error, KeyboardInterrupt):
                    raise
                result["error"] = f"Command timed out after {timeout} seconds."
            result["output"] = output
    except OSError as error:
        result["error"] = str(error)
    result["seconds"] = round(time.monotonic() - start, 2)
    return result


def make_git_snapshot(source: Path, timeout: int, env: dict[str, str]) -> None:
    """Make the extracted source available to tests that clone the project."""
    git_env = dict(env, GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull)
    commands = [
        ["git", "init", "--quiet", "--template="],
        ["git", "add", "--force", "."],
        ["git", "-c", "user.name=Corpus evaluator", "-c", "user.email=corpus@example.invalid",
         "commit", "--quiet", "--no-gpg-sign", "-m", "Import the upstream release"],
    ]
    for command in commands:
        result = run_process(command, source, timeout, git_env)
        if result["returncode"] != 0:
            detail = result.get("error") or result.get("output")
            raise ValueError(f"Could not prepare the Git snapshot: {detail}")


def normalize_roots(value: str, roots: dict[Path, str]) -> str:
    """Replace scratch paths, including paths embedded in compiler arguments."""
    for root, marker in roots.items():
        value = re.sub(re.escape(str(root)) + r"""(?=$|[/\s"'=,:;)])""", lambda _: marker, value)
    return value


def absolute(path: str, directory: str | Path) -> str:
    return os.path.abspath(os.path.join(directory, path))


def compile_database(build: Path, roots: dict[Path, str]) -> list[dict] | None:
    """Normalize source and object paths and keep compiler arguments in order."""
    path = build / "compile_commands.json"
    if not path.is_file():
        return None
    rows = []
    for entry in json.loads(path.read_text(encoding="utf-8")):
        directory = absolute(entry["directory"], build)
        source = absolute(entry["file"], directory)
        argv = list(entry["arguments"]) if "arguments" in entry else shlex.split(entry["command"])
        output = entry.get("output")
        for index, arg in enumerate(argv[1:], start=1):
            if argv[index - 1] == "-o":
                argv[index] = absolute(arg, directory)
                if output is None:
                    output = arg
            elif not arg.startswith("-") and absolute(arg, directory) == source:
                argv[index] = source
        rows.append({
            "directory": normalize_roots(directory, roots),
            "file": normalize_roots(source, roots),
            "output": None if output is None else normalize_roots(absolute(output, directory), roots),
            "arguments": [normalize_roots(arg, roots) for arg in argv],
        })
    return sorted(rows, key=row_key)


def install_tree(prefix: Path) -> list[dict]:
    """List the installed paths and types without following links."""
    rows = []

    def visit(directory: Path) -> None:
        for path in directory.iterdir():
            mode = path.lstat().st_mode
            kind = {stat.S_IFDIR: "directory", stat.S_IFREG: "file", stat.S_IFLNK: "symlink"}
            file_type = kind.get(stat.S_IFMT(mode), stat.filemode(mode)[0])
            rows.append({"path": path.relative_to(prefix).as_posix(), "type": file_type})
            if file_type == "directory":
                visit(path)

    if prefix.is_dir():
        visit(prefix)
    return sorted(rows, key=lambda row: row["path"])


def install_manifest(build: Path, prefix: Path) -> list[str] | None:
    """Return sorted install manifest paths relative to the install prefix."""
    path = build / "install_manifest.txt"
    if not path.is_file():
        return None
    return sorted(
        os.path.relpath(absolute(line, prefix), prefix)
        for line in path.read_text(encoding="utf-8").splitlines()
        if line
    )


def finished(result: dict) -> bool:
    return all(
        stage["returncode"] == 0 or stage.get("skipped") == "disabled"
        for stage in result["stages"].values()
    ) and not result.get("artifact_error")


def evaluate(
    name: str, cmake: Path, ctest: Path | None, project: dict, archive: Path,
    test_archive: Path | None, work: Path, args: argparse.Namespace,
) -> dict:
    """Run one engine in a new source, build and install tree."""
    work.mkdir()
    source = extract(archive, work / "source")
    build = work / "build"
    prefix = work / "install"
    build.mkdir()
    configure_args = list(project["configure_args"])
    roots = {source: "<src>", build: "<bin>", prefix: "<install>"}
    if test_archive is not None:
        dependency = extract(test_archive, work / "test-dependency")
        variable = project["test_dependency"]["configure_variable"]
        configure_args.append(f"-D{variable}={dependency}")
        roots[dependency] = "<test-dependency>"
    if "cmakelists" in project:
        with (source / "CMakeLists.txt").open("x", encoding="utf-8") as wrapper:
            wrapper.write(project["cmakelists"])

    env = os.environ.copy()
    env.pop("DESTDIR", None)
    env.pop("CMAKE_INSTALL_MODE", None)
    env["CMAKE_BUILD_PARALLEL_LEVEL"] = JOBS
    if project.get("git_snapshot"):
        make_git_snapshot(source, args.timeout, env)
    if ctest is None:
        ctest = work / "ctest"
        ctest.symlink_to(cmake)

    commands = (
        [str(cmake), "-S", str(source), "-B", str(build), "-G", args.generator,
         "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON", f"-DCMAKE_INSTALL_PREFIX={prefix}",
         *configure_args],
        [str(cmake), "--build", str(build), "--parallel", JOBS],
        [str(cmake), "--install", str(build)],
        [str(ctest), "--output-on-failure", "--no-tests=error", "--parallel", JOBS],
    )
    stages = {}
    previous_failure = False
    for stage, command in zip(STAGES, commands, strict=True):
        if stage == "test" and not project["test_enabled"]:
            stages[stage] = {"returncode": None, "skipped": "disabled"}
        elif previous_failure:
            stages[stage] = {"returncode": None, "skipped": "previous stage failed"}
        else:
            print(f"{project['name']} [{name}] {stage}", flush=True)
            result = run_process(command, build, args.timeout, env)
            previous_failure = result["returncode"] != 0
            output = result.pop("output", "")
            if previous_failure:
                result["output_tail"] = output[-16000:]
                print(f"  Failed ({result.get('error', result['returncode'])}).", flush=True)
                if output:
                    print(output[-3000:].rstrip(), flush=True)
            elif stage == "test":
                summary = re.search(r"^\d+% tests passed\b.*", output, re.MULTILINE)
                if summary:
                    result["summary"] = summary[0]
                    print(f"  {summary[0]}", flush=True)
            stages[stage] = result

    result = {"stages": stages}
    try:
        if stages["configure"]["returncode"] == 0:
            result["compile_commands"] = compile_database(build, roots)
        if stages["install"]["returncode"] == 0:
            result["install_tree"] = install_tree(prefix)
            result["install_manifest"] = install_manifest(build, prefix)
    except (KeyError, OSError, TypeError, ValueError) as error:
        result["artifact_error"] = str(error)
        print(f"  Could not read the build artifacts: {error}", flush=True)
    return result


def row_key(row: dict | str) -> str:
    return json.dumps(row, sort_keys=True)


def compare(oracle: dict, rcmake: dict) -> dict:
    """Report stage mismatches and rows unique to each engine."""
    differences = {}
    for stage in STAGES:
        fields = ("returncode", "error", "skipped")
        left = {key: oracle["stages"][stage].get(key) for key in fields}
        right = {key: rcmake["stages"][stage].get(key) for key in fields}
        if left != right:
            differences[stage] = {"oracle": left, "rcmake": right}
    for artifact in ("install_tree", "install_manifest", "compile_commands"):
        left, right = oracle.get(artifact), rcmake.get(artifact)
        if left == right:
            continue
        if left is None or right is None:
            differences[artifact] = {"oracle_present": left is not None, "rcmake_present": right is not None}
        else:
            a = Counter(map(row_key, left))
            b = Counter(map(row_key, right))
            differences[artifact] = {
                "oracle_only": [json.loads(row) for row in sorted((a - b).elements())],
                "rcmake_only": [json.loads(row) for row in sorted((b - a).elements())],
            }
    return differences


def executable(path: str | Path) -> Path:
    candidate = Path(path).expanduser().absolute()
    if not candidate.is_file() or not os.access(candidate, os.X_OK):
        raise ValueError(f"Executable unavailable: {candidate}")
    return candidate


def select_projects(args: argparse.Namespace) -> list[dict]:
    with MANIFEST.open("rb") as manifest:
        projects = tomllib.load(manifest)["project"]
    if args.tier != "all":
        projects = [project for project in projects if project["tier"] == int(args.tier)]
    if args.project:
        name = args.project.casefold()
        projects = [
            project for project in projects
            if name in (project["name"].casefold(), project["name"].split("/")[-1].casefold())
        ]
    if not projects:
        raise ValueError("No projects match the selection.")
    return projects


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tier", choices=("1", "2", "all"), default="all")
    parser.add_argument("--project", help="Project name, for example zlib or nlohmann/json.")
    parser.add_argument("--generator", choices=("Ninja", "Unix Makefiles"), default="Ninja")
    parser.add_argument("--oracle-only", action="store_true", help="Run only the pinned CMake oracle.")
    parser.add_argument("--rcmake-bin", type=Path, default=ROOT / "target/stage/bin/rcmake")
    parser.add_argument("--json", type=Path, metavar="PATH", help="Write the complete results as JSON.")
    parser.add_argument("--timeout", type=int, default=600, help="Timeout in seconds for each command.")
    args = parser.parse_args()
    if args.timeout < 1:
        parser.error("--timeout must be positive.")

    try:
        projects = select_projects(args)
        generator_tool = {"Ninja": "ninja", "Unix Makefiles": "make"}[args.generator]
        if shutil.which(generator_tool) is None:
            raise ValueError(f"{generator_tool} is required for {args.generator}.")
        if "RCMAKE_ORACLE_CMAKE" not in os.environ:
            raise ValueError("Set RCMAKE_ORACLE_CMAKE to the pinned binary. See env/env.sh.")
        cmake = executable(os.environ["RCMAKE_ORACLE_CMAKE"])
        ctest = executable(os.environ.get("RCMAKE_ORACLE_CTEST", cmake.with_name("ctest")))
        rcmake = None if args.oracle_only else executable(args.rcmake_bin)
        with tempfile.TemporaryDirectory(prefix="rcmake-corpus-", dir="/tmp") as scratch:
            work = Path(scratch).resolve()
            for tool_name, tool in (("cmake", cmake), ("ctest", ctest)):
                version = run_process([str(tool), "--version"], work, args.timeout, os.environ.copy())
                expected = f"{tool_name} version {ORACLE_VERSION}"
                if version["returncode"] != 0 or expected not in version.get("output", "").splitlines():
                    raise ValueError(f"Expected {expected} from {tool}.")
            report = {"generator": args.generator, "oracle_version": ORACLE_VERSION, "projects": {}}
            for index, project in enumerate(projects):
                result = {"version": project["version"], "passed": False}
                report["projects"][project["name"]] = result
                project_work = work / str(index)
                project_work.mkdir()
                try:
                    archive = cached_archive(project)
                    test_pin = project.get("test_dependency")
                    test_archive = cached_archive(test_pin) if test_pin else None
                    oracle = evaluate(
                        "oracle", cmake, ctest, project, archive, test_archive,
                        project_work / "oracle", args,
                    )
                    result["oracle"] = oracle
                    result["passed"] = finished(oracle)
                    if rcmake is not None:
                        candidate = evaluate(
                            "rcmake", rcmake, None, project, archive, test_archive,
                            project_work / "rcmake", args,
                        )
                        result["rcmake"] = candidate
                        result["differences"] = compare(oracle, candidate)
                        result["passed"] &= finished(candidate) and not result["differences"]
                        for mismatch in result["differences"]:
                            print(f"  Difference in {mismatch}.", flush=True)
                except (OSError, ValueError, HTTPException, tarfile.TarError) as error:
                    result["passed"] = False
                    result["error"] = str(error)
                    print(f"  {error}", file=sys.stderr, flush=True)
                status = "PASS" if result["passed"] else "FAIL"
                print(f"{status} {project['name']} {project['version']}", flush=True)
        if args.json:
            args.json.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        return 0 if all(project["passed"] for project in report["projects"].values()) else 1
    except KeyboardInterrupt:
        print("Corpus run interrupted.", file=sys.stderr)
        return 130
    except (OSError, ValueError) as error:
        print(error, file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
