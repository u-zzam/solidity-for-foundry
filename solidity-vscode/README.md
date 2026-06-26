# Solidity for Foundry — VS Code

The Solidity language server for the modern, Foundry-native development stack:
diagnostics, navigation, and completion on your project's exact solc version,
with imports resolved exactly like `forge build`.

## Install

Install the extension — on first activation it downloads the matching
`solidity-for-foundry-lsp` server binary from the GitHub release, so there's
nothing else to build.

Requirements:

- **[Foundry](https://getfoundry.sh)** (`forge`) on your `PATH` — the server
  shells out to it for `forge fmt` / `forge lint`, and auto-installs the solc
  version your project pins via svm.
- Disable any other Solidity extension (Juan Blanco, Nomic) and reload — an
  editor binds each `.sol` file to a single language server.

## Settings

- `solidity.serverPath` — path to your own `solidity-for-foundry-lsp` build.
  Leave empty (the default) to auto-download the release binary.

## Build from source

To run your own server build instead of the downloaded one:

```sh
cargo install --path ../solidity-lsp --locked   # installs solidity-for-foundry-lsp
```

then point `solidity.serverPath` at it. For client development:

```sh
npm install
npm run compile      # outputs to ./out; press F5 for an Extension Development Host
```
