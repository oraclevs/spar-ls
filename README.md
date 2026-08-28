# spar-ls

**Language server for [Spar](https://github.com/oraclevs/spar) — the typed configuration language.**

`spar-ls` implements the [Language Server Protocol](https://microsoft.github.io/language-server-protocol/) (LSP) for `.spar` files. A language server is a background process that connects to your editor and provides language-aware features — error reporting, hover documentation, completions — without any editor-specific code in the compiler itself.

---

## Features

All features below are provided via the LSP and work in any compatible editor.

| Feature | Description |
|---------|-------------|
| **Diagnostics** | Errors from every compiler stage (lex, parse, resolve, type-check) appear as squiggles in real time |
| **Hover** | Hover over any variable, section name, field, or import alias to see its type and resolved value |
| **Completion** | Context-aware suggestions for section fields, variable names, and symbols from imported files. Triggers on `::` |
| **Semantic tokens** | Token classification for the entire document — editors use this for richer, more accurate coloring than syntax highlighting alone provides |
| **Formatting** | Format the current document via your editor's "Format Document" command (runs the same engine as `spar fmt`) |
| **Cross-file diagnostics** | Errors in imported files are propagated to the file that imports them |

---

## Installation

You need the Rust toolchain installed (`rustup.rs`).

```bash
git clone https://github.com/oraclevs/spar-ls.git
cd spar-ls
cargo build --release
```

Copy the binary to a directory on your PATH:

```bash
sudo cp target/release/spar-ls /usr/local/bin/
```

Verify:

```bash
spar-ls --version
# spar-ls 0.1.0
```

> `spar-ls` requires `spar` (the core crate) to be at a compatible version — both are pinned to the same release tag.

---

## Editor setup

### VS Code

Install the [vscode-spar](https://github.com/oraclevs/vscode-spar) extension. It launches `spar-ls` automatically using the binary on your PATH.

If `spar-ls` is installed to a non-standard location, set the path in VS Code settings:

```json
{
  "spar.serverPath": "/path/to/spar-ls"
}
```

### Neovim (nvim-lspconfig)

```lua
local lspconfig = require('lspconfig')
local configs   = require('lspconfig.configs')

-- Register spar-ls (not yet in upstream lspconfig)
if not configs.spar_ls then
  configs.spar_ls = {
    default_config = {
      cmd       = { 'spar-ls' },
      filetypes = { 'spar' },
      root_dir  = lspconfig.util.root_pattern('.git', '*.spar'),
      settings  = {},
    },
  }
end

-- Associate .spar files with the 'spar' filetype
vim.filetype.add({ extension = { spar = 'spar' } })

lspconfig.spar_ls.setup {}
```

> Neovim integration has not been formally tested by the project — contributions and bug reports are welcome.

### Helix

Add to your `languages.toml`:

```toml
[[language]]
name             = "spar"
scope            = "source.spar"
file-types       = ["spar"]
roots            = [".git"]
comment-token    = "//"
language-servers = ["spar-ls"]

[language-server.spar-ls]
command = "spar-ls"
```

> Helix integration has not been formally tested by the project — contributions and bug reports are welcome.

### Any other LSP client

`spar-ls` communicates over **stdio** with no flags required:

```
spar-ls
```

Point your LSP client at that command for `*.spar` files.

---

## How it works

On every document change, `spar-ls` runs the full Spar compiler pipeline:

1. **Lex** — tokenize the source
2. **Parse** — build the AST
3. **Load imports** — read and compile every imported file
4. **Resolve** — build the complete symbol table
5. **Type-check** — validate types across all files in the import graph

Errors from any stage become LSP `Diagnostic` objects and are pushed to the editor immediately. Hover and completion responses are built from the resolved symbol table.

---

## Relationship to spar and vscode-spar

- `spar-ls` links directly against the `spar` crate and shares its parser, resolver, typechecker, and formatter. There is no independent language implementation.
- [vscode-spar](https://github.com/oraclevs/vscode-spar) is the primary consumer, but `spar-ls` works with any LSP-capable editor.
- For language documentation — syntax, semantics, CLI — see the [spar](https://github.com/oraclevs/spar) repository.

---

## License

MIT — see [LICENSE](LICENSE).
