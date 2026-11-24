#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd -P)"

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
  set_property(TARGET wave_test PROPERTY BUILD_WITH_INSTALL_RPATH ON)
  set_property(TARGET wave_test PROPERTY INSTALL_RPATH "$ORIGIN;${CMAKE_CURRENT_BINARY_DIR}/build-only")
endif()

enable_testing()
add_test(NAME wave_test COMMAND wave_test)
install(TARGETS wave_test wave_static wave_shared
  RUNTIME DESTINATION bin LIBRARY DESTINATION lib ARCHIVE DESTINATION lib)
if(UNIX AND NOT APPLE)
  string(CONFIGURE [[
    file(RPATH_CHANGE FILE "$ENV{DESTDIR}${CMAKE_INSTALL_PREFIX}/bin/wave_test"
      OLD_RPATH "$ORIGIN:@CMAKE_CURRENT_BINARY_DIR@/build-only" NEW_RPATH "$ORIGIN/../lib")
  ]] change_rpath @ONLY)
  install(CODE "${change_rpath}")
endif()
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

  local lane index=0 build prefix dynamic rpaths static_library shared_library
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
      dynamic="$(readelf -d "$build/wave_test")"
      if [[ "$dynamic" != *build-only* ]]; then
        printf 'The build executable has no build-only ELF runtime path.\n' >&2
        exit 1
      fi
    fi
    "$bin/cmake$exe" --install "$build" --config Release --prefix "$prefix"

    case "$platform" in
      Linux)
        static_library=lib/libwave_static.a
        shared_library=lib/libwave_shared.so
        dynamic="$(readelf -d "$prefix/bin/wave_test")"
        rpaths="$(awk '/\((RPATH|RUNPATH)\)/ { sub(/^.*\[/, ""); sub(/\].*$/, ""); print }' <<< "$dynamic")"
        if [[ "$rpaths" != "\$ORIGIN/../lib" ]]; then
          printf 'Unexpected installed ELF runtime path %s.\n' "$rpaths" >&2
          exit 1
        fi
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
    "$python" - "$work/package-$index" "bin/wave_test$exe" "$static_library" "$shared_library" <<'PY'
from pathlib import Path
import sys
from zipfile import ZipFile

directory, *required = sys.argv[1:]
archives = list(Path(directory).glob("*.zip"))
if len(archives) != 1:
    sys.exit(f"Expected one ZIP archive in {directory}.")
with ZipFile(archives[0]) as archive:
    members = archive.namelist()
    for name in required:
        if not any(member == name or member.endswith(f"/{name}") for member in members):
            sys.exit(f"Missing {name} in {archives[0].name}.")
    if bad_file := archive.testzip():
        sys.exit(f"Invalid ZIP entry {bad_file}.")
PY
  done
)

case "$(uname -s)" in
  Linux) platform=Linux ;;
  Darwin) platform=Darwin ;;
  MINGW*|MSYS*) platform=Windows ;;
  *) platform=unknown ;;
esac
case "${1:-}" in
  -h|--help)
    printf 'Usage: %s [--smoke-only <staged-bin-dir>]\n' "$0"
    printf 'Run the full test on Ubuntu 24.04, or test a staged binary on any CI host.\n'
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
esac
if [[ $# != 0 || "$platform" != Linux ]]; then
  printf 'Run the full test on Ubuntu 24.04. Use --help for options.\n' >&2
  exit 2
fi

cd "$root"
original_path="$PATH"
# shellcheck disable=SC1091
source "$root/env/env.sh"
export PATH="$HOME/.cargo/bin:$original_path"
export CARGO_TARGET_DIR="$root/target"

for tool in gcc g++ clang clang++ ninja make patchelf readelf python3 curl git; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    apt=(apt-get)
    if [[ "$EUID" != 0 ]]; then apt=(sudo apt-get); fi
    "${apt[@]}" update
    "${apt[@]}" install -y gcc g++ clang ninja-build make patchelf binutils python3 curl git ca-certificates
    break
  fi
done

work="$(mktemp -d "${TMPDIR:-/tmp}/rcmake-linux.XXXXXXXX")"
trap 'rm -rf "$work"' EXIT
if ! command -v cargo >/dev/null 2>&1; then
  curl -fsSL --proto '=https' --tlsv1.2 --retry 3 --max-time 120 https://sh.rustup.rs -o "$work/rustup.sh"
  bash "$work/rustup.sh" -y --no-modify-path --default-toolchain none
fi

oracle_env="$(scripts/fetch-oracle.sh --print-env)"
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

run_gate timeout --kill-after=30s 45m cargo test --workspace
timeout --kill-after=30s 30m scripts/stage.sh
timeout --kill-after=30s 30m cargo build --locked --bin rcmake-harness
run_gate timeout --kill-after=30s 15m "$0" --smoke-only "$root/target/stage/bin"
run_gate timeout --kill-after=30s 15m "$CARGO_TARGET_DIR/debug/rcmake-harness" --suite all

timeout --kill-after=10s 120s gcc "$RCMAKE_ORACLE_RUNCMAKE_DIR/detect_jobserver.c" -o "$work/detect_jobserver"
run_runcmake() {
  python3 - "$work/detect_jobserver" "$@" <<'PY'
import platform
import shutil
import sys

sys.path.insert(0, "tools/measure")
import runcmake_eval

detect_jobserver, install_variant = sys.argv[1:3]
del sys.argv[1:3]
run_suite = runcmake_eval.run_suite

# The standalone evaluator lacks the settings that upstream CTest supplies for these suites.
def run_linux_suite(driver, target, source, build, wrapper, settings, env, timeout):
    settings = {**settings, "CMAKE_EXECUTABLE_FORMAT": "ELF",
                "CMAKE_SHARED_LIBRARY_RUNTIME_C_FLAG": "-Wl,-rpath,"}
    if source.name == "Make":
        settings.update(RunCMake_GENERATOR="Unix Makefiles", RunCMake_MAKE_PROGRAM=shutil.which("make"),
                        MAKE_IS_GNU=1, DETECT_JOBSERVER=detect_jobserver)
    if source.name == "install":
        settings.update(CMake_TEST_install_VARIANT=install_variant, NO_NAMELINK=0,
                        CMAKE_SYSTEM_PROCESSOR=platform.machine(), CMAKE_SHARED_LIBRARY_RPATH_ORIGIN_TOKEN="$ORIGIN")
    return run_suite(driver, target, source, build, wrapper, settings, env, timeout)

runcmake_eval.run_suite = run_linux_suite
sys.exit(runcmake_eval.main())
PY
}

if ! run_runcmake "" --suite RuntimePath --suite file-RPATH --suite Ninja --suite Make --suite try_compile --timeout 300; then
  failed=1
fi
for variant in Files Targets Exports; do
  if ! run_runcmake "$variant" --suite install --timeout 300; then
    failed=1
  fi
done

for project in zlib cJSON pugixml lz4; do
  run_gate python3 tools/measure/corpus_eval.py --project "$project" --generator Ninja --timeout 300
done
exit "$failed"
