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

## Editors

VS Code · Zed · Neovim · Helix · Emacs (any LSP client).

## Roadmap

- **P1** diagnostics + formatting (alpha)
- **P2** navigation: go-to-def, references, hover, symbols (beta)
- **P3** completion, signature help, rename, as-you-type (1.0 — parity)
- **P4** lint, inlay hints, code actions (1.x — surpass)

## License

MIT.
