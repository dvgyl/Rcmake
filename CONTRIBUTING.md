# Contributing to Rcmake

Thank you for your interest in Rcmake. This guide gives the rules and checks
for every change. Read [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) first for
the crate structure and the data flow.

## Goal of the project

Rcmake must behave as CMake 4.5.1 behaves. The official CMake 4.5.1 binary is
the reference (the "oracle"). When Rcmake and CMake 4.5.1 give different
results, Rcmake has the bug, unless CMake 4.5.1 itself is nondeterministic or
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
oracle variables. It also limits Cargo to 2 build jobs and 3 test threads.
Increase `CARGO_BUILD_JOBS` and `RUST_TEST_THREADS` if your machine has more
memory.

## Clean-room rules

- Write all Rust code in `crates/` and `tools/` from scratch. Do not copy,
  translate, or adapt CMake C++ source code.
- You can read the CMake documentation and run the CMake 4.5.1 binary to
  find the correct behavior. Record the behavior in a test, not in copied code.
- The CMake 4.5.1 files in `modules/cmake-4.5/` stay under `BSD-3-Clause`.
  Do not remove or change their copyright headers.
- Do not add files under GPL, LGPL, AGPL, MPL, or other copyleft licences.
- Rcmake never runs the upstream `cmake` binary at runtime. Only
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

`scripts/gate.sh` runs all of these checks and the Layer 1 oracle suites.

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

To record a Layer 1 baseline from the oracle:

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
- Tell which CMake 4.5.1 behavior the change matches, and how you checked it.
- Include the commands that you ran and their results.
- A change to a shared contract in `rcmake-base`, `rcmake-diag`,
  `rcmake-syntax`, `rcmake-host`, or `rcmake-model` needs a decision record.

## Report a security problem

Rcmake reads untrusted project files and archives. Do not open a public issue
for a security problem. Use GitHub private vulnerability reporting for this
repository.

## Licence of contributions

Unless you state otherwise, any contribution that you submit for inclusion in
Rcmake is licensed as `MIT OR Apache-2.0`, with no additional terms or
conditions.
