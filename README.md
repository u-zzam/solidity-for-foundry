# solidity

A **Foundry-native Solidity language server** — the same behavior in every editor, always on your project's exact solc version, with imports resolved exactly like `forge build`.

> Status: working. Navigation is live as you type; diagnostics and richer
> intelligence (inlay hints, semantic tokens, code actions) run on your
> project's exact solc version.

## Why

Foundry has no official LSP. Today you get a different third-party server per editor (Nomic on VS Code, Juan Blanco on Neovim), each with its own gaps:

- **Nomic** lags solc versions (can't validate projects on recent compilers).
- **Juan Blanco** mis-resolves Foundry remappings (false import errors).
- Behavior differs across editors; nothing is truly Foundry-native.

This is one shared server that fixes all three: identical everywhere, current solc (auto-installed via svm), and import resolution byte-identical to `forge build` (it reuses `foundry-compilers`, the engine `forge` itself uses).

## How it works

- **Diagnostics** come from **solc** (via `foundry-compilers`) → squiggles match `forge build` exactly, on any version.
- **As-you-type diagnostics** type-check the unsaved buffer with solc directly (no codegen, no disk writes).
- **Code intelligence** (go-to-def, hover, completion, document & workspace symbols, document highlight) works the instant a file opens and updates as you type — from an error-tolerant **tree-sitter** parse of the buffer, so no compile and no `foundry.toml` are needed to navigate. The **typed solc AST** from the last successful compile refines it when present (more precise resolution, plus rename, inlay hints, and semantic tokens).
- One Rust server; thin clients for VS Code and Zed; a one-line config for Neovim/Helix/Emacs.

## Install

`forge` must be on your `PATH` (the server shells out to it for formatting and
lint). The server itself auto-downloads the solc version your project pins (via
svm) on first compile.

- **VS Code / Zed:** nothing to build — the extension downloads the prebuilt
  `solidity-lsp` binary matching its version from the GitHub release on first
  activation. (If a `solidity-lsp` is already on your `PATH`, Zed uses it.)
- **Other editors, or to build from source:** install the binary with Rust:

  ```sh
  cargo install --path solidity-lsp --locked
  ```

  This puts `solidity-lsp` on your `PATH`. (`--locked` uses the pinned
  `Cargo.lock`; required on Rust < 1.95.)

## Editor setup

Every editor runs the *same* `solidity-lsp` binary, so behavior is identical.

**VS Code** — install the [`solidity-vscode`](./solidity-vscode) extension
(press F5 from that folder for a dev host). It downloads and spawns
`solidity-lsp` automatically; set `solidity.serverPath` to use your own build.

**Zed** — install [`solidity-zed`](./solidity-zed) as a dev extension
(`zed: install dev extension`). It downloads the server binary, or uses one on
your `PATH`.

**Neovim** (0.11+):

```lua
vim.filetype.add({ extension = { sol = "solidity" } })
vim.lsp.config("solidity_foundry", {
  cmd = { "solidity-lsp" },
  filetypes = { "solidity" },
  root_markers = { "foundry.toml" },
})
vim.lsp.enable("solidity_foundry")
```

**Helix** (`languages.toml`):

```toml
[language-server.solidity-lsp]
command = "solidity-lsp"

[[language]]
name = "solidity"
language-servers = ["solidity-lsp"]
```

**Emacs** (eglot):

```elisp
(with-eval-after-load 'eglot
  (add-to-list 'eglot-server-programs '(solidity-mode . ("solidity-lsp"))))
```

## Features

Implemented:

- **Diagnostics** byte-identical to `forge build` — any solc version (auto-installed), correct imports/remappings, the same warning suppression (`ignored_error_codes`, `ignored_warnings_from`).
- **As-you-type diagnostics** — the unsaved buffer is type-checked live (no codegen, no disk writes).
- **Formatting** via `forge fmt`.
- **Navigation** — go-to-definition, find references, hover (rendered signatures + NatSpec), document & workspace symbols, document highlight. Live from a tree-sitter parse the instant a file opens and as you type; sharpened by the solc AST once a compile lands.
- **Completion** — type-aware member completion after `.`, plus in-scope symbols (available immediately from the parse, before any compile) — and **signature help**.
- **Rename** across declarations and references.
- **Inlay hints** — call-site parameter names (functions, events, errors, struct constructors).
- **Syntax highlighting** — a TextMate grammar in VS Code (tree-sitter in Zed) colors files immediately, then **semantic tokens** recolor each identifier by what it resolves to.
- **Code actions** — `forge lint` fixes, add a missing SPDX identifier or pragma, and import an undeclared symbol from where it's defined.
- **`forge lint`** warnings surfaced inline.
- **Monorepos** — several `foundry.toml` roots open at once each keep their own index.

Planned: solar-based *type-aware* live resolution (live navigation is name-based today, made precise by the solc AST on the next compile); published Marketplace / Open VSX / Zed registry listings.

## License

MIT.
