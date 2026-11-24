#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Run CUDA configuration and host-only build tests without an NVIDIA toolkit.

Use --install-mock-toolkit DIRECTORY to create bin/nvcc and the toolkit stubs.
Set MOCK_NVCC_CXX to select a host compiler and MOCK_NVCC_LOG to record argv.
"""

from dataclasses import dataclass, field
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
DEFAULT_VERSION = "12.6.85"
ARCH_OPTIONS = {
    "-arch", "--gpu-architecture", "-gencode", "--generate-code", "-code", "--gpu-code",
}
VALUE_OPTIONS = {
    "-ccbin", "--compiler-bindir", "-Xcompiler", "--compiler-options", "-o", "--output-file",
}
IGNORED_VALUES = {
    "--keep-dir", "-rdc", "--relocatable-device-code", "-cudart", "--cudart",
    "-cudadevrt", "--cudadevrt", "-Xnvlink", "--nvlink-options",
}
IGNORED_FLAGS = {
    "--keep", "-keep", "-forward-unknown-to-host-compiler", "--forward-unknown-to-host-compiler",
    "--expt-relaxed-constexpr", "--extended-lambda", "--expt-extended-lambda",
}
HOST_VALUE_OPTIONS = {
    "-D", "-U", "-I", "-L", "-l", "-MF", "-MT", "-MQ", "-include", "-isystem",
    "-isysroot", "-Xassembler", "-Xlinker",
}
ARTIFACT_MODES = {
    "-ptx": "ptx", "--ptx": "ptx", "-cubin": "cubin", "--cubin": "cubin",
    "-fatbin": "fatbin", "--fatbin": "fatbin", "-optix-ir": "optixir", "--optix-ir": "optixir",
}
RUNTIME_HEADER = """#pragma once
#include <cstdlib>
#define __global__
#define __device__
#define __host__
#define __shared__
#define __constant__
#define __managed__
#define __forceinline__ inline
#define __launch_bounds__(...)
typedef int cudaError_t;
constexpr cudaError_t cudaSuccess = 0;
struct cudaDeviceProp { int major; int minor; };
inline int mock_cuda_env(const char* name, int fallback) {
    const char* value = std::getenv(name);
    return value ? std::atoi(value) : fallback;
}
inline cudaError_t cudaGetDeviceCount(int* count) {
    *count = mock_cuda_env("MOCK_CUDA_DEVICES", 1);
    return cudaSuccess;
}
inline cudaError_t cudaGetDeviceProperties(cudaDeviceProp* properties, int) {
    const int sm = mock_cuda_env("MOCK_CUDA_SM", 52);
    properties->major = sm / 10;
    properties->minor = sm % 10;
    return cudaSuccess;
}
inline const char* cudaGetErrorString(cudaError_t) { return "mock CUDA runtime"; }
"""


@dataclass
class Invocation:
    """Store the options needed for the host compilation or mock device output."""

    host_args: list[str] = field(default_factory=list)
    host_flags: list[str] = field(default_factory=list)
    architectures: list[str] = field(default_factory=list)
    sources: list[Path] = field(default_factory=list)
    compiler: str = ""
    output: Path | None = None
    mode: str = "host"
    verbose: bool = False


def parse(argv: list[str]) -> Invocation:
    result = Invocation()
    arguments = iter(argv)
    cuda_language = False
    host_cuda_language = False
    for argument in arguments:
        option, equals, value = argument.partition("=")
        if option in ARCH_OPTIONS | VALUE_OPTIONS | IGNORED_VALUES | HOST_VALUE_OPTIONS:
            if not equals and (value := next(arguments, None)) is None:
                raise ValueError(f"Missing value for {option}.")
            if option in ARCH_OPTIONS:
                result.architectures.append(value)
            elif option in {"-ccbin", "--compiler-bindir"}:
                result.compiler = value
            elif option in {"-Xcompiler", "--compiler-options"}:
                flags = value.split(",")
                result.host_flags.extend(flags)
                result.host_args.extend(flags)
            elif option in {"-o", "--output-file"}:
                result.output = Path(value)
                result.host_args.extend(["-o", value])
            elif option in HOST_VALUE_OPTIONS:
                result.host_args.extend([argument] if equals else [option, value])
        elif option in IGNORED_FLAGS:
            continue
        elif option in {"-v", "--verbose"}:
            result.verbose = True
        elif option in {"-dlink", "--device-link"}:
            result.mode = "device-link"
        elif option in ARTIFACT_MODES:
            result.mode = ARTIFACT_MODES[option]
        elif option in {"-dc", "--device-c", "--compile", "-dw", "--device-w"}:
            result.host_args.append("-c")
        elif option in {"-x", "--x"}:
            language = value if equals else next(arguments, "")
            cuda_language = language in {"cu", "cuda"}
            if not cuda_language:
                result.host_args.extend(["-x", language])
                host_cuda_language = False
        elif option == "--std":
            result.host_args.append("-std=" + (value if equals else next(arguments, "")))
        elif option == "--dependency-output":
            result.host_args.extend(["-MF", value if equals else next(arguments, "")])
        elif argument.startswith("-"):
            result.host_args.append(argument)
        else:
            is_cuda = cuda_language or argument.endswith(".cu")
            if is_cuda != host_cuda_language:
                result.host_args.extend(["-x", "c++" if is_cuda else "none"])
                host_cuda_language = is_cuda
            if is_cuda:
                result.sources.append(Path(argument))
            result.host_args.append(argument)
    return result


def host_compiler(preferred: str = "") -> str:
    compiler = preferred or os.environ.get("MOCK_NVCC_CXX", "")
    if compiler and Path(compiler).is_dir():
        compiler = str(Path(compiler) / "g++")
    if compiler:
        found = shutil.which(compiler)
        if found:
            return found
        raise ValueError(f"Host compiler is unavailable: {compiler}")
    for name in ("clang++", "g++"):
        if compiler := shutil.which(name):
            return compiler
    raise ValueError("Install clang++ or g++.")


def version_parts() -> tuple[str, str, str]:
    version = os.environ.get("MOCK_NVCC_VERSION", DEFAULT_VERSION)
    match = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)", version)
    if not match:
        raise ValueError("Set MOCK_NVCC_VERSION to major.minor.patch.")
    return match.groups()


def install(root: Path, version: tuple[str, str, str]) -> None:
    """Create a mock toolkit in an empty directory."""
    root.mkdir(parents=True, exist_ok=True)
    if any(root.iterdir()):
        raise ValueError(f"Use an empty directory for the mock toolkit: {root}")
    for directory in ("bin/crt", "include", "lib64", "mock", "nvvm/libdevice",
                      "targets/x86_64-linux", "targets/sbsa-linux"):
        (root / directory).mkdir(parents=True)
    nvcc = root / "bin" / "nvcc"
    shutil.copyfile(Path(__file__).resolve(), nvcc)
    nvcc.chmod(0o755)
    for tool in ("nvlink", "fatbinary"):
        (nvcc.parent / tool).symlink_to("nvcc")
    (root / "bin/crt/link.stub").touch()
    (root / "include/cuda_runtime.h").write_text(RUNTIME_HEADER, encoding="utf-8")
    (root / "include/cuda.h").write_text(
        f"#define CUDA_VERSION {int(version[0]) * 1000 + int(version[1]) * 10}\n",
        encoding="utf-8",
    )
    (root / "mock/cuda_shim.h").write_text("#include <cuda_runtime.h>\n", encoding="utf-8")
    (root / "version.json").write_text(
        json.dumps({"cuda": {"version": ".".join(version)}}) + "\n", encoding="utf-8"
    )

    compiler = host_compiler()
    stub = root / "mock/stub.o"
    subprocess.run([compiler, "-x", "c++", "-c", "-", "-o", str(stub)],
                   input=b"", check=True, timeout=TIMEOUT)
    archives = ["cudart_static", "cudadevrt", "culibos"]
    if sys.platform == "darwin":
        archives.append("rt")
    for library in archives:
        subprocess.run(["ar", "rcs", str(root / f"lib64/lib{library}.a"), str(stub)],
                       check=True, timeout=TIMEOUT)
    stub.unlink()
    suffix = ".dylib" if sys.platform == "darwin" else ".so"
    for library in ("cuda", "cudart", "cublas"):
        subprocess.run(
            [compiler, "-shared", "-x", "c++", "-", "-o", str(root / f"lib64/lib{library}{suffix}")],
            input=b"", check=True, timeout=TIMEOUT,
        )
    for target in (root / "targets").iterdir():
        (target / "include").symlink_to("../../include")
        (target / "lib").symlink_to("../../lib64")


def print_verbose(root: Path, compiler: str, options: Invocation) -> None:
    target = "sbsa-linux" if platform.machine() in {"arm64", "aarch64"} else "x86_64-linux"
    libraries = [f"-L{root / 'lib64'}", "-lcudart_static"]
    for name, value in (
        ("_NVVM_BRANCH_", "nvvm"), ("TOP", str(root)),
        ("NVVMIR_LIBRARY_DIR", str(root / "nvvm/libdevice")),
        ("_TARGET_DIR_", f"targets/{target}"), ("PATH", os.environ.get("PATH", "")),
        ("INCLUDES", shlex.quote(f"-I{root / 'include'}")), ("LIBRARIES", shlex.join(libraries)),
    ):
        print(f"#$ {name}={value}", file=sys.stderr)
    print("#$ cicc -arch compute_52", file=sys.stderr)
    print("#$ nvlink -arch sm_52", file=sys.stderr)
    output = options.output or Path("a.out")
    objects = [str(source.with_suffix(".o")) for source in options.sources]
    print("#$ " + shlex.join([compiler, "-o", str(output), *objects, *libraries]), file=sys.stderr)


def run(argv: list[str]) -> int:
    if argv == ["--help"]:
        print(__doc__)
        return 0
    if log := os.environ.get("MOCK_NVCC_LOG"):
        with open(log, "a", encoding="utf-8") as stream:
            stream.write(json.dumps(argv) + "\n")
    version = version_parts()
    if argv[:1] == ["--install-mock-toolkit"]:
        if len(argv) != 2:
            raise ValueError("Use --install-mock-toolkit DIRECTORY.")
        install(Path(argv[1]).resolve(), version)
        return 0
    if any(arg in {"--version", "-V"} for arg in argv) or argv == ["-v"]:
        print("nvcc: NVIDIA (R) Cuda compiler driver")
        print(f"Cuda compilation tools, release {'.'.join(version[:2])}, V{'.'.join(version)}")
        if argv != ["-v"]:
            return 0

    options = parse(argv)
    root = Path(os.environ.get("MOCK_CUDA_ROOT", Path(sys.argv[0]).absolute().parent.parent))
    if options.mode not in {"host", "device-link"}:
        output = options.output
        if output is None and options.sources:
            output = Path(options.sources[0].name).with_suffix("." + options.mode)
        if output is None:
            raise ValueError("Specify a source or an output for the mock device artifact.")
        architectures = options.architectures or ["compute_52"]
        output.write_text(f"{options.mode}\n" + "\n".join(architectures) + "\n", encoding="utf-8")
        return 0

    compiler = host_compiler(options.compiler)
    if options.verbose:
        print_verbose(root, compiler, options)
    if "__cmake_determine_cuda" in argv or argv == ["-v"]:
        return 0
    if options.mode == "device-link":
        output = options.output or Path("a_dlink.o")
        command = [compiler, *options.host_flags, "-fPIC", "-x", "c++", "-c", "-", "-o", str(output)]
        return subprocess.run(command, input=b"", timeout=TIMEOUT).returncode

    command = [compiler]
    if options.sources:
        command.extend([f"-I{root / 'include'}", "-include", str(root / "mock/cuda_shim.h"),
                        "-D__NVCC__", "-D__CUDACC__"])
        for component, value in zip(("MAJOR", "MINOR", "BUILD"), version):
            command.append(f"-D__CUDACC_VER_{component}__={value}")
    return subprocess.run([*command, *options.host_args], timeout=TIMEOUT).returncode


if __name__ == "__main__":
    try:
        raise SystemExit(run(sys.argv[1:]))
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"mock nvcc: {error}", file=sys.stderr)
        raise SystemExit(1)
