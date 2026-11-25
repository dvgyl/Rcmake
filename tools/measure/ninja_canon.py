#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Compare expanded edges and dry-run commands from Ninja build directories."""

import argparse
from collections import ChainMap
from collections.abc import Callable, Iterable, Iterator
from dataclasses import dataclass, field
from difflib import unified_diff
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys

ESCAPE = r"\$(?:[ $:]|\{[\w.-]+\}|[\w-]+)"
VARIABLE = re.compile(ESCAPE + r"|\$")
TOKEN = re.compile(rf"(?:{ESCAPE}|[^ :|$])+|\|\||\|@|[:|]")
ASSIGNMENT = re.compile(r"([\w.-]+) *= *(.*)")
SEPARATORS = {"|", "||", "|@"}
PROGRESS = re.compile(r"^\[\d+/\d+\] ", re.MULTILINE)
NINJA_SAFE_PATH = re.compile(r"[a-zA-Z0-9_+.,/\-]+")
MAX_INCLUDE_DEPTH = 64
MAX_VARIABLE_DEPTH = 64


def expand(value: str, lookup: Callable[[str], str]) -> str:
    def replace(match: re.Match[str]) -> str:
        name = match[0][1:]
        if name in {" ", "$", ":"}:
            return name
        if not name:
            raise ValueError(f"Invalid Ninja escape in {value!r}.")
        return lookup(name.removeprefix("{").removesuffix("}"))

    return VARIABLE.sub(replace, value)


def tokens(value: str) -> list[str]:
    result = []
    position = 0
    for match in TOKEN.finditer(value):
        if value[position:match.start()].strip(" "):
            raise ValueError(f"Invalid Ninja path list: {value}")
        result.append(match[0])
        position = match.end()
    if value[position:].strip(" "):
        raise ValueError(f"Invalid Ninja path list: {value}")
    return result


def statements(path: Path) -> Iterator[tuple[str, dict[str, str]]]:
    """Read Ninja statements with their indented bindings."""
    statement = ""
    bindings = {}
    continuation = None
    with path.open(encoding="utf-8", errors="surrogateescape") as stream:
        for physical_line in stream:
            line = physical_line.rstrip("\r\n")
            if continuation is not None:
                line = continuation + line.lstrip(" ")
            if line.lstrip(" ").startswith("#") or not line.strip(" "):
                continuation = None
                continue
            if (len(line) - len(line.rstrip("$"))) % 2:
                continuation = line[:-1]
                continue
            continuation = None
            if line.startswith(" "):
                match = ASSIGNMENT.fullmatch(line.lstrip(" "))
                if not statement or match is None:
                    raise ValueError(f"Invalid Ninja binding in {path}: {line}")
                bindings[match[1]] = match[2]
                continue
            if statement:
                yield statement, bindings
            statement, bindings = line, {}
    if continuation is not None:
        raise ValueError(f"Unfinished Ninja line in {path}.")
    if statement:
        yield statement, bindings


@dataclass
class Edge:
    """Store one Ninja build statement and the scope used to expand its rule."""

    outputs: list[str]
    implicit_outputs: list[str]
    inputs: list[str]
    implicit_inputs: list[str]
    order_only_inputs: list[str]
    validations: list[str]
    rule: dict[str, str]
    scope: ChainMap[str, str]
    bindings: dict[str, str]

    def evaluate(self, key: str, *, shell_escape: bool = True) -> str:
        def path_list(paths: list[str], separator: str) -> str:
            if shell_escape:
                paths = [
                    path if NINJA_SAFE_PATH.fullmatch(path)
                    else "'" + path.replace("'", "'\\''") + "'"
                    for path in paths
                ]
            return separator.join(paths)

        def lookup(name: str, ancestors: tuple[str, ...] = ()) -> str:
            if name in {"in", "in_newline"}:
                return path_list(self.inputs, "\n" if name == "in_newline" else " ")
            if name == "out":
                return path_list(self.outputs, " ")
            if name in self.bindings:
                return self.bindings[name]
            if name not in self.rule:
                return self.scope.get(name, "")
            if name in ancestors or len(ancestors) >= MAX_VARIABLE_DEPTH:
                raise ValueError(f"Invalid Ninja rule variable chain: {' -> '.join((*ancestors, name))}")
            return expand(self.rule[name], lambda item: lookup(item, (*ancestors, name)))

        return lookup(key)


def paths_by_separator(paths: list[str], scope: ChainMap[str, str]) -> dict[str, list[str]]:
    result: dict[str, list[str]] = {"": [], "|": [], "||": [], "|@": []}
    group = ""
    for token in paths:
        if token in SEPARATORS:
            group = token
        else:
            path = expand(token, lambda key: scope.get(key, ""))
            if not path:
                raise ValueError("A Ninja path expanded to an empty string.")
            result[group].append(os.path.normpath(path))
    return result


def read_edges(build: Path, manifest: str = "build.ninja") -> list[Edge]:
    """Read edges in build.ninja and its include and subninja files."""
    edges: list[Edge] = []

    def read(path: Path, scope: ChainMap[str, str],
             rules: ChainMap[str, dict[str, str]], active: tuple[Path, ...]) -> None:
        path = path.resolve()
        if path in active or len(active) >= MAX_INCLUDE_DEPTH:
            raise ValueError(f"Invalid Ninja include chain at {path}.")
        for statement, bindings in statements(path):
            keyword, _, argument = statement.partition(" ")
            argument = argument.lstrip(" ")
            if keyword in {"include", "subninja"}:
                included = expand(argument, lambda key: scope.get(key, ""))
                subninja = keyword == "subninja"
                read(build / included, scope.new_child() if subninja else scope,
                     rules.new_child() if subninja else rules, (*active, path))
            elif keyword == "rule":
                rules[argument] = bindings
            elif keyword == "build":
                parts = tokens(argument)
                try:
                    colon = parts.index(":")
                    rule = rules[parts[colon + 1]]
                except (IndexError, KeyError, ValueError) as error:
                    raise ValueError(f"Invalid Ninja edge in {path}: {statement}") from error
                values = {key: expand(value, lambda name: scope.get(name, ""))
                          for key, value in bindings.items()}
                outputs = paths_by_separator(parts[:colon], scope)
                inputs = paths_by_separator(parts[colon + 2:], scope)
                edges.append(Edge(
                    outputs[""], outputs["|"], inputs[""], inputs["|"],
                    inputs["||"], inputs["|@"], rule, scope, values,
                ))
            elif match := ASSIGNMENT.fullmatch(statement):
                scope[match[1]] = expand(match[2], lambda key: scope.get(key, ""))
            elif keyword not in {"pool", "default"}:
                raise ValueError(f"Unknown Ninja statement in {path}: {statement}")

    read(build / manifest, ChainMap(), ChainMap({"phony": {}}), ())
    return edges


@dataclass
class Normalizer:
    """Replace checkout, build, and optional tool paths with stable labels."""

    build: Path
    source: Path | None = None
    replacements: dict[Path, str] = field(default_factory=dict)
    pattern: re.Pattern[str] = field(init=False, repr=False)
    labels: dict[str, str] = field(init=False, repr=False)

    def __post_init__(self) -> None:
        roots = {self.build: "<build>", **self.replacements}
        self.build = self.build.resolve()
        if self.source is not None:
            roots[self.source] = "<source>"
        self.labels = {}
        for path, label in roots.items():
            for absolute in {str(path.absolute()), str(path.resolve())}:
                self.labels[absolute] = label
                self.labels[shlex.quote(absolute)] = label
        alternatives = "|".join(map(re.escape, sorted(self.labels, key=len, reverse=True)))
        self.pattern = re.compile(rf"(?:{alternatives})(?=$|[/\s'\":;,|)&\]])")

    def text(self, value: str) -> str:
        return self.pattern.sub(lambda match: self.labels[match[0]], value)

    def path(self, value: str) -> str:
        return self.text(os.path.normpath(self.build / value)) if value else ""


def row_key(row: dict | str) -> str:
    return json.dumps(row, sort_keys=True)


def canonicalize(normalize: Normalizer, manifest: str = "build.ninja") -> list[dict]:
    """Expand rule variables and return one canonical record per build edge."""
    rows = []
    for edge in read_edges(normalize.build, manifest):
        rows.append({
            "outputs": sorted(map(normalize.path, edge.outputs)),
            "implicit_outputs": sorted(map(normalize.path, edge.implicit_outputs)),
            "inputs": list(map(normalize.path, edge.inputs)),
            "implicit_inputs": sorted(map(normalize.path, edge.implicit_inputs)),
            "order_only_inputs": sorted(map(normalize.path, edge.order_only_inputs)),
            "validations": sorted(map(normalize.path, edge.validations)),
            "dyndep": normalize.path(edge.evaluate("dyndep", shell_escape=False)),
            "command": normalize.text(edge.evaluate("command")),
            "rspfile": normalize.path(edge.evaluate("rspfile", shell_escape=False)),
            "rspfile_content": normalize.text(edge.evaluate("rspfile_content")),
        })
    return sorted(rows, key=lambda row: (row["outputs"], row["implicit_outputs"], row_key(row)))


def normalized_commands(stdout: str, normalize: Normalizer) -> list[str]:
    """Return sorted Ninja commands with every occurrence retained."""
    return sorted(normalize.text(command.removesuffix("\n"))
                  for command in PROGRESS.split(stdout)[1:])


def dry_run(normalize: Normalizer, targets: Iterable[str] = (), timeout: int = 120,
            manifest: str = "build.ninja") -> list[str]:
    """Return the normalized commands from a verbose Ninja dry run."""
    result = subprocess.run(
        ["ninja", "-f", manifest, "-n", "-v", "--", *targets],
        cwd=normalize.build, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, errors="replace", timeout=timeout,
        env={**os.environ, "NINJA_STATUS": "[%f/%t] "},
    )
    if result.returncode:
        raise ValueError(result.stderr.strip() or result.stdout.strip())
    return normalized_commands(result.stdout, normalize)


def differences(left: list, right: list) -> Iterator[str]:
    """Return a unified diff with one edge or command per line."""
    return unified_diff(list(map(row_key, left)), list(map(row_key, right)),
                        fromfile="oracle", tofile="rcmake", lineterm="")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("build", type=Path)
    parser.add_argument("other_build", type=Path, nargs="?")
    parser.add_argument("--source", type=Path)
    parser.add_argument("--other-source", type=Path)
    parser.add_argument("--manifest", default="build.ninja")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--target", action="append", default=[])
    args = parser.parse_args()

    def read(build: Path, source: Path | None) -> list:
        normalizer = Normalizer(build, source)
        if args.dry_run:
            return dry_run(normalizer, args.target, manifest=args.manifest)
        return canonicalize(normalizer, args.manifest)

    try:
        left = read(args.build, args.source)
        if args.other_build is None:
            print(json.dumps(left, indent=2))
            return 0
        right = read(args.other_build, args.other_source or args.source)
        if left == right:
            return 0
        for line in differences(left, right):
            print(line)
        return 1
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"Ninja comparison failed: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
