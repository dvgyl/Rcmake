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

The score includes cases that pass with CMake 4.5.1 and the Rcmake debugger
cases that run on the verified capability branch. A missing case fails the
comparison. Use --min-pass-rate to allow ordinary case failures within a
required pass rate. Exit status 1 means the comparison failed the case gate.
Exit status 2 means the script could not complete the comparison.
"""

from __future__ import annotations

import argparse
from collections import Counter, defaultdict
from concurrent.futures import ThreadPoolExecutor
from contextlib import nullcontext
from dataclasses import dataclass
import hashlib
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
EXTENDED_SUITES = (
    "CPackCommandLine", "discover_tests", "CMakePresetsTest", "CMakePresetsPackage",
    "CMakePresetsWorkflow", "CTestCommandLine", "CPackConfig", "FileSet-SOURCES",
    "FileSetProperties", "GoogleTest", "find_package-CPS", "InstallPackageInfo",
    "ExportPackageInfo", "InstallSbom", "ExportSbom", "CPack", "ctest_test",
    "ctest_start", "ctest_fixtures", "Instrumentation", "CXXModules",
)
SUITE_GROUPS = {"m1m2": SUITES, "m3": EXTENDED_SUITES, "all": SUITES + EXTENDED_SUITES}
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
    "TEMP", "TMP", "WINDIR", "ALLUSERSPROFILE", "CommonProgramFiles", "CommonProgramFiles(x86)",
    "PROCESSOR_ARCHITECTURE", "NUMBER_OF_PROCESSORS", "HOMEDRIVE", "HOMEPATH",
    "VCINSTALLDIR", "VCToolsInstallDir", "VSINSTALLDIR", "VisualStudioVersion",
    "VS160COMNTOOLS", "VS170COMNTOOLS", "VS180COMNTOOLS",
    "WindowsSdkDir", "WindowsSDKVersion", "DOTNET_ROOT", "WIX",
)
COMPLETE = "-- runcmake_eval: suite complete"
CASE_RESULT = re.compile(
    r"^-- (?P<passed>.+) - PASSED$"
    r"|^CMake Error at .*[/\\]RunCMake\.cmake:\d+ \(message\):\n"
    # CMake can wrap before the hyphen or before FAILED.
    r"(?:[ \t]*\n)*  (?P<failed>\S[^\n]*?)(?: |\n  )-(?: |\n  )FAILED:\n"
    r"(?P<detail>(?:^[ \t].*\n|^\n)*)",
    re.MULTILINE,
)
CHECK_SCRIPT_ERROR = re.compile(
    r"^CMake Error at (?P<location>.+?):\n"
    r"(?P<body>(?:^[ \t].*\n|^\n)*)"
    r"^Call Stack \(most recent call first\):\n"
    r"(?P<stack>(?:^[ \t].*\n)+)",
    re.MULTILINE,
)
CHECK_SCRIPT_FRAME = re.compile(r"[/\\]RunCMake\.cmake:\d+ \(include\)\s*$", re.MULTILINE)
CHECK_SCRIPT_FILE = re.compile(r"-check[^/\\]*\.cmake:\d+\b")

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


def visual_studio_instance(env):
    """Return the generator prefix and installation path for the active Visual Studio."""
    major = env.get("VisualStudioVersion", "").partition(".")[0]
    path = env.get("VSINSTALLDIR", "").rstrip("\\/")
    return (f"Visual Studio {major} ", path) if major.isdecimal() and path else (None, None)


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
            preferred, _ = visual_studio_instance(env)
            if preferred:
                visual_studio.sort(key=lambda name: not name.startswith(preferred))
            generators.extend(visual_studio)
    else:
        if not generators or "Make" in suites:
            generators.append("Unix Makefiles")
        if host == "Darwin" and any(suite.startswith("XcodeProject") for suite in suites):
            generators.append("Xcode")
    return generators


def read_registrations(
    suites, driver, source, work, env, timeout, test_vars, use_upstream_timeout, inventory,
    *, registration_suites=None,
):
    """Ask upstream CMake to register tests. Keep each registration with its arguments."""
    registrations = defaultdict(list)
    profile_scope = suites if registration_suites is None else registration_suites
    generators = registration_generators(profile_scope, driver, work, env, timeout)
    inventory["registration_generators"] = generators
    # The case mismatch tests require case-insensitive lookup and case preservation.
    probe = work / "FilesystemCase"
    probe.touch()
    filesystem_case = int((work / probe.name.lower()).exists()
                          and any(entry.name == probe.name for entry in work.iterdir()))
    options = ["-DCMake_TEST_FindPython3=ON", "-DCMake_TEST_Qt5=OFF", "-DCMake_TEST_Qt6=OFF",
               "-DCMake_TEST_NO_NETWORK=ON", f"-DCMake_TEST_FILESYSTEM_CASE={filesystem_case}"]
    if "Swift" not in profile_scope:
        options.extend(("-DCMake_TEST_Swift=OFF", "-DCMAKE_Swift_COMPILER=NOTFOUND"))
    options.append("-DCMAKE_Fortran_COMPILER=NOTFOUND")
    if "CPack_NuGet" in profile_scope and shutil.which("nuget"):
        options.append("-DCMake_TEST_CPACK_NUGET=ON")
    if "CPack_WIX" in profile_scope and shutil.which("wix"):
        options.append("-DCMake_TEST_CPACK_WIX4=ON")
    if "CPack_WIX" in profile_scope and all(shutil.which(tool) for tool in ("candle", "light")):
        options.append("-DCMake_TEST_CPACK_WIX3=ON")
    preferred_vs, instance = visual_studio_instance(env)
    configured_vs = False
    vs_errors = []
    for index, generator in enumerate(generators):
        visual_studio = generator.startswith("Visual Studio ")
        if visual_studio and configured_vs:
            continue
        build = work / f"g{index}"
        generator_options = []
        if preferred_vs and generator.startswith(preferred_vs):
            generator_options.append(f"-DCMAKE_GENERATOR_INSTANCE={instance}")
        try:
            checked_run([
                driver, "-S", source, "-B", build, "-G", generator, "-DBUILD_TESTING=ON",
                "-DCMake_TEST_HOST_CMAKE=ON", "-DCMAKE_BUILD_TYPE=Debug", *options, *generator_options,
                *(f"-D{value}" for value in test_vars),
            ], work, env, timeout)
        except ValueError as error:
            if visual_studio:
                vs_errors.append(error)
                continue
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
        if "pseudo_pvs" in fixtures:
            # CMake searches for the converter in the analyzer's directory.
            fixtures.add("pseudo_plog_converter")
        if fixtures:
            checked_run([
                driver, "--build", build, "--config", "Debug", "--parallel", "2",
                "--target", *sorted(fixtures),
            ], work, env, timeout)
        if set(suites) <= registrations.keys():
            break
    if vs_errors and not configured_vs:
        raise vs_errors[0]
    return registrations


def target_tools(driver, target, directory):
    """Select the CTest and CPack entry points for this target binary."""
    if target == driver:
        return {name: companion(driver, name) for name in ("ctest", "cpack")}
    if os.name == "nt":
        siblings = {name: target.with_name(name + EXE_SUFFIX) for name in ("ctest", "cpack")}
        if all(path.is_file() for path in siblings.values()):
            return siblings
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
        if entry.script.parent.name == "MaxRecursionDepth":
            # Keep the driver running when the suite lowers the environment limit for a child.
            arguments.append("-DCMAKE_MAXIMUM_RECURSION_DEPTH=100")
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
            test_env.update(HOME=str(home), CTEST_REAL_HOME=env.get("CTEST_REAL_HOME", env["HOME"]))
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
    if source.name in EXTENDED_SUITES:
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
    case_matches = list(CASE_RESULT.finditer(log))
    case_spans = [(m.start(), m.end()) for m in case_matches]

    def overlaps_case(start, end):
        return any(start < c_end and end > c_start for c_start, c_end in case_spans)

    check_errors_by_case = defaultdict(list)
    consumed_spans = list(case_spans)
    for err in CHECK_SCRIPT_ERROR.finditer(log):
        if overlaps_case(err.start(), err.end()) or not CHECK_SCRIPT_FRAME.search(err["stack"]):
            continue
        if not (CHECK_SCRIPT_FILE.search(err["location"]) or CHECK_SCRIPT_FILE.search(err["stack"])):
            continue
        next_case = next((idx for idx, m in enumerate(case_matches) if m.start() >= err.end()), None)
        if next_case is None:
            continue
        body = "\n".join(line.removeprefix("  ") for line in err["body"].rstrip().splitlines())
        snippet = f"CMake Error at {err['location']}:\n{body}" if body else f"CMake Error at {err['location']}:"
        check_errors_by_case[next_case].append(snippet)
        consumed_spans.append((err.start(), err.end()))

    for idx, match in enumerate(case_matches):
        name = match["passed"] or match["failed"]
        occurrences[name] += 1
        extra_errors = check_errors_by_case.get(idx)
        detail_parts = []
        raw_detail = "\n".join(
            line.removeprefix("  ") for line in (match["detail"] or "").rstrip().splitlines()
        )
        if raw_detail:
            detail_parts.append(raw_detail)
        if extra_errors:
            detail_parts.extend(extra_errors)
        cases[name, occurrences[name]] = {
            "passed": match["passed"] is not None and not extra_errors,
            "detail": "\n\n".join(detail_parts),
        }

    consumed_spans.sort()
    pieces, cursor = [], 0
    for start, end in consumed_spans:
        if start > cursor:
            pieces.append(log[cursor:start])
        cursor = max(cursor, end)
    pieces.append(log[cursor:])
    remainder = "".join(pieces)

    complete = COMPLETE in log.splitlines() and code in (0, 1) and not timed_out
    other_errors, skips = [], []
    for line in remainder.splitlines():
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


DEBUGGER_INSPECTION = "DebuggerCapabilityInspect"
DEBUGGER_ENABLED_CASES = ("DebuggerArgMissingPipe", "DebuggerArgMissingDapLog")
DEBUGGER_DISABLED_CASES = (
    "DebuggerNotSupported", "DebuggerNotSupportedPipe", "DebuggerNotSupportedDapLog",
)


def debugger_branches(suite, reference, candidate):
    """Find registrations that ran the two expected debugger branches."""
    statuses, branches = {}, []
    if suite != "CommandLine":
        return statuses, branches
    for (name, occurrence), outcome in reference.items():
        registration, separator, case = name.rpartition(": ")
        if case != DEBUGGER_INSPECTION or occurrence != 1 or outcome["passed"] is not True:
            continue
        prefix = registration + separator
        enabled = {(prefix + case, 1) for case in DEBUGGER_ENABLED_CASES}
        disabled = {(prefix + case, 1) for case in DEBUGGER_DISABLED_CASES}
        names = {name for name, _ in enabled | disabled}
        if ({key for key in reference if key[0] in names} != enabled
                or {key for key in candidate if key[0] in names} != disabled
                or any(reference[key]["passed"] is not True for key in enabled)):
            continue
        statuses.update(dict.fromkeys(enabled, "capability_excluded"))
        statuses.update({key: "passed" if candidate[key]["passed"] else "failed" for key in disabled})
        branches.append({"capability": "debugger", "registration": registration or None,
                         "oracle": True, "rcmake": False})
    return statuses, branches


def compare(reference, candidate, *, branch_statuses=None):
    """Score the cases with the results of any verified capability branches."""
    branch_statuses = branch_statuses or {}
    results = []
    for name, occurrence in dict.fromkeys((*reference, *candidate)):
        key = name, occurrence
        expected = reference.get(key)
        actual = candidate.get(key)
        if key in branch_statuses:
            status = branch_statuses[key]
        elif expected is None:
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


def case_counts(cases):
    """Count scored cases and cases with missing or failed reference results."""
    count = Counter(case["status"] for case in cases)
    failed = count["failed"] + count["missing"]
    return {"passed": count["passed"], "failed": failed, "total": count["passed"] + failed,
            "missing": count["missing"], "reference_failed": count["reference_failed"],
            "no_reference": count["no_reference"], "capability_excluded": count["capability_excluded"]}


def strict_case_issue(suite, row):
    """Return a case issue that fails the gate at any pass rate."""
    cases = row.get("cases", [])
    if not isinstance(cases, list):
        return "invalid cases"
    statuses = {case["status"] for case in cases
                if isinstance(case, dict) and isinstance(case.get("status"), str)}
    if (row.get("reference_failed") or row.get("no_reference")
            or statuses & {"reference_failed", "no_reference"}):
        return "reference"
    if row.get("missing") or "missing" in statuses:
        return "missing"
    if ("capability_branches" not in row and not row.get("capability_excluded")
            and "capability_excluded" not in statuses):
        return None

    reference, candidate, recorded = {}, {}, {}
    for case in cases:
        if not isinstance(case, dict):
            return "capability_branches"
        name, occurrence = case.get("name"), case.get("occurrence")
        if not isinstance(name, str) or type(occurrence) is not int or occurrence < 1:
            return "capability_branches"
        key = name, occurrence
        if key in recorded:
            return "capability_branches"
        recorded[key] = case.get("status")
        for lane, outcomes in (("oracle", reference), ("rcmake", candidate)):
            outcome = case.get(lane)
            if outcome is not None:
                if not isinstance(outcome, dict) or type(outcome.get("passed")) is not bool:
                    return "capability_branches"
                outcomes[key] = outcome

    branch_statuses, branches = debugger_branches(suite, reference, candidate)
    expected = compare(reference, candidate, branch_statuses=branch_statuses)
    if (not branches or row.get("capability_branches") != branches
            or recorded != {(case["name"], case["occurrence"]): case["status"] for case in expected}
            or any(row.get(key) != value for key, value in case_counts(expected).items())):
        return "capability_branches"
    if "failed" in branch_statuses.values():
        return "capability_branch_failed"
    return None


def suite_environment(env, root):
    """Create private home and temporary directories for parallel execution."""
    home = root / "TestHome"
    config = home / ".config" / "cmake"
    temp = root / "tmp"
    config.mkdir(parents=True)
    temp.mkdir()
    result = {**env, "HOME": str(home), "CMAKE_CONFIG_DIR": str(config), "TMPDIR": str(temp),
              "TMP": str(temp), "TEMP": str(temp)}
    if "HOME" in env:
        result["CTEST_REAL_HOME"] = env["HOME"]
    if os.name == "nt":
        appdata = home / "AppData" / "Roaming"
        local_appdata = home / "AppData" / "Local"
        appdata.mkdir(parents=True, exist_ok=True)
        local_appdata.mkdir(parents=True, exist_ok=True)
        result.update(USERPROFILE=str(home), HOMEDRIVE=home.drive,
                      HOMEPATH=str(home)[len(home.drive):],
                      APPDATA=str(appdata),
                      LOCALAPPDATA=str(local_appdata))
    return result


def evaluate(
    suites, driver, candidate, source, timeout, profile, test_vars, use_upstream_timeout, report,
    *, jobs=1, registration_suites=None,
):
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
    # On Windows, keep sandbox prefixes short to stay below MAX_PATH / CMAKE_OBJECT_PATH_MAX.
    temp_base = os.environ.get("RCMAKE_EVAL_TEMP_DIR") or (None if os.name == "nt" else "/tmp")
    if temp_base is not None:
        Path(temp_base).mkdir(parents=True, exist_ok=True)
    temp_prefix = "rc-" if os.name == "nt" else "runcmake-eval-"
    with tempfile.TemporaryDirectory(prefix=temp_prefix, dir=temp_base) as temporary:
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
        scope_suites = suites if registration_suites is None else registration_suites
        modern_scope = [suite for suite in scope_suites if suite not in legacy]
        registered = read_registrations(
            modern, driver, source.parents[1], work, env, timeout, test_vars, use_upstream_timeout, inventory,
            registration_suites=modern_scope,
        ) if modern else {}
        inventory["suite_generators"] = {
            suite: sorted({entry.generator for entry in entries}) for suite, entries in registered.items()
        }
        inventory["suite_generators"].update({suite: [generator] for suite in suites if suite in legacy})
        if {"CommandLine", "execute_process"}.intersection(suites, legacy):
            settings.update(prepare_helpers(source, work / "helpers", env, timeout))
        relative_source = source.relative_to(source.parents[1])
        lanes = (("oracle", driver), ("rcmake", candidate))
        tools = {}

        def lane_root(lane):
            return work / (("o" if lane == "oracle" else "r") if os.name == "nt" else lane)

        for lane, target in lanes:
            base = lane_root(lane)
            base.mkdir()
            if jobs == 1:
                shutil.copytree(
                    source.parents[1], base / "source", symlinks=True,
                    ignore=shutil.ignore_patterns("._*", ".git"),
                )
                (base / "build" / relative_source).mkdir(parents=True)
            if modern:
                tools[lane] = target_tools(driver, target, base / "bin")

        def run_lane(suite, lane, target):
            if suite in legacy:
                def run_at(root, suite_env):
                    return run_suite(
                        driver, target, root / "source" / relative_source / suite,
                        root / "build" / relative_source / suite, wrapper, settings, suite_env, timeout,
                    )
            elif suite in registered:
                def run_at(root, suite_env):
                    return run_registered_suite(
                        registered[suite], driver, target, source.parents[1], root,
                        tools[lane], wrapper, suite_env,
                    )
            else:
                return {}, {
                    "complete": False, "timed_out": False, "driver_errors": [
                        "Upstream CMake did not register this suite with the selected generators and tools.",
                    ],
                }
            base = lane_root(lane)
            if jobs == 1:
                return run_at(base, env)
            # Some upstream suites modify source fixtures. Give each worker its own copy.
            worker_prefix = "w-" if os.name == "nt" else f"{suite}-"
            with tempfile.TemporaryDirectory(prefix=worker_prefix, dir=base) as directory:
                root = Path(directory)
                shutil.copytree(source.parents[1], root / "source", symlinks=True,
                                ignore=shutil.ignore_patterns("._*", ".git"))
                (root / "build" / relative_source).mkdir(parents=True)
                return run_at(root, suite_environment(env, root))

        def run_pair(suite):
            return [run_lane(suite, lane, target) for lane, target in lanes]

        def record(suite, by_lane):
            (reference, oracle_driver), (actual, rcmake_driver) = by_lane
            branch_statuses, branches = debugger_branches(suite, reference, actual)
            cases = compare(reference, actual, branch_statuses=branch_statuses)
            counts = case_counts(cases)
            results[suite] = {
                **counts, "cases": cases,
                "oracle_driver": oracle_driver, "rcmake_driver": rcmake_driver,
            }
            if branches:
                results[suite]["capability_branches"] = branches
            print(
                f"{suite:<32} {counts['passed']:>6} {counts['failed']:>6} {counts['total']:>6}"
                f" {counts['reference_failed']:>9}", flush=True,
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

        with ThreadPoolExecutor(max_workers=jobs) if jobs > 1 else nullcontext() as executor:
            pairs = executor.map(run_pair, suites) if executor else map(run_pair, suites)
            for suite, by_lane in zip(suites, pairs):
                record(suite, by_lane)


SHA256_RE = re.compile(r"^[0-9a-f]{64}$")


def sha256_file(path):
    """Return the lowercase SHA-256 hex digest of a file."""
    digest = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for chunk in iter(lambda: stream.read(65536), b""):
            digest.update(chunk)
    return digest.hexdigest()


def parse_shard(spec):
    """Parse a 1-based shard specifier K/M."""
    match = re.fullmatch(r"([1-9]\d*)/([1-9]\d*)", spec.strip())
    if not match:
        raise ValueError(f"--shard must be K/M with 1 <= K <= M, got {spec!r}.")
    index, count = int(match.group(1)), int(match.group(2))
    if index > count:
        raise ValueError(f"--shard index {index} exceeds shard count {count}.")
    return index, count


def select_shard_suites(suites, index, count):
    """Partition suites deterministically in round-robin order for shard index/count."""
    if count < 1 or not 1 <= index <= count:
        raise ValueError(f"Invalid shard {index}/{count}.")
    return [suite for pos, suite in enumerate(suites) if pos % count == index - 1]


def _reject_duplicate_json_keys(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"Duplicate JSON key {key!r}.")
        result[key] = value
    return result


def load_strict_json(path):
    """Load one JSON object, rejecting duplicate keys."""
    payload = json.loads(Path(path).read_text(encoding="utf-8"), object_pairs_hook=_reject_duplicate_json_keys)
    if not isinstance(payload, dict):
        raise ValueError(f"{path}: expected a JSON object.")
    return payload


def _provenance_from_env():
    provenance = {}
    for env_key, field in (
        ("GITHUB_RUN_ID", "workflow_run_id"),
        ("GITHUB_RUN_ATTEMPT", "workflow_run_attempt"),
        ("CI_BINARY", "ci_binary_artifact"),
        ("CI_BINARY_RUN_ID", "ci_binary_run_id"),
        ("RUNNER_OS", "runner_os"),
    ):
        value = os.environ.get(env_key)
        if value:
            provenance[field] = value
    return provenance


def case_status_issue(suite_name, case):
    """Verify a recorded case status matches its recorded oracle and rcmake outcomes."""
    if not isinstance(case, dict):
        return "invalid case entry"
    status = case.get("status")
    if not isinstance(status, str):
        return "missing case status"
    if "oracle" not in case and "rcmake" not in case:
        return None
    oracle = case.get("oracle")
    rcmake = case.get("rcmake")
    if status == "capability_excluded":
        if not (suite_name == "CommandLine" and isinstance(oracle, dict)
                and oracle.get("passed") is True and rcmake is None):
            return f"invalid capability_excluded outcome on {case.get('name')!r}"
        return None
    if oracle is None:
        if suite_name == "CommandLine" and isinstance(rcmake, dict):
            expected = "passed" if rcmake.get("passed") is True else "failed"
            return None if status == expected else f"status {status!r} != {expected!r} on {case.get('name')!r}"
        return None if status == "no_reference" else f"status {status!r} != 'no_reference' on {case.get('name')!r}"
    if not isinstance(oracle, dict):
        return f"invalid oracle outcome on {case.get('name')!r}"
    if oracle.get("passed") is not True:
        return None if status == "reference_failed" else f"status {status!r} != 'reference_failed' on {case.get('name')!r}"
    if rcmake is None:
        return None if status == "missing" else f"status {status!r} != 'missing' on {case.get('name')!r}"
    if not isinstance(rcmake, dict):
        return f"invalid rcmake outcome on {case.get('name')!r}"
    expected = "passed" if rcmake.get("passed") is True else "failed"
    return None if status == expected else f"status {status!r} != {expected!r} on {case.get('name')!r}"


def merge_shard_reports(reports, *, expected_shards=None, require_provenance=False):
    """Validate and merge a complete set of sharded RunCMake reports for one OS profile."""
    if not reports:
        raise ValueError("At least one shard report is required.")
    parsed = []
    shard_count = None
    seen_indices = set()
    for pos, report in enumerate(reports):
        if not isinstance(report, dict):
            raise ValueError(f"Shard report #{pos + 1} is not a JSON object.")
        if report.get("error"):
            raise ValueError(f"Shard report #{pos + 1} recorded an error: {report['error']}")
        shard = report.get("shard")
        if (not isinstance(shard, dict) or type(shard.get("index")) is not int
                or type(shard.get("count")) is not int):
            raise ValueError(f"Shard report #{pos + 1} is missing integer shard index/count metadata.")
        index, count = shard["index"], shard["count"]
        if count < 1 or not 1 <= index <= count:
            raise ValueError(f"Shard report #{pos + 1} has invalid shard {index}/{count}.")
        if shard_count is None:
            shard_count = count
        elif count != shard_count:
            raise ValueError(f"Shard count mismatch: saw {shard_count} and {count}.")
        if expected_shards is not None and count != expected_shards:
            raise ValueError(f"Expected {expected_shards} shards, found shard count {count}.")
        if index in seen_indices:
            raise ValueError(f"Duplicate shard index {index}/{count}.")
        seen_indices.add(index)
        parsed.append((index, report))

    assert shard_count is not None
    if len(parsed) != shard_count:
        raise ValueError(
            f"Missing RunCMake shard(s): expected {shard_count} shard reports, got {len(parsed)}."
        )
    expected_indices = set(range(1, shard_count + 1))
    if seen_indices != expected_indices:
        missing = sorted(expected_indices - seen_indices)
        raise ValueError(f"Missing RunCMake shard(s): {', '.join(f'{i}/{shard_count}' for i in missing)}.")
    parsed.sort(key=lambda item: item[0])

    first = parsed[0][1]
    profile = first.get("profile")
    rcmake_sha = first.get("rcmake_sha")
    tested_digest = first.get("tested_binary_sha256")
    tested_engine_digest = first.get("tested_engine_sha256")
    oracle_version = first.get("oracle_version")
    first_prov = first.get("provenance") if isinstance(first.get("provenance"), dict) else {}
    first_inv = first.get("suite_inventory")
    if not isinstance(first_inv, dict):
        raise ValueError("Shard 1 is missing suite_inventory.")
    expected_suites = first_inv.get("expected_profile_suites")
    if (not isinstance(expected_suites, list) or not expected_suites
            or not all(isinstance(name, str) and name for name in expected_suites)
            or len(set(expected_suites)) != len(expected_suites)):
        raise ValueError("Shard 1 is missing a valid unique suite_inventory.expected_profile_suites list.")

    if tested_digest is not None and (not isinstance(tested_digest, str) or not SHA256_RE.fullmatch(tested_digest)):
        raise ValueError(f"Invalid tested_binary_sha256 {tested_digest!r}.")
    if tested_engine_digest is not None and (
            not isinstance(tested_engine_digest, str) or not SHA256_RE.fullmatch(tested_engine_digest)):
        raise ValueError(f"Invalid tested_engine_sha256 {tested_engine_digest!r}.")
    if require_provenance:
        if not tested_digest:
            raise ValueError("Missing tested_binary_sha256 in CI shard reports.")
        for key in ("workflow_run_id", "ci_binary_artifact", "ci_binary_run_id"):
            val = first_prov.get(key)
            if not isinstance(val, str) or not val.strip():
                raise ValueError(f"Missing provenance.{key} in CI shard reports.")
    if oracle_version is not None and oracle_version != "4.5.1":
        raise ValueError(f"Unexpected oracle_version {oracle_version!r}; expected '4.5.1'.")

    merged_suites = {}
    suite_generators = {}
    registration_generators_merged = []
    configured_generators_merged = []
    excluded_external = dict(first_inv.get("excluded_external") or {})
    shard_summaries = []

    for index, report in parsed:
        if report.get("profile") != profile:
            raise ValueError(f"Shard {index}/{shard_count} profile {report.get('profile')!r} != {profile!r}.")
        if report.get("rcmake_sha") != rcmake_sha:
            raise ValueError(
                f"Shard {index}/{shard_count} rcmake_sha {report.get('rcmake_sha')!r} != {rcmake_sha!r}."
            )
        if report.get("tested_binary_sha256") != tested_digest:
            raise ValueError(
                f"Shard {index}/{shard_count} tested_binary_sha256 {report.get('tested_binary_sha256')!r} "
                f"!= {tested_digest!r}."
            )
        if report.get("tested_engine_sha256") != tested_engine_digest:
            raise ValueError(
                f"Shard {index}/{shard_count} tested_engine_sha256 {report.get('tested_engine_sha256')!r} "
                f"!= {tested_engine_digest!r}."
            )
        if report.get("oracle_version") != oracle_version:
            raise ValueError(
                f"Shard {index}/{shard_count} oracle_version {report.get('oracle_version')!r} != {oracle_version!r}."
            )
        prov = report.get("provenance") if isinstance(report.get("provenance"), dict) else {}
        for key in ("workflow_run_id", "ci_binary_artifact", "ci_binary_run_id"):
            if prov.get(key) != first_prov.get(key):
                raise ValueError(
                    f"Shard {index}/{shard_count} provenance.{key} {prov.get(key)!r} != {first_prov.get(key)!r}."
                )
        inv = report.get("suite_inventory")
        if not isinstance(inv, dict) or inv.get("expected_profile_suites") != expected_suites:
            raise ValueError(f"Shard {index}/{shard_count} expected_profile_suites does not match shard 1.")
        if report.get("incomplete_suites"):
            raise ValueError(
                f"Shard {index}/{shard_count} has incomplete suites: {', '.join(report['incomplete_suites'][:5])}."
            )
        assigned = select_shard_suites(expected_suites, index, shard_count)
        if inv.get("included") != assigned:
            raise ValueError(
                f"Shard {index}/{shard_count} suite_inventory.included does not match its deterministic shard slice."
            )
        suites = report.get("suites")
        if not isinstance(suites, dict):
            raise ValueError(f"Shard {index}/{shard_count} is missing suites map.")
        if set(suites.keys()) != set(assigned):
            missing = sorted(set(assigned) - set(suites.keys()))
            extra = sorted(set(suites.keys()) - set(assigned))
            raise ValueError(
                f"Shard {index}/{shard_count} suites mismatch (missing={missing[:5]}, extra={extra[:5]})."
            )
        for gen in inv.get("registration_generators") or ():
            if gen not in registration_generators_merged:
                registration_generators_merged.append(gen)
        for gen in inv.get("configured_generators") or ():
            if gen not in configured_generators_merged:
                configured_generators_merged.append(gen)
        shard_suite_gens = inv.get("suite_generators") or {}

        shard_passed = 0
        shard_total = 0
        for suite_name in assigned:
            if suite_name in merged_suites:
                raise ValueError(f"Duplicate suite {suite_name!r} across RunCMake shards.")
            row = suites[suite_name]
            if not isinstance(row, dict) or not isinstance(row.get("cases"), list):
                raise ValueError(f"Shard {index}/{shard_count} suite {suite_name!r} is missing cases list.")
            for lane in ("oracle_driver", "rcmake_driver"):
                driver_meta = row.get(lane)
                if (not isinstance(driver_meta, dict) or driver_meta.get("complete") is not True
                        or driver_meta.get("timed_out") is True or bool(driver_meta.get("driver_errors"))):
                    raise ValueError(f"Shard {index}/{shard_count} suite {suite_name!r} failed {lane} completion.")
            if issue := strict_case_issue(suite_name, row):
                raise ValueError(f"Shard {index}/{shard_count} suite {suite_name!r} failed strict check: {issue}.")
            for case in row["cases"]:
                if issue := case_status_issue(suite_name, case):
                    raise ValueError(
                        f"Shard {index}/{shard_count} suite {suite_name!r} has inconsistent case status: {issue}."
                    )
            recomputed = case_counts(row["cases"])
            for count_key, expected_val in recomputed.items():
                if row.get(count_key) != expected_val:
                    raise ValueError(
                        f"Shard {index}/{shard_count} suite {suite_name!r} count {count_key} "
                        f"is {row.get(count_key)!r}, expected {expected_val} from cases."
                    )
            shard_passed += recomputed["passed"]
            shard_total += recomputed["total"]
            merged_suites[suite_name] = row
            if suite_name in shard_suite_gens:
                suite_generators[suite_name] = shard_suite_gens[suite_name]

        shard_summaries.append({
            "index": index,
            "count": shard_count,
            "suites": len(assigned),
            "passed": shard_passed,
            "total": shard_total,
            **({"tested_binary_sha256": tested_digest} if tested_digest is not None else {}),
            **({"workflow_run_attempt": prov["workflow_run_attempt"]} if "workflow_run_attempt" in prov else {}),
        })

    if set(merged_suites.keys()) != set(expected_suites):
        missing = sorted(set(expected_suites) - set(merged_suites.keys()))
        raise ValueError(f"Merged suites missing expected profile suite(s): {', '.join(missing[:5])}.")

    ordered_suites = {name: merged_suites[name] for name in expected_suites}
    totals = {
        key: sum(suite[key] for suite in ordered_suites.values())
        for key in ("passed", "failed", "total", "missing", "reference_failed", "no_reference",
                    "capability_excluded")
    }
    pass_rate = 100 * totals["passed"] / totals["total"] if totals["total"] else None
    merged_skips = [
        {"suite": name, "engine": lane, **skip}
        for name, suite in ordered_suites.items()
        for lane in ("oracle", "rcmake")
        for skip in suite[f"{lane}_driver"].get("skips", [])
    ]
    merged_inventory = {
        "included": list(expected_suites),
        "expected_profile_suites": list(expected_suites),
        "profile_total_suites": len(expected_suites),
        "excluded_external": excluded_external,
        "registration_generators": registration_generators_merged,
        "configured_generators": configured_generators_merged,
        "suite_generators": {
            name: suite_generators[name] for name in expected_suites if name in suite_generators
        },
    }
    merged = {
        "profile": profile,
        "timeout_seconds": first.get("timeout_seconds"),
        "min_pass_rate": first.get("min_pass_rate"),
        "rcmake_sha": rcmake_sha,
        "oracle_version": oracle_version or "4.5.1",
        "generator": first.get("generator"),
        "source": first.get("source"),
        "oracle": first.get("oracle"),
        "rcmake": first.get("rcmake"),
        "suite_inventory": merged_inventory,
        "suites": ordered_suites,
        "totals": totals,
        "pass_rate_percent": pass_rate,
        "incomplete_suites": [],
        "skips": merged_skips,
        "shards": shard_summaries,
    }
    if tested_digest is not None:
        merged["tested_binary_sha256"] = tested_digest
    if tested_engine_digest is not None:
        merged["tested_engine_sha256"] = tested_engine_digest
    if first_prov:
        merged["provenance"] = first_prov
    return merged


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
    parser.add_argument("--json", "--out", dest="json", type=Path, metavar="PATH",
                        help="Write the case report to PATH.")
    parser.add_argument("--jobs", "-j", type=int, default=1, metavar="N",
                        help="Run up to N suites at the same time. The default is 1.")
    parser.add_argument(
        "--timeout", type=float, metavar="SECONDS",
        help="Limit each suite run. Legacy groups default to 180 seconds. Other suites use CTest timeouts.",
    )
    parser.add_argument("--min-pass-rate", type=float, metavar="PERCENT",
                        help="Require this case pass rate and completion of every selected suite.")
    parser.add_argument("--shard", metavar="K/M",
                        help="Run deterministic round-robin shard K of M (1 <= K <= M) of the selected suites.")
    parser.add_argument(
        "--defer-pass-rate", action="store_true",
        help="In shard mode, gate completion and strict case integrity per shard while deferring pass rate to --merge.",
    )
    parser.add_argument(
        "--merge", nargs="+", type=Path, metavar="REPORT",
        help="Merge and verify a complete set of sharded RunCMake JSON reports into --json/--out.",
    )
    parser.add_argument(
        "--expected-shards", type=int, metavar="N",
        help="With --merge, require exactly N shards.",
    )
    parser.add_argument(
        "--require-provenance", action="store_true",
        help="With --merge, require tested_binary_sha256 and CI run provenance on every shard.",
    )
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
    if args.jobs < 1:
        parser.error("--jobs must be at least 1.")
    if any(not re.fullmatch(r"CMake_TEST_\w+=.*", value) for value in args.test_var):
        parser.error("--test-var requires CMake_TEST_NAME=VALUE.")
    if args.min_pass_rate is not None and not 0 <= args.min_pass_rate <= 100:
        parser.error("--min-pass-rate must be a percentage from 0 to 100.")
    if args.expected_shards is not None and args.expected_shards < 1:
        parser.error("--expected-shards must be at least 1.")
    if args.defer_pass_rate and not args.shard:
        parser.error("--defer-pass-rate requires --shard K/M.")
    if args.merge and (args.shard or args.suite or args.list):
        parser.error("--merge cannot be combined with --shard, --suite, or --list.")

    if args.merge:
        try:
            reports = [load_strict_json(path) for path in args.merge]
            merged = merge_shard_reports(
                reports,
                expected_shards=args.expected_shards,
                require_provenance=args.require_provenance,
            )
        except (OSError, UnicodeDecodeError, ValueError) as exc:
            print(f"{parser.prog}: {exc}", file=sys.stderr)
            return 2
        merged["min_pass_rate"] = args.min_pass_rate
        if args.json:
            args.json.parent.mkdir(parents=True, exist_ok=True)
            args.json.write_text(json.dumps(merged, indent=2) + "\n", encoding="utf-8")
        totals = merged["totals"]
        rate = merged["pass_rate_percent"]
        print(
            f"{'Overall':<32} {totals['passed']:>6} {totals['failed']:>6} "
            f"{totals['total']:>6} {totals['reference_failed']:>9}"
        )
        print(f"Pass rate: {rate:.2f}%" if rate is not None else "Pass rate unavailable.")
        if rate is None:
            return 2
        if args.min_pass_rate is not None:
            return int(rate < args.min_pass_rate)
        return int(bool(totals["failed"]))

    shard_spec = None
    if args.shard:
        try:
            shard_spec = parse_shard(args.shard)
        except ValueError as exc:
            parser.error(str(exc))

    inventory = {"included": [], "expected_profile_suites": [], "profile_total_suites": 0,
                 "excluded_external": {}, "registration_generators": [],
                 "configured_generators": [], "suite_generators": {}}
    report = {"profile": args.profile, "timeout_seconds": timeout, "min_pass_rate": args.min_pass_rate,
              "rcmake_sha": os.environ.get("GITHUB_SHA"), "oracle_version": "4.5.1",
              "suite_inventory": inventory, "suites": {}}
    provenance = _provenance_from_env()
    if provenance:
        report["provenance"] = provenance
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
        full_suites = list(dict.fromkeys(
            suite for name in (args.suite or ["all"])
            for suite in (group if name == "all" else ALIASES.get(name, (name,)))
        ))
        for suite in full_suites:
            if (not re.fullmatch(r"[A-Za-z0-9_][A-Za-z0-9_.+-]*", suite)
                    or not (source / suite / "RunCMakeTest.cmake").is_file()):
                raise ValueError(f"Unknown upstream RunCMake suite {suite!r}.")
        inventory["expected_profile_suites"] = list(full_suites)
        inventory["profile_total_suites"] = len(full_suites)
        inventory["excluded_external"] = {
            name: "Requires an external tool, package, or test fixture outside the standard profile."
            for name in sorted(EXTERNAL_SUITES - set(full_suites))
            if (source / name / "RunCMakeTest.cmake").is_file()
        }
        if shard_spec is not None:
            shard_index, shard_count = shard_spec
            suites = select_shard_suites(full_suites, shard_index, shard_count)
            report["shard"] = {"index": shard_index, "count": shard_count}
        else:
            suites = full_suites
        inventory["included"] = suites
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
        report["tested_binary_sha256"] = sha256_file(candidate)
        engine_sibling = candidate.with_name("rcmake" + EXE_SUFFIX)
        if engine_sibling.is_file() and engine_sibling != candidate:
            engine_digest = sha256_file(engine_sibling)
            if engine_digest != report["tested_binary_sha256"]:
                report["tested_engine_sha256"] = engine_digest
        evaluate(suites, driver, candidate, source, timeout, args.profile, args.test_var,
                 args.timeout is None, report, jobs=args.jobs, registration_suites=full_suites)
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
            for key in ("passed", "failed", "total", "missing", "reference_failed", "no_reference",
                        "capability_excluded")
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
    issues = [f"{name}:{issue}" for name, row in results.items() if (issue := strict_case_issue(name, row))]
    if issues:
        print(f"RunCMake case issues: {', '.join(issues)}", file=sys.stderr)
        return 1
    if args.defer_pass_rate:
        return 0
    if args.min_pass_rate is not None:
        return int(rate < args.min_pass_rate)
    return int(bool(totals["failed"]))


if __name__ == "__main__":
    raise SystemExit(main())
