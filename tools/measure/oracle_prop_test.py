#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Compare generated CMake programs with the CMake 4.5.1 oracle.

Source env/env.sh before use. Each case tests one command family or one
generator-expression family. The JSON report records the seed, outcomes and
paths to reduced cases. Exit 1 means a difference. Exit 2 means a setup error
or a case that did not pass on the oracle.
"""

from __future__ import annotations

import argparse
from collections import Counter
from dataclasses import dataclass, replace
from functools import lru_cache
import hashlib
import json
import os
from pathlib import Path
import random
import re
import shutil
import signal
import subprocess
import sys
import tempfile

ORACLE_VERSION = "4.5.1"
DEFAULT_WORK_DIR = Path("/tmp/rcmake-proptest-work")
SCRIPT = "case.cmake"
CONFIGURE = "CMakeLists.txt"
MINIMUM = "cmake_minimum_required(VERSION 4.5)"
OBSERVE = 'message(STATUS "RESULT=[${result}]")'
C_SOURCE = "int value(void) { return 0; }\n"
MAIN_SOURCE = "int main(void) { return 0; }\n"
NINJA_STEP = re.compile(r"^\[\d+/\d+\] (.+)$")
CONFIGURE_INTERVAL = 24


@dataclass(frozen=True)
class Case:
    """Store a program as removable units of complete CMake commands."""

    family: str
    units: tuple[str, ...]
    configure: bool = False

    @property
    def filename(self) -> str:
        return CONFIGURE if self.configure else SCRIPT

    @property
    def program(self) -> str:
        header = (MINIMUM, "project(PropTest LANGUAGES C)") if self.configure else (MINIMUM,)
        return "\n".join((*header, *self.units)) + "\n"


def run_command(command, cwd: Path, timeout: int, env: dict[str, str]) -> dict:
    """Capture both streams and stop the process group at the timeout."""
    command = [os.fspath(arg) for arg in command]
    with subprocess.Popen(
        command, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        encoding="utf-8", errors="replace", start_new_session=True,
    ) as process:
        try:
            stdout, stderr = process.communicate(timeout=timeout)
            error = None
        except (subprocess.TimeoutExpired, KeyboardInterrupt) as exc:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            if isinstance(exc, KeyboardInterrupt):
                raise
            stdout, stderr = process.communicate(timeout=5)
            error = f"Command timed out after {timeout} seconds."
    return {"returncode": process.returncode, "stdout": stdout, "stderr": stderr,
            "error": error}


def executable(path: str | Path) -> Path:
    """Check the executable and retain the name used to invoke it."""
    candidate = Path(path).expanduser().absolute()
    if not candidate.is_file() or not os.access(candidate, os.X_OK):
        raise ValueError(f"Executable not found: {candidate}")
    return candidate


def write_json(path: Path, value) -> None:
    """Write complete JSON before replacing the destination file."""
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=path.parent,
                                     delete=False) as stream:
        temporary = Path(stream.name)
        try:
            json.dump(value, stream, indent=2, sort_keys=True)
            stream.write("\n")
            stream.close()
            temporary.replace(path)
        finally:
            temporary.unlink(missing_ok=True)


def clean_environment() -> dict[str, str]:
    """Set a stable locale and remove variables that can select the oracle."""
    env = dict(os.environ, LC_ALL="C", NO_COLOR="1", CLICOLOR_FORCE="0",
               CMAKE_COLOR_DIAGNOSTICS="OFF")
    for key in tuple(env):
        if key.startswith("RCMAKE_ORACLE_") or key in {
            "RCMAKE_DIAGNOSTICS", "RCMAKE_EXEC_LOG", "RCMAKE_PROFILE", "RCMAKE_ORACLE_MODE",
        }:
            del env[key]
    return env


def script_cases(rng: random.Random) -> dict[str, tuple[str, ...]]:
    """Make valid script programs with seed-dependent values and options."""
    label = rng.choice(("cat", "dog", "fox"))
    number = rng.randint(1, 99)
    step = rng.randint(1, 4)
    words = rng.sample(["X1", "x2", "X10", "x20", "Ye", "yf", "Yg", "yh"], 4)
    items = ";".join(words)
    order = rng.choice(("ASCENDING", "DESCENDING"))
    sort_mode = rng.choice(("STRING", "NATURAL", "FILE_BASENAME"))
    sort_case = rng.choice(("SENSITIVE", "INSENSITIVE"))
    pattern = rng.choice(("^[Xx]", "^[Yy]", "^[A-Z]"))
    filter_mode = rng.choice(("INCLUDE", "EXCLUDE"))
    transform = rng.choice(("TOUPPER", "TOLOWER", "APPEND _tag", 'REPLACE "." q'))
    pop = rng.choice(("POP_FRONT", "POP_BACK"))
    width = rng.randint(1, 3)
    output_format = rng.choice(("DECIMAL", "HEXADECIMAL"))
    relation = rng.choice(("LESS", "GREATER", "EQUAL", "NOTEQUAL"))
    expression = rng.choice(("$<CONFIG:Debug>", "$<BOOL:1>", "$<IF:1,left,right>"))
    return {
        "if.auto_deref": (
            f"set(value {label})", "set(indirect value)",
            f'if(indirect STREQUAL value)\n  set(result equal)\n'
            f'elseif(${{indirect}} STREQUAL "{label}")\n  set(result dereferenced)\n'
            "else()\n  set(result other)\nendif()",
        ),
        "if.matches": (
            f'set(value "{label}{number}")',
            f'if(value MATCHES "^({label})([0-9]{"+" if step > 1 else ""})$")\n'
            '  set(result "${CMAKE_MATCH_1}|${CMAKE_MATCH_2}")\n'
            "elseif(value)\n  set(result truthy)\nelse()\n  set(result false)\nendif()",
        ),
        "string.regex_matchall": (
            f'string(REGEX MATCHALL "[a-z]+|[0-9]+" result "{label}{number}-{label}")',
        ),
        "string.regex_replace": (
            f'string(REGEX REPLACE "([a-z]+)([0-9]+)" "\\\\2_\\\\1" result "{label}{number}")',
        ),
        "string.replace": (f'string(REPLACE "{label}" "{number}" result "{label}_{label}")',),
        "string.substring": (f'string(SUBSTRING "{label}{number}" {step - 1} {width} result)',),
        "string.json": (
            f'set(document [=[{{"z":{number},"a":[true,"{label}"]}}]=])',
            f'string(JSON changed SET "${{document}}" a 0 {number + step})',
            'string(JSON value GET "${changed}" a 0)',
            'string(JSON names MEMBER "${changed}" 0)',
            'set(result "${value}|${names}|${changed}")',
        ),
        "string.append": (f"set(result {label})", f'string(APPEND result ":" "{number}" "")'),
        "string.compare": (f'string(COMPARE {relation} "{number}" "{number + step}" result)',),
        "string.configure": (
            f"set(a {label})", f"set(b {number})",
            f'string(CONFIGURE [=[@a@:${{b}}:"@a@"]=] result '
            f'{rng.choice(("ESCAPE_QUOTES", "@ONLY", "@ONLY ESCAPE_QUOTES"))})',
        ),
        "string.genex_strip": (f'string(GENEX_STRIP "before:{expression}:{label}" result)',),
        "list.sort": (f'set(result "{items}")',
                      f"list(SORT result COMPARE {sort_mode} CASE {sort_case} ORDER {order})"),
        "list.filter": (f'set(result "{items}")', f'list(FILTER result {filter_mode} REGEX "{pattern}")'),
        "list.transform": (
            f'set(values "{items}")',
            f"list(TRANSFORM values {transform} AT 0 -{width} OUTPUT_VARIABLE result)",
        ),
        "list.sublist": (f'set(values "{items}")',
                         f"list(SUBLIST values {width - 1} {rng.choice((-1, 0, 2, 5))} result)"),
        "list.pop": (f'set(values "{items}")', f"list({pop} values a b)",
                     'set(result "${a}|${b}|${values}")'),
        "list.join": (f'set(values "{items};")', f'list(JOIN values "{label}" result)'),
        "math.expr": (
            f'math(EXPR result "({number} << {step}) ^ (~{step} + {width})" OUTPUT_FORMAT {output_format})',
        ),
        "cmake_path": (
            'set(path "a/./b/../file.tar.gz")',
            f'cmake_path(APPEND path "{label}")',
            "cmake_path(NORMAL_PATH path OUTPUT_VARIABLE normalized)",
            'cmake_path(RELATIVE_PATH normalized BASE_DIRECTORY "a" OUTPUT_VARIABLE result)',
        ),
        "file.read_write": (
            f'file(WRITE data.txt "{label}={number}\\n")',
            f"file(READ data.txt result OFFSET {step - 1} LIMIT {width} {rng.choice(('HEX', ''))})",
        ),
        "file.strings": (
            f'file(WRITE data.txt "{label}={number}\\r\\n;{label}\\n{number}\\n")',
            f'file(STRINGS data.txt result REGEX "{label}" LIMIT_COUNT {width})',
        ),
        "file.glob": (
            f'file(WRITE "data/{label}{number}.txt" "")',
            'file(WRITE "data/.hidden.txt" "")',
            'file(WRITE "data/skip.bin" "")',
            'file(GLOB result RELATIVE "${CMAKE_CURRENT_BINARY_DIR}" "data/*.txt")',
        ),
        "file.relative_path": (
            f'file(RELATIVE_PATH result "${{CMAKE_CURRENT_BINARY_DIR}}/{label}" '
            '"${CMAKE_CURRENT_BINARY_DIR}/nested/file")',
        ),
        "file.real_path": (
            f'file(MAKE_DIRECTORY "data/{label}/nested")',
            f'file(REAL_PATH "{label}/nested/../." result BASE_DIRECTORY "data")',
        ),
        "scope.function_macro": (
            f"set(value {label})",
            'macro(add_suffix arg)\n  string(APPEND value ":${arg}")\nendmacro()',
            'function(produce arg)\n  add_suffix("${arg}")\n'
            '  set(result "${value}" PARENT_SCOPE)\nendfunction()',
            f"produce({number})",
        ),
        "scope.block": (
            f"set(a {label})",
            f"block(SCOPE_FOR VARIABLES PROPAGATE a b)\n  set(a {number})\n"
            f"  set(b {step})\nendblock()",
            'set(result "${a}|${b}")',
        ),
        "foreach.zip_lists": (
            f'set(left "{items}")', f'set(right "{label};{number}")',
            f'foreach({rng.choice(("item", "item_0 item_1"))} IN ZIP_LISTS left right)\n'
            '  string(APPEND result "[${item_0}|${item_1}]")\nendforeach()',
        ),
        "foreach.range": (
            f'foreach(i RANGE {width} {width + number % 9} {step})\n'
            '  string(APPEND result "[${i}]")\nendforeach()',
        ),
        "foreach.in_lists": (
            f'set(a "{items};;")', f'set(b "{label};{number}")',
            'foreach(i IN LISTS a missing b)\n  string(APPEND result "[${i}]")\nendforeach()',
        ),
    }


def configure_cases(rng: random.Random) -> dict[str, tuple[str, ...]]:
    """Make target usage requirements that can change Ninja build commands."""
    flag = rng.randint(0, 1)
    config = rng.choice(("Debug", "Release"))
    token = f"TAG_{rng.randint(1, 99)}"
    target = rng.choice(("core", "iface"))
    list_op = rng.choice((
        "JOIN,one;two,_", "GET,one;two,1", "LENGTH,one;two", "REMOVE_DUPLICATES,one;one",
    ))
    return {
        "genex.bool_logic": (
            f'target_compile_definitions(app PRIVATE "$<$<AND:$<BOOL:{flag}>,$<OR:0,1>>:{token}>")',
        ),
        "genex.if_config": (
            f'target_compile_options(app PRIVATE "$<IF:$<CONFIG:{config}>,-D{token},-DOTHER>")',
        ),
        "genex.target_property": (
            f'set_property(TARGET {target} PROPERTY PROP_TAG {token})',
            f'target_compile_definitions(app PRIVATE "$<TARGET_PROPERTY:{target},PROP_TAG>")',
        ),
        "genex.link_only": (
            f"target_compile_definitions(core INTERFACE {token})",
            f'target_link_libraries(iface INTERFACE "$<LINK_ONLY:$<$<BOOL:{flag}>:core_alias>>")',
        ),
        "genex.build_interface": (
            f'target_include_directories(iface INTERFACE "$<BUILD_INTERFACE:${{CMAKE_CURRENT_SOURCE_DIR}}/{token}>")',
        ),
        "genex.list": (
            f'target_compile_definitions(app PRIVATE "{token}=$<LIST:{list_op}>")',
        ),
    }


PROJECT = (
    "add_library(core STATIC value.c)",
    "add_library(core_alias ALIAS core)",
    "add_library(shared SHARED value.c)",
    "add_library(objects OBJECT value.c)",
    "add_library(iface INTERFACE)",
    "add_executable(app main.c)",
    "target_link_libraries(app PRIVATE iface shared objects)",
)


def generate_cases(seed: int, count: int):
    """Cycle through shuffled script and configure families separately."""
    rng = random.Random(seed)
    scripts = list(script_cases(rng))
    configures = list(configure_cases(rng))
    positions = {False: 0, True: 0}
    for index in range(count):
        configure = index % CONFIGURE_INTERVAL == 0
        families = configures if configure else scripts
        position = positions[configure] % len(families)
        if position == 0:
            rng.shuffle(families)
        family = families[position]
        positions[configure] += 1
        case_seed = rng.getrandbits(64)
        values = random.Random(case_seed)
        if configure:
            case = Case(family, (*PROJECT, *configure_cases(values)[family]), configure=True)
        else:
            case = Case(family, (*script_cases(values)[family], OBSERVE))
        yield index + 1, case_seed, case


def normalize(value: str, directory: Path, binary: Path) -> str:
    """Normalize temporary paths, tool paths and trailing whitespace."""
    roots = {str(path) for path in (directory, directory.resolve())}
    tools = {str(path) for path in (binary, binary.resolve())}
    for path in sorted(roots | tools, key=len, reverse=True):
        value = value.replace(path, "<cmake>" if path in tools else "<work>")
    return "\n".join(line.rstrip() for line in value.strip("\n").splitlines())


def command_diagnostics(result: dict, directory: Path, binary: Path) -> dict:
    """Keep diagnostics and drop configure status lines with compiler timings."""
    stdout = "\n".join(line for line in result["stdout"].splitlines() if not line.startswith("-- "))
    return {field: normalize(value, directory, binary) for field, value in (
        ("stdout", stdout), ("stderr", result["stderr"]),
    )}


def write_case(case: Case, directory: Path) -> None:
    """Write the program and its input files."""
    (directory / case.filename).write_text(case.program, encoding="utf-8")
    if case.configure:
        (directory / "value.c").write_text(C_SOURCE, encoding="utf-8")
        (directory / "main.c").write_text(MAIN_SOURCE, encoding="utf-8")


def evaluate(case: Case, binary: Path, ninja: Path, directory: Path,
             timeout: int, env: dict[str, str]) -> dict:
    """Run a case in a clean directory and collect the comparison fields."""
    directory.mkdir()
    try:
        write_case(case, directory)
        if not case.configure:
            result = run_command([binary, "-P", SCRIPT], directory, timeout, env)
            for stream in ("stdout", "stderr"):
                result[stream] = normalize(result[stream], directory, binary)
            result["files"] = {
                path.relative_to(directory).as_posix(): path.read_bytes().hex()
                for path in sorted(directory.rglob("*")) if path.is_file() and path != directory / SCRIPT
            }
            return result

        result = run_command(
            [binary, "-S", ".", "-B", "build", "-G", "Ninja",
             "-DCMAKE_BUILD_TYPE=Debug", f"-DCMAKE_MAKE_PROGRAM={ninja}"],
            directory, timeout, env,
        )
        outcome = {"returncode": result["returncode"], "error": result["error"],
                   "diagnostics": command_diagnostics(result, directory, binary)}
        if not result["returncode"]:
            dry = run_command([ninja, "-n", "-v"], directory / "build", timeout, env)
            lines = normalize(dry["stdout"], directory, binary).splitlines()
            commands = []
            other = []
            for line in lines:
                match = NINJA_STEP.fullmatch(line)
                if match:
                    commands.append(match[1])
                else:
                    other.append(line)
            outcome["dry_run"] = {
                "returncode": dry["returncode"], "error": dry["error"],
                "commands": sorted(commands), "stdout": other,
                "stderr": normalize(dry["stderr"], directory, binary),
            }
        return outcome
    finally:
        shutil.rmtree(directory)


def succeeded(outcome: dict) -> bool:
    """Check whether the requested steps ran successfully."""
    dry = outcome.get("dry_run", outcome)
    return (outcome["returncode"] == dry["returncode"] == 0
            and not outcome["error"] and not dry["error"])


def differences(oracle: dict, rcmake: dict) -> tuple[str, ...]:
    """List the fields that differ between engines."""
    return tuple(key for key in sorted(oracle.keys() | rcmake.keys())
                 if oracle.get(key) != rcmake.get(key))


def minimize(case: Case, compare, oracle: dict, rcmake: dict) -> tuple[Case, dict, dict]:
    """Remove command groups while the oracle succeeds and the difference persists."""
    wanted = differences(oracle, rcmake)
    rcmake_exit = (rcmake["returncode"], rcmake["error"])
    width = 2
    while case.units:
        chunk = (len(case.units) + width - 1) // width
        for start in range(0, len(case.units), chunk):
            units = case.units[:start] + case.units[start + chunk:]
            candidate = replace(case, units=units)
            left, right = compare(candidate)
            if (succeeded(left) and differences(left, right) == wanted
                    and (right["returncode"], right["error"]) == rcmake_exit):
                case, oracle, rcmake = candidate, left, right
                width = max(2, width - 1)
                break
        else:
            if width >= len(case.units):
                break
            width = min(width * 2, len(case.units))
    return case, oracle, rcmake


def save_repro(root: Path, case: Case, oracle: dict, rcmake: dict) -> Path:
    """Save a reduced program under a content-based directory name."""
    digest = hashlib.sha256(case.program.encode()).hexdigest()[:16]
    directory = root / f"{case.family}-{digest}"
    directory.mkdir(parents=True, exist_ok=True)
    write_case(case, directory)
    write_json(directory / "outcomes.json", {"oracle": oracle, "rcmake": rcmake})
    return directory


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rcmake-bin", required=True, type=Path, help="Path to staged Rcmake.")
    parser.add_argument("--cases", type=int, default=1000, help="Number of cases to run.")
    parser.add_argument("--seed", type=int, default=42, help="Seed for program generation.")
    parser.add_argument("--out", type=Path, required=True, help="Path to the JSON report.")
    parser.add_argument("--work-dir", type=Path, default=DEFAULT_WORK_DIR, help="Test workspace.")
    parser.add_argument("--timeout", type=int, default=20, help="Timeout in seconds per command.")
    args = parser.parse_args()
    if args.cases < 1 or args.timeout < 1:
        parser.error("--cases and --timeout must be positive.")

    try:
        rcmake_bin = executable(args.rcmake_bin)
        oracle_bin = executable(os.environ["RCMAKE_ORACLE_CMAKE"])
        ninja = executable(shutil.which("ninja") or "ninja")
        env = clean_environment()
        env["RCMAKE_ORACLE_MODE"] = "1"
        args.work_dir.mkdir(parents=True, exist_ok=True)
        repro_root = args.out.with_suffix(".repros").absolute()
        counts = Counter({"pass": 0, "fail": 0, "oracle_error": 0})
        records = []
        with tempfile.TemporaryDirectory(prefix="oracle-prop-", dir=args.work_dir) as work:
            work = Path(work).resolve()
            version = run_command([oracle_bin, "--version"], work, args.timeout, env)
            if (version["returncode"] != 0 or version["error"]
                    or version["stdout"].splitlines()[:1] != [f"cmake version {ORACLE_VERSION}"]):
                raise ValueError(f"Expected CMake {ORACLE_VERSION} at {oracle_bin}.")

            @lru_cache(maxsize=256)
            def cached_compare(configure, units):
                case = Case("", units, configure)
                return tuple(evaluate(case, binary, ninja, work / "case", args.timeout, env)
                             for binary in (oracle_bin, rcmake_bin))

            def compare(case):
                return cached_compare(case.configure, case.units)

            for index, case_seed, case in generate_cases(args.seed, args.cases):
                oracle, rcmake = compare(case)
                fields = differences(oracle, rcmake)
                row = {"index": index, "case_seed": case_seed, "family": case.family,
                       "mode": "configure" if case.configure else "script"}
                status = "pass"
                if not succeeded(oracle):
                    status = "oracle_error"
                elif fields:
                    status = "fail"
                    reduced, left, right = minimize(case, compare, oracle, rcmake)
                    if reduced != case:
                        row["original"] = str(save_repro(repro_root / "original", case, oracle, rcmake))
                    case, oracle, rcmake = reduced, left, right
                row["status"] = status
                if status != "pass":
                    row["differences"] = fields
                    row["repro"] = str(save_repro(repro_root, case, oracle, rcmake))
                counts[status] += 1
                records.append(row)
                if index % 100 == 0 or index == args.cases:
                    print(f"{index}/{args.cases} cases. {dict(counts)}", flush=True)
        write_json(args.out, {
            "seed": args.seed, "total": args.cases, "oracle_bin": str(oracle_bin),
            "rcmake_bin": str(rcmake_bin), "ninja_bin": str(ninja), "counts": counts,
            "cases": records,
        })
    except (KeyError, OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"Could not complete the comparison. {error}", file=sys.stderr)
        return 2
    if counts["oracle_error"]:
        return 2
    return int(counts["fail"] != 0)


if __name__ == "__main__":
    raise SystemExit(main())
