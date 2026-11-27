#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Run NVIDIA compiler detection and simple host-only CUDA builds on POSIX.

Use --install-mock-toolkit DIRECTORY to create bin/nvcc and the toolkit stubs.
The toolkit metadata matches Linux. Set MOCK_NVCC_CXX to select a host
compiler and MOCK_NVCC_LOG to record build operations as JSON lines.
"""

from collections.abc import Iterator
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
MAX_RSP_DEPTH = 8
MAX_RSP_BYTES = 1024 * 1024
MAX_RSP_ARGS = 32768
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
COMPILE_OPTIONS = {"-c", "-dc", "--device-c", "--compile", "-dw", "--device-w"}
VALUE_TAKING_OPTIONS = ARCH_OPTIONS | VALUE_OPTIONS | IGNORED_VALUES | HOST_VALUE_OPTIONS | RELOCATABLE_OPTIONS
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


def _read_bounded_text(path: Path, max_bytes: int, label: str) -> tuple[str, int]:
    with open(path, "rb") as stream:
        st = os.fstat(stream.fileno())
        if st.st_size > max_bytes:
            raise ValueError(f"{label} exceeds {max_bytes} bytes: {path}")
        raw = stream.read(max_bytes + 1)
    if len(raw) > max_bytes:
        raise ValueError(f"{label} exceeds {max_bytes} bytes: {path}")
    return raw.decode("utf-8"), len(raw)


@dataclass
class _RspBudget:
    remaining_bytes: int = MAX_RSP_BYTES
    remaining_args: int = MAX_RSP_ARGS

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
        raise ValueError(f"The response file nesting limit is {MAX_RSP_DEPTH}.")
    if budget is None:
        budget = _RspBudget()
    result = []
    arguments = iter(argv)
    for argument in arguments:
        option, _, value = argument.partition("=")
        if option in {"--options-file", "-optf"}:
            value = value or next(arguments, "")
            if not value:
                raise ValueError(f"Missing response file for {option}.")
            paths = value.split(",")
        elif argument.startswith("@") and len(argument) > 1:
            paths = [argument[1:]]
        else:
            budget.consume_arg(argument)
            result.append(argument)
            continue
        for path in paths:
            tokens = budget.read_tokens(Path(path))
            result.extend(expand_response_files(tokens, depth + 1, budget))
    return result


def _apply_value_option(
    result: Invocation,
    argument: str,
    option: str,
    equals: str,
    value: str,
) -> None:
    if option in ARCH_OPTIONS:
        result.architectures.append(architecture(option, value))
        return
    if option in RELOCATABLE_OPTIONS:
        if value not in {"true", "false"}:
            raise ValueError(f"Use true or false for {option}.")
        result.relocatable = value == "true"
        return
    if option in {"-ccbin", "--compiler-bindir"}:
        result.compiler = value
        return
    if option in {"-Xcompiler", "--compiler-options"}:
        flags = value.split(",")
        result.host_flags.extend(flags)
        result.host_args.extend(flags)
        return
    if option in {"-o", "--output-file"}:
        result.output = Path(value)
        result.host_args.extend(["-o", value])
        return
    if option in HOST_VALUE_OPTIONS:
        result.host_args.extend([argument] if equals else [option, value])


def _add_input(
    result: Invocation,
    argument: str,
    cuda_language: bool,
    host_cuda_language: bool,
) -> bool:
    result.inputs.append(Path(argument))
    is_cuda = cuda_language or argument.endswith(".cu")
    if is_cuda != host_cuda_language:
        result.host_args.extend(["-x", "c++" if is_cuda else "none"])
        host_cuda_language = is_cuda
    if is_cuda:
        result.sources.append(Path(argument))
    result.host_args.append(argument)
    return host_cuda_language


def _option_value(equals: str, value: str, arguments: Iterator[str]) -> str:
    return value if equals else next(arguments, "")


def parse(argv: list[str]) -> Invocation:
    result = Invocation()
    arguments = iter(argv)
    cuda_language = False
    host_cuda_language = False
    for argument in arguments:
        option, equals, value = argument.partition("=")
        if option in VALUE_TAKING_OPTIONS:
            if not equals and (value := next(arguments, None)) is None:
                raise ValueError(f"Missing value for {option}.")
            _apply_value_option(result, argument, option, equals, value)
            continue
        if option in IGNORED_FLAGS:
            continue
        if option in {"-v", "--verbose"}:
            result.verbose = True
            continue
        if option in {"-dlink", "--device-link"}:
            result.mode = "device-link"
            continue
        if option in ARTIFACT_MODES:
            result.mode = ARTIFACT_MODES[option]
            continue
        if option in COMPILE_OPTIONS:
            result.host_args.append("-c")
            result.compile_only = True
            if option in {"-dc", "--device-c"}:
                result.relocatable = True
            continue
        if option in {"-x", "--x"}:
            language = _option_value(equals, value, arguments)
            cuda_language = language in {"cu", "cuda"}
            if not cuda_language:
                result.host_args.extend(["-x", language])
                host_cuda_language = False
            continue
        if option == "--std":
            result.host_args.append("-std=" + _option_value(equals, value, arguments))
            continue
        if option == "--dependency-output":
            result.host_args.extend(["-MF", _option_value(equals, value, arguments)])
            continue
        if argument.startswith("-"):
            result.host_args.append(argument)
            continue
        host_cuda_language = _add_input(result, argument, cuda_language, host_cuda_language)
    return result


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


def host_compiler(preferred: str = "") -> str:
    compiler = preferred or os.environ.get("MOCK_NVCC_CXX", "")
    if compiler and Path(compiler).is_dir():
        compiler = str(Path(compiler) / "g++")
    clean_path = _filtered_path()
    if compiler:
        found = shutil.which(compiler, path=clean_path) or (compiler if Path(compiler).is_file() else None)
        if found and not _is_self(found):
            return found
        raise ValueError(f"Host compiler is unavailable: {compiler}")
    saved = Path(sys.argv[0]).absolute().parent.parent / "mock" / "host_cxx.txt"
    if saved.is_file():
        recorded = saved.read_text(encoding="utf-8").strip()
        if recorded and Path(recorded).is_file() and not _is_self(recorded):
            return recorded
    for name in ("clang++", "g++"):
        if (found := shutil.which(name, path=clean_path)) and not _is_self(found):
            return found
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
    (root / "bin" / "clang++").symlink_to("nvcc")
    (root / "bin" / "nvlink").symlink_to("nvcc")
    (root / "bin" / "fatbinary").symlink_to("nvcc")
    (root / "bin/crt/link.stub").write_text(
        "#ifdef REGISTERLINKBINARYFILE\n"
        "#include REGISTERLINKBINARYFILE\n"
        "#endif\n"
        "#ifdef FATBINFILE\n"
        "#include FATBINFILE\n"
        "#endif\n"
        'extern "C" int rcmake_mock_clang_device_link() { return 42; }\n',
        encoding="utf-8",
    )
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
    (root / "mock" / "host_cxx.txt").write_text(compiler + "\n", encoding="utf-8")
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


def record(
    argv: list[str],
    options: Invocation,
    phase: str,
    exit_code: int,
    expanded_argv: list[str] | None = None,
) -> int:
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
        if expanded_argv is not None:
            entry["expanded_argv"] = expanded_argv
        with open(log, "a", encoding="utf-8") as stream:
            stream.write(json.dumps(entry) + "\n")
    return exit_code


def host_link(argv: list[str]) -> int:
    if not argv:
        raise ValueError("Pass a link command to the mock launcher.")
    expanded = expand_response_files(argv[1:])
    options = parse(expanded)
    exit_code = subprocess.run(argv, timeout=TIMEOUT).returncode
    if options.mode == "device-link":
        return exit_code
    return record(argv, options, "host-link", exit_code, expanded_argv=[argv[0], *expanded])


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


def _log_raw(entry: dict) -> int:
    if log := os.environ.get("MOCK_NVCC_LOG"):
        with open(log, "a", encoding="utf-8") as stream:
            stream.write(json.dumps(entry) + "\n")
    return int(entry["exit_code"])


def run_nvlink(argv: list[str]) -> int:
    arch = ""
    register_file: Path | None = None
    output: Path | None = None
    inputs: list[Path] = []
    args = iter(expand_response_files(argv))
    for arg in args:
        if arg.startswith("-arch="):
            arch = arg.split("=", 1)[1]
            if not re.fullmatch(r"sm_[1-9][0-9]+[af]?", arch):
                raise ValueError(f"Invalid nvlink architecture: {arch}")
        elif arg == "-arch":
            arch = next(args, "")
            if not re.fullmatch(r"sm_[1-9][0-9]+[af]?", arch):
                raise ValueError(f"Invalid nvlink architecture: {arch}")
        elif arg.startswith("--register-link-binaries="):
            register_file = Path(arg.split("=", 1)[1])
        elif arg == "--register-link-binaries":
            register_file = Path(next(args, ""))
        elif arg.startswith("-o="):
            output = Path(arg.split("=", 1)[1])
        elif arg == "-o":
            output = Path(next(args, ""))
        elif arg.startswith("-"):
            continue
        else:
            inputs.append(Path(arg))
    if not arch:
        raise ValueError("nvlink requires -arch=sm_XX.")
    if output is None:
        raise ValueError("nvlink requires -o=<cubin>.")
    if not inputs:
        raise ValueError("nvlink requires at least one input object or archive.")
    for path in inputs:
        if not path.is_file() or not path.stat().st_size:
            raise ValueError(f"Invalid nvlink input {path}.")
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(f"cubin {arch}\n" + "\n".join(str(p) for p in inputs) + "\n", encoding="utf-8")
    if register_file is not None:
        register_file.parent.mkdir(parents=True, exist_ok=True)
        register_file.write_text(f"#define RCMAKE_MOCK_NVLINK_ARCH \"{arch}\"\n", encoding="utf-8")
    return _log_raw({
        "phase": "nvlink",
        "argv": argv,
        "inputs": [str(p.resolve()) for p in inputs],
        "output": str(output.resolve()),
        "architectures": [arch],
        "relocatable": True,
        "exit_code": 0,
    })


def run_fatbinary(argv: list[str]) -> int:
    output: Path | None = None
    profiles: list[str] = []
    inputs: list[Path] = []
    for arg in expand_response_files(argv):
        if arg.startswith("--embedded-fatbin="):
            output = Path(arg.split("=", 1)[1])
        elif arg.startswith("-im="):
            spec = arg.split("=", 1)[1]
            match = re.fullmatch(r"profile=(sm_[1-9][0-9]+[af]?),file=(.+)", spec)
            if not match:
                raise ValueError(f"Invalid fatbinary -im argument: {arg}")
            profile, cubin = match.groups()
            cubin_path = Path(cubin)
            if not cubin_path.is_file() or not cubin_path.stat().st_size:
                raise ValueError(f"Missing fatbinary cubin input: {cubin_path}")
            profiles.append(profile)
            inputs.append(cubin_path)
    if output is None:
        raise ValueError("fatbinary requires --embedded-fatbin=<file>.")
    if not profiles:
        raise ValueError("fatbinary requires at least one -im=profile=...,file=... input.")
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(
        f'#define RCMAKE_MOCK_FATBIN_PROFILES "{",".join(profiles)}"\n',
        encoding="utf-8",
    )
    return _log_raw({
        "phase": "fatbinary",
        "argv": argv,
        "inputs": [str(p.resolve()) for p in inputs],
        "output": str(output.resolve()),
        "architectures": profiles,
        "relocatable": True,
        "exit_code": 0,
    })


def run_clang_cuda(argv: list[str]) -> int:
    clang_version = os.environ.get("MOCK_CLANG_CUDA_VERSION", "18.1.8")
    match = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)", clang_version)
    if not match:
        raise ValueError("Set MOCK_CLANG_CUDA_VERSION to major.minor.patch.")
    major, minor, patch = match.groups()
    if any(arg == "--version" for arg in argv):
        print(f"clang version {clang_version} (mock-cuda)")
        print(f"Target: {platform.machine()}-unknown-linux-gnu")
        print("Thread model: posix")
        return 0

    root = Path(os.environ.get("MOCK_CUDA_ROOT", Path(sys.argv[0]).absolute().parent.parent))
    expanded = expand_response_files(argv)
    compiler = host_compiler()
    architectures: list[str] = []
    cuda_path: Path | None = None
    relocatable = False
    offload_link = False
    device_only = False
    compile_only = False
    verbose = False
    cuda_lang = False
    host_cuda_lang = False
    inputs: list[Path] = []
    sources: list[Path] = []
    output: Path | None = None
    host_args: list[str] = []

    args = iter(expanded)
    for arg in args:
        if arg.startswith(("--cuda-gpu-arch=", "--offload-arch=")):
            arch = arg.split("=", 1)[1]
            if not re.fullmatch(r"sm_[1-9][0-9]+[af]?", arch):
                raise ValueError(f"Invalid Clang CUDA architecture: {arg}")
            architectures.append(arg)
            continue
        if arg.startswith(("--cuda-include-ptx=", "--no-cuda-include-ptx=")):
            arch = arg.split("=", 1)[1]
            if not re.fullmatch(r"sm_[1-9][0-9]+[af]?|all", arch):
                raise ValueError(f"Invalid Clang CUDA PTX option: {arg}")
            continue
        if arg.startswith("--cuda-path="):
            cuda_path = Path(arg.split("=", 1)[1])
            if not cuda_path.is_dir():
                raise ValueError(f"Invalid --cuda-path directory: {cuda_path}")
            continue
        if arg == "--cuda-path":
            cuda_path = Path(next(args, ""))
            if not cuda_path.is_dir():
                raise ValueError(f"Invalid --cuda-path directory: {cuda_path}")
            continue
        if arg in {"-fgpu-rdc", "-fcuda-rdc"}:
            relocatable = True
            continue
        if arg in {"-fno-gpu-rdc", "-fno-cuda-rdc"}:
            relocatable = False
            continue
        if arg in {"--cuda-device-only", "-fcuda-device-only", "--cuda-host-only", "-nogpulib",
                   "--offload-link", "-fno-cuda-short-ptr"}:
            if arg in {"--cuda-device-only", "-fcuda-device-only"}:
                device_only = True
            if arg == "--offload-link":
                offload_link = True
            continue
        if arg in {"-v", "--verbose"}:
            verbose = True
            host_args.append("-v")
            continue
        if arg == "-c":
            compile_only = True
            host_args.append("-c")
            continue
        if arg == "-S":
            host_args.append("-S")
            continue
        if arg == "-o":
            val = next(args, "")
            output = Path(val)
            host_args.extend(["-o", val])
            continue
        if arg == "-x":
            lang = next(args, "")
            cuda_lang = lang in {"cuda", "cu"}
            if not cuda_lang:
                host_args.extend(["-x", lang])
                host_cuda_lang = False
            continue
        if arg in HOST_VALUE_OPTIONS:
            val = next(args, "")
            host_args.extend([arg, val])
            continue
        if arg.startswith("-"):
            host_args.append(arg)
            continue
        inputs.append(Path(arg))
        is_cuda = cuda_lang or arg.endswith((".cu", "link.stub"))
        if is_cuda != host_cuda_lang:
            host_args.extend(["-x", "c++" if is_cuda else "none"])
            host_cuda_lang = is_cuda
        if is_cuda:
            sources.append(Path(arg))
        host_args.append(arg)

    if device_only:
        out = output or (Path(sources[0].name).with_suffix(".s") if sources else None)
        if out is None:
            raise ValueError("Specify an output for Clang CUDA device-only compilation.")
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text("ptx\n" + "\n".join(architectures or ["sm_52"]) + "\n", encoding="utf-8")
        return _log_raw({
            "phase": "ptx",
            "argv": argv,
            "inputs": [str(p.resolve()) for p in inputs],
            "output": str(out.resolve()),
            "architectures": architectures,
            "relocatable": relocatable,
            "cuda_path": str(cuda_path) if cuda_path else None,
            "exit_code": 0,
        })

    if verbose:
        for arch_flag in architectures:
            sm = arch_flag.split("=", 1)[1]
            print(f' "/mock/clang" "-cc1" "-triple" "nvptx64-nvidia-cuda" "-target-cpu" "{sm}"',
                  file=sys.stderr)

    toolkit = cuda_path or root
    command = [compiler]
    if sources:
        command.extend([
            f"-I{toolkit / 'include'}",
            "-include", str(root / "mock/cuda_shim.h"),
            "-Wno-macro-redefined",
            "-U__apple_build_version__",
            "-D__CUDACC__",
            "-D__CUDA__",
            f"-D__clang_major__={major}",
            f"-D__clang_minor__={minor}",
            f"-D__clang_patchlevel__={patch}",
        ])
    elif not compile_only:
        command.append(f"-L{toolkit / 'lib64'}")
    extra_cleanup: Path | None = None
    if offload_link and not compile_only:
        import tempfile
        fd, stub_name = tempfile.mkstemp(prefix="mock-clang-offload-", suffix=".o")
        os.close(fd)
        extra_cleanup = Path(stub_name)
        subprocess.run(
            [compiler, "-fPIC", "-x", "c++", "-c", "-", "-o", str(extra_cleanup)],
            input=b'extern "C" int rcmake_mock_clang_device_link() { return 42; }\n',
            check=True,
            timeout=TIMEOUT,
        )
        host_args.append(str(extra_cleanup))
    try:
        exit_code = subprocess.run([*command, *host_args], timeout=TIMEOUT).returncode
    finally:
        if extra_cleanup is not None:
            extra_cleanup.unlink(missing_ok=True)
    phase = "clang-stub" if any(s.name == "link.stub" for s in sources) else (
        "compile" if compile_only else "link"
    )
    return _log_raw({
        "phase": phase,
        "argv": argv,
        "inputs": [str(p.resolve()) for p in inputs],
        "output": str(output.resolve()) if output else None,
        "architectures": architectures,
        "relocatable": relocatable,
        "offload_link": offload_link,
        "cuda_path": str(cuda_path) if cuda_path else None,
        "exit_code": exit_code,
    })


def run(argv: list[str]) -> int:
    if argv == ["--help"]:
        print(__doc__)
        return 0
    if os.name != "posix":
        raise ValueError("The mock toolkit requires POSIX host tools.")
    depth = int(os.environ.get("MOCK_NVCC_DEPTH", "0"))
    if depth >= 3:
        raise ValueError(f"Recursive mock nvcc invocation detected (depth={depth}).")
    os.environ["MOCK_NVCC_DEPTH"] = str(depth + 1)
    tool_name = Path(sys.argv[0]).name
    if tool_name == "host-link":
        return host_link(argv)
    if tool_name == "nvlink":
        return run_nvlink(argv)
    if tool_name == "fatbinary":
        return run_fatbinary(argv)
    if tool_name == "clang++":
        return run_clang_cuda(argv)
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

    options = parse(expand_response_files(argv))
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
