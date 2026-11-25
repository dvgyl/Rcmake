#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Print a canonical Xcode project or compare two projects with a unified diff."""

import argparse
from collections import Counter, deque
import difflib
import json
from pathlib import Path
import re
import shlex
import sys


TOKEN = re.compile(
    r'\s+|//[^\r\n]*|/\*.*?\*/|"(?:[^"\\]|\\.)*"|[{}();=,]|'
    r'(?:[^\s{}();=,"/]|/(?![/*]))+',
    re.DOTALL,
)
ID = re.compile(r"[0-9A-Fa-f]{24}\Z")
ESCAPE = re.compile(r"\\(?:[0-7]{1,3}|U[0-9A-Fa-f]{4}|\r?\n|.)", re.DOTALL)


def unescape(match):
    token = match[0][1:]
    if token in {"\n", "\r\n"}:
        return ""
    if token.startswith("U") and len(token) == 5:
        return chr(int(token[1:], 16))
    if token[0] in "01234567":
        return chr(int(token, 8))
    return {"n": "\n", "r": "\r", "t": "\t", "b": "\b", "f": "\f"}.get(token, token)


class Parser:
    """Read the dictionaries, arrays, strings, and comments in a project."""

    def __init__(self, text):
        self.text = text
        self.pos = 0
        self.token = self.read_token()

    def read_token(self):
        while self.pos < len(self.text):
            match = TOKEN.match(self.text, self.pos)
            if match is None:
                raise ValueError(f"Invalid OpenStep token at offset {self.pos}")
            self.pos = match.end()
            token = match[0]
            if not token.isspace() and not token.startswith(("//", "/*")):
                return token
        return None

    def take(self, expected=None):
        token = self.token
        if token is None or (expected is not None and token != expected):
            raise ValueError(f"Expected {expected or 'a value'} at offset {self.pos}, found {token!r}")
        self.token = self.read_token()
        return token

    def atom(self):
        token = self.take()
        if token in "{}();=,":
            raise ValueError(f"Expected a string at offset {self.pos}")
        return ESCAPE.sub(unescape, token[1:-1]) if token.startswith('"') else token

    def value(self, depth=0):
        if depth > 128:
            raise ValueError("The OpenStep file has more than 128 levels")
        if self.token == "{":
            self.take("{")
            result = {}
            while self.token != "}":
                key = self.atom()
                self.take("=")
                if key in result:
                    raise ValueError(f"Duplicate OpenStep key {key!r}")
                result[key] = self.value(depth + 1)
                self.take(";")
            self.take("}")
            return result
        if self.token == "(":
            self.take("(")
            result = []
            while self.token != ")":
                result.append(self.value(depth + 1))
                if self.token == ")":
                    break
                self.take(",")
            self.take(")")
            return result
        return self.atom()


# Preserve build phase and linker input order.
UNORDERED = {
    "buildConfigurations", "children", "dependencies", "inputPaths",
    "knownRegions", "outputPaths", "targets",
}
SHELL_LINE = re.compile(r"""\s*[^#&|;<>()$`*?\[\]{}~\r\n]+\s*\Z""")


def normalize_script(script, paths):
    lines = []
    for line in script.splitlines(keepends=True):
        condition = re.fullmatch(r'\s*if (?:test|\[) "\$CONFIGURATION" = "?([^";\s]+)"?; then :\s*', line)
        if condition:
            lines.append(f"if CONFIGURATION = {condition[1]}")
        elif SHELL_LINE.fullmatch(line.removesuffix("\n")) and "\\" not in line.rstrip("\n")[-1:]:
            try:
                lines.append([paths(word) for word in shlex.split(line, posix=True)])
            except ValueError:
                lines.append(paths(line.removesuffix("\n")))
        else:
            lines.append(paths(line.removesuffix("\n")))
    return lines


def project_objects(project):
    if not isinstance(project, dict) or not isinstance(project.get("objects"), dict):
        raise ValueError("The file has no Xcode object dictionary")
    objects = project["objects"]
    root = project.get("rootObject")
    if not isinstance(root, str) or root not in objects:
        raise ValueError("The Xcode root object is missing")
    for key, obj in objects.items():
        if not ID.fullmatch(key) or not isinstance(obj, dict) or not isinstance(obj.get("isa"), str):
            raise ValueError(f"Invalid Xcode object {key!r}")
    return root, objects


def canonicalize(project, source, build):
    """Replace object IDs with labels based on the reference graph."""
    root, objects = project_objects(project)
    roots = sorted(((root, name) for root, name in [(source, "<S>"), (build, "<B>")]
                    if root and root != "/"), key=lambda entry: len(entry[0]), reverse=True)

    def paths(value):
        for path, label in roots:
            value = re.sub(re.escape(path) + r"(?=$|[/\s'\":,;)])", label, value)
        return value

    def identity(key):
        obj = objects[key]
        for field in ("path", "name", "remoteInfo", "fileRef", "target"):
            value = obj.get(field)
            if isinstance(value, str):
                if value in objects:
                    target = objects[value]
                    value = target.get("path", target.get("name", target["isa"]))
                return f"{obj['isa']}:{paths(str(value))}"
        if obj["isa"] == "PBXTargetDependency":
            proxy = objects.get(obj.get("targetProxy"), {})
            if "remoteInfo" in proxy:
                return f"PBXTargetDependency:{paths(proxy['remoteInfo'])}"
        return obj["isa"]

    def sort_key(value):
        if isinstance(value, str):
            return identity(value) if value in objects else paths(value)
        return json.dumps(value, sort_keys=True)

    def references(value, field=""):
        if isinstance(value, str):
            if value in objects:
                yield field, value
        elif isinstance(value, list):
            if field in UNORDERED:
                value = sorted(value, key=sort_key)
            for child in value:
                yield from references(child, field)
        elif isinstance(value, dict):
            for key, child in sorted(value.items(), key=lambda item: sort_key(item[0])):
                if key in objects:
                    yield field, key
                yield from references(child, key)

    labels = {root: "@project"}
    queue = deque([root])
    while queue:
        current = queue.popleft()
        count = Counter()
        for field, ref in references(objects[current]):
            if ref in labels:
                continue
            base = f"{labels[current]}/{field}/{identity(ref)}"
            count[base] += 1
            labels[ref] = base if count[base] == 1 else f"{base}#{count[base]}"
            queue.append(ref)

    def shape(value):
        if isinstance(value, str):
            return labels.get(value, identity(value) if value in objects else paths(value))
        if isinstance(value, list):
            return [shape(child) for child in value]
        if isinstance(value, dict):
            return {key: shape(child) for key, child in value.items()}
        return value

    # Some producers leave unreferenced objects. Sort them by their fields.
    count = Counter()
    for ref in sorted(objects.keys() - labels.keys(),
                      key=lambda ref: (identity(ref), json.dumps(shape(objects[ref]), sort_keys=True))):
        base = f"@unreferenced/{identity(ref)}"
        count[base] += 1
        labels[ref] = f"{base}#{count[base]}"

    def replace(value, field=""):
        if isinstance(value, str):
            if field == "shellScript":
                return normalize_script(value, paths)
            return labels.get(value, paths(value))
        if isinstance(value, list):
            items = [replace(child) for child in value]
            return sorted(items, key=lambda item: json.dumps(item, sort_keys=True)) if field in UNORDERED else items
        if isinstance(value, dict):
            return {labels.get(key, key): replace(child, key) for key, child in value.items()}
        return value

    result = replace(project)
    return json.dumps(result, indent=2, sort_keys=True, ensure_ascii=True).splitlines(keepends=True)


def native_targets(project, source):
    """Compare native products, source membership, configurations, and dependencies."""
    root, objects = project_objects(project)
    targets = [objects[ref] for ref in objects[root]["targets"] if objects[ref]["isa"] == "PBXNativeTarget"]
    if not targets or not source:
        raise ValueError("The Xcode project needs native targets and a source root.")
    source = Path(source).resolve()
    rows = {}
    for target in targets:
        name = target["name"]
        if name in rows:
            raise ValueError(f"Duplicate native target {name!r}.")
        sources = []
        for phase_id in target["buildPhases"]:
            phase = objects[phase_id]
            if phase["isa"] != "PBXSourcesBuildPhase":
                continue
            for file_id in phase["files"]:
                file = objects[objects[file_id]["fileRef"]]
                path = Path(file["path"])
                if not path.is_absolute() and file["sourceTree"] != "SOURCE_ROOT":
                    raise ValueError(f"Cannot resolve source path {path} for {name}.")
                path = (source / path).resolve()
                sources.append((path.relative_to(source) if path.is_relative_to(source) else path).as_posix())
        configs = objects[target["buildConfigurationList"]]["buildConfigurations"]
        dependencies = [objects[objects[ref]["target"]] for ref in target.get("dependencies", [])]
        rows[name] = {
            "configurations": sorted(objects[ref]["name"] for ref in configs),
            "dependencies": sorted(dep["name"] for dep in dependencies if dep["isa"] == "PBXNativeTarget"),
            "product": objects[target["productReference"]]["path"],
            "product_type": target["productType"],
            "sources": sorted(sources),
        }
    return json.dumps(rows, indent=2, sort_keys=True).splitlines(keepends=True)


def read(path, source, build, *, targets_only=False):
    if path.is_dir():
        path = path / "project.pbxproj"
    parser = Parser(path.read_text(encoding="utf-8-sig"))
    project = parser.value()
    if parser.token is not None:
        raise ValueError("Unexpected text after the project")
    if source is None and isinstance(project, dict):
        objects = project.get("objects")
        root = project.get("rootObject")
        if isinstance(objects, dict) and isinstance(root, str):
            obj = objects.get(root)
            if isinstance(obj, dict) and isinstance(obj.get("projectDirPath"), str):
                source = obj["projectDirPath"]
    if targets_only:
        return native_targets(project, source)
    return canonicalize(project, source, str(build or path.absolute().parent.parent))


def main():
    argp = argparse.ArgumentParser(description=__doc__)
    argp.add_argument("project", type=Path, help="Path to a pbxproj file or an xcodeproj directory")
    argp.add_argument("other", type=Path, nargs="?", help="Path to a second project")
    argp.add_argument("--source-root", help="Source path to replace with <S>")
    argp.add_argument("--build-root", type=Path, help="Build path to replace with <B>")
    argp.add_argument("--other-source-root", help="Source path for the second project")
    argp.add_argument("--other-build-root", type=Path, help="Build path for the second project")
    argp.add_argument("--native-targets", action="store_true",
                      help="Compare native target products, sources, configurations, and dependencies.")
    args = argp.parse_args()
    try:
        first = read(args.project, args.source_root, args.build_root, targets_only=args.native_targets)
        if args.other is None:
            sys.stdout.write("".join(first) + "\n")
            return 0
        second = read(args.other, args.other_source_root or args.source_root, args.other_build_root,
                      targets_only=args.native_targets)
        if first == second:
            return 0
        for line in difflib.unified_diff(first, second, str(args.project), str(args.other)):
            sys.stdout.write(line if line.endswith("\n") else line + "\n")
        return 1
    except (KeyError, OSError, TypeError, ValueError) as error:
        print(f"pbxproj_canon: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
