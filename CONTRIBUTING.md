# Contributing to rcmake

Thank you for your interest in rcmake. This guide gives the rules and checks
for every change. [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) describes the
crate structure and the data flow.

## Goal of the project

rcmake must behave as CMake 4.5.1 behaves. The official CMake 4.5.1 binary is
the reference (the "oracle"). When rcmake and CMake 4.5.1 give different
results, rcmake has the bug, unless CMake 4.5.1 itself is nondeterministic or
crashes.

## Set up

1. Install the Rust toolchain from `rust-toolchain.toml` (Rust 1.99.1 with
   `rustfmt` and `clippy`). `rustup` installs it automatically.
2. Install Python 3.11 or later for the tools in `tools/`.
3. Install Ninja to run the generator and corpus tests.
4. Load the environment in each new shell:

   ```sh
   source env/env.sh
   ```

5. Download the CMake 4.5.1 oracle for the differential tests:

   ```sh
   scripts/fetch-oracle.sh
   ```

`env/env.sh` sets `RCMAKE_ORACLE_CMAKE`, `RCMAKE_ORACLE_SRC`, and the other
oracle variables. It also sets default limits of 2 Cargo build jobs and 3 test
threads, which keep memory use low. To build faster on a machine with more
memory, set larger `CARGO_BUILD_JOBS` and `RUST_TEST_THREADS` values before you
source the file.

## First contribution in 2 minutes

This path takes you from a clone to a pull request. It assumes that you
finished [Set up](#set-up).

1. Build `rcmake` and stage it with the bundled CMake modules:

   ```sh
   source env/env.sh
   scripts/stage.sh
   ```

   The staged tree is in `target/stage`. `target/stage/bin/cmake` is a link to
   `rcmake`.

2. Run one unit test. Give the crate with `-p` and a part of the test name:

   ```sh
   cargo test -p rcmake-commands search_replace_and_join
   ```

3. Find the code for a CMake command. Each command family is a module in
   `crates/rcmake-commands/src/`, for example `string_math/` for `string()` and
   `math()`, and `file_path/` for `file()` and `cmake_path()`. The `register()`
   function of the family adds each command name:

   ```rust
   registry.add("cmake_path", path_cmd::run, flags);
   ```

4. Compare one behavior with CMake 4.5.1. Write a small script, run it with
   both binaries, and compare the output:

   ```sh
   printf 'string(TOUPPER "abc" out)\nmessage(STATUS "${out}")\n' > probe.cmake
   "$RCMAKE_ORACLE_CMAKE" -P probe.cmake > oracle.txt 2>&1
   target/stage/bin/cmake -P probe.cmake > rcmake.txt 2>&1
   diff oracle.txt rcmake.txt
   ```

   No output from `diff` means that the two tools agree.

5. Run a recorded differential suite. The suite name is the file name in
   `conformance/cases/` without `.case`. `--filter` selects one case:

   ```sh
   cargo run --bin rcmake-harness -- --suite string_math --filter string-combine
   ```

6. Run the [quality gates](#quality-gates), then open a pull request. The pull
   request template asks which CMake 4.5.1 behavior you matched and how you
   checked it.

### Add or change a CMake command

1. Write or change the handler in the command family module, for example
   `crates/rcmake-commands/src/string_math/`. A handler is a
   `rcmake_eval::BuiltinFn`: `fn(&mut Cx<'_>, &[ExpandedArg]) -> CmdResult`.
2. For a new command, call `registry.add` in the `register()` function of the
   family. Set `CmdFlags { scriptable: true }` if the command works in
   `cmake -P` scripts.
3. For a new command, add its name to the `PROJECT` list in
   `crates/rcmake-commands/src/registry_tests.rs`. If it does not work in
   scripts, also add it to `PROJECT_ONLY`.
4. Add a unit test to the `tests.rs` file of the family. Use `PureHost`.
5. Add a case to the matching `conformance/cases/<suite>.case` file. Record its
   baseline from the oracle with
   `cargo run --bin rcmake-harness -- --suite <suite> --record`.

## Clean-room rules

- Write all Rust code in `crates/` and `tools/` from scratch. Do not copy,
  translate, or adapt CMake C++ source code.
- You can read the CMake documentation and run the CMake 4.5.1 binary to
  find the correct behavior. Record the behavior in a test, not in copied code.
- The CMake 4.5.1 files in `modules/cmake-4.5/` stay under `BSD-3-Clause`.
  Do not remove or change their copyright headers.
- Do not add files under GPL, LGPL, AGPL, MPL, or other copyleft licences.
- rcmake never runs the upstream `cmake` binary at runtime. Only
  `rcmake-harness`, `tools/`, and the differential tests use
  `RCMAKE_ORACLE_CMAKE`. The licence scanner checks this rule.

## Engineering rules

### No unsafe code

The workspace sets `unsafe_code = "forbid"`, and each crate root has
`#![forbid(unsafe_code)]`. A change that needs operating system access
through FFI (foreign function interface) needs a new decision record in
[`docs/ADR.md`](docs/ADR.md) first.

### Treat all input as untrusted

`CMakeLists.txt` files, cache files, presets, environment variables, archives,
and process output can contain any bytes. On any path that reads such input:

- Do not use `unwrap()`, `expect()`, `panic!()`, `todo!()`, or
  `unimplemented!()`. Return an error.
- Do not index slices directly. Use `get()` and handle `None`.
- Store CMake values as byte strings (`BString`). Do not assume UTF-8.
- Apply the limits in `rcmake_base::Limits` to recursion depth, variable
  expansion, loop count, regex steps, and file size.

The Clippy configuration reports these problems. Tests can use `unwrap()` and
`expect()`.

### Use the `Host` trait for system access

The evaluator, the expression engines, the presets loader, and the generators
must not call `std::fs`, `std::env`, `std::process`, or the system clock
directly. Use the `rcmake_host::Host` trait:

| Need | `Host` method |
| --- | --- |
| Files and directories | `read`, `write`, `metadata`, `read_dir`, and related methods |
| Environment | `env_get`, `env_all` |
| Processes | `run`, `run_bounded` |
| Time | `now`, `monotonic` |

`NativeHost` uses the real system. `PureHost` is an in-memory host for fast,
deterministic tests. `clippy.toml` bans the direct `std` calls. Only
`rcmake-host`, the `rcmake-cli` entry point, and `rcmake-harness` can use them.

All paths that you give to `Host` must be absolute. Do not use or change the
process working directory.

### Implement general rules

Implement the general CMake 4.5.1 rule. Do not add a special case for one
project, one test, or one file name. If a test needs a special case to pass,
find the rule that CMake applies.

### Document public items

Every public item needs a doc comment. The workspace sets
`missing_docs = "warn"`, and the gates treat warnings as errors.

## Quality gates

Run these checks before you open a pull request. All of them must pass.

```sh
source env/env.sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo test --workspace
python3 tools/measure/licence_scan.py --check-hygiene
```

`scripts/gate.sh` runs all of these checks and the recorded CLI and script cases.

If you change `rcmake-gen-xcode` or `rcmake-gen-vs`, also run Clippy with
the cross-platform generators:

```sh
cargo clippy -p rcmake-cli --features vs-cross,xcode-cross --all-targets -- -D warnings
```

The licence scanner checks these rules:

- Each dependency licence is on the allowlist.
- No crate outside `rcmake-harness` reads the oracle variables.
- No tracked text file contains an absolute path into a user home directory.
  Use placeholders such as `<path>` or `$HOME` in examples.
- Only files that start with `#!` are executable.
- No `._*` or `.DS_Store` file is tracked.

## Add tests

Every change in behavior needs a test. Choose the test type from this table:

| Change | Test |
| --- | --- |
| Pure logic in one crate | Unit test in that crate. Use `PureHost` for file and process access. |
| CLI output, `cmake -P`, or `cmake -E` | A case in `conformance/cases/*.case`, with a baseline recorded from the oracle |
| Generated build files or File API replies | Integration test in the generator crate or in `crates/rcmake-cli/tests/` |
| Upstream `Tests/RunCMake` behavior | Run `tools/measure/runcmake_eval.py` for the affected suites |
| A crash found by fuzzing | Regression test in the crate that owns the code |

To record a baseline for a CLI or script case from the oracle:

```sh
scripts/stage.sh
cargo run --bin rcmake-harness -- --suite <name> --record
```

Use `--offline` to compare with the recorded baselines when the oracle is not
available.

## Add a dependency

Add a third-party crate only when the standard library and the existing
dependencies cannot do the work. Before you add one:

1. Check that its licence is on the allowlist: `MIT`, `Apache-2.0`,
   `BSD-2-Clause`, `BSD-3-Clause`, `0BSD`, `ISC`, `Zlib`, `Unicode-3.0`, or
   `bzip2-1.0.6`.
2. Add a row to [`docs/PRIOR_ART.md`](docs/PRIOR_ART.md).
3. Add a short decision record to [`docs/ADR.md`](docs/ADR.md).
4. Regenerate the notices with
   `python3 tools/measure/licence_scan.py --generate-notices`. Add licence
   texts that the crate does not include to `third_party/licences/`.
5. Add the crate to `[workspace.dependencies]` in the root `Cargo.toml`.

## Commit messages

Use [Conventional Commits](https://www.conventionalcommits.org/). Use the
crate name without the `rcmake-` prefix as the scope:

```text
feat(eval): implement block(SCOPE_FOR)
fix(gen-ninja): quote response file paths on Windows
test(presets): cover schema 13 include cycles
```

- Write the title in the imperative mood.
- Keep the title at 72 characters or fewer.
- In the body, tell why the change is necessary and what CMake 4.5.1 does.
- Do not include machine-specific paths.

## Pull requests

- Keep each pull request to one subject.
- Fill in the pull request template.
- Tell which CMake 4.5.1 behavior the change matches, and how you checked it.
- Include the commands that you ran and their results.
- A change to a shared contract in `rcmake-base`, `rcmake-diag`,
  `rcmake-syntax`, `rcmake-host`, or `rcmake-model` needs a decision record.

## Report a security problem

rcmake reads untrusted project files and archives. Do not open a public issue
for a security problem. Follow [`SECURITY.md`](SECURITY.md) to report it
privately.

## Licence of contributions

Unless you state otherwise, any contribution that you submit for inclusion in
rcmake is licensed as `MIT OR Apache-2.0`, with no additional terms or
conditions.
