#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Differential evaluation suite for CUDA, HIP, and Swift toolchains.

Runs every fixture under conformance/cases/languages/{cuda,hip,swift}/ through
both the CMake 4.5.1 oracle ($RCMAKE_ORACLE_CMAKE) and Rcmake (--rcmake),
verifying:
  - Upfront version checks on both oracle and engine binaries
  - Configure exit status and exact diagnostic phrases (including negative cases)
  - Full normalized File API v1 codemodel-v2 + toolchains-v1 parity
  - Normalized Swift output-file-map.json parity across single- and multi-config builds
  - Clean parallel build (--parallel 4), strict mock compiler trace validation, and
    safe execution of built host binaries
  - No-op rebuild (zero mock compiler invocations on unchanged tree)
  - Incremental upstream edit rebuild (selective rebuilds and updated runtime output)
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time

REPO_ROOT = Path(__file__).resolve().parents[2]
CASES_ROOT = REPO_ROOT / "conformance" / "cases" / "languages"
MOCK_NVCC = REPO_ROOT / "tools" / "measure" / "mock_nvcc.py"
MOCK_HIPCC = REPO_ROOT / "tools" / "measure" / "mock_hipcc.py"
MOCK_SWIFTC = REPO_ROOT / "tools" / "measure" / "mock_swiftc.py"


def run_cmd(
    cmd: list[str],
    *,
    env: dict[str, str] | None = None,
    cwd: Path | None = None,
    timeout: int = 120,
    check: bool = True,
) -> subprocess.CompletedProcess[str]:
    proc = subprocess.run(
        cmd,
        env=env,
        cwd=cwd,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        timeout=timeout,
    )
    if check and proc.returncode != 0:
        raise RuntimeError(
            f"Command failed (exit {proc.returncode}): {' '.join(cmd)}\n{proc.stdout}"
        )
    return proc


def seed_fileapi_queries(build_dir: Path) -> None:
    qdir = build_dir / ".cmake" / "api" / "v1" / "query"
    qdir.mkdir(parents=True, exist_ok=True)
    for name in ("codemodel-v2", "toolchains-v1"):
        (qdir / name).write_text("", encoding="utf-8")


def _norm_path_text(text: str, src: Path, bld: Path) -> str:
    pairs = [
        (str(bld.resolve()), "<BLD>"),
        (str(bld), "<BLD>"),
        (str(src.resolve()), "<SRC>"),
        (str(src), "<SRC>"),
    ]
    out = text
    for old, new in pairs:
        if old:
            out = out.replace(old, new)
    return out


def _norm_ws(text: str) -> str:
    return " ".join(text.split())


def read_fileapi_summary(src: Path, build_dir: Path) -> dict:
    reply_dir = build_dir / ".cmake" / "api" / "v1" / "reply"
    indexes = sorted(reply_dir.glob("index-*.json"))
    if not indexes:
        raise RuntimeError(f"Missing File API index in {reply_dir}")
    index = json.loads(indexes[-1].read_text(encoding="utf-8"))
    seen_kinds: set[str] = set()
    summary: dict = {"toolchains": {}, "targets": {}}
    for obj in index.get("objects", []):
        kind = obj.get("kind")
        if kind not in {"toolchains", "codemodel"}:
            continue
        seen_kinds.add(kind)
        data = json.loads((reply_dir / obj["jsonFile"]).read_text(encoding="utf-8"))
        if kind == "toolchains":
            for tc in data.get("toolchains", []):
                lang = tc["language"]
                comp = tc.get("compiler", {})
                implicit_raw = comp.get("implicit", {})
                implicit = {
                    key: [_norm_path_text(str(item), src, build_dir) for item in val]
                    for key, val in sorted(implicit_raw.items())
                    if isinstance(val, list) and val
                }
                summary["toolchains"][lang] = {
                    "path": _norm_path_text(comp.get("path", ""), src, build_dir),
                    "id": comp.get("id", ""),
                    "version": comp.get("version", ""),
                    "target": comp.get("target", ""),
                    "commandFragment": _norm_ws(
                        _norm_path_text(comp.get("commandFragment", ""), src, build_dir)
                    ),
                    "implicit": implicit,
                    "extensions": sorted(tc.get("sourceFileExtensions", [])),
                }
        elif kind == "codemodel":
            for cfg in data.get("configurations", []):
                cfg_name = cfg.get("name", "")
                cfg_targets = {}
                for tref in cfg.get("targets", []):
                    tdata = json.loads((reply_dir / tref["jsonFile"]).read_text(encoding="utf-8"))
                    sources_list = tdata.get("sources", [])
                    groups = []
                    for cg in tdata.get("compileGroups", []):
                        src_paths = sorted(
                            _norm_path_text(sources_list[idx]["path"], src, build_dir)
                            for idx in cg.get("sourceIndexes", [])
                            if 0 <= idx < len(sources_list)
                        )
                        defines = sorted(
                            d.get("define", "")
                            for d in cg.get("defines", [])
                            if d.get("define")
                        )
                        includes = [
                            {
                                "path": _norm_path_text(inc.get("path", ""), src, build_dir),
                                "isSystem": bool(inc.get("isSystem", False)),
                            }
                            for inc in cg.get("includes", [])
                        ]
                        flags = [
                            _norm_ws(_norm_path_text(f.get("fragment", ""), src, build_dir))
                            for f in cg.get("compileCommandFragments", [])
                            if f.get("fragment", "").strip()
                        ]
                        groups.append({
                            "language": cg.get("language", ""),
                            "standard": cg.get("languageStandard", {}).get("standard"),
                            "sources": src_paths,
                            "defines": defines,
                            "includes": includes,
                            "flags": flags,
                        })
                    deps = sorted(
                        dep["id"].split("::", 1)[0]
                        for dep in tdata.get("dependencies", [])
                        if "id" in dep
                    )
                    link_raw = tdata.get("link")
                    link_norm = None
                    if isinstance(link_raw, dict):
                        link_norm = {
                            "language": link_raw.get("language", ""),
                            "commandFragments": [
                                {
                                    "fragment": _norm_ws(
                                        _norm_path_text(f.get("fragment", ""), src, build_dir)
                                    ),
                                    "role": f.get("role", ""),
                                }
                                for f in link_raw.get("commandFragments", [])
                                if f.get("fragment", "").strip()
                            ],
                        }
                    cfg_targets[tdata["name"]] = {
                        "type": tdata.get("type", ""),
                        "dependencies": deps,
                        "compileGroups": sorted(
                            groups,
                            key=lambda g: (g["language"], str(g["standard"]), g["sources"]),
                        ),
                        "link": link_norm,
                    }
                summary["targets"][cfg_name] = cfg_targets
    missing = {"toolchains", "codemodel"} - seen_kinds
    if missing:
        raise RuntimeError(
            f"Incomplete File API reply in {reply_dir}: missing {sorted(missing)}"
        )
    if not summary["toolchains"]:
        raise RuntimeError(f"File API toolchains-v1 reply is empty in {reply_dir}")
    if not summary["targets"] or all(not tgts for tgts in summary["targets"].values()):
        raise RuntimeError(f"File API codemodel-v2 reply has no targets in {reply_dir}")
    return summary


def read_normalized_output_maps(src: Path, build_dir: Path) -> dict[str, dict[str, dict[str, str]]]:
    maps: dict[str, dict[str, dict[str, str]]] = {}
    for map_file in sorted((build_dir / "CMakeFiles").rglob("output-file-map.json")):
        rel = map_file.relative_to(build_dir).as_posix()
        raw = json.loads(map_file.read_text(encoding="utf-8"))
        norm_map: dict[str, dict[str, str]] = {}
        for key, entry in raw.items():
            norm_key = re.sub(r"//+", "/", _norm_path_text(key, src, build_dir))
            norm_map[norm_key] = {
                ek: re.sub(r"//+", "/", _norm_path_text(str(ev), src, build_dir))
                for ek, ev in sorted(entry.items())
            }
        maps[rel] = dict(sorted(norm_map.items()))
    return maps


def read_jsonl(path: Path) -> list[dict]:
    if not path.is_file():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]


def compare_fileapi(case: str, oracle_sum: dict, rcmake_sum: dict) -> None:
    if oracle_sum["toolchains"] != rcmake_sum["toolchains"]:
        raise RuntimeError(
            f"[{case}] File API toolchains mismatch:\n"
            f"  oracle: {json.dumps(oracle_sum['toolchains'], sort_keys=True, indent=2)}\n"
            f"  rcmake: {json.dumps(rcmake_sum['toolchains'], sort_keys=True, indent=2)}"
        )
    if oracle_sum["targets"] != rcmake_sum["targets"]:
        raise RuntimeError(
            f"[{case}] File API codemodel targets mismatch:\n"
            f"  oracle: {json.dumps(oracle_sum['targets'], sort_keys=True, indent=2)}\n"
            f"  rcmake: {json.dumps(rcmake_sum['targets'], sort_keys=True, indent=2)}"
        )


def compare_output_maps(case: str, oracle_maps: dict, rcmake_maps: dict) -> None:
    if not oracle_maps:
        raise RuntimeError(f"[{case}] Oracle produced no Swift output-file-map.json files")
    if oracle_maps != rcmake_maps:
        raise RuntimeError(
            f"[{case}] Swift output-file-map.json mismatch:\n"
            f"  oracle: {json.dumps(oracle_maps, sort_keys=True, indent=2)}\n"
            f"  rcmake: {json.dumps(rcmake_maps, sort_keys=True, indent=2)}"
        )


def _find_compile_row(rows: list[dict], suffix: str, label: str, engine: str) -> dict:
    matches = [
        r for r in rows
        if r["phase"] == "compile" and any(i.endswith(suffix) for i in r["inputs"])
    ]
    if len(matches) != 1:
        raise RuntimeError(
            f"[{label}/{engine}] Expected exactly 1 compile row for {suffix}, got {matches}"
        )
    return matches[0]


def run_cuda_nvcc_case(
    work: Path,
    engines: list[tuple[str, Path]],
    cuda_root: Path,
    generator: str,
) -> None:
    label = f"cuda/nvcc-separable ({generator})"
    summaries: dict[str, dict] = {}
    for name, cmake in engines:
        case_dir = work / f"cuda-nvcc-{generator.replace(' ', '_')}-{name}"
        src = case_dir / "src"
        bld = case_dir / "bld"
        log = case_dir / "nvcc.jsonl"
        shutil.copytree(CASES_ROOT / "cuda" / "nvcc-separable", src)
        seed_fileapi_queries(bld)
        env = {**os.environ, "MOCK_NVCC_LOG": str(log)}
        run_cmd(
            [
                str(cmake), "-G", generator, "-S", str(src), "-B", str(bld),
                f"-DCMAKE_CUDA_COMPILER={cuda_root / 'bin/nvcc'}",
                "-DCMAKE_CUDA_ARCHITECTURES=75",
                f"-DMOCK_CUDA_LINKER={cuda_root / 'mock/host-link'}",
            ],
            env=env,
        )
        summaries[name] = read_fileapi_summary(src, bld)
        log.unlink(missing_ok=True)

        run_cmd([str(cmake), "--build", str(bld), "--parallel", "4"], env=env)
        rows = read_jsonl(log)

        # Verify exact relocatable-device-code state and architectures per source.
        basic_row = _find_compile_row(rows, "/basic.cu", label, name)
        if basic_row["relocatable"] is not False:
            raise RuntimeError(f"[{label}/{name}] basic.cu should not enable RDC: {basic_row}")

        off_row = _find_compile_row(rows, "/arch_off.cu", label, name)
        if off_row["relocatable"] is not False or off_row["architectures"] != []:
            raise RuntimeError(f"[{label}/{name}] arch_off.cu unexpected flags: {off_row}")

        dev_row = _find_compile_row(rows, "/device.cu", label, name)
        if dev_row["relocatable"] is not True or dev_row["architectures"] != [
            "--generate-code=arch=compute_75,code=[sm_75]",
            "--generate-code=arch=compute_80,code=[compute_80]",
        ]:
            raise RuntimeError(f"[{label}/{name}] Unexpected device.cu trace: {dev_row}")

        resolved_row = _find_compile_row(rows, "/resolved.cu", label, name)
        if resolved_row["relocatable"] is not True:
            raise RuntimeError(f"[{label}/{name}] resolved.cu must enable RDC: {resolved_row}")

        main_row = _find_compile_row(rows, "/main.cu", label, name)
        if main_row["relocatable"] is not True:
            raise RuntimeError(f"[{label}/{name}] main.cu must enable RDC: {main_row}")
        if not any(a in {"--std=c++17", "-std=c++17"} for a in main_row["argv"]):
            raise RuntimeError(f"[{label}/{name}] main.cu missing C++17 standard flag: {main_row}")

        ptx_rows = [r for r in rows if r["phase"] == "ptx"]
        if len(ptx_rows) != 1 or ptx_rows[0]["relocatable"] is not False or not any(
            i.endswith("/ptx.cu") for i in ptx_rows[0]["inputs"]
        ):
            raise RuntimeError(f"[{label}/{name}] Unexpected ptx trace: {ptx_rows}")

        # Verify exact device-link count and target inputs (resolved_lib + app).
        dlink_rows = [r for r in rows if r["phase"] == "device-link"]
        if len(dlink_rows) != 2:
            raise RuntimeError(
                f"[{label}/{name}] Expected 2 device-link steps (resolved_lib + app), got {dlink_rows}"
            )
        app_dlink = [r for r in dlink_rows if r["output"] and "/app.dir/" in r["output"]]
        res_dlink = [r for r in dlink_rows if r["output"] and "/resolved_lib.dir/" in r["output"]]
        if len(app_dlink) != 1 or not any(
            i.endswith("libdevlib.a") for i in app_dlink[0]["inputs"]
        ) or not any(i.endswith("main.cu.o") for i in app_dlink[0]["inputs"]):
            raise RuntimeError(f"[{label}/{name}] Unexpected app device-link inputs: {app_dlink}")
        if len(res_dlink) != 1 or not any(
            i.endswith("resolved.cu.o") for i in res_dlink[0]["inputs"]
        ):
            raise RuntimeError(
                f"[{label}/{name}] Unexpected resolved_lib device-link inputs: {res_dlink}"
            )

        # Verify host-link consumes cmake_device_link.o once and passes CUDA runtime libs.
        host_links = [r for r in rows if r["phase"] == "host-link"]
        if len(host_links) != 1:
            raise RuntimeError(f"[{label}/{name}] Expected 1 host-link trace for app: {host_links}")
        host_tokens = host_links[0].get("expanded_argv", host_links[0]["argv"])
        dlink_tokens = [
            tok for tok in host_tokens if tok.endswith("cmake_device_link.o")
        ]
        if len(dlink_tokens) != 1:
            raise RuntimeError(
                f"[{label}/{name}] Expected cmake_device_link.o once on host link: {host_tokens}"
            )
        if not any(
            tok in {"-lcudart_static", "-lcudart", "-lcudadevrt"}
            or tok.endswith(("libcudart_static.a", "libcudadevrt.a"))
            for tok in host_tokens
        ):
            raise RuntimeError(
                f"[{label}/{name}] Expected CUDA runtime library token on host link: {host_tokens}"
            )

        run_cmd([str(bld / "basic")], env=env)
        run_cmd([str(bld / "app")], env=env)

        log.unlink(missing_ok=True)
        run_cmd([str(cmake), "--build", str(bld), "--parallel", "4"], env=env)
        noop_rows = read_jsonl(log)
        if noop_rows:
            raise RuntimeError(f"[{label}/{name}] No-op rebuild invoked nvcc: {noop_rows}")

    compare_fileapi(label, summaries["oracle"], summaries["rcmake"])


def run_cuda_clang_case(
    work: Path,
    engines: list[tuple[str, Path]],
    cuda_root: Path,
    *,
    clang_version: str,
    expected_arch_flag: str,
) -> None:
    label = f"cuda/clang-cuda-{clang_version.split('.')[0]} (Ninja)"
    summaries: dict[str, dict] = {}
    for name, cmake in engines:
        case_dir = work / f"cuda-clang-{clang_version.replace('.', '_')}-{name}"
        src = case_dir / "src"
        bld = case_dir / "bld"
        log = case_dir / "clang-cuda.jsonl"
        shutil.copytree(CASES_ROOT / "cuda" / "clang-cuda", src)
        seed_fileapi_queries(bld)
        env = {
            **os.environ,
            "MOCK_NVCC_LOG": str(log),
            "MOCK_CUDA_ROOT": str(cuda_root),
            "MOCK_CLANG_CUDA_VERSION": clang_version,
        }
        run_cmd(
            [
                str(cmake), "-G", "Ninja", "-S", str(src), "-B", str(bld),
                f"-DCMAKE_CUDA_COMPILER={cuda_root / 'bin/clang++'}",
                f"-DCUDAToolkit_ROOT={cuda_root}",
                "-DCMAKE_CUDA_ARCHITECTURES=75",
            ],
            env=env,
        )
        summaries[name] = read_fileapi_summary(src, bld)
        log.unlink(missing_ok=True)

        run_cmd([str(cmake), "--build", str(bld), "--parallel", "4"], env=env)
        rows = read_jsonl(log)
        phases = {r["phase"] for r in rows}
        clang_major = int(clang_version.split(".")[0])
        required_phases = (
            ("compile", "ptx", "link")
            if clang_major >= 20
            else ("compile", "nvlink", "fatbinary", "clang-stub", "ptx", "link")
        )
        for req in required_phases:
            if req not in phases:
                raise RuntimeError(f"[{label}/{name}] Missing Clang CUDA phase '{req}': {phases}")

        for src_name in ("/main.cu", "/device.cu"):
            row = _find_compile_row(rows, src_name, label, name)
            if not row.get("cuda_path"):
                raise RuntimeError(f"[{label}/{name}] Missing --cuda-path= on {src_name}: {row}")
            if row["relocatable"] is not True:
                raise RuntimeError(f"[{label}/{name}] {src_name} must enable -fgpu-rdc: {row}")
            if expected_arch_flag not in row["architectures"]:
                raise RuntimeError(
                    f"[{label}/{name}] Expected {expected_arch_flag} on {src_name}, got {row['architectures']}"
                )

        ptx_rows = [r for r in rows if r["phase"] == "ptx"]
        if len(ptx_rows) != 1 or ptx_rows[0]["relocatable"] is not False or (
            expected_arch_flag not in ptx_rows[0]["architectures"]
        ):
            raise RuntimeError(f"[{label}/{name}] Unexpected Clang PTX trace: {ptx_rows}")

        link_rows = [r for r in rows if r["phase"] == "link"]
        if len(link_rows) != 1:
            raise RuntimeError(f"[{label}/{name}] Expected 1 link row: {link_rows}")
        if clang_major >= 20:
            if phases & {"nvlink", "fatbinary", "clang-stub"}:
                raise RuntimeError(
                    f"[{label}/{name}] Clang 20+ should use --offload-link instead of explicit nvlink: {phases}"
                )
            if link_rows[0].get("offload_link") is not True:
                raise RuntimeError(
                    f"[{label}/{name}] Expected --offload-link on Clang 20+ link: {link_rows[0]['argv']}"
                )
        else:
            nvlink_rows = [r for r in rows if r["phase"] == "nvlink"]
            if len(nvlink_rows) != 1 or nvlink_rows[0]["architectures"] != ["sm_75"]:
                raise RuntimeError(f"[{label}/{name}] Unexpected nvlink trace: {nvlink_rows}")
            fatbin_rows = [r for r in rows if r["phase"] == "fatbinary"]
            if len(fatbin_rows) != 1 or fatbin_rows[0]["architectures"] != ["sm_75"]:
                raise RuntimeError(f"[{label}/{name}] Unexpected fatbinary trace: {fatbin_rows}")
            stub_rows = [r for r in rows if r["phase"] == "clang-stub"]
            if len(stub_rows) != 1:
                raise RuntimeError(f"[{label}/{name}] Expected 1 clang-stub compile: {stub_rows}")
            dlink_tokens = [
                tok for tok in link_rows[0]["argv"] if tok.endswith("cmake_device_link.o")
            ]
            if len(dlink_tokens) != 1:
                raise RuntimeError(
                    f"[{label}/{name}] Expected cmake_device_link.o once on Clang link: {link_rows[0]['argv']}"
                )

        run_cmd([str(bld / "clang_sep")], env=env)

        log.unlink(missing_ok=True)
        run_cmd([str(cmake), "--build", str(bld), "--parallel", "4"], env=env)
        if read_jsonl(log):
            raise RuntimeError(f"[{label}/{name}] No-op rebuild re-ran Clang CUDA tools")

    compare_fileapi(label, summaries["oracle"], summaries["rcmake"])


def run_hip_amd_case(
    work: Path,
    engines: list[tuple[str, Path]],
    rocm_amd: Path,
    generator: str,
) -> None:
    label = f"hip/amd-runtime ({generator})"
    summaries: dict[str, dict] = {}
    for name, cmake in engines:
        case_dir = work / f"hip-amd-{generator.replace(' ', '_')}-{name}"
        src = case_dir / "src"
        bld = case_dir / "bld"
        log = case_dir / "hip.jsonl"
        shutil.copytree(CASES_ROOT / "hip" / "amd-runtime", src)
        seed_fileapi_queries(bld)
        env = {
            **os.environ,
            "MOCK_HIP_LOG": str(log),
            "MOCK_ROCM_ROOT": str(rocm_amd),
            "PATH": f"{rocm_amd / 'bin'}:{os.environ.get('PATH', '')}",
        }
        cxx_arg = (
            [f"-DCMAKE_CXX_COMPILER={os.environ['MOCK_HIP_CXX']}"]
            if os.environ.get("MOCK_HIP_CXX")
            else []
        )
        run_cmd(
            [
                str(cmake), "-G", generator, "-S", str(src), "-B", str(bld),
                *cxx_arg,
                "-DCMAKE_HIP_PLATFORM=amd",
                f"-DCMAKE_HIP_COMPILER={rocm_amd / 'bin/clang++'}",
            ],
            env=env,
        )
        summaries[name] = read_fileapi_summary(src, bld)
        log.unlink(missing_ok=True)

        run_cmd([str(cmake), "--build", str(bld), "--parallel", "4"], env=env)
        rows = read_jsonl(log)
        kernel_row = _find_compile_row(rows, "/kernel.hip", label, name)
        if kernel_row["offload_archs"] != ["gfx90a:xnack+", "gfx1100"]:
            raise RuntimeError(
                f"[{label}/{name}] Expected ordered AMD HIP offload archs, got {kernel_row}"
            )
        off_row = _find_compile_row(rows, "/arch_off.hip", label, name)
        if off_row["offload_archs"] != []:
            raise RuntimeError(f"[{label}/{name}] HIP_ARCHITECTURES OFF emitted archs: {off_row}")

        run_cmd([str(bld / "consumer")], env=env)

        log.unlink(missing_ok=True)
        run_cmd([str(cmake), "--build", str(bld), "--parallel", "4"], env=env)
        if read_jsonl(log):
            raise RuntimeError(f"[{label}/{name}] No-op rebuild invoked HIP compiler")

    compare_fileapi(label, summaries["oracle"], summaries["rcmake"])


def run_hip_genex_case(
    work: Path,
    engines: list[tuple[str, Path]],
    rocm_amd: Path,
) -> None:
    label = "hip/genex-source (Ninja Debug+Release)"
    for cfg in ("Debug", "Release"):
        summaries: dict[str, dict] = {}
        for name, cmake in engines:
            case_dir = work / f"hip-genex-{cfg}-{name}"
            src = case_dir / "src"
            bld = case_dir / "bld"
            log = case_dir / "hip-genex.jsonl"
            shutil.copytree(CASES_ROOT / "hip" / "genex-source", src)
            seed_fileapi_queries(bld)
            env = {
                **os.environ,
                "MOCK_HIP_LOG": str(log),
                "MOCK_ROCM_ROOT": str(rocm_amd),
                "PATH": f"{rocm_amd / 'bin'}:{os.environ.get('PATH', '')}",
            }
            cxx_arg = (
                [f"-DCMAKE_CXX_COMPILER={os.environ['MOCK_HIP_CXX']}"]
                if os.environ.get("MOCK_HIP_CXX")
                else []
            )
            run_cmd(
                [
                    str(cmake), "-G", "Ninja", "-S", str(src), "-B", str(bld),
                    f"-DCMAKE_BUILD_TYPE={cfg}",
                    *cxx_arg,
                    "-DCMAKE_HIP_PLATFORM=amd",
                    f"-DCMAKE_HIP_COMPILER={rocm_amd / 'bin/clang++'}",
                ],
                env=env,
            )
            summaries[name] = read_fileapi_summary(src, bld)
            link_frags = [
                f["fragment"]
                for f in (summaries[name]["targets"][cfg]["consumer"]["link"] or {}).get(
                    "commandFragments", []
                )
            ]
            has_rt = any("libhip_mock_rt.a" in frag for frag in link_frags)
            if cfg == "Debug" and not has_rt:
                raise RuntimeError(
                    f"[{label}/{name}/Debug] Expected libhip_mock_rt.a in Debug consumer link: {link_frags}"
                )
            if cfg == "Release" and has_rt:
                raise RuntimeError(
                    f"[{label}/{name}/Release] Unexpected libhip_mock_rt.a in Release consumer link: {link_frags}"
                )

            log.unlink(missing_ok=True)
            run_cmd([str(cmake), "--build", str(bld), "--parallel", "4"], env=env)
            rows = read_jsonl(log)
            if cfg == "Debug":
                kernel_row = _find_compile_row(rows, "/kernel.hip", f"{label}/{cfg}", name)
                if kernel_row["offload_archs"] != ["gfx90a"]:
                    raise RuntimeError(
                        f"[{label}/{name}/Debug] Unexpected offload archs: {kernel_row}"
                    )
            else:
                if rows:
                    raise RuntimeError(
                        f"[{label}/{name}/Release] Release unexpectedly invoked HIP compiler: {rows}"
                    )
            run_cmd([str(bld / "consumer")], env=env)

            log.unlink(missing_ok=True)
            run_cmd([str(cmake), "--build", str(bld), "--parallel", "4"], env=env)
            if read_jsonl(log):
                raise RuntimeError(f"[{label}/{name}/{cfg}] No-op rebuild invoked HIP compiler")

        compare_fileapi(f"{label}/{cfg}", summaries["oracle"], summaries["rcmake"])


def run_hip_spirv_case(work: Path, engines: list[tuple[str, Path]], rocm_spirv: Path) -> None:
    label = "hip/spirv-pkg (Ninja)"
    summaries: dict[str, dict] = {}
    for name, cmake in engines:
        case_dir = work / f"hip-spirv-{name}"
        src = case_dir / "src"
        bld = case_dir / "bld"
        log = case_dir / "hip-spirv.jsonl"
        shutil.copytree(CASES_ROOT / "hip" / "spirv-pkg", src)
        seed_fileapi_queries(bld)
        env = {
            **os.environ,
            "MOCK_HIP_LOG": str(log),
            "MOCK_ROCM_ROOT": str(rocm_spirv),
            "HIP_PATH": str(rocm_spirv),
            "PATH": f"{rocm_spirv / 'bin'}:{os.environ.get('PATH', '')}",
        }
        cxx_arg = (
            [f"-DCMAKE_CXX_COMPILER={os.environ['MOCK_HIP_CXX']}"]
            if os.environ.get("MOCK_HIP_CXX")
            else []
        )
        run_cmd(
            [
                str(cmake), "-G", "Ninja", "-S", str(src), "-B", str(bld),
                *cxx_arg,
                "-DCMAKE_HIP_PLATFORM=spirv",
                f"-DCMAKE_HIP_COMPILER={rocm_spirv / 'bin/clang++'}",
            ],
            env=env,
        )
        summaries[name] = read_fileapi_summary(src, bld)
        log.unlink(missing_ok=True)
        run_cmd([str(cmake), "--build", str(bld), "--parallel", "4"], env=env)
        rows = read_jsonl(log)
        compiles = [r for r in rows if r["phase"] == "compile"]
        if not compiles or any(a.startswith("gfx") for r in compiles for a in r["offload_archs"]):
            raise RuntimeError(f"[{label}/{name}] Unexpected SPIR-V offload archs: {compiles}")
        run_cmd([str(bld / "spirv_app")], env=env)

    compare_fileapi(label, summaries["oracle"], summaries["rcmake"])


def run_hip_nvidia_case(
    work: Path,
    engines: list[tuple[str, Path]],
    cuda_root: Path,
    rocm_amd: Path,
) -> None:
    label = "hip/nvidia-hip (Ninja)"
    summaries: dict[str, dict] = {}
    for name, cmake in engines:
        case_dir = work / f"hip-nvidia-{name}"
        src = case_dir / "src"
        bld = case_dir / "bld"
        log = case_dir / "hip-nv.jsonl"
        shutil.copytree(CASES_ROOT / "hip" / "nvidia-hip", src)
        seed_fileapi_queries(bld)
        env = {
            **os.environ,
            "MOCK_NVCC_LOG": str(log),
            "MOCK_CUDA_ROOT": str(cuda_root),
            "PATH": f"{rocm_amd / 'bin'}:{os.environ.get('PATH', '')}",
        }
        cxx_arg = (
            [f"-DCMAKE_CXX_COMPILER={os.environ['MOCK_HIP_CXX']}"]
            if os.environ.get("MOCK_HIP_CXX")
            else []
        )
        run_cmd(
            [
                str(cmake), "-G", "Ninja", "-S", str(src), "-B", str(bld),
                *cxx_arg,
                "-DCMAKE_HIP_PLATFORM=nvidia",
                f"-DCMAKE_HIP_COMPILER={cuda_root / 'bin/nvcc'}",
                f"-DCMAKE_HIP_COMPILER_ROCM_ROOT={rocm_amd}",
            ],
            env=env,
        )
        summaries[name] = read_fileapi_summary(src, bld)
        log.unlink(missing_ok=True)
        run_cmd([str(cmake), "--build", str(bld), "--parallel", "4"], env=env)
        rows = read_jsonl(log)
        compiles = [r for r in rows if r["phase"] == "compile"]
        if len(compiles) != 1 or compiles[0]["architectures"] != [
            "--generate-code=arch=compute_75,code=[sm_75]",
            "--generate-code=arch=compute_80,code=[compute_80]",
        ]:
            raise RuntimeError(f"[{label}/{name}] Unexpected NVIDIA HIP arch flags: {compiles}")
        run_cmd([str(bld / "nv_hip_app")], env=env)

    compare_fileapi(label, summaries["oracle"], summaries["rcmake"])


def run_swift_split_case(
    work: Path,
    engines: list[tuple[str, Path]],
    swift_root: Path,
    *,
    variant: str,
    generator: str = "Ninja",
    extra_args: list[str] | None = None,
) -> None:
    label = f"swift/split-modules ({variant})"
    summaries: dict[str, dict] = {}
    output_maps: dict[str, dict] = {}
    for name, cmake in engines:
        case_dir = work / f"swift-split-{variant}-{name}"
        src = case_dir / "src"
        bld = case_dir / "bld"
        log = case_dir / "swiftc.jsonl"
        shutil.copytree(CASES_ROOT / "swift" / "split-modules", src)
        seed_fileapi_queries(bld)
        env = {**os.environ, "MOCK_SWIFTC_LOG": str(log)}
        cfg_args = [
            str(cmake), "-G", generator, "-S", str(src), "-B", str(bld),
            f"-DCMAKE_Swift_COMPILER={swift_root / 'bin/swiftc'}",
            *(extra_args or []),
        ]
        run_cmd(cfg_args, env=env)
        summaries[name] = read_fileapi_summary(src, bld)
        output_maps[name] = read_normalized_output_maps(src, bld)
        log.unlink(missing_ok=True)

        if generator == "Ninja Multi-Config":
            for cfg in ("Debug", "Release"):
                map_rel = f"CMakeFiles/libA.dir/{cfg}/output-file-map.json"
                if map_rel not in output_maps[name]:
                    raise RuntimeError(f"[{label}/{name}/{cfg}] Missing {map_rel}")
                if "<BLD>/gen.swift" not in output_maps[name][map_rel]:
                    raise RuntimeError(
                        f"[{label}/{name}/{cfg}] Missing generated gen.swift entry in {map_rel}"
                    )
                run_cmd([str(cmake), "--build", str(bld), "--config", cfg, "--parallel", "4"], env=env)
                app_bin = bld / cfg / "app"
                res = run_cmd([str(app_bin)], env=env, check=False)
                if res.returncode != 60:
                    raise RuntimeError(f"[{label}/{name}/{cfg}] Expected exit 60, got {res.returncode}")
            continue

        if "<BLD>/gen.swift" not in output_maps[name].get(
            "CMakeFiles/libA.dir/output-file-map.json", {}
        ):
            raise RuntimeError(f"[{label}/{name}] Missing generated gen.swift in libA output-file-map")

        run_cmd([str(cmake), "--build", str(bld), "--parallel", "4"], env=env)
        rows = read_jsonl(log)
        emit_mods = [r for r in rows if r["phase"] == "emit-module"]
        if {r["module_name"] for r in emit_mods} != {"libA", "libB"}:
            raise RuntimeError(f"[{label}/{name}] Expected separate emit-module for libA & libB: {emit_mods}")
        wmo_rows = [r for r in rows if r["module_name"] == "libWmo"]
        if not any(r["phase"] == "compile-combined" and r["mode"] == "wholemodule" for r in wmo_rows):
            raise RuntimeError(f"[{label}/{name}] Expected combined wholemodule for libWmo: {wmo_rows}")
        if variant == "force-rsp" and not any(r.get("used_rsp") for r in rows):
            raise RuntimeError(f"[{label}/{name}] Expected @rsp response file usage: {rows}")
        if not (bld / "libA-Swift.h").is_file():
            raise RuntimeError(f"[{label}/{name}] Missing explicit -emit-objc-header output libA-Swift.h")
        if not list((bld / "custom_mods" / "libA.swiftmodule").glob("*.swiftmodule")):
            raise RuntimeError(f"[{label}/{name}] Missing CMP0195 module under custom_mods/libA.swiftmodule")

        res = run_cmd([str(bld / "app")], env=env, check=False)
        if res.returncode != 60:
            raise RuntimeError(f"[{label}/{name}] Expected app exit 60, got {res.returncode}")

        log.unlink(missing_ok=True)
        run_cmd([str(cmake), "--build", str(bld), "--parallel", "4"], env=env)
        if read_jsonl(log):
            raise RuntimeError(f"[{label}/{name}] No-op rebuild re-invoked swiftc")

        # Incremental upstream edit in libA/a/dup.swift: 10 -> 15 (new total 65).
        time.sleep(0.05)
        dup_a = src / "a" / "dup.swift"
        dup_a.write_text(
            "// MOCK_EXPORT a_dup_val = 15\npublic func aDupVal() -> Int { return 15 }\n",
            encoding="utf-8",
        )
        log.unlink(missing_ok=True)
        run_cmd([str(cmake), "--build", str(bld), "--parallel", "4"], env=env)
        inc_rows = read_jsonl(log)
        rebuilt_mods = {r.get("module_name") for r in inc_rows if r["phase"] in {"emit-module", "compile"}}
        if not {"libA", "libB", "app"}.issubset(rebuilt_mods):
            raise RuntimeError(
                f"[{label}/{name}] Incremental edit did not rebuild libA->libB->app: {rebuilt_mods}"
            )
        if "libWmo" in rebuilt_mods:
            raise RuntimeError(
                f"[{label}/{name}] Incremental edit unexpectedly rebuilt unrelated libWmo: {rebuilt_mods}"
            )
        res2 = run_cmd([str(bld / "app")], env=env, check=False)
        if res2.returncode != 65:
            raise RuntimeError(f"[{label}/{name}] Post-edit app expected exit 65, got {res2.returncode}")

    compare_fileapi(label, summaries["oracle"], summaries["rcmake"])
    compare_output_maps(label, output_maps["oracle"], output_maps["rcmake"])


def run_swift_genex_custom_mod_case(
    work: Path,
    engines: list[tuple[str, Path]],
    swift_root: Path,
) -> None:
    label = "swift/genex-custom-mod (Ninja)"
    summaries: dict[str, dict] = {}
    output_maps: dict[str, dict] = {}
    for name, cmake in engines:
        case_dir = work / f"swift-genex-mod-{name}"
        src = case_dir / "src"
        bld = case_dir / "bld"
        log = case_dir / "swiftc.jsonl"
        shutil.copytree(CASES_ROOT / "swift" / "genex-custom-mod", src)
        seed_fileapi_queries(bld)
        env = {**os.environ, "MOCK_SWIFTC_LOG": str(log)}
        run_cmd(
            [
                str(cmake), "-G", "Ninja", "-S", str(src), "-B", str(bld),
                "-DCMAKE_BUILD_TYPE=Debug",
                f"-DCMAKE_Swift_COMPILER={swift_root / 'bin/swiftc'}",
            ],
            env=env,
        )
        summaries[name] = read_fileapi_summary(src, bld)
        output_maps[name] = read_normalized_output_maps(src, bld)
        log.unlink(missing_ok=True)

        run_cmd([str(cmake), "--build", str(bld), "--parallel", "4"], env=env)
        rows = read_jsonl(log)
        emit_mods = {r["module_name"] for r in rows if r["phase"] == "emit-module"}
        if emit_mods != {"libA", "libB", "unrelated"}:
            raise RuntimeError(
                f"[{label}/{name}] Expected emit-module for libraries only (not app), got {emit_mods}"
            )
        if not list((bld / "mods_a" / "libA.swiftmodule").glob("*.swiftmodule")):
            raise RuntimeError(f"[{label}/{name}] Missing custom mods_a/libA.swiftmodule")
        if not list((bld / "mods_b" / "libB.swiftmodule").glob("*.swiftmodule")):
            raise RuntimeError(f"[{label}/{name}] Missing custom mods_b/libB.swiftmodule")

        res = run_cmd([str(bld / "app")], env=env, check=False)
        if res.returncode != 40:
            raise RuntimeError(f"[{label}/{name}] Expected app exit 40, got {res.returncode}")

        # No-op rebuild must invoke 0 swiftc commands.
        log.unlink(missing_ok=True)
        run_cmd([str(cmake), "--build", str(bld), "--parallel", "4"], env=env)
        if read_jsonl(log):
            raise RuntimeError(f"[{label}/{name}] No-op rebuild re-invoked swiftc")

        # Edit genex-selected upstream source a.swift (10 -> 25): libA, libB, app rebuild;
        # unrelated library must stay up to date.
        time.sleep(0.05)
        (src / "a.swift").write_text(
            "// MOCK_EXPORT a_val = 25\npublic func aVal() -> Int { return 25 }\n",
            encoding="utf-8",
        )
        log.unlink(missing_ok=True)
        run_cmd([str(cmake), "--build", str(bld), "--parallel", "4"], env=env)
        inc_rows = read_jsonl(log)
        rebuilt = {
            r.get("module_name")
            for r in inc_rows
            if r["phase"] in {"emit-module", "compile", "compile-combined"}
        }
        if not {"libA", "libB", "app"}.issubset(rebuilt):
            raise RuntimeError(
                f"[{label}/{name}] Upstream edit failed to rebuild libA/libB/app: {rebuilt}"
            )
        if "unrelated" in rebuilt:
            raise RuntimeError(
                f"[{label}/{name}] Upstream edit unnecessarily rebuilt unrelated: {rebuilt}"
            )
        res2 = run_cmd([str(bld / "app")], env=env, check=False)
        if res2.returncode != 55:
            raise RuntimeError(f"[{label}/{name}] Post-edit app expected exit 55, got {res2.returncode}")

    compare_fileapi(label, summaries["oracle"], summaries["rcmake"])
    compare_output_maps(label, output_maps["oracle"], output_maps["rcmake"])


def run_swift_legacy_case(work: Path, engines: list[tuple[str, Path]], swift_root: Path) -> None:
    label = "swift/legacy-cmp0157-old (Ninja)"
    summaries: dict[str, dict] = {}
    for name, cmake in engines:
        case_dir = work / f"swift-legacy-{name}"
        src = case_dir / "src"
        bld = case_dir / "bld"
        log = case_dir / "swiftc.jsonl"
        shutil.copytree(CASES_ROOT / "swift" / "legacy-cmp0157-old", src)
        seed_fileapi_queries(bld)
        env = {**os.environ, "MOCK_SWIFTC_LOG": str(log)}
        run_cmd(
            [
                str(cmake), "-G", "Ninja", "-S", str(src), "-B", str(bld),
                f"-DCMAKE_Swift_COMPILER={swift_root / 'bin/swiftc'}",
            ],
            env=env,
        )
        summaries[name] = read_fileapi_summary(src, bld)
        log.unlink(missing_ok=True)
        run_cmd([str(cmake), "--build", str(bld), "--parallel", "4"], env=env)
        rows = read_jsonl(log)
        if not any(r["phase"] == "legacy-compile-link" for r in rows):
            raise RuntimeError(f"[{label}/{name}] Expected legacy-compile-link phase: {rows}")
        run_cmd([str(bld / "old_app")], env=env)

    compare_fileapi(label, summaries["oracle"], summaries["rcmake"])


def run_negative_case(
    work: Path,
    engines: list[tuple[str, Path]],
    label: str,
    fixture: Path,
    generator: str,
    extra_args: list[str],
    expected_substrings: list[str],
    extra_env: dict[str, str] | None = None,
    forbid_log_env: str | None = None,
) -> None:
    for name, cmake in engines:
        case_dir = work / f"neg-{label.replace('/', '_')}-{name}"
        src = case_dir / "src"
        bld = case_dir / "bld"
        shutil.copytree(fixture, src)
        env = {**os.environ, **(extra_env or {})}
        forbid_log: Path | None = None
        if forbid_log_env:
            forbid_log = case_dir / "forbidden.jsonl"
            env[forbid_log_env] = str(forbid_log)
        proc = run_cmd(
            [str(cmake), "-G", generator, "-S", str(src), "-B", str(bld), *extra_args],
            env=env,
            check=False,
        )
        if proc.returncode == 0:
            raise RuntimeError(f"[neg:{label}/{name}] Expected failure, but configure succeeded")
        for expected in expected_substrings:
            if expected.lower() not in proc.stdout.lower():
                raise RuntimeError(
                    f"[neg:{label}/{name}] Expected '{expected}' in output:\n{proc.stdout}"
                )
        if forbid_log is not None:
            invoked = read_jsonl(forbid_log)
            if invoked:
                raise RuntimeError(
                    f"[neg:{label}/{name}] Forbidden tool was invoked instead of rejected: {invoked}"
                )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rcmake", type=Path, required=True, help="Path to rcmake binary")
    parser.add_argument(
        "--oracle",
        type=Path,
        default=Path(os.environ.get("RCMAKE_ORACLE_CMAKE", "")),
        help="Path to CMake 4.5.1 oracle binary (defaults to $RCMAKE_ORACLE_CMAKE)",
    )
    parser.add_argument(
        "--filter",
        default="",
        help="Optional substring filter to run only matching test cases",
    )
    args = parser.parse_args()

    rcmake = args.rcmake.resolve()
    oracle = args.oracle.resolve() if str(args.oracle) else Path()
    if not rcmake.is_file():
        raise SystemExit(f"Rcmake binary not found: {rcmake}")
    if not oracle.is_file():
        raise SystemExit("CMake 4.5.1 oracle not found; source env/env.sh or pass --oracle.")

    # Verify both binaries execute and report versions upfront before mock installation.
    oracle_ver = run_cmd([str(oracle), "--version"]).stdout.splitlines()[0].strip()
    rcmake_ver = run_cmd([str(rcmake), "--version"]).stdout.splitlines()[0].strip()
    if not oracle_ver or not rcmake_ver:
        raise SystemExit("Empty --version output from oracle or rcmake.")
    print(f"Oracle: {oracle_ver} | Engine: {rcmake_ver}", flush=True)

    engines = [("oracle", oracle), ("rcmake", rcmake)]
    real_cxx = shutil.which("clang++") or shutil.which("g++") or ""
    real_cc = shutil.which("clang") or shutil.which("cc") or shutil.which("gcc") or ""
    if real_cxx:
        os.environ["MOCK_NVCC_CXX"] = real_cxx
        os.environ["MOCK_HIP_CXX"] = real_cxx
    if real_cc:
        os.environ["MOCK_SWIFTC_CC"] = real_cc
    with tempfile.TemporaryDirectory(prefix="rcmake-lang-eval-") as tmp:
        work = Path(tmp).resolve()
        cuda_root = work / "mock-cuda"
        rocm_amd = work / "mock-rocm-amd"
        rocm_spirv = work / "mock-rocm-spirv"
        swift_root = work / "mock-swift"

        tests: list[tuple[str, callable]] = [
            ("cuda/nvcc-ninja", lambda: run_cuda_nvcc_case(work, engines, cuda_root, "Ninja")),
            ("cuda/nvcc-make", lambda: run_cuda_nvcc_case(work, engines, cuda_root, "Unix Makefiles")),
            (
                "cuda/clang-18-ninja",
                lambda: run_cuda_clang_case(
                    work, engines, cuda_root,
                    clang_version="18.1.8",
                    expected_arch_flag="--cuda-gpu-arch=sm_75",
                ),
            ),
            (
                "cuda/clang-20-ninja",
                lambda: run_cuda_clang_case(
                    work, engines, cuda_root,
                    clang_version="20.1.0",
                    expected_arch_flag="--offload-arch=sm_75",
                ),
            ),
            ("hip/amd-ninja", lambda: run_hip_amd_case(work, engines, rocm_amd, "Ninja")),
            ("hip/amd-make", lambda: run_hip_amd_case(work, engines, rocm_amd, "Unix Makefiles")),
            ("hip/genex-source", lambda: run_hip_genex_case(work, engines, rocm_amd)),
            ("hip/spirv-ninja", lambda: run_hip_spirv_case(work, engines, rocm_spirv)),
            ("hip/nvidia-ninja", lambda: run_hip_nvidia_case(work, engines, cuda_root, rocm_amd)),
            (
                "hip/neg-empty-arch",
                lambda: run_negative_case(
                    work, engines, "hip-empty-arch",
                    CASES_ROOT / "hip" / "empty-arch-error", "Ninja",
                    ["-DCMAKE_HIP_PLATFORM=amd", f"-DCMAKE_HIP_COMPILER={rocm_amd / 'bin/clang++'}"],
                    ['HIP_ARCHITECTURES is empty for target "bad"'],
                    {"MOCK_ROCM_ROOT": str(rocm_amd), "PATH": f"{rocm_amd / 'bin'}:{os.environ.get('PATH', '')}"},
                ),
            ),
            (
                "hip/neg-hipcc-wrapper",
                lambda: run_negative_case(
                    work, engines, "hip-hipcc-wrapper",
                    CASES_ROOT / "hip" / "amd-runtime", "Ninja",
                    ["-DCMAKE_HIP_PLATFORM=amd", f"-DCMAKE_HIP_COMPILER={rocm_amd / 'bin/hipcc'}"],
                    ["CMAKE_HIP_COMPILER is set to the hipcc wrapper", "Use Clang directly"],
                    forbid_log_env="MOCK_HIP_LOG",
                ),
            ),
            ("swift/split-ninja", lambda: run_swift_split_case(work, engines, swift_root, variant="default")),
            (
                "swift/split-force-rsp",
                lambda: run_swift_split_case(
                    work, engines, swift_root, variant="force-rsp",
                    extra_args=["-DCMAKE_NINJA_FORCE_RESPONSE_FILE=1"],
                ),
            ),
            (
                "swift/split-single-output-wmo",
                lambda: run_swift_split_case(
                    work, engines, swift_root, variant="single-output-wmo",
                    extra_args=["-DCMAKE_Swift_NUM_THREADS=0"],
                ),
            ),
            (
                "swift/split-multi-config",
                lambda: run_swift_split_case(
                    work, engines, swift_root, variant="multi-config",
                    generator="Ninja Multi-Config",
                ),
            ),
            ("swift/genex-custom-mod", lambda: run_swift_genex_custom_mod_case(work, engines, swift_root)),
            ("swift/legacy-old", lambda: run_swift_legacy_case(work, engines, swift_root)),
            (
                "swift/neg-unix-makefiles",
                lambda: run_negative_case(
                    work, engines, "swift-make",
                    CASES_ROOT / "swift" / "split-modules", "Unix Makefiles",
                    [f"-DCMAKE_Swift_COMPILER={swift_root / 'bin/swiftc'}"],
                    ['Swift language not supported by "Unix Makefiles" generator'],
                ),
            ),
        ]

        selected = [
            (name, fn) for name, fn in tests if not args.filter or args.filter in name
        ]
        if not selected:
            print(f"Error: filter '{args.filter}' matched zero test cases.", file=sys.stderr)
            return 2

        run_cmd([sys.executable, str(MOCK_NVCC), "--install-mock-toolkit", str(cuda_root)])
        run_cmd([sys.executable, str(MOCK_HIPCC), "--install-mock-rocm", str(rocm_amd), "--platform", "amd"])
        run_cmd([sys.executable, str(MOCK_HIPCC), "--install-mock-rocm", str(rocm_spirv), "--platform", "spirv"])
        run_cmd([sys.executable, str(MOCK_SWIFTC), "--install-mock-swift", str(swift_root)])

        passed = 0
        failed: list[tuple[str, str]] = []
        for name, fn in selected:
            start = time.monotonic()
            try:
                fn()
                elapsed = time.monotonic() - start
                print(f"PASS {name} ({elapsed:.2f}s)", flush=True)
                passed += 1
            except Exception as err:
                elapsed = time.monotonic() - start
                print(f"FAIL {name} ({elapsed:.2f}s): {err}", flush=True)
                failed.append((name, str(err)))

        print(f"\nSummary: {passed} passed, {len(failed)} failed", flush=True)
        return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
