# Security policy

rcmake parses untrusted `CMakeLists.txt` scripts, cache files, presets, CPS and
SBOM documents, binary headers, and compressed archives (`tar`, `gzip`, `bzip2`,
`xz`, `zstd`, `zip`, `7z`).

## Report a vulnerability

Do **not** open a public GitHub issue for a security vulnerability. Examples
are path traversal during archive extraction, unbounded resource use, command
injection, and a bypass of the `--hermetic` checks.

Report vulnerabilities privately through
[GitHub private vulnerability reporting](https://github.com/dvgyl/rcmake/security/advisories/new).

Include:

- A minimal reproducer (`CMakeLists.txt`, archive, or command line)
- The output of `rcmake --version`, or the commit SHA, and the host platform
- The observed behavior and the expected behavior

## Hardening guarantees

- All 35 workspace crates enforce `#![forbid(unsafe_code)]`.
- Reachable input paths avoid panics and enforce explicit recursion, expansion,
  loop, and file-size limits (`rcmake_base::Limits`).
- Archive extraction validates member paths against traversal and symlink escape.
- `scripts/install.sh` verifies the SHA-256 checksum of every downloaded release
  archive before it installs files.
