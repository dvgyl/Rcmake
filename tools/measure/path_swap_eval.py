#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Compare CMake and Rcmake through PATH.

Compare command output, child tools, test results, and file contents.
Use --smoke to run the fixture without a compiler. Use --source to compare a
project with reproducible outputs after the fixture passes. Set
RCMAKE_ORACLE_BIN_DIR to the bin directory of CMake 4.5.1.
"""

from __future__ import annotations

import argparse
from difflib import unified_diff
import gzip
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import xml.etree.ElementTree as ET
import zipfile
import zlib

try:
    from ._proc import run_process as _run_process
except ImportError:
    from _proc import run_process as _run_process
from corpus_eval import (
    PACKAGE_EXTENSIONS, install_manifest, install_tree, normalize_roots, package_tree, row_key,
)
from oracle_prop_test import ORACLE_VERSION, executable, write_json

TOOLS = ("cmake", "ctest", "cpack")
EXE_SUFFIX = ".exe" if os.name == "nt" else ""
BLOCKED_TOOLS = (*TOOLS, "rcmake", "ccmake", "cmake-gui", "cargo", "rustc")
BUILD_TOOLS = ("ninja", "make", "cc", "c++", "gcc", "g++", "clang", "clang++", "gfortran",
               "cl", "msbuild", "xcodebuild")
TOOL_CACHE_KEYS = dict(zip(TOOLS, ("CMAKE_COMMAND", "CMAKE_CTEST_COMMAND", "CMAKE_CPACK_COMMAND")))
DATA_DIR = Path("share/cmake-4.5")
DEFAULT_WORK_DIR = Path(tempfile.gettempdir()) / "rcmake-path-swap"
FIXTURE_FILE = "share/path-swap/payload.txt"
FIXTURE_TEST = "path-swap-payload"
FIXTURE_PAYLOAD = b"Rcmake PATH fixture"
OUTPUT_STAGES = ("configure", "build", "test", "package")
VERSION_FOOTER = re.compile(
    rf"^((?:cmake|ctest|cpack) version {re.escape(ORACLE_VERSION)}\n)"
    r"(?:\nCMake suite maintained and supported by Kitware \(kitware\.com/cmake\)\."
    r"|Rcmake \d+\.\d+\.\d+(?:[-+][\w.+-]+)?)\n",
    re.MULTILINE,
)
CONFIGURE_TIME = re.compile(r"^(-- (?:Configuring|Generating) done) \(\d+(?:\.\d+)?s\)$", re.MULTILINE)
TEST_TIME = re.compile(
    r"^(\d+/\d+ Test +#\d+: .*?\.{2,} +(?:Passed|\*\*\*[^\r\n]+?)"
    r"|Total Test time \(real\) =) +\d+(?:\.\d+)? sec$",
    re.MULTILINE,
)


def tool_stem(path: Path) -> str:
    """Return a tool name without its Windows launcher extension."""
    name = path.name.lower()
    for extension in (".exe", ".com", ".cmd", ".bat"):
        name = name.removesuffix(extension)
    return name


def support_path(root: Path, inherited_path: str, excluded: tuple[str, ...] = TOOLS) -> list[Path]:
    """Keep support directories on PATH without alternate CMake tools."""
    paths = []
    directories = dict.fromkeys(Path(entry).resolve() for entry in inherited_path.split(os.pathsep)
                                if Path(entry).is_absolute() and Path(entry).is_dir())
    for index, directory in enumerate(directories):
        entries = list(directory.iterdir())
        if not any(tool_stem(tool) in excluded for tool in entries):
            paths.append(directory)
            continue
        filtered = root / f"support-{index}"
        filtered.mkdir()
        for tool in entries:
            if tool_stem(tool) in excluded or not tool.is_file():
                continue
            if os.name == "nt":
                shutil.copy2(tool, filtered / tool.name)
            elif os.access(tool, os.X_OK):
                (filtered / tool.name).symlink_to(tool)
        paths.append(filtered)
    return paths


def controlled_path(root: Path, extra_dirs: list[Path]) -> list[Path]:
    """Select system and build tool directories for both lanes."""
    if os.name == "nt":
        windows = Path(os.environ["SYSTEMROOT"])
        system_dirs = [windows / "System32", windows]
    else:
        system_dirs = [Path("/usr/bin"), Path("/bin")]
    inherited = os.pathsep.join(entry for entry in os.get_exec_path() if Path(entry).is_absolute())
    located = [Path(path).parent for name in BUILD_TOOLS
               if (path := shutil.which(name + EXE_SUFFIX, path=inherited))
               and Path(path).is_absolute()]
    extras = [directory.resolve(strict=True) for directory in extra_dirs]
    if any(not directory.is_dir() for directory in extras):
        raise ValueError("Use directories for --tool-dir.")
    return support_path(root, os.pathsep.join(map(str, [*extras, *located, *system_dirs])),
                        BLOCKED_TOOLS)


def lane_environment(root: Path, paths: list[Path]) -> dict[str, str]:
    """Use private state and keep system and compiler discovery settings."""
    home, cache, scratch = (root / name for name in ("home", "cache", "tmp"))
    for directory in (home, cache, scratch):
        directory.mkdir(parents=True)
    keys = {"SYSTEMROOT", "WINDIR", "SYSTEMDRIVE", "PROGRAMFILES", "PROGRAMFILES(X86)",
            "PROGRAMW6432", "PROGRAMDATA", "COMMONPROGRAMFILES", "COMMONPROGRAMFILES(X86)",
            "PROCESSOR_ARCHITECTURE", "PROCESSOR_ARCHITEW6432", "COMSPEC", "OS",
            "INCLUDE", "LIB", "LIBPATH", "VCINSTALLDIR", "VCTOOLSINSTALLDIR",
            "VCTOOLSVERSION", "VSINSTALLDIR", "VISUALSTUDIOVERSION", "WINDOWSSDKDIR",
            "WINDOWSSDKVERSION", "UNIVERSALCRTSDKDIR", "UCRTVERSION", "DEVENVDIR",
            "SDKROOT", "DEVELOPER_DIR"}
    env = {key.upper(): value for key, value in os.environ.items() if key.upper() in keys}
    return {**env, "PATH": os.pathsep.join(map(str, paths)), "LC_ALL": "C", "NO_COLOR": "1",
            "HOME": str(home), "USERPROFILE": str(home), "XDG_CONFIG_HOME": str(home / ".config"),
            "APPDATA": str(home / "AppData/Roaming"), "LOCALAPPDATA": str(home / "AppData/Local"),
            "XDG_CACHE_HOME": str(cache), "RCMAKE_CACHE_DIR": str(cache / "rcmake"),
            "TMPDIR": str(scratch), "TEMP": str(scratch), "TMP": str(scratch),
            "PATHEXT": ".EXE;.COM;.BAT;.CMD", "NoDefaultCurrentDirectoryInExePath": "1"}


def run_command(command: list[str], cwd: Path, timeout: int, env: dict[str, str]) -> dict:
    """Capture output and stop the process tree when a command times out."""
    completed = _run_process(command, cwd, timeout, env, use_tempfiles=True)
    if completed.start_error is not None:
        error = completed.start_error
    elif completed.timed_out:
        error = f"Command timed out after {timeout} seconds."
    else:
        error = None
    return {
        "returncode": completed.returncode,
        "error": error,
        "stdout": completed.stdout,
        "stderr": completed.stderr,
    }


def succeeded(result: dict) -> bool:
    """Check whether a command completed and returned zero."""
    return result["returncode"] == 0 and result["error"] is None


def version_line(name: str, output: str, candidate: bool | None) -> str:
    """Check the version and, if requested, the Rcmake identity."""
    lines = output.splitlines()
    banner = f"{name} version {ORACLE_VERSION}"
    if lines[:1] != [banner]:
        raise ValueError(f"Expected {banner}. Output was {output!r}.")
    rcmake = any(line.startswith("Rcmake ") and line[7:].strip() for line in lines[1:])
    if candidate is not None and rcmake != candidate:
        expected = "Rcmake" if candidate else "upstream CMake"
        raise ValueError(f"Expected {expected} for {name}. Output was {output!r}.")
    return banner


def verify_path(bin_dir: Path, root: Path, timeout: int, env: dict[str, str],
                *, candidate: bool | None = None) -> dict:
    """Check the only PATH match for each tool and run its version command."""
    evidence = {}
    for name in TOOLS:
        filename = name + EXE_SUFFIX
        expected = bin_dir / filename
        matches = [Path(entry) / filename for entry in env["PATH"].split(os.pathsep)
                   if entry and (Path(entry) / filename).is_file()
                   and os.access(Path(entry) / filename, os.X_OK)]
        if matches != [expected]:
            raise ValueError(f"Expected only {expected} on PATH. Found {matches!r}.")
        # Use the PATH match explicitly. Windows CreateProcess searches the
        # parent's PATH when it starts a command with an unqualified name.
        version = run_command([str(matches[0]), "--version"], root, timeout, env)
        if not succeeded(version):
            raise ValueError(f"Could not run {expected}. Result was {version!r}.")
        evidence[name] = {"which": str(matches[0]),
                          "version": version_line(name, version["stdout"], candidate),
                          "banner": version["stdout"].strip()}
    return evidence


def stage_tools(root: Path, binaries: dict[str, Path], data: Path) -> Path:
    """Copy the selected executables and their data into one installation."""
    if not (data / "Modules/CMakeDetermineSystem.cmake").is_file():
        raise ValueError(f"No CMake modules in {data}.")
    bin_dir = root / "bin"
    bin_dir.mkdir(parents=True)
    for name, binary in binaries.items():
        shutil.copy2(binary, bin_dir / (name + EXE_SUFFIX))
    shutil.copytree(data, root / DATA_DIR)
    return bin_dir


def safe_source(source: Path) -> None:
    """Check that source links are relative and stay in the source tree."""
    for directory, subdirs, files in os.walk(source):
        for name in (*subdirs, *files):
            path = Path(directory) / name
            if path.is_symlink():
                target = path.readlink()
                if target.anchor or not path.resolve().is_relative_to(source):
                    raise ValueError(f"Source link points outside the project copy at {path}.")
            elif getattr(path.lstat(), "st_file_attributes", 0) & stat.FILE_ATTRIBUTE_REPARSE_POINT:
                raise ValueError(f"Source has an unsupported reparse point at {path}.")


def check_tool_path(value: str, tool: Path) -> str:
    """Check that a path identifies the tool or data in this lane."""
    path = Path(value)
    if os.name == "nt" and tool.suffix == EXE_SUFFIX and not path.suffix:
        path = path.with_suffix(EXE_SUFFIX)
    if not path.is_absolute() or path.resolve() != tool.resolve():
        raise ValueError(f"Expected {tool}. Found {value!r}.")
    return value


def cache_tools(build: Path, bin_dir: Path) -> dict:
    """Read and check the tool and data paths in the configure cache."""
    values = {}
    for line in (build / "CMakeCache.txt").read_text(encoding="utf-8").splitlines():
        key, separator, value = line.partition(":INTERNAL=")
        if separator:
            values[key] = value
    paths = {key: bin_dir / (name + EXE_SUFFIX) for name, key in TOOL_CACHE_KEYS.items()}
    paths["CMAKE_ROOT"] = bin_dir.parent / DATA_DIR
    return {key: check_tool_path(values.get(key, ""), expected)
            for key, expected in paths.items()}


def test_results(report: Path) -> list[dict]:
    """Read the reported CTest results and require a passing test."""
    results = []
    for test in ET.parse(report).iter("testcase"):
        status = next((tag for tag in ("error", "failure", "skipped")
                       if test.find(tag) is not None), None)
        if status is None:
            status = "pass" if test.get("status", "run") == "run" else "skipped"
        results.append({"name": test.attrib["name"], "status": status})
    if not any(row["status"] == "pass" for row in results):
        raise ValueError("CTest did not report a passing test.")
    return sorted(results, key=row_key)


def installed_files(prefix: Path) -> list[dict]:
    """Record file digests and symbolic link targets in the install tree."""
    if prefix.is_symlink():
        raise ValueError("The install prefix is a symbolic link.")
    rows = install_tree(prefix)
    for row in rows:
        path = prefix / row["path"]
        if row["type"] == "file":
            with path.open("rb") as stream:
                row["sha256"] = hashlib.file_digest(stream, "sha256").hexdigest()
        elif row["type"] == "symlink":
            row["target"] = normalize_roots(str(path.readlink()), {prefix.parent: "<work>"})
        elif row["type"] != "directory":
            raise ValueError(f"Unsupported installed file type at {path}.")
    if not any(row["type"] in ("file", "symlink") for row in rows):
        raise ValueError("The install tree has no files or symbolic links.")
    return rows


def archive_digests(archive: Path, generator: str) -> dict[str, str]:
    """Read member contents and check the archive compression checksum."""
    digests = {}
    seen = set()

    def record(name, stream):
        path = Path(name).as_posix()
        if path in seen:
            raise ValueError(f"Duplicate member {name!r} in {archive.name}.")
        seen.add(path)
        if stream is not None:
            with stream:
                digests[path] = hashlib.file_digest(stream, "sha256").hexdigest()

    if generator == "TGZ":
        with gzip.open(archive) as stream:
            with tarfile.open(fileobj=stream, mode="r:") as source:
                for member in source:
                    record(member.name, source.extractfile(member) if member.isfile() else None)
                stream.seek(source.offset)
            if stream.read(1024) != bytes(1024):
                raise ValueError(f"Missing tar end marker in {archive.name}.")
            while padding := stream.read(1024 * 1024):
                if any(padding):
                    raise ValueError(f"Unexpected data after the tar end marker in {archive.name}.")
    else:
        with zipfile.ZipFile(archive) as source:
            for member in source.infolist():
                record(member.filename, source.open(member))
    return digests


def archive_files(archive: Path, generator: str, root: Path) -> list[dict]:
    """Check archive integrity and record file digests with member metadata."""
    rows = package_tree(archive, generator, {root: "<work>"})
    digests = archive_digests(archive, generator)
    for row in rows:
        if row["type"] == "file":
            row["sha256"] = digests[row["path"]]
    if not any(row["type"] in ("file", "symlink", "hardlink") for row in rows):
        raise ValueError(f"The package has no files or links in {archive.name}.")
    return rows


def collect_artifacts(stage: str, root: Path, args: argparse.Namespace) -> dict:
    """Read the current stage's artifacts before later stages can change them."""
    if stage == "test":
        return {"tests": test_results(root / "ctest.xml")}
    if stage == "install":
        prefix = root / "install"
        tree = installed_files(prefix)
        manifest = [Path(path).as_posix() for path in install_manifest(root / "build", prefix) or ()]
        if not manifest or not set(manifest) <= {row["path"] for row in tree}:
            raise ValueError("The install manifest is empty or names files outside the install tree.")
        return {"install_tree": tree, "install_manifest": manifest}
    if stage == "package":
        extension = PACKAGE_EXTENSIONS[args.cpack_generator]
        archives = sorted((root / "packages").glob(f"*{extension}"))
        if not archives:
            raise ValueError(f"CPack did not produce a {args.cpack_generator} archive.")
        rows = [row for archive in archives for row in archive_files(archive, args.cpack_generator, root)]
        return {"package_tree": sorted(rows, key=row_key)}
    return {}


def write_fixture(source: Path, compile_app: bool) -> None:
    """Write a project with known child invocations and payloads."""
    source.mkdir()
    (source / "child.cmake").write_text("""\
cmake_minimum_required(VERSION 4.5)
set(proof "$ENV{RCMAKE_PATH_SWAP_WITNESS_DIR}")
file(WRITE "${proof}/cmake.txt" "${CMAKE_COMMAND}\\n${CMAKE_ROOT}\\n")
foreach(tool IN ITEMS ctest cpack)
  find_program(tool_path NAMES "${tool}" PATHS ENV PATH NO_DEFAULT_PATH NO_CACHE REQUIRED)
  file(WRITE "${proof}/${tool}-path.txt" "${tool_path}")
  execute_process(COMMAND "${tool_path}" --version OUTPUT_FILE "${proof}/${tool}.txt"
                  COMMAND_ERROR_IS_FATAL ANY)
  unset(tool_path)
endforeach()
if(DEFINED PAYLOAD)
  file(WRITE "${PAYLOAD}" "Rcmake PATH fixture")
endif()
if(DEFINED PAYLOAD_TO_TEST)
  file(READ "${PAYLOAD_TO_TEST}" contents)
  if(NOT contents STREQUAL "Rcmake PATH fixture")
    message(FATAL_ERROR "The build did not write the expected payload.")
  endif()
endif()
if(DEFINED APP)
  execute_process(COMMAND "${APP}" OUTPUT_VARIABLE contents COMMAND_ERROR_IS_FATAL ANY)
  string(STRIP "${contents}" contents)
  if(NOT contents STREQUAL "Rcmake PATH fixture")
    message(FATAL_ERROR "The program did not print the expected payload.")
  endif()
endif()
""", encoding="utf-8")
    if compile_app:
        (source / "main.c").write_text(
            '#include <stdio.h>\nint main(void) { puts("Rcmake PATH fixture"); return 0; }\n',
            encoding="utf-8",
        )
    language = "C" if compile_app else "NONE"
    target = ("add_executable(path_swap_app main.c)\n"
              "add_dependencies(path_swap_app path_swap_payload)") if compile_app else ""
    app_arg = '"-DAPP=$<TARGET_FILE:path_swap_app>"' if compile_app else ""
    (source / "CMakeLists.txt").write_text(f"""\
cmake_minimum_required(VERSION 4.5)
project(PathSwap VERSION 1.0 LANGUAGES {language})
set(child "${{CMAKE_CURRENT_SOURCE_DIR}}/child.cmake")
set(payload "${{CMAKE_CURRENT_BINARY_DIR}}/payload.txt")
add_custom_target(path_swap_payload ALL
  COMMAND cmake "-DPAYLOAD=${{payload}}" -P "${{child}}"
  COMMENT "Write the PATH fixture payload." VERBATIM)
{target}
enable_testing()
add_test(NAME {FIXTURE_TEST}
  COMMAND cmake "-DPAYLOAD_TO_TEST=${{payload}}" {app_arg} -P "${{child}}")
install(CODE "execute_process(COMMAND cmake -P \\"${{child}}\\" COMMAND_ERROR_IS_FATAL ANY)")
install(FILES "${{payload}}" DESTINATION "share/path-swap")
set(CPACK_PACKAGE_FILE_NAME "path-swap")
set(CPACK_INCLUDE_TOPLEVEL_DIRECTORY ON)
# Use a package staging script with no install progress messages.
set(package_stage "${{CMAKE_CURRENT_BINARY_DIR}}/package-stage")
file(GENERATE OUTPUT "${{package_stage}}/cmake_install.cmake" CONTENT "
  execute_process(COMMAND cmake -P \\"${{child}}\\" COMMAND_ERROR_IS_FATAL ANY)
  file(MAKE_DIRECTORY \\"\\${{CMAKE_INSTALL_PREFIX}}/share/path-swap\\")
  configure_file(\\"${{payload}}\\" \\"\\${{CMAKE_INSTALL_PREFIX}}/{FIXTURE_FILE}\\" COPYONLY)
")
set(CPACK_INSTALL_CMAKE_PROJECTS "${{package_stage}};PathSwap;ALL;/")
include(CPack)
""", encoding="utf-8")


def read_witness(directory: Path, bin_dir: Path, candidate: bool) -> dict:
    """Check evidence written by the CMake child and its companion tools."""
    command, data = (directory / "cmake.txt").read_text(encoding="utf-8").splitlines()
    evidence = {
        "cmake": check_tool_path(command, bin_dir / ("cmake" + EXE_SUFFIX)),
        "data_root": check_tool_path(data, bin_dir.parent / DATA_DIR),
    }
    for name in ("ctest", "cpack"):
        banner = (directory / f"{name}.txt").read_text(encoding="utf-8")
        command = (directory / f"{name}-path.txt").read_text(encoding="utf-8")
        version_line(name, banner, candidate)
        evidence[name] = {"path": check_tool_path(command, bin_dir / (name + EXE_SUFFIX)),
                          "banner": banner.strip()}
    return evidence


def check_fixture(artifacts: dict, generator: str) -> None:
    """Require the expected test and exact installed and packaged payloads."""
    if artifacts["tests"] != [{"name": FIXTURE_TEST, "status": "pass"}]:
        raise ValueError("The fixture did not report its expected passing test.")
    if artifacts["install_manifest"] != [FIXTURE_FILE]:
        raise ValueError("The fixture did not install its expected file.")
    digest = hashlib.sha256(FIXTURE_PAYLOAD).hexdigest()
    for tree, path in (("install_tree", FIXTURE_FILE),
                       ("package_tree", f"path-swap/{FIXTURE_FILE}")):
        files = [(row["path"], row.get("sha256")) for row in artifacts[tree]
                 if row["type"] != "directory"]
        if files != [(path, digest)]:
            raise ValueError(f"The fixture has the wrong file or contents in {tree}.")
    expected_archive = "path-swap" + PACKAGE_EXTENSIONS[generator]
    if {row["archive"] for row in artifacts["package_tree"]} != {expected_archive}:
        raise ValueError("The fixture did not produce its expected archive.")


def evaluate(name: str, bin_dir: Path, root: Path, source: Path, env: dict[str, str],
             args: argparse.Namespace, *, fixture: bool) -> dict:
    """Run a project and collect its tool and output evidence."""
    candidate = name == "rcmake"
    result = {"stages": {}, "children": {}, "artifacts": {},
              "paths": {"home": env["HOME"], "cache": env["XDG_CACHE_HOME"],
                        "search": env["PATH"].split(os.pathsep),
                        "scratch": env["TMPDIR"], "installation": str(bin_dir.parent),
                        "project": str(root)}}
    try:
        result["tools"] = verify_path(bin_dir, root, args.timeout, env, candidate=candidate)
    except ValueError as error:
        return dict(result, identity_error=str(error))
    shutil.copytree(source, root / "source", symlinks=True)
    build_type = f"-DCMAKE_BUILD_TYPE={args.config}"
    commands = {
        "configure": ["cmake", "-S", "source", "-B", "build", "-G", args.generator, build_type,
                      *(() if fixture and args.source else args.configure_arg)],
        "build": ["cmake", "--build", "build", "--config", args.config, "--parallel", "1"],
        "test": ["ctest", "--test-dir", "build", "-C", args.config, "--output-on-failure",
                 "--no-tests=error", "--parallel", "1", "--output-junit", str(root / "ctest.xml")],
        "install": ["cmake", "--install", "build", "--config", args.config,
                    "--prefix", str(root / "install")],
        "package": ["cpack", "--config", "build/CPackConfig.cmake", "-C", args.config,
                    "-G", args.cpack_generator, "-B", str(root / "packages")],
    }
    failed = False
    for stage, command in commands.items():
        resolved = [result["tools"][command[0]]["which"], *command[1:]]
        if failed:
            outcome = {"returncode": None, "error": None, "skipped": True}
        else:
            print(f"[{name}] {stage}", flush=True)
            witness = root / "witness" / stage
            stage_env = env
            if fixture:
                witness.mkdir(parents=True)
                stage_env = dict(env, RCMAKE_PATH_SWAP_WITNESS_DIR=str(witness))
            outcome = run_command(resolved, root, args.timeout, stage_env)
            if succeeded(outcome):
                try:
                    if stage == "configure":
                        result["cache_tools"] = cache_tools(root / "build", bin_dir)
                    elif fixture:
                        result["children"][stage] = read_witness(witness, bin_dir, candidate)
                except (OSError, ValueError) as error:
                    outcome["error"] = str(error)
                try:
                    result["artifacts"].update(collect_artifacts(stage, root, args))
                except (KeyError, OSError, ValueError, ET.ParseError, tarfile.TarError,
                        zipfile.BadZipFile, EOFError, zlib.error) as error:
                    result["artifact_error"] = str(error)
            failed = not succeeded(outcome) or "artifact_error" in result
        result["stages"][stage] = dict(outcome, command=command, executable=resolved[0])
    if fixture and not failed:
        try:
            check_fixture(result["artifacts"], args.cpack_generator)
        except ValueError as error:
            result["artifact_error"] = str(error)
    return result


def normalize_output(output: str, paths: dict, stage: str) -> str:
    """Replace only lane paths, tool version footers, and known timing fields."""
    roots = {}
    for key in ("home", "cache", "scratch", "installation", "project"):
        if key in paths:
            root = Path(paths[key])
            for spelling in (str(root), root.as_posix()):
                roots[spelling] = f"<{key}>"
    for root, label in sorted(roots.items(), key=lambda entry: len(entry[0]), reverse=True):
        output = re.sub(re.escape(root) + r"""(?=$|[/\s\\'"=:;,()\]])""", label, output)
    output = VERSION_FOOTER.sub(r"\1", output.replace("\r\n", "\n"))
    if stage == "configure":
        output = CONFIGURE_TIME.sub(r"\1 (<time>)", output)
    if stage == "test":
        output = TEST_TIME.sub(r"\1 <time> sec", output)
    return output


def compare(oracle: dict, rcmake: dict) -> tuple[list[str], dict[str, str]]:
    """List the fields that differ and show differences in command output."""
    differences = []
    for stage in oracle["stages"].keys() | rcmake["stages"].keys():
        expected = oracle["stages"].get(stage, {})
        actual = rcmake["stages"].get(stage, {})
        if any(expected.get(key) != actual.get(key) for key in ("returncode", "error", "skipped")):
            differences.append(stage)
    left, right = (engine.get("artifacts", {}) for engine in (oracle, rcmake))
    differences.extend(key for key in left.keys() | right.keys() if left.get(key) != right.get(key))
    differences.extend(key for key in ("identity_error", "artifact_error")
                       if oracle.get(key) != rcmake.get(key))
    output_diffs = {}
    for stage in OUTPUT_STAGES:
        for stream in ("stdout", "stderr"):
            left, right = (normalize_output(engine["stages"].get(stage, {}).get(stream, ""),
                                            engine.get("paths", {}), stage)
                           for engine in (oracle, rcmake))
            if left != right:
                key = f"{stage}.{stream}"
                output_diffs[key] = "\n".join(unified_diff(
                    left.split("\n"), right.split("\n"), f"cmake/{key}", f"rcmake/{key}", lineterm="",
                ))
    return sorted([*differences, *output_diffs]), output_diffs


def evaluate_project(source: Path, root: Path, installations: dict[str, Path],
                     support: list[Path], args: argparse.Namespace, *, fixture: bool) -> dict:
    """Compare the engines at the same project path with separate state."""
    results = {}
    for name, bin_dir in installations.items():
        project = root / "project"
        project.mkdir()
        env = lane_environment(root / "state" / ("fixture" if fixture else "source") / name,
                               [bin_dir, *support])
        results[name] = evaluate(name, bin_dir, project, source, env, args, fixture=fixture)
        shutil.rmtree(project)
    differences, output_diffs = compare(results["oracle"], results["rcmake"])

    def passed(result):
        return (not result.get("identity_error") and not result.get("artifact_error")
                and bool(result["stages"]) and all(map(succeeded, result["stages"].values())))

    oracle_ok = passed(results["oracle"])
    rcmake_ok = passed(results["rcmake"])
    status = "oracle_error" if not oracle_ok else "fail" if differences or not rcmake_ok else "pass"
    return {"status": status, "differences": differences, "output_diffs": output_diffs, **results}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", "--project", type=Path,
                        help="Also compare a project with tests, install rules and CPack.")
    parser.add_argument("--smoke", action="store_true", help="Run the fixture without a compiler.")
    parser.add_argument("--rcmake-bin", required=True, type=Path, help="Path to staged Rcmake.")
    parser.add_argument("--rcmake-data-root", type=Path, help="Path to Rcmake's cmake-4.5 data.")
    parser.add_argument("--out", required=True, type=Path, help="Path to the JSON report.")
    parser.add_argument("--work-dir", type=Path, default=DEFAULT_WORK_DIR, help="Test workspace.")
    parser.add_argument("--tool-dir", type=Path, action="append", default=[],
                        help="Add a support tool directory to both lanes. Repeat as needed.")
    parser.add_argument("--generator", default="Ninja", help="CMake build generator.")
    parser.add_argument("--config", default="Release", help="Build configuration.")
    parser.add_argument("--configure-arg", action="append", default=[],
                        help="Extra CMake configure argument. Repeat to pass several.")
    parser.add_argument("--cpack-generator", choices=PACKAGE_EXTENSIONS, default="TGZ")
    parser.add_argument("--timeout", type=int, default=600, help="Timeout in seconds per command.")
    args = parser.parse_args()
    if args.timeout < 1:
        parser.error("--timeout must be positive.")

    try:
        binary = executable(args.rcmake_bin)
        oracle_bin = Path(os.environ["RCMAKE_ORACLE_BIN_DIR"]).resolve()
        oracle_tools = {name: executable(oracle_bin / (name + EXE_SUFFIX)) for name in TOOLS}
        work_dir = args.work_dir.resolve()
        source = args.source.resolve() if args.source else None
        if source is not None:
            if not (source / "CMakeLists.txt").is_file():
                raise ValueError(f"No CMakeLists.txt in {source}.")
            if work_dir.is_relative_to(source):
                raise ValueError("The test workspace must be outside the project directory.")
            safe_source(source)
        work_dir.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="path-swap-", dir=work_dir) as directory:
            root = Path(directory)
            candidate_data = args.rcmake_data_root or binary.parent.parent / DATA_DIR
            installations = {
                "oracle": stage_tools(root / "oracle", oracle_tools, oracle_bin.parent / DATA_DIR),
                "rcmake": stage_tools(root / "rcmake", dict.fromkeys(TOOLS, binary), candidate_data),
            }
            support = controlled_path(root, args.tool_dir)
            fixture = root / "fixture"
            write_fixture(fixture, compile_app=not args.smoke and source is None)
            report = evaluate_project(fixture, root, installations, support, args, fixture=True)
            report["source"] = "<fixture>"
            if source is not None and report["status"] == "pass":
                comparison = evaluate_project(source, root, installations, support, args, fixture=False)
                report = {"source": str(source), "fixture": report, **comparison}
        write_json(args.out, report)
    except (KeyError, OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"Could not complete the PATH comparison. {error}", file=sys.stderr)
        return 2
    print(json.dumps({key: report[key] for key in ("status", "differences")}))
    for diff in report["output_diffs"].values():
        print(diff, file=sys.stderr)
    return {"pass": 0, "fail": 1, "oracle_error": 2}[report["status"]]


if __name__ == "__main__":
    raise SystemExit(main())
