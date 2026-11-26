<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
# Ecosystem compatibility fixtures

These fixtures compare a staged `rcmake` binary against CMake 4.5.1 across real build-tool workflows. Run the suite with `tools/corpus/ecosystem_eval.py`.

~~~sh
source env/env.sh
python3 tools/corpus/ecosystem_eval.py \
  --stage /path/to/stage \
  --oracle "$RCMAKE_ORACLE_CMAKE" \
  --out-json /tmp/rcmake-ecosystem.json \
  --out-md /tmp/rcmake-ecosystem.md
~~~

The report records `PATH` lookups, version checks, command results, and file comparisons. Execution logs and unified diffs sit next to the JSON report. If a scenario needs a tool that is not installed, the report marks it as `skipped_missing_tool`. The package manager and Python wheel scenarios can download dependencies over the network.

Set `VCPKG_ROOT` to your vcpkg checkout if the `vcpkg` executable lives outside that checkout. Pass `--rust-cmake-driver /path/to/driver` to run an existing driver built with the Rust `cmake` crate. The script starts that driver inside a copy of `recursive-driver/sub`, where the driver can call `cmake::Config::new(".")` and write its output inside the working directory. The runner uses the staged binary directly and does not build `rcmake`.

## Compare a project with its own commands

To compare a single project using custom commands, run `tools/corpus/pathswap.py` with `--stage`, `--oracle`, `--out-json`, `--source`, and `--commands`. The commands file holds a JSON array of stages, like this example for the recursive fixture.

~~~json
[
  {"name": "configure", "command": "cmake -S . -B build -G Ninja"},
  {"name": "build", "command": ["cmake", "--build", "build"]},
  {"name": "test", "command": "ctest --test-dir build --no-tests=error",
   "expect": ["ctest", "cmake", "cpack"]},
  {"name": "install", "command": "cmake --install build --prefix install"},
  {"name": "package", "command": "cpack --config build/CPackConfig.cmake",
   "expect": ["cpack"]}
]
~~~

`pathswap.py` passes each string command to `sh -c` verbatim and starts each stage inside the project copy. Put shell commands that share shell state into the same stage, such as `cd build && ctest`.

The `expect` field lists the tool names, such as `cmake`, `ctest`, or `cpack`, that must appear in the `rcmake` execution log for that stage. It defaults to `["cmake"]`. Pass an empty list for stages that do not invoke a CMake tool. If an expected execution record is missing, the stage fails the check even when the command exits with code 0.

Pass `--configured-output build/version.h` to compare a generated header, and repeat the flag to include more files by project-relative glob. `pathswap.py` compares known generated file classes automatically.
