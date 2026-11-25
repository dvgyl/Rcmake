# Security Policy

Rcmake parses untrusted `CMakeLists.txt` scripts, cache files, presets, CPS and
SBOM documents, binary headers, and compressed archives (`tar`, `gzip`, `bzip2`,
`xz`, `zstd`, `zip`, `7z`).

## Reporting a vulnerability

Do **not** open a public GitHub issue for a security vulnerability (such as
path traversal during archive extraction, unbounded resource consumption,
command injection, or hermetic sandbox bypass).

Instead, report vulnerabilities privately through
[GitHub Private Vulnerability Reporting](https://github.com/dvgyl/Rcmake/security/advisories/new).

Include:

- A minimal reproducer (`CMakeLists.txt`, archive, or CLI command)
- The Rcmake version or commit SHA and host platform
- Observed behavior versus expected behavior

## Hardening guarantees

- All 34 workspace crates enforce `#![forbid(unsafe_code)]`.
- Reachable input paths avoid panics and enforce explicit recursion, expansion,
  loop, and file-size bounds (`rcmake_base::Limits`).
- Archive extraction validates member paths against traversal and symlink escape.
