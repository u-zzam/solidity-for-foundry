# Solidity for Foundry — Zed

Thin Zed client for [`solidity-for-foundry-lsp`](../solidity-lsp). Registers the
Solidity language and runs the shared server, so behavior matches every other editor.

## Install

Install the extension — it uses a `solidity-for-foundry-lsp` already on your
`PATH`, otherwise it downloads the matching binary from the GitHub release. So
there's nothing else to build.

Requirements:

- **[Foundry](https://getfoundry.sh)** (`forge`) on your `PATH` — the server
  shells out to it for `forge fmt` / `forge lint`, and auto-installs the solc
  version your project pins via svm.
- Disable any other Solidity extension and reload — an editor binds each `.sol`
  file to a single language server.

## Install as a dev extension

`zed: install dev extension` → pick this directory. Zed compiles the wasm
extension itself; the manual build below just verifies it:

```sh
rustup target add wasm32-wasip1
cargo build --target wasm32-wasip1
```

To run your own server build, put it on your `PATH`:

```sh
cargo install --path ../solidity-lsp --locked   # installs solidity-for-foundry-lsp
```
