#!/usr/bin/env python3
"""Check source licences, clean-room rules, and locked Cargo dependencies."""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
VENDORED_CMAKE = pathlib.Path("modules/cmake-4.5")
CMAKE_LICENCE = pathlib.Path("third_party/licences/CMake-BSD-3-Clause.txt")
NOTICES = ROOT / "THIRD_PARTY_NOTICES.md"
NOTICES_START = "<!-- BEGIN GENERATED CARGO NOTICES -->"
NOTICES_END = "<!-- END GENERATED CARGO NOTICES -->"

SKIP_DIRS = {".git", "target", "env", "__pycache__"}
FORBIDDEN_SPDX = re.compile(
    r"SPDX-" r"License-Identifier:\s*[^\n]*\b(A?L?GPL|MPL|SSPL|BUSL|CDDL|EPL)\b",
    re.IGNORECASE,
)
FORBIDDEN_UNSAFE_RE = re.compile(r"\bunsafe\s*(\{|fn\b|impl\b|trait\b)")
ALLOW_UNSAFE_RE = re.compile(r"#!?\[allow\([^)]*unsafe_code[^)]*\)\]")
ORACLE_ENV_RE = re.compile(r"RCMAKE_ORACLE_CMAKE|RCMAKE_ORACLE_CTEST|RCMAKE_ORACLE_CPACK")
CORE_CRATE_ALLOW_ORACLE = {"rcmake-harness"}

ALLOWED_LICENCES = {
    "MIT", "Apache-2.0", "BSD-2-Clause", "BSD-3-Clause", "0BSD",
    "ISC", "Zlib", "Unicode-3.0", "bzip2-1.0.6",
}
SPDX_IDENTIFIER = re.compile(r"[A-Za-z0-9.-]+(?:\+|:LicenseRef-[A-Za-z0-9.-]+)?")
NOTICE_FILE = re.compile(
    r"(?:licen[cs]e|copying|copyright|notice|authors|unlicense)(?:$|[._-])",
    re.IGNORECASE,
)
HOME_PATH = re.compile(r"/(?:Users|home)/")
HOME_PATH_EXAMPLES = {
    VENDORED_CMAKE / "Modules/FindDoxygen.cmake": "/".join(("", "home", "user/doxygen-example/build")),
    VENDORED_CMAKE / "Modules/Platform/Windows-MSVC.cmake": "/".join(("", "Users", "*.")),
}
HYGIENE_SUFFIXES = {
    ".bash", ".bat", ".cfg", ".cmake", ".cmd", ".conf", ".env", ".fish",
    ".ini", ".js", ".json", ".md", ".mdx", ".mk", ".pl", ".plist", ".ps1",
    ".py", ".rb", ".rst", ".sh", ".toml", ".ts", ".txt", ".xml", ".yaml",
    ".yml", ".zsh",
}
SEALED_INPUTS = {pathlib.Path("conformance/corpus/unseen.toml")}
HYGIENE_NAMES = {".editorconfig", ".gitattributes", ".gitignore", "GNUmakefile", "Makefile"}


def iter_files(base: pathlib.Path):
    for path in sorted(base.rglob("*")):
        if any(part in SKIP_DIRS for part in path.relative_to(ROOT).parts):
            continue
        if path.is_file():
            yield path


def check_source(errors: list[str]) -> None:
    for directory in ("crates", "tools", "scripts", "modules"):
        for path in iter_files(ROOT / directory):
            rel = path.relative_to(ROOT)
            try:
                content = path.read_text(encoding="utf-8", errors="ignore")
            except OSError as exc:
                errors.append(f"Could not read {rel}: {exc}")
                continue

            if FORBIDDEN_SPDX.search(content):
                errors.append(f"Non-permissive SPDX identifier in {rel}")

            if path.suffix.lower() in {".cc", ".cpp", ".cxx", ".h", ".hpp", ".hxx"}:
                if not rel.is_relative_to(VENDORED_CMAKE) and "https://cmake.org/licensing" in content:
                    errors.append(f"Upstream CMake C++ source outside the vendored tree: {rel}")

            if directory != "crates" or path.suffix != ".rs":
                continue
            if ALLOW_UNSAFE_RE.search(content):
                errors.append(f"unsafe_code allow attribute in {rel}")
            for lineno, line in enumerate(content.splitlines(), 1):
                if not line.lstrip().startswith("//") and FORBIDDEN_UNSAFE_RE.search(line):
                    errors.append(f"unsafe keyword in {rel}:{lineno}")
            if rel.parts[1] not in CORE_CRATE_ALLOW_ORACLE and ORACLE_ENV_RE.search(content):
                errors.append(f"Core crate references oracle env var: {rel}")

    for required in (
        pathlib.Path("LICENSE-MIT"),
        pathlib.Path("LICENSE-APACHE"),
        NOTICES.relative_to(ROOT),
        CMAKE_LICENCE,
        CMAKE_LICENCE.with_name("CMake-CONTRIBUTORS.rst"),
        VENDORED_CMAKE / "Modules/CMake.cmake",
        VENDORED_CMAKE / "Templates/CPackConfig.cmake.in",
        VENDORED_CMAKE / "Help/reference.zst",
    ):
        if not (ROOT / required).is_file():
            errors.append(f"Missing required file: {required}")

    licence = ROOT / CMAKE_LICENCE
    if licence.is_file():
        content = licence.read_text(encoding="utf-8")
        for marker in (
            "Kitware, Inc.",
            "Redistributions of source code",
            "Redistributions in binary form",
            "Neither the name of Kitware, Inc.",
            "THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS",
        ):
            if marker not in content:
                errors.append(f"Missing BSD 3-Clause text in {CMAKE_LICENCE}: {marker}")

    cmake_module = ROOT / VENDORED_CMAKE / "Modules/CMake.cmake"
    if cmake_module.is_file() and "BSD 3-Clause License" not in cmake_module.read_text(encoding="utf-8"):
        errors.append(f"Missing BSD 3-Clause header in {cmake_module.relative_to(ROOT)}")


def spdx_expression(declaration: str) -> str:
    # Older Cargo manifests use a slash for OR.
    return re.sub(r"\s*/\s*", " OR ", declaration)


def licence_allowed(expression: str) -> bool:
    """Evaluate a Cargo SPDX expression against the project allowlist."""
    tokens = iter(re.findall(r"[^\s()]+|[()]", spdx_expression(expression)))
    token = next(tokens, None)

    def consume(expected: str) -> bool:
        nonlocal token
        if token != expected:
            return False
        token = next(tokens, None)
        return True

    def identifier() -> str:
        nonlocal token
        if token is None or token in {"AND", "OR", "WITH"} or not SPDX_IDENTIFIER.fullmatch(token):
            raise ValueError(f"Expected a licence identifier, found {token!r}")
        value = token
        token = next(tokens, None)
        return value

    def term() -> bool:
        if consume("("):
            value = disjunction()
            if not consume(")"):
                raise ValueError("Missing closing parenthesis")
            return value
        licence = identifier()
        if consume("WITH"):
            identifier()
            return False
        return any(licence.lower() == allowed.lower() for allowed in ALLOWED_LICENCES)

    def conjunction() -> bool:
        value = term()
        while consume("AND"):
            right = term()
            value = value and right
        return value

    def disjunction() -> bool:
        value = conjunction()
        while consume("OR"):
            right = conjunction()
            value = value or right
        return value

    allowed = disjunction()
    if token is not None:
        raise ValueError(f"Unexpected token {token!r}")
    return allowed


def resolved_dependencies(nodes: dict, roots, kinds: set) -> set[str]:
    pending = list(roots)
    visited = set()
    while pending:
        package = pending.pop()
        if package in visited:
            continue
        visited.add(package)
        pending.extend(
            dependency["pkg"]
            for dependency in nodes[package]["deps"]
            if any(edge["kind"] in kinds for edge in dependency["dep_kinds"])
        )
    return visited


def audit_dependencies(errors: list[str]) -> tuple[list[dict], int, int]:
    result = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked"],
        cwd=ROOT, check=True, stdout=subprocess.PIPE, timeout=120,
    )
    metadata = json.loads(result.stdout)
    members = set(metadata["workspace_members"])
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}

    cli = next((key for key in members if packages[key]["name"] == "rcmake-cli"), None)
    if cli is None:
        raise ValueError("Cargo metadata has no rcmake-cli workspace member")
    workspace_deps = resolved_dependencies(nodes, members, {None, "build", "dev"}) - members
    normal_deps = resolved_dependencies(nodes, [cli], {None}) - members
    cli_deps = resolved_dependencies(nodes, [cli], {None, "build"}) - members

    for key in sorted(workspace_deps):
        package = packages[key]
        name = f'{package["name"]} {package["version"]}'
        expression = package["license"]
        if not expression:
            errors.append(f"No SPDX licence expression for {name}")
            continue
        try:
            if not licence_allowed(expression):
                errors.append(f"No allowed licence for {name}: {expression}")
        except (ValueError, RecursionError) as exc:
            errors.append(f"Invalid SPDX licence expression for {name}: {expression} ({exc})")

    return [packages[key] for key in sorted(cli_deps)], len(normal_deps), len(workspace_deps)


def licence_files(package: dict) -> list[pathlib.Path]:
    directory = pathlib.Path(package["manifest_path"]).parent
    files = {path for path in directory.iterdir() if path.is_file() and NOTICE_FILE.match(path.name)}
    if package["license_file"]:
        files.add(directory / package["license_file"])
    return sorted(files, key=lambda path: path.as_posix())


def table_text(value: str) -> str:
    # Escape Markdown table delimiters and prevent HTML from crate metadata.
    return (
        " ".join(value.split()).replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
        .replace("\\", "\\\\").replace("|", "\\|").replace("[", "\\[").replace("]", "\\]")
    )


def generate_notices(packages: list[dict]) -> str:
    rows = [
        "### Resolved crates for `rcmake-cli`",
        "",
        "Cargo resolves this list for all targets. It includes normal and build dependencies.",
        "Where a licence offers a choice, Rcmake uses an option on the scanner's allowlist.",
        "The table links to the licence and notice files included in each crate.",
        "",
        "| Crate | Version | SPDX licence | Repository | Authors | Licence and notice files |",
        "| --- | --- | --- | --- | --- | --- |",
    ]
    texts: dict[str, int] = {}
    for package in sorted(packages, key=lambda p: (p["name"], p["version"], p["id"])):
        links = []
        for path in licence_files(package):
            if not path.is_file():
                continue
            content = "\n".join(
                line.rstrip() for line in path.read_text(encoding="utf-8").splitlines()
            ).strip("\n")
            if content:
                number = texts.setdefault(content, len(texts) + 1)
                links.append(f"[`{path.name}`](#crate-notice-{number})")
        repository = package["repository"] or "—"
        if re.fullmatch(r"https?://[^\s<>`|]+", repository):
            repository = f"<{repository}>"
        else:
            repository = table_text(repository)
        rows.append(
            f'| `{package["name"]}` | `{package["version"]}` | '
            f'`{spdx_expression(package["license"])}` | {repository} | '
            f'{table_text(", ".join(package["authors"])) or "—"} | '
            f'{", ".join(links) or "No notice file in the Cargo package"} |'
        )

    rows += ["", "### Licence and notice text from Cargo packages", ""]
    for content, number in texts.items():
        longest_run = max((len(run) for run in re.findall(r"`+", content)), default=0)
        fence = "`" * max(3, longest_run + 1)
        rows += [f"#### Crate notice {number}", "", fence + "text", content, fence, ""]
    return "\n".join(rows)


def check_notices(packages: list[dict], regenerate: bool, errors: list[str]) -> None:
    if not NOTICES.is_file():
        return
    with NOTICES.open(encoding="utf-8", newline="") as file:
        existing = file.read()
    before, start, current = existing.partition(NOTICES_START)
    current, end, after = current.partition(NOTICES_END)
    marker_counts = (existing.count(NOTICES_START), existing.count(NOTICES_END))
    if marker_counts != (0, 0) and (marker_counts != (1, 1) or not end):
        errors.append("Invalid Cargo notice markers in THIRD_PARTY_NOTICES.md")
        return

    generated = generate_notices(packages)
    newline = "\r\n" if "\r\n" in before else "\n"
    section = (f"\n\n{generated}\n").replace("\n", newline)
    if regenerate:
        if not start:
            before = before.rstrip("\r\n") + newline * 2
            after = newline
        updated = before + NOTICES_START + section + NOTICES_END + after
        if updated != existing:
            NOTICES.write_text(updated, encoding="utf-8", newline="")
    elif current != section:
        errors.append(
            "Cargo notices are missing or out of date. Run "
            "python3 tools/measure/licence_scan.py --generate-notices."
        )


def check_hygiene(errors: list[str]) -> None:
    result = subprocess.run(
        ["git", "ls-files", "--stage", "-z"], cwd=ROOT, check=True,
        stdout=subprocess.PIPE, timeout=30,
    )
    for entry in result.stdout.split(b"\0"):
        if not entry:
            continue
        fields, filename = entry.split(b"\t", 1)
        mode, _, stage = fields.split()
        rel = pathlib.Path(os.fsdecode(filename))
        if rel in SEALED_INPUTS:
            continue
        if any(part.startswith("._") or part == ".DS_Store" for part in rel.parts):
            errors.append(f"Tracked macOS metadata file: {rel}")
        if stage != b"0" or mode not in {b"100644", b"100755"}:
            continue
        path = ROOT / rel
        if not path.is_file() or path.is_symlink():
            continue
        with path.open("rb") as file:
            prefix = file.read(3)
            hashbang = prefix.startswith(b"#!") and prefix != b"#!["
            if rel.parts[0] != "modules":
                if hashbang and mode != b"100755":
                    errors.append(f"File starts with #! but index mode is {mode.decode()}: {rel}")
                if not hashbang and mode == b"100755":
                    errors.append(f"Index mode is 100755 but file does not start with #!: {rel}")

            suffixes = {suffix.lower() for suffix in rel.suffixes}
            script = hashbang
            if not (script or suffixes & HYGIENE_SUFFIXES or rel.name in HYGIENE_NAMES):
                continue
            file.seek(0)
            example = HOME_PATH_EXAMPLES.get(rel)
            for lineno, raw_line in enumerate(file, 1):
                line = raw_line.decode("utf-8", errors="replace")
                for match in HOME_PATH.finditer(line):
                    if example and line[match.start():].split(maxsplit=1)[0] == example:
                        continue
                    errors.append(f"Developer home path at {rel}:{lineno}:{match.start() + 1}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--generate-notices", action="store_true", help="Regenerate the Cargo notices.")
    parser.add_argument("--check-hygiene", action="store_true", help="Also check Git modes and tracked paths.")
    parser.add_argument("--hygiene-only", action="store_true", help="Check only Git modes and tracked paths.")
    args = parser.parse_args()
    if args.hygiene_only and args.generate_notices:
        parser.error("--hygiene-only cannot regenerate Cargo notices.")

    errors: list[str] = []
    if not args.hygiene_only:
        try:
            check_source(errors)
            packages, normal_count, workspace_count = audit_dependencies(errors)
            if not errors:
                check_notices(packages, args.generate_notices, errors)
        except (OSError, ValueError, KeyError, subprocess.SubprocessError) as exc:
            errors.append(f"Could not complete the licence scan: {exc}")

    if args.check_hygiene or args.hygiene_only:
        try:
            check_hygiene(errors)
        except (OSError, ValueError, subprocess.SubprocessError) as exc:
            errors.append(f"Could not complete the hygiene scan: {exc}")

    if errors:
        print("Licence or hygiene scan failed.", file=sys.stderr)
        for error in errors:
            print(f"  - {error}", file=sys.stderr)
        return 1

    if not args.hygiene_only:
        print(
            f"Licence scan passed for {workspace_count} workspace dependencies. "
            f"Cargo notices cover {normal_count} normal and {len(packages) - normal_count} "
            "build-only rcmake-cli dependencies."
        )
    if args.check_hygiene or args.hygiene_only:
        print("Git hygiene scan passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
