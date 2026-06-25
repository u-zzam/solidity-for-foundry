# solidity

A **Foundry-native Solidity language server** — the same behavior in every editor, always on your project's exact solc version, with imports resolved exactly like `forge build`.

> Status: pre-implementation. See [DESIGN.md](./DESIGN.md) for the full plan.

## Why

Foundry has no official LSP. Today you get a different third-party server per editor (Nomic on VS Code, Juan Blanco on Neovim), each with its own gaps:

- **Nomic** lags solc versions (can't validate projects on recent compilers).
- **Juan Blanco** mis-resolves Foundry remappings (false import errors).
- Behavior differs across editors; nothing is truly Foundry-native.

This is one shared server that fixes all three: identical everywhere, current solc (auto-installed via svm), and import resolution byte-identical to `forge build` (it reuses `foundry-compilers`, the engine `forge` itself uses).

## How it works

- **Diagnostics** come from **solc** (via `foundry-compilers`) → squiggles match `forge build` exactly, on any version.
- **Code intelligence** (go-to-def, hover, completion) is powered by the **typed solc AST** plus **solar** for fast, error-tolerant live parsing.
- One Rust server; thin clients for VS Code and Zed; a one-line config for Neovim/Helix/Emacs.

## Install

Build and install the server binary (requires Rust and `forge` on your `PATH`):

```sh
cargo install --path solidity-lsp --locked
```

This puts `solidity-lsp` on your `PATH`. (`--locked` uses the pinned
`Cargo.lock`; required on Rust < 1.95.) It auto-downloads the solc version your
project pins (via svm) on first compile.

## Editor setup

Every editor runs the *same* `solidity-lsp` binary, so behavior is identical.

**VS Code** — install the [`solidity-vscode`](./solidity-vscode) extension
(press F5 from that folder for a dev host). It spawns `solidity-lsp` automatically.

**Zed** — install [`solidity-zed`](./solidity-zed) as a dev extension
(`zed: install dev extension`).

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
- **Navigation** — go-to-definition, find references, hover (rendered signatures + NatSpec), document & workspace symbols.
- **Completion** — type-aware member completion after `.`, plus in-scope symbols — and **signature help**.
- **Rename** across declarations and references.
- **`forge lint`** warnings surfaced inline.

Planned: inlay hints, code actions, semantic tokens, packaged per-editor releases.

## License

MIT.
