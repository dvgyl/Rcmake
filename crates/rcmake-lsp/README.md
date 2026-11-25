# Rcmake language server

Build the native language server from the repository root.

```sh
source env/env.sh
cargo build --locked --release -p rcmake-lsp --features eval,stdio
```

The executable is `target/release/rcmake-lsp` or `rcmake-lsp.exe` on Windows.
If you set `CARGO_TARGET_DIR`, use its `release` directory.

To start the server from an editor, set these options in the editor's Language
Server Protocol (LSP) client.

- Use the absolute path to the executable as the command.
- Use standard input and output for the transport. Use an empty argument list.
- Enable the server for `CMakeLists.txt` and `*.cmake` files.
- Use the project directory with the top-level `CMakeLists.txt` as the workspace.

For a release installation, put the executable in the installation's `bin`
directory. The server finds bundled modules in `share/cmake-4.5/Modules` under
the same installation. To use vendored modules from a checkout, set
`RCMAKE_VENDORED_DATA_ROOT` in the editor's server environment to the absolute
path to `modules/cmake-4.5`.

## Editor behavior

With `eval,stdio`, the server evaluates the project in memory when it finds
the workspace listfile and CMake modules. It uses unsaved changes and prevents
configure from starting external processes. Hover can show evaluated variables,
targets, and generator expressions. Generator expression hover shows the result
for each build configuration in the snapshot. The server marks results from an
incomplete configure or an earlier edit as provisional.

With `stdio` alone, the server uses lexical analysis. The evaluated server also
uses lexical analysis when it has no evaluated result for a query.
It follows literal `include()` and `add_subdirectory()` calls from the project
root. It uses open document text in place of disk text, including unsaved
changes. It recognizes literal module search paths and the
`${CMAKE_CURRENT_LIST_DIR}` and `${CMAKE_CURRENT_SOURCE_DIR}` path prefixes.
It can return more than one possible definition when the source has conditional
declarations or path choices.

Document symbols, completion, navigation, and semantic tokens use available
source text while a call or block is incomplete. Formatting changes only the
leading indentation of files that parse successfully. It preserves blank lines
and the interior lines of quoted arguments, bracket arguments, and bracket
comments. It returns an error if the formatting response exceeds 16 MiB.

If the server loses synchronization with a document, save it with full text,
send a full-text change, or reopen it. A save notification without text cannot
restore synchronization. The server accepts up to 16 MiB per document and
64 MiB across open documents.

The library has no required features. An application can use `Server` with
its own `Host` and add a `SemanticProvider` to supply results from an
evaluated project snapshot. Supply the snapshot version with hover text so the
server can mark results from an earlier document version as provisional.
