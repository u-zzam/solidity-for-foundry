# Solidity for Foundry — VS Code

Thin VS Code client for [`solidity-for-foundry-lsp`](../solidity-lsp): a
Foundry-native Solidity language server that uses your project's exact solc
version and resolves imports exactly like `forge build`.

## Requirements

The `solidity-for-foundry-lsp` server binary on your `PATH` (or set `solidity.serverPath`):

```sh
cargo install --path ../solidity-lsp --locked   # or use a release binary
```

## Develop

```sh
npm install
npm run compile      # outputs to ./out
```

Then press F5 in VS Code to launch an Extension Development Host.

## Settings

- `solidity.serverPath` — path to the `solidity-for-foundry-lsp` binary (default: `solidity-for-foundry-lsp`).
