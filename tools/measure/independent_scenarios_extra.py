# SPDX-License-Identifier: MIT OR Apache-2.0
"""Scenarios s15 to s20 of the independent black-box verification suite.

independent_verify.py imports this module, and the decorators below add the
scenarios to its registry. Each scenario writes its projects below the
scenario root in the /tmp sandbox, and the runner removes that root.

  s15  install/export diamond with a versioned shared library
  s16  file sets, C++20 module diagnostics, and a module build if possible
  s17  File-Based API replies for all five object kinds
  s18  adversarial paths, policy scopes, and runtime dependency sets
  s19  cmake -E utility gauntlet
  s20  check modules, FetchContent, CTest, and CPack in one project
"""

from __future__ import annotations

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
from typing import Any, Mapping, Sequence
import zipfile

import independent_verify as iv
from independent_verify import DARWIN, Ctx, ScenarioSkip, StepFailed, scenario


# --------------------------------------------------------------------------
# Shared helpers


# A fixed time for archive members: 2001-02-03 04:05:06 UTC.
FIXED_MTIME = "2001-02-03 04:05:06 UTC"
FIXED_EPOCH = 981173106


def is_appledouble(name: str) -> bool:
    """Return True for macOS metadata entries such as ._file and .DS_Store."""
    base = name.rstrip("/").rsplit("/", 1)[-1]
    return base.startswith("._") or base == ".DS_Store"


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


def tool_output(binary: Path, *args: str) -> str:
    """Run a binary inspection tool and return its stdout, or "" on error."""
    try:
        return subprocess.run([*args, str(binary)], capture_output=True, text=True, timeout=60).stdout
    except (OSError, subprocess.SubprocessError):
        return ""


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


def result(proc: iv.Proc) -> list[Any]:
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


# --------------------------------------------------------------------------
# s15: install/export diamond with a versioned shared library


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
    if DARWIN:
        return {"lib/libpkg_shared.2.3.4.dylib": "", "lib/libpkg_shared.2.dylib": "libpkg_shared.2.3.4.dylib",
                "lib/libpkg_shared.dylib": "libpkg_shared.2.dylib"}
    return {"lib/libpkg_shared.so.2.3.4": "", "lib/libpkg_shared.so.2": "libpkg_shared.so.2.3.4",
            "lib/libpkg_shared.so": "libpkg_shared.so.2"}


def pkg_flow(ctx: Ctx, gen: str, src: Path, consumer: Path) -> None:
    """Build, install, inspect, and consume the Pkg project with one generator."""
    tag = iv.slug(gen)
    build, prefix, cbuild = ctx.root / f"b-{tag}", ctx.root / f"inst-{tag}", ctx.root / f"c-{tag}"
    ctx.ok(f"{gen}: configure", ctx.configure(src, build, gen, f"-DCMAKE_INSTALL_PREFIX={prefix}",
                                              "-DCMAKE_INSTALL_LIBDIR=lib"))
    ctx.ok(f"{gen}: build", ctx.build(build, gen))
    run = ctx.run([build / "app/pkg_app"])
    ctx.check(f"{gen}: build-tree app runs", run.ok and run.stdout == "pkg 100 33\n", run.tail())
    ctx.ok(f"{gen}: install", ctx.cmake("--install", build))

    listing = iv.file_listing(prefix)
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

    if DARWIN:
        lib = prefix / real
        facts = {
            "id": iv.macho_id(lib),
            "app rpaths": iv.macho_rpaths(prefix / "bin/pkg_app"),
            "app loads": macho_load_dylibs(prefix / "bin/pkg_app"),
            "versions": macho_versions(prefix / "bin/pkg_app", "@rpath/libpkg_shared.2.dylib"),
            "lib rpaths": iv.macho_rpaths(lib),
        }
        ctx.check(f"{gen}: install name", facts["id"] == "@rpath/libpkg_shared.2.dylib", facts)
        ctx.check(f"{gen}: installed app RPATH", facts["app rpaths"] == ["@loader_path/../lib"], facts)
        ctx.check(f"{gen}: installed app loads the SOVERSION name",
                  facts["app loads"] == ["@rpath/libpkg_shared.2.dylib"], facts)
        ctx.check(f"{gen}: Mach-O compatibility and current version",
                  facts["versions"] == "compatibility version 2.0.0, current version 2.3.4", facts)
        build_rpaths = iv.macho_rpaths(build / "app/pkg_app")
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
    if DARWIN:
        crpaths = iv.macho_rpaths(cbuild / "use")
        ctx.check(f"{gen}: consumer RPATH points at the installed library", crpaths == [f"{prefix}/lib"], crpaths)
        ctx.observe(f"{gen}: consumer rpaths", crpaths)
    ctx.observe(f"{gen}: outputs", [run.stdout, installed.stdout, use.stdout])


@scenario("s15", "Install/export diamond: INTERFACE+OBJECT+STATIC+versioned SHARED, GenerateExportHeader, hidden inlines, PackageConfigHelpers, RPATH, consumer")
def s15_package_diamond(ctx: Ctx) -> None:
    gens = [g for g in ("Ninja", "Unix Makefiles") if g in iv.generators_available(ctx.options)]
    if not gens:
        raise ScenarioSkip("needs Ninja or Unix Makefiles")
    src = ctx.tree(ctx.root / "src", PKG_PROJECT)
    consumer = ctx.tree(ctx.root / "consumer", PKG_CONSUMER)
    for gen in gens:
        try:
            pkg_flow(ctx, gen, src, consumer)
        except StepFailed:
            continue


# --------------------------------------------------------------------------
# s16: file sets, C++20 module diagnostics, and a module build


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
# Project name, CMakeLists.txt body. Each project fails on every generator.
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


@scenario("s16", "File sets + C++20 modules: generator/std/scope diagnostics, HEADERS install/export/verify, module build if supported")
def s16_file_sets(ctx: Ctx) -> None:
    r = ctx.root
    gens = iv.generators_available(ctx.options)
    if "Ninja" not in gens or "Unix Makefiles" not in gens:
        raise ScenarioSkip("needs Ninja and Unix Makefiles")

    # (a) Unix Makefiles reports the generator error before the cxx_std_20 check.
    cases = (("Unix Makefiles", "target_compile_features(mods PUBLIC cxx_std_20)", NO_MODULE_GENERATOR),
             ("Unix Makefiles", "", NO_MODULE_GENERATOR), ("Ninja", "", NO_CXX20))
    for gen, feature, text in cases:
        tag = f"{iv.slug(gen)}-{'std20' if feature else 'nostd'}"
        src = ctx.tree(r / f"m-{tag}", MODULE_FILES)
        ctx.write("CMakeLists.txt", MODULE_LIB.replace("{feature}", feature), src)
        proc = ctx.configure(src, r / f"mb-{tag}", gen)
        blocks = cmake_errors(proc.output)
        ctx.check(f"{tag}: configure fails", proc.returncode != 0, proc.tail())
        ctx.check(f"{tag}: exact diagnostic", any(text in block for block in blocks), blocks)
        ctx.check(f"{tag}: one module diagnostic only", len(blocks) == 1, blocks)
        ctx.check(f"{tag}: generate step failed", "CMake Generate step failed." in proc.output, proc.tail())
        ctx.observe(f"modules {tag}", [proc.returncode, blocks])

    # File set errors that do not depend on the generator or the compiler.
    for name, body in FILE_SET_ERRORS.items():
        src = ctx.tree(r / f"e-{name}", MODULE_FILES)
        ctx.write("CMakeLists.txt", body, src)
        proc = ctx.configure(src, r / f"eb-{name}", "Ninja")
        blocks = cmake_errors(proc.output)
        ctx.check(f"{name}: configure fails", proc.returncode != 0, proc.tail())
        ctx.check(f"{name}: diagnostic", any(FILE_SET_ERROR_TEXT[name] in block for block in blocks), blocks)
        ctx.observe(f"file set error {name}", [proc.returncode, collapse_repeats(blocks)])

    # (b) A HEADERS file set: build, verify, install, export, and consume.
    src = ctx.tree(r / "fsh", HEADER_SET_PROJECT)
    consumer = ctx.tree(r / "fshuse", HEADER_SET_CONSUMER)
    for gen in ("Ninja", "Unix Makefiles"):
        tag = iv.slug(gen)
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
        listing = iv.file_listing(prefix)
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

    # (c) A real module build when the compiler has a dependency scanner.
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
        ctx.check("module rebuild is a no-op", iv.no_op(again) or "no work to do" in again.stdout, again.tail())
        ctx.observe("module output", run.stdout)
    else:
        blocks = cmake_errors(conf.output)
        ctx.check("compiler without a module scanner: exact diagnostic",
                  any(NO_SCANNER in block for block in blocks), blocks)
        ctx.observe("module support", ["no scanner", blocks])
        ctx.skip("C++20 module build: the compiler has no dependency scanner; checked its diagnostic instead")


# --------------------------------------------------------------------------
# s17: File-Based API


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
    if "Ninja Multi-Config" not in iv.generators_available(ctx.options):
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

    # The model must match the build: build Release and look for each artifact.
    ctx.ok("build Release", ctx.build(build, "Ninja Multi-Config"))
    for target in ("app", "core"):
        for art in find_target(release, target).get("artifacts", []):
            ctx.check(f"artifact {art.get('path')} exists", (build / art.get("path", "")).is_file())
    ctx.check("program runs", ctx.run([build / "Release/app"]).ok)

    # A new configure replaces the reply. Only one index file stays.
    (build / ".cmake/api/v1/query/client-indep/query.json").unlink()
    ctx.ok("reconfigure", ctx.cmake(build))
    indexes2, index2 = file_api_reply(build)
    ctx.check("one index after reconfigure", len(indexes2) == 1, indexes2)
    ctx.check("removed client query is gone from the reply",
              "query.json" not in index2.get("reply", {}).get("client-indep", {}), index2.get("reply", {}).get("client-indep"))
    ctx.observe("reply keys after reconfigure", sorted(index2.get("reply", {})))


# --------------------------------------------------------------------------
# s18: adversarial paths, policy scopes, and runtime dependency sets


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


def shared_names(name: str, version: str, soversion: str) -> tuple[str, str]:
    """Return the real file name and the SONAME of a versioned shared library."""
    if DARWIN:
        return f"lib{name}.{version}.dylib", f"@rpath/lib{name}.{soversion}.dylib"
    return f"lib{name}.so.{version}", f"lib{name}.so.{soversion}"


@scenario("s18", "Adversarial paths (space, UTF-8, + @ #) + block/cmake_policy scopes + CMP0070/0111/0115/0116/0220 + RUNTIME_DEPENDENCY_SET")
def s18_adversarial(ctx: Ctx) -> None:
    gens = [g for g in ("Ninja", "Unix Makefiles") if g in iv.generators_available(ctx.options)]
    if not gens:
        raise ScenarioSkip("needs Ninja or Unix Makefiles")
    base = ctx.root / ADV_BASE
    rel_rpath = "@loader_path/../lib" if DARWIN else "$ORIGIN/../lib"
    real, soname = shared_names("vendor", "1.0", "1")

    vsrc = ctx.tree(base / "vendor src", VENDOR_PROJECT)
    vinst = base / "v inst+@#"
    ctx.ok("vendor configure", ctx.configure(vsrc, base / "v bld+@#", gens[0], f"-DCMAKE_INSTALL_PREFIX={vinst}"))
    ctx.ok("vendor build", ctx.build(base / "v bld+@#", gens[0]))
    ctx.ok("vendor install", ctx.cmake("--install", base / "v bld+@#"))
    ctx.require("vendor library installed", (vinst / "lib" / real).is_file(), iv.file_listing(vinst))

    src = ctx.tree(base / ADV_SRC, ADV_PROJECT)
    for gen in gens:
        tag = iv.slug(gen)
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
                      if (build / "rel gen/out.txt").is_file() else False, iv.file_listing(build)[:50])
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
            iv.touch_later(src / "dep sub/input.txt", "second\n")
            ctx.ok(f"{gen}: rebuild after a DEPFILE input changes", ctx.build(build, gen))
            ctx.check(f"{gen}: DEPFILE dependency reruns the command",
                      new_txt.is_file() and new_txt.read_text() == "second\n",
                      new_txt.read_text() if new_txt.is_file() else "missing")
            (src / "dep sub/input.txt").write_text("first\n", encoding="utf-8")

            ctx.ok(f"{gen}: install", ctx.cmake("--install", build))
            listing = iv.file_listing(prefix)
            expect_libs = sorted(["lib/libdep.dylib" if DARWIN else "lib/libdep.so", f"lib/{real}",
                                  "lib/" + soname.rsplit("/", 1)[-1]])
            ctx.check(f"{gen}: install tree has the transitive runtime libraries",
                      sorted(item.split(" -> ")[0] for item in listing) == sorted(["bin/app", *expect_libs]),
                      listing)
            ctx.observe(f"{gen}: install tree", listing)
            installed = ctx.run([prefix / "bin/app"])
            ctx.check(f"{gen}: installed app runs", installed.ok and installed.stdout == "adv 84 3\n",
                      installed.tail())
            ctx.observe(f"{gen}: outputs", [run.stdout, installed.stdout])
            if DARWIN:
                rpaths = iv.macho_rpaths(prefix / "bin/app")
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

    # Negative projects: CMP0220 OLD and CMP0111 in nested policy scopes.
    for name, body in ADV_NEGATIVE.items():
        nsrc = ctx.tree(base / f"neg {name}", ADV_NEGATIVE_FILES)
        ctx.write("CMakeLists.txt", body, nsrc)
        proc = ctx.configure(nsrc, base / f"neg-b {name}", gens[0])
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


# --------------------------------------------------------------------------
# s19: cmake -E utility gauntlet


# Archive file, tar flags, extra arguments, expected kind.
E_ARCHIVES = (
    ("t.tar", "cf", (), "tar-posix"),
    ("t.tar.gz", "cfz", (), "gzip"),
    ("t.tar.bz2", "cfj", (), "bzip2"),
    ("t.tar.xz", "cfJ", (), "xz"),
    ("t.tar.zst", "cf", ("--zstd",), "zstd"),
    ("t.zip", "cf", ("--format=zip",), "zip"),
    ("t.7z", "cf", ("--format=7zip",), "7zip"),
    ("g.tar", "cf", ("--format=gnutar",), "tar-gnu"),
    ("p.tar", "cf", ("--format=pax",), "tar-posix"),
)
E_MEMBERS = ["d", "d/a.txt", "d/link", "d/sp ace.txt", "d/sub", "d/sub/b.txt"]
HASH_ALGOS = ("md5", "sha1", "sha224", "sha256", "sha384", "sha512")


# The generators that Rcmake implements. Rcmake has no Watcom WMake, no
# FASTBuild, and no legacy extra generators (CodeBlocks, Kate, ...).
RCMAKE_GENERATORS = ("Ninja", "Ninja Multi-Config", "Unix Makefiles", "Xcode")


def supported_capabilities(data: Any) -> Any:
    """Return "cmake -E capabilities" output without the parts Rcmake omits by design.

    Rcmake has no DAP debugger ("debugger") and no extra generators. Only
    the generators that Rcmake implements stay, sorted by name. Visual
    Studio generators stay too. All other members stay as they are.
    """
    if not isinstance(data, Mapping):
        return data
    out = {key: value for key, value in data.items() if key != "debugger"}
    if isinstance(data.get("generators"), list):
        gens = [{key: value for key, value in gen.items() if key != "extraGenerators"}
                for gen in data["generators"] if isinstance(gen, Mapping)
                and (gen.get("name") in RCMAKE_GENERATORS or str(gen.get("name", "")).startswith("Visual Studio"))]
        out["generators"] = sorted(gens, key=lambda gen: str(gen.get("name", "")))
    return out


def without_tar_owner(line: str) -> str:
    """Return a "tar tvf" line with collapsed spaces and no owner or group names.

    Columns 3 and 4 are the owner and the group. CMake stores the owner of
    the file on disk, so the value depends on the host. Rcmake stores uid 0
    and gid 0 for reproducible archives. The mode, link count, size, date,
    path, and link target stay.
    """
    parts = re.sub(r"\s+", " ", line).split(" ", 4)
    if len(parts) == 5:
        parts[2:4] = ["<OWNER>", "<GROUP>"]
    return " ".join(parts)


def e_fixture(ctx: Ctx) -> None:
    """Write the input files of s19."""
    r = ctx.root
    ctx.write("d/a.txt", "alpha\n")
    ctx.write("d/sp ace.txt", "space name\n")
    (r / "d/sub").mkdir(parents=True, exist_ok=True)
    (r / "d/sub/b.txt").write_bytes(b"b\r\nline2\r\n")
    os.chmod(r / "d/sub/b.txt", 0o755)
    os.chmod(r / "d/a.txt", 0o644)
    os.chmod(r / "d/sp ace.txt", 0o600)
    (r / "d/link").symlink_to("a.txt")
    ctx.write("e/extra.txt", "extra\n")
    (r / "lf.txt").write_bytes(b"b\nline2\n")
    (r / "crlf.txt").write_bytes(b"b\r\nline2\r\n")
    (r / "other.txt").write_bytes(b"b\nline3\n")
    ctx.write("-dashfile", "dash file\n")


@scenario("s19", "cmake -E gauntlet: tar/zip/7z formats + --mtime + selective extract, hashes, copy, symlink, env, cat, compare_files, capabilities")
def s19_cmake_e(ctx: Ctx) -> None:
    r = ctx.root
    e_fixture(ctx)
    env = {"TZ": "UTC"}

    def cm(*args: str, cwd: Path | None = None, stdin: str | None = None) -> iv.Proc:
        return ctx.cmake("-E", *args, cwd=cwd or r, env=env, stdin=stdin)

    source = tree_digest(r / "d")
    ctx.observe("source digest", source)

    # Archives: create, identify, list, extract, and compare with the source.
    for name, flags, extra, kind in E_ARCHIVES:
        made = cm("tar", flags, name, f"--mtime={FIXED_MTIME}", *extra, "d")
        ctx.check(f"tar {name}: create", made.ok, made.tail())
        ctx.check(f"tar {name}: format is {kind}", archive_kind(r / name) == kind, archive_kind(r / name))
        listed = cm("tar", "tf", name)
        names = listing_lines(listed.stdout)
        ctx.check(f"tar {name}: list", listed.ok and sorted(names) == E_MEMBERS, listed.tail())
        ctx.observe(f"tar {name}: list", [listed.returncode, names, listed.stderr])
        if kind not in ("zstd", "7zip"):
            members = python_members(r / name)
            ctx.check(f"tar {name}: Python reads the same members",
                      sorted(m for m, _ in members) == E_MEMBERS, members)
            if kind != "zip":
                ctx.check(f"tar {name}: --mtime on every member",
                          all(t == FIXED_EPOCH for _, t in members), members)
        out = r / f"x-{iv.slug(name)}"
        out.mkdir()
        got = cm("tar", "xf", f"../{name}", cwd=out)
        digest = tree_digest(out / "d") if (out / "d").is_dir() else []
        ctx.check(f"tar {name}: extract", got.ok and digest == source, [got.tail(), digest, source])
        mtime = int((out / "d/a.txt").stat().st_mtime) if (out / "d/a.txt").exists() else -1
        ctx.check(f"tar {name}: extracted mtime", mtime == FIXED_EPOCH, mtime)
        ctx.observe(f"tar {name}: extract", [got.returncode, got.stdout, got.stderr, digest, mtime])

    verbose = cm("tar", "tvf", "g.tar")
    lines = [line for line in verbose.stdout.splitlines() if not is_appledouble(line.split(" ")[-1])]
    ctx.check("tar tvf shows the link and the fixed date",
              verbose.ok and any(line.endswith("d/link -> a.txt") for line in lines)
              and all("2001" in line for line in lines), verbose.tail())
    ctx.observe("tar tvf", [verbose.returncode, [without_tar_owner(line) for line in lines]])

    # Selective extraction and extraction errors.
    sel = r / "x-select"
    sel.mkdir()
    one = cm("tar", "xzf", "../t.tar.gz", "d/sub/b.txt", cwd=sel)
    expect = ["d/", "d/sub/"] + [f"d/{item}" for item in source if item.startswith("sub/b.txt ")]
    ctx.check("tar xf one member", one.ok and tree_digest(sel) == expect, [one.tail(), tree_digest(sel)])
    ctx.observe("tar xf one member", [result(one), tree_digest(sel)])
    missing = cm("tar", "xf", "../t.tar", "d/absent.txt", cwd=sel)
    ctx.check("tar xf absent member fails", missing.returncode != 0 and "Not found in archive" in missing.stderr,
              missing.tail())
    errors = {
        "bad mtime": cm("tar", "cf", "bad1.tar", "--mtime=2001-02-03T04:05:06", "d"),
        "bad format": cm("tar", "cf", "bad2.tar", "--format=ustar", "d"),
        "absent archive": cm("tar", "tf", "absent.tar"),
        "absent input": cm("tar", "cf", "bad3.tar", "no-such-dir"),
    }
    ctx.check("tar rejects an ISO 8601 mtime", "unable to parse mtime" in errors["bad mtime"].stderr,
              errors["bad mtime"].tail())
    ctx.check("tar rejects --format=ustar", "Unknown -E tar --format= argument: ustar" in errors["bad format"].stderr,
              errors["bad format"].tail())
    ctx.check("tar tf of an absent archive fails", errors["absent archive"].returncode != 0,
              errors["absent archive"].tail())
    ctx.observe("tar errors", {key: result(proc) for key, proc in errors.items()} | {"absent member": result(missing)})

    # Hash utilities.
    for algo in HASH_ALGOS:
        proc = cm(f"{algo}sum", "lf.txt", "d/sp ace.txt")
        expect = "".join(f"{hashlib.new(algo, (r / f).read_bytes()).hexdigest()}  {f}\n"
                         for f in ("lf.txt", "d/sp ace.txt"))
        ctx.check(f"{algo}sum", proc.ok and proc.stdout == expect, [proc.tail(), expect])
        ctx.observe(f"{algo}sum", result(proc))
    absent = cm("md5sum", "lf.txt", "nope.txt")
    ctx.check("md5sum of an absent file fails", absent.returncode == 1
              and "nope.txt: No such file or directory" in absent.output
              and absent.stdout.startswith(hashlib.md5((r / "lf.txt").read_bytes()).hexdigest()), absent.tail())
    ctx.observe("md5sum absent", result(absent))

    # compare_files.
    compares = {
        "lf crlf": cm("compare_files", "lf.txt", "crlf.txt"),
        "ignore-eol lf crlf": cm("compare_files", "--ignore-eol", "lf.txt", "crlf.txt"),
        "ignore-eol lf other": cm("compare_files", "--ignore-eol", "lf.txt", "other.txt"),
        "same": cm("compare_files", "lf.txt", "lf.txt"),
        "absent": cm("compare_files", "lf.txt", "nope.txt"),
    }
    codes = {key: proc.returncode for key, proc in compares.items()}
    ctx.check("compare_files exit codes", codes == {
        "lf crlf": 1, "ignore-eol lf crlf": 0, "ignore-eol lf other": 1, "same": 0, "absent": 1}, codes)
    ctx.observe("compare_files", {key: result(proc) for key, proc in compares.items()})

    # copy_directory, copy_directory_if_different, copy_if_different.
    # copy_directory follows symbolic links and copies the file they point to.
    followed = sorted(item.replace("link -> a.txt", "link " + next(
        x.split(" ", 1)[1] for x in source if x.startswith("a.txt "))) for item in source)
    copy = cm("copy_directory", "d", "copy1")
    ctx.check("copy_directory follows links", copy.ok and tree_digest(r / "copy1") == followed,
              [copy.tail(), tree_digest(r / "copy1"), followed])
    merged = cm("copy_directory", "d", "e", "copy2")
    ctx.check("copy_directory merges two sources", merged.ok
              and tree_digest(r / "copy2") == sorted(followed + tree_digest(r / "e")), tree_digest(r / "copy2"))
    bad_copy = cm("copy_directory", "d", "absent-dir", "copy3")
    ctx.check("copy_directory with an absent source fails", bad_copy.returncode != 0, bad_copy.tail())
    os.utime(r / "copy1/a.txt", (1000000000, 1000000000))
    (r / "copy1/sub/b.txt").write_text("changed\n")
    again = cm("copy_directory_if_different", "d", "copy1")
    ctx.check("copy_directory_if_different keeps the same file",
              again.ok and int((r / "copy1/a.txt").stat().st_mtime) == 1000000000, again.tail())
    ctx.check("copy_directory_if_different replaces a changed file",
              (r / "copy1/sub/b.txt").read_bytes() == (r / "d/sub/b.txt").read_bytes())
    (r / "cdir").mkdir()
    cid = cm("copy_if_different", "lf.txt", "crlf.txt", "cdir")
    os.utime(r / "cdir/lf.txt", (1000000000, 1000000000))
    cid2 = cm("copy_if_different", "lf.txt", "cdir/lf.txt")
    ctx.check("copy_if_different to a directory", cid.ok and sorted(os.listdir(r / "cdir")) == ["crlf.txt", "lf.txt"],
              cid.tail())
    ctx.check("copy_if_different keeps the same file",
              cid2.ok and int((r / "cdir/lf.txt").stat().st_mtime) == 1000000000, cid2.tail())
    cid3 = cm("copy_if_different", "lf.txt", "crlf.txt", "not-a-dir")
    ctx.check("copy_if_different with two files and no directory fails",
              cid3.returncode != 0 and "is not a directory" in cid3.output, cid3.tail())
    ctx.observe("copies", {
        "copy_directory": [result(copy), tree_digest(r / "copy1")],
        "merged": [result(merged), tree_digest(r / "copy2")],
        "absent source": result(bad_copy), "if_different": result(again),
        "copy_if_different": [result(cid), result(cid2), result(cid3)],
    })

    # create_symlink.
    link1 = cm("create_symlink", "lf.txt", "sl")
    link2 = cm("create_symlink", "crlf.txt", "sl")
    target = os.readlink(r / "sl") if (r / "sl").is_symlink() else ""
    ctx.check("create_symlink replaces an existing link", link1.ok and link2.ok and target == "crlf.txt",
              [link1.tail(), link2.tail(), target])
    ctx.observe("create_symlink", [result(link1), result(link2), target])

    # env --modify, --unset, and errors.
    show = 'printf "%s|%s|%s|%s|%s\\n" "$FOO" "${PATH%%:*}" "${HOME-unset}" "${EMPTY-unset}" "$LIST"'
    mod = cm("env", "--modify", "PATH=path_list_prepend:/opt/indep", "--modify", "FOO=set:bar",
             "--modify", "FOO=string_append:baz", "--modify", "LIST=cmake_list_append:x",
             "--modify", "LIST=cmake_list_prepend:w", "--unset=HOME", "EMPTY=", "--", "sh", "-c", show)
    ctx.check("env --modify", mod.ok and mod.stdout == "barbaz|/opt/indep|unset||w;x\n", mod.tail())
    bad_env = cm("env", "--modify", "BAD=nope:x", "--", "true")
    ctx.check("env --modify with a bad operation fails", bad_env.returncode != 0
              and "invalid parameter to --modify" in bad_env.stderr, bad_env.tail())
    ctx.observe("env", [result(mod), result(bad_env)])

    # cat with stdin, a dash file name, and an absent file.
    cats = {
        "- -dashfile": cm("cat", "-", "-dashfile", stdin="from stdin\n"),
        "-- -dashfile lf.txt": cm("cat", "--", "-dashfile", "lf.txt"),
        "absent": cm("cat", "lf.txt", "absent.txt"),
    }
    ctx.check("cat - -dashfile", cats["- -dashfile"].stdout == "from stdin\ndash file\n", cats["- -dashfile"].tail())
    ctx.check("cat -- -dashfile lf.txt", cats["-- -dashfile lf.txt"].stdout == "dash file\nb\nline2\n",
              cats["-- -dashfile lf.txt"].tail())
    ctx.check("cat of an absent file fails", cats["absent"].returncode != 0, cats["absent"].tail())
    ctx.observe("cat", {key: result(proc) for key, proc in cats.items()})

    # capabilities.
    caps = cm("capabilities")
    try:
        data = json.loads(caps.stdout)
    except ValueError:
        data = {}
    kinds = sorted(item.get("kind", "") for item in data.get("fileApi", {}).get("requests", []))
    ctx.check("capabilities is JSON for 4.5.1", data.get("version", {}).get("string") == "4.5.1", caps.tail())
    ctx.check("capabilities lists the five File-API kinds",
              kinds == ["cache", "cmakeFiles", "codemodel", "configureLog", "toolchains"], kinds)
    names = sorted(gen.get("name", "") for gen in data.get("generators", []))
    ctx.check("capabilities lists Ninja and Unix Makefiles",
              {"Ninja", "Ninja Multi-Config", "Unix Makefiles"} <= set(names), names)
    ctx.observe("capabilities", supported_capabilities(data))


# --------------------------------------------------------------------------
# s20: check modules, FetchContent, CTest, and CPack in one project


PROBE_PROJECT = {
    "CMakeLists.txt": """
        cmake_minimum_required(VERSION 3.28)
        project(Probe VERSION 0.4.2 LANGUAGES C CXX)
        include(CheckCCompilerFlag)
        include(CheckCXXCompilerFlag)
        include(CheckSymbolExists)
        include(CheckIncludeFile)
        include(CheckTypeSize)
        include(TestBigEndian)
        include(FetchContent)
        enable_testing()
        set(THREADS_PREFER_PTHREAD_FLAG ON)
        find_package(Threads REQUIRED)
        check_c_compiler_flag(-Wall HAVE_WALL)
        check_c_compiler_flag(-fno-such-flag-indep HAVE_BOGUS_FLAG)
        check_cxx_compiler_flag(-std=c++17 HAVE_CXX17_FLAG)
        check_symbol_exists(snprintf stdio.h HAVE_SNPRINTF)
        check_symbol_exists(no_such_symbol_indep stdio.h HAVE_NO_SUCH_SYMBOL)
        check_include_file(stdint.h HAVE_STDINT_H)
        check_include_file(no_such_header_indep.h HAVE_NO_SUCH_HEADER)
        check_type_size("long long" SIZEOF_LONG_LONG)
        check_type_size("struct no_such_struct_indep" SIZEOF_NO_SUCH)
        test_big_endian(IS_BIG_ENDIAN)
        message(STATUS "CHECKS wall=[${HAVE_WALL}] bogus=[${HAVE_BOGUS_FLAG}] cxx17=[${HAVE_CXX17_FLAG}]")
        message(STATUS "CHECKS snprintf=[${HAVE_SNPRINTF}] nosym=[${HAVE_NO_SUCH_SYMBOL}] stdint=[${HAVE_STDINT_H}] nohdr=[${HAVE_NO_SUCH_HEADER}]")
        message(STATUS "CHECKS ll=[${SIZEOF_LONG_LONG}] have_ll=[${HAVE_SIZEOF_LONG_LONG}] nostruct=[${SIZEOF_NO_SUCH}] big=[${IS_BIG_ENDIAN}]")
        message(STATUS "CHECKS threads=[${Threads_FOUND}] pthreads=[${CMAKE_USE_PTHREADS_INIT}]")
        if(USE_GIT)
          FetchContent_Declare(mathlib GIT_REPOSITORY "${MATHLIB_REPO}" GIT_TAG v1.0)
        else()
          FetchContent_Declare(mathlib URL "${MATHLIB_URL}")
        endif()
        FetchContent_Declare(strlib URL "${STRLIB_URL}" URL_HASH SHA256=${STRLIB_SHA256})
        FetchContent_MakeAvailable(mathlib strlib)
        FetchContent_GetProperties(strlib)
        file(RELATIVE_PATH strlib_rel "${CMAKE_BINARY_DIR}" "${strlib_SOURCE_DIR}")
        message(STATUS "FETCH strlib populated=[${strlib_POPULATED}] source=[${strlib_rel}]")
        configure_file(config.h.in config.h @ONLY)
        add_library(util STATIC util.cpp)
        target_link_libraries(util PUBLIC Threads::Threads)
        target_compile_features(util PUBLIC cxx_std_17)
        add_executable(probe main.c)
        target_include_directories(probe PRIVATE ${CMAKE_CURRENT_BINARY_DIR})
        target_link_libraries(probe PRIVATE mathlib strlib util)
        set_target_properties(probe PROPERTIES LINKER_LANGUAGE CXX)
        add_test(NAME probe_runs COMMAND probe)
        set_tests_properties(probe_runs PROPERTIES PASS_REGULAR_EXPRESSION "^probe 0\\\\.4\\\\.2 42 strlib-1\\\\.2")
        add_test(NAME probe_threads COMMAND probe --threads)
        add_test(NAME probe_expected_failure COMMAND probe --fail)
        set_tests_properties(probe_expected_failure PROPERTIES WILL_FAIL TRUE)
        add_test(NAME quick_echo COMMAND ${CMAKE_COMMAND} -E echo quick)
        set_tests_properties(quick_echo PROPERTIES LABELS quick)
        install(TARGETS probe RUNTIME DESTINATION bin)
        install(FILES README.txt DESTINATION share/doc/probe)
        set(CPACK_PACKAGE_NAME probe)
        set(CPACK_PACKAGE_FILE_NAME probe-${PROJECT_VERSION}-indep)
        set(CPACK_GENERATOR "TGZ;ZIP")
        set(CPACK_PACKAGE_CHECKSUM SHA256)
        include(CPack)
        """,
    "config.h.in": """
        #define PROBE_VERSION "@PROJECT_VERSION@"
        #define PROBE_SIZEOF_LONG_LONG @SIZEOF_LONG_LONG@
        #define PROBE_BIG_ENDIAN @IS_BIG_ENDIAN@
        #cmakedefine HAVE_SNPRINTF 1
        #cmakedefine HAVE_NO_SUCH_SYMBOL 1
        #cmakedefine01 HAVE_STDINT_H
        #define PROBE_KEEP "${NOT_EXPANDED}"
        """,
    "main.c": """
        #include <pthread.h>
        #include <stdio.h>
        #include <string.h>
        #include "config.h"
        #include "mathlib.h"
        #include "strlib.h"
        int util_parallel_sum(int n);
        static void *worker(void *arg) { *(int *)arg = math_answer(); return NULL; }
        int main(int argc, char **argv) {
          if (argc > 1 && strcmp(argv[1], "--fail") == 0) return 3;
          if (argc > 1 && strcmp(argv[1], "--threads") == 0) {
            int v = 0;
            pthread_t t;
            pthread_create(&t, NULL, worker, &v);
            pthread_join(t, NULL);
            printf("thread %d %d\\n", v, util_parallel_sum(4));
            return v == 42 && util_parallel_sum(4) == 10 ? 0 : 1;
          }
        #if HAVE_STDINT_H && defined(HAVE_SNPRINTF) && !defined(HAVE_NO_SUCH_SYMBOL)
          char buf[96];
          snprintf(buf, sizeof buf, "%s %s %d %s", "probe", PROBE_VERSION, math_answer(), str_tag());
          printf("%s ll=%d be=%d keep=%s\\n", buf, PROBE_SIZEOF_LONG_LONG, PROBE_BIG_ENDIAN, PROBE_KEEP);
          return 0;
        #else
          return 2;
        #endif
        }
        """,
    "util.cpp": """
        #include <atomic>
        #include <thread>
        #include <vector>
        extern "C" int util_parallel_sum(int n) {
          std::atomic<int> total{0};
          std::vector<std::thread> pool;
          for (int i = 1; i <= n; ++i) pool.emplace_back([&total, i] { total += i; });
          for (auto &t : pool) t.join();
          return total.load();
        }
        """,
    "README.txt": "probe package\n",
}
MATHLIB_FILES = {
    "CMakeLists.txt": """
        cmake_minimum_required(VERSION 3.28)
        project(mathlib C)
        add_library(mathlib STATIC mathlib.c)
        target_include_directories(mathlib PUBLIC ${CMAKE_CURRENT_SOURCE_DIR})
        """,
    "mathlib.h": "int math_answer(void);\n",
    "mathlib.c": '#include "mathlib.h"\nint math_answer(void) { return 6 * 7; }\n',
}
STRLIB_FILES = {
    "CMakeLists.txt": """
        cmake_minimum_required(VERSION 3.28)
        project(strlib C)
        add_library(strlib STATIC strlib.c)
        target_include_directories(strlib PUBLIC ${CMAKE_CURRENT_SOURCE_DIR})
        """,
    "strlib.h": "const char *str_tag(void);\n",
    "strlib.c": '#include "strlib.h"\nconst char *str_tag(void) { return "strlib-1.2"; }\n',
}
GIT_ENV = {
    "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull, "GIT_TERMINAL_PROMPT": "0",
    "GIT_AUTHOR_NAME": "indep", "GIT_AUTHOR_EMAIL": "indep@example.invalid",
    "GIT_COMMITTER_NAME": "indep", "GIT_COMMITTER_EMAIL": "indep@example.invalid",
    "GIT_AUTHOR_DATE": "2001-02-03T04:05:06Z", "GIT_COMMITTER_DATE": "2001-02-03T04:05:06Z",
}
PROBE_OUTPUT = 'probe 0.4.2 42 strlib-1.2 ll=8 be=0 keep=${NOT_EXPANDED}\n'


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


def make_git_repo(ctx: Ctx, repo: Path) -> str:
    """Make a local git repository whose v1.0 tag holds MATHLIB_FILES. Return the tag commit."""
    ctx.tree(repo, MATHLIB_FILES)

    def git(*args: str) -> iv.Proc:
        return ctx.run(["git", "-c", "init.defaultBranch=main", "-c", "commit.gpgsign=false",
                        "-c", "tag.gpgsign=false", *args], cwd=repo, env=GIT_ENV)
    for args in (("init", "-q"), ("add", "."), ("commit", "-q", "-m", "mathlib 1.0"), ("tag", "v1.0")):
        ctx.ok(f"git {args[0]}", git(*args))
    tag = git("rev-parse", "HEAD").stdout.strip()
    # A later commit breaks the library, so the build proves that GIT_TAG selected v1.0.
    (repo / "mathlib.c").write_text('#error "HEAD is not v1.0"\n', encoding="utf-8")
    ctx.ok("git commit after the tag", git("commit", "-q", "-am", "after the tag"))
    return tag


@scenario("s20", "Real-world probe: Check* modules + TestBigEndian + Threads + offline FetchContent (git + tarball) + configure_file + CTest JUnit + CPack TGZ/ZIP")
def s20_real_world(ctx: Ctx) -> None:
    r = ctx.root
    gens = [g for g in ("Ninja", "Unix Makefiles") if g in iv.generators_available(ctx.options)]
    if not gens:
        raise ScenarioSkip("needs Ninja or Unix Makefiles")
    deps = r / "deps"
    strlib_tgz = deps / "strlib-1.2.tar.gz"
    write_tar_gz(strlib_tgz, "strlib-1.2", STRLIB_FILES)
    fetch = [f"-DSTRLIB_URL=file://{strlib_tgz}",
             f"-DSTRLIB_SHA256={hashlib.sha256(strlib_tgz.read_bytes()).hexdigest()}"]
    tag = ""
    if shutil.which("git"):
        tag = make_git_repo(ctx, deps / "mathlib.git")
        fetch += ["-DUSE_GIT=ON", f"-DMATHLIB_REPO=file://{deps / 'mathlib.git'}"]
        ctx.observe("git tag commit", tag)
    else:
        write_tar_gz(deps / "mathlib-1.0.tar.gz", "mathlib-1.0", MATHLIB_FILES)
        fetch += ["-DUSE_GIT=OFF", f"-DMATHLIB_URL=file://{deps / 'mathlib-1.0.tar.gz'}"]
        ctx.skip("git not found; FetchContent used a tarball for mathlib")
    src = ctx.tree(r / "src", PROBE_PROJECT)

    for gen in gens:
        tag_gen = iv.slug(gen)
        build = r / f"b-{tag_gen}"
        try:
            conf = ctx.configure(src, build, gen, *fetch, f"-DCMAKE_INSTALL_PREFIX={r}/inst-{tag_gen}", env=GIT_ENV)
            ctx.ok(f"{gen}: configure", conf)
            ctx.learn_tool_paths(build)
            checks = message_lines(conf.stdout, "-- CHECKS", "-- FETCH")
            ctx.check(f"{gen}: check results", checks[:4] == [
                "-- CHECKS wall=[1] bogus=[] cxx17=[1]",
                "-- CHECKS snprintf=[1] nosym=[] stdint=[1] nohdr=[]",
                "-- CHECKS ll=[8] have_ll=[TRUE] nostruct=[] big=[0]",
                "-- CHECKS threads=[TRUE] pthreads=[1]",
            ], checks)
            ctx.check(f"{gen}: FetchContent populated the tarball",
                      "-- FETCH strlib populated=[1] source=[_deps/strlib-src]" in checks, checks)
            ctx.observe(f"{gen}: configure messages", checks)
            ctx.observe(f"{gen}: check cache", iv.cache_entries(build, [
                "HAVE_WALL", "HAVE_BOGUS_FLAG", "HAVE_CXX17_FLAG", "HAVE_SNPRINTF", "HAVE_NO_SUCH_SYMBOL",
                "HAVE_STDINT_H", "HAVE_NO_SUCH_HEADER", "HAVE_SIZEOF_LONG_LONG", "SIZEOF_LONG_LONG",
                "HAVE_SIZEOF_NO_SUCH", "IS_BIG_ENDIAN", "CMAKE_HAVE_LIBC_PTHREAD", "FETCHCONTENT_BASE_DIR"]))
            config_h = (build / "config.h").read_text(encoding="utf-8") if (build / "config.h").is_file() else ""
            for line in ("#define HAVE_SNPRINTF 1", "/* #undef HAVE_NO_SUCH_SYMBOL */", "#define HAVE_STDINT_H 1",
                         '#define PROBE_KEEP "${NOT_EXPANDED}"', "#define PROBE_SIZEOF_LONG_LONG 8"):
                ctx.check(f"{gen}: config.h has {line}", line in config_h.splitlines(), config_h)
            ctx.observe(f"{gen}: config.h", config_h)
            if tag:
                head = ctx.run(["git", "rev-parse", "HEAD"], cwd=build / "_deps/mathlib-src", env=GIT_ENV)
                ctx.check(f"{gen}: FetchContent checked out the v1.0 tag", head.stdout.strip() == tag, head.tail())

            ctx.ok(f"{gen}: build", ctx.build(build, gen))
            run = ctx.run([build / "probe"])
            ctx.check(f"{gen}: probe runs", run.ok and run.stdout == PROBE_OUTPUT, run.tail())
            junit = build / "junit.xml"
            test = ctx.ctest("--test-dir", build, "--output-on-failure", "--output-junit", junit, "-C", "Release")
            ctx.check(f"{gen}: ctest passes", test.ok and "100% tests passed out of 4" in test.stdout,
                      test.tail())
            ctx.observe(f"{gen}: ctest output", [test.returncode, test.stdout])
            cases = iv.junit_cases(junit) if junit.is_file() else []
            ctx.check(f"{gen}: JUnit cases", sorted(cases) == [
                "probe_expected_failure:run", "probe_runs:run", "probe_threads:run", "quick_echo:run"], cases)
            ctx.observe(f"{gen}: junit", sorted(cases))
            labeled = ctx.ctest("--test-dir", build, "-L", "quick", "-C", "Release")
            ctx.check(f"{gen}: ctest -L quick", labeled.ok and "out of 1" in labeled.stdout, labeled.tail())
            listed = ctx.ctest("--test-dir", build, "-N", "-C", "Release")
            names = re.findall(r"Test\s+#\d+: (\S+)", listed.stdout)
            ctx.check(f"{gen}: ctest -N", names == ["probe_runs", "probe_threads", "probe_expected_failure",
                                                     "quick_echo"], listed.tail())
            ctx.observe(f"{gen}: ctest -N", names)

            offline = ctx.cmake("-DFETCHCONTENT_FULLY_DISCONNECTED=ON", build, env=GIT_ENV)
            ctx.ok(f"{gen}: reconfigure with FETCHCONTENT_FULLY_DISCONNECTED", offline)
        except StepFailed:
            continue

    # CPack with the first generator's tree, then run the extracted program.
    build = r / f"b-{iv.slug(gens[0])}"
    if not (build / "CPackConfig.cmake").is_file():
        return
    pack = ctx.cpack("-G", "TGZ;ZIP", "--config", build / "CPackConfig.cmake", "-B", r / "pkg", cwd=build)
    ctx.ok("cpack TGZ;ZIP", pack)
    tgz, zipf = r / "pkg/probe-0.4.2-indep.tar.gz", r / "pkg/probe-0.4.2-indep.zip"
    expect = ["probe-0.4.2-indep/bin/probe", "probe-0.4.2-indep/share/doc/probe/README.txt"]
    for path in (tgz, zipf):
        names = sorted(n for n in iv.archive_names(path) if not n.endswith("/")) if path.is_file() else []
        ctx.check(f"cpack {path.name} members", names == expect, names)
        ctx.observe(f"cpack {path.name} members", names)
        checksum = path.with_name(path.name + ".sha256")
        digest = hashlib.sha256(path.read_bytes()).hexdigest() if path.is_file() else ""
        ctx.check(f"cpack {path.name} checksum file", checksum.is_file()
                  and checksum.read_text().split()[:2] == [digest, path.name],
                  checksum.read_text() if checksum.is_file() else "missing")
    ctx.observe("cpack outputs", sorted(p.name for p in (r / "pkg").iterdir() if p.is_file()))

    out = r / "unpacked-tgz"
    with tarfile.open(tgz) as tar:
        tar.extractall(out, filter="tar")
    run = ctx.run([out / "probe-0.4.2-indep/bin/probe"])
    ctx.check("program from the TGZ package runs", run.ok and run.stdout == PROBE_OUTPUT, run.tail())
    zout = r / "unpacked-zip"
    with zipfile.ZipFile(zipf) as zf:
        for info in zf.infolist():
            target = zf.extract(info, zout)
            mode = (info.external_attr >> 16) & 0o777
            if mode:
                os.chmod(target, mode)
    zrun = ctx.run([zout / "probe-0.4.2-indep/bin/probe", "--threads"])
    ctx.check("program from the ZIP package runs", zrun.ok and zrun.stdout == "thread 42 10\n", zrun.tail())
    ctx.observe("package outputs", [run.stdout, zrun.stdout])
