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

## Debugger transport

`--debugger` starts a Debug Adapter Protocol (DAP) server for one IDE client. `rcmake` treats the client as untrusted: message frames, JSON depth, breakpoint counts, and responses have limits. A write that the client does not read in 30 seconds ends the session.

- On macOS and Linux, the server listens on an `AF_UNIX` socket at the `--debugger-pipe` path and removes the path after the client connects. The file permissions of the socket and of its directory control access.
- On Windows, the server makes one named pipe with the CMake 4.5.1 settings (ADR-050). `first_pipe_instance(true)` makes the bind fail if a pipe with the name already exists. Thus `rcmake` never shares a pipe that another process made first, and the run stops with an error. This setting does not control which pipe the IDE opens. `max_instances(1)` stops a second client. `reject_remote_clients(true)` stops clients on other computers. The name must start with `\\.\pipe\` and must have no NUL character.
- The Windows pipe has the default security descriptor, as in CMake and in libuv. This descriptor gives full control to LocalSystem, to administrators, and to the creator owner, and gives read access to Everyone and to the anonymous account. Before the IDE connects, another local account can thus open the pipe for read. That client cannot send requests, and the IDE cannot connect, so the run waits for a DAP handshake that does not start. A custom DACL (discretionary access control list) needs an `unsafe` Win32 call, so `rcmake` does not set one. Use a pipe name that other users cannot guess, as VS Code CMake Tools does.
