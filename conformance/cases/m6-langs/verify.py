#!/usr/bin/env python3
"""Build and check the language activation fixtures."""

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time


ROOT = Path(__file__).resolve().parent
EXE = ".exe" if os.name == "nt" else ""


def run(*command):
    result = subprocess.run(command, text=True, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, timeout=120)
    print(result.stdout, end="", flush=True)
    result.check_returncode()
    return result.stdout


def prepare(work, case, cmake, generator, *options):
    src, build = work / "source", work / "build"
    shutil.copytree(ROOT / case, src)
    run(cmake, "-G", generator, "-S", src, "-B", build, *options)
    return src, build


def compile_project(cmake, build):
    run(cmake, "--build", build, "--parallel", "2", "--verbose")


def check_noop(cmake, build, output):
    before = output.stat().st_mtime_ns
    compile_project(cmake, build)
    assert before == output.stat().st_mtime_ns, f"An unchanged build relinked {output}."


def cross(work, cmake, generator):
    staging = work / "stage"
    override = work / "override"
    runtime = []
    if sys.platform == "darwin" and shutil.which("otool"):
        lib = staging / "lib" / "libstage_dependency.dylib"
        lib.parent.mkdir(parents=True)
        source = work / "staged.c"
        source.write_text("int stage_dependency(void) { return 0; }\n", encoding="utf-8")
        run("cc", "-dynamiclib", "-install_name", f"@rpath/{lib.name}", source, "-o", lib)
        runtime.append(f"-DPROBE_RUNTIME_LIBRARY={lib}")
    src, build = prepare(work, "cross", cmake, generator,
                         f"-DCMAKE_STAGING_PREFIX={staging}",
                         f"-DCMAKE_INSTALL_PREFIX={work / 'logical'}", *runtime)
    app = build / f"app{EXE}"
    compile_project(cmake, build)
    check_noop(cmake, build, app)
    for dependency in ("direct.in", "interface.in"):
        before = app.stat().st_mtime_ns
        # The system Make can compare timestamps with one-second precision.
        time.sleep(1.05)
        with (src / dependency).open("a", encoding="utf-8") as stream:
            stream.write("Dependency update\n")
        compile_project(cmake, build)
        assert app.stat().st_mtime_ns > before, f"An edit to {dependency} must relink app."
    run(cmake, "--install", build)
    run(cmake, "--install", build, "--prefix", override)
    for prefix in (staging, override):
        installed = prefix / "bin" / f"app{EXE}"
        assert installed.is_file()
        if runtime:
            commands = run("otool", "-l", installed)
            assert f"path {staging / 'explicit'} " in commands
            assert f"path {work / 'logical/lib'} " in commands
            assert f"path {staging / 'lib'} " not in commands


def fortran(work, cmake, ctest, generator, compiler):
    option = f"-DCMAKE_Fortran_COMPILER={compiler}" if compiler else "-DMOCK_FORTRAN=ON"
    src, build = prepare(work, "fortran", cmake, generator, option,
                         "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON")
    compile_project(cmake, build)
    app = build / f"app{EXE}"
    run(ctest, "--test-dir", build, "--output-on-failure")
    check_noop(cmake, build, app)
    modules = list((build / "modules").glob("provider.mod"))
    assert modules, "The provider module output is missing."
    database = json.loads((build / "compile_commands.json").read_text(encoding="utf-8"))
    assert {Path(entry["file"]).resolve() for entry in database} >= {
        src / "main.f90", src / "provider.f90"
    }
    run(cmake, "--build", build, "--target", "clean")
    assert not any(module.exists() for module in modules)


def autogen(work, cmake, ctest, qt, generator, defines):
    src, build = prepare(work, "autogen", cmake, generator,
                         f"-DCMAKE_PREFIX_PATH={qt}", *(f"-D{item}" for item in defines))
    compile_project(cmake, build)
    app = build / f"autogen_app{EXE}"
    run(ctest, "--test-dir", build, "--output-on-failure")
    check_noop(cmake, build, app)
    checks = [
        ("widget.h", "    Q_OBJECT", '    Q_OBJECT\n    Q_CLASSINFO("Wave", "Moc update")',
         "**/moc_widget.cpp", "Moc update"),
        ("panel.ui", "AUTOUIC", "Uic update", "**/ui_panel.h", "Uic update"),
        ("message.txt", "AUTORCC resource", "AUTORCC resource updated",
         "**/qrc_icons.cpp", None),
    ]
    for name, before, after, pattern, expected in checks:
        current = {path: path.stat().st_mtime_ns for path in build.glob(pattern)}
        assert current, f"Missing generated output {pattern}."
        path = src / name
        time.sleep(1.05)
        path.write_text(path.read_text(encoding="utf-8").replace(before, after), encoding="utf-8")
        compile_project(cmake, build)
        assert any(path.stat().st_mtime_ns > stamp for path, stamp in current.items())
        if expected:
            assert any(expected in path.read_text(encoding="utf-8") for path in current)
        check_noop(cmake, build, app)
    run(ctest, "--test-dir", build, "--output-on-failure")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("cmake", type=Path)
    parser.add_argument("--qt", type=Path)
    parser.add_argument("--define", action="append", default=[])
    parser.add_argument("--fortran")
    parser.add_argument("--case", choices=("cross", "fortran", "autogen"), required=True)
    parser.add_argument("--generator", choices=("Ninja", "Unix Makefiles"), required=True)
    args = parser.parse_args()
    cmake = args.cmake.absolute()
    ctest = cmake.with_name(f"ctest{EXE}")
    with tempfile.TemporaryDirectory(prefix="rcmake-m6-langs-") as temporary:
        work = Path(temporary).resolve()
        if args.case == "cross":
            cross(work, cmake, args.generator)
        elif args.case == "fortran":
            fortran(work, cmake, ctest, args.generator, args.fortran)
        else:
            if not args.qt:
                parser.error("--qt is required for AUTOGEN.")
            autogen(work, cmake, ctest, args.qt, args.generator, args.define)


if __name__ == "__main__":
    main()
