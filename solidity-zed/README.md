# Solidity (Foundry) — Zed

Thin Zed client for [`solidity-lsp`](../solidity-lsp). Registers the Solidity
language and runs the shared server, so behavior matches every other editor.

## Requirements

The `solidity-lsp` binary on your `PATH`:

```sh
cargo install --path ../solidity-lsp
```

## Install (dev)

Build the wasm extension and install it as a dev extension in Zed
(`zed: install dev extension` → pick this directory):

```sh
rustup target add wasm32-wasip1
cargo build --target wasm32-wasip1
```

Zed compiles the extension itself on install; the manual build above just
verifies it.
