#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Run NVIDIA compiler detection and simple host-only CUDA builds on POSIX.

Use --install-mock-toolkit DIRECTORY to create bin/nvcc and the toolkit stubs.
The toolkit metadata matches Linux. Set MOCK_NVCC_CXX to select a host
compiler and MOCK_NVCC_LOG to record build operations as JSON lines.
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
    "--keep-dir", "-cudart", "--cudart",
    "-cudadevrt", "--cudadevrt", "-Xnvlink", "--nvlink-options",
}
RELOCATABLE_OPTIONS = {"-rdc", "--relocatable-device-code"}
DEVICE_ARCH = r"(?:compute|sm)_[1-9][0-9]+[af]?"
CODE_ARCH = rf"(?:{DEVICE_ARCH}|lto_[1-9][0-9]+[af]?)"
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
    inputs: list[Path] = field(default_factory=list)
    sources: list[Path] = field(default_factory=list)
    compiler: str = ""
    output: Path | None = None
    mode: str = "host"
    verbose: bool = False
    compile_only: bool = False
    relocatable: bool = False


def architecture(option: str, value: str) -> str:
    if option in {"-gencode", "--generate-code"}:
        match = re.fullmatch(rf"arch=(compute_[1-9][0-9]+[af]?),code=({CODE_ARCH}|\[{CODE_ARCH}(?:,{CODE_ARCH})*\])", value)
    elif option in {"-arch", "--gpu-architecture"}:
        match = re.fullmatch(rf"{DEVICE_ARCH}|all|all-major|native", value)
    else:
        match = re.fullmatch(rf"{CODE_ARCH}(?:,{CODE_ARCH})*", value)
    if not match:
        raise ValueError(f"Invalid CUDA architecture option {option}={value}.")
    return f"{option}={value}"


def parse(argv: list[str]) -> Invocation:
    result = Invocation()
    arguments = iter(argv)
    cuda_language = False
    host_cuda_language = False
    for argument in arguments:
        option, equals, value = argument.partition("=")
        if option in ARCH_OPTIONS | VALUE_OPTIONS | IGNORED_VALUES | HOST_VALUE_OPTIONS | RELOCATABLE_OPTIONS:
            if not equals and (value := next(arguments, None)) is None:
                raise ValueError(f"Missing value for {option}.")
            if option in ARCH_OPTIONS:
                result.architectures.append(architecture(option, value))
            elif option in RELOCATABLE_OPTIONS:
                if value not in {"true", "false"}:
                    raise ValueError(f"Use true or false for {option}.")
                result.relocatable = value == "true"
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
        elif option in {"-c", "-dc", "--device-c", "--compile", "-dw", "--device-w"}:
            result.host_args.append("-c")
            result.compile_only = True
            if option in {"-dc", "--device-c"}:
                result.relocatable = True
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
            result.inputs.append(Path(argument))
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
    (root / "mock" / "host-link").symlink_to("../bin/nvcc")
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
        for name, dest in (("include", "../../include"), ("lib", "../../lib64")):
            link = target / name
            if link.exists() or link.is_symlink():
                link.unlink()
            link.symlink_to(dest)


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


def record(argv: list[str], options: Invocation, phase: str, exit_code: int) -> int:
    if log := os.environ.get("MOCK_NVCC_LOG"):
        entry = {
            "phase": phase,
            "argv": argv,
            "inputs": [str(path.resolve()) for path in options.inputs],
            "output": str(options.output.resolve()) if options.output else None,
            "architectures": options.architectures,
            "relocatable": options.relocatable,
            "exit_code": exit_code,
        }
        with open(log, "a", encoding="utf-8") as stream:
            stream.write(json.dumps(entry) + "\n")
    return exit_code


def host_link(argv: list[str]) -> int:
    if not argv:
        raise ValueError("Pass a link command to the mock launcher.")
    options = parse(argv[1:])
    exit_code = subprocess.run(argv, timeout=TIMEOUT).returncode
    if options.mode == "device-link":
        return exit_code
    return record(argv, options, "host-link", exit_code)


def device_link(argv: list[str], options: Invocation, compiler: str) -> int:
    if not options.inputs:
        raise ValueError("Pass at least one object file or archive to the device linker.")
    for path in options.inputs:
        if path.suffix not in {".o", ".obj", ".a", ".lib"} or not path.is_file() or not path.stat().st_size:
            raise ValueError(f"Invalid device-link input {path}.")
    options.output = options.output or Path("a_dlink.o")
    command = [compiler, *options.host_flags, "-fPIC", "-x", "c++", "-c", "-", "-o", str(options.output)]
    source = b'extern "C" int rcmake_mock_device_link() { return 42; }\n'
    exit_code = subprocess.run(command, input=source, timeout=TIMEOUT).returncode
    return record(argv, options, "device-link", exit_code)


def run(argv: list[str]) -> int:
    if argv == ["--help"]:
        print(__doc__)
        return 0
    if os.name != "posix":
        raise ValueError("The mock toolkit requires POSIX host tools.")
    if Path(sys.argv[0]).name == "host-link":
        return host_link(argv)
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
        options.output = output
        return record(argv, options, options.mode, 0)

    compiler = host_compiler(options.compiler)
    if options.verbose:
        print_verbose(root, compiler, options)
    if "__cmake_determine_cuda" in argv or argv == ["-v"]:
        return 0
    if options.mode == "device-link":
        return device_link(argv, options, compiler)

    command = [compiler]
    if options.sources:
        command.extend([f"-I{root / 'include'}", "-include", str(root / "mock/cuda_shim.h"),
                        "-D__NVCC__", "-D__CUDACC__"])
        for component, value in zip(("MAJOR", "MINOR", "BUILD"), version):
            command.append(f"-D__CUDACC_VER_{component}__={value}")
    exit_code = subprocess.run([*command, *options.host_args], timeout=TIMEOUT).returncode
    return record(argv, options, "compile" if options.compile_only else "link", exit_code)


if __name__ == "__main__":
    try:
        raise SystemExit(run(sys.argv[1:]))
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"mock nvcc: {error}", file=sys.stderr)
        raise SystemExit(1)
