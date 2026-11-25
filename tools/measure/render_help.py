#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Render CMake 4.5.1 help for the standalone Rcmake binary."""

import argparse
import concurrent.futures
from pathlib import Path
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("cmake", type=Path)
    parser.add_argument("help_source", type=Path, help="CMake's installed Help directory")
    args = parser.parse_args()
    cmake = args.cmake.resolve()
    repo = Path(__file__).resolve().parents[2]
    usage_out = repo / "crates/rcmake-cli/help/usage.txt"
    reference_out = repo / "modules/cmake-4.5/Help/reference.zst"
    reference_out.parent.mkdir(parents=True, exist_ok=True)

    def run(*flags):
        return subprocess.check_output([cmake, *flags], cwd=work, timeout=15)

    with tempfile.TemporaryDirectory() as work:
        if run("--version").splitlines()[0] != b"cmake version 4.5.1":
            parser.error("Use CMake 4.5.1 to render the help.")
        usage = run("--help").split(b"\nGenerators\n", 1)[0]
        usage_out.write_bytes(usage)

        entries = [(("full", ""), ("--help-full",))]
        for category in ("command", "diagnostic", "manual", "module", "policy", "property", "variable"):
            flag = f"--help-{category}"
            list_flag = f"{flag}-list"
            entries.append(((f"{category}-list", ""), (list_flag,)))
            for name in run(list_flag).decode().splitlines():
                if category == "manual":
                    name = name[:-3] + "." + name[-2]
                entries.append(((category, name), (flag, name)))

        def keyword(path):
            name = path.stem
            if path.parent.name.startswith("prop_") or path.parent.name in ("variable", "envvar"):
                return name.replace("LANG", "<LANG>").replace("CONFIG", "<CONFIG>").replace("TYPE", "<TYPE>").replace("FEATURE", "<FEATURE>")
            return name

        generic = {keyword(path) for path in args.help_source.glob("*/*.rst")}
        generic.update(name for (_, name), _ in entries if name)
        entries.extend((("keyword", name), ("--help", name)) for name in generic)
        entries.sort(key=lambda entry: (entry[0][1], entry[0][0]))

        def render(entry):
            (category, name), flags = entry
            # Some files serve only as includes. They have no standalone help.
            result = subprocess.run(
                [cmake, *flags], cwd=work, timeout=15, stdout=subprocess.PIPE, check=False
            )
            if result.returncode and category == "keyword":
                return b""
            result.check_returncode()
            content = result.stdout
            key = f"{category}\t{name}".encode()
            return key + b"\0" + len(content).to_bytes(4, "little") + content

        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            data = b"".join(pool.map(render, dict.fromkeys(entries)))
        subprocess.run(
            ["zstd", "-q", "-T2", "-19", "--long=25", "-f", "-o", reference_out],
            input=data, timeout=120, check=True,
        )


if __name__ == "__main__":
    main()
