#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Generate LSP documentation tables from CMake 4.5.1 RST files."""

import argparse
import os
from pathlib import Path
import re
import subprocess
from textwrap import dedent

ROOT = Path(__file__).resolve().parents[2]
BUNDLED = ROOT / "modules/cmake-4.5"
OUTPUT = ROOT / "crates/rcmake-lsp/src/generated_catalog.rs"
CATEGORIES = {
    "command": "COMMANDS",
    "variable": "VARIABLES",
    "policy": "POLICIES",
    "prop_tgt": "TARGET_PROPERTIES",
    "module": "MODULES",
}
HEADING = re.compile(r"\A([^\n]+)\n[=~^`-]{2,}\s*\n")
ROLE = re.compile(r":([a-z_]+):`([^`]+)`")
INCLUDE = re.compile(r"(?m)^\.\. include:: +(\S+)[ \t]*$")
SUBSTITUTION = re.compile(r"(?m)^\.\. \|([^|\n]+)\| replace::[ \t]*(.*(?:\n[ \t]+[^\n]+)*)")
RST_CODE = re.compile(
    r"(?m)^\.\. (?:code-block|code|parsed-literal|signature)::([^\n]*)\n"
    r"((?:[ \t][^\n]*\n|\n(?=[ \t]))*)"
)
FIND_SUMMARY = {
    "find_file": "Find the full path to a named file.",
    "find_library": "Find a library.",
    "find_path": "Find the directory that contains a named file.",
    "find_program": "Find an executable program.",
}
MODULE = re.compile(r"(?m)^\.\. cmake-module::\s+(\S+)")
BLOCK = re.compile(r"#\[(=*)\[\.rst:\s*\n(.*?)\]\1\]", re.S)
READONLY_VARS = {
    "CMAKE_BINARY_DIR", "CMAKE_COMMAND", "CMAKE_CURRENT_BINARY_DIR",
    "CMAKE_CURRENT_FUNCTION", "CMAKE_CURRENT_FUNCTION_LIST_DIR",
    "CMAKE_CURRENT_FUNCTION_LIST_FILE", "CMAKE_CURRENT_FUNCTION_LIST_LINE",
    "CMAKE_CURRENT_LIST_DIR", "CMAKE_CURRENT_LIST_FILE", "CMAKE_CURRENT_LIST_LINE",
    "CMAKE_CURRENT_SOURCE_DIR", "CMAKE_GENERATOR", "CMAKE_MAJOR_VERSION",
    "CMAKE_MINOR_VERSION", "CMAKE_PATCH_VERSION", "CMAKE_PROJECT_NAME",
    "CMAKE_SOURCE_DIR", "CMAKE_VERSION", "PROJECT_BINARY_DIR", "PROJECT_NAME",
    "PROJECT_SOURCE_DIR", "PROJECT_VERSION",
}


def help_source():
    if (BUNDLED / "Help/command/add_executable.rst").is_file():
        return BUNDLED / "Help"
    if source := os.environ.get("RCMAKE_ORACLE_SRC"):
        if (Path(source) / "Help").is_dir():
            return Path(source) / "Help"
    git_dir = subprocess.check_output(
        ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"],
        cwd=ROOT, text=True, timeout=10,
    ).strip()
    return Path(git_dir).parent / "env/oracle/cmake-4.5.1/Help"


def documentation(path):
    text = path.read_text(encoding="utf-8")
    match = MODULE.search(text)
    if not match:
        return text
    module = (BUNDLED / "Modules" / Path(match[1]).name)
    if not module.is_file():
        module = path.parent / match[1]
    body = module.read_text(encoding="utf-8")
    if block := BLOCK.search(body):
        return block[2]
    return "\n".join(
        line.removeprefix("#").removeprefix(" ")
        for line in body.splitlines() if line.startswith("#")
    )


def render_role(match):
    role, label = match.groups()
    if " <" in label and label.endswith(">"):
        label = label.rsplit(" <", 1)[0]
    elif role == "command" and "(" not in label:
        label += "()"
    return f"``{label.lstrip('~')}``"


def expand_includes(text, path, help_dir, depth=0):
    if depth >= 4:
        return text

    def include(match):
        name = match[1]
        included = (help_dir / name[1:]) if name.startswith("/") else path.parent / name
        if included.is_file():
            return "\n\n" + expand_includes(documentation(included), included, help_dir, depth + 1)
        return ""

    return INCLUDE.sub(include, text)


def replace_substitutions(text):
    values = {match[1]: " ".join(match[2].split()) for match in SUBSTITUTION.finditer(text)}
    text = SUBSTITUTION.sub("", text)
    for _ in range(3):
        text = re.sub(r"\|([^|\n]+)\|", lambda match: values.get(match[1], match[0]), text)
    return text


def summarize(text):
    if heading := HEADING.match(text):
        text = text[heading.end():]
    opening = ""
    for paragraph in re.split(r"\n[ \t]*\n", text):
        paragraph = paragraph.lstrip("\n")
        if not paragraph or paragraph.startswith((" ", "\t", "..")) or HEADING.match(paragraph + "\n"):
            continue
        opening = " ".join(paragraph.split())
        break
    if not opening:
        directive = re.search(
            r"(?m)^\.\. (?:deprecated|note|versionchanged)::[^\n]*\n(?:\n)?"
            r"((?:[ \t][^\n]+\n)+)",
            text,
        )
        if directive:
            opening = " ".join(directive[1].split())
    opening = ROLE.sub(render_role, opening)
    opening = re.sub(r"`([^`]+?)`_+", r"``\1``", opening).removesuffix("::")
    if len(opening) > 360:
        opening = opening[:357].rsplit(" ", 1)[0] + "..."
        if opening.count("``") % 2:
            opening += "``"
    return opening


def command_signature(text, name):
    for block in RST_CODE.finditer(text):
        lines = [block[1].strip(), *dedent(block[2]).strip().splitlines()]
        for index, line in enumerate(lines):
            if not line.startswith((f"{name}(", f"{name} (")):
                continue
            signature = []
            for part in lines[index:]:
                if not part.strip() or part.lstrip().startswith(":"):
                    break
                part = part.split(" #", 1)[0].rstrip()
                signature.append(part)
                if part.endswith(")"):
                    return "\n".join(signature)
    raise ValueError(f"Cannot find the signature for {name}.")


def entries(help_dir, category):
    rows = {}
    aliases = {}
    for path in sorted((help_dir / category).glob("*.rst")):
        if path.name.startswith("."):
            continue
        raw = documentation(path)
        text = replace_substitutions(expand_includes(raw, path, help_dir))
        heading = HEADING.match(text)
        name = heading[1] if heading and category in ("variable", "prop_tgt") else path.stem
        description = FIND_SUMMARY.get(name) or summarize(text) or f"See the CMake documentation for ``{name}``."
        signature = ""
        if category == "command":
            signature = command_signature(text, name)
        elif category == "module":
            signature = (f"find_package({name[4:]} ...)" if name.startswith("Find")
                         else f"include({name})")
        deprecated = bool(re.search(r"(?m)^\.\. deprecated::", raw))
        readonly = category in ("variable", "prop_tgt") and (
            name in READONLY_VARS or description.lower().startswith(("read-only", "read only"))
        )
        flags = (2 if deprecated else 0) | (4 if readonly else 0)
        stem = "" if path.stem == name else path.stem
        rows[name] = (description, signature, stem, flags)
        if category == "variable" and "<LANG>" in name and name.count("<") == 1:
            for language in ("C", "CXX"):
                alias = name.replace("<LANG>", language)
                aliases[alias] = (description.replace("<LANG>", language), "", path.stem, flags)
    aliases.update(rows)
    return rows, aliases


def rust_string(value):
    escapes = {"\\": r"\\", '"': r'\"', "\n": r"\n", "\r": r"\r", "\t": r"\t", "\0": r"\0"}
    parts = ['"']
    for char in value:
        code = ord(char)
        if char in escapes:
            parts.append(escapes[char])
        elif code < 0x20 or 0x7f <= code <= 0x9f:
            parts.append(f"\\u{{{code:x}}}")
        else:
            parts.append(char)
    parts.append('"')
    return "".join(parts)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--help-source", type=Path, help="CMake 4.5.1 Help directory")
    args = parser.parse_args()
    help_dir = args.help_source or help_source()
    licence = (ROOT / "third_party/licences/CMake-BSD-3-Clause.txt").read_text()
    output = [
        "// @generated by tools/gen/gen_lsp_catalog.py",
        "// Documentation from CMake 4.5.1 (BSD-3-Clause).",
        *[f"// {line}".rstrip() for line in licence.splitlines()],
        "",
        "use crate::catalog::Entry;",
        "",
    ]
    for category, table in CATEGORIES.items():
        source, rows = entries(help_dir, category)
        if not source:
            parser.error(f"Cannot find RST files in {help_dir / category}. Use --help-source.")
        output.extend(("#[rustfmt::skip]", f"pub(super) static {table}: &[Entry] = &["))
        for name, (*fields, flags) in sorted(rows.items()):
            strings = ", ".join(rust_string(field) for field in (name, *fields))
            output.append(f"    Entry({strings}, {flags}),")
        output.extend(("];", ""))
        print(f"{category}: {len(source)} topics, {len(rows) - len(source)} aliases")
    OUTPUT.write_text("\n".join(output), encoding="utf-8")


if __name__ == "__main__":
    main()
