# SPDX-License-Identifier: MIT OR Apache-2.0
"""Compare generated files, File API replies, and compile databases."""

from __future__ import annotations

from collections import Counter
from dataclasses import dataclass
from difflib import unified_diff
import filecmp
from functools import lru_cache
from hashlib import sha256
import json
import os
from pathlib import Path
import posixpath
import re
import shlex
import sys
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "measure"))
from ninja_canon import Normalizer, canonicalize_strict

BUCKETS = ("byte_identical", "identical_after_normalization", "different")
EXACT_NAMES = {
    "build.ninja", "rules.ninja", "Makefile", "build.make", "flags.make",
    "link.txt", "compile_commands.json", "CMakeCache.txt",
}
PATTERNS = {
    "*Config.cmake": ("*Config.cmake", "*-config.cmake"),
    "*ConfigVersion.cmake": ("*ConfigVersion.cmake", "*-config-version.cmake"),
    "*Targets*.cmake": ("*Targets*.cmake", "*-targets*.cmake"),
    "*.pc": ("*.pc",),
}
RULES = {
    "cache.layout": "Compare sorted cache entries. Ignore comments and blank lines.",
    "cache.tool_path": "The cache records the executable path of the selected CMake tool.",
    "cache.edit_command": "One installation can have a cache editor when the other has none.",
    "conan.home_path": "Replace each Conan home path and the 13-digit hexadecimal suffix of build folders under p/b. Keep the package prefix and remaining path.",
    "ninja.canonical_graph": "Compare Ninja build edges with rule variables expanded.",
    "json.layout": "JSON object key order and whitespace do not change the JSON value.",
    "file_api.index_name": "The File API puts the time in the index file name.",
    "file_api.reply_hash": "The File API puts a content hash in reply file names and references.",
    "file_api.tool_path": "The File API records the executable and share paths of the selected CMake installation.",
    "compile_commands.order": "Compilation database entries are independent translation units whose array order is not significant.",
}
CACHE_TOOLS = {"CMAKE_COMMAND": "cmake", "CMAKE_CTEST_COMMAND": "ctest", "CMAKE_CPACK_COMMAND": "cpack"}
CACHE_ENTRY = re.compile(r"(.+?):([^:=]+)=(.*)")
EDIT_COMMAND = "CMAKE_EDIT_COMMAND"
CONAN_BUILD_HASH = re.compile(r"(<conan-home>/p/b/[a-z0-9_+.-]+)[0-9a-f]{13}(?=/)")
IDE_FILE_API_QUERIES = ("codemodel-v2", "cache-v2", "toolchains-v1", "cmakeFiles-v1")
REPLY_HASH = re.compile(
    r"(codemodel|cache|cmakeFiles|toolchains|configureLog|target|directory)"
    r"(-.+)-[0-9a-f]{20}(\.json)"
)
MAX_DIFF_BYTES = 256 * 1024
MAX_NORMALIZE_BYTES = 4 * 1024 * 1024
ATTACHED_PATH_FLAGS = ("-I", "-isystem", "-iquote", "--sysroot=", "-F")
SEPARATE_PATH_FLAGS = frozenset({"-I", "-isystem", "-iquote", "-F", "-o", "-c"})
JOINED_VALUE_FLAGS = frozenset({"-I", "-D", "-isystem", "-iquote", "-F"})


@dataclass
class File:
    """Store the path and class of one generated file."""

    path: Path
    relative: Path
    file_class: str


def inventory(tree: Path, configured: set[Path]) -> dict[Path, File]:
    """Find known generated files and explicitly selected configure_file outputs."""
    result = {}
    if not tree.is_dir():
        return result
    for directory, dirs, names in os.walk(tree):
        dirs[:] = [name for name in dirs if name not in {".git", "__pycache__"}]
        for name in names:
            path = Path(directory) / name
            if name.startswith("._") or not (path.is_symlink() or path.is_file()):
                continue
            relative = path.relative_to(tree)
            file_class = name if name in EXACT_NAMES else None
            if path.suffix == ".json" and relative.parts[-5:-1] == (".cmake", "api", "v1", "reply"):
                file_class = "File API reply"
            if file_class is None:
                file_class = next((
                    label for label, patterns in PATTERNS.items()
                    if any(relative.match(pattern) for pattern in patterns)
                ), None)
            if file_class is None and relative in configured:
                file_class = "configure_file"
            if file_class is not None:
                result[relative] = File(path, relative, file_class)
    return result


def reply_key(file: File) -> tuple[Path, str | None]:
    """Identify the stable part of a File API reply name."""
    name = file.relative.name
    if file.file_class == "File API reply":
        if name.startswith("index-"):
            return file.relative.with_name("index.json"), "file_api.index_name"
        if match := REPLY_HASH.fullmatch(name):
            return file.relative.with_name("".join(match.groups())), "file_api.reply_hash"
    return file.relative, None


def align(left: dict, right: dict) -> tuple[list, tuple[dict, dict]]:
    """Pair replies by stable names if each side has at most one candidate."""
    groups = {}
    for side, files in enumerate((left, right)):
        for file in files.values():
            key, rule = reply_key(file)
            groups.setdefault(key, ([], []))[side].append((file, rule))

    pairs = []
    references = ({}, {})
    for key, (oracle, rcmake) in sorted(groups.items()):
        if len(oracle) != 1 or len(rcmake) != 1:
            first, second = ({file.relative: file for file, _ in side} for side in (oracle, rcmake))
            for relative in sorted(first.keys() | second.keys()):
                pairs.append((first.get(relative), second.get(relative), None))
            continue
        (before, rule), (after, _) = oracle[0], rcmake[0]
        if before.relative == after.relative:
            rule = None
        if rule == "file_api.reply_hash":
            for names, file in zip(references, (before, after)):
                names.setdefault(key.parent, {})[file.relative.name] = key.name
        pairs.append((before, after, rule))
    return pairs, references


def unique_keys(entries: list[tuple[str, object]]) -> dict:
    """Reject duplicate JSON object keys."""
    result = dict(entries)
    if len(result) != len(entries):
        raise ValueError("Duplicate JSON object key.")
    return result


def reject_noninteger_number(_: str) -> object:
    """Leave JSON numbers that can lose precision in their original form."""
    raise ValueError("Normalization requires integer JSON numbers.")


def rewrite_replies(value: object, names: dict[str, str]) -> None:
    if isinstance(value, dict):
        name = value.get("jsonFile")
        if isinstance(name, str) and name in names:
            value["jsonFile"] = names[name]
        for entry in value.values():
            rewrite_replies(entry, names)
    elif isinstance(value, list):
        for entry in value:
            rewrite_replies(entry, names)


def _posix_str(value: str | Path) -> str:
    return str(value).replace("\\", "/")


def _root_variants(root: Path | None) -> tuple[str, ...]:
    if root is None:
        return ()
    variants: list[str] = []
    for candidate in (root, root.resolve()):
        text = _posix_str(candidate).rstrip("/")
        if text and text not in variants:
            variants.append(text)
    return tuple(sorted(variants, key=len, reverse=True))


def _cmake_root_variants(cmake_root: str | None) -> tuple[str, ...]:
    if not cmake_root:
        return ()
    raw = _posix_str(cmake_root).rstrip("/")
    variants = [raw]
    resolved = _posix_str(Path(cmake_root).resolve()).rstrip("/")
    if resolved and resolved not in variants:
        variants.append(resolved)
    return tuple(sorted(variants, key=len, reverse=True))


def _replace_prefix(text: str, prefixes: tuple[str, ...], placeholder: str) -> str:
    for prefix in prefixes:
        if text == prefix:
            return placeholder
        if text.startswith(prefix + "/"):
            return placeholder + text[len(prefix):]
    return text


def _normalize_path(value: str, *, root: Path | None = None, cmake_root: str | None = None) -> str:
    raw = _posix_str(value)
    if not raw:
        return raw
    normalized = posixpath.normpath(raw)
    if raw.startswith("//") and not normalized.startswith("//"):
        normalized = "/" + normalized
    replaced = _replace_prefix(normalized, _root_variants(root), "<project>")
    if replaced == normalized:
        replaced = _replace_prefix(normalized, _cmake_root_variants(cmake_root), "<cmake_root>")
    return replaced


def _tool_lookup(tools: dict[str, set[str]] | None) -> dict[str, str]:
    lookup: dict[str, str] = {}
    if not tools:
        return lookup
    for name, paths in tools.items():
        for candidate in paths:
            lookup[_posix_str(candidate)] = f"<{name}>"
    return lookup


@lru_cache(maxsize=64)
def _conan_home_pattern(home: str) -> re.Pattern[str]:
    paths = "|".join(map(re.escape, _root_variants(Path(home))))
    return re.compile(
        rf"(?:(?<![\w./-])|(?<=-I)|(?<=-L)|(?<=-F)|(?<=-isystem)|(?<=-iquote))"
        rf"(?:{paths})(?=$|[/\\\s'\"\x60:;,|)&\]])"
    )


def conan_paths(value: str, home: str | None) -> str:
    """Replace the selected Conan home and its per-build folder suffixes."""
    if not home:
        return value
    value = _conan_home_pattern(home).sub("<conan-home>", value)
    return CONAN_BUILD_HASH.sub(r"\1<conan-hash>", value)


def _compile_conan_paths(value: object, home: str | None) -> object:
    if not isinstance(value, list) or not home:
        return value
    entries = []
    for entry in value:
        if isinstance(entry, dict):
            entry = entry.copy()
            for key in ("command", "directory", "file", "output"):
                if isinstance(entry.get(key), str):
                    entry[key] = conan_paths(entry[key], home)
            if isinstance(entry.get("arguments"), list):
                entry["arguments"] = [conan_paths(arg, home) if isinstance(arg, str) else arg
                                      for arg in entry["arguments"]]
        entries.append(entry)
    return entries


class BuildNormalizer(Normalizer):
    """Normalize tool, project, and Conan paths in Ninja commands and edges."""

    def __init__(self, build, project, tools, *, cmake_roots=(), conan_home=None,
                 recorded_build=None):
        replacements = {project: "<project>", build: "<build>"}
        if recorded_build is not None:
            replacements[recorded_build] = "<build>"
        replacements.update({Path(root): "<cmake_root>" for root in cmake_roots if root})
        super().__init__(build, replacements=replacements)
        self.recorded_build = recorded_build or self.build
        self.conan_home = conan_home
        tool_labels = _tool_lookup(tools)
        self.labels.update(tool_labels)
        self.labels.update({shlex.quote(path): label for path, label in tool_labels.items()})
        alternatives = "|".join(map(re.escape, sorted(self.labels, key=len, reverse=True)))
        self.pattern = re.compile(rf"(?:{alternatives})(?=$|[/\\\s'\":;,|)&\]])")

    def text(self, value: str) -> str:
        return super().text(conan_paths(value, self.conan_home))

    def path(self, value: str) -> str:
        return self.text(Path(os.path.normpath(self.recorded_build / value)).as_posix()) if value else ""


def _cache_lines(data: bytes) -> list[str]:
    return [line for line in os.fsdecode(data).split("\n")
            if line.strip() and not line.lstrip().startswith(("#", "//"))]


def _read_cache(build: Path) -> bytes:
    path = build / "CMakeCache.txt"
    if not path.is_file() or path.is_symlink() or path.stat().st_size > MAX_NORMALIZE_BYTES:
        return b""
    return path.read_bytes()


def _internal_cache(data: bytes) -> dict[str, str]:
    return {match[1]: match[3].removesuffix("\r")
            for line in _cache_lines(data) if (match := CACHE_ENTRY.fullmatch(line))
            and match[2] == "INTERNAL"}


def build_cmake_roots(tree: Path) -> set[str]:
    """Collect CMake installation roots recorded in build caches."""
    return {root for path in tree.rglob("CMakeCache.txt")
            if (root := _internal_cache(_read_cache(path.parent)).get("CMAKE_ROOT")) and Path(root).is_absolute()}


def _has_cache_editor(command: str) -> bool:
    path = Path(command)
    return path.is_absolute() and path.name.lower().removesuffix(".exe") in {"ccmake", "cmake-gui"}


def _normalize_cache(data: bytes, tools: dict, conan_home: str | None,
                     omit_editor: bool) -> tuple[bytes, set[str]]:
    lines = []
    rules = {"cache.layout"}
    for line in _cache_lines(data):
        line = line.removesuffix("\r")
        key = line
        if match := CACHE_ENTRY.fullmatch(line):
            key, kind, value = match.groups()
            if omit_editor and key == EDIT_COMMAND and kind == "INTERNAL":
                rules.add("cache.edit_command")
                continue
            tool = CACHE_TOOLS.get(key)
            if (tool and kind in {"INTERNAL", "FILEPATH"} and value in tools.get(tool, ())) or (
                key == "CMAKE_ROOT" and kind == "INTERNAL" and Path(value).is_absolute()
            ):
                value = f"<{tool or 'cmake_root'}>"
                rules.add("cache.tool_path")
            replaced = conan_paths(value, conan_home)
            if replaced != value:
                rules.add("conan.home_path")
            line = f"{key}:{kind}={replaced}"
        lines.append((key, line))
    # Preserve the order of repeated keys because CMake uses the last value.
    return os.fsencode("\n".join(line for _, line in sorted(lines, key=lambda entry: entry[0]))), rules


def _optional_cache_editor(before: bytes, after: bytes) -> tuple[bool, bool]:
    editors = [
        [match.groups()[1:] for line in _cache_lines(data)
         if (match := CACHE_ENTRY.fullmatch(line.removesuffix("\r")))
         and match[1].strip('"') == EDIT_COMMAND]
        for data in (before, after)
    ]
    return tuple(len(entries) == 1 and entries[0][0] == "INTERNAL"
                 and _has_cache_editor(entries[0][1]) and not other
                 for entries, other in (editors, editors[::-1]))


def _rewrite_file_api_tools(
    value: object,
    tools: dict[str, set[str]],
    cmake_root: str | None,
) -> bool:
    if not isinstance(value, dict):
        return False
    changed = False
    lookup = _tool_lookup(tools)
    root_variants = _cmake_root_variants(cmake_root)

    cmake_info = value.get("cmake")
    if isinstance(cmake_info, dict) and isinstance(cmake_info.get("paths"), dict):
        paths = cmake_info["paths"]
        for key in ("cmake", "ctest", "cpack"):
            current = paths.get(key)
            if isinstance(current, str) and _posix_str(current) in lookup:
                paths[key] = lookup[_posix_str(current)]
                changed = True
        current_root = paths.get("root")
        if isinstance(current_root, str) and root_variants:
            replaced = _replace_prefix(_posix_str(current_root).rstrip("/"), root_variants, "<cmake_root>")
            if replaced != current_root:
                paths["root"] = replaced
                changed = True

    if value.get("kind") == "cache" and isinstance(value.get("entries"), list):
        for entry in value["entries"]:
            if not isinstance(entry, dict):
                continue
            name = entry.get("name")
            current = entry.get("value")
            if not isinstance(current, str):
                continue
            tool = CACHE_TOOLS.get(name) if isinstance(name, str) else None
            if tool and _posix_str(current) in lookup:
                entry["value"] = f"<{tool}>"
                changed = True
            elif name == "CMAKE_ROOT" and root_variants:
                replaced = _replace_prefix(_posix_str(current).rstrip("/"), root_variants, "<cmake_root>")
                if replaced != current:
                    entry["value"] = replaced
                    changed = True

    if value.get("kind") == "cmakeFiles" and isinstance(value.get("inputs"), list) and root_variants:
        for item in value["inputs"]:
            if not isinstance(item, dict) or not item.get("isCMake"):
                continue
            current = item.get("path")
            if isinstance(current, str):
                replaced = _replace_prefix(_posix_str(current), root_variants, "<cmake_root>")
                if replaced != current:
                    item["path"] = replaced
                    changed = True

    backtrace = value.get("backtraceGraph")
    if isinstance(backtrace, dict) and isinstance(backtrace.get("files"), list) and root_variants:
        updated_files = []
        for item in backtrace["files"]:
            if isinstance(item, str):
                replaced = _replace_prefix(_posix_str(item), root_variants, "<cmake_root>")
                if replaced != item:
                    changed = True
                updated_files.append(replaced)
            else:
                updated_files.append(item)
        backtrace["files"] = updated_files

    return changed


def _compile_command_sort_key(entry: object) -> tuple[str, str, str, str]:
    if not isinstance(entry, dict):
        return ("", "", "", json.dumps(entry, sort_keys=True))
    command = entry.get("command")
    if command is None and isinstance(entry.get("arguments"), list):
        command = json.dumps(entry["arguments"])
    return (
        str(entry.get("file", "")),
        str(entry.get("directory", "")),
        str(entry.get("output", "")),
        str(command or ""),
    )


def normalize(
    file: File,
    data: bytes,
    tools: dict[str, set[str]],
    references: dict[Path, dict[str, str]],
    *,
    cmake_root: str | None = None,
    conan_home: str | None = None,
    omit_cache_editor: bool = False,
) -> tuple[bytes, set[str]]:
    """Apply only the rules for this file class."""
    rules = set()
    if file.file_class == "CMakeCache.txt":
        return _normalize_cache(data, tools, conan_home, omit_cache_editor)
    if file.file_class in {"File API reply", "compile_commands.json"}:
        try:
            value = json.loads(
                data, object_pairs_hook=unique_keys, parse_float=reject_noninteger_number,
            )
            raw = json.dumps(value, sort_keys=True, allow_nan=False).encode()
            rewritten = raw
            if file.file_class == "compile_commands.json" and isinstance(value, list):
                with_paths = _compile_conan_paths(value, conan_home)
                if with_paths != value:
                    value = with_paths
                    rules.add("conan.home_path")
                    rewritten = json.dumps(value, sort_keys=True, allow_nan=False).encode()
                ordered = sorted(value, key=_compile_command_sort_key)
                if ordered != value:
                    value = ordered
                    rules.add("compile_commands.order")
                    rewritten = json.dumps(value, sort_keys=True, allow_nan=False).encode()
            if file.file_class == "File API reply":
                if names := references.get(file.relative.parent):
                    before_reply = json.dumps(value, sort_keys=True, allow_nan=False).encode()
                    rewrite_replies(value, names)
                    after_reply = json.dumps(value, sort_keys=True, allow_nan=False).encode()
                    if after_reply != before_reply:
                        rules.add("file_api.reply_hash")
                        rewritten = after_reply
                if _rewrite_file_api_tools(value, tools, cmake_root):
                    rules.add("file_api.tool_path")
                    rewritten = json.dumps(value, sort_keys=True, allow_nan=False).encode()
        except (ValueError, RecursionError):
            return data, rules
        if raw != data:
            rules.add("json.layout")
        return rewritten, rules
    return data, rules


def _normalize_flag_token(
    token: str,
    *,
    root: Path | None = None,
    tools: dict[str, set[str]] | None = None,
    cmake_root: str | None = None,
) -> str:
    lookup = _tool_lookup(tools)
    posix_token = _posix_str(token)
    if posix_token in lookup:
        return lookup[posix_token]
    for prefix in ATTACHED_PATH_FLAGS:
        if token.startswith(prefix) and len(token) > len(prefix):
            return prefix + _normalize_path(token[len(prefix):], root=root, cmake_root=cmake_root)
    if root is not None or cmake_root is not None:
        replaced = _normalize_path(token, root=root, cmake_root=cmake_root)
        if replaced != posix_token and (replaced.startswith("<project>") or replaced.startswith("<cmake_root>")):
            return replaced
    return token


def _canonicalize_tokens(
    tokens: list[str],
    *,
    root: Path | None = None,
    tools: dict[str, set[str]] | None = None,
    cmake_root: str | None = None,
) -> list[str]:
    normalized: list[str] = []
    index = 0
    while index < len(tokens):
        token = tokens[index]
        if token in JOINED_VALUE_FLAGS and index + 1 < len(tokens):
            nxt = tokens[index + 1]
            if token == "-D":
                normalized.append(f"-D{nxt}")
            else:
                normalized.append(token + _normalize_path(nxt, root=root, cmake_root=cmake_root))
            index += 2
            continue
        if token in SEPARATE_PATH_FLAGS and index + 1 < len(tokens):
            normalized.append(token)
            normalized.append(_normalize_path(tokens[index + 1], root=root, cmake_root=cmake_root))
            index += 2
            continue
        normalized.append(_normalize_flag_token(token, root=root, tools=tools, cmake_root=cmake_root))
        index += 1
    return normalized


def normalize_command_flags(
    entry: dict[str, Any],
    *,
    root: Path | None = None,
    tools: dict[str, set[str]] | None = None,
    cmake_root: str | None = None,
) -> tuple[str, list[str]]:
    """Parse one compile_commands entry into a normalized compiler and flag list."""
    if not isinstance(entry, dict):
        raise ValueError("Each compile_commands entry must be a JSON object.")
    if "arguments" in entry:
        raw_args = entry["arguments"]
        if not isinstance(raw_args, list) or not raw_args or not all(isinstance(a, str) for a in raw_args):
            raise ValueError("compile_commands arguments must be a non-empty list of strings.")
        tokens = list(raw_args)
    elif isinstance(entry.get("command"), str) and entry["command"].strip():
        tokens = shlex.split(entry["command"], posix=True)
    else:
        raise ValueError("compile_commands entry needs a command string or arguments list.")
    if not tokens:
        raise ValueError("compile_commands entry produced an empty command.")
    canonical = _canonicalize_tokens(tokens, root=root, tools=tools, cmake_root=cmake_root)
    return canonical[0], canonical[1:]


def normalize_compile_commands(
    entries: list[dict[str, Any]],
    *,
    root: Path | None = None,
    tools: dict[str, set[str]] | None = None,
    cmake_root: str | None = None,
) -> list[dict[str, Any]]:
    """Normalize compile_commands.json entries for deterministic IDE parity diffing."""
    if not isinstance(entries, list):
        raise ValueError("compile_commands.json must contain a JSON array.")
    normalized = []
    for entry in entries:
        if not isinstance(entry, dict):
            raise ValueError("compile_commands.json entries must be JSON objects.")
        raw_file = entry.get("file")
        raw_dir = entry.get("directory")
        if not isinstance(raw_file, str) or not isinstance(raw_dir, str):
            raise ValueError("Each compile_commands entry requires string file and directory fields.")
        raw_output = entry.get("output")
        if raw_output is not None and not isinstance(raw_output, str):
            raise ValueError("compile_commands output must be a string when present.")
        compiler, flags = normalize_command_flags(entry, root=root, tools=tools, cmake_root=cmake_root)
        normalized.append({
            "file": _normalize_path(raw_file, root=root, cmake_root=cmake_root),
            "directory": _normalize_path(raw_dir, root=root, cmake_root=cmake_root),
            "output": _normalize_path(raw_output, root=root, cmake_root=cmake_root) if raw_output else None,
            "compiler": compiler,
            "flags": flags,
        })
    normalized.sort(key=lambda item: (
        item["file"], item["directory"], item["output"] or "", item["compiler"], tuple(item["flags"]),
    ))
    return normalized


def _counter_diff(left: list[str], right: list[str]) -> tuple[list[str], list[str]]:
    left_counts, right_counts = Counter(left), Counter(right)
    missing = sorted((left_counts - right_counts).elements())
    extra = sorted((right_counts - left_counts).elements())
    return missing, extra


def diff_compile_commands(
    oracle_entries: list[dict[str, Any]],
    rcmake_entries: list[dict[str, Any]],
    *,
    oracle_root: Path | None = None,
    rcmake_root: Path | None = None,
    oracle_tools: dict[str, set[str]] | None = None,
    rcmake_tools: dict[str, set[str]] | None = None,
    oracle_cmake_root: str | None = None,
    rcmake_cmake_root: str | None = None,
) -> dict[str, Any]:
    """Compare normalized compile_commands.json entries between oracle and rcmake."""
    left = normalize_compile_commands(
        oracle_entries, root=oracle_root, tools=oracle_tools, cmake_root=oracle_cmake_root,
    )
    right = normalize_compile_commands(
        rcmake_entries, root=rcmake_root, tools=rcmake_tools, cmake_root=rcmake_cmake_root,
    )

    left_by_key: dict[tuple[str, str | None], list[dict[str, Any]]] = {}
    right_by_key: dict[tuple[str, str | None], list[dict[str, Any]]] = {}
    for item in left:
        left_by_key.setdefault((item["file"], item["output"]), []).append(item)
    for item in right:
        right_by_key.setdefault((item["file"], item["output"]), []).append(item)

    missing_entries: list[dict[str, Any]] = []
    extra_entries: list[dict[str, Any]] = []
    entry_diffs: list[dict[str, Any]] = []

    for key in sorted(left_by_key.keys() | right_by_key.keys()):
        left_group = left_by_key.get(key, [])
        right_group = right_by_key.get(key, [])
        paired = min(len(left_group), len(right_group))
        for idx in range(paired):
            oracle_item, rcmake_item = left_group[idx], right_group[idx]
            diff: dict[str, Any] = {"file": oracle_item["file"], "output": oracle_item["output"]}
            if oracle_item["directory"] != rcmake_item["directory"]:
                diff["directory_mismatch"] = {
                    "oracle": oracle_item["directory"],
                    "rcmake": rcmake_item["directory"],
                }
            if oracle_item["compiler"] != rcmake_item["compiler"]:
                diff["compiler_mismatch"] = {
                    "oracle": oracle_item["compiler"],
                    "rcmake": rcmake_item["compiler"],
                }
            missing_flags, extra_flags = _counter_diff(oracle_item["flags"], rcmake_item["flags"])
            if missing_flags:
                diff["missing_flags"] = missing_flags
            if extra_flags:
                diff["extra_flags"] = extra_flags
            if not missing_flags and not extra_flags and oracle_item["flags"] != rcmake_item["flags"]:
                diff["flag_order_diff"] = {
                    "oracle": oracle_item["flags"],
                    "rcmake": rcmake_item["flags"],
                }
            if len(diff) > 2:
                entry_diffs.append(diff)
        for item in left_group[paired:]:
            missing_entries.append({
                "file": item["file"], "directory": item["directory"], "output": item["output"],
            })
        for item in right_group[paired:]:
            extra_entries.append({
                "file": item["file"], "directory": item["directory"], "output": item["output"],
            })

    equal = not (missing_entries or extra_entries or entry_diffs)
    return {
        "status": "pass" if equal else "fail",
        "oracle_count": len(left),
        "rcmake_count": len(right),
        "missing_entries": missing_entries,
        "extra_entries": extra_entries,
        "entry_diffs": entry_diffs,
    }


def load_file_api_index(reply_dir: Path) -> dict[str, Any] | None:
    """Load the latest File API index-*.json file in a reply directory."""
    if not reply_dir.is_dir():
        return None
    indices = sorted(reply_dir.glob("index-*.json"))
    if not indices:
        return None
    payload = json.loads(indices[-1].read_text(encoding="utf-8"))
    if not isinstance(payload, dict):
        raise ValueError(f"Invalid File API index payload in {indices[-1]}")
    return payload


def _extract_cmake_root(index_payload: dict[str, Any] | None) -> str | None:
    if not isinstance(index_payload, dict):
        return None
    cmake = index_payload.get("cmake")
    if isinstance(cmake, dict) and isinstance(cmake.get("paths"), dict):
        root = cmake["paths"].get("root")
        if isinstance(root, str) and root:
            return root
    return None


def _normalize_target_data(
    target: dict[str, Any],
    id_to_name: dict[str, str],
    *,
    root: Path | None = None,
    cmake_root: str | None = None,
) -> dict[str, Any]:
    raw_sources = target.get("sources") if isinstance(target.get("sources"), list) else []
    source_groups = target.get("sourceGroups") if isinstance(target.get("sourceGroups"), list) else []

    normalized_sources = []
    source_paths: list[str] = []
    for src in raw_sources:
        if not isinstance(src, dict) or not isinstance(src.get("path"), str):
            source_paths.append("")
            continue
        path = _normalize_path(src["path"], root=root, cmake_root=cmake_root)
        source_paths.append(path)
        group_name = None
        group_idx = src.get("sourceGroupIndex")
        if isinstance(group_idx, int) and 0 <= group_idx < len(source_groups):
            group = source_groups[group_idx]
            if isinstance(group, dict) and isinstance(group.get("name"), str):
                group_name = group["name"]
        normalized_sources.append({
            "path": path,
            "is_generated": bool(src.get("isGenerated", False)),
            "compiled": isinstance(src.get("compileGroupIndex"), int),
            "source_group": group_name,
        })
    normalized_sources.sort(key=lambda item: (
        item["path"], item["is_generated"], item["compiled"], item["source_group"] or "",
    ))

    compile_groups = []
    raw_groups = target.get("compileGroups") if isinstance(target.get("compileGroups"), list) else []
    for group in raw_groups:
        if not isinstance(group, dict):
            continue
        raw_fragments = group.get("compileCommandFragments")
        tokens: list[str] = []
        if isinstance(raw_fragments, list):
            for frag in raw_fragments:
                if isinstance(frag, dict) and isinstance(frag.get("fragment"), str):
                    tokens.extend(shlex.split(frag["fragment"], posix=True))
        flags = _canonicalize_tokens(tokens, root=root, cmake_root=cmake_root)
        defines = sorted(
            item["define"]
            for item in (group.get("defines") or [])
            if isinstance(item, dict) and isinstance(item.get("define"), str)
        )
        includes = [
            {
                "path": _normalize_path(item["path"], root=root, cmake_root=cmake_root),
                "is_system": bool(item.get("isSystem", False)),
            }
            for item in (group.get("includes") or [])
            if isinstance(item, dict) and isinstance(item.get("path"), str)
        ]
        frameworks = [
            {
                "path": _normalize_path(item["path"], root=root, cmake_root=cmake_root),
                "is_system": bool(item.get("isSystem", False)),
            }
            for item in (group.get("frameworks") or [])
            if isinstance(item, dict) and isinstance(item.get("path"), str)
        ]
        group_sources = sorted(
            source_paths[idx]
            for idx in (group.get("sourceIndexes") or [])
            if isinstance(idx, int) and 0 <= idx < len(source_paths) and source_paths[idx]
        )
        standard = None
        if isinstance(group.get("languageStandard"), dict):
            standard = group["languageStandard"].get("standard")
        sysroot = None
        if isinstance(group.get("sysroot"), dict) and isinstance(group["sysroot"].get("path"), str):
            sysroot = _normalize_path(group["sysroot"]["path"], root=root, cmake_root=cmake_root)
        compile_groups.append({
            "language": group.get("language"),
            "language_standard": standard,
            "flags": flags,
            "defines": defines,
            "includes": includes,
            "frameworks": frameworks,
            "sysroot": sysroot,
            "sources": group_sources,
        })
    compile_groups.sort(key=lambda item: (
        str(item["language"] or ""),
        tuple(item["sources"]),
        tuple(item["defines"]),
        tuple(item["flags"]),
    ))

    artifacts = sorted(
        _normalize_path(item["path"], root=root, cmake_root=cmake_root)
        for item in (target.get("artifacts") or [])
        if isinstance(item, dict) and isinstance(item.get("path"), str)
    )
    dep_list = target.get("dependencies") or target.get("compileDependencies") or []
    dependencies = sorted({
        id_to_name.get(item["id"], item["id"].split("::@", 1)[0])
        for item in dep_list
        if isinstance(item, dict) and isinstance(item.get("id"), str)
    })
    raw_paths = target.get("paths") if isinstance(target.get("paths"), dict) else {}
    return {
        "name": target.get("name"),
        "type": target.get("type"),
        "paths": {
            "source": _normalize_path(str(raw_paths.get("source", "")), root=root, cmake_root=cmake_root),
            "build": _normalize_path(str(raw_paths.get("build", "")), root=root, cmake_root=cmake_root),
        },
        "artifacts": artifacts,
        "dependencies": dependencies,
        "compile_groups": compile_groups,
        "sources": normalized_sources,
    }


def _remap_abstract_target_ref(
    ref: Any,
    old_to_new: dict[int, int],
    old_ids: dict[int, Any],
) -> Any:
    if not isinstance(ref, dict):
        return ref
    idx = ref.get("index")
    target_id = ref.get("id")
    if (
        isinstance(idx, int)
        and not isinstance(idx, bool)
        and target_id is not None
        and old_ids.get(idx) == target_id
        and idx in old_to_new
        and old_to_new[idx] != idx
    ):
        return {**ref, "index": old_to_new[idx]}
    return ref


def _remap_abstract_target_indexes(
    indexes: list[Any],
    old_to_new: dict[int, int],
) -> list[Any]:
    return sorted(
        old_to_new.get(idx, idx)
        if isinstance(idx, int) and not isinstance(idx, bool)
        else idx
        for idx in indexes
    )


def _sort_dependency_list(deps: list[Any]) -> tuple[list[Any], bool]:
    """Symmetrically sort codemodel target dependencies[] by target id.

    Upstream CMake orders cmTargetDependSet by raw C++ pointer address
    (cmTargetDepend.h:34-37), so dependency array order is non-deterministic
    across runs and processes.
    """
    if len(deps) < 2:
        return deps, False
    ordered = sorted(
        deps,
        key=lambda dep: dep.get("id", "") if isinstance(dep, dict) else "",
    )
    if ordered == deps:
        return deps, False
    return ordered, True


def _sort_target_entry_dependencies(entry: Any) -> tuple[Any, bool]:
    if not isinstance(entry, dict):
        return entry, False
    entry_changed = False
    updated = entry
    if isinstance(entry.get("dependencies"), list):
        ordered, did_sort = _sort_dependency_list(entry["dependencies"])
        if did_sort:
            updated = {**updated, "dependencies": ordered}
            entry_changed = True
    child = entry.get("jsonFile")
    if isinstance(child, dict) and isinstance(child.get("dependencies"), list):
        ordered, did_sort = _sort_dependency_list(child["dependencies"])
        if did_sort:
            if updated is entry:
                updated = dict(entry)
            updated["jsonFile"] = {**child, "dependencies": ordered}
            entry_changed = True
    return updated, entry_changed


def normalize_codemodel_abstract_target_ties(
    codemodel: Any,
    *,
    dir_key: str = "directories",
    dir_data_key: str = "data",
) -> Any:
    """Canonicalize equal-name abstractTargets ties and pointer-ordered dependencies.

    Remaps matching abstractTargetIndexes and directory installer target
    references without changing the order of targets with distinct names, and
    symmetrically sorts each target's ``dependencies`` array by ``id``.
    """
    if not isinstance(codemodel, dict) or not isinstance(codemodel.get("configurations"), list):
        return codemodel
    configs = []
    changed = False
    for config in codemodel["configurations"]:
        if not isinstance(config, dict):
            configs.append(config)
            continue
        new_config = config
        config_changed = False
        abstract = config.get("abstractTargets")
        if isinstance(abstract, list):
            ordered = []
            i = 0
            while i < len(abstract):
                name = abstract[i].get("name", "") if isinstance(abstract[i], dict) else ""
                j = i + 1
                while j < len(abstract) and (
                    abstract[j].get("name", "") if isinstance(abstract[j], dict) else ""
                ) == name:
                    j += 1
                run = list(enumerate(abstract[i:j], start=i))
                if j - i > 1:
                    run.sort(key=lambda pair: pair[1].get("id", "") if isinstance(pair[1], dict) else "")
                ordered.extend(run)
                i = j
            old_to_new = {old: new for new, (old, _) in enumerate(ordered)}
            if any(old != new for old, new in old_to_new.items()):
                config_changed = True
                old_ids = {
                    old: item.get("id")
                    for old, item in enumerate(abstract)
                    if isinstance(item, dict)
                }
                new_config = {**new_config, "abstractTargets": [item for _, item in ordered]}
                if isinstance(config.get("projects"), list):
                    new_config["projects"] = [
                        {
                            **proj,
                            "abstractTargetIndexes": _remap_abstract_target_indexes(
                                proj["abstractTargetIndexes"], old_to_new
                            ),
                        }
                        if isinstance(proj, dict) and isinstance(proj.get("abstractTargetIndexes"), list)
                        else proj
                        for proj in config["projects"]
                    ]
                if isinstance(config.get(dir_key), list):
                    new_dirs = []
                    for directory in config[dir_key]:
                        if not isinstance(directory, dict):
                            new_dirs.append(directory)
                            continue
                        new_dir = dict(directory)
                        if isinstance(directory.get("abstractTargetIndexes"), list):
                            new_dir["abstractTargetIndexes"] = _remap_abstract_target_indexes(
                                directory["abstractTargetIndexes"], old_to_new
                            )
                        child = directory.get(dir_data_key)
                        if isinstance(child, dict) and isinstance(child.get("installers"), list):
                            new_installers = []
                            for inst in child["installers"]:
                                if not isinstance(inst, dict):
                                    new_installers.append(inst)
                                    continue
                                updated = dict(inst)
                                for key in ("fileSetTarget", "cxxModuleBmiTarget"):
                                    if key in updated:
                                        updated[key] = _remap_abstract_target_ref(
                                            updated[key], old_to_new, old_ids
                                        )
                                if isinstance(updated.get("exportTargets"), list):
                                    updated["exportTargets"] = [
                                        _remap_abstract_target_ref(item, old_to_new, old_ids)
                                        for item in updated["exportTargets"]
                                    ]
                                new_installers.append(updated)
                            new_dir[dir_data_key] = {**child, "installers": new_installers}
                        new_dirs.append(new_dir)
                    new_config[dir_key] = new_dirs
        for list_key in ("targets", "abstractTargets"):
            entries = new_config.get(list_key)
            if not isinstance(entries, list):
                continue
            updated_entries = []
            list_changed = False
            for entry in entries:
                updated_entry, entry_changed = _sort_target_entry_dependencies(entry)
                updated_entries.append(updated_entry)
                if entry_changed:
                    list_changed = True
            if list_changed:
                new_config = {**new_config, list_key: updated_entries}
                config_changed = True
        if config_changed:
            changed = True
        configs.append(new_config)
    return {**codemodel, "configurations": configs} if changed else codemodel


def normalize_codemodel_data(
    codemodel: dict[str, Any],
    targets: dict[str, dict[str, Any]],
    *,
    root: Path | None = None,
    cmake_root: str | None = None,
) -> dict[str, Any]:
    """Normalize an in-memory codemodel-v2 document and its target payloads."""
    raw_paths = codemodel.get("paths") if isinstance(codemodel.get("paths"), dict) else {}
    configurations: dict[str, dict[str, Any]] = {}
    for cfg in codemodel.get("configurations") or []:
        if not isinstance(cfg, dict):
            continue
        cfg_name = str(cfg.get("name", ""))
        raw_targets = [t for t in (cfg.get("targets") or []) if isinstance(t, dict)]
        id_to_name = {
            t["id"]: t["name"]
            for t in raw_targets
            if isinstance(t.get("id"), str) and isinstance(t.get("name"), str)
        }
        normalized_targets: dict[str, dict[str, Any]] = {}
        for ref in raw_targets:
            json_file = ref.get("jsonFile")
            name = ref.get("name")
            if not isinstance(name, str):
                continue
            payload = targets.get(json_file) if isinstance(json_file, str) else None
            if payload is None:
                payload = targets.get(name, {"name": name})
            normalized_targets[name] = _normalize_target_data(
                payload, id_to_name, root=root, cmake_root=cmake_root,
            )
        projects = sorted(
            p["name"] for p in (cfg.get("projects") or [])
            if isinstance(p, dict) and isinstance(p.get("name"), str)
        )
        configurations[cfg_name] = {
            "name": cfg_name,
            "projects": projects,
            "targets": dict(sorted(normalized_targets.items())),
        }
    return {
        "kind": codemodel.get("kind"),
        "version": codemodel.get("version"),
        "paths": {
            "source": _normalize_path(str(raw_paths.get("source", "")), root=root, cmake_root=cmake_root),
            "build": _normalize_path(str(raw_paths.get("build", "")), root=root, cmake_root=cmake_root),
        },
        "configurations": dict(sorted(configurations.items())),
    }


def normalize_codemodel(
    reply_dir: Path,
    codemodel_file: str,
    *,
    root: Path | None = None,
    cmake_root: str | None = None,
) -> dict[str, Any]:
    """Load and normalize codemodel-v2 and every referenced target file."""
    codemodel = json.loads((reply_dir / codemodel_file).read_text(encoding="utf-8"))
    targets: dict[str, dict[str, Any]] = {}
    for cfg in codemodel.get("configurations") or []:
        if not isinstance(cfg, dict):
            continue
        for ref in cfg.get("targets") or []:
            if isinstance(ref, dict) and isinstance(ref.get("jsonFile"), str):
                target_path = reply_dir / ref["jsonFile"]
                if target_path.is_file():
                    targets[ref["jsonFile"]] = json.loads(target_path.read_text(encoding="utf-8"))
    return normalize_codemodel_data(codemodel, targets, root=root, cmake_root=cmake_root)


def _diff_target(oracle_target: dict[str, Any], rcmake_target: dict[str, Any]) -> dict[str, Any]:
    diff: dict[str, Any] = {}
    for field in ("type", "paths", "artifacts", "dependencies"):
        if oracle_target.get(field) != rcmake_target.get(field):
            diff[field] = {"oracle": oracle_target.get(field), "rcmake": rcmake_target.get(field)}

    left_sources = oracle_target.get("sources", [])
    right_sources = rcmake_target.get("sources", [])
    if left_sources != right_sources:
        left_by_path = {s["path"]: s for s in left_sources}
        right_by_path = {s["path"]: s for s in right_sources}
        missing_sources = sorted(left_by_path.keys() - right_by_path.keys())
        extra_sources = sorted(right_by_path.keys() - left_by_path.keys())
        changed_sources = [
            {"path": p, "oracle": left_by_path[p], "rcmake": right_by_path[p]}
            for p in sorted(left_by_path.keys() & right_by_path.keys())
            if left_by_path[p] != right_by_path[p]
        ]
        diff["sources"] = {
            "missing_paths": missing_sources,
            "extra_paths": extra_sources,
            "changed": changed_sources,
        }

    left_groups = oracle_target.get("compile_groups", [])
    right_groups = rcmake_target.get("compile_groups", [])
    if left_groups != right_groups:
        group_diffs = []
        paired = min(len(left_groups), len(right_groups))
        for idx in range(paired):
            lg, rg = left_groups[idx], right_groups[idx]
            if lg == rg:
                continue
            item: dict[str, Any] = {"index": idx, "language": lg.get("language")}
            if lg.get("language") != rg.get("language"):
                item["language_mismatch"] = {"oracle": lg.get("language"), "rcmake": rg.get("language")}
            if lg.get("language_standard") != rg.get("language_standard"):
                item["language_standard_mismatch"] = {
                    "oracle": lg.get("language_standard"),
                    "rcmake": rg.get("language_standard"),
                }
            missing_defs, extra_defs = _counter_diff(lg.get("defines", []), rg.get("defines", []))
            if missing_defs or extra_defs:
                item["defines"] = {"missing": missing_defs, "extra": extra_defs}
            if lg.get("includes") != rg.get("includes"):
                item["includes"] = {"oracle": lg.get("includes"), "rcmake": rg.get("includes")}
            missing_flags, extra_flags = _counter_diff(lg.get("flags", []), rg.get("flags", []))
            if missing_flags or extra_flags or lg.get("flags") != rg.get("flags"):
                item["flags"] = {
                    "missing": missing_flags,
                    "extra": extra_flags,
                    "oracle": lg.get("flags"),
                    "rcmake": rg.get("flags"),
                }
            if lg.get("frameworks") != rg.get("frameworks"):
                item["frameworks"] = {"oracle": lg.get("frameworks"), "rcmake": rg.get("frameworks")}
            if lg.get("sysroot") != rg.get("sysroot"):
                item["sysroot"] = {"oracle": lg.get("sysroot"), "rcmake": rg.get("sysroot")}
            if lg.get("sources") != rg.get("sources"):
                item["sources"] = {
                    "missing": sorted(set(lg.get("sources", [])) - set(rg.get("sources", []))),
                    "extra": sorted(set(rg.get("sources", [])) - set(lg.get("sources", []))),
                }
            group_diffs.append(item)
        diff["compile_groups"] = {
            "oracle_count": len(left_groups),
            "rcmake_count": len(right_groups),
            "diffs": group_diffs,
        }
    return diff


def diff_codemodel(oracle_model: dict[str, Any], rcmake_model: dict[str, Any]) -> dict[str, Any]:
    """Compare two normalized File API codemodel-v2 trees."""
    version_match = oracle_model.get("version") == rcmake_model.get("version")
    paths_match = oracle_model.get("paths") == rcmake_model.get("paths")
    left_cfgs = oracle_model.get("configurations", {})
    right_cfgs = rcmake_model.get("configurations", {})
    missing_configs = sorted(left_cfgs.keys() - right_cfgs.keys())
    extra_configs = sorted(right_cfgs.keys() - left_cfgs.keys())
    config_diffs: dict[str, dict[str, Any]] = {}

    for cfg_name in sorted(left_cfgs.keys() & right_cfgs.keys()):
        lc, rc = left_cfgs[cfg_name], right_cfgs[cfg_name]
        lt, rt = lc.get("targets", {}), rc.get("targets", {})
        missing_targets = sorted(lt.keys() - rt.keys())
        extra_targets = sorted(rt.keys() - lt.keys())
        target_diffs = {}
        for target_name in sorted(lt.keys() & rt.keys()):
            tdiff = _diff_target(lt[target_name], rt[target_name])
            if tdiff:
                target_diffs[target_name] = tdiff
        projects_mismatch = None
        if lc.get("projects") != rc.get("projects"):
            projects_mismatch = {"oracle": lc.get("projects"), "rcmake": rc.get("projects")}
        if missing_targets or extra_targets or target_diffs or projects_mismatch:
            config_diffs[cfg_name] = {
                "missing_targets": missing_targets,
                "extra_targets": extra_targets,
                "target_diffs": target_diffs,
                "projects_mismatch": projects_mismatch,
            }

    equal = (
        version_match
        and paths_match
        and not missing_configs
        and not extra_configs
        and not config_diffs
    )
    return {
        "status": "pass" if equal else "fail",
        "oracle_version": oracle_model.get("version"),
        "rcmake_version": rcmake_model.get("version"),
        "version_match": version_match,
        "paths_match": paths_match,
        "configurations": sorted(left_cfgs.keys() | right_cfgs.keys()),
        "missing_configurations": missing_configs,
        "extra_configurations": extra_configs,
        "configuration_diffs": config_diffs,
    }


def normalize_cache_v2(
    payload: dict[str, Any],
    *,
    root: Path | None = None,
    tools: dict[str, set[str]] | None = None,
    cmake_root: str | None = None,
) -> dict[str, dict[str, str]]:
    """Normalize File API cache-v2 entries by name, type, and normalized value."""
    lookup = _tool_lookup(tools)
    entries: dict[str, dict[str, str]] = {}
    for item in payload.get("entries") or []:
        if not isinstance(item, dict) or not isinstance(item.get("name"), str):
            continue
        name = item["name"]
        raw_val = str(item.get("value", ""))
        tool = CACHE_TOOLS.get(name)
        if tool and _posix_str(raw_val) in lookup:
            value = f"<{tool}>"
        else:
            value = _normalize_path(raw_val, root=root, cmake_root=cmake_root)
        entries[name] = {"type": str(item.get("type", "")), "value": value}
    return dict(sorted(entries.items()))


def diff_cache_v2(
    oracle_cache: dict[str, dict[str, str]],
    rcmake_cache: dict[str, dict[str, str]],
) -> dict[str, Any]:
    """Compare normalized File API cache-v2 maps."""
    missing = sorted(oracle_cache.keys() - rcmake_cache.keys())
    extra = sorted(rcmake_cache.keys() - oracle_cache.keys())
    mismatches = [
        {"name": name, "oracle": oracle_cache[name], "rcmake": rcmake_cache[name]}
        for name in sorted(oracle_cache.keys() & rcmake_cache.keys())
        if oracle_cache[name] != rcmake_cache[name]
    ]
    equal = not (missing or extra or mismatches)
    return {
        "status": "pass" if equal else "fail",
        "oracle_count": len(oracle_cache),
        "rcmake_count": len(rcmake_cache),
        "missing_entries": missing,
        "extra_entries": extra,
        "mismatches": mismatches,
    }


def normalize_toolchains_v1(
    payload: dict[str, Any],
    *,
    root: Path | None = None,
    cmake_root: str | None = None,
) -> dict[str, dict[str, Any]]:
    """Normalize File API toolchains-v1 entries keyed by language."""
    result: dict[str, dict[str, Any]] = {}
    def _norm_dirs(items: object) -> list[str]:
        return [
            _normalize_path(str(p), root=root, cmake_root=cmake_root)
            for p in (items or [])
            if isinstance(p, str)
        ]

    for entry in payload.get("toolchains") or []:
        if not isinstance(entry, dict) or not isinstance(entry.get("language"), str):
            continue
        compiler = entry.get("compiler") if isinstance(entry.get("compiler"), dict) else {}
        implicit = compiler.get("implicit") if isinstance(compiler.get("implicit"), dict) else {}
        result[entry["language"]] = {
            "compiler_id": compiler.get("id"),
            "compiler_version": compiler.get("version"),
            "compiler_path": _normalize_path(str(compiler.get("path", "")), root=root, cmake_root=cmake_root),
            "implicit_include_directories": _norm_dirs(implicit.get("includeDirectories")),
            "implicit_link_directories": _norm_dirs(implicit.get("linkDirectories")),
            "implicit_link_framework_directories": _norm_dirs(implicit.get("linkFrameworkDirectories")),
            "implicit_link_libraries": list(implicit.get("linkLibraries") or []),
            "source_file_extensions": sorted(str(e) for e in (entry.get("sourceFileExtensions") or [])),
        }
    return dict(sorted(result.items()))


def diff_toolchains_v1(
    oracle_tc: dict[str, dict[str, Any]],
    rcmake_tc: dict[str, dict[str, Any]],
) -> dict[str, Any]:
    """Compare normalized File API toolchains-v1 entries."""
    missing = sorted(oracle_tc.keys() - rcmake_tc.keys())
    extra = sorted(rcmake_tc.keys() - oracle_tc.keys())
    mismatches = [
        {"language": lang, "oracle": oracle_tc[lang], "rcmake": rcmake_tc[lang]}
        for lang in sorted(oracle_tc.keys() & rcmake_tc.keys())
        if oracle_tc[lang] != rcmake_tc[lang]
    ]
    equal = not (missing or extra or mismatches)
    return {
        "status": "pass" if equal else "fail",
        "languages": sorted(oracle_tc.keys() | rcmake_tc.keys()),
        "missing_languages": missing,
        "extra_languages": extra,
        "mismatches": mismatches,
    }


def normalize_cmake_files_v1(
    payload: dict[str, Any],
    *,
    root: Path | None = None,
    cmake_root: str | None = None,
) -> dict[str, Any]:
    """Normalize File API cmakeFiles-v1 project and built-in module inputs."""
    raw_paths = payload.get("paths") if isinstance(payload.get("paths"), dict) else {}
    project_inputs: list[str] = []
    cmake_inputs: list[str] = []
    for item in payload.get("inputs") or []:
        if not isinstance(item, dict) or not isinstance(item.get("path"), str):
            continue
        path = _normalize_path(item["path"], root=root, cmake_root=cmake_root)
        if item.get("isCMake"):
            cmake_inputs.append(path)
        else:
            project_inputs.append(path)
    return {
        "paths": {
            "source": _normalize_path(str(raw_paths.get("source", "")), root=root, cmake_root=cmake_root),
            "build": _normalize_path(str(raw_paths.get("build", "")), root=root, cmake_root=cmake_root),
        },
        "project_inputs": sorted(set(project_inputs)),
        "cmake_inputs": sorted(set(cmake_inputs)),
    }


def diff_cmake_files_v1(oracle_cf: dict[str, Any], rcmake_cf: dict[str, Any]) -> dict[str, Any]:
    """Compare normalized File API cmakeFiles-v1 inputs."""
    paths_match = oracle_cf.get("paths") == rcmake_cf.get("paths")
    left_proj = set(oracle_cf.get("project_inputs", []))
    right_proj = set(rcmake_cf.get("project_inputs", []))
    left_cmake = set(oracle_cf.get("cmake_inputs", []))
    right_cmake = set(rcmake_cf.get("cmake_inputs", []))
    missing_proj = sorted(left_proj - right_proj)
    extra_proj = sorted(right_proj - left_proj)
    missing_cmake = sorted(left_cmake - right_cmake)
    extra_cmake = sorted(right_cmake - left_cmake)
    equal = paths_match and not (missing_proj or extra_proj or missing_cmake or extra_cmake)
    return {
        "status": "pass" if equal else "fail",
        "paths_match": paths_match,
        "project_input_count": {"oracle": len(left_proj), "rcmake": len(right_proj)},
        "missing_project_inputs": missing_proj,
        "extra_project_inputs": extra_proj,
        "missing_cmake_inputs": missing_cmake,
        "extra_cmake_inputs": extra_cmake,
    }


def _reply_objects(index_payload: dict[str, Any] | None) -> dict[str, str]:
    if not isinstance(index_payload, dict):
        return {}
    reply = index_payload.get("reply")
    if not isinstance(reply, dict):
        return {}
    objects: dict[str, str] = {}
    for key, entry in reply.items():
        if isinstance(entry, dict) and isinstance(entry.get("jsonFile"), str):
            objects[key] = entry["jsonFile"]
    return objects


def compare_file_api_reply(
    oracle_reply: Path,
    rcmake_reply: Path,
    *,
    oracle_root: Path | None = None,
    rcmake_root: Path | None = None,
    oracle_tools: dict[str, set[str]] | None = None,
    rcmake_tools: dict[str, set[str]] | None = None,
    required_queries: tuple[str, ...] = (),
) -> dict[str, Any]:
    """Compare all File API reply objects in one build directory."""
    oracle_index = load_file_api_index(oracle_reply)
    rcmake_index = load_file_api_index(rcmake_reply)
    oracle_objs = _reply_objects(oracle_index)
    rcmake_objs = _reply_objects(rcmake_index)
    oracle_cmake_root = _extract_cmake_root(oracle_index)
    rcmake_cmake_root = _extract_cmake_root(rcmake_index)

    missing_oracle = sorted(set(required_queries) - set(oracle_objs))
    missing_rcmake = sorted((set(required_queries) | set(oracle_objs)) - set(rcmake_objs))
    extra_rcmake = sorted(set(rcmake_objs) - set(oracle_objs))

    results: dict[str, Any] = {}
    models: dict[str, dict[str, Any]] = {}
    caches: dict[str, dict[str, dict[str, str]]] = {}

    if "codemodel-v2" in oracle_objs and "codemodel-v2" in rcmake_objs:
        oracle_cm = normalize_codemodel(
            oracle_reply, oracle_objs["codemodel-v2"], root=oracle_root, cmake_root=oracle_cmake_root,
        )
        rcmake_cm = normalize_codemodel(
            rcmake_reply, rcmake_objs["codemodel-v2"], root=rcmake_root, cmake_root=rcmake_cmake_root,
        )
        models["oracle"] = oracle_cm
        models["rcmake"] = rcmake_cm
        results["codemodel-v2"] = diff_codemodel(oracle_cm, rcmake_cm)

    if "cache-v2" in oracle_objs and "cache-v2" in rcmake_objs:
        oracle_ca = normalize_cache_v2(
            json.loads((oracle_reply / oracle_objs["cache-v2"]).read_text(encoding="utf-8")),
            root=oracle_root, tools=oracle_tools, cmake_root=oracle_cmake_root,
        )
        rcmake_ca = normalize_cache_v2(
            json.loads((rcmake_reply / rcmake_objs["cache-v2"]).read_text(encoding="utf-8")),
            root=rcmake_root, tools=rcmake_tools, cmake_root=rcmake_cmake_root,
        )
        caches["oracle"] = oracle_ca
        caches["rcmake"] = rcmake_ca
        results["cache-v2"] = diff_cache_v2(oracle_ca, rcmake_ca)

    if "toolchains-v1" in oracle_objs and "toolchains-v1" in rcmake_objs:
        oracle_tc = normalize_toolchains_v1(
            json.loads((oracle_reply / oracle_objs["toolchains-v1"]).read_text(encoding="utf-8")),
            root=oracle_root, cmake_root=oracle_cmake_root,
        )
        rcmake_tc = normalize_toolchains_v1(
            json.loads((rcmake_reply / rcmake_objs["toolchains-v1"]).read_text(encoding="utf-8")),
            root=rcmake_root, cmake_root=rcmake_cmake_root,
        )
        results["toolchains-v1"] = diff_toolchains_v1(oracle_tc, rcmake_tc)

    if "cmakeFiles-v1" in oracle_objs and "cmakeFiles-v1" in rcmake_objs:
        oracle_cf = normalize_cmake_files_v1(
            json.loads((oracle_reply / oracle_objs["cmakeFiles-v1"]).read_text(encoding="utf-8")),
            root=oracle_root, cmake_root=oracle_cmake_root,
        )
        rcmake_cf = normalize_cmake_files_v1(
            json.loads((rcmake_reply / rcmake_objs["cmakeFiles-v1"]).read_text(encoding="utf-8")),
            root=rcmake_root, cmake_root=rcmake_cmake_root,
        )
        results["cmakeFiles-v1"] = diff_cmake_files_v1(oracle_cf, rcmake_cf)

    queries_ok = (
        oracle_index is not None
        and rcmake_index is not None
        and not missing_oracle
        and not missing_rcmake
        and not extra_rcmake
        and all(row["status"] == "pass" for row in results.values())
    )
    return {
        "status": "pass" if queries_ok else "fail",
        "oracle_queries": sorted(oracle_objs),
        "rcmake_queries": sorted(rcmake_objs),
        "missing_oracle_queries": missing_oracle,
        "missing_rcmake_queries": missing_rcmake,
        "extra_rcmake_queries": extra_rcmake,
        "queries": results,
        "_models": models,
        "_caches": caches,
    }


def _discover_ide_build_dirs(tree: Path) -> set[Path]:
    discovered: set[Path] = set()
    if not tree.is_dir():
        return discovered
    for directory, dirs, names in os.walk(tree):
        dirs[:] = [name for name in dirs if name not in {".git", "__pycache__"}]
        current = Path(directory)
        if "compile_commands.json" in names:
            discovered.add(current.relative_to(tree))
        rel = current.relative_to(tree)
        if rel.parts[-4:] == (".cmake", "api", "v1", "reply"):
            discovered.add(Path(*rel.parts[:-4]) if len(rel.parts) > 4 else Path("."))
    return discovered


def _configuration_signature(
    model: dict[str, Any] | None,
    compile_entries: list[dict[str, Any]] | None,
) -> dict[str, Any]:
    defines: set[str] = set()
    group_flags: set[str] = set()
    if model:
        for cfg in model.get("configurations", {}).values():
            for target_name, target in cfg.get("targets", {}).items():
                for group in target.get("compile_groups", []):
                    for define in group.get("defines", []):
                        defines.add(f"{target_name}:{define}")
                    for flag in group.get("flags", []):
                        group_flags.add(f"{target_name}:{flag}")
    cc_flags: set[str] = set()
    if compile_entries:
        for entry in compile_entries:
            flags = entry.get("flags", [])
            index = 0
            while index < len(flags):
                flag = flags[index]
                if flag in {"-o", "-c"} and index + 1 < len(flags):
                    index += 2
                    continue
                if flag.startswith("-o") and len(flag) > 2:
                    index += 1
                    continue
                cc_flags.add(f"{entry['file']}:{flag}")
                index += 1
    return {
        "defines": sorted(defines),
        "compile_group_flags": sorted(group_flags),
        "compile_command_flags": sorted(cc_flags),
    }


def _configuration_name(
    build_dir: str,
    file_api: dict[str, Any] | None,
) -> str | None:
    if file_api:
        for side in ("oracle", "rcmake"):
            model = file_api.get("_models", {}).get(side)
            if model and len(model.get("configurations", {})) == 1:
                only = next(iter(model["configurations"]))
                if only:
                    return only
            cache = file_api.get("_caches", {}).get(side)
            if cache and "CMAKE_BUILD_TYPE" in cache and cache["CMAKE_BUILD_TYPE"]["value"]:
                return cache["CMAKE_BUILD_TYPE"]["value"]
    leaf = Path(build_dir).name.lower()
    if leaf == "debug":
        return "Debug"
    if leaf == "release":
        return "Release"
    return None


def _compare_config_switch(
    debug_sigs: dict[str, dict[str, Any]],
    release_sigs: dict[str, dict[str, Any]],
) -> dict[str, Any]:
    deltas: dict[str, dict[str, Any]] = {}
    for engine in ("oracle", "rcmake"):
        dbg = debug_sigs.get(engine, {})
        rel = release_sigs.get(engine, {})
        dbg_defs, rel_defs = set(dbg.get("defines", [])), set(rel.get("defines", []))
        dbg_cc, rel_cc = set(dbg.get("compile_command_flags", [])), set(rel.get("compile_command_flags", []))
        dbg_cg, rel_cg = set(dbg.get("compile_group_flags", [])), set(rel.get("compile_group_flags", []))
        deltas[engine] = {
            "defines_added_in_release": sorted(rel_defs - dbg_defs),
            "defines_removed_in_release": sorted(dbg_defs - rel_defs),
            "compile_command_flags_added_in_release": sorted(rel_cc - dbg_cc),
            "compile_command_flags_removed_in_release": sorted(dbg_cc - rel_cc),
            "compile_group_flags_added_in_release": sorted(rel_cg - dbg_cg),
            "compile_group_flags_removed_in_release": sorted(dbg_cg - rel_cg),
            "changed": bool(dbg_defs != rel_defs or dbg_cc != rel_cc or dbg_cg != rel_cg),
        }
    deltas_match = deltas["oracle"] == deltas["rcmake"]
    both_changed = deltas["oracle"]["changed"] and deltas["rcmake"]["changed"]
    return {
        "from": "Debug",
        "to": "Release",
        "status": "pass" if (deltas_match and both_changed) else "fail",
        "deltas_match": deltas_match,
        "oracle_changed": deltas["oracle"]["changed"],
        "rcmake_changed": deltas["rcmake"]["changed"],
        "oracle": deltas["oracle"],
        "rcmake": deltas["rcmake"],
    }


def compare_ide_build_trees(
    oracle_tree: Path,
    rcmake_tree: Path,
    *,
    oracle_tools: dict[str, set[str]] | None = None,
    rcmake_tools: dict[str, set[str]] | None = None,
    required_queries: tuple[str, ...] = (),
    oracle_root: Path | None = None,
    conan_homes: tuple[str | None, str | None] = (None, None),
) -> dict[str, Any]:
    """Compare IDE data. Use oracle_root for paths recorded before a tree move."""
    oracle_root = oracle_root if oracle_root is not None else oracle_tree
    oracle_dirs = _discover_ide_build_dirs(oracle_tree)
    rcmake_dirs = _discover_ide_build_dirs(rcmake_tree)
    all_dirs = sorted(oracle_dirs | rcmake_dirs)
    build_reports: dict[str, dict[str, Any]] = {}
    config_signatures: dict[str, dict[str, dict[str, Any]]] = {}
    configurations: dict[str, str] = {}

    for rel_dir in all_dirs:
        key = _posix_str(rel_dir)
        oracle_build = oracle_tree / rel_dir
        rcmake_build = rcmake_tree / rel_dir
        entry: dict[str, Any] = {}

        oracle_reply = oracle_build / ".cmake/api/v1/reply"
        rcmake_reply = rcmake_build / ".cmake/api/v1/reply"
        oracle_cmake_root = None
        rcmake_cmake_root = None
        file_api_raw: dict[str, Any] | None = None

        if oracle_reply.is_dir() or rcmake_reply.is_dir():
            file_api_raw = compare_file_api_reply(
                oracle_reply, rcmake_reply,
                oracle_root=oracle_root, rcmake_root=rcmake_tree,
                oracle_tools=oracle_tools, rcmake_tools=rcmake_tools,
                required_queries=required_queries,
            )
            oracle_cmake_root = _extract_cmake_root(load_file_api_index(oracle_reply))
            rcmake_cmake_root = _extract_cmake_root(load_file_api_index(rcmake_reply))
            entry["file_api"] = {k: v for k, v in file_api_raw.items() if not k.startswith("_")}

        oracle_cc_path = oracle_build / "compile_commands.json"
        rcmake_cc_path = rcmake_build / "compile_commands.json"
        norm_cc: dict[str, list[dict[str, Any]]] = {}
        if oracle_cc_path.is_file() or rcmake_cc_path.is_file():
            if not oracle_cc_path.is_file() or not rcmake_cc_path.is_file():
                entry["compile_commands"] = {
                    "status": "fail",
                    "oracle_present": oracle_cc_path.is_file(),
                    "rcmake_present": rcmake_cc_path.is_file(),
                    "error": "compile_commands.json missing on one engine.",
                }
            else:
                try:
                    oracle_cc, rcmake_cc = (
                        _compile_conan_paths(json.loads(path.read_text(encoding="utf-8")), home)
                        for path, home in zip((oracle_cc_path, rcmake_cc_path), conan_homes)
                    )
                    norm_cc["oracle"] = normalize_compile_commands(
                        oracle_cc, root=oracle_root, tools=oracle_tools, cmake_root=oracle_cmake_root,
                    )
                    norm_cc["rcmake"] = normalize_compile_commands(
                        rcmake_cc, root=rcmake_tree, tools=rcmake_tools, cmake_root=rcmake_cmake_root,
                    )
                    entry["compile_commands"] = diff_compile_commands(
                        oracle_cc, rcmake_cc,
                        oracle_root=oracle_root, rcmake_root=rcmake_tree,
                        oracle_tools=oracle_tools, rcmake_tools=rcmake_tools,
                        oracle_cmake_root=oracle_cmake_root, rcmake_cmake_root=rcmake_cmake_root,
                    )
                except (OSError, ValueError) as exc:
                    entry["compile_commands"] = {"status": "fail", "error": str(exc)}

        cfg_name = _configuration_name(key, file_api_raw)
        if cfg_name:
            entry["configuration"] = cfg_name
            configurations[cfg_name] = key
            config_signatures[cfg_name] = {
                engine: _configuration_signature(
                    file_api_raw.get("_models", {}).get(engine) if file_api_raw else None,
                    norm_cc.get(engine),
                )
                for engine in ("oracle", "rcmake")
            }

        sub_statuses = [
            entry[section]["status"]
            for section in ("compile_commands", "file_api")
            if section in entry
        ]
        entry["status"] = "pass" if sub_statuses and all(s == "pass" for s in sub_statuses) else "fail"
        build_reports[key] = entry

    switches: list[dict[str, Any]] = []
    if "Debug" in config_signatures and "Release" in config_signatures:
        switches.append(_compare_config_switch(config_signatures["Debug"], config_signatures["Release"]))

    all_ok = (
        bool(build_reports)
        and all(row["status"] == "pass" for row in build_reports.values())
        and all(sw["status"] == "pass" for sw in switches)
    )
    return {
        "status": "pass" if all_ok else ("empty" if not build_reports else "fail"),
        "build_dirs": build_reports,
        "configurations": configurations,
        "configuration_switches": switches,
    }


def contents(file: File | None, limit: int = MAX_DIFF_BYTES) -> bytes:
    if file is None:
        return b""
    if file.path.is_symlink():
        return os.fsencode(os.readlink(file.path))
    with file.path.open("rb") as stream:
        return stream.read(limit + 1)


def diff_lines(file: File | None) -> list[str]:
    """Use text for small files and a digest for large or binary files."""
    if file is None:
        return []
    data = contents(file)
    if file.path.is_symlink():
        return [f"Symbolic link to {os.fsdecode(data)!r}\n"]
    if len(data) <= MAX_DIFF_BYTES and b"\0" not in data:
        try:
            return [
                line if line.endswith("\n") else line + "\n\\ No newline at end of file\n"
                for line in data.decode().splitlines(keepends=True)
            ] or ["Empty file\n"]
        except UnicodeDecodeError:
            pass
    with file.path.open("rb") as stream:
        digest = sha256()
        for block in iter(lambda: stream.read(64 * 1024), b""):
            digest.update(block)
    return [f"{file.path.stat().st_size} bytes, SHA-256 {digest.hexdigest()}\n"]


def _reply_cmake_roots(tree: Path, files: dict[Path, File]) -> dict[Path, str]:
    roots: dict[Path, str] = {}
    reply_parents = {
        file.relative.parent
        for file in files.values()
        if file.file_class == "File API reply"
    }
    for parent in reply_parents:
        try:
            root = _extract_cmake_root(load_file_api_index(tree / parent))
        except (OSError, ValueError):
            root = None
        if root:
            roots[parent] = root
    return roots


def compare_pair(
    left: File | None,
    right: File | None,
    name_rule: str | None,
    tools: tuple[dict, dict],
    references: tuple[dict, dict],
    *,
    cmake_roots: tuple[dict[Path, str], dict[Path, str]] = ({}, {}),
    conan_homes: tuple[str | None, str | None] = (None, None),
) -> tuple[str, list[str]]:
    if left is None or right is None:
        return "different", []
    rules = {name_rule} if name_rule else set()
    if left.path.is_symlink() or right.path.is_symlink():
        equal = (left.path.is_symlink() and right.path.is_symlink()
                 and contents(left) == contents(right))
    else:
        equal = filecmp.cmp(left.path, right.path, shallow=False)
        side_paths = conan_homes[0] != conan_homes[1] and left.file_class in {
            "CMakeCache.txt", "compile_commands.json",
        }
        if (not equal or side_paths) and max(left.path.stat().st_size, right.path.stat().st_size) <= MAX_NORMALIZE_BYTES:
            left_data, right_data = (contents(file, MAX_NORMALIZE_BYTES) for file in (left, right))
            omit_editors = (
                _optional_cache_editor(left_data, right_data) if left.file_class == "CMakeCache.txt" else (False, False)
            )
            before, left_rules = normalize(
                left, left_data, tools[0], references[0],
                cmake_root=cmake_roots[0].get(left.relative.parent),
                conan_home=conan_homes[0], omit_cache_editor=omit_editors[0],
            )
            after, right_rules = normalize(
                right, right_data, tools[1], references[1],
                cmake_root=cmake_roots[1].get(right.relative.parent),
                conan_home=conan_homes[1], omit_cache_editor=omit_editors[1],
            )
            equal = before == after
            rules.update(left_rules | right_rules)
    if not equal:
        return "different", []
    return ("identical_after_normalization", sorted(rules)) if rules else ("byte_identical", [])


def _ninja_build(file: File) -> Path:
    parent = file.path.parent
    return parent.parent if file.file_class == "rules.ninja" and parent.name == "CMakeFiles" else parent


def _normalize_editor_edges(rows, normalizer, cache, has_editor):
    """Replace the cache editor command when it matches the generated utility."""
    for row in rows:
        if row.get("outputs") not in (["<build>/CMakeFiles/edit_cache.util"], ["<build>/edit_cache.util"]):
            continue
        if has_editor:
            editor = cache.get(EDIT_COMMAND)
            source = cache.get("CMAKE_HOME_DIRECTORY")
            if not editor or not source:
                continue
            expected = [normalizer.text(editor), "-S" + normalizer.text(source), "-B<build>"]
        else:
            expected = ["<cmake>", "-E", "echo", "No interactive CMake dialog available."]
        try:
            tokens = shlex.split(row["command"])
        except ValueError:
            continue
        direct = tokens == expected
        in_build = tokens == ["cd", "<build>", "&&", *expected]
        if in_build or (direct and not has_editor):
            row["command"] = "<edit-cache>"


def compare(oracle: Path, rcmake: Path, diff_dir: Path, *,
            configured_outputs: tuple[str, ...] = (), tools: tuple[dict, dict] = ({}, {}),
            oracle_root: Path | None = None,
            conan_homes: tuple[str | None, str | None] = (None, None)) -> dict:
    """Return file class counts and write a unified diff for every difference."""
    trees = (oracle, rcmake)
    if any(Path(pattern).is_absolute() or ".." in Path(pattern).parts for pattern in configured_outputs):
        raise ValueError("A configure_file output glob must stay within the project.")
    configured = {
        path.relative_to(tree) for tree in trees for pattern in configured_outputs
        for path in tree.glob(pattern)
    }
    inventories = tuple(inventory(tree, configured) for tree in trees)
    cmake_roots = tuple(_reply_cmake_roots(tree, inv) for tree, inv in zip(trees, inventories))
    pairs, references = align(*inventories)
    project_roots = (oracle_root or oracle, rcmake)

    @lru_cache(maxsize=None)
    def ninja_diff(before: Path, after: Path) -> tuple[list[str], list[str], bool]:
        builds = (before, after)
        cache_data = tuple(map(_read_cache, builds))
        caches = map(_internal_cache, cache_data)
        editors = _optional_cache_editor(*cache_data)
        graphs = []
        for index, (build, cache, editor) in enumerate(zip(builds, caches, editors)):
            root = cache.get("CMAKE_ROOT")
            normalizer = BuildNormalizer(
                build, project_roots[index], tools[index],
                cmake_roots=(root,) if root and Path(root).is_absolute() else (),
                conan_home=conan_homes[index],
                recorded_build=project_roots[index] / build.relative_to(trees[index]),
            )
            rows = canonicalize_strict(normalizer)
            if any(editors):
                _normalize_editor_edges(rows, normalizer, cache, editor)
            graphs.append([json.dumps(row, sort_keys=True) + "\n" for row in rows])
        return graphs[0], graphs[1], any(editors)

    classes = {}
    files = []
    for index, (left, right, rule) in enumerate(pairs, 1):
        file = left or right
        graph_lines = None
        graph_error = None
        if left and right and file.file_class in {"build.ninja", "rules.ninja"} and not (
            left.path.is_symlink() or right.path.is_symlink()
        ):
            status, rules = "different", []
            try:
                before, after, editors = ninja_diff(_ninja_build(left), _ninja_build(right))
                graph_lines = (before, after)
                if before == after:
                    status = "identical_after_normalization"
                    rules = ["ninja.canonical_graph"]
                    if editors:
                        rules.append("cache.edit_command")
                    if any(conan_homes):
                        rules.append("conan.home_path")
                # Keep this bucket for intact, byte-identical manifests.
                if before == after and file.file_class == "build.ninja" and filecmp.cmp(
                    left.path, right.path, shallow=False
                ):
                    status, rules = "byte_identical", []
            except (OSError, ValueError) as error:
                graph_error = str(error)
        else:
            status, rules = compare_pair(
                left, right, rule, tools, references, cmake_roots=cmake_roots, conan_homes=conan_homes,
            )
        row = {
            "oracle": str(left.relative) if left else None,
            "rcmake": str(right.relative) if right else None,
            "class": file.file_class, "status": status, "rules": rules,
        }
        if status == "different":
            diff_dir.mkdir(parents=True, exist_ok=True)
            diff = diff_dir / f"{index:05d}.diff"
            first, second = graph_lines if graph_lines is not None else (diff_lines(left), diff_lines(right))
            details = "".join(unified_diff(
                first, second,
                fromfile=f"oracle/{left.relative}" if left else "/dev/null",
                tofile=f"rcmake/{right.relative}" if right else "/dev/null",
            ))
            if graph_error:
                row["error"] = graph_error
                details = f"{graph_error}\n{details}"
            diff.write_text(details, encoding="utf-8", errors="backslashreplace")
            row["diff"] = str(diff)
        classes.setdefault(file.file_class, dict.fromkeys(BUCKETS, 0))[status] += 1
        files.append(row)
    result: dict[str, Any] = {"classes": classes, "files": files, "rules": RULES}
    ide = compare_ide_build_trees(oracle, rcmake, oracle_tools=tools[0], rcmake_tools=tools[1],
                                  oracle_root=oracle_root, conan_homes=conan_homes)
    if ide["build_dirs"]:
        result["ide"] = ide
    return result
