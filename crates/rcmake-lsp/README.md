# rcmake language server

`rcmake-lsp` is a Language Server Protocol (LSP) server for `CMakeLists.txt` and `*.cmake` files. It gives hover, completion, go to definition, document symbols, semantic tokens, and formatting. With the `eval` feature, hover also shows evaluated variables, targets, and generator expressions.

## Get the server

The release archives and `scripts/install.sh` install `rcmake-lsp` next to `rcmake`, in `~/.local/bin` by default. To build it from the repository root:

```sh
source env/env.sh
cargo build --locked --release -p rcmake-lsp --features eval,stdio
```

The executable is `target/release/rcmake-lsp`, or `rcmake-lsp.exe` on Windows. If you set `CARGO_TARGET_DIR`, use its `release` directory. A build with `--features stdio` alone gives a lexical-only server.

The server reads LSP messages on standard input and writes responses on standard output. It takes no arguments. When you start it in a terminal, it waits for input.

The server finds the bundled CMake modules in `share/cmake-4.5/Modules` of the same installation. To use the modules of a checkout, set `RCMAKE_VENDORED_DATA_ROOT` in the server environment to the absolute path of `modules/cmake-4.5`.

## Configure your editor

The examples use the command `rcmake-lsp`. If the `bin` directory is not on the `PATH` of your editor, use the absolute path to the executable. Use the directory that contains the top-level `CMakeLists.txt` as the workspace root.

### VS Code

Install the [Generic LSP Client](https://marketplace.visualstudio.com/items?itemName=llllvvuu.llllvvuu-glspc) extension (`llllvvuu.llllvvuu-glspc`). VS Code has no built-in CMake language, so also install an extension that defines the `cmake` language ID, for example [CMake](https://marketplace.visualstudio.com/items?itemName=twxs.cmake). Then add these keys to your workspace `.vscode/settings.json`:

```json
{
	"glspc.serverCommand": "rcmake-lsp",
	"glspc.serverCommandArguments": [],
	"glspc.languageId": "cmake"
}
```

The extension runs one server for one language ID.

### Neovim 0.11 and later

```lua
vim.lsp.config("rcmake", {
	cmd = { "rcmake-lsp" },
	filetypes = { "cmake" },
	root_markers = { "CMakePresets.json", ".git" },
})
vim.lsp.enable("rcmake")
```

### Neovim with nvim-lspconfig

```lua
local lspconfig = require("lspconfig")
local configs = require("lspconfig.configs")

if not configs.rcmake then
	configs.rcmake = {
		default_config = {
			cmd = { "rcmake-lsp" },
			filetypes = { "cmake" },
			root_dir = lspconfig.util.root_pattern("CMakePresets.json", ".git"),
		},
	}
end
lspconfig.rcmake.setup({})
```

Disable other CMake servers, such as `neocmake` or `cmake`, for the same buffers to prevent duplicate results.

### Helix

Add this to `~/.config/helix/languages.toml`, or to `.helix/languages.toml` in the project:

```toml
[language-server.rcmake-lsp]
command = "rcmake-lsp"

[[language]]
name = "cmake"
language-servers = ["rcmake-lsp"]
```

Run `hx --health cmake` to check that Helix finds the server.

### Zed

Install the NeoCMake extension (`neocmake`). It registers a CMake language server with the ID `cmake`. Then point that server to `rcmake-lsp` in your Zed `settings.json`:

```json
{
	"lsp": {
		"cmake": {
			"binary": {
				"path": "/absolute/path/to/rcmake-lsp",
				"arguments": []
			}
		}
	},
	"languages": {
		"CMake": {
			"language_servers": ["cmake"]
		}
	}
}
```

Zed needs an absolute `path`.

### Emacs (Eglot)

```elisp
(with-eval-after-load 'eglot
  (add-to-list 'eglot-server-programs
               '((cmake-mode cmake-ts-mode) . ("rcmake-lsp"))))
```

Then run `M-x eglot` in a CMake buffer.

### CLion and other JetBrains IDEs

1. Install the [LSP4IJ](https://plugins.jetbrains.com/plugin/23257-lsp4ij) plugin.
2. Open **Settings**, then **Languages & Frameworks**, then **Language Servers**, and add a server.
3. Set **Command** to the absolute path of `rcmake-lsp`.
4. On the **Mappings** tab, add the file name patterns `CMakeLists.txt` and `*.cmake`.

CLion has its own CMake support. LSP4IJ adds the `rcmake-lsp` results next to it.

### Other editors

Any LSP client works with these settings:

- Command: the absolute path to `rcmake-lsp`, with an empty argument list.
- Transport: standard input and output.
- Files: `CMakeLists.txt` and `*.cmake`.
- Workspace root: the directory with the top-level `CMakeLists.txt`.

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
