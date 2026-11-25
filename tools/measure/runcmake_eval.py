#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Compare the staged Rcmake binary with CMake 4.5.1 using upstream RunCMake.

Source env/env.sh before use. The upstream driver runs all assertions on each
binary in a separate temporary copy of the source tree. Use --json to keep
the per-case results and upstream failure messages.

Use --profile with --list-suites to see the available suites. The expanded
profiles use the test commands that upstream CMake registers with CTest. The
original groups and the core portion of the wide profile keep their legacy
settings on POSIX hosts.

The score includes cases that pass with CMake 4.5.1. A missing Rcmake result
counts as a failure. Use --min-pass-rate to allow case differences within a
required pass rate. Exit status 1 means the comparison failed the case gate.
Exit status 2 means the script could not complete the comparison.
"""

from __future__ import annotations

import argparse
from collections import Counter, defaultdict
from dataclasses import dataclass
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import subprocess
import sys
import tempfile

SUITES = (
    "Syntax", "if", "foreach", "while", "function", "block", "return", "set",
    "option", "list", "string", "math", "file", "cmake_path",
    "separate_arguments", "configure_file", "cmake_parse_arguments",
    "cmake_language", "CommandLine", "CommandLineTar", "CMakePresets",
    "Diagnostics", "DiagnosticScope", "PolicyScope", "message", "include",
    "include_guard", "get_filename_component", "execute_process", "find_file",
    "find_path", "find_program", "find_library", "cmake_minimum_required",
    "Policy", "get_property", "set_property", "define_property",
    "cmake_host_system_information", "variable_watch", "CMP0219",
)
M3_SUITES = (
    "CPackCommandLine", "discover_tests", "CMakePresetsTest", "CMakePresetsPackage",
    "CMakePresetsWorkflow", "CTestCommandLine", "CPackConfig", "FileSet-SOURCES",
    "FileSetProperties", "GoogleTest", "find_package-CPS", "InstallPackageInfo",
    "ExportPackageInfo", "InstallSbom", "ExportSbom", "CPack", "ctest_test",
    "ctest_start", "ctest_fixtures", "Instrumentation", "CXXModules",
)
SUITE_GROUPS = {"m1m2": SUITES, "m3": M3_SUITES, "all": SUITES + M3_SUITES}
WIDE_SUITES = (
    "GeneratorExpression", "GeneratorExpressionShortCircuit", "GenEx-TARGET_PROPERTY",
    "GenEx-TARGET_FILE", "target_link_libraries", "target_compile_features", "target_sources",
    "add_custom_command", "Byproducts", "CompatibleInterface", "LinkLibrariesStrategy",
    "ObjectLibrary", "InterfaceLibrary", "ExternalProject", "FetchContent", "PrecompileHeaders",
    "CompilerLauncher", "CheckIPOSupported", "RuntimePath", "CompileWarningAsError",
    "PositionIndependentCode", "UnityBuild", "cmake_pkg_config", "DependencyProviders",
    "try_compile", "try_run", "ToolchainFile", "Configure", "FileAPI", "Ninja", "NinjaMultiConfig",
    "Make", "install", "export", "find_package", "SarifOutput", "MaxRecursionDepth", "CMP0054",
    "CMP0057", "CMP0118", "CMP0125", "CMP0126", "CMP0156", "target_link_libraries-ALIAS",
)
PROFILES = ("core", "wide", "policies", "all-macos", "linux", "windows")
DEFAULT_TIMEOUT = 180
CTEST_DEFAULT_TIMEOUT = 1500
EXE_SUFFIX = ".exe" if os.name == "nt" else ""
MACOS_ONLY = frozenset("""
    AppleSDK AppleSilicon AppleTextStubs CMP0042 CMP0068 CMakePackage Framework
    INSTALL_NAME_DIR MacOSVersions XcFramework XcodeProject XcodeProject-Device XcodeProject-Embed
""".split())
LINUX_ONLY = frozenset("CPack_AppImage CPack_DEB CPack_RPM LinkWhatYouUse".split())
VS_SUITES = frozenset("""
    CMAKE_MSVCIDE_RUN_PATH CSharpCustomCommand CSharpReferenceImport VS10Project
    VS10ProjectUseDebugLibraries VSSolution VsDotnetSdk VsNugetPackageRestore include_external_msproject
""".split())
WINDOWS_ONLY = VS_SUITES | frozenset("""
    CMP0194 CMP0212 CPack_NuGet CPack_WIX MSVCDebugInformationFormat MSVCDefaultFlags
    MSVCRuntimeChecks MSVCRuntimeLibrary MSVCRuntimeTypeInfo Win32GenEx
""".split())
POSIX_ONLY = frozenset("Byproducts CMP0152 CPackSymlinks PositionIndependentCode RuntimePath SymlinkTrees".split())
EXTERNAL_SUITES = frozenset("""
    Android Autogen_2 Autogen_3 Autogen_4 Autogen_5 Autogen_6 Autogen_7 CMP0104 CMP0129
    CPack_APK CSharpCustomCommand CSharpReferenceImport CTestCoverage CTestResourceAllocation
    CUDA_architectures Diab Emscripten FASTBuild FindGTK2 FindMatlab FindOpenSSL FindRuby FindSWIG
    IAR IncompatibleQt MetaCompileFeatures ObsoleteQtMacros Renesas Rust Swift SYCL TIClang
    Tutorial UseSWIG ctest_coverage ctest_memcheck
""".split())
ALIASES = {
    "macro": ("Syntax", "function", "CMP0219"),
    "unset": ("set", "CommandLine"),
    "cmake_policy": ("Policy", "PolicyScope"),
    "cmake_diagnostic": ("Diagnostics", "DiagnosticScope"),
}
HELPERS = {
    "EXIT_CODE_EXE": "exit_code",
    "EXIT_CRASH_EXE": "exit_crash",
    "PRINT_STDIN_EXE": "print_stdin",
    "LLVM_RC": "pseudo_llvm-rc",
}
TEST_ENVIRONMENT = (
    "PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "LC_CTYPE", "LC_MESSAGES",
    "SDKROOT", "DEVELOPER_DIR", "RCMAKE_VENDORED_DATA_ROOT", "APPIMAGE_EXTRACT_AND_RUN",
)
WINDOWS_ENVIRONMENT = (
    "SystemRoot", "SystemDrive", "ComSpec", "PATHEXT", "USERPROFILE", "APPDATA", "LOCALAPPDATA",
    "ProgramData", "ProgramFiles", "ProgramFiles(x86)", "INCLUDE", "LIB", "LIBPATH",
    "VCINSTALLDIR", "VCToolsInstallDir", "VSINSTALLDIR", "VisualStudioVersion",
    "WindowsSdkDir", "WindowsSDKVersion", "DOTNET_ROOT", "WIX",
)
COMPLETE = "-- runcmake_eval: suite complete"
CASE_RESULT = re.compile(
    r"^-- (?P<passed>.+) - PASSED$"
    r"|^CMake Error at .*[/\\]RunCMake\.cmake:\d+ \(message\):\n"
    r"  (?P<failed>.+) - FAILED:\n(?P<detail>(?:^[ \t].*\n|^\n)*)",
    re.MULTILINE,
)

RUN_CMAKE_SKIP = re.compile(
    r"^-- (?P<name>.+?)(?: - | )(?:All checks )?SKIPPED\b(?P<reason>.*)$",
    re.IGNORECASE,
)
CTEST_SKIP = re.compile(
    r"^\s*\d+/\d+\s+Test\s+#\d+:\s+(?P<name>.+?)\s+\.{2,}\s*"
    r"\*{0,3}(?P<reason>Skipped|Not Run)\b.*$", re.IGNORECASE,
)


@dataclass(frozen=True)
class Registration:
    """One CTest command. Several commands can use the same suite driver."""

    name: str
    generator: str
    script: Path
    directory: Path
    arguments: tuple[str, ...]
    working_directory: Path
    timeout: float


def profile_suites(source, profile):
    """Return suite directories in the requested profile."""
    if profile == "core":
        return SUITE_GROUPS["all"]
    if profile == "wide":
        return SUITE_GROUPS["all"] + WIDE_SUITES
    host = {"all-macos": "Darwin", "linux": "Linux", "windows": "Windows"}.get(
        profile, platform.system(),
    )
    exclusions = EXTERNAL_SUITES | {
        "Darwin": LINUX_ONLY | WINDOWS_ONLY,
        "Linux": MACOS_ONLY | WINDOWS_ONLY,
        "Windows": MACOS_ONLY | LINUX_ONLY | POSIX_ONLY | {"CheckLinkerFlag", "CMP0125"},
    }.get(host, MACOS_ONLY | LINUX_ONLY | WINDOWS_ONLY)
    return tuple(
        entry.name for entry in sorted(source.iterdir())
        if entry.name not in exclusions and (entry / "RunCMakeTest.cmake").is_file()
        and (profile != "policies" or re.fullmatch(r"CMP\d+", entry.name))
    )


def run(command, cwd, env, timeout):
    """Capture combined output and terminate the process group on timeout."""
    with tempfile.TemporaryFile(dir=cwd) as output:
        with subprocess.Popen(
            command, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
            stdout=output, stderr=subprocess.STDOUT, start_new_session=os.name == "posix",
        ) as process:
            try:
                process.wait(timeout=timeout)
                timed_out = False
            except (subprocess.TimeoutExpired, KeyboardInterrupt) as exc:
                try:
                    if os.name == "nt":
                        try:
                            subprocess.run(
                                ["taskkill", "/F", "/T", "/PID", str(process.pid)],
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                timeout=5, check=False,
                            )
                        except (OSError, subprocess.TimeoutExpired):
                            pass
                        process.kill()
                    else:
                        os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait()
                if isinstance(exc, KeyboardInterrupt):
                    raise
                timed_out = True
        output.seek(0)
        log = output.read().decode("utf-8", errors="replace").replace("\r\n", "\n")
    return process.returncode, timed_out, log


def prepare_helpers(source, directory, env, timeout):
    """Build the small C fixtures used by CommandLine and execute_process."""
    directory.mkdir()
    compiler = shutil.which("cc")
    if compiler is None:
        raise ValueError("Install a C compiler named cc for the upstream test fixtures.")
    for name in HELPERS.values():
        code, timed_out, log = run(
            [compiler, str(source / f"{name}.c"), "-o", str(directory / name)],
            directory, env, timeout,
        )
        if code or timed_out:
            raise ValueError(f"Cannot build upstream fixture {name}.\n{log}")
    return {key: directory / name for key, name in HELPERS.items()}


def checked_run(command, cwd, env, timeout):
    """Run an upstream setup command and report its full log on failure."""
    code, timed_out, log = run(command, cwd, env, timeout)
    if code or timed_out:
        reason = "timed out" if timed_out else f"exited with status {code}"
        raise ValueError(f"{subprocess.list2cmdline(map(str, command))} {reason}.\n{log}")
    return log


def companion(driver, name):
    """Find a tool that belongs to the oracle installation."""
    return Path(os.environ.get(f"RCMAKE_ORACLE_{name.upper()}",
                               driver.with_name(name + EXE_SUFFIX)))


def registration_generators(suites, driver, work, env, timeout):
    """Select the default generator and the dedicated platform generators."""
    host = platform.system()
    generators = ["Ninja"] if shutil.which("ninja") else []
    if host == "Windows":
        if (not generators or "Make" in suites) and shutil.which("nmake"):
            generators.append("NMake Makefiles")
        if not generators or set(suites) & WINDOWS_ONLY:
            capabilities = json.loads(checked_run([driver, "-E", "capabilities"], work, env, timeout))
            visual_studio = [
                entry["name"] for entry in capabilities["generators"]
                if entry["name"].startswith("Visual Studio ")
            ]
            generators.extend(visual_studio)
    else:
        if not generators or "Make" in suites:
            generators.append("Unix Makefiles")
        if host == "Darwin" and any(suite.startswith("XcodeProject") for suite in suites):
            generators.append("Xcode")
    return generators


def read_registrations(suites, driver, source, work, env, timeout, test_vars, use_upstream_timeout, inventory):
    """Ask upstream CMake to register tests. Keep each registration with its arguments."""
    registrations = defaultdict(list)
    generators = registration_generators(suites, driver, work, env, timeout)
    inventory["registration_generators"] = generators
    # These tests use the file system in the temporary workspace.
    (work / "FilesystemCase").touch()
    case_sensitive = int(not (work / "filesystemcase").exists())
    options = ["-DCMake_TEST_FindPython3=ON", "-DCMake_TEST_Qt5=OFF", "-DCMake_TEST_Qt6=OFF",
               "-DCMake_TEST_NO_NETWORK=ON", f"-DCMake_TEST_FILESYSTEM_CASE={case_sensitive}"]
    if "Swift" not in suites:
        options.extend(("-DCMake_TEST_Swift=OFF", "-DCMAKE_Swift_COMPILER=NOTFOUND"))
    options.append("-DCMAKE_Fortran_COMPILER=NOTFOUND")
    if "CPack_NuGet" in suites and shutil.which("nuget"):
        options.append("-DCMake_TEST_CPACK_NUGET=ON")
    if "CPack_WIX" in suites and shutil.which("wix"):
        options.append("-DCMake_TEST_CPACK_WIX4=ON")
    if "CPack_WIX" in suites and all(shutil.which(tool) for tool in ("candle", "light")):
        options.append("-DCMake_TEST_CPACK_WIX3=ON")
    configured_vs = False
    for index, generator in enumerate(generators):
        visual_studio = generator.startswith("Visual Studio ")
        if visual_studio and configured_vs:
            continue
        build = work / f"g{index}"
        try:
            checked_run([
                driver, "-S", source, "-B", build, "-G", generator, "-DBUILD_TESTING=ON",
                "-DCMake_TEST_HOST_CMAKE=ON", "-DCMAKE_BUILD_TYPE=Debug", *options,
                *(f"-D{value}" for value in test_vars),
            ], work, env, timeout)
        except ValueError:
            if os.name == "nt" and index + 1 < len(generators):
                continue
            raise
        configured_vs |= visual_studio
        inventory["configured_generators"].append(generator)
        listing = checked_run([
            companion(driver, "ctest"), "--test-dir", build, "-C", "Debug", "--show-only=json-v1",
            "-R", r"^RunCMake\.",
        ], work, env, timeout)
        fixtures = set()
        selected = set(suites) - registrations.keys()
        for test in json.loads(listing)["tests"]:
            command = test.get("command", ())
            if len(command) < 3 or command[-2] != "-P":
                continue
            script = Path(command[-1]).relative_to(source)
            suite = script.parent.name
            if suite not in selected or (suite in VS_SUITES and not visual_studio):
                continue
            arguments = tuple(command[1:-2])
            properties = {prop["name"]: prop["value"] for prop in test["properties"]}
            upstream_timeout = properties.get("TIMEOUT") or CTEST_DEFAULT_TIMEOUT
            registrations[suite].append(Registration(
                test["name"].removeprefix("RunCMake."), generator, script, build, arguments,
                Path(properties["WORKING_DIRECTORY"]).relative_to(build),
                upstream_timeout if use_upstream_timeout else min(timeout, upstream_timeout),
            ))
            for argument in arguments:
                value = argument.partition("=")[2]
                if (not argument.startswith("-DRunCMake_BINARY_DIR=")
                        and value.startswith(f"{build.as_posix()}/Tests/RunCMake/")):
                    fixture = Path(value)
                    if fixture.parent.name in ("RunCMake", "Debug") and fixture.suffix in ("", ".exe"):
                        fixtures.add(fixture.stem)
        if fixtures:
            checked_run([
                driver, "--build", build, "--config", "Debug", "--parallel", "2",
                "--target", *sorted(fixtures),
            ], work, env, timeout)
        if set(suites) <= registrations.keys():
            break
    return registrations


def target_tools(driver, target, directory):
    """Select the CTest and CPack entry points for this target binary."""
    if target == driver:
        return {name: companion(driver, name) for name in ("ctest", "cpack")}
    directory.mkdir()
    tools = {}
    for name in ("ctest", "cpack"):
        path = directory / (name + EXE_SUFFIX)
        if os.name == "nt":
            shutil.copy2(target, path)
        else:
            path.symlink_to(target)
        tools[name] = path
    return tools


def run_registered_suite(registrations, driver, target, source, root, tools, wrapper, env):
    """Replay each CTest command with its original working directory layout."""
    cases = {}
    variants = {}
    occurrences = Counter()
    for entry in registrations:
        build = root / entry.directory.name
        cwd = build / entry.working_directory
        cwd.mkdir(parents=True, exist_ok=True)
        arguments = []
        for argument in entry.arguments:
            if argument.startswith("-DRunCMake_BINARY_DIR="):
                argument = argument.replace(entry.directory.as_posix(), build.as_posix(), 1)
            arguments.append(argument.replace(source.as_posix(), (root / "source").as_posix()))
        command = [
            driver, *arguments, f"-DEVAL_TEST_BINARY={target.as_posix()}",
            f"-DEVAL_TEST_CTEST={tools['ctest'].as_posix()}",
            f"-DEVAL_TEST_CPACK={tools['cpack'].as_posix()}",
            f"-DEVAL_TEST_SCRIPT={(root / 'source' / entry.script).as_posix()}", "-P", wrapper,
        ]
        home = cwd / "TestHome"
        config = home / ".config" / "cmake"
        config.mkdir(parents=True, exist_ok=True)
        test_env = {**env, "CMAKE_CONFIG_DIR": str(config), "CMAKE_MAXIMUM_RECURSION_DEPTH": "100"}
        if "HOME" in env:
            test_env.update(HOME=str(home), CTEST_REAL_HOME=env["HOME"])
        code, timed_out, log = run(command, cwd, test_env, entry.timeout)
        reported, result = case_results(code, timed_out, log)
        if not reported and not result["skips"] and result["complete"] and not result["driver_errors"]:
            if entry.name == "CacheVarHelpCoverage" and "Built-in cache documentation coverage OK:" in log:
                reported[entry.name, 1] = {"passed": True, "detail": ""}
            else:
                result.update(driver_errors=["The upstream registration reported no cases."], log=log)
        for (name, _), case in reported.items():
            if len(registrations) > 1:
                name = f"{entry.name}: {name}"
            occurrences[name] += 1
            cases[name, occurrences[name]] = case
        variants[entry.name] = {
            "generator": entry.generator, "arguments": arguments, "timeout_seconds": entry.timeout, **result,
        }
    result = {
        "complete": all(item["complete"] for item in variants.values()),
        "timed_out": any(item["timed_out"] for item in variants.values()),
        "driver_errors": [line for item in variants.values() for line in item["driver_errors"]],
        "variants": variants,
        "skips": [{"registration": name, **skip}
                  for name, item in variants.items() for skip in item["skips"]],
    }
    return cases, result


def run_suite(driver, target, source, build, wrapper, settings, env, timeout):
    """Run the upstream suite and collect each reported case in execution order."""
    build.mkdir()
    variables = {
        **settings,
        "CMAKE_MODULE_PATH": source.parent,
        "RunCMake_SOURCE_DIR": source,
        "RunCMake_BINARY_DIR": build,
        "RunCMake_TEST_TIMEOUT": min(timeout, 20),
        "EVAL_TEST_BINARY": target,
    }
    if source.name in M3_SUITES:
        if target == driver:
            ctest = os.environ.get("RCMAKE_ORACLE_CTEST", driver.with_name("ctest"))
            cpack = os.environ.get("RCMAKE_ORACLE_CPACK", driver.with_name("cpack"))
        else:
            ctest, cpack = build / "ctest", build / "cpack"
            ctest.symlink_to(target)
            cpack.symlink_to(target)
        variables.update(
            EVAL_TEST_CTEST=ctest, EVAL_TEST_CPACK=cpack,
            IMAGE_DIR=source.parents[2] / "Utilities/Sphinx/static",
        )
    if source.name == "CPack":
        config_file = build / "TGZ_config.cmake"
        config_file.touch()
        variables.update(TEST_TYPE="TGZ", config_file=config_file)
    command = [driver, *(f"-D{key}={value}" for key, value in variables.items()), "-P", wrapper]
    code, timed_out, log = run(command, build, env, timeout)
    return case_results(code, timed_out, log)


def case_results(code, timed_out, log):
    """Read the case messages and detect errors in the upstream driver."""
    cases = {}
    occurrences = Counter()
    for match in CASE_RESULT.finditer(log):
        name = match["passed"] or match["failed"]
        occurrences[name] += 1
        cases[name, occurrences[name]] = {
            "passed": match["passed"] is not None,
            "detail": "\n".join(
                line.removeprefix("  ") for line in (match["detail"] or "").rstrip().splitlines()
            ),
        }
    complete = COMPLETE in log.splitlines() and code in (0, 1) and not timed_out
    other_errors, skips = [], []
    for line in CASE_RESULT.sub("", log).splitlines():
        if line.startswith(("CMake Error", "CMake Internal Error")):
            other_errors.append(line)
        match = RUN_CMAKE_SKIP.fullmatch(line) or CTEST_SKIP.fullmatch(line)
        if match:
            skips.append({"name": match["name"], "reason": match["reason"].strip(" :()") or None,
                          "line": line})
        elif line.startswith("-- ") and re.search(r"\b(?:skipping|being skipped)\b", line, re.IGNORECASE):
            skips.append({"name": None, "reason": line.removeprefix("-- "), "line": line})
    result = {"complete": complete, "timed_out": timed_out, "driver_errors": other_errors,
              "skips": skips}
    if not complete or other_errors:
        result["log"] = log
    return cases, result


def compare(reference, candidate):
    """Score each case that passed with the reference binary."""
    results = []
    for name, occurrence in dict.fromkeys((*reference, *candidate)):
        key = name, occurrence
        expected = reference.get(key)
        actual = candidate.get(key)
        if expected is None:
            status = "no_reference"
        elif not expected["passed"]:
            status = "reference_failed"
        elif actual is None:
            status = "missing"
        else:
            status = "passed" if actual["passed"] else "failed"
        results.append({
            "name": name, "occurrence": occurrence, "status": status,
            "oracle": expected, "rcmake": actual,
        })
    return results


def evaluate(suites, driver, candidate, source, timeout, profile, test_vars, use_upstream_timeout, report):
    """Run the selected suites in separate reference and candidate workspaces."""
    # A CLI case can print the test environment into the report.
    env = {key: os.environ[key] for key in TEST_ENVIRONMENT if key in os.environ}
    if os.name == "nt":
        env.update({key: os.environ[key] for key in WINDOWS_ENVIRONMENT if key in os.environ})

    ninja = shutil.which("ninja")
    generator = "Ninja" if ninja else "Unix Makefiles"
    settings = {
        "RunCMake_GENERATOR": generator,
        "RunCMake_GENERATOR_IS_MULTI_CONFIG": 0,
        "CMAKE_SYSTEM_NAME": platform.system(),
        "Python_EXECUTABLE": sys.executable,
    }
    if ninja:
        settings["RunCMake_MAKE_PROGRAM"] = ninja
    print(f"{'Suite':<32} {'Passed':>6} {'Failed':>6} {'Total':>6} {'Ref. fail':>9}", flush=True)
    report["generator"] = generator
    results = report["suites"]
    inventory = report["suite_inventory"]
    # Use the physical path. CMake and some checks canonicalize /tmp on macOS.
    with tempfile.TemporaryDirectory(prefix="runcmake-eval-", dir=None if os.name == "nt" else "/tmp") as temporary:
        work = Path(temporary).resolve()
        temp = work / "tmp"
        temp.mkdir()
        env["TMPDIR"] = str(temp)
        if os.name == "nt":
            env.update(TEMP=str(temp), TMP=str(temp))
        wrapper = work / "driver.cmake"
        wrapper.write_text(
            'set(CMAKE_COMMAND "${EVAL_TEST_BINARY}")\n'
            'if(DEFINED EVAL_TEST_SCRIPT)\n'
            '  set(CMAKE_CMAKE_COMMAND "${EVAL_TEST_BINARY}")\n'
            'else()\n'
            '  set(EVAL_TEST_SCRIPT "${RunCMake_SOURCE_DIR}/RunCMakeTest.cmake")\n'
            'endif()\n'
            'if(DEFINED EVAL_TEST_CTEST)\n'
            '  set(CMAKE_CTEST_COMMAND "${EVAL_TEST_CTEST}")\n'
            '  set(CMAKE_CPACK_COMMAND "${EVAL_TEST_CPACK}")\n'
            'endif()\n'
            'include("${EVAL_TEST_SCRIPT}" NO_POLICY_SCOPE)\n'
            'message(STATUS "runcmake_eval: suite complete")\n', encoding="utf-8",
        )
        legacy = set(SUITE_GROUPS["all"]) if profile in (None, "core", "wide") and os.name == "posix" else set()
        modern = [suite for suite in suites if suite not in legacy]
        registered = read_registrations(
            modern, driver, source.parents[1], work, env, timeout, test_vars, use_upstream_timeout, inventory,
        ) if modern else {}
        inventory["suite_generators"] = {
            suite: sorted({entry.generator for entry in entries}) for suite, entries in registered.items()
        }
        inventory["suite_generators"].update({suite: [generator] for suite in suites if suite in legacy})
        if {"CommandLine", "execute_process"}.intersection(suites, legacy):
            settings.update(prepare_helpers(source, work / "helpers", env, timeout))
        relative_source = source.relative_to(source.parents[1])
        tools = {}
        for lane, target in (("oracle", driver), ("rcmake", candidate)):
            shutil.copytree(
                source.parents[1], work / lane / "source", symlinks=True,
                ignore=shutil.ignore_patterns("._*", ".git"),
            )
            (work / lane / "build" / relative_source).mkdir(parents=True)
            if modern:
                tools[lane] = target_tools(driver, target, work / lane / "bin")
        for suite in suites:
            by_lane = {}
            for lane, target in (("oracle", driver), ("rcmake", candidate)):
                root = work / lane
                if suite in legacy:
                    by_lane[lane] = run_suite(
                        driver, target, root / "source" / relative_source / suite,
                        root / "build" / relative_source / suite, wrapper, settings, env, timeout,
                    )
                elif suite in registered:
                    by_lane[lane] = run_registered_suite(
                        registered[suite], driver, target, source.parents[1], root, tools[lane], wrapper, env,
                    )
                else:
                    by_lane[lane] = {}, {
                        "complete": False, "timed_out": False, "driver_errors": [
                            "Upstream CMake did not register this suite with the selected generators and tools.",
                        ],
                    }
            reference, oracle_driver = by_lane["oracle"]
            actual, rcmake_driver = by_lane["rcmake"]
            cases = compare(reference, actual)
            count = Counter(case["status"] for case in cases)
            failed = count["failed"] + count["missing"]
            total = count["passed"] + failed
            results[suite] = {
                "passed": count["passed"], "failed": failed, "total": total,
                "missing": count["missing"], "reference_failed": count["reference_failed"],
                "no_reference": count["no_reference"], "cases": cases,
                "oracle_driver": oracle_driver, "rcmake_driver": rcmake_driver,
            }
            print(
                f"{suite:<32} {count['passed']:>6} {failed:>6} {total:>6}"
                f" {count['reference_failed']:>9}", flush=True,
            )
            for lane, metadata in (("oracle", oracle_driver), ("Rcmake", rcmake_driver)):
                if metadata["timed_out"]:
                    print(f"  {lane} exceeded the suite timeout.", flush=True)
                elif not metadata["complete"] or metadata["driver_errors"]:
                    print(f"  {lane} reported a driver error.", flush=True)
                    if suite not in legacy and metadata["driver_errors"]:
                        print(f"  {metadata['driver_errors'][0]}", flush=True)
            # Keep failure messages readable after the temporary directories are removed.
            suite_result = results[suite]
            for case in suite_result["cases"]:
                for lane in ("oracle", "rcmake"):
                    if case[lane] is not None:
                        case[lane]["detail"] = case[lane]["detail"].replace(str(work), "<sandbox>")
            for lane in ("oracle_driver", "rcmake_driver"):
                for metadata in (suite_result[lane], *suite_result[lane].get("variants", {}).values()):
                    metadata["driver_errors"] = [
                        line.replace(str(work), "<sandbox>") for line in metadata["driver_errors"]
                    ]
                    if "log" in metadata:
                        metadata["log"] = metadata["log"].replace(str(work), "<sandbox>")
                    if "arguments" in metadata:
                        metadata["arguments"] = [arg.replace(work.as_posix(), "<sandbox>")
                                                 for arg in metadata["arguments"]]


def main():
    """Parse options, run the comparison, and write the report."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--suite", action="append", metavar="NAME",
        help="Select a suite by name. Repeat to select more. Use all for the selected group.",
    )
    parser.add_argument(
        "--suite-group", choices=SUITE_GROUPS, default="m1m2",
        help="Select a suite group. The default is m1m2.",
    )
    parser.add_argument("--profile", choices=PROFILES, help="Select an upstream suite profile.")
    parser.add_argument("--all", action="append_const", const="all", dest="suite",
                        help="Select all suites in the selected profile or group.")
    parser.add_argument("--list", "--list-suites", action="store_true",
                        help="List the selected upstream suites.")
    parser.add_argument("--json", type=Path, metavar="PATH", help="Write the case report to PATH.")
    parser.add_argument(
        "--timeout", type=float, metavar="SECONDS",
        help="Limit each suite run. Legacy groups default to 180 seconds. Other suites use CTest timeouts.",
    )
    parser.add_argument("--min-pass-rate", type=float, metavar="PERCENT",
                        help="Require this case pass rate and completion of every selected suite.")
    binary = parser.add_mutually_exclusive_group()
    binary.add_argument(
        "--rcmake-bin", "--bin", type=Path, metavar="PATH",
        help="Use this Rcmake executable. The default is $RCMAKE_ROOT/target/stage/bin/cmake.",
    )
    binary.add_argument("--stage", type=Path, nargs="?", const=False, metavar="DIRECTORY",
                        help="Use the cmake executable in this stage directory.")
    parser.add_argument("--test-var", action="append", default=[], metavar="CMake_TEST_NAME=VALUE",
                        help="Set an upstream test option for the expanded harness. Repeat as needed.")
    args = parser.parse_args()
    timeout = DEFAULT_TIMEOUT if args.timeout is None else args.timeout
    if not math.isfinite(timeout) or timeout <= 0:
        parser.error("--timeout must be a finite number greater than zero.")
    if any(not re.fullmatch(r"CMake_TEST_\w+=.*", value) for value in args.test_var):
        parser.error("--test-var requires CMake_TEST_NAME=VALUE.")
    if args.min_pass_rate is not None and not 0 <= args.min_pass_rate <= 100:
        parser.error("--min-pass-rate must be a percentage from 0 to 100.")

    inventory = {"included": [], "excluded_external": {}, "registration_generators": [],
                 "configured_generators": [], "suite_generators": {}}
    report = {"profile": args.profile, "timeout_seconds": timeout, "min_pass_rate": args.min_pass_rate,
              "rcmake_sha": os.environ.get("GITHUB_SHA"), "suite_inventory": inventory, "suites": {}}
    try:
        source_value = os.environ.get("RCMAKE_ORACLE_RUNCMAKE_DIR")
        if source_value is None and "RCMAKE_ORACLE_SRC" in os.environ:
            source_value = Path(os.environ["RCMAKE_ORACLE_SRC"]) / "Tests" / "RunCMake"
        if source_value is None:
            raise ValueError("Set RCMAKE_ORACLE_SRC or source env/env.sh.")
        source = Path(source_value).resolve()
        report["source"] = str(source)
        if source.parts[-2:] != ("Tests", "RunCMake") or not (source / "RunCMake.cmake").is_file():
            raise ValueError("RCMAKE_ORACLE_RUNCMAKE_DIR must select Tests/RunCMake in the CMake source tree.")
        group = profile_suites(source, args.profile) if args.profile else SUITE_GROUPS[args.suite_group]
        suites = list(dict.fromkeys(
            suite for name in (args.suite or ["all"])
            for suite in (group if name == "all" else ALIASES.get(name, (name,)))
        ))
        inventory["included"] = suites
        inventory["excluded_external"] = {
            name: "Requires an external tool, package, or test fixture outside the standard profile."
            for name in sorted(EXTERNAL_SUITES - set(suites))
            if (source / name / "RunCMakeTest.cmake").is_file()
        }
        for suite in suites:
            if (not re.fullmatch(r"[A-Za-z0-9_][A-Za-z0-9_.+-]*", suite)
                    or not (source / suite / "RunCMakeTest.cmake").is_file()):
                raise ValueError(f"Unknown upstream RunCMake suite {suite!r}.")
        if args.list:
            print("\n".join(suites))
            return 0
        host = {"all-macos": "Darwin", "linux": "Linux", "windows": "Windows"}.get(args.profile)
        if host and platform.system() != host:
            raise ValueError(f"The {args.profile} profile requires a {host} host. Use --list-suites to list its suites.")

        driver = Path(os.environ["RCMAKE_ORACLE_CMAKE"]).absolute()
        report["oracle"] = str(driver)
        candidate = args.rcmake_bin
        if candidate is None:
            stage = args.stage
            if not stage:
                stage = Path(os.environ["RCMAKE_ROOT"]) / "target" / "stage" / "bin"
            elif (stage / "bin").is_dir():
                stage /= "bin"
            candidate = stage / ("cmake" + EXE_SUFFIX)
        candidate = candidate.absolute()  # Keep the cmake symlink for multi-call dispatch.
        report["rcmake"] = str(candidate)
        version = subprocess.check_output([driver, "--version"], timeout=timeout, text=True).splitlines()
        if version[:1] != ["cmake version 4.5.1"]:
            raise ValueError("RCMAKE_ORACLE_CMAKE must select CMake 4.5.1.")
        if not candidate.is_file() or not os.access(candidate, os.X_OK):
            raise ValueError(f"Rcmake binary is missing or cannot run: {candidate}")
        evaluate(suites, driver, candidate, source, timeout, args.profile, args.test_var,
                 args.timeout is None, report)
    except (KeyError, OSError, ValueError, subprocess.SubprocessError, KeyboardInterrupt) as exc:
        error = (f"Source env/env.sh to set {exc.args[0]}." if isinstance(exc, KeyError)
                 else str(exc) or "Run interrupted.")
        report["error"] = error
        print(f"{parser.prog}: {error}", file=sys.stderr)
        return 2
    finally:
        results = report["suites"]
        totals = {
            key: sum(suite[key] for suite in results.values())
            for key in ("passed", "failed", "total", "missing", "reference_failed", "no_reference")
        }
        rate = 100 * totals["passed"] / totals["total"] if totals["total"] else None
        incomplete = [
            name for name in inventory["included"]
            if name not in results or any(
                not results[name][f"{lane}_driver"]["complete"] or results[name][f"{lane}_driver"]["driver_errors"]
                for lane in ("oracle", "rcmake")
            )
        ]
        report.update(totals=totals, pass_rate_percent=rate, incomplete_suites=incomplete)
        report["skips"] = [
            {"suite": name, "engine": lane, **skip}
            for name, suite in results.items() for lane in ("oracle", "rcmake")
            for skip in suite[f"{lane}_driver"].get("skips", [])
        ]
        if args.json and (not args.list or "error" in report):
            args.json.parent.mkdir(parents=True, exist_ok=True)
            args.json.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    print(
        f"{'Overall':<32} {totals['passed']:>6} {totals['failed']:>6} "
        f"{totals['total']:>6} {totals['reference_failed']:>9}"
    )
    print(f"Pass rate: {rate:.2f}%" if rate is not None else "Pass rate unavailable.")
    if incomplete:
        print(f"Incomplete suites: {len(incomplete)}", file=sys.stderr)
    if rate is None or incomplete:
        return 2
    if args.min_pass_rate is not None:
        return int(rate < args.min_pass_rate)
    return int(bool(totals["failed"] or totals["reference_failed"] or totals["no_reference"]))


if __name__ == "__main__":
    raise SystemExit(main())
