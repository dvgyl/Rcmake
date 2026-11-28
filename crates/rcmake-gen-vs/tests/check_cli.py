#!/usr/bin/env python3
"""Check CLI output from a staged Rcmake build with rcmake-model/vs-cross."""

import argparse
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET


WORKSPACE = Path(__file__).resolve().parents[3]
CANON = WORKSPACE / "tools/measure/vs_canon.py"
MODULES = WORKSPACE / "modules/cmake-4.5"
NS = {"m": "http://schemas.microsoft.com/developer/msbuild/2003"}

PROJECT = r"""
cmake_minimum_required(VERSION 4.5)
project(Structure C)
set(CMAKE_CONFIGURATION_TYPES Debug Release)
set(CMAKE_VS_GLOBALS "DefaultLanguage=en-US" "TestValue=initial")
set_property(GLOBAL PROPERTY USE_FOLDERS ON)
set_property(DIRECTORY PROPERTY VS_STARTUP_PROJECT app)

source_group("Code/Main" FILES src/main.c)
source_group("Reset" FILES include/reset.h)
source_group("" FILES include/reset.h)
source_group("Diagnostics" REGULAR_EXPRESSION "object[.]c$")
source_group(TREE "${CMAKE_CURRENT_SOURCE_DIR}/include" PREFIX "API/Public"
             FILES include/extra/api.h)
source_group(TREE "${CMAKE_CURRENT_BINARY_DIR}" PREFIX Generated
             FILES "${CMAKE_CURRENT_BINARY_DIR}/generated.h")

add_library(objects OBJECT src/object.c)
add_library(headers INTERFACE include/extra/api.h)
add_library(unused INTERFACE)
add_executable(excluded EXCLUDE_FROM_ALL src/main.c)
add_executable(app src/main.c include/reset.h "${CMAKE_CURRENT_BINARY_DIR}/generated.h")
target_sources(app PRIVATE "$<$<CONFIG:Debug>:src/debug.c>")
target_link_libraries(app PRIVATE objects headers)
set_source_files_properties(src/main.c PROPERTIES
  COMPILE_DEFINITIONS "$<$<CONFIG:Debug>:SOURCE_DEBUG=1>"
  INCLUDE_DIRECTORIES "$<$<CONFIG:Debug>:$(VCInstallDir)include>"
  COMPILE_OPTIONS "$<$<CONFIG:Debug>:/FI$(ProjectDir)pch.h>")
set_target_properties(app PROPERTIES
  FOLDER "Products/Tools"
  VS_GLOBAL_TestValue "app&tools"
  VS_DEBUGGER_COMMAND "debug app"
  VS_DEBUGGER_COMMAND_ARGUMENTS "--config $<CONFIG>"
  VS_DEBUGGER_ENVIRONMENT "MODE=$<CONFIG>"
  VS_DEBUGGER_WORKING_DIRECTORY "run here"
  VS_DPI_AWARE PerMonitor
  EXCLUDE_FROM_DEFAULT_BUILD_RELEASE ON)
set_target_properties(excluded PROPERTIES VS_DPI_AWARE OFF)

add_custom_command(OUTPUT "${CMAKE_CURRENT_BINARY_DIR}/generated.h"
  COMMAND "${CMAKE_COMMAND}" -E touch "${CMAKE_CURRENT_BINARY_DIR}/generated.h"
  COMMENT "Write generated header" VERBATIM)
add_custom_command(TARGET app PRE_BUILD COMMAND "${CMAKE_COMMAND}" -E echo before)
add_custom_command(TARGET app PRE_LINK COMMAND "${CMAKE_COMMAND}" -E echo link)
add_custom_command(TARGET app POST_BUILD COMMAND "${CMAKE_COMMAND}" -E echo after)
"""


def run(*args):
    result = subprocess.run(args, capture_output=True, text=True, timeout=45)
    if result.returncode:
        raise RuntimeError(f"{args}\n{result.stdout}{result.stderr}")
    return result.stdout


def project(tree, name):
    return ET.parse(tree / f"{name}.vcxproj").getroot()


def value(root, path):
    return root.findtext(path, namespaces=NS)


def item(root, kind, name):
    return next(
        entry
        for entry in root.findall(f"./m:ItemGroup/m:{kind}", NS)
        if entry.get("Include", "").replace("\\", "/").split("/")[-1] == name
    )


def check_projects(tree, version):
    app = project(tree, "app")
    globals_path = "./m:PropertyGroup[@Label='Globals']"
    assert value(app, f"{globals_path}/m:DefaultLanguage") == "en-US"
    assert value(app, f"{globals_path}/m:TestValue") == "app&tools"
    config_groups = app.findall("./m:PropertyGroup[@Label='Configuration']", NS)
    assert len(config_groups) == 2
    for config in config_groups:
        assert value(config, "m:ConfigurationType") == "Application"
        assert value(config, "m:PlatformToolset") == {"17": "v143", "18": "v145"}[version]
        assert config.find("m:TestValue", NS) is None

    for element, expected in {
        "LocalDebuggerCommand": ["debug app"] * 2,
        "LocalDebuggerCommandArguments": ["--config Debug", "--config Release"],
        "LocalDebuggerEnvironment": ["MODE=Debug", "MODE=Release"],
        "LocalDebuggerWorkingDirectory": ["run here"] * 2,
    }.items():
        values = app.findall(f"./m:PropertyGroup/m:{element}", NS)
        assert [entry.text for entry in values] == expected, element
        assert all(config in entry.get("Condition", "")
                   for config, entry in zip(("Debug", "Release"), values)), element

    for name, expected in [("app", "PerMonitorHighDPIAware"), ("excluded", "false")]:
        manifest = project(tree, name).findall(
            "./m:ItemDefinitionGroup/m:Manifest/m:EnableDpiAwareness", NS
        )
        assert [entry.text for entry in manifest] == [expected] * 2
    for event, command in [("PreBuild", "before"), ("PreLink", "link"), ("PostBuild", "after")]:
        commands = app.findall(f"./m:ItemDefinitionGroup/m:{event}Event/m:Command", NS)
        assert len(commands) == 2 and all(command in entry.text for entry in commands)
    assert any("generated.h" in (entry.text or "")
               for entry in app.findall("./m:ItemGroup/m:CustomBuild/m:Outputs", NS))
    for group in app.findall("./m:ItemDefinitionGroup/m:ClCompile", NS):
        assert 'CMAKE_INTDIR="$(Configuration)"' in value(group, "m:PreprocessorDefinitions")
    main = item(app, "ClCompile", "main.c")
    for element, text in [
        ("PreprocessorDefinitions", "SOURCE_DEBUG=1"),
        ("AdditionalIncludeDirectories", "$(VCInstallDir)include"),
        ("ForcedIncludeFiles", "$(ProjectDir)pch.h"),
    ]:
        values = main.findall(f"./m:{element}", NS)
        assert len(values) == 1 and text in values[0].text, element
        assert "Debug|x64" in values[0].get("Condition", ""), element

    debug = item(app, "ClCompile", "debug.c")
    excluded = debug.findall("./m:ExcludedFromBuild", NS)
    assert len(excluded) == 1 and excluded[0].text == "true"
    assert "Release|x64" in excluded[0].get("Condition", "")

    refs = {entry.get("Include") for entry in app.findall("./m:ItemGroup/m:ProjectReference", NS)}
    assert {"objects.vcxproj", "headers.vcxproj"} <= refs
    objects = project(tree, "objects")
    assert item(objects, "ClCompile", "object.c").find("m:ObjectFileName", NS) is not None
    assert objects.find("./m:Target[@DependsOnTargets='ClCompile']", NS) is not None
    assert item(project(tree, "headers"), "None", "api.h") is not None
    assert not (tree / "unused.vcxproj").exists()

    filters = ET.parse(tree / "app.vcxproj.filters").getroot()
    definitions = {entry.get("Include"): value(entry, "m:UniqueIdentifier")
                   for entry in filters.findall("./m:ItemGroup/m:Filter", NS)}
    for name in ("Code", r"Code\Main", "Generated"):
        assert definitions[name], name
    assert "Reset" not in definitions
    assert value(item(filters, "ClCompile", "main.c"), "m:Filter") == r"Code\Main"
    assert item(filters, "ClInclude", "reset.h").find("m:Filter", NS) is None
    assert value(item(filters, "ClInclude", "generated.h"), "m:Filter") == "Generated"
    object_filters = ET.parse(tree / "objects.vcxproj.filters").getroot()
    assert value(item(object_filters, "ClCompile", "object.c"), "m:Filter") == "Diagnostics"
    header_filters = ET.parse(tree / "headers.vcxproj.filters").getroot()
    assert value(item(header_filters, "None", "api.h"), "m:Filter") == r"API\Public\extra"


def check_sln(tree):
    solution = (tree / "Structure.sln").read_text(encoding="utf-8-sig")
    assert solution.lstrip().startswith(
        "Microsoft Visual Studio Solution File, Format Version 12.00\n"
        "# Visual Studio Version 17\n"
    )
    names = re.findall(r'^Project\("\{[^}]+\}"\) = "([^"]+)", "([^"]+)", "(\{[^}]+\})"$',
                       solution, re.MULTILINE)
    assert ('Project("{2150E333-8FDC-42A3-9474-1A3956D46DE8}") '
            '= "Tools", "Products\\Tools",') in solution
    ids = {name: identifier for name, _, identifier in names}
    projects = [name for name, path, _ in names if path.endswith(".vcxproj")]
    assert projects[0] == "app"
    assert "GlobalSection(NestedProjects)" in solution
    assert f'{ids["app"]} = {ids["Tools"]}' in solution
    assert f'{ids["Tools"]} = {ids["Products"]}' in solution
    assert f'{ids["app"]}.Debug|x64.Build.0' in solution
    assert f'{ids["app"]}.Release|x64.Build.0' not in solution
    assert f'{ids["excluded"]}.Debug|x64.Build.0' not in solution
    assert f'{ids["excluded"]}.Release|x64.Build.0' not in solution


def check_slnx(tree):
    solution = ET.parse(tree / "Structure.slnx").getroot()
    assert solution.tag == "Solution"
    assert [entry.get("Name") for entry in solution.findall("./Configurations/BuildType")] == [
        "Debug", "Release"
    ]
    assert solution.find("./Configurations/Platform[@Name='x64']") is not None
    assert solution.find("./Folder[@Name='/Products/']") is not None
    folder = solution.find("./Folder[@Name='/Products/Tools/']")
    app = folder.find("./Project[@Path='app.vcxproj']")
    assert app.get("DefaultStartup") == "true"
    assert len(list(solution.iterfind(".//Project[@DefaultStartup='true']"))) == 1
    assert app.find("./BuildDependency[@Project='objects.vcxproj']") is not None
    assert app.find("./Build[@Solution='Release|*'][@Project='false']") is not None
    assert app.find("./Build[@Solution='Debug|*']") is None
    excluded = solution.find("./Project[@Path='excluded.vcxproj']")
    assert {entry.get("Solution") for entry in excluded.findall("./Build[@Project='false']")} == {
        "Debug|*", "Release|*"
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("rcmake", type=Path, help="Path to the CLI binary")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="rcmake-vs-") as directory:
        root = Path(directory).resolve()
        name = "cmake.exe" if sys.platform == "win32" else "cmake"
        cmake = root / "bin" / name
        cmake.parent.mkdir()
        shutil.copy2(args.rcmake, cmake)
        share = root / "share"
        share.mkdir()
        (share / "cmake-4.5").symlink_to(MODULES, target_is_directory=True)
        source = root / "src"
        for path in ("src/main.c", "src/debug.c", "src/object.c",
                     "include/extra/api.h", "include/reset.h"):
            file = source / path
            file.parent.mkdir(parents=True, exist_ok=True)
            file.touch()
        (source / "CMakeLists.txt").write_text(PROJECT, encoding="utf-8")
        toolchain = root / "toolchain.cmake"
        # Supply compiler metadata for generation on hosts without Visual Studio.
        toolchain.write_text(
            'set(CMAKE_C_COMPILER "${CMAKE_COMMAND}")\n'
            'set(CMAKE_C_COMPILER_ID MSVC)\n'
            'set(CMAKE_C_COMPILER_ID_RUN TRUE)\n'
            'set(CMAKE_C_COMPILER_FORCED TRUE)\n'
            'set(CMAKE_C_COMPILER_WORKS TRUE)\n', encoding="utf-8"
        )
        for version, year, check in [("17", "2022", check_sln), ("18", "2026", check_slnx)]:
            tree = root / f"vs{version}"
            run(str(cmake), "-S", str(source), "-B", str(tree),
                "-G", f"Visual Studio {version} {year}", "-A", "x64",
                f"-DCMAKE_MAKE_PROGRAM={cmake.as_posix()}",
                f"-DCMAKE_TOOLCHAIN_FILE={toolchain.as_posix()}")
            # check_sln validates the VS 17 solution. vs_canon.py uses the old folder type.
            paths = ([str(path) for path in sorted(tree.rglob("*.vcxproj*"))]
                     if version == "17" else [str(tree)])
            summary = run(sys.executable, str(CANON), "--check", *paths).strip()
            print(f"Visual Studio {version} {year}. {summary}")
            check(tree)
            check_projects(tree, version)


if __name__ == "__main__":
    main()
