#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Generate CMake target property initializer pairs from the pinned Help files."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import sys


ROOT = Path(__file__).resolve().parents[2]
INCLUDE = re.compile(r"^\.\. include:: (\S+)\s*$", re.MULTILINE)
REPLACE = re.compile(r"^\.\. \|([^|]+)\| replace:: ([^\n]+)$", re.MULTILINE)
VARIABLE = r"(?::variable:`|``)(CMAKE_[A-Za-z0-9_<>]+)(?: <[^`]+>)?`{1,2}"
INITIALIZERS = [
    re.compile(pattern, re.IGNORECASE)
    for pattern in (
        r"\binitialized (?:by|from|to|with) (?:the )?(?:value of )?(?:the )?(?:variable )?" + VARIABLE,
        r"\binitialized for all targets using the (?:variable )?" + VARIABLE,
        r"\binitial value for this property is taken from the " + VARIABLE,
        r"\bthis property defaults to (?:the )?" + VARIABLE,
        r"\bif (?:the )?(?:variable )?" + VARIABLE
        + r" [^.]+?(?:used to initialize (?:this|the) property|this property is initialized to)",
        VARIABLE + r" variable, which can be used to initialize this property\b",
        VARIABLE + r" variable is used to initialize the property instead\b",
    )
]
FALLBACK = re.compile(r"\botherwise by the value of " + VARIABLE)
SET_INITIALIZER = re.compile(r"\bTo initialize this (?:property|variable) for all targets set (.+)")

# The Help page does not list languages for this property.
# CMake 4.5.1 initializes these variants when it creates a target.
VISIBILITY_LANGUAGES = ("C", "CXX", "CUDA", "Fortran", "HIP", "OBJC", "OBJCXX", "SYCL")


def property_docs(override: Path | None) -> Path:
    """Find the pinned property documentation directory."""
    if override is not None:
        return override
    candidates = []
    if run_cmake := os.environ.get("RCMAKE_ORACLE_RUNCMAKE_DIR"):
        candidates.append(Path(run_cmake).expanduser().resolve().parents[1])
    if source := os.environ.get("RCMAKE_ORACLE_SRC"):
        candidates.append(Path(source).expanduser())
    candidates.append(ROOT / "env/oracle/cmake-4.5.1")
    for source in candidates:
        directory = source / "Help/prop_tgt"
        if directory.is_dir():
            return directory
    raise ValueError(
        "CMake 4.5.1 Help files are unavailable. Set RCMAKE_ORACLE_RUNCMAKE_DIR or use --help-dir."
    )


def expand_includes(path: Path, help_root: Path, stack: tuple[Path, ...] = ()) -> str:
    """Read the Help page and expand its shared sections."""
    if path in stack or len(stack) >= 16:
        raise ValueError(f"Cannot expand the include chain for {path}.")
    if not path.is_relative_to(help_root):
        raise ValueError(f"Include outside the Help tree: {path}")

    def include(match: re.Match[str]) -> str:
        target = match[1]
        included = help_root / target.lstrip("/") if target.startswith("/") else path.parent / target
        return expand_includes(included.resolve(), help_root, (*stack, path))

    return INCLUDE.sub(include, path.read_text(encoding="utf-8"))


def resolve_substitutions(document: str) -> str:
    """Replace RST substitutions used to name initializer variables."""
    substitutions = dict(REPLACE.findall(document))
    document = REPLACE.sub("", document)
    return re.sub(r"\|([^|\n]+)\|", lambda match: substitutions.get(match[1], match[0]), document)


def language_variants(prop: str, document: str) -> list[str]:
    """Read the languages or property variants named by a template page."""
    if prop == "<LANG>_VISIBILITY_PRESET":
        return [prop.replace("<LANG>", language) for language in VISIBILITY_LANGUAGES]
    if "The variations are:" in document:
        suffix = prop.removeprefix("<LANG>")
        pattern = rf"^\* :prop_tgt:`([A-Za-z0-9_]+{re.escape(suffix)})`"
        return re.findall(pattern, document, re.MULTILINE)
    scope = re.search(
        r"(?:implemented|supported) only when ``<LANG>`` is (``.+?)(?:\.\s|\.$)",
        document, re.DOTALL,
    )
    if scope:
        languages = re.findall(r"``([A-Za-z0-9_]+)``", scope[1])
    else:
        languages = re.findall(r"^\* ``([A-Za-z0-9_]+)``$", document, re.MULTILINE)
    return [prop.replace("<LANG>", language) for language in languages]


def initializer_pairs(directory: Path) -> list[tuple[str, str]]:
    """Extract the documented initializer pairs in target property order."""
    directory = directory.resolve()
    pairs = set()
    for page in sorted(directory.glob("*.rst")):
        document = resolve_substitutions(expand_includes(page, directory.parent))
        prop = document.splitlines()[0].strip()
        if not re.fullmatch(r"[A-Za-z0-9_<>]+", prop):
            continue
        variables = set()
        for paragraph in re.split(r"\n[ \t]*\n", document):
            text = " ".join(paragraph.split())
            matched = set()
            for pattern in INITIALIZERS:
                matched.update(pattern.findall(text))
            if matched:
                variables.update(matched)
                variables.update(FALLBACK.findall(text))
            if set_clause := SET_INITIALIZER.search(text):
                variable = f"CMAKE_{prop}"
                if variable in re.findall(VARIABLE, set_clause[1]):
                    variables.add(variable)
        for variable in variables:
            if prop.startswith("<LANG>") and "<LANG>" in variable:
                variants = language_variants(prop, document)
                if variants:
                    suffix = prop.removeprefix("<LANG>")
                    pairs.update(
                        (variant, variable.replace("<LANG>", variant.removesuffix(suffix)))
                        for variant in variants
                    )
                    continue
            pairs.add((prop, variable))
    if not pairs:
        raise ValueError(f"No target property initializers found in {directory}.")
    return sorted(pairs)


def render(pairs: list[tuple[str, str]]) -> str:
    """Return a Rust table with one row per documented pair."""
    lines = [
        "// @generated by tools/gen/gen_target_init.py from CMake 4.5.1 Help.",
        "",
        "/// CMake variables documented as target property initializers.",
        "/// Some initializers apply only to specific target types, generators, or policies.",
        "pub static TARGET_PROP_INITS: &[(&str, &str)] = &[",
    ]
    for prop, variable in pairs:
        prop, variable = json.dumps(prop), json.dumps(variable)
        row = f"    ({prop}, {variable}),"
        if len(row) <= 68:
            lines.append(row)
        else:
            lines.extend(["    (", f"        {prop},", f"        {variable},", "    ),"])
    lines.append("];")
    return "\n".join(lines) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--stdout", action="store_true", help="Print the Rust table. This is the default.")
    mode.add_argument("--out", type=Path, help="Write the Rust table to this path.")
    parser.add_argument("--help-dir", type=Path, help="Path to the pinned Help/prop_tgt directory.")
    args = parser.parse_args()
    try:
        pairs = initializer_pairs(property_docs(args.help_dir))
        output = render(pairs)
        if args.out is not None:
            args.out.write_text(output, encoding="utf-8")
        else:
            sys.stdout.write(output)
        print(f"Generated {len(pairs)} target property initializer pairs.", file=sys.stderr)
    except BrokenPipeError:
        return 0
    except (OSError, ValueError) as error:
        print(error, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
