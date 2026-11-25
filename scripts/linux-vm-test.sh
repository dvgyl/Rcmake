#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd -P)"

elf_rpath() {
  LC_ALL=C readelf -d "$1" |
    awk '/\((RPATH|RUNPATH)\)/ { sub(/^.*\[/, ""); sub(/\].*$/, ""); print }'
}

check_install_rpath() {
  local rpaths
  rpaths="$(elf_rpath "$1")"
  if [[ "$rpaths" != '$ORIGIN/../lib' ]]; then
    printf 'Unexpected ELF runtime path in %s. Found %s.\n' "$1" "$rpaths" >&2
    return 1
  fi
}

smoke() (
  local bin="$1" platform="$2" work source generator compiler cxx compiler_id
  local exe="" python=python3
  bin="$(cd "$bin" && pwd -P)"
  work="$(mktemp -d "${TMPDIR:-/tmp}/rcmake-smoke.XXXXXXXX")"
  trap 'rm -rf "$work"' EXIT
  if [[ "$platform" == Windows ]]; then
    bin="$(cygpath -am "$bin")"
    work="$(cygpath -am "$work")"
    exe=.exe
    python=python
  else
    work="$(cd "$work" && pwd -P)"
  fi
  source="$work/source"
  mkdir -p "$source"
  unset DESTDIR CMAKE_INSTALL_MODE RCMAKE_VENDORED_DATA_ROOT
  if [[ "$platform" == Linux ]]; then
    unset LD_LIBRARY_PATH LD_PRELOAD LD_AUDIT
  fi
  cat > "$source/CMakeLists.txt" <<'CMAKE'
cmake_minimum_required(VERSION 3.20)
project(WaveSmoke VERSION 1.0 LANGUAGES C CXX)

if(NOT CMAKE_C_COMPILER_ID STREQUAL EXPECTED_COMPILER OR
   NOT CMAKE_CXX_COMPILER_ID STREQUAL EXPECTED_COMPILER)
  message(FATAL_ERROR "Expected ${EXPECTED_COMPILER}. Found ${CMAKE_C_COMPILER_ID} and ${CMAKE_CXX_COMPILER_ID}.")
endif()

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
add_library(wave_static STATIC static.c)
add_library(wave_shared SHARED shared.c)
target_compile_definitions(wave_shared PRIVATE WAVE_EXPORT)
add_executable(wave_test main.cpp)
target_link_libraries(wave_test PRIVATE wave_static wave_shared)

if(APPLE)
  set_property(TARGET wave_test PROPERTY BUILD_RPATH "${CMAKE_CURRENT_BINARY_DIR}/build-only")
  set_property(TARGET wave_test PROPERTY INSTALL_RPATH "@loader_path/../lib")
elseif(UNIX)
  set_property(TARGET wave_test PROPERTY BUILD_RPATH "${CMAKE_CURRENT_BINARY_DIR}/build-only")
  set_property(TARGET wave_test PROPERTY INSTALL_RPATH "$ORIGIN/../lib")
endif()

enable_testing()
add_test(NAME wave_test COMMAND wave_test)
install(TARGETS wave_test wave_static wave_shared
  RUNTIME DESTINATION bin LIBRARY DESTINATION lib ARCHIVE DESTINATION lib)
set(CPACK_GENERATOR ZIP)
include(CPack)
CMAKE
  cat > "$source/shared.c" <<'C'
#if defined(_WIN32) && defined(WAVE_EXPORT)
__declspec(dllexport)
#endif
int shared_value(void) { return 23; }
C
  cat > "$source/static.c" <<'C'
int static_value(void) { return 19; }
C
  cat > "$source/main.cpp" <<'CPP'
extern "C" int static_value(void);
#ifdef _WIN32
extern "C" __declspec(dllimport) int shared_value(void);
#else
extern "C" int shared_value(void);
#endif
consteval int expected_value() { return 42; }
int main() { return static_value() + shared_value() == expected_value() ? 0 : 1; }
CPP

  local -a lanes
  case "$platform" in
    Linux) lanes=("gcc|g++|GNU|Ninja" "gcc|g++|GNU|Unix Makefiles"
                  "clang|clang++|Clang|Ninja" "clang|clang++|Clang|Unix Makefiles") ;;
    Darwin) lanes=("clang|clang++|AppleClang|Ninja" "clang|clang++|AppleClang|Ninja Multi-Config"
                   "clang|clang++|AppleClang|Unix Makefiles" "clang|clang++|AppleClang|Xcode") ;;
    Windows) lanes=("cl.exe|cl.exe|MSVC|Ninja" "||MSVC|Visual Studio 17 2022") ;;
  esac

  local lane index=0 build prefix dynamic rpaths static_library shared_library extracted
  local -a configure
  for lane in "${lanes[@]}"; do
    IFS='|' read -r compiler cxx compiler_id generator <<< "$lane"
    printf 'Test %s with %s.\n' "$compiler_id" "$generator"
    index=$((index + 1))
    build="$work/build-$index"
    prefix="$work/install-$index"
    configure=(-S "$source" -B "$build" -G "$generator" "-DEXPECTED_COMPILER=$compiler_id")
    case "$generator" in
      "Visual Studio 17 2022") configure+=(-A x64) ;;
      "Ninja Multi-Config"|Xcode) ;;
      *) configure+=(-DCMAKE_BUILD_TYPE=Release) ;;
    esac
    if [[ -n "$compiler" ]]; then
      configure+=("-DCMAKE_C_COMPILER=$compiler" "-DCMAKE_CXX_COMPILER=$cxx")
    fi
    "$bin/cmake$exe" "${configure[@]}"
    "$bin/cmake$exe" --build "$build" --config Release --parallel 2
    "$bin/ctest$exe" --test-dir "$build" -C Release --output-on-failure --no-tests=error
    if [[ "$platform" == Linux ]]; then
      rpaths="$(elf_rpath "$build/wave_test")"
      if [[ "$rpaths" == *$'\n'* || ":$rpaths:" != *":$build/build-only:"* ||
            ":$rpaths:" == *':$ORIGIN/../lib:'* ]]; then
        printf 'Unexpected build ELF runtime path %s.\n' "$rpaths" >&2
        exit 1
      fi
    fi
    "$bin/cmake$exe" --install "$build" --config Release --prefix "$prefix"

    case "$platform" in
      Linux)
        static_library=lib/libwave_static.a
        shared_library=lib/libwave_shared.so
        check_install_rpath "$prefix/bin/wave_test"
        ;;
      Darwin)
        static_library=lib/libwave_static.a
        shared_library=lib/libwave_shared.dylib
        dynamic="$(otool -l "$prefix/bin/wave_test")"
        rpaths="$(awk '/cmd LC_RPATH$/ { getline; getline; print $2 }' <<< "$dynamic")"
        if [[ "$rpaths" != '@loader_path/../lib' ]]; then
          printf 'Unexpected installed Mach-O runtime path %s.\n' "$rpaths" >&2
          exit 1
        fi
        # A Mach-O runtime path edit invalidates the Xcode code signature.
        if [[ "$generator" == Xcode ]]; then
          codesign --force --sign - "$prefix/bin/wave_test"
        fi
        ;;
      Windows)
        static_library=lib/wave_static.lib
        shared_library=bin/wave_shared.dll
        ;;
    esac
    test -f "$prefix/$static_library"
    test -f "$prefix/$shared_library"
    "$prefix/bin/wave_test$exe"

    "$bin/cpack$exe" --config "$build/CPackConfig.cmake" -G ZIP -C Release -B "$work/package-$index"
    extracted=""
    if [[ "$platform" == Linux ]]; then
      extracted="$work/extracted-$index"
    fi
    "$python" - "$work/package-$index" "$extracted" "bin/wave_test$exe" "$static_library" "$shared_library" <<'PY'
from pathlib import Path
import sys
from zipfile import ZipFile

directory, extracted, *required = sys.argv[1:]
archives = list(Path(directory).glob("*.zip"))
if len(archives) != 1:
    sys.exit(f"Expected one ZIP archive in {directory}.")
with ZipFile(archives[0]) as archive:
    for name in required:
        members = [member for member in archive.namelist()
                   if member == name or member.endswith(f"/{name}")]
        if len(members) != 1:
            sys.exit(f"Expected one {name} in {archives[0].name}.")
        if extracted:
            destination = Path(extracted) / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(archive.read(members[0]))
    if bad_file := archive.testzip():
        sys.exit(f"Invalid ZIP entry {bad_file}.")
PY
    if [[ -n "$extracted" ]]; then
      check_install_rpath "$extracted/bin/wave_test"
      chmod +x "$extracted/bin/wave_test"
      (cd "$extracted" && ./bin/wave_test)
    fi
  done
)

linux_integration() (
  local bin="$1" work generator build
  if ! command -v gfortran >/dev/null 2>&1; then
    printf 'The Linux integration tests require gfortran.\n' >&2
    exit 1
  fi
  work="$(mktemp -d)"
  trap 'rm -rf "$work"' EXIT
  cd "$work"

  printf 'int leaf(void) { return 42; }\n' > leaf.c
  printf 'extern int leaf(void); int parent(void) { return leaf(); }\n' > parent.c
  printf 'extern int parent(void); int main(void) { return parent() != 42; }\n' > main.c
  gcc -shared -fPIC leaf.c -Wl,-soname,librcmake_leaf.so -o librcmake_leaf.so
  gcc -shared -fPIC parent.c -Wl,-soname,librcmake_parent.so -Wl,-rpath,'$ORIGIN' \
    -L. -lrcmake_leaf -o librcmake_parent.so
  gcc main.c -Wl,-rpath,'$ORIGIN' -Wl,-rpath-link,. -L. -lrcmake_parent -o probe
  ./probe
  cat > dependencies.cmake <<'CMAKE'
cmake_minimum_required(VERSION 4.5)
file(GET_RUNTIME_DEPENDENCIES
  EXECUTABLES "${CMAKE_CURRENT_LIST_DIR}/probe"
  PRE_INCLUDE_REGEXES "^librcmake_(parent|leaf)[.]so$"
  PRE_EXCLUDE_REGEXES ".*"
  RESOLVED_DEPENDENCIES_VAR resolved
  UNRESOLVED_DEPENDENCIES_VAR unresolved)
if(unresolved)
  message(FATAL_ERROR "Unresolved ELF dependencies: ${unresolved}")
endif()
file(WRITE "${CMAKE_CURRENT_LIST_DIR}/resolved.txt" "${resolved}")
CMAKE
  "$bin/cmake" -P dependencies.cmake
  mv resolved.txt resolved-rcmake.txt
  timeout 30s "${RCMAKE_ORACLE_CMAKE:?Set RCMAKE_ORACLE_CMAKE to CMake 4.5.1.}" -P dependencies.cmake
  cmp resolved.txt resolved-rcmake.txt
  LC_ALL=C python3 - <<'PY'
from pathlib import Path
import re
import subprocess
import sys

def fixture_dependencies(binary):
    dynamic = subprocess.check_output(["readelf", "-d", binary], text=True, timeout=10)
    return set(re.findall(r"\(NEEDED\).*\[(librcmake_[^\]]+\.so)\]", dynamic))

direct = fixture_dependencies("probe")
transitive = fixture_dependencies("librcmake_parent.so")
if direct != {"librcmake_parent.so"} or transitive != {"librcmake_leaf.so"}:
    sys.exit(f"Unexpected ELF fixture dependencies. Found {direct} and {transitive}.")
expected = sorted(Path(name).resolve() for name in direct | transitive)
resolved = Path("resolved.txt").read_text().split(";")
if sorted(Path(name).resolve(strict=True) for name in resolved if name) != expected:
    sys.exit(f"ELF dependency paths differ from readelf. Found {resolved}. Expected {expected}.")
PY

  mkdir fortran
  cat > fortran/CMakeLists.txt <<'CMAKE'
cmake_minimum_required(VERSION 4.5)
project(FortranModules LANGUAGES Fortran)
add_executable(probe main.f90 number.f90)
CMAKE
  cat > fortran/main.f90 <<'FORTRAN'
program main
use number
if (value /= 42) stop 1
end program
FORTRAN
  cat > fortran/number.f90 <<'FORTRAN'
module number
integer, parameter :: value = 42
end module
FORTRAN
  for generator in Ninja "Unix Makefiles"; do
    build="fortran-${generator// /-}"
    "$bin/cmake" -S fortran -B "$build" -G "$generator" -DCMAKE_Fortran_COMPILER=gfortran
    "$bin/cmake" --build "$build" --parallel 2
    test -n "$(find "$build" -type f -iname number.mod -print -quit)"
    "$build/probe"
  done
)

linux_qt() (
  local bin="$1" work test_name
  work="$(mktemp -d)"
  trap 'rm -rf "$work"' EXIT
  timeout --kill-after=10s 120s "$bin/cmake" -S "$root/conformance/cases/languages/autogen" -B "$work" -G Ninja
  timeout --kill-after=10s 180s "$bin/cmake" --build "$work" --parallel 2
  for test_name in autogen_app autogen_other; do
    QT_QPA_PLATFORM=offscreen timeout --kill-after=10s 60s \
      "$bin/ctest" --test-dir "$work" --output-on-failure --no-tests=error -R "^${test_name}$"
  done
)

linux_runcmake() (
  local bin="$1" report="${2:-}" status=0 output suite version
  local -a suites=(RuntimePath file-RPATH file-GET_RUNTIME_DEPENDENCIES Ninja Make try_compile install)
  local -a args=()
  if [[ -n "$report" ]]; then
    output="$(mktemp "$(dirname "$report")/.runcmake.XXXXXXXX")" || exit 1
  else
    output="$(mktemp)" || exit 1
  fi
  trap 'rm -f "$output"' EXIT
  version="$(timeout --kill-after=5s 10s "$bin/cmake" --version)" || exit $?
  if [[ "$version" != *Rcmake* ]]; then
    printf 'Expected an Rcmake executable at %s.\n' "$bin/cmake" >&2
    exit 1
  fi
  for suite in "${suites[@]}"; do
    args+=(--suite "$suite")
  done
  timeout --signal=INT --kill-after=15s 40m python3 "$root/tools/measure/runcmake_eval.py" --profile linux --stage "$bin" \
    "${args[@]}" --timeout 300 --json "$output" || status=$?
  if [[ ! -s "$output" ]]; then
    printf 'RunCMake did not write a report. Exit status %s.\n' "$status" >&2
    exit "$((status == 0 ? 1 : status))"
  fi
  if [[ -n "$report" ]]; then
    mv -T -- "$output" "$report" || exit 1
    output="$report"
    trap - EXIT
  fi
  python3 - "$output" "${suites[@]}" <<'PY' || status=$((status == 0 ? 1 : status))
import json
from pathlib import Path
import sys

report, *expected = sys.argv[1:]
suites = json.loads(Path(report).read_text())["suites"]
for name in expected:
    suite = suites[name]
    problems = [case for case in suite.get("cases", ()) if case["status"] != "passed"]
    for case in problems[:5]:
        print(f"{name} / {case['name']}. Result {case['status']}.", file=sys.stderr)
    if len(problems) > 5:
        print(f"{name} has {len(problems)} cases that did not pass.", file=sys.stderr)
    if suite["total"] < 1 or suite["passed"] < 1:
        sys.exit(f"{name} did not report any passing cases.")
    if name == "install":
        variants = {"install-Files", "install-Targets", "install-Exports"}
        for lane in ("oracle", "rcmake"):
            if set(suite[f"{lane}_driver"]["variants"]) != variants:
                sys.exit(f"{lane} did not run all three install variants.")
        for variant in variants:
            if not any(case["name"].startswith(variant + ": ") and case["status"] == "passed"
                       for case in suite["cases"]):
                sys.exit(f"{variant} did not report any passing cases.")
PY
  exit "$status"
)

case "$(uname -s)" in
  Linux) platform=Linux ;;
  Darwin) platform=Darwin ;;
  MINGW*|MSYS*) platform=Windows ;;
  *) platform=unknown ;;
esac
case "${1:-}" in
  -h|--help)
    printf 'Usage: %s [--smoke-only | --extended-only | --qt-only] <staged-bin-dir>\n' "$0"
    printf 'Run the full test on Ubuntu 24.04. Use --smoke-only on Linux, macOS, or Windows.\n'
    printf 'Use --extended-only <staged-bin-dir> [report-path] on Linux with a configured oracle.\n'
    printf 'The RunCMake gate also fails when a case has no passing oracle result.\n'
    exit 0
    ;;
  --smoke-only)
    if [[ $# != 2 || "$platform" == unknown ]]; then
      printf 'Use --smoke-only with a staged bin directory on Linux, macOS, or Windows.\n' >&2
      exit 2
    fi
    smoke "$2" "$platform"
    exit
    ;;
  --qt-only)
    if [[ $# != 2 || "$platform" != Linux ]]; then
      printf 'Use --qt-only with a staged bin directory on Linux.\n' >&2
      exit 2
    fi
    linux_qt "$(cd "$2" && pwd -P)"
    exit
    ;;
  --extended-only)
    if [[ $# -lt 2 || $# -gt 3 || "$platform" != Linux ]]; then
      printf 'Use --extended-only with a staged bin directory and an optional report path on Linux.\n' >&2
      exit 2
    fi
    bin="$(cd "$2" && pwd -P)"
    status=0
    export -f linux_integration
    timeout --kill-after=10s 5m bash -euo pipefail -c 'linux_integration "$1"' _ "$bin" || status=1
    linux_runcmake "$bin" "${3:-}" || status=$?
    exit "$status"
    ;;
esac
if [[ $# != 0 || "$platform" != Linux ]]; then
  printf 'Run the full test on Ubuntu 24.04. Use --help for options.\n' >&2
  exit 2
fi

cd "$root"
original_path="$PATH"
# shellcheck disable=SC1091
source "$root/env/env.sh"
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$original_path"
export CARGO_TARGET_DIR="$root/target"
export CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=3

if [[ "$( . /etc/os-release && printf '%s:%s' "$ID" "$VERSION_ID" )" != ubuntu:24.04 ]]; then
  printf 'The full test requires Ubuntu 24.04.\n' >&2
  exit 2
fi

df -h "$root" "${TMPDIR:-/tmp}"
free -h
timeout --version
vm_tools_ready() {
  local tool
  for tool in gcc g++ clang clang++ gfortran ninja make ld readelf python3 curl git \
    /usr/lib/qt6/libexec/{moc,uic,rcc}; do
    if ! command -v "$tool" >/dev/null 2>&1; then
      printf 'Missing tool %s.\n' "$tool" >&2
      return 1
    fi
  done
  if ! compgen -G '/usr/lib/*/cmake/Qt6/Qt6Config.cmake' >/dev/null; then
    printf 'Missing the Qt 6 CMake package.\n' >&2
    return 1
  fi
}
if ! vm_tools_ready; then
  apt=(apt-get)
  if [[ "$EUID" != 0 ]]; then apt=(sudo apt-get); fi
  timeout --kill-after=10s 120s "${apt[@]}" update
  timeout --kill-after=10s 300s "${apt[@]}" install -y gcc g++ clang gfortran ninja-build make \
    binutils python3 curl git ca-certificates qt6-base-dev qt6-base-dev-tools
  vm_tools_ready
fi
python3 -c 'import sys; sys.exit(0 if sys.version_info >= (3, 12) else "Python 3.12 or later is required.")'

work="$(mktemp -d "${TMPDIR:-/tmp}/rcmake-linux.XXXXXXXX")"
trap 'rm -rf "$work"' EXIT
if ! command -v cargo >/dev/null 2>&1; then
  curl -fsSL --proto '=https' --tlsv1.2 --retry 3 --max-time 120 https://sh.rustup.rs -o "$work/rustup.sh"
  timeout --kill-after=10s 5m bash "$work/rustup.sh" -y --no-modify-path --default-toolchain none
  command -v cargo
fi

oracle_env="$(timeout --kill-after=10s 15m scripts/fetch-oracle.sh --print-env)"
eval "$oracle_env"
export PATH="$RCMAKE_ORACLE_BIN_DIR:$PATH"

failed=0
run_gate() {
  if "$@"; then
    return
  fi
  printf 'Check failed: %s\n' "$*" >&2
  failed=1
}

run_gate timeout --kill-after=30s 45m cargo test --locked --workspace
timeout --kill-after=30s 30m scripts/stage.sh
timeout --kill-after=30s 30m cargo build --locked --bin rcmake-harness
run_gate timeout --kill-after=30s 15m "$root/scripts/linux-vm-test.sh" --smoke-only "$root/target/stage/bin"
run_gate timeout --kill-after=30s 15m "$CARGO_TARGET_DIR/debug/rcmake-harness" --suite all

run_gate timeout --kill-after=30s 47m "$root/scripts/linux-vm-test.sh" --extended-only "$root/target/stage/bin"

run_gate "$root/scripts/linux-vm-test.sh" --qt-only "$root/target/stage/bin"

for project in zlib cJSON pugixml lz4; do
  run_gate python3 tools/measure/corpus_eval.py --project "$project" --generator Ninja --timeout 300
done
exit "$failed"
