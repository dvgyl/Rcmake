#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Mock ROCm / chipStar HIP toolchain for differential testing against CMake 4.5.1.

Use --install-mock-rocm DIRECTORY [--platform amd|spirv] to install:
  - bin/clang++                (mock HIP Clang compiler; never named hipcc)
  - bin/hipconfig              (--platform, --rocmpath, --hipclangpath)
  - bin/rocm_agent_enumerator  (-t GPU -> gfx90a / gfx1100)
  - bin/hipcc                  (wrapper stub for negative hipcc-rejection tests)
  - include/hip/hip_runtime.h & include/hip/spirv_fixups.h
  - runtime_inc/hip_mock_runtime.h
  - lib/libhip_mock_rt.a       (exports rcmake_mock_hip_runtime() -> 77)
  - lib/libCHIP.so             (for spirv platform linking)
  - lib/cmake/hip-lang/hip-lang-config.cmake
        defines imported hip::host (STATIC -> libhip_mock_rt.a) and
        hip::device (INTERFACE injecting runtime_inc, HIP_MOCK_RUNTIME_DEFINE=1,
        and transitive hip::host) and sets _CMAKE_HIP_DEVICE_RUNTIME_TARGET.

Set MOCK_HIP_LOG to record JSONL traces and MOCK_HIP_CXX to pick the host C++ compiler.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
import platform
import re
import shlex
import shutil
import subprocess
import sys

TIMEOUT = 120
MAX_RSP_DEPTH = 8
MAX_RSP_BYTES = 1024 * 1024
MAX_RSP_ARGS = 32768
DEFAULT_CLANG_VERSION = "18.1.8"
OFFLOAD_ARCH_RE = re.compile(r"gfx[0-9a-z]+(?::(?:xnack|sramecc)[+-])*|spirv64")

HIP_RUNTIME_HEADER = """#pragma once
#define __global__
#define __device__
#define __host__
#define __shared__
typedef int hipError_t;
constexpr hipError_t hipSuccess = 0;
"""

HIP_MOCK_RUNTIME_HEADER = """#pragma once
extern "C" int rcmake_mock_hip_runtime();
"""

SPIRV_FIXUPS_HEADER = """#pragma once
#define RCMAKE_MOCK_SPIRV_FIXUPS 1
"""

HIP_LANG_CONFIG_CMAKE = """get_filename_component(_HIP_ROOT "${CMAKE_CURRENT_LIST_DIR}/../../.." ABSOLUTE)
if(NOT TARGET hip::host)
  add_library(hip::host STATIC IMPORTED)
  set_target_properties(hip::host PROPERTIES
    IMPORTED_LOCATION "${_HIP_ROOT}/lib/libhip_mock_rt.a"
  )
endif()
if(NOT TARGET hip::device)
  add_library(hip::device INTERFACE IMPORTED)
  set(_HIP_DEVICE_INCLUDES "${_HIP_ROOT}/include")
  if(NOT CMAKE_BINARY_DIR MATCHES "CMakeScratch")
    list(APPEND _HIP_DEVICE_INCLUDES "${_HIP_ROOT}/runtime_inc")
  endif()
  set_target_properties(hip::device PROPERTIES
    INTERFACE_INCLUDE_DIRECTORIES "${_HIP_DEVICE_INCLUDES}"
    INTERFACE_COMPILE_DEFINITIONS "HIP_MOCK_RUNTIME_DEFINE=1"
    INTERFACE_LINK_LIBRARIES "hip::host"
  )
endif()
set(_CMAKE_HIP_DEVICE_RUNTIME_TARGET "hip::device")
"""

HOST_VALUE_OPTIONS = {
    "-D", "-U", "-I", "-L", "-l", "-MF", "-MT", "-MQ", "-include", "-isystem",
    "-isysroot", "-Xlinker", "-Wl",
}


def _is_self(candidate: str) -> bool:
    try:
        self_real = Path(__file__).resolve()
        cand_path = Path(candidate)
        return cand_path.resolve() == self_real or cand_path.parent.resolve() == self_real.parent
    except OSError:
        return False


def _filtered_path() -> str:
    self_dirs = {
        str(Path(__file__).resolve().parent),
        str(Path(sys.argv[0]).absolute().parent),
    }
    entries = []
    for entry in os.environ.get("PATH", "").split(os.pathsep):
        if not entry:
            continue
        try:
            resolved = str(Path(entry).resolve())
        except OSError:
            resolved = entry
        if entry in self_dirs or resolved in self_dirs:
            continue
        entries.append(entry)
    return os.pathsep.join(entries)


def host_compiler(root: Path | None = None) -> str:
    clean_path = _filtered_path()
    preferred = os.environ.get("MOCK_HIP_CXX", "")
    if preferred:
        found = shutil.which(preferred, path=clean_path) or (preferred if Path(preferred).is_file() else None)
        if found and not _is_self(found):
            return found
        raise ValueError(f"Host compiler is unavailable: {preferred}")
    if root is not None:
        saved = root / "mock" / "host_cxx.txt"
        if saved.is_file():
            recorded = saved.read_text(encoding="utf-8").strip()
            if recorded and Path(recorded).is_file() and not _is_self(recorded):
                return recorded
    for name in ("clang++", "g++"):
        if (found := shutil.which(name, path=clean_path)) and not _is_self(found):
            return found
    raise ValueError("Install clang++ or g++.")


def _read_bounded_text(path: Path, max_bytes: int, label: str) -> tuple[str, int]:
    with open(path, "rb") as stream:
        st = os.fstat(stream.fileno())
        if st.st_size > max_bytes:
            raise ValueError(f"{label} exceeds {max_bytes} bytes: {path}")
        raw = stream.read(max_bytes + 1)
    if len(raw) > max_bytes:
        raise ValueError(f"{label} exceeds {max_bytes} bytes: {path}")
    return raw.decode("utf-8"), len(raw)


class _RspBudget:
    def __init__(self) -> None:
        self.remaining_bytes = MAX_RSP_BYTES
        self.remaining_args = MAX_RSP_ARGS

    def consume_arg(self, source: str) -> None:
        if self.remaining_args <= 0:
            raise ValueError(f"Response file argument budget exceeded: {source}")
        self.remaining_args -= 1

    def read_tokens(self, path: Path) -> list[str]:
        if self.remaining_bytes <= 0:
            raise ValueError(f"Response file byte budget exceeded: {path}")
        content, nbytes = _read_bounded_text(path, self.remaining_bytes, "Response file")
        self.remaining_bytes -= nbytes
        tokens = shlex.split(content)
        if len(tokens) > self.remaining_args:
            raise ValueError(f"Response file argument budget exceeded: {path}")
        return tokens


def expand_response_files(
    argv: list[str],
    depth: int = 0,
    budget: _RspBudget | None = None,
) -> list[str]:
    if depth >= MAX_RSP_DEPTH:
        raise ValueError("Response file nesting limit exceeded.")
    if budget is None:
        budget = _RspBudget()
    out: list[str] = []
    for arg in argv:
        if arg.startswith("@") and len(arg) > 1:
            tokens = budget.read_tokens(Path(arg[1:]))
            out.extend(expand_response_files(tokens, depth + 1, budget))
        else:
            budget.consume_arg(arg)
            out.append(arg)
    return out


def install(root: Path, hip_platform: str) -> None:
    root.mkdir(parents=True, exist_ok=True)
    if any(root.iterdir()):
        raise ValueError(f"Use an empty directory for the mock ROCm root: {root}")
    for sub in ("bin", "include/hip", "runtime_inc", "lib/cmake/hip-lang", "mock"):
        (root / sub).mkdir(parents=True, exist_ok=True)

    driver = root / "bin" / "clang++"
    shutil.copyfile(Path(__file__).resolve(), driver)
    driver.chmod(0o755)
    for alias in ("hipconfig", "rocm_agent_enumerator", "hipcc"):
        (root / "bin" / alias).symlink_to("clang++")

    (root / "mock" / "platform.txt").write_text(hip_platform + "\n", encoding="utf-8")
    (root / "include/hip/hip_runtime.h").write_text(HIP_RUNTIME_HEADER, encoding="utf-8")
    (root / "include/hip/spirv_fixups.h").write_text(SPIRV_FIXUPS_HEADER, encoding="utf-8")
    (root / "runtime_inc/hip_mock_runtime.h").write_text(
        HIP_MOCK_RUNTIME_HEADER, encoding="utf-8"
    )
    (root / "lib/cmake/hip-lang/hip-lang-config.cmake").write_text(
        HIP_LANG_CONFIG_CMAKE, encoding="utf-8"
    )

    compiler = host_compiler()
    (root / "mock" / "host_cxx.txt").write_text(compiler + "\n", encoding="utf-8")
    rt_obj = root / "mock" / "hip_rt.o"
    rt_src = b'extern "C" int rcmake_mock_hip_runtime() { return 77; }\n'
    subprocess.run(
        [compiler, "-fPIC", "-x", "c++", "-c", "-", "-o", str(rt_obj)],
        input=rt_src,
        check=True,
        timeout=TIMEOUT,
    )
    subprocess.run(
        ["ar", "rcs", str(root / "lib" / "libhip_mock_rt.a"), str(rt_obj)],
        check=True,
        timeout=TIMEOUT,
    )
    rt_obj.unlink()

    chip_lib = root / "lib" / "libCHIP.so"
    chip_cmd = [compiler, "-shared", "-fPIC", "-x", "c++", "-", "-o", str(chip_lib)]
    if sys.platform == "darwin":
        chip_cmd.extend(["-Wl,-install_name," + str(chip_lib)])
    chip_src = b'extern "C" int rcmake_mock_chipstar_rt() { return 88; }\n'
    subprocess.run(chip_cmd, input=chip_src, check=True, timeout=TIMEOUT)


def log_event(entry: dict) -> int:
    if path := os.environ.get("MOCK_HIP_LOG"):
        with open(path, "a", encoding="utf-8") as stream:
            stream.write(json.dumps(entry) + "\n")
    return int(entry["exit_code"])


def read_platform(root: Path) -> str:
    env_plat = os.environ.get("MOCK_HIP_PLATFORM", "").strip()
    if env_plat:
        return env_plat
    plat_file = root / "mock" / "platform.txt"
    if plat_file.is_file():
        return plat_file.read_text(encoding="utf-8").strip() or "amd"
    return "amd"


def run_hipconfig(root: Path, argv: list[str]) -> int:
    plat = read_platform(root)
    if "--platform" in argv:
        print(plat)
        return 0
    if "--rocmpath" in argv or "--path" in argv:
        print(str(root))
        return 0
    if "--hipclangpath" in argv:
        print(str(root / "bin"))
        return 0
    print(f"HIP version: 6.2.0\nPlatform: {plat}\nROCm path: {root}")
    return 0


def run_rocm_agent_enumerator(argv: list[str]) -> int:
    agents = os.environ.get("MOCK_ROCM_AGENTS", "gfx000\ngfx90a")
    for line in agents.splitlines():
        if line.strip():
            print(line.strip())
    return 0


def run_clang_hip(root: Path, argv: list[str]) -> int:
    version = os.environ.get("MOCK_HIP_CLANG_VERSION", DEFAULT_CLANG_VERSION)
    match = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)", version)
    if not match:
        raise ValueError("Set MOCK_HIP_CLANG_VERSION to major.minor.patch.")
    major, minor, patch = match.groups()

    if any(arg == "--version" for arg in argv):
        print(f"clang version {version} (mock-hip)")
        print(f"Target: {platform.machine()}-unknown-linux-gnu")
        print("Thread model: posix")
        return 0
    if "-print-targets" in argv:
        print(f"Found HIP installation: {root}, version 6.2.0", file=sys.stderr)
        print("  Registered Targets:\n    amdgcn - AMD GCN GPUs\n    spirv64 - SPIR-V 64-bit")
        return 0

    expanded = expand_response_files(argv)
    compiler = host_compiler(root)
    offload_archs: list[str] = []
    hip_path: Path | None = None
    compile_only = False
    hip_lang = False
    host_hip_lang = False
    inputs: list[Path] = []
    sources: list[Path] = []
    output: Path | None = None
    host_args: list[str] = []

    args = iter(expanded)
    for arg in args:
        if arg.startswith("--offload-arch="):
            arch = arg.split("=", 1)[1]
            if not OFFLOAD_ARCH_RE.fullmatch(arch):
                raise ValueError(f"Invalid HIP --offload-arch value: {arg}")
            offload_archs.append(arch)
            continue
        if arg.startswith("--offload="):
            arch = arg.split("=", 1)[1]
            if not OFFLOAD_ARCH_RE.fullmatch(arch):
                raise ValueError(f"Invalid HIP --offload value: {arg}")
            offload_archs.append(arch)
            continue
        if arg.startswith("--hip-path=") or arg.startswith("--rocm-path="):
            hip_path = Path(arg.split("=", 1)[1])
            if not hip_path.is_dir():
                raise ValueError(f"Invalid HIP root path: {hip_path}")
            continue
        if arg in {"--cuda-host-only", "--cuda-device-only", "-nogpulib", "-nohipwrapperinc",
                   "-no-hip-rt", "-fgpu-rdc", "-fno-gpu-rdc"}:
            continue
        if arg == "-c":
            compile_only = True
            host_args.append("-c")
            continue
        if arg == "-o":
            val = next(args, "")
            output = Path(val)
            host_args.extend(["-o", val])
            continue
        if arg == "-x":
            lang = next(args, "")
            hip_lang = lang == "hip"
            if not hip_lang:
                host_args.extend(["-x", lang])
                host_hip_lang = False
            continue
        if arg in HOST_VALUE_OPTIONS:
            val = next(args, "")
            host_args.extend([arg, val])
            continue
        if arg.startswith("-"):
            host_args.append(arg)
            continue
        inputs.append(Path(arg))
        is_hip = hip_lang or arg.endswith(".hip")
        if is_hip != host_hip_lang:
            host_args.extend(["-x", "c++" if is_hip else "none"])
            host_hip_lang = is_hip
        if is_hip:
            sources.append(Path(arg))
        host_args.append(arg)

    toolkit = hip_path or root
    command = [compiler]
    if sources:
        command.extend([
            f"-I{toolkit / 'include'}",
            "-Wno-macro-redefined",
            "-U__apple_build_version__",
            "-D__HIP__",
            "-D__HIPCC__",
            f"-D__clang_major__={major}",
            f"-D__clang_minor__={minor}",
            f"-D__clang_patchlevel__={patch}",
        ])
    exit_code = subprocess.run([*command, *host_args], timeout=TIMEOUT).returncode
    return log_event({
        "phase": "compile" if compile_only else "link",
        "argv": argv,
        "inputs": [str(p.resolve()) for p in inputs],
        "output": str(output.resolve()) if output else None,
        "offload_archs": offload_archs,
        "hip_path": str(hip_path) if hip_path else None,
        "exit_code": exit_code,
    })


def run(argv: list[str]) -> int:
    if argv == ["--help"]:
        print(__doc__)
        return 0
    if os.name != "posix":
        raise ValueError("The mock HIP toolchain requires POSIX host tools.")
    depth = int(os.environ.get("MOCK_HIP_DEPTH", "0"))
    if depth >= 2:
        raise ValueError(f"Recursive mock hip invocation detected (depth={depth}).")
    os.environ["MOCK_HIP_DEPTH"] = str(depth + 1)
    if argv[:1] == ["--install-mock-rocm"]:
        if len(argv) < 2:
            raise ValueError("Use --install-mock-rocm DIRECTORY [--platform amd|spirv].")
        target = Path(argv[1]).resolve()
        hip_platform = "amd"
        if len(argv) == 4 and argv[2] == "--platform":
            hip_platform = argv[3]
        elif len(argv) != 2:
            raise ValueError("Use --install-mock-rocm DIRECTORY [--platform amd|spirv].")
        if hip_platform not in {"amd", "spirv", "nvidia"}:
            raise ValueError(f"Unsupported mock HIP platform: {hip_platform}")
        install(target, hip_platform)
        return 0

    root = Path(os.environ.get("MOCK_ROCM_ROOT", Path(sys.argv[0]).absolute().parent.parent))
    tool = Path(sys.argv[0]).name
    if tool == "hipconfig":
        return run_hipconfig(root, argv)
    if tool == "rocm_agent_enumerator":
        return run_rocm_agent_enumerator(argv)
    if tool == "hipcc":
        log_event({"phase": "hipcc-wrapper", "argv": argv, "exit_code": 1})
        print("mock hipcc wrapper should never be selected directly by CMake", file=sys.stderr)
        return 1
    return run_clang_hip(root, argv)


if __name__ == "__main__":
    try:
        raise SystemExit(run(sys.argv[1:]))
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"mock hip: {error}", file=sys.stderr)
        raise SystemExit(1)
