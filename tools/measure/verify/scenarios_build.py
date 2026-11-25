# SPDX-License-Identifier: MIT OR Apache-2.0
"""Build, link, PCH, Fortran, unity, install/export, and C++20 module scenarios."""

from __future__ import annotations

import json
from pathlib import Path
import re
import shutil
import subprocess
import tempfile

from . import context
from .context import (
    Ctx,
    Options,
    cmake_errors,
    collapse_repeats,
    compiled,
    elf_dynamic,
    exe_path,
    exported_symbols,
    file_listing,
    generators_available,
    macho_id,
    macho_load_dylibs,
    macho_rpaths,
    macho_versions,
    message_lines,
    no_op,
    scenario,
    slug,
    touch_later,
)
from .sandbox import ScenarioSkip, StepFailed


DEMO_HEADER = """
    #ifndef DEMO_DEMO_H
    #define DEMO_DEMO_H
    #ifdef __cplusplus
    extern "C" {
    #endif
    int demo_obj_value(void);
    int demo_static_value(void);
    int demo_shared_value(void);
    int demo_objcxx_value(void);
    #ifdef __cplusplus
    }
    #endif
    #endif
    """

DEMO_MAIN = """
    #include <cstdio>
    #include "demo/demo.h"
    #ifndef DEMO_HAVE_OBJCXX
    extern "C" int demo_objcxx_value(void) { return 6; }
    #endif
    consteval int square(int x) { return x * x; }
    int main() {
      constexpr int sq = square(7);
      int a = demo_static_value(), b = demo_shared_value(), c = demo_objcxx_value();
      std::printf("%s %d %d %d %d\\n", NAME, sq, a, b, c);
      return (sq == 49 && a == 13 && b == 83 && c == 6) ? 0 : 1;
    }
    """

DEMO_PROJECT = {
    "CMakeLists.txt": """
        cmake_minimum_required(VERSION 3.28)
        project(Demo VERSION 1.2.3 LANGUAGES C CXX)
        if(APPLE)
          enable_language(OBJCXX)
        endif()
        include(GNUInstallDirs)
        include(CMakePackageConfigHelpers)
        enable_testing()
        set(CMAKE_CXX_STANDARD 20)
        set(CMAKE_CXX_STANDARD_REQUIRED ON)
        set(CMAKE_CXX_EXTENSIONS OFF)
        if(APPLE)
          set(rel_rpath "@loader_path/../${CMAKE_INSTALL_LIBDIR}")
        else()
          set(rel_rpath "$ORIGIN/../${CMAKE_INSTALL_LIBDIR}")
        endif()

        add_library(demo_obj OBJECT obj.c)
        set_target_properties(demo_obj PROPERTIES POSITION_INDEPENDENT_CODE ON)
        target_include_directories(demo_obj PUBLIC $<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/include>)

        add_library(demo_static STATIC static.c $<TARGET_OBJECTS:demo_obj>)
        set_target_properties(demo_static PROPERTIES POSITION_INDEPENDENT_CODE ON)
        target_include_directories(demo_static PUBLIC
          $<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/include>
          $<INSTALL_INTERFACE:${CMAKE_INSTALL_INCLUDEDIR}>)

        add_library(demo_shared SHARED shared.cpp)
        target_link_libraries(demo_shared PRIVATE demo_static)
        target_include_directories(demo_shared PUBLIC
          $<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/include>
          $<INSTALL_INTERFACE:${CMAKE_INSTALL_INCLUDEDIR}>)
        set_target_properties(demo_shared PROPERTIES VERSION ${PROJECT_VERSION} SOVERSION 1
          INSTALL_NAME_DIR "@rpath")
        target_compile_features(demo_shared PUBLIC cxx_std_20)

        set(demo_targets demo_obj demo_static demo_shared)
        add_executable(demo_app app.cpp)
        target_compile_definitions(demo_app PRIVATE NAME="app")
        target_link_libraries(demo_app PRIVATE demo_shared demo_static)
        if(APPLE)
          add_library(demo_objcxx STATIC objcxx.mm)
          target_compile_options(demo_objcxx PRIVATE -fobjc-arc)
          target_link_libraries(demo_objcxx PUBLIC "-framework Foundation")
          target_include_directories(demo_objcxx PUBLIC
            $<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/include>
            $<INSTALL_INTERFACE:${CMAKE_INSTALL_INCLUDEDIR}>)
          list(APPEND demo_targets demo_objcxx)
          target_link_libraries(demo_app PRIVATE demo_objcxx)
          target_compile_definitions(demo_app PRIVATE DEMO_HAVE_OBJCXX)
        endif()
        set_target_properties(demo_app PROPERTIES
          BUILD_RPATH "${CMAKE_BINARY_DIR}/extra-rpath"
          INSTALL_RPATH "${rel_rpath}")
        add_test(NAME demo_app COMMAND demo_app)
        set_tests_properties(demo_app PROPERTIES PASS_REGULAR_EXPRESSION "^app 49 13 83 6")

        install(TARGETS ${demo_targets} EXPORT DemoTargets
          ARCHIVE DESTINATION ${CMAKE_INSTALL_LIBDIR}
          LIBRARY DESTINATION ${CMAKE_INSTALL_LIBDIR}
          RUNTIME DESTINATION ${CMAKE_INSTALL_BINDIR}
          OBJECTS DESTINATION ${CMAKE_INSTALL_LIBDIR}/objects
          INCLUDES DESTINATION ${CMAKE_INSTALL_INCLUDEDIR})
        install(TARGETS demo_app RUNTIME DESTINATION ${CMAKE_INSTALL_BINDIR})
        install(DIRECTORY include/ DESTINATION ${CMAKE_INSTALL_INCLUDEDIR})
        install(EXPORT DemoTargets NAMESPACE Demo:: DESTINATION ${CMAKE_INSTALL_LIBDIR}/cmake/Demo)
        configure_package_config_file(DemoConfig.cmake.in ${CMAKE_CURRENT_BINARY_DIR}/DemoConfig.cmake
          INSTALL_DESTINATION ${CMAKE_INSTALL_LIBDIR}/cmake/Demo
          PATH_VARS CMAKE_INSTALL_INCLUDEDIR)
        write_basic_package_version_file(${CMAKE_CURRENT_BINARY_DIR}/DemoConfigVersion.cmake
          VERSION ${PROJECT_VERSION} COMPATIBILITY SameMajorVersion)
        install(FILES ${CMAKE_CURRENT_BINARY_DIR}/DemoConfig.cmake
          ${CMAKE_CURRENT_BINARY_DIR}/DemoConfigVersion.cmake
          DESTINATION ${CMAKE_INSTALL_LIBDIR}/cmake/Demo)
        """,
    "DemoConfig.cmake.in": """
        @PACKAGE_INIT@
        include("${CMAKE_CURRENT_LIST_DIR}/DemoTargets.cmake")
        set_and_check(DEMO_INCLUDE_DIR "@PACKAGE_CMAKE_INSTALL_INCLUDEDIR@")
        check_required_components(Demo)
        """,
    "include/demo/demo.h": DEMO_HEADER,
    "obj.c": """
        #include "demo/demo.h"
        int demo_obj_value(void) { return 3; }
        """,
    "static.c": """
        #include "demo/demo.h"
        int demo_static_value(void) { return 10 + demo_obj_value(); }
        """,
    "shared.cpp": """
        #include <algorithm>
        #include "demo/demo.h"
        extern "C" int demo_shared_value(void) {
          return std::ranges::max({1, 7, 4}) * 10 + demo_static_value();
        }
        """,
    "objcxx.mm": """
        #import <Foundation/Foundation.h>
        #include "demo/demo.h"
        #if !__has_feature(objc_arc)
        #error "ARC is required"
        #endif
        extern "C" int demo_objcxx_value(void) {
          @autoreleasepool {
            NSString *text = [NSString stringWithFormat:@"%d-%s", 42, "arc"];
            return (int)[text length];
          }
        }
        """,
    "app.cpp": DEMO_MAIN,
}

DEMO_CONSUMER = {
    "CMakeLists.txt": """
        cmake_minimum_required(VERSION 3.28)
        project(Consumer CXX)
        find_package(Demo 1.2 CONFIG REQUIRED)
        message(STATUS "FOUND Demo ${Demo_VERSION} include=${DEMO_INCLUDE_DIR}")
        enable_testing()
        add_executable(consumer consumer.cpp)
        target_compile_definitions(consumer PRIVATE NAME="consumer"
          $<$<TARGET_EXISTS:Demo::demo_objcxx>:DEMO_HAVE_OBJCXX>)
        target_link_libraries(consumer PRIVATE Demo::demo_shared Demo::demo_static
          $<TARGET_NAME_IF_EXISTS:Demo::demo_objcxx>)
        add_test(NAME consumer COMMAND consumer)
        """,
    "consumer.cpp": DEMO_MAIN,
}


@scenario("s01", "End-to-end C + C++20 + ObjC++ build/test/install/export/import consumer on every generator")
def s01_end_to_end(ctx: Ctx) -> None:
    src = ctx.tree(ctx.root / "src", DEMO_PROJECT)
    consumer_src = ctx.tree(ctx.root / "consumer", DEMO_CONSUMER)
    generators = generators_available(ctx.options)
    if not generators:
        raise ScenarioSkip("no generator available")
    if "Xcode" not in generators and context.DARWIN:
        ctx.skip("Xcode generator not available or disabled")
    for gen in generators:
        try:
            e2e_generator(ctx, gen, src, consumer_src)
        except StepFailed:
            continue  # Report the failure and continue with the next generator.


def e2e_generator(ctx: Ctx, gen: str, src: Path, consumer_src: Path) -> None:
    """Run the end-to-end flow with one generator."""
    tag = slug(gen)
    build = ctx.root / f"b-{tag}"
    prefix = ctx.root / f"inst-{tag}"
    # Xcode signs binaries after the link. The install step then edits the
    # rpaths with install_name_tool, which makes that signature invalid, and
    # macOS kills the installed program. With signing off, the linker ad-hoc
    # signature stays valid because install_name_tool updates it.
    signing = ["-DCMAKE_XCODE_ATTRIBUTE_CODE_SIGNING_ALLOWED=NO"] if gen == "Xcode" else []
    ctx.ok(f"{gen}: configure", ctx.configure(src, build, gen, f"-DCMAKE_INSTALL_PREFIX={prefix}", *signing))
    ctx.ok(f"{gen}: build", ctx.build(build, gen))
    app = exe_path(build, "demo_app", gen)
    run = ctx.run([app])
    ctx.check(f"{gen}: build-tree app runs", run.stdout == "app 49 13 83 6\n", run.tail())
    test = ctx.ctest("--test-dir", build, "-C", "Release", "--output-on-failure")
    ctx.check(f"{gen}: ctest", test.ok and "100% tests passed" in test.stdout, test.tail())
    ctx.ok(f"{gen}: install", ctx.cmake("--install", build, "--config", "Release"))
    listing = file_listing(prefix)
    for required in ("bin/demo_app", "include/demo/demo.h", "lib/cmake/Demo/DemoConfig.cmake",
                     "lib/cmake/Demo/DemoConfigVersion.cmake", "lib/cmake/Demo/DemoTargets.cmake",
                     "lib/cmake/Demo/DemoTargets-release.cmake"):
        ctx.check(f"{gen}: installs {required}", required in listing, listing)
    ctx.check(f"{gen}: installs the object file",
              any(item.startswith("lib/objects/") for item in listing), listing)
    ctx.observe(f"{gen}: install tree", listing)
    installed = ctx.run([prefix / "bin/demo_app"])
    ctx.check(f"{gen}: installed app runs", installed.stdout == "app 49 13 83 6\n", installed.tail())
    if context.DARWIN:
        build_rpaths = macho_rpaths(app)
        ctx.check(f"{gen}: BUILD_RPATH in build tree", f"{build}/extra-rpath" in build_rpaths, build_rpaths)
        inst_rpaths = macho_rpaths(prefix / "bin/demo_app")
        ctx.check(f"{gen}: INSTALL_RPATH replaces BUILD_RPATH", inst_rpaths == ["@loader_path/../lib"], inst_rpaths)
        lib = prefix / "lib/libdemo_shared.1.dylib"
        ctx.check(f"{gen}: INSTALL_NAME_DIR", macho_id(lib) == "@rpath/libdemo_shared.1.dylib", macho_id(lib))
        ctx.observe(f"{gen}: installed rpaths", inst_rpaths)
        ctx.observe(f"{gen}: installed id", macho_id(lib))
    targets = (prefix / "lib/cmake/Demo/DemoTargets.cmake").read_text()
    ctx.check(f"{gen}: export uses the Demo:: namespace",
              "add_library(Demo::demo_shared SHARED IMPORTED)" in targets
              and "add_library(Demo::demo_obj OBJECT IMPORTED)" in targets, targets[:2000])

    cbuild = ctx.root / f"c-{tag}"
    conf = ctx.configure(consumer_src, cbuild, gen, f"-DCMAKE_PREFIX_PATH={prefix}")
    ctx.ok(f"{gen}: consumer configure", conf)
    found = re.findall(r"(?m)^-- FOUND Demo .*$", conf.stdout)
    ctx.check(f"{gen}: consumer find_package", found == [f"-- FOUND Demo 1.2.3 include={prefix}/include"], found)
    ctx.observe(f"{gen}: consumer found", found)
    ctx.ok(f"{gen}: consumer build", ctx.build(cbuild, gen))
    crun = ctx.run([exe_path(cbuild, "consumer", gen)])
    ctx.check(f"{gen}: consumer runs", crun.stdout == "consumer 49 13 83 6\n", crun.tail())
    ctest = ctx.ctest("--test-dir", cbuild, "-C", "Release")
    ctx.check(f"{gen}: consumer ctest", ctest.ok and "100% tests passed" in ctest.stdout, ctest.tail())
    ctx.observe(f"{gen}: outputs", [run.stdout, installed.stdout, crun.stdout])


LONG_DIRS = [f"component_{i}_with_a_deliberately_long_directory_name_x" for i in range(6)]
LONG_FILES = 440


def long_link_sources() -> dict[str, str]:
    """Return the sources of the long link project."""
    files: dict[str, str] = {}
    deep = "/".join(LONG_DIRS)
    for i in range(LONG_FILES):
        files[f"{deep}/translation_unit_with_a_long_descriptive_file_name_{i:04d}.c"] = (
            f"int long_fn_{i}(void) {{ return {i % 7}; }}\n"
        )
    decls = "".join(f"int long_fn_{i}(void);\n" for i in range(LONG_FILES))
    calls = "".join(f"  sum += long_fn_{i}();\n" for i in range(LONG_FILES))
    files["main.c"] = (
        f"#include <stdio.h>\n{decls}int main(void) {{\n  int sum = 0;\n{calls}"
        "  printf(\"sum %d\\n\", sum);\n  return 0;\n}\n"
    )
    files["CMakeLists.txt"] = (
        "cmake_minimum_required(VERSION 4.0)\nproject(longlink C)\n"
        "file(GLOB_RECURSE parts CONFIGURE_DEPENDS \"${CMAKE_CURRENT_SOURCE_DIR}/component_*/*.c\")\n"
        "list(SORT parts)\nadd_executable(longlink main.c ${parts})\n"
    )
    return files


@scenario("s02", "Long link command (> 140 KiB of object paths, spaces and quotes in paths), no-op rebuild")
def s02_long_link(ctx: Ctx) -> None:
    files = long_link_sources()
    src = ctx.root / "src dir with 'quote'"
    for rel, text in files.items():
        path = src / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
    objects = [f"CMakeFiles/longlink.dir/{rel}.o" for rel in files if rel.endswith(".c")]
    total = sum(len(obj) + 1 for obj in objects)
    ctx.check("object paths exceed 140 KiB", total > 140 * 1024, total)
    expected = f"sum {sum(i % 7 for i in range(LONG_FILES))}\n"
    gens = [g for g in ("Ninja", "Unix Makefiles") if g in generators_available(ctx.options)]
    for gen in gens:
        build = ctx.root / f"build dir 'q' {slug(gen)}"
        try:
            ctx.ok(f"{gen}: configure", ctx.configure(src, build, gen))
            ctx.ok(f"{gen}: build", ctx.build(build, gen))
        except StepFailed:
            continue
        run = ctx.run([build / "longlink"])
        ctx.check(f"{gen}: executable runs", run.stdout == expected, run.tail())
        again = ctx.build(build, gen)
        ctx.check(f"{gen}: second build is a no-op", no_op(again), again.tail())
        rsp = sorted(p.relative_to(build).as_posix() for p in build.rglob("*.rsp"))
        ctx.observe(f"{gen}: response files", rsp)
        ctx.observe(f"{gen}: output", run.stdout)


def universal_supported(_options: Options) -> str | None:
    """Skip when the host cannot build arm64 and x86_64 code."""
    if not context.DARWIN:
        return "needs macOS"
    with tempfile.TemporaryDirectory() as tmp:
        probe = subprocess.run(
            ["clang", "-arch", "arm64", "-arch", "x86_64", "-x", "c", "-c", "-", "-o", f"{tmp}/p.o"],
            input=b"int x;\n", capture_output=True, timeout=120,
        )
    return None if probe.returncode == 0 else "the toolchain cannot build arm64;x86_64"


@scenario("s03", "Universal arm64;x86_64 PCH (target_precompile_headers, SKIP_PRECOMPILE_HEADERS, REUSE_FROM)",
          requires=universal_supported)
def s03_universal_pch(ctx: Ctx) -> None:
    src = ctx.tree(ctx.root / "src", {
        "CMakeLists.txt": """
            cmake_minimum_required(VERSION 4.0)
            project(pch CXX)
            set(CMAKE_CXX_STANDARD 17)
            add_library(pchlib STATIC a.cpp b.cpp skip.cpp)
            target_precompile_headers(pchlib PRIVATE pch.h)
            set_source_files_properties(skip.cpp PROPERTIES SKIP_PRECOMPILE_HEADERS ON)
            add_executable(pchapp main.cpp)
            target_link_libraries(pchapp PRIVATE pchlib)
            set_target_properties(pchapp PROPERTIES PRECOMPILE_HEADERS_REUSE_FROM pchlib)
            """,
        "pch.h": """
            #include <vector>
            #define PCH_MARKER 1
            inline int pch_version() { return 1; }
            """,
        "a.cpp": """
            #ifndef PCH_MARKER
            #error "pch.h is not force-included"
            #endif
            int a_value() { return pch_version() * 10; }
            """,
        "b.cpp": """
            int b_value() { std::vector<int> v{1, 2, 3}; return static_cast<int>(v.size()); }
            """,
        "skip.cpp": """
            #ifdef PCH_MARKER
            #error "SKIP_PRECOMPILE_HEADERS source sees the PCH"
            #endif
            int skip_value() { return 5; }
            """,
        "main.cpp": """
            #include <cstdio>
            int a_value(); int b_value(); int skip_value();
            int main() {
              std::printf("pch %d %d %d %d\\n", pch_version(), a_value(), b_value(), skip_value());
              return 0;
            }
            """,
    })
    pch = src / "pch.h"
    original = pch.read_text()
    for gen in [g for g in ("Ninja", "Unix Makefiles") if g in generators_available(ctx.options)]:
        pch.write_text(original)
        build = ctx.root / f"b-{slug(gen)}"
        try:
            ctx.ok(f"{gen}: configure", ctx.configure(src, build, gen, "-DCMAKE_OSX_ARCHITECTURES=arm64;x86_64"))
            ctx.ok(f"{gen}: build", ctx.build(build, gen))
        except StepFailed:
            continue
        app = build / "pchapp"
        archs = subprocess.run(["lipo", "-archs", str(app)], capture_output=True, text=True, timeout=60).stdout.split()
        ctx.check(f"{gen}: lipo -archs has arm64 and x86_64", sorted(archs) == ["arm64", "x86_64"], archs)
        lib_archs = subprocess.run(["lipo", "-archs", str(build / "libpchlib.a")],
                                   capture_output=True, text=True, timeout=60).stdout.split()
        ctx.check(f"{gen}: static library is universal", sorted(lib_archs) == ["arm64", "x86_64"], lib_archs)
        run = ctx.run([app])
        ctx.check(f"{gen}: first output", run.stdout == "pch 1 10 3 5\n", run.tail())
        ctx.observe(f"{gen}: pch files", sorted(p.name for p in build.rglob("cmake_pch*") if p.is_file()))
        touch_later(pch, original.replace("return 1;", "return 2;"))
        rebuild = ctx.build(build, gen)
        ctx.ok(f"{gen}: rebuild after PCH header edit", rebuild, required=False)
        ctx.check(f"{gen}: PCH edit recompiles a.cpp", compiled(rebuild, r"a\.cpp\.o"), rebuild.tail())
        ctx.check(f"{gen}: PCH edit does not recompile skip.cpp", not compiled(rebuild, r"skip\.cpp\.o"),
                  rebuild.tail())
        run2 = ctx.run([app])
        ctx.check(f"{gen}: output after PCH edit", run2.stdout == "pch 2 20 3 5\n", run2.tail())
        again = ctx.build(build, gen)
        ctx.check(f"{gen}: third build is a no-op", no_op(again), again.tail())
        ctx.observe(f"{gen}: outputs", [run.stdout, run2.stdout])


@scenario("s04", "OBJECT_DEPENDS + MAIN_DEPENDENCY + generated headers + configure_file reconfigure rebuild (-j4)")
def s04_generated(ctx: Ctx) -> None:
    files = {
        "CMakeLists.txt": """
            cmake_minimum_required(VERSION 4.0)
            project(gen C)
            configure_file(config.h.in ${CMAKE_BINARY_DIR}/gen/config.h)
            add_custom_command(OUTPUT ${CMAKE_BINARY_DIR}/gen/table.h
              COMMAND ${CMAKE_COMMAND} -DIN=${CMAKE_SOURCE_DIR}/table.def
                      -DOUT=${CMAKE_BINARY_DIR}/gen/table.h -P ${CMAKE_SOURCE_DIR}/gen_table.cmake
              MAIN_DEPENDENCY table.def
              DEPENDS gen_table.cmake
              COMMENT "Generating table.h")
            add_custom_target(gen_headers DEPENDS ${CMAKE_BINARY_DIR}/gen/table.h)
            file(GLOB parts ${CMAKE_SOURCE_DIR}/part*.c)
            list(SORT parts)
            add_library(parts STATIC ${parts})
            target_include_directories(parts PUBLIC ${CMAKE_BINARY_DIR}/gen)
            add_dependencies(parts gen_headers)
            add_executable(genapp main.c ${CMAKE_BINARY_DIR}/gen/table.h)
            target_link_libraries(genapp PRIVATE parts)
            set_source_files_properties(main.c PROPERTIES OBJECT_DEPENDS ${CMAKE_SOURCE_DIR}/extra.dep)
            """,
        "config.h.in": "#define CONFIG_VALUE @CONFIG_NUMBER@\n#define CONFIG_TAG \"one\"\n",
        "table.def": "1;2;3\n",
        "extra.dep": "first\n",
        "gen_table.cmake": """
            file(READ "${IN}" content)
            string(STRIP "${content}" content)
            set(sum 0)
            foreach(n IN LISTS content)
              math(EXPR sum "${sum} + ${n}")
            endforeach()
            file(WRITE "${OUT}.tmp" "#define TABLE_SUM ${sum}\\n")
            file(COPY_FILE "${OUT}.tmp" "${OUT}" ONLY_IF_DIFFERENT)
            """,
        "main.c": """
            #include <stdio.h>
            #include "config.h"
            #include "table.h"
            int parts_total(void);
            int main(void) {
              printf("config=%d tag=%s table=%d parts=%d\\n", CONFIG_VALUE, CONFIG_TAG, TABLE_SUM, parts_total());
              return 0;
            }
            """,
    }
    for i in range(8):
        files[f"part{i}.c"] = (
            f'#include "table.h"\nint part_{i}(void) {{ return TABLE_SUM + {i}; }}\n'
            + ("" if i else "".join(f"int part_{j}(void);\n" for j in range(1, 8))
               + "int parts_total(void) { return part_0()"
               + "".join(f" + part_{j}()" for j in range(1, 8)) + "; }\n")
        )
    for gen in [g for g in ("Ninja", "Unix Makefiles") if g in generators_available(ctx.options)]:
        src = ctx.tree(ctx.root / f"src-{slug(gen)}", files)
        build = ctx.root / f"b-{slug(gen)}"
        try:
            generated_flow(ctx, gen, src, build)
        except StepFailed:
            continue


def generated_flow(ctx: Ctx, gen: str, src: Path, build: Path) -> None:
    """Run the generated header flow with one generator."""
    ctx.ok(f"{gen}: configure", ctx.configure(src, build, gen, "-DCONFIG_NUMBER=1"))
    ctx.ok(f"{gen}: parallel -j4 clean build", ctx.cmake("--build", build, "-j", "4"))
    app = build / "genapp"
    outputs = []

    def expect(label: str, text: str) -> None:
        run = ctx.run([app])
        outputs.append(run.stdout)
        ctx.check(f"{gen}: {label}", run.stdout == text, run.tail())

    expect("first output", "config=1 tag=one table=6 parts=76\n")
    touch_later(src / "config.h.in", "#define CONFIG_VALUE @CONFIG_NUMBER@\n#define CONFIG_TAG \"two\"\n")
    step = ctx.cmake("--build", build, "-j", "4")
    ctx.ok(f"{gen}: rebuild after config.h.in edit", step)
    ctx.check(f"{gen}: config.h regenerated", '"two"' in (build / "gen/config.h").read_text())
    ctx.check(f"{gen}: config.h.in edit reruns CMake", "Configuring done" in step.output, step.tail())
    expect("output after config.h.in edit", "config=1 tag=two table=6 parts=76\n")
    touch_later(src / "table.def", "1;2;3;4\n")
    step = ctx.cmake("--build", build, "-j", "4")
    ctx.ok(f"{gen}: rebuild after table.def edit", step)
    ctx.check(f"{gen}: MAIN_DEPENDENCY edit regenerates table.h", "Generating table.h" in step.output, step.tail())
    expect("output after table.def edit", "config=1 tag=two table=10 parts=108\n")
    touch_later(src / "extra.dep", "second\n")
    step = ctx.cmake("--build", build, "-j", "4")
    ctx.ok(f"{gen}: rebuild after OBJECT_DEPENDS edit", step)
    ctx.check(f"{gen}: OBJECT_DEPENDS edit recompiles main.c", compiled(step, r"main\.c\.o"), step.tail())
    ctx.check(f"{gen}: OBJECT_DEPENDS edit does not recompile parts", not compiled(step, r"part\d\.c\.o"), step.tail())
    step = ctx.cmake("--build", build, "-j", "4")
    ctx.check(f"{gen}: final build is a no-op", no_op(step), step.tail())
    ctx.observe(f"{gen}: outputs", outputs)


UNITY_FILES = {
    "CMakeLists.txt": """
        cmake_minimum_required(VERSION 4.0)
        project(un C)
        add_subdirectory(lib)
        add_executable(unapp main.c)
        target_link_libraries(unapp PRIVATE unilib grouped)
        """,
    "lib/CMakeLists.txt": """
        add_library(unilib STATIC a.c b.c c.c d.c e.c)
        set_target_properties(unilib PROPERTIES UNITY_BUILD ON UNITY_BUILD_BATCH_SIZE 2
          UNITY_BUILD_RELOCATABLE TRUE UNITY_BUILD_UNIQUE_ID UNITY_ID)
        set_source_files_properties(e.c PROPERTIES SKIP_UNITY_BUILD_INCLUSION ON)
        add_library(grouped STATIC g1.c g2.c g3.c)
        set_target_properties(grouped PROPERTIES UNITY_BUILD ON UNITY_BUILD_MODE GROUP
          UNITY_BUILD_RELOCATABLE TRUE)
        set_source_files_properties(g1.c g2.c PROPERTIES UNITY_GROUP first)
        set_source_files_properties(g3.c PROPERTIES UNITY_GROUP second)
        """,
    # a.c and e.c both define helper(). e.c is not in a unity file, so they do not collide.
    "lib/a.c": "static int helper(void) { return 1; }\nint fa(void) { return helper(); }\n",
    "lib/b.c": "int fb(void) { return 2; }\n",
    "lib/c.c": "static int only_c(void) { return 3; }\nint fc(void) { return only_c(); }\n",
    "lib/d.c": "int fd(void) { return 4; }\n",
    "lib/e.c": "static int helper(void) { return 5; }\nint fe(void) { return helper(); }\n",
    "lib/g1.c": "int g1(void) { return 10; }\n",
    "lib/g2.c": "int g2(void) { return 20; }\n",
    "lib/g3.c": "int g3(void) { return 30; }\n",
    "main.c": """
        #include <stdio.h>
        int fa(void); int fb(void); int fc(void); int fd(void); int fe(void);
        int g1(void); int g2(void); int g3(void);
        int main(void) {
          printf("unity %d %d\\n", fa() + fb() + fc() + fd() + fe(), g1() + g2() + g3());
          return 0;
        }
        """,
}

FORTRAN_FILES = {
    "CMakeLists.txt": """
        cmake_minimum_required(VERSION 4.0)
        project(fmod Fortran)
        add_library(fmods STATIC mod_c.f90 mod_b.f90 mod_a.f90)
        add_executable(fapp main.f90)
        target_link_libraries(fapp PRIVATE fmods)
        """,
    "mod_a.f90": "module mod_a\n  implicit none\n  integer, parameter :: base = 40\nend module mod_a\n",
    "mod_b.f90": ("module mod_b\n  use mod_a\n  implicit none\ncontains\n  integer function plus_one()\n"
                  "    plus_one = base + 1\n  end function plus_one\nend module mod_b\n"),
    "mod_c.f90": ("module mod_c\n  use mod_b\n  implicit none\ncontains\n  integer function answer()\n"
                  "    answer = plus_one() + 1\n  end function answer\nend module mod_c\n"),
    "main.f90": "program fapp\n  use mod_c\n  implicit none\n  print '(A,I0)', 'fortran ', answer()\nend program fapp\n",
}


def unity_sources(build: Path) -> dict[str, str]:
    """Return the generated unity source files of a build tree."""
    return {
        p.relative_to(build).as_posix(): p.read_text()
        for p in sorted(build.rglob("Unity/unity_*"))
        if p.is_file() and p.suffix in (".c", ".cxx")
    }


@scenario("s13", "Fortran module graph (if gfortran) + UNITY_BUILD batch/group + relocatable unity subdirectory")
def s13_unity_fortran(ctx: Ctx) -> None:
    gens = [g for g in ("Ninja", "Unix Makefiles") if g in generators_available(ctx.options)]
    for gen in gens:
        tag = slug(gen)
        first: dict[str, str] | None = None
        for place in ("src-one", "relocated source dir"):
            src = ctx.tree(ctx.root / tag / place, UNITY_FILES)
            build = ctx.root / tag / f"build {place}"
            try:
                ctx.ok(f"{gen} [{place}]: configure", ctx.configure(src, build, gen))
                ctx.ok(f"{gen} [{place}]: build", ctx.build(build, gen))
            except StepFailed:
                break
            run = ctx.run([build / "unapp"])
            ctx.check(f"{gen} [{place}]: output", run.stdout == "unity 15 60\n", run.tail())
            unity = unity_sources(build)
            names = sorted(Path(name).name for name in unity)
            ctx.check(f"{gen} [{place}]: unity files", names == [
                "unity_0_c.c", "unity_1_c.c", "unity_first_c.c", "unity_second_c.c"], names)
            text = "".join(unity.values())
            ctx.check(f"{gen} [{place}]: relocatable unity files have no absolute paths",
                      str(ctx.root) not in text, text)
            ctx.check(f"{gen} [{place}]: SKIP_UNITY_BUILD_INCLUSION source is not included",
                      "e.c" not in text and "a.c" in text, text)
            ctx.check(f"{gen} [{place}]: UNITY_BUILD_UNIQUE_ID", "#define UNITY_ID" in text, text)
            if first is None:
                first = unity
                ctx.observe(f"{gen}: unity sources", unity)
            else:
                ctx.check(f"{gen}: unity sources are identical after relocation", unity == first,
                          json.dumps(unity)[:2000])
    if not shutil.which("gfortran"):
        ctx.skip("gfortran is not installed: Fortran module graph not tested")
        return
    for gen in gens:
        src = ctx.tree(ctx.root / f"fortran-{slug(gen)}", FORTRAN_FILES)
        build = ctx.root / f"fortran-build-{slug(gen)}"
        try:
            ctx.ok(f"Fortran {gen}: configure", ctx.configure(src, build, gen))
            ctx.ok(f"Fortran {gen}: build -j4", ctx.cmake("--build", build, "-j", "4"))
        except StepFailed:
            continue
        run = ctx.run([build / "fapp"])
        ctx.check(f"Fortran {gen}: output", run.stdout.strip() == "fortran 42", run.tail())
        touch_later(src / "mod_a.f90", FORTRAN_FILES["mod_a.f90"].replace("40", "50"))
        ctx.ok(f"Fortran {gen}: rebuild after module edit", ctx.cmake("--build", build, "-j", "4"), required=False)
        run = ctx.run([build / "fapp"])
        ctx.check(f"Fortran {gen}: module edit propagates", run.stdout.strip() == "fortran 52", run.tail())
        ctx.observe(f"Fortran {gen}: modules", sorted(p.name for p in build.rglob("*.mod")))


PKG_PROJECT = {
    "CMakeLists.txt": """
        cmake_minimum_required(VERSION 3.28)
        project(Pkg VERSION 2.3.4 LANGUAGES CXX)
        include(GNUInstallDirs)
        include(CMakePackageConfigHelpers)
        set(CMAKE_CXX_STANDARD 17)
        set(CMAKE_CXX_VISIBILITY_PRESET hidden)
        set(CMAKE_VISIBILITY_INLINES_HIDDEN ON)
        set(CMAKE_POSITION_INDEPENDENT_CODE ON)
        if(APPLE)
          set(pkg_rpath "@loader_path/../${CMAKE_INSTALL_LIBDIR}")
        else()
          set(pkg_rpath "$ORIGIN/../${CMAKE_INSTALL_LIBDIR}")
        endif()
        add_subdirectory(iface)
        add_subdirectory(core)
        add_subdirectory(shared)
        add_subdirectory(app)
        set(cfgdir ${CMAKE_INSTALL_LIBDIR}/cmake/Pkg)
        install(EXPORT PkgTargets NAMESPACE Pkg:: DESTINATION ${cfgdir})
        configure_package_config_file(cmake/PkgConfig.cmake.in ${CMAKE_CURRENT_BINARY_DIR}/PkgConfig.cmake
          INSTALL_DESTINATION ${cfgdir} PATH_VARS CMAKE_INSTALL_INCLUDEDIR)
        write_basic_package_version_file(${CMAKE_CURRENT_BINARY_DIR}/PkgConfigVersion.cmake
          VERSION ${PROJECT_VERSION} COMPATIBILITY SameMajorVersion)
        install(FILES ${CMAKE_CURRENT_BINARY_DIR}/PkgConfig.cmake ${CMAKE_CURRENT_BINARY_DIR}/PkgConfigVersion.cmake
          DESTINATION ${cfgdir})
        """,
    "cmake/PkgConfig.cmake.in": """
        @PACKAGE_INIT@
        include("${CMAKE_CURRENT_LIST_DIR}/PkgTargets.cmake")
        set_and_check(Pkg_INCLUDE_DIR "@PACKAGE_CMAKE_INSTALL_INCLUDEDIR@")
        check_required_components(Pkg)
        """,
    "iface/CMakeLists.txt": """
        add_library(pkg_headers INTERFACE)
        target_include_directories(pkg_headers INTERFACE
          $<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/include> $<INSTALL_INTERFACE:${CMAKE_INSTALL_INCLUDEDIR}>)
        target_compile_definitions(pkg_headers INTERFACE PKG_HEADER_LEVEL=1)
        target_compile_features(pkg_headers INTERFACE cxx_std_17)
        install(TARGETS pkg_headers EXPORT PkgTargets)
        install(DIRECTORY include/ DESTINATION ${CMAKE_INSTALL_INCLUDEDIR})
        """,
    "iface/include/pkg/config.hpp": """
        #pragma once
        inline int pkg_header_value() { return 100 * PKG_HEADER_LEVEL; }
        """,
    "core/CMakeLists.txt": """
        add_library(pkg_obj OBJECT obj.cpp)
        target_include_directories(pkg_obj PUBLIC $<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/include>)
        target_link_libraries(pkg_obj PUBLIC pkg_headers)
        add_library(pkg_static STATIC static.cpp)
        target_include_directories(pkg_static PUBLIC $<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/include>)
        target_link_libraries(pkg_static PUBLIC pkg_headers PRIVATE pkg_obj)
        install(TARGETS pkg_obj pkg_static EXPORT PkgTargets
          ARCHIVE DESTINATION ${CMAKE_INSTALL_LIBDIR} OBJECTS DESTINATION ${CMAKE_INSTALL_LIBDIR}/objects)
        install(DIRECTORY include/ DESTINATION ${CMAKE_INSTALL_INCLUDEDIR})
        """,
    "core/include/pkg/core.hpp": """
        #pragma once
        extern "C" int pkg_obj_value(void);
        extern "C" int pkg_static_value(void);
        """,
    "core/obj.cpp": '#include "pkg/core.hpp"\nextern "C" int pkg_obj_value(void) { return 10; }\n',
    "core/static.cpp": '#include "pkg/core.hpp"\nextern "C" int pkg_static_value(void) { return pkg_obj_value() + 20; }\n',
    "shared/CMakeLists.txt": """
        include(GenerateExportHeader)
        add_library(pkg_shared SHARED shared.cpp)
        generate_export_header(pkg_shared BASE_NAME PKG
          EXPORT_FILE_NAME ${CMAKE_CURRENT_BINARY_DIR}/include/pkg/pkg_export.h)
        target_include_directories(pkg_shared PUBLIC
          $<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/include>
          $<BUILD_INTERFACE:${CMAKE_CURRENT_BINARY_DIR}/include>
          $<INSTALL_INTERFACE:${CMAKE_INSTALL_INCLUDEDIR}>)
        target_link_libraries(pkg_shared PUBLIC pkg_headers PRIVATE pkg_static)
        set_target_properties(pkg_shared PROPERTIES VERSION ${PROJECT_VERSION} SOVERSION ${PROJECT_VERSION_MAJOR})
        install(TARGETS pkg_shared EXPORT PkgTargets
          LIBRARY DESTINATION ${CMAKE_INSTALL_LIBDIR} RUNTIME DESTINATION ${CMAKE_INSTALL_BINDIR})
        install(DIRECTORY include/ ${CMAKE_CURRENT_BINARY_DIR}/include/ DESTINATION ${CMAKE_INSTALL_INCLUDEDIR})
        """,
    "shared/include/pkg/shared.hpp": """
        #pragma once
        #include "pkg/config.hpp"
        #include "pkg/pkg_export.h"
        extern "C" PKG_EXPORT int pkg_shared_value(void);
        extern "C" PKG_DEPRECATED_EXPORT int pkg_shared_legacy(void);
        inline int pkg_inline_twice(int x) { return 2 * x; }
        """,
    "shared/shared.cpp": """
        #include "pkg/shared.hpp"
        #include "pkg/core.hpp"
        namespace { int hidden_helper() { return 1; } }
        extern "C" int pkg_internal_hidden(void) { return pkg_inline_twice(1); }
        extern "C" int pkg_shared_value(void) { return pkg_static_value() + hidden_helper() + pkg_internal_hidden(); }
        extern "C" int pkg_shared_legacy(void) { return 0; }
        """,
    "app/CMakeLists.txt": """
        add_executable(pkg_app main.cpp)
        target_link_libraries(pkg_app PRIVATE pkg_shared)
        set_target_properties(pkg_app PROPERTIES INSTALL_RPATH "${pkg_rpath}")
        install(TARGETS pkg_app EXPORT PkgTargets RUNTIME DESTINATION ${CMAKE_INSTALL_BINDIR})
        """,
    "app/main.cpp": """
        #include <cstdio>
        #include "pkg/shared.hpp"
        int main() { std::printf("pkg %d %d\\n", pkg_header_value(), pkg_shared_value()); return pkg_shared_value() == 33 ? 0 : 1; }
        """,
}
PKG_CONSUMER = {
    "CMakeLists.txt": """
        cmake_minimum_required(VERSION 3.28)
        project(PkgUse CXX)
        foreach(want 3.0 2.9 1.0)
          find_package(Pkg ${want} CONFIG QUIET)
          message(STATUS "PROBE ${want} found=[${Pkg_FOUND}] considered=[${Pkg_CONSIDERED_VERSIONS}]")
          unset(Pkg_DIR CACHE)
        endforeach()
        find_package(Pkg 2.3 CONFIG REQUIRED)
        message(STATUS "FOUND ${Pkg_VERSION} major=${Pkg_VERSION_MAJOR} include=${Pkg_INCLUDE_DIR}")
        foreach(t Pkg::pkg_headers Pkg::pkg_obj Pkg::pkg_static Pkg::pkg_shared Pkg::pkg_app)
          get_target_property(type ${t} TYPE)
          get_target_property(links ${t} INTERFACE_LINK_LIBRARIES)
          get_target_property(defs ${t} INTERFACE_COMPILE_DEFINITIONS)
          message(STATUS "TARGET ${t} ${type} links=${links} defs=${defs}")
        endforeach()
        add_executable(use use.cpp)
        target_link_libraries(use PRIVATE Pkg::pkg_shared)
        add_custom_target(run_pkg_app ALL COMMAND Pkg::pkg_app VERBATIM)
        """,
    "use.cpp": """
        #include <cstdio>
        #include "pkg/shared.hpp"
        int main() { std::printf("use %d %d %d\\n", pkg_header_value(), pkg_shared_value(), pkg_inline_twice(4)); return 0; }
        """,
}


def pkg_library_files() -> dict[str, str]:
    """Return the expected install entries of the versioned shared library."""
    if context.DARWIN:
        return {"lib/libpkg_shared.2.3.4.dylib": "", "lib/libpkg_shared.2.dylib": "libpkg_shared.2.3.4.dylib",
                "lib/libpkg_shared.dylib": "libpkg_shared.2.dylib"}
    return {"lib/libpkg_shared.so.2.3.4": "", "lib/libpkg_shared.so.2": "libpkg_shared.so.2.3.4",
            "lib/libpkg_shared.so": "libpkg_shared.so.2"}


def pkg_flow(ctx: Ctx, gen: str, src: Path, consumer: Path) -> None:
    """Build, install, inspect, and consume the Pkg project with one generator."""
    tag = slug(gen)
    build, prefix, cbuild = ctx.root / f"b-{tag}", ctx.root / f"inst-{tag}", ctx.root / f"c-{tag}"
    ctx.ok(f"{gen}: configure", ctx.configure(src, build, gen, f"-DCMAKE_INSTALL_PREFIX={prefix}",
                                              "-DCMAKE_INSTALL_LIBDIR=lib"))
    ctx.ok(f"{gen}: build", ctx.build(build, gen))
    run = ctx.run([build / "app/pkg_app"])
    ctx.check(f"{gen}: build-tree app runs", run.ok and run.stdout == "pkg 100 33\n", run.tail())
    ctx.ok(f"{gen}: install", ctx.cmake("--install", build))

    listing = file_listing(prefix)
    links = {item.split(" -> ")[0]: (item.split(" -> ")[1] if " -> " in item else "") for item in listing}
    for rel, target in pkg_library_files().items():
        ctx.check(f"{gen}: installs {rel}", links.get(rel) == target, listing)
    for rel in ("bin/pkg_app", "include/pkg/config.hpp", "include/pkg/core.hpp", "include/pkg/pkg_export.h",
                "include/pkg/shared.hpp", "lib/libpkg_static.a", "lib/cmake/Pkg/PkgConfig.cmake",
                "lib/cmake/Pkg/PkgConfigVersion.cmake", "lib/cmake/Pkg/PkgTargets.cmake",
                "lib/cmake/Pkg/PkgTargets-release.cmake"):
        ctx.check(f"{gen}: installs {rel}", rel in links, listing)
    ctx.check(f"{gen}: installs the object file", any(r.startswith("lib/objects/") for r in links), listing)
    ctx.observe(f"{gen}: install tree", listing)

    targets = (prefix / "lib/cmake/Pkg/PkgTargets.cmake").read_text(encoding="utf-8") \
        if (prefix / "lib/cmake/Pkg/PkgTargets.cmake").is_file() else ""
    imported = re.findall(r"add_(?:library|executable)\((Pkg::\w+) (\w+(?: \w+)?)\)", targets)
    ctx.check(f"{gen}: export defines every target with the namespace", sorted(imported) == [
        ("Pkg::pkg_app", "IMPORTED"), ("Pkg::pkg_headers", "INTERFACE IMPORTED"),
        ("Pkg::pkg_obj", "OBJECT IMPORTED"), ("Pkg::pkg_shared", "SHARED IMPORTED"),
        ("Pkg::pkg_static", "STATIC IMPORTED")], imported)
    ctx.check(f"{gen}: private OBJECT dependency of the static library is LINK_ONLY",
              'INTERFACE_LINK_LIBRARIES "Pkg::pkg_headers;\\$<LINK_ONLY:Pkg::pkg_obj>"' in targets, targets[:4000])
    ctx.check(f"{gen}: private static dependency of the shared library is not exported",
              "LINK_ONLY:Pkg::pkg_static" not in targets, targets[:4000])
    ctx.observe(f"{gen}: exported targets", sorted(imported))
    export_h = (prefix / "include/pkg/pkg_export.h").read_text(encoding="utf-8") \
        if (prefix / "include/pkg/pkg_export.h").is_file() else ""
    ctx.check(f"{gen}: GenerateExportHeader visibility macros",
              '__attribute__((visibility("default")))' in export_h and "PKG_DEPRECATED_EXPORT" in export_h,
              export_h[:2000])

    real = next(rel for rel, target in pkg_library_files().items() if not target)
    symbols = [s for s in exported_symbols(prefix / real) if s.startswith("pkg")]
    ctx.check(f"{gen}: hidden visibility exports only the PKG_EXPORT symbols",
              symbols == ["pkg_shared_legacy", "pkg_shared_value"], symbols)
    ctx.observe(f"{gen}: exported symbols", symbols)
    installed = ctx.run([prefix / "bin/pkg_app"])
    ctx.check(f"{gen}: installed app runs", installed.ok and installed.stdout == "pkg 100 33\n", installed.tail())

    if context.DARWIN:
        lib = prefix / real
        facts = {
            "id": macho_id(lib),
            "app rpaths": macho_rpaths(prefix / "bin/pkg_app"),
            "app loads": macho_load_dylibs(prefix / "bin/pkg_app"),
            "versions": macho_versions(prefix / "bin/pkg_app", "@rpath/libpkg_shared.2.dylib"),
            "lib rpaths": macho_rpaths(lib),
        }
        ctx.check(f"{gen}: install name", facts["id"] == "@rpath/libpkg_shared.2.dylib", facts)
        ctx.check(f"{gen}: installed app RPATH", facts["app rpaths"] == ["@loader_path/../lib"], facts)
        ctx.check(f"{gen}: installed app loads the SOVERSION name",
                  facts["app loads"] == ["@rpath/libpkg_shared.2.dylib"], facts)
        ctx.check(f"{gen}: Mach-O compatibility and current version",
                  facts["versions"] == "compatibility version 2.0.0, current version 2.3.4", facts)
        build_rpaths = macho_rpaths(build / "app/pkg_app")
        ctx.check(f"{gen}: build-tree RPATH points at the build tree", build_rpaths == [f"{build}/shared"],
                  build_rpaths)
        facts["build rpaths"] = build_rpaths
        ctx.observe(f"{gen}: Mach-O facts", facts)
    elif shutil.which("readelf"):
        facts = {"lib": elf_dynamic(prefix / real), "app": elf_dynamic(prefix / "bin/pkg_app")}
        ctx.check(f"{gen}: SONAME", facts["lib"].get("SONAME") == ["libpkg_shared.so.2"], facts)
        ctx.check(f"{gen}: installed app RUNPATH",
                  (facts["app"].get("RUNPATH") or facts["app"].get("RPATH")) == ["$ORIGIN/../lib"], facts)
        ctx.check(f"{gen}: installed app needs the SOVERSION name",
                  "libpkg_shared.so.2" in facts["app"].get("NEEDED", []), facts)
        ctx.observe(f"{gen}: ELF facts", facts)
    else:
        ctx.skip(f"{gen}: readelf not found; ELF dynamic section not inspected")

    conf = ctx.configure(consumer, cbuild, gen, f"-DCMAKE_PREFIX_PATH={prefix}")
    ctx.ok(f"{gen}: consumer configure", conf)
    probes = message_lines(conf.stdout, "-- PROBE", "-- FOUND", "-- TARGET")
    ctx.check(f"{gen}: SameMajorVersion rejects 3.0, 2.9, and 1.0", probes[:3] == [
        f"-- PROBE {want} found=[0] considered=[2.3.4]" for want in ("3.0", "2.9", "1.0")], probes)
    ctx.check(f"{gen}: find_package(Pkg 2.3 CONFIG REQUIRED)",
              f"-- FOUND 2.3.4 major=2 include={prefix}/include" in probes, probes)
    ctx.check(f"{gen}: imported target types", [p.split(" links=")[0] for p in probes if p.startswith("-- TARGET")] == [
        "-- TARGET Pkg::pkg_headers INTERFACE_LIBRARY", "-- TARGET Pkg::pkg_obj OBJECT_LIBRARY",
        "-- TARGET Pkg::pkg_static STATIC_LIBRARY", "-- TARGET Pkg::pkg_shared SHARED_LIBRARY",
        "-- TARGET Pkg::pkg_app EXECUTABLE"], probes)
    ctx.observe(f"{gen}: consumer messages", probes)
    cbuilt = ctx.build(cbuild, gen)
    ctx.ok(f"{gen}: consumer build", cbuilt)
    ctx.check(f"{gen}: imported executable runs in a custom target", "pkg 100 33" in cbuilt.stdout, cbuilt.tail())
    use = ctx.run([cbuild / "use"])
    ctx.check(f"{gen}: consumer runs", use.ok and use.stdout == "use 100 33 8\n", use.tail())
    if context.DARWIN:
        crpaths = macho_rpaths(cbuild / "use")
        ctx.check(f"{gen}: consumer RPATH points at the installed library", crpaths == [f"{prefix}/lib"], crpaths)
        ctx.observe(f"{gen}: consumer rpaths", crpaths)
    ctx.observe(f"{gen}: outputs", [run.stdout, installed.stdout, use.stdout])


@scenario("s15", "Install/export diamond: INTERFACE+OBJECT+STATIC+versioned SHARED, GenerateExportHeader, hidden inlines, PackageConfigHelpers, RPATH, consumer")
def s15_package_diamond(ctx: Ctx) -> None:
    gens = [g for g in ("Ninja", "Unix Makefiles") if g in generators_available(ctx.options)]
    if not gens:
        raise ScenarioSkip("needs Ninja or Unix Makefiles")
    src = ctx.tree(ctx.root / "src", PKG_PROJECT)
    consumer = ctx.tree(ctx.root / "consumer", PKG_CONSUMER)
    for gen in gens:
        try:
            pkg_flow(ctx, gen, src, consumer)
        except StepFailed:
            continue


MODULE_LIB = """
    cmake_minimum_required(VERSION 3.28)
    project(modfs CXX)
    add_library(mods g.cpp)
    target_sources(mods
      PUBLIC FILE_SET HEADERS BASE_DIRS inc FILES inc/mods/h.hpp
      PUBLIC FILE_SET CXX_MODULES FILES m.cppm)
    {feature}
    """
MODULE_FILES = {
    "inc/mods/h.hpp": "int h();\n",
    "m.cppm": "export module m;\nexport int f() { return 1; }\n",
    "g.cpp": "int g() { return 2; }\n",
}
FILE_SET_ERRORS = {
    "interface-modules": """
        cmake_minimum_required(VERSION 3.28)
        project(e CXX)
        add_library(mods INTERFACE)
        target_sources(mods INTERFACE FILE_SET CXX_MODULES FILES m.cppm)
        """,
    "no-type": """
        cmake_minimum_required(VERSION 3.28)
        project(e CXX)
        add_library(mods g.cpp)
        target_sources(mods PUBLIC FILE_SET extra FILES m.cppm)
        """,
    "outside-base": """
        cmake_minimum_required(VERSION 3.28)
        project(e CXX)
        add_library(mods g.cpp)
        target_sources(mods PUBLIC FILE_SET HEADERS BASE_DIRS inc FILES g.cpp)
        """,
    "install-without-file-set": """
        cmake_minimum_required(VERSION 3.28)
        project(e CXX)
        add_library(mods g.cpp)
        target_sources(mods PUBLIC FILE_SET HEADERS BASE_DIRS inc FILES inc/mods/h.hpp)
        install(TARGETS mods EXPORT E)
        install(EXPORT E DESTINATION lib/cmake/E)
        """,
}
FILE_SET_ERROR_TEXT = {
    "interface-modules": 'target_sources File set TYPE "CXX_MODULES" may not have "INTERFACE" scope.',
    "no-type": "target_sources Must specify a TYPE when creating file set.",
    "outside-base": "must be in one of the file set's base directories:",
    "install-without-file-set": "install TARGETS target mods is exported but not all of its interface file sets are installed",
}
NO_MODULE_GENERATOR = (
    'The target named "mods" has C++ sources that may use modules, but modules are not supported by this '
    "generator: Unix Makefiles Modules are supported only by Ninja, Ninja Multi-Config, and Visual Studio "
    "generators for VS 17.4 and newer."
)
NO_CXX20 = (
    'The target named "mods" has C++ sources that use modules, but does not include "cxx_std_20" (or newer) '
    "among its `target_compile_features`; no C++ standard found."
)
NO_SCANNER = (
    'has C++ sources that may use modules, but the compiler does not provide a way to discover the import '
    "graph dependencies."
)
HEADER_SET_PROJECT = {
    "CMakeLists.txt": """
        cmake_minimum_required(VERSION 3.28)
        project(fsh VERSION 1.0 LANGUAGES CXX)
        set(CMAKE_VERIFY_INTERFACE_HEADER_SETS ON)
        configure_file(version.hpp.in ${CMAKE_CURRENT_BINARY_DIR}/gen/fsh/version.hpp @ONLY)
        add_library(fsh STATIC fsh.cpp)
        target_sources(fsh PUBLIC FILE_SET HEADERS
          BASE_DIRS include ${CMAKE_CURRENT_BINARY_DIR}/gen
          FILES include/fsh/api.hpp include/fsh/detail/impl.hpp ${CMAKE_CURRENT_BINARY_DIR}/gen/fsh/version.hpp)
        target_sources(fsh PRIVATE FILE_SET internal TYPE HEADERS BASE_DIRS src FILES src/private.hpp)
        target_compile_features(fsh PUBLIC cxx_std_17)
        get_target_property(sets fsh HEADER_SETS)
        get_target_property(isets fsh INTERFACE_HEADER_SETS)
        get_target_property(dirs fsh HEADER_DIRS)
        message(STATUS "SETS ${sets} | ${isets}")
        list(TRANSFORM dirs REPLACE "^${CMAKE_CURRENT_SOURCE_DIR}" "<src>")
        list(TRANSFORM dirs REPLACE "^${CMAKE_CURRENT_BINARY_DIR}" "<bin>")
        message(STATUS "DIRS ${dirs}")
        install(TARGETS fsh EXPORT fshTargets ARCHIVE DESTINATION lib
          FILE_SET HEADERS DESTINATION include/fsh-1)
        install(EXPORT fshTargets NAMESPACE fsh:: DESTINATION lib/cmake/fsh FILE fshConfig.cmake)
        """,
    "version.hpp.in": "#pragma once\n#define FSH_VERSION \"@PROJECT_VERSION@\"\n",
    "include/fsh/api.hpp": """
        #pragma once
        #include "fsh/detail/impl.hpp"
        #include "fsh/version.hpp"
        int fsh_value();
        """,
    "include/fsh/detail/impl.hpp": "#pragma once\ninline int fsh_detail() { return 4; }\n",
    "src/private.hpp": "#pragma once\ninline int fsh_private() { return 10; }\n",
    "fsh.cpp": '#include "fsh/api.hpp"\n#include "private.hpp"\nint fsh_value() { return fsh_private() + fsh_detail(); }\n',
}
HEADER_SET_CONSUMER = {
    "CMakeLists.txt": """
        cmake_minimum_required(VERSION 3.28)
        project(fshuse CXX)
        find_package(fsh CONFIG REQUIRED)
        get_target_property(dirs fsh::fsh INTERFACE_INCLUDE_DIRECTORIES)
        message(STATUS "IMPORTED ${dirs}")
        add_executable(use use.cpp)
        target_link_libraries(use PRIVATE fsh::fsh)
        """,
    "use.cpp": """
        #include <cstdio>
        #include "fsh/api.hpp"
        int main() { std::printf("fsh %s %d\\n", FSH_VERSION, fsh_value()); return fsh_value() == 14 ? 0 : 1; }
        """,
}
MODULE_BUILD = {
    "CMakeLists.txt": """
        cmake_minimum_required(VERSION 3.28)
        project(modbuild CXX)
        add_library(greet)
        target_sources(greet PUBLIC FILE_SET CXX_MODULES FILES greet.cppm)
        target_compile_features(greet PUBLIC cxx_std_20)
        add_executable(hello main.cpp)
        target_link_libraries(hello PRIVATE greet)
        """,
    "greet.cppm": "export module greet;\nexport int greet_value() { return 20; }\n",
    "main.cpp": '#include <cstdio>\nimport greet;\nint main() { std::printf("module %d\\n", greet_value()); return 0; }\n',
}


def _check_module_diagnostics(ctx: Ctx) -> None:
    r = ctx.root
    cases = (("Unix Makefiles", "target_compile_features(mods PUBLIC cxx_std_20)", NO_MODULE_GENERATOR),
             ("Unix Makefiles", "", NO_MODULE_GENERATOR), ("Ninja", "", NO_CXX20))
    for gen, feature, text in cases:
        tag = f"{slug(gen)}-{'std20' if feature else 'nostd'}"
        src = ctx.tree(r / f"m-{tag}", MODULE_FILES)
        ctx.write("CMakeLists.txt", MODULE_LIB.replace("{feature}", feature), src)
        proc = ctx.configure(src, r / f"mb-{tag}", gen)
        blocks = cmake_errors(proc.output)
        ctx.check(f"{tag}: configure fails", proc.returncode != 0, proc.tail())
        ctx.check(f"{tag}: exact diagnostic", any(text in block for block in blocks), blocks)
        ctx.check(f"{tag}: one module diagnostic only", len(blocks) == 1, blocks)
        ctx.check(f"{tag}: generate step failed", "CMake Generate step failed." in proc.output, proc.tail())
        ctx.observe(f"modules {tag}", [proc.returncode, blocks])

    for name, body in FILE_SET_ERRORS.items():
        src = ctx.tree(r / f"e-{name}", MODULE_FILES)
        ctx.write("CMakeLists.txt", body, src)
        proc = ctx.configure(src, r / f"eb-{name}", "Ninja")
        blocks = cmake_errors(proc.output)
        ctx.check(f"{name}: configure fails", proc.returncode != 0, proc.tail())
        ctx.check(f"{name}: diagnostic", any(FILE_SET_ERROR_TEXT[name] in block for block in blocks), blocks)
        ctx.observe(f"file set error {name}", [proc.returncode, collapse_repeats(blocks)])


def _check_header_sets(ctx: Ctx) -> None:
    r = ctx.root
    src = ctx.tree(r / "fsh", HEADER_SET_PROJECT)
    consumer = ctx.tree(r / "fshuse", HEADER_SET_CONSUMER)
    for gen in ("Ninja", "Unix Makefiles"):
        tag = slug(gen)
        build, prefix = r / f"fb-{tag}", r / f"fi-{tag}"
        conf = ctx.configure(src, build, gen, f"-DCMAKE_INSTALL_PREFIX={prefix}")
        ctx.ok(f"{gen}: header set configure", conf)
        status = message_lines(conf.stdout, "-- SETS", "-- DIRS")
        ctx.check(f"{gen}: HEADER_SETS and HEADER_DIRS", status == [
            "-- SETS HEADERS;internal | HEADERS", "-- DIRS <src>/include;<bin>/gen"], status)
        ctx.observe(f"{gen}: header set properties", status)
        ctx.ok(f"{gen}: header set build", ctx.build(build, gen))
        verify = ctx.build(build, gen, "--target", "all_verify_interface_header_sets")
        ctx.check(f"{gen}: all_verify_interface_header_sets", verify.ok, verify.tail())
        ctx.ok(f"{gen}: header set install", ctx.cmake("--install", build))
        listing = file_listing(prefix)
        ctx.check(f"{gen}: headers keep their base-relative paths", [
            item for item in listing if item.startswith("include/")] == [
            "include/fsh-1/fsh/api.hpp", "include/fsh-1/fsh/detail/impl.hpp", "include/fsh-1/fsh/version.hpp"],
            listing)
        ctx.observe(f"{gen}: header set install tree", listing)
        exported = (prefix / "lib/cmake/fsh/fshConfig.cmake").read_text(encoding="utf-8") \
            if (prefix / "lib/cmake/fsh/fshConfig.cmake").is_file() else ""
        sets = [" ".join(line.split()) for line in exported.splitlines() if "FILE_SET" in line]
        ctx.check(f"{gen}: export records the HEADERS file set",
                  any('FILE_SET "HEADERS"' in line for line in sets), exported[:3000])
        ctx.observe(f"{gen}: exported file set lines", sets)
        cbuild = r / f"fc-{tag}"
        cconf = ctx.configure(consumer, cbuild, gen, f"-DCMAKE_PREFIX_PATH={prefix}")
        ctx.ok(f"{gen}: consumer configure", cconf)
        imported = message_lines(cconf.stdout, "-- IMPORTED")
        ctx.check(f"{gen}: imported include directory",
                  len(imported) == 1 and f"{prefix}/include/fsh-1" in imported[0], imported)
        ctx.observe(f"{gen}: imported include directory", imported)
        ctx.ok(f"{gen}: consumer build", ctx.build(cbuild, gen))
        run = ctx.run([cbuild / "use"])
        ctx.check(f"{gen}: consumer runs", run.ok and run.stdout == "fsh 1.0 14\n", run.tail())
        ctx.observe(f"{gen}: consumer output", [run.returncode, run.stdout])


def _check_module_build(ctx: Ctx) -> None:
    r = ctx.root
    msrc = ctx.tree(r / "modbuild", MODULE_BUILD)
    mbuild = r / "modbuild-b"
    conf = ctx.configure(msrc, mbuild, "Ninja")
    if conf.ok:
        ctx.observe("module support", "scanner")
        ctx.ok("module build", ctx.build(mbuild, "Ninja"))
        run = ctx.run([mbuild / "hello"])
        ctx.check("module program runs", run.ok and run.stdout == "module 20\n", run.tail())
        ninja = (mbuild / "build.ninja").read_text(encoding="utf-8", errors="replace")
        ctx.check("build.ninja uses dyndep for module scanning", "dyndep" in ninja, "no dyndep in build.ninja")
        again = ctx.build(mbuild, "Ninja")
        ctx.check("module rebuild is a no-op", no_op(again) or "no work to do" in again.stdout, again.tail())
        ctx.observe("module output", run.stdout)
    else:
        blocks = cmake_errors(conf.output)
        ctx.check("compiler without a module scanner: exact diagnostic",
                  any(NO_SCANNER in block for block in blocks), blocks)
        ctx.observe("module support", ["no scanner", blocks])
        ctx.skip("C++20 module build: the compiler has no dependency scanner; checked its diagnostic instead")


@scenario("s16", "File sets + C++20 modules: generator/std/scope diagnostics, HEADERS install/export/verify, module build if supported")
def s16_file_sets(ctx: Ctx) -> None:
    gens = generators_available(ctx.options)
    if "Ninja" not in gens or "Unix Makefiles" not in gens:
        raise ScenarioSkip("needs Ninja and Unix Makefiles")
    _check_module_diagnostics(ctx)
    _check_header_sets(ctx)
    _check_module_build(ctx)
