# SPDX-License-Identifier: MIT OR Apache-2.0
"""Core configure, search, policy, File API, and determinism scenarios."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
import platform
import re
import shutil
from typing import Any, Mapping

from . import context
from .context import (
    Ctx,
    cache_entries,
    cmake_errors,
    collapse_repeats,
    diff_observations,
    elf_dynamic,
    file_listing,
    generators_available,
    macho_load_dylibs,
    macho_rpaths,
    message_lines,
    scenario,
    shared_names,
    slug,
    touch_later,
)
from .sandbox import ScenarioSkip, StepFailed


DIAMOND_LEVELS = 20
DIAMOND_WIDTH = ("a", "b", "c")


@scenario("s05", "Deep diamond $<TARGET_PROPERTY:...,INCLUDE_DIRECTORIES> scaling (3x20 targets, 3^20 paths)")
def s05_diamond(ctx: Ctx) -> None:
    src = ctx.root / "src"
    ctx.write("src/CMakeLists.txt", f"""
        cmake_minimum_required(VERSION 4.0)
        project(diamond C)
        set(prev "")
        foreach(level RANGE 1 {DIAMOND_LEVELS})
          set(cur "")
          foreach(k {' '.join(DIAMOND_WIDTH)})
            add_library(L${{level}}${{k}} INTERFACE)
            target_include_directories(L${{level}}${{k}} INTERFACE "${{CMAKE_SOURCE_DIR}}/inc/L${{level}}${{k}}")
            target_compile_definitions(L${{level}}${{k}} INTERFACE "D_L${{level}}${{k}}=${{level}}")
            if(prev)
              target_link_libraries(L${{level}}${{k}} INTERFACE ${{prev}})
            endif()
            list(APPEND cur L${{level}}${{k}})
          endforeach()
          set(prev ${{cur}})
        endforeach()
        add_library(top INTERFACE)
        target_link_libraries(top INTERFACE ${{prev}})
        add_executable(app main.c)
        target_link_libraries(app PRIVATE top)
        file(GENERATE OUTPUT incs.txt CONTENT "$<TARGET_PROPERTY:app,INCLUDE_DIRECTORIES>\\n")
        file(GENERATE OUTPUT defs.txt CONTENT "$<TARGET_PROPERTY:app,COMPILE_DEFINITIONS>\\n")
        file(GENERATE OUTPUT iface.txt CONTENT "$<TARGET_PROPERTY:top,INTERFACE_INCLUDE_DIRECTORIES>\\n")
        """)
    for level in range(1, DIAMOND_LEVELS + 1):
        for k in DIAMOND_WIDTH:
            ctx.write(f"src/inc/L{level}{k}/L{level}{k}.h", f"#define VALUE_L{level}{k} {level}\n")
    ctx.write("src/main.c", """
        #include <stdio.h>
        #include "L1a.h"
        #include "L20c.h"
        int main(void) {
          printf("diamond %d %d %d\\n", VALUE_L1a + VALUE_L20c, D_L1b, D_L20a);
          return 0;
        }
        """)
    build = ctx.root / "build"
    cold = ctx.configure(src, build, "Ninja")
    ctx.ok("cold configure", cold)
    ctx.check("cold configure < 3.0s", cold.seconds < 3.0, f"{cold.seconds:.2f}s")
    warm = ctx.cmake(build)
    ctx.ok("warm reconfigure", warm)
    ctx.check("warm reconfigure < 3.0s", warm.seconds < 3.0, f"{warm.seconds:.2f}s")
    expected = {f"{src}/inc/L{level}{k}" for level in range(1, DIAMOND_LEVELS + 1) for k in DIAMOND_WIDTH}
    for name in ("incs.txt", "iface.txt"):
        items = (build / name).read_text().strip().split(";")
        ctx.check(f"{name} lists each of the 60 directories once",
                  len(items) == 60 and set(items) == expected, f"{len(items)} items")
        ctx.observe(name, (build / name).read_text())
    defs = (build / "defs.txt").read_text().strip().split(";")
    ctx.check("defs.txt lists 60 definitions", len(set(defs)) == 60 == len(defs), len(defs))
    ctx.observe("defs.txt", (build / "defs.txt").read_text())
    ctx.ok("build", ctx.build(build, "Ninja"))
    run = ctx.run([build / "app"])
    ctx.check("app output", run.ok and run.stdout == "diamond 21 1 20\n", run.tail())
    ctx.observe("app", run.stdout)


@scenario("s06", "find_library lib64 + /* wildcard HINTS/PATHS + FeatureSummary + find_package literal ENV")
def s06_find(ctx: Ctx) -> None:
    r = ctx.root
    for rel in ("pfx/p64/lib64/libsixty.a", "pfx/plib/lib/libplain.a",
                "opt/vendorB-2.0/lib/libwild.a", "opt/vendorA-1.0/lib/libwildhint.a"):
        ctx.write(rel, "!<arch>\n")
    ctx.write("opt/vendorB-2.0/lib/cmake/Wid/WidConfig.cmake", "set(Wid_MARK wildcard)\n")
    ctx.write("envroot/lib/cmake/Gadget/GadgetConfig.cmake", "set(Gadget_MARK expanded-env)\n")
    ctx.write("src/ENV/GadgetConfig.cmake", "set(Gadget_MARK literal-env)\n")
    ctx.write("env dir/share/Thing/ThingConfig.cmake", "set(Thing_MARK spaced)\n")
    variables = ("SIXTY_LIB", "SIXTY_PFX", "SIXTY_OFF", "PLAIN_LIB", "WILD_LIB", "WILDHINT_LIB",
                 "WILDTOP_LIB", "Gadget_DIR", "Thing_DIR", "Thing2_DIR", "Missing_DIR", "Wid_DIR",
                 "Gadget_MARK", "Thing_MARK", "Wid_MARK", "Gadget_FOUND", "Thing_FOUND")
    ctx.write("src/CMakeLists.txt", f"""
        cmake_minimum_required(VERSION 4.0)
        project(finds C)
        set_property(GLOBAL PROPERTY FIND_LIBRARY_USE_LIB64_PATHS TRUE)
        find_library(SIXTY_LIB sixty PATHS "${{ROOTX}}/pfx/p64/lib" NO_DEFAULT_PATH)
        find_library(SIXTY_PFX sixty)
        find_library(PLAIN_LIB plain PATHS "${{ROOTX}}/pfx/plib/lib" NO_DEFAULT_PATH)
        set_property(GLOBAL PROPERTY FIND_LIBRARY_USE_LIB64_PATHS FALSE)
        find_library(SIXTY_OFF sixty PATHS "${{ROOTX}}/pfx/p64/lib" NO_DEFAULT_PATH)
        find_library(WILD_LIB wild PATHS "${{ROOTX}}/opt/*/lib" NO_DEFAULT_PATH)
        find_library(WILDHINT_LIB wildhint HINTS "${{ROOTX}}/opt/*/lib" NO_DEFAULT_PATH)
        find_library(WILDTOP_LIB wild HINTS "${{ROOTX}}/opt/*" PATH_SUFFIXES lib NO_DEFAULT_PATH)
        # In find_package, ENV is not a keyword. It is the relative path "ENV".
        find_package(Gadget CONFIG PATHS ENV GADGET_PREFIX NO_DEFAULT_PATH)
        find_package(Thing CONFIG HINTS ENV THING_PREFIX NO_DEFAULT_PATH)
        find_package(Thing2 NAMES Thing CONFIG HINTS "$ENV{{THING_PREFIX}}" NO_DEFAULT_PATH)
        find_package(Missing CONFIG QUIET PATHS ENV NOT_SET_VAR NO_DEFAULT_PATH)
        find_package(Wid CONFIG QUIET PATHS "${{ROOTX}}/opt/*" NO_DEFAULT_PATH)
        include(FeatureSummary)
        set_package_properties(Gadget PROPERTIES DESCRIPTION "Gadget library"
          URL "https://example.invalid/gadget" TYPE REQUIRED PURPOSE "Gadget support")
        set_package_properties(Thing PROPERTIES DESCRIPTION "Thing library" TYPE RECOMMENDED)
        set_package_properties(Missing PROPERTIES DESCRIPTION "Missing thing" TYPE OPTIONAL)
        option(WITH_FAST "fast" ON)
        add_feature_info(Fast WITH_FAST "Fast mode")
        add_feature_info(Slow "NOT WITH_FAST" "Slow mode")
        feature_summary(WHAT ALL)
        feature_summary(WHAT ENABLED_FEATURES DISABLED_FEATURES PACKAGES_FOUND PACKAGES_NOT_FOUND
          FILENAME "${{CMAKE_BINARY_DIR}}/features.txt")
        foreach(v {' '.join(variables)})
          message(STATUS "RESULT ${{v}}=${{${{v}}}}")
        endforeach()
        """)
    env = {"GADGET_PREFIX": str(r / "envroot"), "THING_PREFIX": str(r / "env dir")}
    build = r / "build"
    proc = ctx.configure(r / "src", build, "Ninja", f"-DROOTX={r}", f"-DCMAKE_PREFIX_PATH={r}/pfx/p64", env=env)
    ctx.ok("configure", proc)
    results = dict(re.findall(r"(?m)^-- RESULT (\w+)=(.*)$", proc.stdout))
    expect = {
        "SIXTY_LIB": f"{r}/pfx/p64/lib64/libsixty.a",
        "SIXTY_PFX": f"{r}/pfx/p64/lib64/libsixty.a",
        "SIXTY_OFF": "SIXTY_OFF-NOTFOUND",
        "PLAIN_LIB": f"{r}/pfx/plib/lib/libplain.a",
        "WILD_LIB": f"{r}/opt/vendorB-2.0/lib/libwild.a",
        "WILDHINT_LIB": f"{r}/opt/vendorA-1.0/lib/libwildhint.a",
        "WILDTOP_LIB": f"{r}/opt/vendorB-2.0/lib/libwild.a",
        "Gadget_DIR": f"{r}/src/ENV",
        "Gadget_MARK": "literal-env",
        "Thing_DIR": "Thing_DIR-NOTFOUND",
        "Thing2_DIR": f"{r}/env dir/share/Thing",
        "Thing_MARK": "spaced",
        "Missing_DIR": "Missing_DIR-NOTFOUND",
        "Wid_DIR": f"{r}/opt/vendorB-2.0/lib/cmake/Wid",
        "Wid_MARK": "wildcard",
    }
    for key, value in expect.items():
        ctx.check(f"{key} result", results.get(key) == value, f"got {results.get(key)!r}, want {value!r}")
    summary = proc.stdout.split("-- RESULT", 1)[0]
    summary = "\n".join(line for line in summary.splitlines()
                        if not re.match(r"-- (The C compiler|Detecting|Check for working|Found)", line))
    ctx.check("summary lists the REQUIRED package as found",
              "REQUIRED packages have been found:\n\n * Gadget, Gadget library" in summary, summary)
    ctx.check("summary lists the RECOMMENDED package as not found",
              "RECOMMENDED packages have not been found:\n\n * Thing, Thing library" in summary, summary)
    ctx.check("summary lists the disabled feature", " * Slow, Slow mode" in summary, summary)
    ctx.observe("results", results)
    ctx.observe("summary", summary)
    ctx.observe("features.txt", (build / "features.txt").read_text())
    ctx.observe("cache", cache_entries(build, variables + ("WITH_FAST",)))
    cache = cache_entries(build, ("SIXTY_LIB", "Gadget_DIR"))
    ctx.check("cache SIXTY_LIB is FILEPATH", cache.get("SIXTY_LIB") == f"FILEPATH={expect['SIXTY_LIB']}", cache)
    ctx.check("cache Gadget_DIR is PATH", cache.get("Gadget_DIR") == f"PATH={expect['Gadget_DIR']}", cache)


@scenario("s08", "try_compile genex imported deps ($<$<BOOL:1>:Vendor::Dep>) + cross try_run preseeded cache")
def s08_try(ctx: Ctx) -> None:
    src = ctx.tree(ctx.root / "src", {
        "CMakeLists.txt": """
            cmake_minimum_required(VERSION 4.0)
            project(tc C)
            add_library(Vendor::Dep INTERFACE IMPORTED)
            set_target_properties(Vendor::Dep PROPERTIES
              INTERFACE_COMPILE_DEFINITIONS "VENDOR_DEP=7"
              INTERFACE_INCLUDE_DIRECTORIES "${CMAKE_SOURCE_DIR}/vendor")
            add_library(Vendor::Top INTERFACE IMPORTED)
            set_target_properties(Vendor::Top PROPERTIES INTERFACE_LINK_LIBRARIES "$<$<BOOL:1>:Vendor::Dep>")
            add_library(Vendor::TopOff INTERFACE IMPORTED)
            set_target_properties(Vendor::TopOff PROPERTIES INTERFACE_LINK_LIBRARIES "$<$<BOOL:0>:Vendor::Dep>")
            try_compile(TC_GENEX SOURCES ${CMAKE_SOURCE_DIR}/probe.c LINK_LIBRARIES Vendor::Top)
            try_compile(TC_PLAIN SOURCES ${CMAKE_SOURCE_DIR}/probe.c LINK_LIBRARIES Vendor::Dep)
            try_compile(TC_OFF SOURCES ${CMAKE_SOURCE_DIR}/probe.c LINK_LIBRARIES Vendor::TopOff)
            message(STATUS "TRY TC_GENEX=${TC_GENEX} TC_PLAIN=${TC_PLAIN} TC_OFF=${TC_OFF}")
            if(TOP_LEVEL_GENEX)
              # try_compile copies only plain imported target names into the
              # test project, so this target name inside a genex is not found.
              try_compile(TC_TOP SOURCES ${CMAKE_SOURCE_DIR}/probe.c LINK_LIBRARIES "$<$<BOOL:1>:Vendor::Dep>")
            endif()
            """,
        "vendor/vendor.h": "#define VENDOR_HEADER 1\n",
        "probe.c": """
            #include "vendor.h"
            #if VENDOR_DEP != 7
            #error "Vendor::Dep usage requirements are missing"
            #endif
            int main(void) { return VENDOR_HEADER - 1; }
            """,
    })
    proc = ctx.configure(src, ctx.root / "b-tc", "Ninja")
    ctx.ok("try_compile configure", proc)
    line = re.findall(r"(?m)^-- TRY .*$", proc.stdout)
    ctx.check("try_compile results", line == ["-- TRY TC_GENEX=TRUE TC_PLAIN=TRUE TC_OFF=FALSE"], line)
    ctx.observe("try_compile", line)
    top = ctx.configure(src, ctx.root / "b-top", "Ninja", "-DTOP_LEVEL_GENEX=ON")
    ctx.fails("top-level genex in LINK_LIBRARIES is not copied", top)
    missing = "links to:\n\n    Vendor::Dep\n\n  but the target was not found." in top.stderr
    ctx.check("top-level genex error names Vendor::Dep", missing, top.tail())
    ctx.observe("top-level genex", [top.returncode != 0, missing])

    cross = ctx.tree(ctx.root / "cross", {
        "CMakeLists.txt": """
            cmake_minimum_required(VERSION 4.0)
            project(cross C)
            try_run(RUN_RES COMPILE_RES SOURCES ${CMAKE_SOURCE_DIR}/runme.c
              RUN_OUTPUT_VARIABLE run_out ARGS "${CMAKE_BINARY_DIR}/executed.flag")
            message(STATUS "TRYRUN cross=${CMAKE_CROSSCOMPILING} compile=${COMPILE_RES} run=${RUN_RES} out=[${run_out}]")
            """,
        "runme.c": """
            #include <stdio.h>
            int main(int argc, char **argv) {
              if (argc > 1) { FILE *f = fopen(argv[1], "w"); if (f) { fputs("ran\\n", f); fclose(f); } }
              puts("real-output");
              return 3;
            }
            """,
        "toolchain.cmake": f"""
            set(CMAKE_SYSTEM_NAME {platform.system()})
            set(CMAKE_SYSTEM_PROCESSOR {platform.machine()})
            """,
    })
    build = ctx.root / "b-cross"
    seeded = ctx.configure(cross, build, "Ninja", f"-DCMAKE_TOOLCHAIN_FILE={cross}/toolchain.cmake",
                           "-DRUN_RES=42", "-DRUN_RES__TRYRUN_OUTPUT=preseeded-output")
    ctx.ok("cross configure with preseeded cache", seeded)
    line = re.findall(r"(?m)^-- TRYRUN .*$", seeded.stdout)
    ctx.check("preseeded try_run results",
              line == ["-- TRYRUN cross=TRUE compile=TRUE run=42 out=[preseeded-output]"], line)
    ctx.check("preseeded binary is not executed", not (build / "executed.flag").exists())
    ctx.observe("preseeded", line)

    unseeded_build = ctx.root / "b-cross-unseeded"
    unseeded = ctx.configure(cross, unseeded_build, "Ninja", f"-DCMAKE_TOOLCHAIN_FILE={cross}/toolchain.cmake")
    ctx.fails("cross try_run without preseeded cache fails", unseeded)
    ctx.check("TryRunResults.cmake written", (unseeded_build / "TryRunResults.cmake").is_file())
    ctx.check("unseeded binary is not executed", not (unseeded_build / "executed.flag").exists())
    if (unseeded_build / "TryRunResults.cmake").is_file():
        sets = re.findall(r"(?m)^set\(\s*(\S+)", (unseeded_build / "TryRunResults.cmake").read_text())
        ctx.check("TryRunResults.cmake lists RUN_RES", "RUN_RES" in sets and "RUN_RES__TRYRUN_OUTPUT" in sets, sets)
        ctx.observe("TryRunResults variables", sets)
    ctx.observe("unseeded cache", cache_entries(unseeded_build, ["RUN_RES", "RUN_RES__TRYRUN_OUTPUT"]))


def _check_cps_consumer(ctx: Ctx) -> None:
    r = ctx.root
    ctx.write("pfx/include/widget.h", """
        #if __cplusplus < 202002L
        #error "widget needs C++20"
        #endif
        consteval int widget_value() { return 20; }
        """)
    ctx.write("pfx/lib/cps/widget/widget.cps", """
        {
          "cps_version": "0.13.0",
          "name": "widget",
          "version": "2.1.0",
          "compat_version": "2.0.0",
          "cps_path": "@prefix@/lib/cps/widget",
          "default_components": ["core"],
          "components": {
            "core": {
              "type": "interface",
              "compile_features": ["c++20"],
              "definitions": {"*": {"WIDGET_CORE": "1"}},
              "includes": ["@prefix@/include"]
            },
            "extra": {"type": "interface", "requires": [":core"], "compile_features": ["threads"]}
          }
        }
        """)
    ctx.write("cps/CMakeLists.txt", """
        cmake_minimum_required(VERSION 4.0)
        project(cpsuse CXX)
        find_package(widget 2.0 REQUIRED)
        foreach(t widget::core widget::extra)
          foreach(p INTERFACE_COMPILE_FEATURES INTERFACE_COMPILE_DEFINITIONS INTERFACE_LINK_LIBRARIES)
            get_target_property(v ${t} ${p})
            message(STATUS "PROP ${t} ${p}=${v}")
          endforeach()
        endforeach()
        message(STATUS "PROP widget_VERSION=${widget_VERSION}")
        add_executable(use use.cpp)
        target_link_libraries(use PRIVATE widget::core)
        """)
    ctx.write("cps/use.cpp", """
        #include <cstdio>
        #include "widget.h"
        int main() { std::printf("widget %d %d\\n", widget_value(), WIDGET_CORE); return 0; }
        """)
    build = r / "cps-build"
    proc = ctx.configure(r / "cps", build, "Ninja", f"-DCMAKE_PREFIX_PATH={r}/pfx")
    ctx.ok("CPS configure", proc)
    props = [line for line in proc.stdout.splitlines() if line.startswith("-- PROP")]
    ctx.check("CPS c++20 maps to cxx_std_20",
              "-- PROP widget::core INTERFACE_COMPILE_FEATURES=cxx_std_20" in props, props)
    ctx.check("CPS threads maps to Threads::Threads",
              "-- PROP widget::extra INTERFACE_LINK_LIBRARIES=Threads::Threads;widget::core" in props, props)
    ctx.observe("cps properties", props)
    ctx.ok("CPS consumer build", ctx.build(build, "Ninja"))
    run = ctx.run([build / "use"])
    ctx.check("CPS consumer output", run.stdout == "widget 20 1\n", run.tail())


@scenario("s09", "file(INSTALL) lib64->lib symlink + file:// URL with spaces + RELATIVE_PATH case + CPS cxx_std_20")
def s09_files(ctx: Ctx) -> None:
    r = ctx.root
    ctx.write("tree/lib/libx.txt", "lib payload\n")
    (r / "tree/lib64").symlink_to("lib")
    ctx.write("dl dir/data file.txt", "payload\n")
    sha = hashlib.sha256(b"payload\n").hexdigest()
    ctx.write("t.cmake", f"""
        file(INSTALL "${{R}}/tree/lib" "${{R}}/tree/lib64" DESTINATION "${{R}}/out1")
        file(INSTALL "${{R}}/tree" DESTINATION "${{R}}/out2")
        file(INSTALL FILES "${{R}}/tree/lib64" DESTINATION "${{R}}/out3" TYPE DIRECTORY)
        foreach(d out1/lib64 out2/tree/lib64 out3/lib64 out2/tree/lib/libx.txt)
          if(IS_SYMLINK "${{R}}/${{d}}")
            file(READ_SYMLINK "${{R}}/${{d}}" t)
            message("KIND ${{d}} symlink ${{t}}")
          elseif(IS_DIRECTORY "${{R}}/${{d}}")
            message("KIND ${{d}} directory")
          elseif(EXISTS "${{R}}/${{d}}")
            message("KIND ${{d}} file")
          else()
            message("KIND ${{d}} missing")
          endif()
        endforeach()
        file(DOWNLOAD "file://${{R}}/dl%20dir/data%20file.txt" "${{R}}/got1.txt" STATUS s1
             EXPECTED_HASH SHA256={sha})
        list(GET s1 0 c1)
        message("DL encoded ${{c1}}")
        file(DOWNLOAD "file://${{R}}/dl dir/data file.txt" "${{R}}/got2.txt" STATUS s2)
        list(GET s2 0 c2)
        message("DL raw ${{c2}}")
        file(DOWNLOAD "file://${{R}}/dl%20dir/absent.txt" "${{R}}/got3.txt" STATUS s3)
        list(GET s3 0 c3)
        message("DL absent-nonzero ${{c3}}")
        file(RELATIVE_PATH r1 "/Data/A/B" "/data/a/b/c/d.txt")
        file(RELATIVE_PATH r2 "/Data/A/B" "/Data/A/B/c/d.txt")
        file(RELATIVE_PATH r3 "/x/Y" "/x/y")
        file(RELATIVE_PATH r4 "/x/y/" "/x/y/z/")
        message("REL r1=[${{r1}}] r2=[${{r2}}] r3=[${{r3}}] r4=[${{r4}}]")
        """)
    proc = ctx.cmake(f"-DR={r}", "-P", r / "t.cmake")
    ctx.ok("cmake -P", proc)
    out = proc.stderr
    for d in ("out1/lib64", "out2/tree/lib64", "out3/lib64"):
        ctx.check(f"{d} stays a symlink to lib", f"KIND {d} symlink lib" in out, out)
    ctx.check("installed file content", (r / "out2/tree/lib/libx.txt").read_text() == "lib payload\n")
    ctx.check("file:// %20 download", "DL encoded 0" in out and (r / "got1.txt").read_text() == "payload\n", out)
    ctx.check("file:// raw space download", "DL raw 0" in out and (r / "got2.txt").read_text() == "payload\n", out)
    ctx.check("file:// missing file fails", "DL absent-nonzero 0" not in out, out)
    # macOS (like Windows) compares paths without case in RELATIVE_PATH.
    if context.DARWIN:
        rel = "REL r1=[c/d.txt] r2=[c/d.txt] r3=[] r4=[z]"
    else:
        rel = "REL r1=[../../../data/a/b/c/d.txt] r2=[c/d.txt] r3=[../y] r4=[z]"
    ctx.check("RELATIVE_PATH case rule", rel in out, out)
    installs = sorted(line for line in proc.stdout.splitlines() if line.startswith("-- Installing"))
    ctx.observe("installing", installs)
    ctx.observe("messages", [line for line in out.splitlines() if line.split(" ")[0] in ("KIND", "DL", "REL")])
    ctx.observe("out2", file_listing(r / "out2"))
    _check_cps_consumer(ctx)


DETERMINISM_FILES = {
    "CMakeLists.txt": """
        cmake_minimum_required(VERSION 4.0)
        project(det VERSION 3.1 LANGUAGES C CXX)
        set(CMAKE_EXPORT_COMPILE_COMMANDS ON)
        enable_testing()
        option(DET_FEATURE "feature" ON)
        configure_file(version.h.in ${CMAKE_BINARY_DIR}/version.h @ONLY)
        add_library(detcore STATIC core.c)
        target_include_directories(detcore PUBLIC ${CMAKE_BINARY_DIR} include)
        target_compile_definitions(detcore PUBLIC $<$<BOOL:${DET_FEATURE}>:DET_FEATURE=1> DET_NAME="det")
        add_library(detshared SHARED shared.cpp)
        target_link_libraries(detshared PRIVATE detcore)
        target_compile_features(detshared PRIVATE cxx_std_17)
        add_executable(detapp main.cpp)
        target_link_libraries(detapp PRIVATE detshared detcore)
        add_test(NAME detapp COMMAND detapp)
        set_tests_properties(detapp PROPERTIES PASS_REGULAR_EXPRESSION "^det 3.1 7 1")
        file(GENERATE OUTPUT targets.txt CONTENT
          "core=$<TARGET_FILE_NAME:detcore>\\nshared=$<TARGET_FILE_NAME:detshared>\\nsoname=$<TARGET_SONAME_FILE_NAME:detshared>\\napp=$<TARGET_FILE_NAME:detapp>\\ndefs=$<TARGET_PROPERTY:detapp,COMPILE_DEFINITIONS>\\nincs=$<TARGET_PROPERTY:detapp,INCLUDE_DIRECTORIES>\\nfeatures=$<TARGET_PROPERTY:detshared,COMPILE_FEATURES>\\n")
        get_property(vars DIRECTORY PROPERTY VARIABLES)
        list(FILTER vars INCLUDE REGEX "^(PROJECT_|det_|CMAKE_PROJECT_)")
        list(SORT vars)
        set(dump "")
        foreach(v IN LISTS vars)
          string(APPEND dump "${v}=${${v}}\\n")
        endforeach()
        file(WRITE ${CMAKE_BINARY_DIR}/vars.txt "${dump}")
        """,
    "version.h.in": "#define DET_VERSION \"@PROJECT_VERSION@\"\n#define DET_MAJOR @PROJECT_VERSION_MAJOR@\n",
    "include/det.h": """
        #ifdef __cplusplus
        extern "C" {
        #endif
        int det_core(void);
        int det_shared(void);
        #ifdef __cplusplus
        }
        #endif
        """,
    "core.c": '#include "det.h"\n#include "version.h"\nint det_core(void) { return DET_MAJOR + 4; }\n',
    "shared.cpp": '#include "det.h"\nextern "C" int det_shared(void) { return det_core(); }\n',
    "main.cpp": """
        #include <cstdio>
        #include "det.h"
        #include "version.h"
        int main() { std::printf("det %s %d %d\\n", DET_VERSION, det_shared(), DET_FEATURE); return 0; }
        """,
}


def compile_db(build: Path) -> list[dict[str, str]]:
    """Read compile_commands.json sorted by file."""
    path = build / "compile_commands.json"
    if not path.is_file():
        return []
    entries = json.loads(path.read_text())
    return sorted(
        ({key: entry.get(key, "") for key in ("directory", "file", "output", "command")} for entry in entries),
        key=lambda entry: entry["file"],
    )


def determinism_snapshot(build: Path) -> dict[str, Any]:
    """Collect the configure outputs that must not depend on history."""
    def text(name: str) -> str:
        path = build / name
        return path.read_text() if path.is_file() else "<missing>"
    return {
        "targets.txt": text("targets.txt"),
        "vars.txt": text("vars.txt"),
        "version.h": text("version.h"),
        "compile_commands": compile_db(build),
        "cache": cache_entries(build, ["DET_FEATURE", "CMAKE_BUILD_TYPE", "CMAKE_PROJECT_NAME",
                                       "det_BINARY_DIR", "det_SOURCE_DIR", "det_IS_TOP_LEVEL"]),
    }


@scenario("s14", "Cold vs warm vs oracle determinism (configure outputs, compile database, binaries)")
def s14_determinism(ctx: Ctx) -> None:
    src = ctx.tree(ctx.root / "src", DETERMINISM_FILES)
    gens = [g for g in ("Ninja", "Unix Makefiles") if g in generators_available(ctx.options)]
    for gen in gens:
        tag = slug(gen)
        cold_dir = ctx.root / f"cold-{tag}"
        try:
            ctx.ok(f"{gen}: cold configure", ctx.configure(src, cold_dir, gen))
        except StepFailed:
            continue
        cold = determinism_snapshot(cold_dir)
        ctx.check(f"{gen}: compile database has 3 entries", len(cold["compile_commands"]) == 3, cold["compile_commands"])
        warm_proc = ctx.cmake(cold_dir)
        ctx.ok(f"{gen}: warm reconfigure", warm_proc, required=False)
        warm = determinism_snapshot(cold_dir)
        ctx.check(f"{gen}: warm reconfigure equals cold configure", warm == cold,
                  json.dumps(diff_observations(cold, warm))[:2000])
        again_dir = ctx.root / f"again-{tag}"
        ctx.ok(f"{gen}: second cold configure", ctx.configure(src, again_dir, gen), required=False)
        again = json.loads(json.dumps(determinism_snapshot(again_dir)).replace(f"again-{tag}", f"cold-{tag}"))
        ctx.check(f"{gen}: second cold configure equals the first", again == cold,
                  json.dumps(diff_observations(cold, again))[:2000])
        ctx.ok(f"{gen}: build", ctx.build(cold_dir, gen), required=False)
        run = ctx.run([cold_dir / "detapp"])
        ctx.check(f"{gen}: binary output", run.stdout == "det 3.1 7 1\n", run.tail())
        test = ctx.ctest("--test-dir", cold_dir)
        ctx.check(f"{gen}: ctest", test.ok and "100% tests passed" in test.stdout, test.tail())
        flip = ctx.cmake("-DDET_FEATURE=OFF", cold_dir)
        ctx.ok(f"{gen}: reconfigure with an option change", flip, required=False)
        ctx.check(f"{gen}: option change reaches file(GENERATE)",
                  "DET_FEATURE=1" not in (cold_dir / "targets.txt").read_text())
        back = ctx.cmake("-DDET_FEATURE=ON", cold_dir)
        ctx.ok(f"{gen}: reconfigure back", back, required=False)
        ctx.check(f"{gen}: reconfigure back restores the snapshot", determinism_snapshot(cold_dir) == cold)
        ctx.observe(f"{gen}: snapshot", cold)
        ctx.observe(f"{gen}: output", run.stdout)


FILE_API_PROJECT = {
    "CMakeLists.txt": """
        cmake_minimum_required(VERSION 3.28)
        project(fapi VERSION 1.0 LANGUAGES C CXX)
        add_library(ext SHARED IMPORTED)
        set_target_properties(ext PROPERTIES IMPORTED_LOCATION /nonexistent/libext.so)
        add_library(ext::opts INTERFACE IMPORTED)
        set_target_properties(ext::opts PROPERTIES INTERFACE_COMPILE_DEFINITIONS EXT_OPT=1)
        add_custom_command(OUTPUT ${CMAKE_CURRENT_BINARY_DIR}/gen.c
          COMMAND ${CMAKE_COMMAND} -DOUT=${CMAKE_CURRENT_BINARY_DIR}/gen.c -P ${CMAKE_CURRENT_SOURCE_DIR}/gen.cmake
          DEPENDS gen.cmake COMMENT "Generating gen.c" VERBATIM)
        add_custom_target(stamp ALL COMMAND ${CMAKE_COMMAND} -E touch stamp.txt BYPRODUCTS stamp.txt VERBATIM)
        add_subdirectory(sub)
        add_executable(app main.cpp ${CMAKE_CURRENT_BINARY_DIR}/gen.c)
        target_precompile_headers(app PRIVATE "$<$<COMPILE_LANGUAGE:CXX>:<vector$<ANGLE-R>>")
        target_link_libraries(app PRIVATE core ext::opts)
        add_dependencies(app stamp)
        install(TARGETS app core EXPORT fapiTargets RUNTIME DESTINATION bin ARCHIVE DESTINATION lib)
        install(EXPORT fapiTargets NAMESPACE fapi:: DESTINATION lib/cmake/fapi)
        install(FILES main.cpp DESTINATION share/fapi COMPONENT docs)
        install(CODE "message(STATUS \\"custom install code\\")")
        """,
    "gen.cmake": 'file(WRITE "${OUT}" "int gen_value(void) { return 5; }\\n")\n',
    "sub/CMakeLists.txt": """
        add_library(objs OBJECT obj.c)
        add_library(core STATIC core.c $<TARGET_OBJECTS:objs>)
        target_include_directories(core PUBLIC $<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}>)
        target_compile_definitions(core PUBLIC CORE_LEVEL=3)
        """,
    "sub/core.c": "int obj_value(void);\nint core_value(void) { return CORE_LEVEL + obj_value(); }\n",
    "sub/obj.c": "int obj_value(void) { return 0; }\n",
    "main.cpp": """
        #include <vector>
        extern "C" int core_value(void);
        extern "C" int gen_value(void);
        int main() { std::vector<int> v{core_value(), gen_value(), EXT_OPT}; return v[0] + v[1] + v[2] == 9 ? 0 : 1; }
        """,
}
FILE_API_KINDS = {"codemodel": 2, "cache": 2, "toolchains": 1, "cmakeFiles": 1, "configureLog": 1}
FILE_API_CLIENT_QUERY = {
    "requests": [
        {"kind": "codemodel", "version": [{"major": 2, "minor": 99}, 2]},
        {"kind": "cache", "version": 2},
        {"kind": "toolchains", "version": 1},
        {"kind": "cmakeFiles", "version": 1},
        {"kind": "configureLog", "version": 1},
        {"kind": "cache", "version": 3},
        {"kind": "bogus", "version": 1},
    ],
    "client": {"purpose": "independent-verify"},
}


def write_file_api_queries(build: Path) -> None:
    """Write shared stateless queries, a client stateless query, and a client stateful query."""
    query = build / ".cmake/api/v1/query"
    (query / "client-indep").mkdir(parents=True, exist_ok=True)
    (query / "client-other").mkdir(parents=True, exist_ok=True)
    for kind, major in FILE_API_KINDS.items():
        (query / f"{kind}-v{major}").write_text("", encoding="utf-8")
    (query / "client-other/cache-v2").write_text("", encoding="utf-8")
    (query / "client-indep/query.json").write_text(json.dumps(FILE_API_CLIENT_QUERY), encoding="utf-8")


def resolve_reply(reply: Path, value: Any, hops: int = 0) -> Any:
    """Replace each "jsonFile" member with a "json" member that holds the file content.

    Reply file names hold content hashes, which change with the absolute
    paths inside them. The content does not, after path normalization.
    A reply file cannot refer to more than four levels of other files.
    """
    if isinstance(value, Mapping):
        out = {}
        for key, item in value.items():
            if key == "jsonFile" and isinstance(item, str):
                if hops >= 4 or "/" in item or item.startswith("."):
                    out["json"] = f"<not resolved {item}>"
                    continue
                try:
                    data = json.loads((reply / item).read_text(encoding="utf-8"))
                except (OSError, ValueError) as exc:
                    data = f"<unreadable {type(exc).__name__}>"
                out["json"] = resolve_reply(reply, data, hops + 1)
            else:
                out[key] = resolve_reply(reply, item, hops)
        return out
    if isinstance(value, list):
        return [resolve_reply(reply, item, hops) for item in value]
    return value


def file_api_reply(build: Path) -> tuple[list[str], dict[str, Any]]:
    """Return the index file names and the resolved newest index of a build tree."""
    reply = build / ".cmake/api/v1/reply"
    indexes = sorted(p.name for p in reply.glob("index-*.json"))
    if not indexes:
        return [], {}
    return indexes, resolve_reply(reply, json.loads((reply / indexes[-1]).read_text(encoding="utf-8")))


def without_install_layout(value: Any) -> Any:
    """Remove the cache entries that depend only on the programs next to cmake.

    CMAKE_EDIT_COMMAND names ccmake or cmake-gui when one of them is next to
    the cmake program. That is a property of the installed package, not of
    the configure behavior.
    """
    if isinstance(value, Mapping):
        return {key: without_install_layout(item) for key, item in value.items()}
    if isinstance(value, list):
        return [without_install_layout(item) for item in value
                if not (isinstance(item, Mapping) and item.get("name") == "CMAKE_EDIT_COMMAND")]
    return value


def backtrace_chain(graph: Mapping[str, Any], index: Any) -> list[str]:
    """Return the frames of a backtraceGraph node, from the node to the root.

    Each frame is "file:line (command)", or only "file" for a file node.
    A bad index or a parent loop gives a "<bad node ...>" frame.
    """
    nodes, files, commands = (graph.get(key) or [] for key in ("nodes", "files", "commands"))
    chain: list[str] = []
    while index is not None:
        if isinstance(index, bool) or not isinstance(index, int) or not 0 <= index < len(nodes) \
                or len(chain) > len(nodes) or not isinstance(nodes[index], Mapping):
            chain.append(f"<bad node {index}>")
            break
        node = nodes[index]
        file, command = node.get("file"), node.get("command")
        frame = files[file] if isinstance(file, int) and 0 <= file < len(files) else f"<bad file {file}>"
        if "line" in node:
            frame += f":{node['line']}"
        if command is not None:
            frame += f" ({commands[command]})" if isinstance(command, int) and 0 <= command < len(commands) \
                else f" (<bad command {command}>)"
        chain.append(frame)
        index = node.get("parent")
    return chain


def canonical_codemodel(value: Any, graph: Mapping[str, Any] | None = None) -> Any:
    """Make a File API reply independent of two orders that CMake does not specify.

    - A target's "dependencies" array is sorted by "id". CMake writes it from
      a std::set that is ordered by object address (cmTargetDepend.h).
    - Each "backtrace" index and each "backtraces" index list becomes the
      resolved chain of frames. An index is the position of a node in
      "backtraceGraph", so it moves when an earlier node changes. The graph
      itself becomes the sorted list of the chains of all its nodes. Thus
      an extra, missing, or different frame is still a difference.
    """
    if isinstance(value, list):
        return [canonical_codemodel(item, graph) for item in value]
    if not isinstance(value, Mapping):
        return value
    own = value.get("backtraceGraph")
    if isinstance(own, Mapping):
        graph = own
    out: dict[str, Any] = {}
    for key, item in value.items():
        if key == "backtraceGraph" and isinstance(item, Mapping):
            nodes = item.get("nodes") if isinstance(item.get("nodes"), list) else []
            out[key] = [list(chain) for chain in sorted({tuple(backtrace_chain(item, i)) for i in range(len(nodes))})]
        elif key == "backtrace" and graph is not None and isinstance(item, int) and not isinstance(item, bool):
            out[key] = backtrace_chain(graph, item)
        elif key == "backtraces" and graph is not None and isinstance(item, list):
            out[key] = [backtrace_chain(graph, index) for index in item]
        elif key == "dependencies" and isinstance(item, list) and all(isinstance(d, Mapping) for d in item):
            out[key] = sorted((canonical_codemodel(d, graph) for d in item), key=lambda d: str(d.get("id", "")))
        else:
            out[key] = canonical_codemodel(item, graph)
    return out


def find_target(config: Mapping[str, Any], name: str) -> dict[str, Any]:
    """Return the resolved target object of a codemodel configuration."""
    for item in config.get("targets", []) + config.get("abstractTargets", []):
        if item.get("name") == name:
            return item.get("json", {})
    return {}


@scenario("s17", "File-Based API: codemodel v2.12/cache v2/toolchains v1/cmakeFiles v1/configureLog v1 on Ninja Multi-Config, stateful+stateless")
def s17_file_api(ctx: Ctx) -> None:
    r = ctx.root
    if "Ninja Multi-Config" not in generators_available(ctx.options):
        raise ScenarioSkip("needs Ninja Multi-Config")
    src = ctx.tree(r / "src", FILE_API_PROJECT)
    build = r / "build"
    write_file_api_queries(build)
    ctx.ok("configure", ctx.configure(src, build, "Ninja Multi-Config", f"-DCMAKE_INSTALL_PREFIX={r}/inst"))
    ctx.learn_tool_paths(build)
    indexes, index = file_api_reply(build)
    ctx.require("one index file", len(indexes) == 1, indexes)

    objects = sorted((o.get("kind"), o.get("version", {}).get("major"), o.get("version", {}).get("minor"))
                     for o in index.get("objects", []))
    ctx.check("index objects and versions", objects == [
        ("cache", 2, 0), ("cmakeFiles", 1, 1), ("codemodel", 2, 12), ("configureLog", 1, 0), ("toolchains", 1, 1)],
        objects)
    reply = index.get("reply", {})
    ctx.check("index reply keys", sorted(reply) == [
        "cache-v2", "client-indep", "client-other", "cmakeFiles-v1", "codemodel-v2", "configureLog-v1",
        "toolchains-v1"], sorted(reply))
    ctx.check("client stateless query", "cache-v2" in reply.get("client-other", {}), reply.get("client-other"))
    responses = reply.get("client-indep", {}).get("query.json", {}).get("responses", [])
    shape = [r_.get("kind") or ("error" if "error" in r_ else "?") for r_ in responses]
    ctx.check("client stateful responses", shape == [
        "codemodel", "cache", "toolchains", "cmakeFiles", "configureLog", "error", "error"], shape)
    ctx.check("client data echoed",
              reply.get("client-indep", {}).get("query.json", {}).get("client") == FILE_API_CLIENT_QUERY["client"])
    ctx.observe("client errors", [r_.get("error") for r_ in responses if "error" in r_])
    gen = index.get("cmake", {}).get("generator", {})
    ctx.check("generator is multi-config", gen == {"multiConfig": True, "name": "Ninja Multi-Config"}, gen)

    codemodel = reply.get("codemodel-v2", {}).get("json", {})
    configs = {c.get("name"): c for c in codemodel.get("configurations", [])}
    ctx.check("three configurations", list(configs) == ["Debug", "Release", "RelWithDebInfo"], list(configs))
    release = configs.get("Release", {})
    names = sorted(t.get("name") for t in release.get("targets", []))
    abstract = sorted(t.get("name") for t in release.get("abstractTargets", []))
    ctx.check("build system targets", names == ["app", "core", "objs", "stamp"], names)
    ctx.check("imported targets are abstract", abstract == ["ext", "ext::opts"], abstract)
    dirs = [(d.get("source"), d.get("build"), d.get("hasInstallRule", False)) for d in release.get("directories", [])]
    ctx.check("directories", dirs == [(".", ".", True), ("sub", "sub", False)], dirs)

    app = find_target(release, "app")
    groups = app.get("compileGroups", [])
    pch = [h.get("header") for g in groups for h in g.get("precompileHeaders", [])]
    defines = sorted({d.get("define") for g in groups for d in g.get("defines", [])})
    ctx.check("app type and artifact", app.get("type") == "EXECUTABLE"
              and [a.get("path") for a in app.get("artifacts", [])] == ["Release/app"], app.get("artifacts"))
    ctx.check("app precompiled header", "<vector>" in pch, pch)
    ctx.check("app usage requirements", defines == ["CORE_LEVEL=3", "EXT_OPT=1"], defines)
    generated = [s.get("path", "") for s in app.get("sources", []) if s.get("isGenerated")]
    ctx.check("app generated source", any(p.endswith("/build/gen.c") for p in generated), generated)
    deps = sorted(d.get("id", "").split("::@")[0] for d in app.get("dependencies", []))
    ctx.check("app dependencies", deps == ["core", "stamp"], deps)
    ctx.check("app install destination",
              [d.get("path") for d in app.get("install", {}).get("destinations", [])] == ["bin"], app.get("install"))
    core = find_target(release, "core")
    ctx.check("core artifact", core.get("type") == "STATIC_LIBRARY" and
              [a.get("path") for a in core.get("artifacts", [])] == ["sub/Release/libcore.a"], core.get("artifacts"))
    ctx.check("ext is imported", find_target(release, "ext").get("imported") is True, find_target(release, "ext"))
    top = next((d.get("json", {}) for d in release.get("directories", []) if d.get("source") == "."), {})
    installers = [(i.get("type"), i.get("component"), i.get("destination")) for i in top.get("installers", [])]
    kinds = sorted({str(i[0]) for i in installers})
    ctx.check("installers of the top directory", kinds == ["code", "export", "file", "target"], installers)

    cache = {e.get("name"): e.get("value") for e in reply.get("cache-v2", {}).get("json", {}).get("entries", [])}
    ctx.check("cache entries", cache.get("CMAKE_CONFIGURATION_TYPES") == "Debug;Release;RelWithDebInfo"
              and cache.get("CMAKE_PROJECT_NAME") == "fapi", {k: cache.get(k) for k in ("CMAKE_CONFIGURATION_TYPES",)})
    toolchains = reply.get("toolchains-v1", {}).get("json", {}).get("toolchains", [])
    langs = sorted(t.get("language") for t in toolchains)
    ctx.check("toolchains", langs == ["C", "CXX"] and all(t.get("compiler", {}).get("id") for t in toolchains), langs)
    inputs = reply.get("cmakeFiles-v1", {}).get("json", {}).get("inputs", [])
    ctx.check("cmakeFiles inputs", any(i.get("path") == "sub/CMakeLists.txt" for i in inputs)
              and any(i.get("isCMake") for i in inputs), [i.get("path") for i in inputs][:40])
    log = reply.get("configureLog-v1", {}).get("json", {})
    ctx.check("configureLog", log.get("path", "").endswith("CMakeFiles/CMakeConfigureLog.yaml")
              and "try_compile-v1" in log.get("eventKindNames", []), log)

    ctx.observe(
        "index",
        canonical_codemodel(without_install_layout({key: value for key, value in index.items() if key != "reply"})),
    )
    for key in sorted(reply):
        ctx.observe(f"reply {key}", canonical_codemodel(without_install_layout(reply[key])))

    ctx.ok("build Release", ctx.build(build, "Ninja Multi-Config"))
    for target in ("app", "core"):
        for art in find_target(release, target).get("artifacts", []):
            ctx.check(f"artifact {art.get('path')} exists", (build / art.get("path", "")).is_file())
    ctx.check("program runs", ctx.run([build / "Release/app"]).ok)

    (build / ".cmake/api/v1/query/client-indep/query.json").unlink()
    ctx.ok("reconfigure", ctx.cmake(build))
    indexes2, index2 = file_api_reply(build)
    ctx.check("one index after reconfigure", len(indexes2) == 1, indexes2)
    ctx.check("removed client query is gone from the reply",
              "query.json" not in index2.get("reply", {}).get("client-indep", {}), index2.get("reply", {}).get("client-indep"))
    ctx.observe("reply keys after reconfigure", sorted(index2.get("reply", {})))


ADV_BASE = "adv dir+ü@#"
ADV_SRC = "sr c+é@#1"
VENDOR_PROJECT = {
    "CMakeLists.txt": """
        cmake_minimum_required(VERSION 3.28)
        project(vendor C)
        add_library(vendor SHARED vendor.c)
        set_target_properties(vendor PROPERTIES VERSION 1.0 SOVERSION 1)
        install(TARGETS vendor LIBRARY DESTINATION lib)
        """,
    "vendor.c": "int vendor_value(void) { return 2; }\n",
}
ADV_PROJECT = {
    "CMakeLists.txt": """
        cmake_minimum_required(VERSION 3.19)
        project(adv C)
        set(v outer)
        block(SCOPE_FOR VARIABLES POLICIES PROPAGATE v)
          cmake_policy(SET CMP0115 NEW)
          set(v inner)
          set(hidden yes)
          cmake_policy(GET CMP0115 p1)
          cmake_policy(PUSH)
            cmake_policy(SET CMP0070 OLD)
            cmake_policy(GET CMP0070 p2)
          cmake_policy(POP)
          cmake_policy(GET CMP0070 p3)
          message(STATUS "POLICY block CMP0115=${p1} push CMP0070=${p2} pop CMP0070=${p3}")
        endblock()
        cmake_policy(GET CMP0115 a)
        cmake_policy(GET CMP0116 b)
        cmake_policy(GET CMP0220 c)
        cmake_policy(GET CMP0111 d)
        message(STATUS "POLICY after v=${v} hidden=[${hidden}] CMP0115=[${a}] CMP0116=[${b}] CMP0220=[${c}] CMP0111=[${d}]")
        block(SCOPE_FOR VARIABLES)
          cmake_policy(SET CMP0115 NEW)
        endblock()
        cmake_policy(GET CMP0115 e)
        message(STATUS "POLICY variables-only block leaks CMP0115=[${e}]")
        block(SCOPE_FOR POLICIES)
          cmake_policy(SET CMP0220 NEW)
          add_subdirectory("c++ part")
        endblock()
        message(STATUS "POLICY CMP0220 NEW gives CXX_LOADED=[${CMAKE_CXX_COMPILER_LOADED}]")
        file(GENERATE OUTPUT "rel gen/out.txt" CONTENT "generated $<CONFIG>\\n")
        add_subdirectory("dep sub")
        add_library(vendor SHARED IMPORTED)
        set_target_properties(vendor PROPERTIES IMPORTED_LOCATION "${VENDOR_LIB}" IMPORTED_SONAME "${VENDOR_SONAME}")
        add_library(dep SHARED "lib dir/dep.c")
        add_library(mid STATIC mid.c)
        target_link_libraries(mid PRIVATE dep vendor)
        add_executable(app main.c)
        target_link_libraries(app PRIVATE mid cxxpart)
        set_target_properties(app dep PROPERTIES INSTALL_RPATH "${REL_RPATH}")
        install(TARGETS app dep RUNTIME_DEPENDENCY_SET rds)
        install(RUNTIME_DEPENDENCY_SET rds DIRECTORIES "${VENDOR_DIR}"
          PRE_INCLUDE_REGEXES "vendor" PRE_EXCLUDE_REGEXES ".*"
          POST_INCLUDE_REGEXES "vendor" POST_EXCLUDE_REGEXES ".*")
        """,
    "c++ part/CMakeLists.txt": """
        enable_language(CXX)
        add_library(cxxpart STATIC part.cpp)
        """,
    "c++ part/part.cpp": """
        #include <string>
        extern "C" int cxx_value(void) { return static_cast<int>(std::string("abc").size()); }
        """,
    "dep sub/CMakeLists.txt": """
        add_custom_command(OUTPUT old.txt COMMAND ${CMAKE_COMMAND} -DOUT=old.txt -P "${CMAKE_CURRENT_SOURCE_DIR}/gen.cmake"
          DEPFILE old.d)
        add_custom_target(oldt DEPENDS old.txt)
        block(SCOPE_FOR POLICIES)
          cmake_policy(SET CMP0116 NEW)
          add_custom_command(OUTPUT new.txt COMMAND ${CMAKE_COMMAND} -DOUT=new.txt -P "${CMAKE_CURRENT_SOURCE_DIR}/gen.cmake"
            DEPFILE new.d)
        endblock()
        add_custom_target(newt ALL DEPENDS new.txt)
        """,
    "dep sub/gen.cmake": """
        file(READ "${CMAKE_CURRENT_LIST_DIR}/input.txt" text)
        file(WRITE "${OUT}" "${text}")
        get_filename_component(stem "${OUT}" NAME_WE)
        file(WRITE "${stem}.d" "${OUT}: ${CMAKE_CURRENT_LIST_DIR}/input.txt\\n")
        """,
    "dep sub/input.txt": "first\n",
    "lib dir/dep.c": "int dep_value(void) { return 7; }\n",
    "mid.c": "int dep_value(void);\nint vendor_value(void);\nint mid_value(void) { return dep_value() * 6 * vendor_value(); }\n",
    "main.c": """
        #include <stdio.h>
        int mid_value(void);
        int cxx_value(void);
        int main(void) { printf("adv %d %d\\n", mid_value(), cxx_value()); return mid_value() == 84 ? 0 : 1; }
        """,
}
ADV_NEGATIVE = {
    "cmp0220-old": """
        cmake_minimum_required(VERSION 3.28)
        project(n C)
        block(SCOPE_FOR POLICIES)
          cmake_policy(SET CMP0220 OLD)
          add_subdirectory(sub)
        endblock()
        message(STATUS "POLICY CMP0220 OLD gives CXX_LOADED=[${CMAKE_CXX_COMPILER_LOADED}]")
        add_executable(cx x.cpp)
        """,
    "cmp0111": """
        cmake_minimum_required(VERSION 3.28)
        project(n C)
        block(SCOPE_FOR POLICIES)
          cmake_policy(SET CMP0111 OLD)
          add_library(oldimp SHARED IMPORTED)
        endblock()
        add_library(newimp SHARED IMPORTED)
        add_executable(e main.c)
        target_link_libraries(e oldimp newimp)
        """,
}
ADV_NEGATIVE_FILES = {
    "sub/CMakeLists.txt": "enable_language(CXX)\n",
    "x.cpp": "int main() { return 0; }\n",
    "main.c": "int main(void) { return 0; }\n",
}


def build_ninja_rules(build: Path, output: str) -> list[str]:
    """Return the COMMAND and depfile lines of the build.ninja rule for an output."""
    text = (build / "build.ninja").read_text(encoding="utf-8", errors="replace") \
        if (build / "build.ninja").is_file() else ""
    lines: list[str] = []
    capture = False
    for line in text.splitlines():
        if line.startswith("build "):
            capture = f"/{output} " in line.split(":")[0] + " " and "CUSTOM_COMMAND" in line
        elif capture and line.strip().startswith(("COMMAND =", "depfile =", "deps =")):
            lines.append(re.sub(r"[0-9a-f]{64}", "<HASH>", line.strip()))
    return lines


def _check_adversarial_negative_scopes(ctx: Ctx, base: Path, generator: str) -> None:
    for name, body in ADV_NEGATIVE.items():
        nsrc = ctx.tree(base / f"neg {name}", ADV_NEGATIVE_FILES)
        ctx.write("CMakeLists.txt", body, nsrc)
        proc = ctx.configure(nsrc, base / f"neg-b {name}", generator)
        blocks = cmake_errors(proc.output)
        ctx.check(f"{name}: configure fails", proc.returncode != 0, proc.tail())
        ctx.observe(f"{name}: diagnostics",
                    [proc.returncode, message_lines(proc.stdout, "-- POLICY"), collapse_repeats(blocks)])
        if name == "cmp0220-old":
            ctx.check("CMP0220 OLD keeps CXX in the block",
                      "-- POLICY CMP0220 OLD gives CXX_LOADED=[]" in proc.stdout, proc.tail())
            ctx.check("CMP0220 OLD: top-level C++ target cannot compile",
                      "Missing variable is:\nCMAKE_CXX_COMPILE_OBJECT" in proc.output, proc.tail())
        else:
            ctx.check("CMP0111 NEW errors for the target made outside the block", any(
                'IMPORTED_LOCATION or IMPORTED_IMPLIB not set for imported target "newimp" configuration "Release".' in b
                for b in blocks),
                blocks)
            ctx.check("CMP0111 OLD target made inside the block gives no error",
                      not any('"oldimp"' in b for b in blocks if b.startswith("CMake Error")), blocks)
            ctx.check("CMP0111 OLD deprecation warning", any(
                "The OLD behavior for policy CMP0111 will be removed" in b for b in blocks), blocks)


@scenario("s18", "Adversarial paths (space, UTF-8, + @ #) + block/cmake_policy scopes + CMP0070/0111/0115/0116/0220 + RUNTIME_DEPENDENCY_SET")
def s18_adversarial(ctx: Ctx) -> None:
    gens = [g for g in ("Ninja", "Unix Makefiles") if g in generators_available(ctx.options)]
    if not gens:
        raise ScenarioSkip("needs Ninja or Unix Makefiles")
    base = ctx.root / ADV_BASE
    rel_rpath = "@loader_path/../lib" if context.DARWIN else "$ORIGIN/../lib"
    real, soname = shared_names("vendor", "1.0", "1")

    vsrc = ctx.tree(base / "vendor src", VENDOR_PROJECT)
    vinst = base / "v inst+@#"
    ctx.ok("vendor configure", ctx.configure(vsrc, base / "v bld+@#", gens[0], f"-DCMAKE_INSTALL_PREFIX={vinst}"))
    ctx.ok("vendor build", ctx.build(base / "v bld+@#", gens[0]))
    ctx.ok("vendor install", ctx.cmake("--install", base / "v bld+@#"))
    ctx.require("vendor library installed", (vinst / "lib" / real).is_file(), file_listing(vinst))

    src = ctx.tree(base / ADV_SRC, ADV_PROJECT)
    for gen in gens:
        tag = slug(gen)
        build = base / f"bl d+ö@#-{tag}"
        prefix = base / f"in st+ß@#-{tag}"
        try:
            conf = ctx.configure(
                src, build, gen, f"-DCMAKE_INSTALL_PREFIX={prefix}", "-DCMAKE_POLICY_DEFAULT_CMP0116=OLD",
                f"-DVENDOR_LIB={vinst / 'lib' / real}", f"-DVENDOR_SONAME={soname}", f"-DVENDOR_DIR={vinst / 'lib'}",
                f"-DREL_RPATH={rel_rpath}")
            ctx.ok(f"{gen}: configure", conf)
            ctx.learn_tool_paths(build)
            policy = message_lines(conf.stdout, "-- POLICY")
            ctx.check(f"{gen}: block and cmake_policy scopes", policy == [
                "-- POLICY block CMP0115=NEW push CMP0070=OLD pop CMP0070=NEW",
                "-- POLICY after v=inner hidden=[] CMP0115=[] CMP0116=[OLD] CMP0220=[] CMP0111=[NEW]",
                "-- POLICY variables-only block leaks CMP0115=[NEW]",
                "-- POLICY CMP0220 NEW gives CXX_LOADED=[1]",
            ], policy)
            warnings = cmake_errors(conf.stderr)
            ctx.check(f"{gen}: CMAKE_POLICY_DEFAULT_CMP0116=OLD deprecation warning", any(
                "at CMakeLists.txt:1 (cmake_minimum_required)" in w
                and "The OLD behavior for policy CMP0116 will be removed" in w for w in warnings), warnings)
            ctx.check(f"{gen}: CMP0070 OLD deprecation warning", any(
                "The OLD behavior for policy CMP0070 will be removed" in w for w in warnings), warnings)
            ctx.check(f"{gen}: no unused-cli warning", not any("unused-cli" in w for w in warnings), warnings)
            ctx.observe(f"{gen}: configure messages", [policy, warnings])
            ctx.check(f"{gen}: CMP0070 NEW puts relative file(GENERATE) output in the build tree",
                      (build / "rel gen/out.txt").read_text(encoding="utf-8") == "generated Release\n"
                      if (build / "rel gen/out.txt").is_file() else False, file_listing(build)[:50])
            if gen == "Ninja":
                old_rule, new_rule = build_ninja_rules(build, "old.txt"), build_ninja_rules(build, "new.txt")
                ctx.check("Ninja: CMP0116 OLD keeps the DEPFILE path", "depfile = old.d" in old_rule
                          and not any("cmake_transform_depfile" in line for line in old_rule), old_rule)
                ctx.check("Ninja: CMP0116 NEW transforms the DEPFILE", any(
                    "cmake_transform_depfile Ninja gccdepfile" in line for line in new_rule)
                    and "depfile = CMakeFiles/d/<HASH>.d" in new_rule, new_rule)
                ctx.observe("Ninja: depfile rules", [old_rule, new_rule])

            ctx.ok(f"{gen}: build", ctx.build(build, gen))
            run = ctx.run([build / "app"])
            ctx.check(f"{gen}: build-tree app runs", run.ok and run.stdout == "adv 84 3\n", run.tail())
            new_txt = build / "dep sub/new.txt"
            ctx.check(f"{gen}: DEPFILE command output", new_txt.is_file() and new_txt.read_text() == "first\n")
            touch_later(src / "dep sub/input.txt", "second\n")
            ctx.ok(f"{gen}: rebuild after a DEPFILE input changes", ctx.build(build, gen))
            ctx.check(f"{gen}: DEPFILE dependency reruns the command",
                      new_txt.is_file() and new_txt.read_text() == "second\n",
                      new_txt.read_text() if new_txt.is_file() else "missing")
            (src / "dep sub/input.txt").write_text("first\n", encoding="utf-8")

            ctx.ok(f"{gen}: install", ctx.cmake("--install", build))
            listing = file_listing(prefix)
            expect_libs = sorted(["lib/libdep.dylib" if context.DARWIN else "lib/libdep.so", f"lib/{real}",
                                  "lib/" + soname.rsplit("/", 1)[-1]])
            ctx.check(f"{gen}: install tree has the transitive runtime libraries",
                      sorted(item.split(" -> ")[0] for item in listing) == sorted(["bin/app", *expect_libs]),
                      listing)
            ctx.observe(f"{gen}: install tree", listing)
            installed = ctx.run([prefix / "bin/app"])
            ctx.check(f"{gen}: installed app runs", installed.ok and installed.stdout == "adv 84 3\n",
                      installed.tail())
            ctx.observe(f"{gen}: outputs", [run.stdout, installed.stdout])
            if context.DARWIN:
                rpaths = macho_rpaths(prefix / "bin/app")
                loads = macho_load_dylibs(prefix / "bin/app")
                ctx.check(f"{gen}: installed RPATH", rpaths == ["@loader_path/../lib"], rpaths)
                ctx.check(f"{gen}: installed load commands",
                          sorted(loads) == ["@rpath/libdep.dylib", "@rpath/libvendor.1.dylib"], loads)
                ctx.observe(f"{gen}: installed load commands", [rpaths, sorted(loads)])
            elif shutil.which("readelf"):
                dyn = elf_dynamic(prefix / "bin/app")
                ctx.check(f"{gen}: installed RUNPATH", (dyn.get("RUNPATH") or dyn.get("RPATH")) == ["$ORIGIN/../lib"], dyn)
                ctx.observe(f"{gen}: installed dynamic section",
                            {k: v for k, v in dyn.items() if k != "NEEDED"} | {
                                "NEEDED": sorted(n for n in dyn.get("NEEDED", []) if "dep" in n or "vendor" in n)})
            else:
                ctx.skip(f"{gen}: readelf not found; RUNPATH not inspected")
        except StepFailed:
            continue

    _check_adversarial_negative_scopes(ctx, base, gens[0])
