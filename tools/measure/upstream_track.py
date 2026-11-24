#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Compare upstream CMake with the Rcmake 4.5.1 baseline.

The default mode checks stable release tags and the upstream default branch.
Use --offline-dir with an extracted CMake source tree to skip all network calls.
Exit status 1 means a delta was found with --exit-on-delta. Status 2 means an error.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tomllib
from urllib.request import Request, urlopen

import runcmake_eval


ROOT = Path(__file__).resolve().parents[2]
BASELINE = (4, 5, 1)
VENDORED = ROOT / "modules/cmake-4.5/Modules"
POLICY_TABLE = ROOT / "crates/rcmake-diag/src/generated_policies.rs"
SUITE_MANIFEST = ROOT / "conformance/runcmake/manifest.toml"
UPSTREAM = "https://github.com/Kitware/CMake.git"
TREE_API = "https://api.github.com/repos/Kitware/CMake/git/trees"
RELEASE = re.compile(r"v?(\d+)\.(\d+)\.(\d+)")
POLICY_DOC = re.compile(r"CMP\d{4}\.rst")
SUITE_DRIVER = re.compile(r"Tests/RunCMake/([^/]+)/RunCMakeTest\.cmake")


def version_text(version):
    """Format a version from its numeric components."""
    return ".".join(map(str, version))


def local_version(source):
    """Read the source version and any release candidate suffix."""
    contents = (source / "Source/CMakeVersion.cmake").read_text(encoding="utf-8")
    parts = dict(re.findall(r"(?m)^\s*set\(CMake_VERSION_(MAJOR|MINOR|PATCH|RC) (\d+)\)", contents))
    version = tuple(int(parts[key]) for key in ("MAJOR", "MINOR", "PATCH"))
    stable = "RC" not in parts and version[2] < 20_000_000
    suffix = f"-rc{parts['RC']}" if "RC" in parts else ""
    return version, version_text(version) + suffix, stable


def local_modules(directory):
    """Map module paths to Git blob hashes and file types."""
    if not directory.is_dir():
        raise ValueError(f"Missing Modules directory: {directory}")
    modules = {}
    for path in directory.rglob("*"):
        if path.name.startswith("._") or path.name == ".DS_Store":
            continue
        if path.is_symlink():
            data = os.fsencode(os.readlink(path))
            kind = "symlink"
        elif path.is_file():
            data = path.read_bytes()
            kind = "file"
            if path.name.endswith((".bat", ".bat.in")):
                # Source archives use CRLF for batch files. Git stores LF.
                data = data.replace(b"\r\n", b"\n")
        else:
            continue
        header = f"blob {len(data)}\0".encode("ascii")
        modules[path.relative_to(directory).as_posix()] = (
            kind, hashlib.sha1(header + data, usedforsecurity=False).hexdigest(),
        )
    return modules


def suites(paths):
    """Find suites with a RunCMake test driver."""
    return {match[1] for path in paths if (match := SUITE_DRIVER.fullmatch(path))}


def local_suites(source):
    """Read all suite names from a CMake source tree."""
    directory = source / "Tests/RunCMake"
    if not directory.is_dir():
        raise ValueError(f"Missing RunCMake directory: {directory}")
    return suites(path.relative_to(source).as_posix()
                  for path in directory.glob("*/RunCMakeTest.cmake") if path.is_file())


def baseline_suites():
    """Read the explicit suite manifest or the evaluator's current profiles.

    The optional TOML manifest contains a top-level array named suites.
    """
    if SUITE_MANIFEST.is_file():
        with SUITE_MANIFEST.open("rb") as manifest:
            return set(tomllib.load(manifest)["suites"]), str(SUITE_MANIFEST.relative_to(ROOT))
    return (
        set(runcmake_eval.SUITE_GROUPS["all"] + runcmake_eval.WIDE_SUITES),
        "tools/measure/runcmake_eval.py (core and wide profiles)",
    )


def baseline_policies():
    """Read the policy IDs from the generated registry."""
    contents = POLICY_TABLE.read_text(encoding="utf-8")
    ids = {int(number) for number in re.findall(r"id:\s*PolicyId::new\((\d+)\)", contents)}
    maximum = re.search(r"MAX_POLICY_ID:\s*u16\s*=\s*(\d+)", contents)
    if not maximum or ids != set(range(int(maximum[1]) + 1)):
        raise ValueError("The generated policy registry has missing policy IDs.")
    return {f"CMP{number:04d}" for number in ids}


def policies(paths):
    """Find policy IDs from upstream policy file names."""
    return {Path(path).stem for path in paths if POLICY_DOC.fullmatch(Path(path).name)}


def remote_refs(timeout):
    """Read stable tags and the default branch commit from upstream."""
    listing = subprocess.run(
        ["git", "ls-remote", UPSTREAM, "HEAD", "refs/tags/v*"],
        check=True, capture_output=True, text=True, timeout=timeout,
        env={**os.environ, "GIT_TERMINAL_PROMPT": "0"},
    )
    refs = {ref: sha for sha, ref in (line.split() for line in listing.stdout.splitlines())}
    baseline_ref = f"refs/tags/v{version_text(BASELINE)}"
    return (
        sorted(tuple(map(int, match.groups())) for ref in refs
               if (match := RELEASE.fullmatch(ref.removeprefix("refs/tags/")))),
        refs["HEAD"],
        refs.get(f"{baseline_ref}^{{}}") or refs[baseline_ref],
    )


def remote_tree(ref, timeout):
    """Get and validate the complete GitHub tree for a commit."""
    headers = {
        "Accept": "application/vnd.github+json",
        "User-Agent": "Rcmake-upstream-tracker",
    }
    if token := os.environ.get("GH_TOKEN"):
        headers["Authorization"] = f"Bearer {token}"
    request = Request(f"{TREE_API}/{ref}?recursive=1", headers=headers)
    limit = 32 * 1024 * 1024
    with urlopen(request, timeout=timeout) as response:
        payload = response.read(limit + 1)
    if len(payload) > limit:
        raise ValueError(f"The GitHub tree for {ref} exceeds {limit} bytes.")
    data = json.loads(payload)
    if data["truncated"]:
        raise ValueError(f"GitHub returned an incomplete tree for {ref}.")
    return {entry["path"]: entry for entry in data["tree"] if entry["type"] == "blob"}


def offline_baseline(source):
    """Find the pinned source tree for offline suite comparison."""
    candidates = [source, Path(os.environ.get("RCMAKE_ORACLE_SRC", source)),
                  source.with_name(f"cmake-{version_text(BASELINE)}")]
    for candidate in candidates:
        if (candidate / "Source/CMakeVersion.cmake").is_file():
            version, _, stable = local_version(candidate)
            if version == BASELINE and stable:
                return candidate
    raise ValueError("Set RCMAKE_ORACLE_SRC to the pinned CMake 4.5.1 source for offline comparison.")


def upstream_snapshot(offline, timeout):
    """Read one upstream snapshot and the pinned upstream suite inventory."""
    if offline is not None:
        version, label, stable = local_version(offline)
        snapshots = [version] if stable else []
        policy_dir = offline / "Help/policy"
        if not policy_dir.is_dir():
            raise ValueError(f"Missing policy documentation: {policy_dir}")
        return (
            f"local CMake {label}", snapshots,
            local_modules(offline / "Modules"), policies(path.name for path in policy_dir.glob("*.rst")),
            local_suites(offline), local_suites(offline_baseline(offline)),
        )

    releases, head, baseline = remote_refs(timeout)
    tree = remote_tree(head, timeout)
    modules = {
        path.removeprefix("Modules/"): (
            "symlink" if entry["mode"] == "120000" else "file", entry["sha"],
        )
        for path, entry in tree.items() if path.startswith("Modules/")
    }
    return (
        f"Kitware/CMake HEAD at {head}", releases, modules,
        policies(path for path in tree if path.startswith("Help/policy/")),
        suites(tree), suites(remote_tree(baseline, timeout)),
    )


def compare(offline, timeout):
    """Compare the upstream snapshot with the pinned repository baseline."""
    known_policies = baseline_policies()
    known_modules = local_modules(VENDORED)
    known_suites, suite_source = baseline_suites()
    source, releases, modules, policy_ids, suite_ids, old_suites = upstream_snapshot(offline, timeout)
    if not all((known_modules, known_suites, modules, policy_ids, suite_ids, old_suites)):
        raise ValueError("A required module, policy, or suite inventory is empty.")
    new_releases = [version_text(version) for version in releases if version > BASELINE]
    new_policies = sorted(policy_ids - known_policies)
    delta = {
        "releases": new_releases,
        "policies": new_policies,
        "modules_added": sorted(modules.keys() - known_modules.keys()),
        "modules_changed": sorted(path for path in modules.keys() & known_modules.keys()
                                  if modules[path] != known_modules[path]),
        "modules_removed": sorted(known_modules.keys() - modules.keys()),
        "runcmake_suites_added": sorted(suite_ids - old_suites - known_suites),
    }
    return {
        "baseline": {
            "version": version_text(BASELINE),
            "modules": str(VENDORED.relative_to(ROOT)),
            "policies": str(POLICY_TABLE.relative_to(ROOT)),
            "runcmake_suites": suite_source,
            "module_count": len(known_modules),
            "policy_count": len(known_policies),
            "runcmake_suite_count": len(old_suites),
            "tracked_runcmake_suite_count": len(known_suites),
        },
        "upstream": {
            "source": source,
            "module_count": len(modules),
            "policy_count": len(policy_ids),
            "runcmake_suite_count": len(suite_ids),
        },
        "delta": delta,
        "has_delta": any(delta.values()),
        "issue_required": bool(new_releases or new_policies),
    }


def markdown(report):
    """Show counts and list the entries in each delta."""
    baseline, upstream = report["baseline"], report["upstream"]
    lines = [
        "# Upstream CMake report",
        "",
        f"Baseline: CMake {baseline['version']}. Source: {upstream['source']}.",
        "",
        "| Inventory | Baseline | Upstream |",
        "| --- | ---: | ---: |",
        f"| Modules | {baseline['module_count']} | {upstream['module_count']} |",
        f"| Policies | {baseline['policy_count']} | {upstream['policy_count']} |",
        f"| RunCMake suites | {baseline['runcmake_suite_count']} | {upstream['runcmake_suite_count']} |",
        "",
        f"Rcmake tracks {baseline['tracked_runcmake_suite_count']} explicit RunCMake suites "
        f"using {baseline['runcmake_suites']}.",
    ]
    if report["has_delta"]:
        for key, values in report["delta"].items():
            if values:
                title = key.replace("runcmake", "RunCMake").replace("_", " ")
                lines += ["", f"## {title[0].upper()}{title[1:]} ({len(values)})", "",
                          *(f"- `{value}`" for value in values)]
    else:
        lines += ["", "No upstream changes found."]
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--offline-dir", type=Path, help="Read a local CMake source tree without network access.")
    parser.add_argument("--out-json", type=Path, metavar="OUT")
    parser.add_argument("--out-md", type=Path, metavar="OUT")
    parser.add_argument("--exit-on-delta", action="store_true", help="Exit with status 1 if any delta is present.")
    parser.add_argument("--timeout", type=int, default=60, help="Timeout in seconds for network operations.")
    args = parser.parse_args()
    if args.timeout < 1:
        parser.error("Use a positive timeout.")
    offline = args.offline_dir
    if (offline and not offline.exists()
            and offline.resolve() == ROOT / f"env/oracle/cmake-{version_text(BASELINE)}"
            and (shared_source := os.environ.get("RCMAKE_ORACLE_SRC"))):
        offline = Path(shared_source)
    try:
        report = compare(offline, args.timeout)
        output = markdown(report)
        if args.out_json:
            args.out_json.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        if args.out_md:
            args.out_md.write_text(output, encoding="utf-8")
        print(output, end="")
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as exc:
        print(f"Upstream tracking failed. {exc}", file=sys.stderr)
        return 2
    return int(args.exit_on_delta and report["has_delta"])


if __name__ == "__main__":
    raise SystemExit(main())
