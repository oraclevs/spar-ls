# spar-ls

The editor-neutral intelligence layer for [Spar](https://github.com/oraclevs/spar): hover, completion, diagnostics, rename, and formatting, all speaking standard LSP over stdio. It links directly against the `spar` compiler, parser, and resolver instead of maintaining a second implementation of the language that could drift from the real one.

Nothing here is tied to VS Code. The same binary works from Neovim, Zed, Helix, JetBrains with an LSP client, or any other editor that speaks the protocol.

## Launch contract

```bash
spar-ls --stdio
```

Running `spar-ls` with no arguments is equivalent. The server reads LSP messages from stdin and writes LSP messages to stdout.

Verify the installed build with:

```bash
spar-ls --version
# spar-ls 0.6.0
```

## Portable feature matrix

| LSP capability | Spar behavior |
|---|---|
| `textDocument/publishDiagnostics` | Compiler lex/parse/resolve/type diagnostics |
| `textDocument/hover` | Types, functions, fields, imports, callable signatures |
| `textDocument/completion` | Locals and parameters, file-level and imported symbols, members, imports, import paths, named arguments (declared order) and argument values |
| `completionItem/resolve` | Symbol detail plus declaration documentation when supported by the client |
| `textDocument/signatureHelp` | Function/task/function-group parameter signatures and active parameter |
| `textDocument/definition` | Same-file and cross-file definitions |
| `textDocument/references` | Cross-file references through Spar imports |
| `textDocument/documentHighlight` | Semantic read/write/declaration highlights without text-wide matching |
| `textDocument/documentSymbol` | Hierarchical symbols with flat fallback for minimal clients |
| `workspace/symbol` | Cached project symbol search |
| `textDocument/prepareRename` + `textDocument/rename` | Semantic local/workspace rename using standard `WorkspaceEdit` |
| `textDocument/codeAction` | Direct auto-import quick-fix edits for unresolved exported symbols |
| `textDocument/semanticTokens/full` | Spar tokens plus native-shell command/flag/argument/interpolation classes |
| `textDocument/formatting` | The same safe formatter used by Spar |

Optional client capabilities only improve presentation. Core responses remain usable when a client does not support completion resolve, snippets, or hierarchical symbols.

## Import IntelliSense

Inside selective imports, completion resolves the actual target module:

```spar
import { | } from "./utils.spar";
import type { | } from "./types.spar";
import pkg { | } from "std/fs";
```

Before `from "..."` is typed, `import pkg { | ` lists the exports of every bundled `std/*` module. Accepting one adds the `from "std/fs"` clause automatically (as an additional edit after the closing `}`, or by completing the whole statement when no `}` exists yet).

Only legal exported symbols are offered. `import type` filters to exported `type`/`enum` declarations, and already-selected names are omitted. Canonical `struct` values use normal imports even when referenced in a type position, because structs are also constructible runtime values. Auto-import follows the same rule.

Import-path completion covers local `.spar` modules, bundled `std/*` modules, and dependency aliases/submodules available from the current package lockfile. It performs no network access.


## Formatting and documentation

`textDocument/formatting` delegates to Spar's own safe source formatter. The LSP therefore formats the same modern syntax the compiler understands, including `impl` blocks, closures, method calls, and `|>` structured pipelines, and formatting is expected to be idempotent. If the source cannot be parsed safely, formatting fails soft instead of rewriting it heuristically.

A contiguous standalone `// ...` or `/* ... */` comment immediately before a declaration is exposed as declaration documentation. Import completion includes that text directly, `completionItem/resolve` preserves it while adding origin information, and indexed method hover shows the same documentation. A blank line separates an ordinary comment from declaration documentation.

## Call IntelliSense

Standard signature help is triggered by `(` and `,`. Named-argument completion offers only parameters not already supplied, with required parameters sorted ahead of defaulted parameters.

Clients advertising standard LSP snippet support may receive required-argument call snippets. Clients without snippet support receive ordinary plain-text completion items.

## Native shell semantics

Native `shell { ... }` expressions expose these custom semantic token types through the standard LSP semantic-token protocol:

- `shellCommand`
- `shellBuiltin`
- `shellArgument`
- `shellFlag`
- `shellOperator`
- `shellRedirect`
- `shellEnvironment`
- `shellInterpolation`

The server also exposes `resolved`, `unresolved`, and `defaultLibrary` semantic modifiers. External command resolution only checks filesystem executability against the server process `PATH`; **it never executes a command** to provide editor metadata, and an unresolved command is not a compiler diagnostic by default.

Foreign shell blocks such as `shell bash { ... }` are intentionally not classified as Spar native-shell syntax. Editor clients can embed their own Bash grammar for those regions.

The language server reports semantic *meaning*, not RGB colors. Each editor/theme decides how semantic token classes are rendered. A Spar editor extension can map resolved commands to green, flags to an accent color, and so on without changing `spar-ls`.

## Client compatibility

The intelligence core uses standard LSP request/response types and `file://` source locations. Core language features do not require VS Code commands, VS Code URI schemes, or custom JSON-RPC methods.

Expected client targets include:

- VS Code
- Neovim
- Zed
- Helix
- JetBrains IDEs through an LSP integration
- other standards-compliant LSP clients

Different editors expose standard LSP features through different UI, so identical protocol support does not imply identical menus or keybindings.

### Neovim example

```lua
vim.filetype.add({ extension = { spar = 'spar' } })

vim.lsp.start({
  name = 'spar-ls',
  cmd = { 'spar-ls', '--stdio' },
  root_dir = vim.fs.root(0, { 'spar.toml', '.git' }) or vim.fn.getcwd(),
})
```

### Helix example

```toml
[language-server.spar-ls]
command = "spar-ls"
args = ["--stdio"]

[[language]]
name = "spar"
scope = "source.spar"
file-types = ["spar"]
language-servers = ["spar-ls"]
comment-token = "//"
```

## Architecture

On document updates, `spar-ls` delegates parsing, resolving and type checking to the `spar` crate, then maintains an editor-query workspace index derived from those compiler results. Unsaved open-document content wins over disk content, while last-known-good semantics are retained during transient syntax errors where possible.

Small read-only compiler APIs expose bundled stdlib/module identity to editor tooling; the server does not reach into private compiler internals or execute packages merely to discover metadata.

## Development verification

From the `spar-ls` repository:

```bash
cargo fmt --check
cargo check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

The repository also includes regression coverage for minimal/rich client capability negotiation, UTF-16 LSP positions, editor-neutral payloads, import completion, signatures, symbols, semantic identity, auto-imports, and native-shell semantic token classes.

## License

MIT — see [LICENSE](LICENSE).

## Semantic tokens

Standard token types plus these Spar-specific types (append-only legend): `section`, `task`, `taskField`, `functionGroup`, `declarationKeyword`, `shellCommand`, `shellBuiltin`, `shellArgument`, `shellFlag`, `shellOperator`, `shellRedirect`, `shellEnvironment`, `shellInterpolation`, and the standard `typeParameter`.

`declarationKeyword` covers declaring words (`var`, `function`, `export`, `import`, `struct`, `type`, `task`); `keyword` covers control flow (`if`, `for`, `return`, `try`).

Modifiers: `declaration`, `resolved`, `unresolved`, and `defaultLibrary`. `defaultLibrary` marks everything built in (built-in types, conversion functions, bundled `std` functions, shell builtins); user-declared symbols never carry it, so clients can color built-in and user-defined names differently.

Keywords and types are recovered from the lexer when the file has a syntax error, so highlighting does not disappear while typing.

## Smoke test

`scripts/lsp_smoke.py` drives the server over plain stdio with no editor involved. It opens a valid file, edits it into an incomplete state, and checks completion, named arguments, import discovery and semantic tokens:

```bash
python3 scripts/lsp_smoke.py ./target/release/spar-ls
```
