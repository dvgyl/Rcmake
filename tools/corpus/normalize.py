# SPDX-License-Identifier: MIT OR Apache-2.0
"""Compare generated files and record each normalization rule used."""

from __future__ import annotations

from dataclasses import dataclass
from difflib import unified_diff
import filecmp
from hashlib import sha256
import json
import os
from pathlib import Path
import re

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
    "cache.tool_path": "The cache records the executable path of the selected CMake tool.",
    "json.layout": "JSON object key order and whitespace do not change the JSON value.",
    "file_api.index_name": "The File API puts the time in the index file name.",
    "file_api.reply_hash": "The File API puts a content hash in reply file names and references.",
}
CACHE_TOOLS = {"CMAKE_COMMAND": "cmake", "CMAKE_CTEST_COMMAND": "ctest", "CMAKE_CPACK_COMMAND": "cpack"}
REPLY_HASH = re.compile(
    r"(codemodel|cache|cmakeFiles|toolchains|configureLog|target|directory)"
    r"(-.+)-[0-9a-f]{20}(\.json)"
)
MAX_DIFF_BYTES = 256 * 1024
MAX_NORMALIZE_BYTES = 4 * 1024 * 1024


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


def normalize(file: File, data: bytes, tools: dict[str, set[str]],
              references: dict[Path, dict[str, str]]) -> tuple[bytes, set[str]]:
    """Apply only the rules for this file class."""
    rules = set()
    if file.file_class == "CMakeCache.txt":
        lines = []
        for line in data.splitlines(keepends=True):
            match = re.fullmatch(rb"([^#/:=\r\n]+):(?:INTERNAL|FILEPATH)=([^\r\n]+)(\r?\n)?", line)
            if match:
                key = match[1].decode("utf-8", errors="replace")
                tool = CACHE_TOOLS.get(key)
                if tool and os.fsdecode(match[2]) in tools.get(tool, ()):
                    start, end = match.span(2)
                    line = line[:start] + f"<{tool}>".encode() + line[end:]
                    rules.add("cache.tool_path")
            lines.append(line)
        return b"".join(lines), rules
    if file.file_class in {"File API reply", "compile_commands.json"}:
        try:
            value = json.loads(
                data, object_pairs_hook=unique_keys, parse_float=reject_noninteger_number,
            )
            raw = json.dumps(value, sort_keys=True, allow_nan=False).encode()
            rewritten = raw
            if names := references.get(file.relative.parent):
                rewrite_replies(value, names)
                rewritten = json.dumps(value, sort_keys=True, allow_nan=False).encode()
        except (ValueError, RecursionError):
            return data, rules
        if raw != data:
            rules.add("json.layout")
        if raw != rewritten:
            rules.add("file_api.reply_hash")
        return rewritten, rules
    return data, rules


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


def compare_pair(left: File | None, right: File | None, name_rule: str | None,
                 tools: tuple[dict, dict], references: tuple[dict, dict]) -> tuple[str, list[str]]:
    if left is None or right is None:
        return "different", []
    rules = {name_rule} if name_rule else set()
    if left.path.is_symlink() or right.path.is_symlink():
        equal = (left.path.is_symlink() and right.path.is_symlink()
                 and contents(left) == contents(right))
    else:
        equal = filecmp.cmp(left.path, right.path, shallow=False)
        if not equal and max(left.path.stat().st_size, right.path.stat().st_size) <= MAX_NORMALIZE_BYTES:
            before, left_rules = normalize(left, contents(left, MAX_NORMALIZE_BYTES), tools[0], references[0])
            after, right_rules = normalize(right, contents(right, MAX_NORMALIZE_BYTES), tools[1], references[1])
            equal = before == after
            rules.update(left_rules | right_rules)
    if not equal:
        return "different", []
    return ("identical_after_normalization", sorted(rules)) if rules else ("byte_identical", [])


def compare(oracle: Path, rcmake: Path, diff_dir: Path, *,
            configured_outputs: tuple[str, ...] = (), tools: tuple[dict, dict] = ({}, {})) -> dict:
    """Return file class counts and write a unified diff for every difference."""
    trees = (oracle, rcmake)
    if any(Path(pattern).is_absolute() or ".." in Path(pattern).parts for pattern in configured_outputs):
        raise ValueError("A configure_file output glob must stay within the project.")
    configured = {
        path.relative_to(tree) for tree in trees for pattern in configured_outputs
        for path in tree.glob(pattern)
    }
    pairs, references = align(*(inventory(tree, configured) for tree in trees))
    classes = {}
    files = []
    for index, (left, right, rule) in enumerate(pairs, 1):
        file = left or right
        status, rules = compare_pair(left, right, rule, tools, references)
        row = {
            "oracle": str(left.relative) if left else None,
            "rcmake": str(right.relative) if right else None,
            "class": file.file_class, "status": status, "rules": rules,
        }
        if status == "different":
            diff_dir.mkdir(parents=True, exist_ok=True)
            diff = diff_dir / f"{index:05d}.diff"
            diff.write_text("".join(unified_diff(
                diff_lines(left), diff_lines(right),
                fromfile=f"oracle/{left.relative}" if left else "/dev/null",
                tofile=f"rcmake/{right.relative}" if right else "/dev/null",
            )), encoding="utf-8", errors="backslashreplace")
            row["diff"] = str(diff)
        classes.setdefault(file.file_class, dict.fromkeys(BUCKETS, 0))[status] += 1
        files.append(row)
    return {"classes": classes, "files": files, "rules": RULES}
