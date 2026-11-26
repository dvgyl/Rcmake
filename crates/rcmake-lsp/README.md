# rcmake language server

`rcmake-lsp` is a Language Server Protocol server for `CMakeLists.txt` and `*.cmake` files. It provides hover, completion, go to definition, document symbols, semantic tokens, and formatting. When you build it with the `eval` feature, hover also shows evaluated variables, targets, and generator expressions.

## Get the server

The release archives and `scripts/install.sh` install `rcmake-lsp` next to `rcmake`, in `~/.local/bin` by default. You can also build it from the repository root with these commands.

```sh
source env/env.sh
cargo build --locked --release -p rcmake-lsp --features eval,stdio
```

The build writes `target/release/rcmake-lsp`, or `rcmake-lsp.exe` on Windows. If you set `CARGO_TARGET_DIR`, look inside its `release` directory instead. Building with `--features stdio` alone produces a lexical-only server without project evaluation.

The server takes no command-line arguments. It reads LSP messages from standard input and writes responses to standard output, so starting it directly in a terminal waits for input.

By default, the server looks for bundled CMake modules in `share/cmake-4.5/Modules` next to its installation. If you run it from a source checkout, set `RCMAKE_VENDORED_DATA_ROOT` in the server environment to the absolute path of `modules/cmake-4.5`.

## Configure your editor

Each example below runs the `rcmake-lsp` command. If your editor does not have that `bin` directory on its `PATH`, replace `rcmake-lsp` with the absolute path to the executable. Set the workspace root to the directory that holds the top-level `CMakeLists.txt`.

### VS Code

Install the [Generic LSP Client](https://marketplace.visualstudio.com/items?itemName=llllvvuu.llllvvuu-glspc) extension, ID `llllvvuu.llllvvuu-glspc`. Because VS Code has no built-in CMake language, also install an extension that registers the `cmake` language ID, such as [CMake](https://marketplace.visualstudio.com/items?itemName=twxs.cmake). Then add these keys to your workspace `.vscode/settings.json`.

```json
{
	"glspc.serverCommand": "rcmake-lsp",
	"glspc.serverCommandArguments": [],
	"glspc.languageId": "cmake"
}
```

Generic LSP Client runs one server for one language ID.

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

Disable other CMake language servers, such as `neocmake` or `cmake`, on the same buffers so you do not get duplicate results.

### Helix

Add this configuration to `~/.config/helix/languages.toml`, or to `.helix/languages.toml` inside the project.

```toml
[language-server.rcmake-lsp]
command = "rcmake-lsp"

[[language]]
name = "cmake"
language-servers = ["rcmake-lsp"]
```

Run `hx --health cmake` to check that Helix finds the server.

### Zed

Install the NeoCMake extension, named `neocmake`, which registers a CMake language server under the ID `cmake`. Then point that server to `rcmake-lsp` in your Zed `settings.json`.

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

### Emacs with Eglot

```elisp
(with-eval-after-load 'eglot
  (add-to-list 'eglot-server-programs
               '((cmake-mode cmake-ts-mode) . ("rcmake-lsp"))))
```

Then run `M-x eglot` in a CMake buffer.

### CLion and other JetBrains IDEs

1. Install the [LSP4IJ](https://plugins.jetbrains.com/plugin/23257-lsp4ij) plugin.
2. Open Settings, go to `Languages & Frameworks`, then `Language Servers`, and add a server.
3. Set the command to the absolute path of `rcmake-lsp`.
4. On the Mappings tab, add the file name patterns `CMakeLists.txt` and `*.cmake`.

CLion includes its own CMake support, so LSP4IJ shows results from `rcmake-lsp` alongside it.

### Other editors

Any LSP client can connect to `rcmake-lsp` using these settings.

- **Command.** The absolute path to `rcmake-lsp` with an empty argument list.
- **Transport.** Standard input and standard output.
- **Files.** `CMakeLists.txt` and `*.cmake`.
- **Workspace root.** The directory that contains the top-level `CMakeLists.txt`.

## Editor behavior

With `eval,stdio`, the server evaluates the project in memory whenever it finds the workspace listfile and the CMake modules. It reads unsaved edits instead of files on disk and blocks configure from starting external processes. In this mode, hover can show evaluated variables, targets, and generator expressions, and generator expression hover reports the result for each build configuration in the snapshot. When a result comes from an incomplete configure or an earlier edit, the server marks it as provisional.

With `stdio` alone, the server uses lexical analysis. The `eval,stdio` build also falls back to lexical analysis whenever it has no evaluated result for a query. The lexical analyzer follows literal `include()` and `add_subdirectory()` calls from the project root and reads open document text, including unsaved edits, instead of reading from disk. It recognizes literal module search paths along with the `${CMAKE_CURRENT_LIST_DIR}` and `${CMAKE_CURRENT_SOURCE_DIR}` path prefixes. When a file has conditional declarations or multiple path choices, go to definition can return more than one possible definition.

Document symbols, completion, navigation, and semantic tokens still work on available source text while a call or block is incomplete. Formatting adjusts only the leading indentation of files that parse cleanly. It leaves blank lines and the interior lines of quoted arguments, bracket arguments, and bracket comments untouched, and returns an error if the formatted response exceeds 16 MiB.

If the server loses synchronization with a document, save the file with full text, send a full-text change, or reopen the document. A save notification without document text cannot restore synchronization. The server accepts up to 16 MiB per document and 64 MiB across all open documents.

As a Rust library, `rcmake-lsp` has no required features. An application can create `Server` with its own `Host` and attach a `SemanticProvider` to supply results from an evaluated project snapshot. Pass the snapshot version alongside hover text so the server knows when to mark results from an earlier document version as provisional.
