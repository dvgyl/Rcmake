# Security policy

`rcmake` parses untrusted `CMakeLists.txt` scripts, cache files, presets, CPS and SBOM documents, binary headers, and compressed archives (`tar`, `gzip`, `bzip2`, `xz`, `zstd`, `zip`, `7z`).

## Report a vulnerability

Do not open a public GitHub issue for a security vulnerability. Examples include path traversal during archive extraction, unbounded resource use, command injection, and a bypass of the `--hermetic` checks.

Report vulnerabilities privately through [GitHub private vulnerability reporting](https://github.com/rcmake/rcmake/security/advisories/new).

Include these details in your report.

- A minimal reproducer such as a `CMakeLists.txt` file, an archive, or a command line.
- The output of `rcmake --version`, or the commit SHA, along with the host platform.
- The observed behavior and the expected behavior.

## Hardening guarantees

- All 35 workspace crates enforce `#![forbid(unsafe_code)]`.
- Reachable input paths return errors instead of panicking and enforce explicit limits on recursion depth, variable expansion, loop counts, and file sizes through `rcmake_base::Limits`.
- Archive extraction validates member paths and rejects directory traversal and symlink escapes.
- `scripts/install.sh` verifies the SHA-256 checksum of every downloaded release archive before installing files.
