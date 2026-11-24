<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
# Ecosystem compatibility fixtures

Run the fixtures with CMake 4.5.1 and a staged Rcmake executable.

~~~sh
source env/env.sh
python3 tools/corpus/ecosystem_eval.py \
  --stage /path/to/stage \
  --oracle "$RCMAKE_ORACLE_CMAKE" \
  --out-json /tmp/rcmake-ecosystem.json \
  --out-md /tmp/rcmake-ecosystem.md
~~~

The report records the PATH lookups, the version checks, the result of each
command and the file comparison. Look next to the JSON report for the
execution logs and unified diffs. The report shows
`skipped_missing_tool` for scenarios that require a tool the harness could
not find. The package manager and wheel scenarios can download dependencies.

Use `VCPKG_ROOT` to specify the vcpkg checkout if the executable is outside
that checkout. Use `--rust-cmake-driver /path/to/driver` to run an existing
driver that uses the Rust `cmake` crate. The harness starts the driver in a
copy of `recursive-driver/sub`. The driver can use `cmake::Config::new(".")`
with an output directory in the working directory. The harness uses the
staged binary and does not build Rcmake.

## Compare a project with its own commands

Use `tools/corpus/pathswap.py` with `--stage`, `--oracle`, `--out-json`,
`--source` and `--commands`. The commands file contains a JSON array. For
example, use these commands for the recursive fixture.

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

The harness passes each command string to `sh -c` as written. It starts each
command in the project copy. Put shell commands that share shell state in the
same stage. For example, put `cd build && ctest` in one stage.

The `expect` field lists the tool personas that must appear in Rcmake's
execution log for the stage. It defaults to `["cmake"]`. Use an empty list
for commands that do not need to invoke a CMake tool. A stage that needs an
execution record fails proof when the record is missing, even if the command
returns zero.

Use `--configured-output build/version.h` to include the generated header.
Repeat the option to select more files by project-relative glob. The harness
compares the known generated file classes automatically.
