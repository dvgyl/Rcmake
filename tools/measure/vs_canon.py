#!/usr/bin/env python3
"""Validate and canonicalize Visual Studio solutions and MSBuild projects."""

from __future__ import annotations

import argparse
import json
from pathlib import Path, PurePosixPath
import posixpath
import re
import sys
from urllib.parse import unquote
import xml.etree.ElementTree as ET

MSBUILD = "{http://schemas.microsoft.com/developer/msbuild/2003}"
FOLDER_TYPE = "2150e333-8fdc-42a3-9474-1a3956d46de8"
GUID = re.compile(r"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}", re.I)
SLN_PROJECT = re.compile(
    r'^Project\("([^"]+)"\) = "((?:[^"]|"")*)", "((?:[^"]|"")*)", "([^"]+)"$'
)
SECTION = re.compile(r"GlobalSection\(([^)]+)\) = (preSolution|postSolution)")
SUFFIXES = (".sln", ".slnx", ".vcxproj", ".vcxproj.filters")
ORDER = {
    "ProjectConfigurations": 0, "Globals": 1, "Microsoft.Cpp.Default.props": 2,
    "Configuration": 3, "Microsoft.Cpp.props": 4, "ExtensionSettings": 5,
    "PropertySheets": 6, "UserMacros": 7, "General": 8, "ItemDefinitionGroup": 9,
    "Items": 10, "Microsoft.Cpp.targets": 11, "ExtensionTargets": 12,
}


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def guid(value: str | None) -> str:
    value = (value or "").strip().removeprefix("{").removesuffix("}")
    require(bool(GUID.fullmatch(value)), f"Invalid GUID: {value!r}")
    return value.lower()


def path_text(value: str) -> str:
    return posixpath.normpath(value.replace("\\", "/"))


def local_path(owner: Path, value: str) -> Path:
    return (owner.parent / path_text(value)).resolve()


def insert(mapping: dict, key, value, label: str) -> None:
    require(key not in mapping, f"Duplicate {label}: {key}")
    mapping[key] = value


def pair(value: str) -> tuple[str, str]:
    fields = value.split("|")
    require(len(fields) == 2 and all(fields), f"Invalid configuration: {value!r}")
    return fields[0], fields[1]


def tag(element: ET.Element) -> str:
    return element.tag.removeprefix(MSBUILD)


def xml_file(path: Path, root_tag: str) -> ET.Element:
    root = ET.parse(path).getroot()
    require(root.tag == root_tag, f"Expected root element {root_tag}")
    return root


def project_xml(path: Path, root: ET.Element) -> dict:
    metadata = root.find(f"./{MSBUILD}PropertyGroup[@Label='Globals']")
    require(metadata is not None, "Missing Globals property group")
    identifier = guid(metadata.findtext(f"{MSBUILD}ProjectGuid"))
    configs = {}
    for entry in root.findall(f"./*/{MSBUILD}ProjectConfiguration"):
        name = entry.get("Include", "")
        config = entry.findtext(f"{MSBUILD}Configuration", "")
        platform = entry.findtext(f"{MSBUILD}Platform", "")
        require(pair(name) == (config, platform), f"Invalid ProjectConfiguration: {name}")
        insert(configs, name, True, "project configuration")
    require(bool(configs), "Missing project configuration matrix")

    order = []
    labels = ("Configuration", "ItemDefinitionGroup")
    configured = {label: set() for label in labels}
    for child in root:
        child_tag = tag(child)
        label = child.get("Label", "")
        name = label or child_tag
        if child_tag == "Import":
            name = PurePosixPath(path_text(child.get("Project", ""))).name
        elif child_tag == "ItemGroup" and not label:
            name = "Items"
        elif child_tag == "PropertyGroup" and child.find(f"{MSBUILD}_ProjectFileVersion") is not None:
            name = "General"
        if name in ORDER:
            order.append(ORDER[name])
        if name in configured:
            condition = child.get("Condition", "")
            match = re.fullmatch(r"'\$\(Configuration\)\|\$\(Platform\)'\s*==\s*'([^']+)'", condition)
            require(match is not None, f"Invalid {name} condition: {condition!r}")
            config = unquote(match[1])
            require(config not in configured[name], f"Duplicate {name} condition: {config}")
            configured[name].add(config)
    require(order == sorted(order), "MSBuild project elements are out of order")
    for name in ("ProjectConfigurations", "Globals", "Microsoft.Cpp.Default.props", "Microsoft.Cpp.props", "Microsoft.Cpp.targets"):
        require(order.count(ORDER[name]) == 1, f"Expected one {name} element")
    for name, matrix in configured.items():
        require(matrix == configs.keys(), f"Incomplete {name} configuration matrix")

    refs = {}
    for reference in root.findall(f"./*/{MSBUILD}ProjectReference"):
        target = unquote(reference.get("Include", ""))
        require(bool(target), "Missing ProjectReference Include")
        identifier_ref = guid(reference.findtext(f"{MSBUILD}Project"))
        resolved = local_path(path, target)
        require(resolved != path, "Project reference points to itself")
        existing = refs.setdefault(resolved, identifier_ref)
        require(existing == identifier_ref, f"Conflicting ProjectReference GUID: {target}")
    items = {(tag(item), path_text(unquote(item.get("Include", ""))))
             for group in root.findall(f"./{MSBUILD}ItemGroup")
             for item in group
             if tag(item) not in ("ProjectConfiguration", "ProjectReference")}
    return {"guid": identifier, "configs": set(configs), "refs": refs, "items": items}


def filters_xml(root: ET.Element) -> set:
    groups = {}
    ids = set()
    items = {}
    for item in root.findall(f"./{MSBUILD}ItemGroup/*"):
        include = path_text(unquote(item.get("Include", "")))
        require(include != ".", f"Missing Include for {tag(item)}")
        if tag(item) == "Filter":
            identifier = guid(item.findtext(f"{MSBUILD}UniqueIdentifier"))
            require(identifier not in ids, f"Duplicate filter GUID: {identifier}")
            ids.add(identifier)
            insert(groups, include, identifier, "filter")
        else:
            group = item.findtext(f"{MSBUILD}Filter")
            insert(items, (tag(item), include), path_text(unquote(group)) if group else None, "filter item")
    for group in groups:
        parent = posixpath.dirname(group)
        require(not parent or parent in groups, f"Missing parent filter: {parent}")
    for group in items.values():
        require(group is None or group in groups, f"Unknown filter: {group}")
    return set(items)


def sln_text(path: Path) -> tuple[list[str], dict]:
    lines = [line.strip() for line in path.read_text(encoding="utf-8-sig").splitlines() if line.strip()]
    require(lines[:2] == ["Microsoft Visual Studio Solution File, Format Version 12.00",
                          "# Visual Studio Version 17"], "Invalid Visual Studio 17 solution header")
    require(lines.count("Global") == 1 and lines[-1] == "EndGlobal", "Invalid Global block")
    entries = {}
    dependencies = {}
    sections = {}
    current_project = None
    section = None
    in_dependencies = False
    normalized = []
    for line in lines:
        if match := SLN_PROJECT.fullmatch(line):
            kind, name, project_path, value = match.groups()
            identifier = guid(value)
            require(current_project is None, "Missing EndProject")
            current_project = identifier
            relative = path_text(project_path.replace('""', '"'))
            insert(entries, identifier, None if guid(kind) == FOLDER_TYPE else local_path(path, relative), "solution GUID")
            if entries[identifier] is not None:
                line = f'Project("{kind}") = "{name}", "{relative.replace(chr(34), chr(34) * 2)}", "{value}"'
        elif line == "EndProject":
            require(current_project is not None and not in_dependencies, "Unexpected EndProject")
            current_project = None
        elif line == "ProjectSection(ProjectDependencies) = postProject":
            require(current_project is not None and not in_dependencies, "Unexpected dependency section")
            insert(dependencies, current_project, set(), "dependency section")
            in_dependencies = True
        elif line == "EndProjectSection":
            require(in_dependencies, "Unexpected EndProjectSection")
            in_dependencies = False
        elif match := SECTION.fullmatch(line):
            require(section is None, "Missing EndGlobalSection")
            section = match[1]
            insert(sections, section, {}, "global section")
        elif line == "EndGlobalSection":
            require(section is not None, "Unexpected EndGlobalSection")
            section = None
        elif in_dependencies or section:
            key, separator, value = line.partition(" = ")
            require(bool(separator), f"Invalid section entry: {line}")
            if in_dependencies:
                require(guid(key) == guid(value), f"Invalid dependency: {line}")
                dependencies[current_project].add(guid(key))
            else:
                insert(sections[section], key, value, "section entry")
        normalized.append(line)
    require(current_project is None and section is None, "Unclosed solution section")
    projects = {key: value for key, value in entries.items() if value is not None}
    require(len(set(projects.values())) == len(projects), "Duplicate solution project path")
    for project, deps in dependencies.items():
        require(project in projects and deps <= projects.keys() and project not in deps,
                f"Invalid solution dependencies for {project}")
    nested = {guid(child): guid(parent) for child, parent in sections.get("NestedProjects", {}).items()}
    for child, parent in nested.items():
        require(child in entries and parent in entries and entries[parent] is None, "Invalid nested project")
        visited = {child}
        while parent in nested:
            require(parent not in visited, "Circular solution folder")
            visited.add(parent)
            parent = nested[parent]
    solution_guid = sections.get("ExtensibilityGlobals", {}).get("SolutionGuid")
    if solution_guid:
        require(guid(solution_guid) not in entries, "Solution GUID duplicates a project GUID")

    configs = sections.get("SolutionConfigurationPlatforms", {})
    require(bool(configs), "Missing solution configuration matrix")
    for name, mapped in configs.items():
        require(pair(name) == pair(mapped), f"Invalid solution configuration: {name}")
    matrix = sections.get("ProjectConfigurationPlatforms", {})
    mappings = {key: {} for key in projects}
    for name, mapped in matrix.items():
        identifier, _, key = name.partition(".")
        identifier = guid(identifier)
        require(identifier in projects, f"Unknown project in configuration matrix: {identifier}")
        pair(mapped)
        suffix = next((suffix for suffix in (".ActiveCfg", ".Build.0", ".Deploy.0")
                       if key.endswith(suffix)), None)
        require(suffix is not None, f"Invalid project configuration entry: {name}")
        config = key.removesuffix(suffix)
        require(config in configs, f"Unknown solution configuration: {config}")
        if suffix == ".ActiveCfg":
            insert(mappings[identifier], config, mapped, "active configuration")
    require(all(mapped.keys() == configs.keys() for mapped in mappings.values()),
            "Incomplete project configuration matrix")
    return normalized, {
        file: (identifier, set(mappings[identifier].values()))
        for identifier, file in projects.items()
    }


def slnx_xml(path: Path, root: ET.Element) -> dict:
    configs = [element.get("Name", "") for element in root.findall("./Configurations/BuildType")]
    platforms = [element.get("Name", "") for element in root.findall("./Configurations/Platform")]
    for axis in (configs, platforms):
        require(bool(axis) and all(axis) and len(set(axis)) == len(axis), "Invalid solution configuration axis")
    matrix = {f"{config}|{platform}" for config in configs for platform in platforms}
    ids = set()
    folders = set()
    for folder in root.findall("./Folder"):
        name = folder.get("Name", "")
        require(name.startswith("/") and name.endswith("/") and name not in folders, f"Invalid folder: {name!r}")
        folders.add(name)
    for entry in root.iter():
        if entry.tag in ("Folder", "Project") and entry.get("Id"):
            identifier = guid(entry.get("Id"))
            require(identifier not in ids, f"Duplicate solution GUID: {identifier}")
            ids.add(identifier)

    projects = {}
    dependencies = {}
    for project in root.iter("Project"):
        value = project.get("Path", "")
        require(bool(value), "Missing solution project path")
        project_path = local_path(path, value)
        active = {config: config for config in matrix}
        for rule in project:
            if rule.tag in ("BuildType", "Platform", "Build", "Deploy"):
                configuration, platform = pair(rule.get("Solution", "*|*"))
                require(configuration in configs or configuration == "*", f"Unknown configuration: {configuration}")
                require(platform in platforms or platform == "*", f"Unknown platform: {platform}")
                mapped = rule.get("Project", "")
                require(bool(mapped), f"Missing {rule.tag} mapping")
                if rule.tag in ("BuildType", "Platform") and mapped != "*":
                    for key in active:
                        current_config, current_platform = pair(key)
                        if configuration in ("*", current_config) and platform in ("*", current_platform):
                            x, y = pair(active[key])
                            active[key] = f"{mapped}|{y}" if rule.tag == "BuildType" else f"{x}|{mapped}"
        insert(projects, project_path, (guid(project.get("Id")), set(active.values())), "solution project path")
        dependencies[project_path] = {local_path(path, dep.get("Project", "")) for dep in project.findall("BuildDependency")}
    for project, deps in dependencies.items():
        require(deps <= projects.keys() and project not in deps, f"Invalid solution dependencies: {project.name}")
    return projects


def canonical_xml(element: ET.Element) -> dict:
    attrs = dict(element.attrib)
    for name in ("Path", "Include", "Project"):
        if name in attrs and (name != "Project" or element.tag in ("BuildDependency", f"{MSBUILD}Import")):
            if tag(element) not in ("ProjectConfiguration",):
                attrs[name] = path_text(attrs[name])
    text = element.text if element.text and element.text.strip() else None
    if text and tag(element) in ("Filter", "OutDir", "IntDir", "ObjectFileName"):
        text = path_text(text)
    return {"tag": tag(element), "attributes": attrs, "text": text,
            "children": [canonical_xml(child) for child in element]}


def validate(paths: list[Path], base: Path) -> dict:
    documents = {}
    projects = {}
    solutions = {}
    filters = {}
    for path in paths:
        try:
            if path.suffix.lower() == ".sln":
                content, solutions[path] = sln_text(path)
            else:
                root = xml_file(path, "Solution" if path.suffix.lower() == ".slnx" else f"{MSBUILD}Project")
                content = canonical_xml(root)
                if path.suffix.lower() == ".slnx":
                    solutions[path] = slnx_xml(path, root)
                elif path.name.lower().endswith(".vcxproj.filters"):
                    filters[path] = filters_xml(root)
                else:
                    projects[path] = project_xml(path, root)
            documents[posixpath.relpath(path, base)] = content
        except (ValueError, ET.ParseError, OSError) as error:
            raise ValueError(f"{path}: {error}") from error

    def load(path: Path) -> dict:
        if path not in projects:
            projects[path] = project_xml(path, xml_file(path, f"{MSBUILD}Project"))
        return projects[path]

    for solution, members in solutions.items():
        for path, (identifier, configs) in members.items():
            try:
                project = load(path)
                require(project["guid"] == identifier, "Solution and project GUIDs differ")
                require(configs <= project["configs"], "Solution refers to a missing project configuration")
                require(project["refs"].keys() <= members.keys(), "ProjectReference is outside the solution")
            except (ValueError, ET.ParseError, OSError) as error:
                raise ValueError(f"{solution}: {path}: {error}") from error
    for path, items in filters.items():
        project = load(path.with_suffix(""))
        require(items <= project["items"], f"{path}: Filter refers to an item outside the project")
    pending = list(projects)
    visited = set()
    while pending:
        path = pending.pop()
        if path in visited:
            continue
        visited.add(path)
        for target, identifier in projects[path]["refs"].items():
            require(load(target)["guid"] == identifier, f"{path}: Incorrect ProjectReference GUID for {target}")
            pending.append(target)
    return documents


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("paths", nargs="+", type=Path, help="Solution files, project files, or build directories")
    parser.add_argument("--check", action="store_true", help="Report the number of files that pass validation")
    options = parser.parse_args()
    try:
        files = set()
        for path in options.paths:
            candidates = path.rglob("*") if path.is_dir() else [path]
            found = {file.resolve() for file in candidates if file.is_file()
                     and file.name.lower().endswith(SUFFIXES) and not file.name.startswith("._")}
            require(bool(found), f"No Visual Studio files at {path}")
            files.update(found)
        base = options.paths[0].resolve()
        documents = validate(sorted(files), base if base.is_dir() else base.parent)
    except (ValueError, OSError, ET.ParseError) as error:
        print(error, file=sys.stderr)
        return 1
    if options.check:
        print(f"Validated {len(documents)} Visual Studio files.")
    else:
        print(json.dumps(documents, indent=2, sort_keys=True, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
