# Contributing to rcmake

Read [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the crate structure and data flow.

## Goal of the project

`rcmake` must behave the way CMake 4.5.1 behaves. The official CMake 4.5.1 binary is the reference oracle. Whenever `rcmake` and CMake 4.5.1 give different results, `rcmake` has the bug, unless CMake 4.5.1 itself is nondeterministic or crashes.

## Set up

1. Install the Rust toolchain pinned in `rust-toolchain.toml`, which is Rust 1.99.1 with `rustfmt` and `clippy`. `rustup` installs it automatically when you run a Cargo command in the repository.
2. Install Python 3.11 or later for the scripts in `tools/`.
3. Install Ninja to run the generator and corpus tests.
4. Load the environment in each new shell.

   ```sh
   source env/env.sh
   ```

5. Download the CMake 4.5.1 oracle for the differential tests.

   ```sh
   scripts/fetch-oracle.sh
   ```

`env/env.sh` exports `RCMAKE_ORACLE_CMAKE`, `RCMAKE_ORACLE_SRC`, and the other oracle variables. It also sets default limits of 2 Cargo build jobs and 3 test threads so builds stay within memory on smaller machines. If your machine has more RAM, export higher `CARGO_BUILD_JOBS` and `RUST_TEST_THREADS` values before sourcing `env/env.sh`.

## First contribution in 2 minutes

Complete [Set up](#set-up) first.

1. Build `rcmake` and stage it alongside the bundled CMake modules.

   ```sh
   source env/env.sh
   scripts/stage.sh
   ```

   This writes the staged tree into `target/stage`, where `target/stage/bin/cmake` links to `rcmake`.

2. Run a single unit test by passing its crate name to `-p` and part of the test name.

   ```sh
   cargo test -p rcmake-commands search_replace_and_join
   ```

3. Find the implementation of a CMake command. Each command family lives in its own module under `crates/rcmake-commands/src/`, such as `string_math/` for `string()` and `math()`, or `file_path/` for `file()` and `cmake_path()`. Each family's `register()` function registers its commands.

   ```rust
   registry.add("cmake_path", path_cmd::run, flags);
   ```

4. Compare a behavior directly against CMake 4.5.1 by running the same small script with both binaries and diffing their output.

   ```sh
   printf 'string(TOUPPER "abc" out)\nmessage(STATUS "${out}")\n' > probe.cmake
   "$RCMAKE_ORACLE_CMAKE" -P probe.cmake > oracle.txt 2>&1
   target/stage/bin/cmake -P probe.cmake > rcmake.txt 2>&1
   diff oracle.txt rcmake.txt
   ```

   An empty diff means the two tools agree.

5. Run a recorded differential suite. The suite name matches a file in `conformance/cases/` without the `.case` suffix, and `--filter` selects one case.

   ```sh
   cargo run --bin rcmake-harness -- --suite string_math --filter string-combine
   ```

6. Run the [quality gates](#quality-gates), then open a pull request. The pull request template asks which CMake 4.5.1 behavior your change matches and how you checked it.

### Add or change a CMake command

1. Edit or add the command handler inside its command family module, such as `crates/rcmake-commands/src/string_math/`. Each handler implements `rcmake_eval::BuiltinFn`, which has the signature `fn(&mut Cx<'_>, &[ExpandedArg]) -> CmdResult`.
2. For a new command, call `registry.add` in the family's `register()` function. Set `CmdFlags { scriptable: true }` if the command works in `cmake -P` scripts.
3. For a new command, add its name to the `PROJECT` list in `crates/rcmake-commands/src/registry_tests.rs`. If it does not work in scripts, add it to `PROJECT_ONLY` as well.
4. Add a unit test to the family's `tests.rs` file using `PureHost`.
5. Add a case to the matching `conformance/cases/<suite>.case` file and record its oracle baseline with `cargo run --bin rcmake-harness -- --suite <suite> --record`.

## Independent implementation and licensing rules

- Write all Rust code in `crates/` and `tools/` from scratch. Never copy, translate, or adapt CMake C++ source code.
- You may read the CMake documentation and run the CMake 4.5.1 binary to observe exact behavior. Capture what you learn in a test rather than in copied code.
- The CMake 4.5.1 files in `modules/cmake-4.5/` stay under `BSD-3-Clause`. Keep their original copyright headers intact.
- Do not add files under GPL, LGPL, AGPL, MPL, or other copyleft licences.
- `rcmake` never runs the upstream `cmake` binary at runtime. Only `rcmake-harness`, `tools/`, and the differential tests read `RCMAKE_ORACLE_CMAKE`. The licence scanner enforces this rule.

## Engineering rules

### No unsafe code

The workspace sets `unsafe_code = "forbid"`, and every crate root declares `#![forbid(unsafe_code)]`. Any change that requires operating system access through a foreign function interface needs an accepted decision record in [`docs/ADR.md`](docs/ADR.md) first.

### Treat all input as untrusted

`CMakeLists.txt` files, cache files, presets, environment variables, archives, and process output can contain arbitrary bytes. Follow these rules on every path that reads untrusted input.

- Return an error instead of calling `unwrap()`, `expect()`, `panic!()`, `todo!()`, or `unimplemented!()`.
- Never index slices directly. Call `get()` and handle `None`.
- Store CMake values as byte strings (`BString`) rather than assuming UTF-8.
- Apply the limits in `rcmake_base::Limits` to recursion depth, variable expansion, loop counts, regex steps, and file sizes.

Clippy catches these mistakes in library and binary code. Unit tests may use `unwrap()` and `expect()`.

### Use the `Host` trait for system access

The evaluator, expression engines, presets loader, and generators must not call `std::fs`, `std::env`, `std::process`, or the system clock directly. Route all operating system access through the `rcmake_host::Host` trait.

| Need | `Host` method |
| --- | --- |
| Files and directories | `read`, `write`, `metadata`, `read_dir`, and related methods |
| Environment | `env_get`, `env_all` |
| Processes | `run`, `run_bounded` |
| Time | `now`, `monotonic` |

`NativeHost` talks to the real operating system, while `PureHost` provides an in-memory host for fast, deterministic tests. `clippy.toml` bans the direct `std` calls everywhere except `rcmake-host`, the `rcmake-cli` entry point, and `rcmake-harness`.

Every path passed to `Host` must be absolute. Do not rely on or change the process working directory.

### Implement general rules

Implement the general CMake 4.5.1 rule instead of adding a special case for one project, one test, or one file name. When a test seems to need a special case, find the underlying rule that CMake applies.

### Document public items

Every public item needs a doc comment. The workspace sets `missing_docs = "warn"`, and the quality gates turn warnings into errors.

## Quality gates

Run these commands before opening a pull request.

```sh
source env/env.sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo test --workspace
python3 tools/measure/licence_scan.py --check-hygiene
```

`scripts/gate.sh` runs all of these checks along with the recorded CLI and script cases.

If you touch `rcmake-gen-xcode` or `rcmake-gen-vs`, run Clippy with the cross-platform generator features enabled as well.

```sh
cargo clippy -p rcmake-cli --features vs-cross,xcode-cross --all-targets -- -D warnings
```

The licence scanner enforces these repository checks.

- Every dependency licence is on the allowlist.
- No crate outside `rcmake-harness` reads the oracle variables.
- No tracked text file contains an absolute path into a user home directory. Use placeholders such as `<path>` or `$HOME` in examples.
- Only files that begin with `#!` have the executable bit set.
- No `._*` or `.DS_Store` file is tracked in Git.

## Add tests

Every behavior change requires a test.

| Change | Test |
| --- | --- |
| Pure logic in one crate | Unit test in that crate using `PureHost` for file and process access |
| CLI output, `cmake -P`, or `cmake -E` | A case in `conformance/cases/*.case` with a baseline recorded from the oracle |
| Generated build files or File API replies | Integration test in the generator crate or in `crates/rcmake-cli/tests/` |
| Upstream `Tests/RunCMake` behavior | Run `tools/measure/runcmake_eval.py` for the affected suites |
| A crash found by fuzzing | Regression test in the crate that owns the code |

Run these commands to record a baseline for a CLI or script case from the oracle.

```sh
scripts/stage.sh
cargo run --bin rcmake-harness -- --suite <name> --record
```

Pass `--offline` to compare against the recorded baselines when the oracle is unavailable.

## Add a dependency

Add a third-party crate only when the standard library and existing dependencies cannot do the work. Complete these steps before adding one.

1. Check that its licence is one of `MIT`, `Apache-2.0`, `BSD-2-Clause`, `BSD-3-Clause`, `0BSD`, `ISC`, `Zlib`, `Unicode-3.0`, or `bzip2-1.0.6`.
2. Add a row to [`docs/PRIOR_ART.md`](docs/PRIOR_ART.md).
3. Add a short decision record to [`docs/ADR.md`](docs/ADR.md).
4. Regenerate the notices with `python3 tools/measure/licence_scan.py --generate-notices`, and copy any licence texts missing from the crate into `third_party/licences/`.
5. Add the crate to `[workspace.dependencies]` in the root `Cargo.toml`.

## Commit messages

Follow [Conventional Commits](https://www.conventionalcommits.org/), using the crate name without the `rcmake-` prefix as the scope.

```text
feat(eval): implement block(SCOPE_FOR)
fix(gen-ninja): quote response file paths on Windows
test(presets): cover schema 13 include cycles
```

- Write the title in the imperative mood.
- Keep the title to 72 characters or fewer.
- Explain in the body why the change is necessary and what CMake 4.5.1 does.
- Omit machine-specific paths.

## Pull requests

- Keep each pull request focused on one subject.
- Fill in the pull request template.
- State which CMake 4.5.1 behavior the change matches and how you checked it.
- Include the commands you ran and their results.
- Add a decision record in [`docs/ADR.md`](docs/ADR.md) for any change to a shared contract in `rcmake-base`, `rcmake-diag`, `rcmake-syntax`, `rcmake-host`, or `rcmake-model`.

## Report a security problem

`rcmake` reads untrusted project files and archives. Do not open a public issue for a security problem. Follow [`SECURITY.md`](SECURITY.md) to report it privately.

## Licence of contributions

Unless you state otherwise, any contribution you submit for inclusion in `rcmake` is licensed under `MIT OR Apache-2.0`, with no additional terms or conditions.
