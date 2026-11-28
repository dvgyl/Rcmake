# SPDX-License-Identifier: MIT OR Apache-2.0
"""CLI, presets, streaming archives, CTest, CPack, cmake -E, and probe scenarios."""

from __future__ import annotations

import gzip
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
from typing import Any, Callable, Mapping
import zipfile

from .context import (
    FIXED_EPOCH,
    FIXED_MTIME,
    Ctx,
    archive_kind,
    archive_names,
    cache_entries,
    file_listing,
    generators_available,
    is_appledouble,
    junit_cases,
    listing_lines,
    message_lines,
    python_members,
    result,
    sarif_digest,
    scenario,
    slug,
    tree_digest,
    write_tar_gz,
)
from .sandbox import (
    MIB,
    RSS_WRAPPER,
    Proc,
    ScenarioSkip,
    StepFailed,
    peak_rss,
    remove_tree,
)


BIG_SENTINEL = b"BIG-END-SENTINEL"
RSS_LIMIT = 256 * MIB


class ZeroStream:
    """A file-like reader of zero bytes that ends with BIG_SENTINEL."""

    def __init__(self, size: int) -> None:
        self.size = size
        self.pos = 0

    def read(self, count: int = -1) -> bytes:
        """Return the next bytes of the stream."""
        left = self.size - self.pos
        if count < 0 or count > left:
            count = left
        start, end = self.pos, self.pos + count
        self.pos = end
        tail_start = self.size - len(BIG_SENTINEL)
        if end <= tail_start:
            return bytes(count)
        zeros = max(0, tail_start - start)
        return bytes(zeros) + BIG_SENTINEL[max(0, start - tail_start):end - tail_start]


class _BytesReader:
    """A minimal reader over bytes for tarfile.addfile."""

    def __init__(self, data: bytes) -> None:
        self.data = data
        self.pos = 0

    def read(self, count: int = -1) -> bytes:
        """Return the next bytes."""
        end = len(self.data) if count < 0 else self.pos + count
        chunk = self.data[self.pos:end]
        self.pos += len(chunk)
        return chunk


def write_big_tar_gz(path: Path, size: int) -> None:
    """Write a gzip tar file with two small members and one large member."""
    def add(tar: tarfile.TarFile, name: str, data: bytes | ZeroStream, length: int) -> None:
        info = tarfile.TarInfo(name)
        info.size = length
        info.mode = 0o644
        info.mtime = 1_700_000_000
        tar.addfile(info, data if isinstance(data, ZeroStream) else _BytesReader(data))
    with gzip.GzipFile(path, "wb", compresslevel=1, mtime=0) as gz:
        with tarfile.open(fileobj=gz, mode="w|", format=tarfile.PAX_FORMAT) as tar:
            add(tar, "payload/sentinel-begin.txt", b"BEGIN\n", 6)
            add(tar, "payload/big.bin", ZeroStream(size), size)
            add(tar, "payload/sentinel-end.txt", b"END\n", 4)


def write_stored_zip(path: Path, size: int) -> None:
    """Write a stored (not compressed) zip file with one large member."""
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_STORED) as zf:
        zf.writestr("zpayload/z-begin.txt", "ZBEGIN\n")
        with zf.open(zipfile.ZipInfo("zpayload/zbig.bin", (2024, 1, 1, 0, 0, 0)), "w") as out:
            stream = ZeroStream(size)
            while chunk := stream.read(4 * MIB):
                out.write(chunk)
        zf.writestr("zpayload/z-end.txt", "ZEND\n")


def big_file_ok(path: Path, size: int) -> bool:
    """Check the size and the end sentinel of a large extracted member."""
    if not path.is_file() or path.stat().st_size != size:
        return False
    with path.open("rb") as file:
        file.seek(size - len(BIG_SENTINEL))
        return file.read() == BIG_SENTINEL


@scenario("s07", "Large streaming archives (> 513 MiB decoded .tar.gz + stored .zip) with bounded peak RSS")
def s07_archives(ctx: Ctx) -> None:
    size = ctx.options.archive_mib * MIB
    tgz = ctx.root / "big.tar.gz"
    zipf = ctx.root / "stored.zip"
    write_big_tar_gz(tgz, size)
    write_stored_zip(zipf, size)
    ctx.check("tar.gz decodes to more than 513 MiB", size > 513 * MIB, size)

    def measured(label: str, *args: str | os.PathLike[str], cwd: Path | None = None) -> Proc:
        proc = ctx.run([sys.executable, "-c", RSS_WRAPPER, ctx.tools.cmake, *args], cwd=cwd)
        rss = peak_rss(proc)
        ctx.check(f"{label}: exit 0", proc.ok, proc.tail())
        ctx.check(f"{label}: peak RSS < 256 MiB", rss is not None and rss < RSS_LIMIT,
                  f"{(rss or 0) / MIB:.1f} MiB")
        return proc

    listing = measured("tar tf big.tar.gz", "-E", "tar", "tf", tgz)
    names = [line for line in listing.stdout.splitlines() if line]
    ctx.check("tar tf lists the members", names == [
        "payload/sentinel-begin.txt", "payload/big.bin", "payload/sentinel-end.txt"], names)
    ctx.observe("tgz members", names)

    full = ctx.root / "x-full"
    full.mkdir()
    measured("tar xf big.tar.gz", "-E", "tar", "xf", tgz, cwd=full)
    ctx.check("extracted big.bin size and sentinel", big_file_ok(full / "payload/big.bin", size))
    ctx.check("extracted sentinels", (full / "payload/sentinel-begin.txt").read_bytes() == b"BEGIN\n"
              and (full / "payload/sentinel-end.txt").read_bytes() == b"END\n")
    remove_tree(full)

    member = ctx.root / "x-member"
    member.mkdir()
    measured("tar xf big.tar.gz <member>", "-E", "tar", "xf", tgz, "payload/sentinel-end.txt", cwd=member)
    ctx.check("member filter extracts one file", file_listing(member) == ["payload/sentinel-end.txt"],
              file_listing(member))

    ctx.write("extract.cmake", """
        file(ARCHIVE_EXTRACT INPUT "${ARCHIVE}" DESTINATION "${DEST}" ${EXTRA})
        """)
    dest = ctx.root / "x-patterns"
    measured("file(ARCHIVE_EXTRACT PATTERNS) tar.gz", f"-DARCHIVE={tgz}", f"-DDEST={dest}",
             "-DEXTRA=PATTERNS;payload/sentinel-*", "-P", ctx.root / "extract.cmake")
    ctx.check("ARCHIVE_EXTRACT PATTERNS skips big.bin", file_listing(dest) == [
        "payload/sentinel-begin.txt", "payload/sentinel-end.txt"], file_listing(dest))
    ctx.observe("pattern extract", file_listing(dest))

    zlist = measured("tar tf stored.zip", "-E", "tar", "tf", zipf)
    znames = [line for line in zlist.stdout.splitlines() if line]
    ctx.check("zip members", znames == ["zpayload/z-begin.txt", "zpayload/zbig.bin", "zpayload/z-end.txt"], znames)
    ctx.observe("zip members", znames)
    zmember = ctx.root / "z-member"
    zmember.mkdir()
    measured("tar xf stored.zip <member>", "-E", "tar", "xf", zipf, "zpayload/z-end.txt", cwd=zmember)
    ctx.check("zip member filter", file_listing(zmember) == ["zpayload/z-end.txt"], file_listing(zmember))
    zdest = ctx.root / "z-full"
    measured("file(ARCHIVE_EXTRACT) stored.zip", f"-DARCHIVE={zipf}", f"-DDEST={zdest}", "-DEXTRA=",
             "-P", ctx.root / "extract.cmake")
    ctx.check("zip big member size and sentinel", big_file_ok(zdest / "zpayload/zbig.bin", size))
    ctx.check("zip small members", (zdest / "zpayload/z-end.txt").read_text() == "ZEND\n")
    remove_tree(zdest)


PRESETS_V13 = """
{
  "version": 13,
  "cmakeMinimumRequired": {"major": 4, "minor": 5, "patch": 0},
  "configurePresets": [
    {"name": "base", "hidden": true, "generator": "Ninja", "binaryDir": "${sourceDir}/out/${presetName}",
     "cacheVariables": {"CMAKE_BUILD_TYPE": "Release", "SEEN_PRESET": "${configurePresetName}"}},
    {"name": "dev", "inherits": "base"},
    {"name": "off", "inherits": "base", "condition": false}
  ],
  "buildPresets": [
    {"name": "dev", "configurePreset": "dev"},
    {"name": "dry", "configurePreset": "dev", "nativeToolOptions": ["-n"]}
  ],
  "testPresets": [
    {"name": "dev", "configurePreset": "dev", "environment": {"CFG_NAME": "${configurePresetName}"},
     "output": {"outputOnFailure": true}}
  ],
  "packagePresets": [
    {"name": "dev", "configurePreset": "dev", "generators": ["TGZ"]}
  ],
  "workflowPresets": [
    {"name": "dev", "steps": [
      {"type": "configure", "name": "dev"}, {"type": "build", "name": "dev"},
      {"type": "test", "name": "dev"}, {"type": "package", "name": "dev"}]}
  ]
}
"""


def _check_presets_workflow_and_options(ctx: Ctx, src: Path, out: Path) -> None:
    wf = ctx.cmake("--workflow", "--preset", "dev", cwd=src)
    ctx.ok("workflow preset", wf)
    steps = re.findall(r"(?m)^Executing workflow step .*$", wf.output)
    ctx.check("workflow ran 4 steps", len(steps) == 4, steps)
    ctx.check("workflow tests passed", "100% tests passed out of 2" in wf.output, wf.tail())
    ctx.check("workflow package", (out / "dev/cli-pkg.tar.gz").is_file())
    ctx.check("${configurePresetName} in cache",
              cache_entries(out / "dev", ["SEEN_PRESET"]).get("SEEN_PRESET", "").endswith("=dev"))
    ctx.observe("workflow steps", steps)

    ctx.ok("plain configure of out/off", ctx.cmake("-S", src, "-B", out / "off", "-G", "Ninja", cwd=src))
    off = ctx.cmake("--preset", "off", cwd=src)
    ctx.fails("disabled preset is rejected", off)
    ctx.check("disabled preset message", "disabled" in off.stderr, off.tail())
    ctx.observe("disabled preset", [off.returncode != 0, off.stderr.strip()])

    (out / "dev/a").unlink(missing_ok=True)
    dry = ctx.cmake("--build", "--preset", "dry", cwd=src)
    ctx.ok("build preset with nativeToolOptions -n", dry)
    ctx.check("dry run does not link", not (out / "dev/a").exists())
    real = ctx.cmake("--build", "--preset", "dry", "--", "-v", cwd=src)
    ctx.ok("build preset with -- -v", real)
    ctx.check("-- options replace nativeToolOptions", (out / "dev/a").is_file(), real.tail())


def _check_targets_install_and_subdir_build(ctx: Ctx, src: Path, out: Path) -> None:
    r = ctx.root
    ctx.ok("clean", ctx.cmake("--build", out / "dev", "--target", "clean"))
    tgt = ctx.cmake("--build", out / "dev", "--target", "a;b")
    ctx.ok("--build --target 'a;b'", tgt)
    built = [name for name in ("a", "b", "c") if (out / "dev" / name).is_file()]
    ctx.check("only a and b are built", built == ["a", "b"], built)
    ctx.observe("target a;b built", built)

    for label, args in (("A;B", ["--component", "A;B"]), ("A B", ["--component", "A", "--component", "B"])):
        prefix = r / f"inst-{slug(label)}"
        proc = ctx.cmake("--install", out / "dev", "--prefix", prefix, *args)
        ctx.ok(f"--install --component {label}", proc)
        listing = file_listing(prefix) if prefix.exists() else []
        ctx.check(f"component {label} installs a and b", listing == ["bin/a", "bin/b"], listing)
        ctx.observe(f"install {label}", listing)

    sub_ninja = ctx.cmake("--build", "out/dev/sub", cwd=src)
    ctx.observe("build ninja subdir", [sub_ninja.returncode != 0, "build.ninja" in sub_ninja.output])
    ctx.fails("--build out/dev/sub fails for Ninja", sub_ninja)
    if shutil.which("make"):
        ctx.ok("configure Makefiles tree", ctx.configure(src, out / "mk", "Unix Makefiles"))
        sub_make = ctx.cmake("--build", "out/mk/sub", cwd=src)
        ctx.ok("--build out/mk/sub", sub_make)
        ctx.check("subdirectory target built", (out / "mk/sub/s").is_file())
        ctx.check("other targets not built", not (out / "mk/a").exists())


def _check_cli_flags_sarif_fresh_and_cat(ctx: Ctx, src: Path) -> None:
    r = ctx.root
    cli = r / "cli"
    sarif = r / "diag.sarif"
    first = ctx.cmake("-S", src, "-B", cli, "-G", "Ninja", "-C", src / "init.cmake", "-DOVR=cli",
                      "-DVAR_A=1", "-DVAR_B=2", "-DVAR_C=3", f"--sarif-output={sarif}")
    ctx.ok("configure with -C and -D", first)
    ctx.check("-D overrides -C", "VALUES PRE=from-preload OVR=cli VAR_A=1 VAR_B=2 VAR_C=3" in first.stdout, first.tail())
    ctx.check("warning backtrace", "CMake Warning at cmake/warn.cmake:2 (message):" in first.stderr, first.stderr)
    order = ctx.cmake("-S", src, "-B", r / "cli2", "-G", "Ninja", "-DOVR=cli-first", "-C", src / "init.cmake")
    ctx.ok("configure with -D before -C", order)
    ctx.check("-D before -C still wins", "OVR=cli-first" in order.stdout, order.tail())
    unset = ctx.cmake("-U", "VAR_[AB]", cli)
    ctx.ok("-U 'VAR_[AB]'", unset)
    left = cache_entries(cli, ["VAR_A", "VAR_B", "VAR_C"])
    ctx.check("-U removes VAR_A and VAR_B only", sorted(left) == ["VAR_C"], left)
    ctx.observe("values", re.findall(r"(?m)^-- VALUES .*$", first.stdout + unset.stdout))
    ctx.observe("warning", first.stderr.strip())
    if sarif.is_file():
        digest = sarif_digest(sarif)
        ctx.observe("sarif", digest)
        warn = [item for item in digest if item["message"] == "deliberate warning"]
        stack = warn[0]["stacks"][0] if warn and warn[0]["stacks"] else []
        ctx.check("SARIF warning location",
                  bool(warn) and warn[0]["locations"] == ["CMAKE_SOURCE_DIR:cmake/warn.cmake:2:message"], digest)
        ctx.check("SARIF stack frames", stack == [
            "CMAKE_SOURCE_DIR:cmake/warn.cmake:2:message",
            "CMAKE_SOURCE_DIR:cmake/warn.cmake:5:emit_warning",
            "CMAKE_SOURCE_DIR:cmake/warn.cmake:7:outer",
            "CMAKE_SOURCE_DIR:CMakeLists.txt:4:include",
        ], stack)
    else:
        ctx.check("SARIF file written", False, str(sarif))

    fresh = ctx.cmake("-S", src, "-B", r / "fresh", "-G", "Ninja", "--fresh")
    ctx.ok("--fresh without CMakeCache.txt", fresh)
    ctx.check("--fresh writes a cache", (r / "fresh/CMakeCache.txt").is_file())

    ctx.write("-dashfile", "dash file\n")
    cats = {}
    for label, args in (("- -dashfile", ["-", "-dashfile"]), ("- ./-dashfile", ["-", "./-dashfile"]),
                        ("-- -dashfile", ["--", "-dashfile"]), ("- -- -dashfile", ["-", "--", "-dashfile"])):
        proc = ctx.cmake("-E", "cat", *args, stdin="from stdin\n")
        cats[label] = [proc.returncode, proc.stdout, proc.stderr.strip()]
    ctx.check("-E cat - -dashfile", cats["- -dashfile"][:2] == [0, "from stdin\ndash file\n"], cats)
    ctx.check("-E cat -- -dashfile", cats["-- -dashfile"][:2] == [0, "dash file\n"], cats)
    ctx.observe("cat", cats)


@scenario("s10", "Presets v13 workflow + CLI matrix (disabled preset, nativeToolOptions, -C/-D/-U, --fresh, SARIF, -E cat)")
def s10_presets_cli(ctx: Ctx) -> None:
    src = ctx.root / "src"
    ctx.tree(src, {
        "CMakeLists.txt": """
            cmake_minimum_required(VERSION 4.0)
            project(cli C)
            enable_testing()
            include(cmake/warn.cmake)
            add_executable(a a.c)
            add_executable(b b.c)
            add_executable(c c.c)
            add_subdirectory(sub)
            install(TARGETS a COMPONENT A DESTINATION bin)
            install(TARGETS b COMPONENT B DESTINATION bin)
            install(TARGETS c COMPONENT C DESTINATION bin)
            add_test(NAME ta COMMAND a)
            add_test(NAME envcheck COMMAND ${CMAKE_COMMAND} -P ${CMAKE_SOURCE_DIR}/check_env.cmake)
            message(STATUS "VALUES PRE=${PRE} OVR=${OVR} VAR_A=${VAR_A} VAR_B=${VAR_B} VAR_C=${VAR_C}")
            set(CPACK_GENERATOR TGZ)
            set(CPACK_PACKAGE_FILE_NAME cli-pkg)
            include(CPack)
            """,
        "cmake/warn.cmake": """
            function(emit_warning)
              message(WARNING "deliberate warning")
            endfunction()
            macro(outer)
              emit_warning()
            endmacro()
            outer()
            """,
        "check_env.cmake": """
            if(NOT "$ENV{CFG_NAME}" STREQUAL "dev")
              message(FATAL_ERROR "CFG_NAME=[$ENV{CFG_NAME}]")
            endif()
            """,
        "init.cmake": """
            set(PRE "from-preload" CACHE STRING "")
            set(OVR "from-preload" CACHE STRING "")
            """,
        "sub/CMakeLists.txt": "add_executable(s s.c)\n",
        "sub/s.c": "int main(void) { return 0; }\n",
        "a.c": "int main(void) { return 0; }\n",
        "b.c": "int main(void) { return 0; }\n",
        "c.c": "int main(void) { return 0; }\n",
        "CMakePresets.json": PRESETS_V13,
    })
    out = src / "out"
    _check_presets_workflow_and_options(ctx, src, out)
    _check_targets_install_and_subdir_build(ctx, src, out)
    _check_cli_flags_sarif_fresh_and_cat(ctx, src)


CTEST_PROJECT = {
    "CMakeLists.txt": """
        cmake_minimum_required(VERSION 4.0)
        project(ct C)
        enable_testing()
        set(S ${CMAKE_SOURCE_DIR}/scripts)
        set(W ${CMAKE_BINARY_DIR}/work)
        file(MAKE_DIRECTORY ${W})

        # gtest-style discovery: the build lists the cases and writes add_test() calls.
        add_executable(gt gt.c)
        set(disc ${CMAKE_CURRENT_BINARY_DIR}/gt_tests.cmake)
        add_custom_command(TARGET gt POST_BUILD
          COMMAND ${CMAKE_COMMAND} -DEXE=$<TARGET_FILE:gt> -DOUT=${disc} -P ${S}/discover.cmake)
        set_property(DIRECTORY APPEND PROPERTY TEST_INCLUDE_FILES ${disc})

        # ENVIRONMENT: APPEND adds entries, a second set_tests_properties replaces them.
        add_test(NAME env_append COMMAND ${CMAKE_COMMAND} -DHAVE=A=1|B=2 -DABSENT=X -P ${S}/checkenv.cmake)
        set_tests_properties(env_append PROPERTIES ENVIRONMENT "A=1")
        set_property(TEST env_append APPEND PROPERTY ENVIRONMENT "B=2")
        add_test(NAME env_replace COMMAND ${CMAKE_COMMAND} -DHAVE=Y=2 -DABSENT=X -P ${S}/checkenv.cmake)
        set_tests_properties(env_replace PROPERTIES ENVIRONMENT "X=1")
        set_tests_properties(env_replace PROPERTIES ENVIRONMENT "Y=2")
        # A bare name without "=" in ENVIRONMENT removes the variable.
        add_test(NAME env_unset COMMAND ${CMAKE_COMMAND} -DHAVE=KEEP=outer -DABSENT=FOO -P ${S}/checkenv.cmake)
        set_tests_properties(env_unset PROPERTIES ENVIRONMENT "FOO")

        # DEPENDS appended twice: dep_c must start after dep_a and dep_b.
        foreach(t dep_a dep_b)
          add_test(NAME ${t} COMMAND ${CMAKE_COMMAND} -DNAME=${t} -DLOG=${W}/deps.log -P ${S}/slow_append.cmake)
        endforeach()
        add_test(NAME dep_c COMMAND ${CMAKE_COMMAND} -DLOG=${W}/deps.log -P ${S}/check_deps.cmake)
        set_property(TEST dep_c APPEND PROPERTY DEPENDS dep_a)
        set_property(TEST dep_c APPEND PROPERTY DEPENDS dep_b)

        # Fixtures with FIXTURES_REQUIRED appended twice.
        add_test(NAME fx_setup COMMAND ${CMAKE_COMMAND} -E touch ${W}/fx.ready)
        add_test(NAME fx_setup2 COMMAND ${CMAKE_COMMAND} -E touch ${W}/fx2.ready)
        add_test(NAME fx_use COMMAND ${CMAKE_COMMAND} -DFILES=${W}/fx.ready|${W}/fx2.ready -P ${S}/check_files.cmake)
        add_test(NAME fx_cleanup COMMAND ${CMAKE_COMMAND} -E rm -f ${W}/fx.ready)
        set_tests_properties(fx_setup PROPERTIES FIXTURES_SETUP fx)
        set_tests_properties(fx_setup2 PROPERTIES FIXTURES_SETUP fx2)
        set_tests_properties(fx_cleanup PROPERTIES FIXTURES_CLEANUP fx)
        set_property(TEST fx_use APPEND PROPERTY FIXTURES_REQUIRED fx)
        set_property(TEST fx_use APPEND PROPERTY FIXTURES_REQUIRED fx2)

        # RESOURCE_LOCK appended: the two tests must not overlap.
        foreach(t lock_a lock_b)
          add_test(NAME ${t} COMMAND ${CMAKE_COMMAND} -DLOCK=${W}/lock.held -P ${S}/hold_lock.cmake)
          set_property(TEST ${t} APPEND PROPERTY RESOURCE_LOCK other)
          set_property(TEST ${t} APPEND PROPERTY RESOURCE_LOCK shared_res)
        endforeach()

        # TIMEOUT_AFTER_MATCH: the timeout becomes 1 second after "READY".
        add_test(NAME tam COMMAND sh -c "echo READY; exec sleep 30")
        set_tests_properties(tam PROPERTIES TIMEOUT 120 TIMEOUT_AFTER_MATCH "1;READY")

        # --stop-on-failure with a fixture.
        add_test(NAME sf_setup COMMAND ${CMAKE_COMMAND} -E touch ${W}/sf.setup)
        add_test(NAME sf_fail COMMAND ${CMAKE_COMMAND} -E false)
        add_test(NAME sf_later COMMAND ${CMAKE_COMMAND} -E touch ${W}/sf.later)
        add_test(NAME sf_cleanup COMMAND ${CMAKE_COMMAND} -E touch ${W}/sf.cleanup)
        set_tests_properties(sf_setup PROPERTIES FIXTURES_SETUP sf)
        set_tests_properties(sf_fail sf_later PROPERTIES FIXTURES_REQUIRED sf)
        set_tests_properties(sf_later PROPERTIES DEPENDS sf_fail)
        set_tests_properties(sf_cleanup PROPERTIES FIXTURES_CLEANUP sf)
        """,
    "gt.c": """
        #include <stdio.h>
        #include <string.h>
        static const char *cases[] = {"alpha", "beta", "gamma"};
        int main(int argc, char **argv) {
          if (argc > 1 && strcmp(argv[1], "--list") == 0) {
            for (int i = 0; i < 3; ++i) puts(cases[i]);
            return 0;
          }
          for (int i = 0; i < 3; ++i)
            if (argc > 1 && strcmp(argv[1], cases[i]) == 0) { printf("case %s ok\\n", cases[i]); return 0; }
          return 1;
        }
        """,
    "scripts/discover.cmake": """
        execute_process(COMMAND "${EXE}" --list OUTPUT_VARIABLE names RESULT_VARIABLE rv)
        if(NOT rv EQUAL 0)
          message(FATAL_ERROR "listing failed")
        endif()
        string(REPLACE "\\n" ";" names "${names}")
        set(content "")
        foreach(n IN LISTS names)
          if(NOT n STREQUAL "")
            string(APPEND content "add_test([=[gt.${n}]=] [==[${EXE}]==] ${n})\\n")
            string(APPEND content "set_tests_properties([=[gt.${n}]=] PROPERTIES LABELS discovered PASS_REGULAR_EXPRESSION \\"case ${n} ok\\")\\n")
          endif()
        endforeach()
        file(WRITE "${OUT}" "${content}")
        """,
    "scripts/checkenv.cmake": """
        string(REPLACE "|" ";" HAVE "${HAVE}")
        foreach(pair IN LISTS HAVE)
          string(REGEX MATCH "^([^=]+)=(.*)$" _ "${pair}")
          if(NOT "$ENV{${CMAKE_MATCH_1}}" STREQUAL "${CMAKE_MATCH_2}")
            message(FATAL_ERROR "${CMAKE_MATCH_1}=[$ENV{${CMAKE_MATCH_1}}], want [${CMAKE_MATCH_2}]")
          endif()
        endforeach()
        foreach(name IN LISTS ABSENT)
          if(DEFINED ENV{${name}})
            message(FATAL_ERROR "${name} is set to [$ENV{${name}}]")
          endif()
        endforeach()
        """,
    "scripts/slow_append.cmake": """
        execute_process(COMMAND "${CMAKE_COMMAND}" -E sleep 1)
        file(APPEND "${LOG}" "${NAME}\\n")
        """,
    "scripts/check_deps.cmake": """
        file(STRINGS "${LOG}" lines)
        list(SORT lines)
        if(NOT lines STREQUAL "dep_a;dep_b")
          message(FATAL_ERROR "dependencies did not finish first: [${lines}]")
        endif()
        """,
    "scripts/check_files.cmake": """
        string(REPLACE "|" ";" FILES "${FILES}")
        foreach(f IN LISTS FILES)
          if(NOT EXISTS "${f}")
            message(FATAL_ERROR "missing ${f}")
          endif()
        endforeach()
        """,
    "scripts/hold_lock.cmake": """
        if(EXISTS "${LOCK}")
          message(FATAL_ERROR "the resource lock was not respected")
        endif()
        file(TOUCH "${LOCK}")
        execute_process(COMMAND "${CMAKE_COMMAND}" -E sleep 1)
        file(REMOVE "${LOCK}")
        """,
}

DASHBOARD_PROJECT = {
    "CMakeLists.txt": """
        cmake_minimum_required(VERSION 4.0)
        project(dash C)
        enable_testing()
        file(WRITE ${CMAKE_BINARY_DIR}/build_type.txt "${CMAKE_BUILD_TYPE}")
        add_executable(dapp dapp.c)
        add_test(NAME always COMMAND dapp)
        add_test(NAME debug_only COMMAND ${CMAKE_COMMAND} -E touch ${CMAKE_BINARY_DIR}/debug.flag CONFIGURATIONS Debug)
        add_test(NAME release_only COMMAND ${CMAKE_COMMAND} -E touch ${CMAKE_BINARY_DIR}/release.flag CONFIGURATIONS Release)
        """,
    "dapp.c": "int main(void) { return 0; }\n",
    "dash.cmake": """
        set(CTEST_SOURCE_DIRECTORY "${SRC}")
        set(CTEST_BINARY_DIRECTORY "${BIN}")
        set(CTEST_CMAKE_GENERATOR "Ninja")
        set(CTEST_CONFIGURATION_TYPE Debug)
        set(CTEST_SITE indep)
        set(CTEST_BUILD_NAME dash)
        ctest_start(Experimental)
        ctest_configure(RETURN_VALUE conf_rv)
        ctest_build(RETURN_VALUE build_rv)
        ctest_test(RETURN_VALUE test_rv)
        message("DASH configure=${conf_rv} build=${build_rv} test=${test_rv}")
        """,
}


@scenario("s11", "CTest matrix (dynamic discovery, appended properties, TIMEOUT_AFTER_MATCH, --stop-on-failure, ctest -S, JUnit)")
def s11_ctest(ctx: Ctx) -> None:
    src = ctx.tree(ctx.root / "src", CTEST_PROJECT)
    build = ctx.root / "build"
    ctx.ok("configure", ctx.configure(src, build, "Ninja"))
    ctx.ok("build", ctx.build(build, "Ninja"))
    env = {"FOO": "outer", "KEEP": "outer", "X": None}

    listing = ctx.ctest("--test-dir", build, "-N", env=env)
    names = re.findall(r"(?m)^\s*Test\s+#\d+: (\S+)$", listing.stdout)
    ctx.check("discovered tests are listed", {"gt.alpha", "gt.beta", "gt.gamma"} <= set(names), names)
    ctx.observe("test list", names)

    junit = ctx.root / "junit.xml"
    main_run = ctx.ctest("--test-dir", build, "-j4", "-E", "^(tam|sf_.*)$", "--output-on-failure",
                         "--output-junit", junit, env=env)
    ctx.check("main test set passes", main_run.ok and "100% tests passed" in main_run.stdout, main_run.tail(60))
    if junit.is_file():
        cases = junit_cases(junit)
        ctx.check("JUnit lists every test as passed", len(cases) == 15 and all(c.endswith(":run") for c in cases),
                  cases)
        ctx.observe("junit", cases)
    else:
        ctx.check("JUnit file written", False, str(junit))
    labels = ctx.ctest("--test-dir", build, "-L", "discovered", env=env)
    ctx.check("label of discovered tests", labels.ok and "100% tests passed out of 3" in labels.stdout, labels.tail())

    tam = ctx.ctest("--test-dir", build, "-R", "^tam$", env=env)
    ctx.fails("TIMEOUT_AFTER_MATCH test fails", tam)
    ctx.check("TIMEOUT_AFTER_MATCH reports a timeout", "***Timeout" in tam.stdout, tam.tail())
    ctx.check("TIMEOUT_AFTER_MATCH ends early", tam.seconds < 15, f"{tam.seconds:.1f}s")
    ctx.observe("tam", [line.split("  ")[0] for line in tam.stdout.splitlines() if "tam" in line and "Test" in line])

    work = build / "work"
    stop = ctx.ctest("--test-dir", build, "-R", "^sf_", "--stop-on-failure", env=env)
    ran = [name for name in ("setup", "later", "cleanup") if (work / f"sf.{name}").exists()]
    ctx.fails("--stop-on-failure run fails", stop)
    ctx.check("--stop-on-failure ran the setup only", ran == ["setup"], ran)
    ctx.observe("stop-on-failure", ran)
    for name in ("setup", "later", "cleanup"):
        (work / f"sf.{name}").unlink(missing_ok=True)
    full = ctx.ctest("--test-dir", build, "-R", "^sf_", env=env)
    ran = [name for name in ("setup", "later", "cleanup") if (work / f"sf.{name}").exists()]
    ctx.fails("run without --stop-on-failure fails", full)
    ctx.check("without --stop-on-failure the cleanup runs", "setup" in ran and "cleanup" in ran, ran)
    ctx.observe("no stop-on-failure", ran)

    dsrc = ctx.tree(ctx.root / "dash", DASHBOARD_PROJECT)
    dbin = ctx.root / "dash-build"
    dash = ctx.ctest("-S", dsrc / "dash.cmake", f"-DSRC={dsrc}", f"-DBIN={dbin}", "-VV")
    ctx.ok("ctest -S dashboard", dash, required=False)
    ctx.check("dashboard steps succeed", "DASH configure=0 build=0 test=0" in dash.output, dash.tail())
    build_type = (dbin / "build_type.txt").read_text() if (dbin / "build_type.txt").is_file() else None
    ctx.check("ctest_configure passes CMAKE_BUILD_TYPE", build_type == "Debug", build_type)
    ctx.check("ctest_test uses CTEST_CONFIGURATION_TYPE",
              (dbin / "debug.flag").exists() and not (dbin / "release.flag").exists())
    ctx.observe("dashboard", [build_type, (dbin / "debug.flag").exists(), (dbin / "release.flag").exists()])


CPACK_PROJECT = {
    "CMakeLists.txt": """
        cmake_minimum_required(VERSION 4.0)
        project(pk VERSION 1.0 LANGUAGES C)
        add_executable(tool tool.c)
        target_compile_options(tool PRIVATE -g)
        install(TARGETS tool RUNTIME DESTINATION bin COMPONENT runtime)
        install(FILES doc.txt DESTINATION share/doc COMPONENT docs)
        install(FILES debug_only.txt DESTINATION share/doc CONFIGURATIONS Debug COMPONENT docs)
        set(CPACK_PACKAGE_NAME pk)
        set(CPACK_SYSTEM_NAME sys)
        set(CPACK_GENERATOR "TGZ;ZIP")
        set(CPACK_ARCHIVE_COMPONENT_INSTALL ON)
        set(CPACK_STRIP_FILES ON)
        set(CPACK_PRE_BUILD_SCRIPTS ${CMAKE_SOURCE_DIR}/pre.cmake)
        set(CPACK_PRE_LOG ${CMAKE_BINARY_DIR}/pre.log)
        include(CPack)
        """,
    "tool.c": """
        #include <stdio.h>
        static int __attribute__((noinline)) local_marker_fn(int x) { return x * 3; }
        int main(int argc, char **argv) { (void)argv; printf("tool %d\\n", local_marker_fn(argc + 1)); return 0; }
        """,
    "doc.txt": "documentation\n",
    "debug_only.txt": "debug notes\n",
    "pre.cmake": """
        if(DEFINED PRE_SCRIPT_RAN)
          set(leak "leaked")
        else()
          set(leak "clean")
        endif()
        set(PRE_SCRIPT_RAN 1)
        file(APPEND "${CPACK_PRE_LOG}" "pre ${CPACK_GENERATOR} config=${CPACK_BUILD_CONFIG} ${leak}\\n")
        """,
}


@scenario("s12", "CPack matrix (component TGZ/ZIP, INSTALLED_DIRECTORIES + PACKAGING_INSTALL_PREFIX, PRE_BUILD_SCRIPTS, STRIP, -C)")
def s12_cpack(ctx: Ctx) -> None:
    if "Ninja Multi-Config" not in generators_available(ctx.options):
        raise ScenarioSkip("needs Ninja")
    src = ctx.tree(ctx.root / "src", CPACK_PROJECT)
    build = ctx.root / "build"
    ctx.ok("configure", ctx.configure(src, build, "Ninja Multi-Config"))
    for config in ("Debug", "Release"):
        ctx.ok(f"build {config}", ctx.build(build, "Ninja Multi-Config", config=config))
    contents: dict[str, Any] = {}
    for config in ("Debug", "Release"):
        out = ctx.root / f"pkg-{config}"
        (build / "pre.log").unlink(missing_ok=True)
        proc = ctx.cpack("-C", config, "-B", out, cwd=build)
        ctx.ok(f"cpack -C {config}", proc, required=False)
        packages = sorted(p.name for p in out.glob("*") if p.is_file())
        want = sorted(f"pk-1.0-sys-{comp}.{ext}" for comp in ("docs", "runtime") for ext in ("tar.gz", "zip"))
        ctx.check(f"{config}: one archive per component and generator", packages == want, packages)
        for name in packages:
            contents[f"{config}/{name}"] = archive_names(out / name)
        docs = contents.get(f"{config}/pk-1.0-sys-docs.tar.gz", [])
        has_debug = any(name.endswith("share/doc/debug_only.txt") for name in docs)
        ctx.check(f"{config}: CONFIGURATIONS Debug file follows -C", has_debug == (config == "Debug"), docs)
        log = (build / "pre.log").read_text().splitlines() if (build / "pre.log").is_file() else []
        ctx.check(f"{config}: pre-build script runs once per generator with a clean scope",
                  sorted(log) == sorted([f"pre TGZ config={config} clean", f"pre ZIP config={config} clean"]), log)
        ctx.observe(f"{config}: pre-build log", sorted(log))
        runtime = out / "pk-1.0-sys-runtime.tar.gz"
        if runtime.is_file():
            unpack = ctx.root / f"unpack-{config}"
            with tarfile.open(runtime) as tar:
                tar.extractall(unpack, filter="tar")
            tools = list(unpack.rglob("bin/tool"))
            if tools:
                run = ctx.run([tools[0]])
                ctx.check(f"{config}: packaged tool runs", run.stdout == "tool 6\n", run.tail())
                nm = subprocess.run(["nm", str(tools[0])], capture_output=True, text=True, timeout=60).stdout
                ctx.check(f"{config}: CPACK_STRIP_FILES strips local symbols", "local_marker_fn" not in nm, nm[:500])
            else:
                ctx.check(f"{config}: runtime archive has bin/tool", False, archive_names(runtime))
    built_nm = subprocess.run(["nm", str(build / "Debug/tool")], capture_output=True, text=True, timeout=60).stdout
    ctx.check("the build-tree tool keeps local symbols", "local_marker_fn" in built_nm, built_nm[:500])
    ctx.observe("archives", contents)

    ctx.write("payload/data/info.txt", "installed directory payload\n")
    ctx.write("payload/data/nested/deep.txt", "deep\n")
    config_file = ctx.write("installed.cmake", f"""
        set(CPACK_GENERATOR TGZ)
        set(CPACK_PACKAGE_NAME inst)
        set(CPACK_PACKAGE_VERSION 2.0)
        set(CPACK_PACKAGE_DESCRIPTION "Installed directory package")
        set(CPACK_SYSTEM_NAME sys)
        set(CPACK_PACKAGE_FILE_NAME inst-pkg)
        set(CPACK_INSTALL_CMAKE_PROJECTS "")
        set(CPACK_INSTALLED_DIRECTORIES "{ctx.root}/payload;share/payload")
        set(CPACK_PACKAGING_INSTALL_PREFIX "/opt/inst")
        set(CPACK_INCLUDE_TOPLEVEL_DIRECTORY OFF)
        """)
    out = ctx.root / "pkg-installed"
    proc = ctx.cpack("--config", config_file, "-B", out, cwd=ctx.root)
    ctx.ok("cpack CPACK_INSTALLED_DIRECTORIES", proc, required=False)
    archive = out / "inst-pkg.tar.gz"
    names = archive_names(archive) if archive.is_file() else []
    ctx.check("installed directory under the packaging prefix", names == [
        "opt/inst/share/payload/data/info.txt", "opt/inst/share/payload/data/nested/deep.txt"], names)
    ctx.observe("installed directories", names)


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
RCMAKE_GENERATORS = ("Ninja", "Ninja Multi-Config", "Unix Makefiles", "NMake Makefiles", "Xcode")


def supported_capabilities(data: Any) -> Any:
    """Return "cmake -E capabilities" output without the parts Rcmake omits by design.

    Rcmake omits legacy extra generators ("extraGenerators") and unimplemented
    generators (Watcom WMake, FASTBuild, etc.). Only the generators that Rcmake
    implements stay, sorted by name. Visual Studio generators stay too. All
    other members (including "debugger") stay as they are.
    """
    if not isinstance(data, Mapping):
        return data
    out = dict(data)
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


def _check_e_archives(ctx: Ctx, cm: Callable[..., Proc], source: list[str]) -> None:
    r = ctx.root
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
        out = r / f"x-{slug(name)}"
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


def _check_e_hashes_and_compare(ctx: Ctx, cm: Callable[..., Proc]) -> None:
    r = ctx.root
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


def _check_e_copies_and_links(ctx: Ctx, cm: Callable[..., Proc], source: list[str]) -> None:
    r = ctx.root
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

    link1 = cm("create_symlink", "lf.txt", "sl")
    link2 = cm("create_symlink", "crlf.txt", "sl")
    target = os.readlink(r / "sl") if (r / "sl").is_symlink() else ""
    ctx.check("create_symlink replaces an existing link", link1.ok and link2.ok and target == "crlf.txt",
              [link1.tail(), link2.tail(), target])
    ctx.observe("create_symlink", [result(link1), result(link2), target])


def _check_e_env_cat_and_capabilities(ctx: Ctx, cm: Callable[..., Proc]) -> None:
    show = 'printf "%s|%s|%s|%s|%s\\n" "$FOO" "${PATH%%:*}" "${HOME-unset}" "${EMPTY-unset}" "$LIST"'
    mod = cm("env", "--modify", "PATH=path_list_prepend:/opt/indep", "--modify", "FOO=set:bar",
             "--modify", "FOO=string_append:baz", "--modify", "LIST=cmake_list_append:x",
             "--modify", "LIST=cmake_list_prepend:w", "--unset=HOME", "EMPTY=", "--", "sh", "-c", show)
    ctx.check("env --modify", mod.ok and mod.stdout == "barbaz|/opt/indep|unset||w;x\n", mod.tail())
    bad_env = cm("env", "--modify", "BAD=nope:x", "--", "true")
    ctx.check("env --modify with a bad operation fails", bad_env.returncode != 0
              and "invalid parameter to --modify" in bad_env.stderr, bad_env.tail())
    ctx.observe("env", [result(mod), result(bad_env)])

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


@scenario("s19", "cmake -E gauntlet: tar/zip/7z formats + --mtime + selective extract, hashes, copy, symlink, env, cat, compare_files, capabilities")
def s19_cmake_e(ctx: Ctx) -> None:
    r = ctx.root
    e_fixture(ctx)
    env = {"TZ": "UTC"}

    def cm(*args: str, cwd: Path | None = None, stdin: str | None = None) -> Proc:
        return ctx.cmake("-E", *args, cwd=cwd or r, env=env, stdin=stdin)

    source = tree_digest(r / "d")
    ctx.observe("source digest", source)
    _check_e_archives(ctx, cm, source)
    _check_e_hashes_and_compare(ctx, cm)
    _check_e_copies_and_links(ctx, cm, source)
    _check_e_env_cat_and_capabilities(ctx, cm)


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


def make_git_repo(ctx: Ctx, repo: Path) -> str:
    """Make a local git repository whose v1.0 tag holds MATHLIB_FILES. Return the tag commit."""
    ctx.tree(repo, MATHLIB_FILES)

    def git(*args: str) -> Proc:
        return ctx.run(["git", "-c", "init.defaultBranch=main", "-c", "commit.gpgsign=false",
                        "-c", "tag.gpgsign=false", *args], cwd=repo, env=GIT_ENV)
    for args in (("init", "-q"), ("add", "."), ("commit", "-q", "-m", "mathlib 1.0"), ("tag", "v1.0")):
        ctx.ok(f"git {args[0]}", git(*args))
    tag = git("rev-parse", "HEAD").stdout.strip()
    # A later commit breaks the library, so the build proves that GIT_TAG selected v1.0.
    (repo / "mathlib.c").write_text('#error "HEAD is not v1.0"\n', encoding="utf-8")
    ctx.ok("git commit after the tag", git("commit", "-q", "-am", "after the tag"))
    return tag


def _check_probe_packages(ctx: Ctx, build: Path) -> None:
    r = ctx.root
    if not (build / "CPackConfig.cmake").is_file():
        return
    pack = ctx.cpack("-G", "TGZ;ZIP", "--config", build / "CPackConfig.cmake", "-B", r / "pkg", cwd=build)
    ctx.ok("cpack TGZ;ZIP", pack)
    tgz, zipf = r / "pkg/probe-0.4.2-indep.tar.gz", r / "pkg/probe-0.4.2-indep.zip"
    expect = ["probe-0.4.2-indep/bin/probe", "probe-0.4.2-indep/share/doc/probe/README.txt"]
    for path in (tgz, zipf):
        names = sorted(n for n in archive_names(path) if not n.endswith("/")) if path.is_file() else []
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


@scenario("s20", "Real-world probe: Check* modules + TestBigEndian + Threads + offline FetchContent (git + tarball) + configure_file + CTest JUnit + CPack TGZ/ZIP")
def s20_real_world(ctx: Ctx) -> None:
    r = ctx.root
    gens = [g for g in ("Ninja", "Unix Makefiles") if g in generators_available(ctx.options)]
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
        tag_gen = slug(gen)
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
            ctx.observe(f"{gen}: check cache", cache_entries(build, [
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
            cases = junit_cases(junit) if junit.is_file() else []
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

    _check_probe_packages(ctx, r / f"b-{slug(gens[0])}")
