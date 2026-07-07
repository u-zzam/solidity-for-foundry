# Contributing

Thanks for helping out. This project values small, focused changes — the
smallest thing that works.

## Ground rules

- **Foundry-only by design.** No Hardhat / Truffle / Brownie support — please
  don't open PRs adding it.
- **Lean code.** No new dependencies for what a few lines can do, no
  speculative abstractions, no scaffolding "for later".
- **Atomic commits.** One logical unit of working code per commit. Messages
  describe what the change implements (e.g. "Map solc diagnostics to LSP
  ranges").

## Development

The server is the main surface (`solidity-lsp/`, Rust):

```sh
cargo build
cargo test
cargo clippy --all-targets   # must stay clean — CI runs it with -D warnings
```

`forge` must be on `PATH` — the server shells out to it for `forge fmt` /
`forge lint`, and the project's pinned solc is auto-installed via svm on first
compile.

The editor clients are thin process-spawners:

- **VS Code** (`solidity-vscode/`): `npm install && npm run compile`, or press
  F5 for a dev host. To run your local server build instead of the downloaded
  binary, set `solidity.serverPath` to
  `<repo>/target/debug/solidity-for-foundry-lsp`.
- **Zed** (`solidity-zed/`): install as a dev extension
  (`zed: install dev extension`). It is excluded from the Cargo workspace
  (wasm target, own toolchain).

To sanity-check a change, open a `.sol` file in a real Foundry project:
diagnostics should match `forge build` exactly, imports should resolve, and
navigation should work.

## Pull requests

- Keep `cargo test` green and `cargo clippy --all-targets` clean — CI enforces
  both, plus a compile of the VS Code client.
- User-facing change? Add a line under `[Unreleased]` in
  [`CHANGELOG.md`](CHANGELOG.md). Prefix breaking entries with **Breaking:**.
- **Never bump version numbers in a PR.** Releases are cut separately by the
  maintainer (see below).

## Reporting bugs

Use the bug-report issue template. The single most useful piece of triage
information: what does `forge build` say for the same file? Diagnostics are
meant to be byte-identical to it.

## Versioning

[Semantic Versioning](https://semver.org/), currently pre-1.0, following the
Cargo convention:

- **Patch** (`0.3.0 → 0.3.1`) — the default. Every backward-compatible change:
  bug fixes *and* new features alike.
- **Minor** (`0.3.x → 0.4.0`) — breaking changes only: a removed or renamed
  setting, a raised minimum requirement, anything that forces users to act.
- **`1.0.0`** — an explicit stability commitment; the maintainer's call.

When unsure, it's a patch.

## Releasing (maintainers)

One version, everywhere: a release bumps the same `X.Y.Z` in six files —
`solidity-lsp/Cargo.toml`, root `Cargo.lock`, `solidity-vscode/package.json`,
`solidity-zed/Cargo.toml`, `solidity-zed/Cargo.lock`,
`solidity-zed/extension.toml`.

1. Land the feature commits, then a separate `Release vX.Y.Z` commit that
   bumps the six files and moves `[Unreleased]` in `CHANGELOG.md` under the
   new version.
2. Push `main`, then push the tag `vX.Y.Z`. The tag drives
   `.github/workflows/release.yml`, which builds the per-platform server
   binaries and creates the GitHub release. Never create the release by hand —
   a manual release pre-empts the workflow and ships without binary assets.
3. Publish the clients manually at the same version: VS Code Marketplace,
   Open VSX, and the Zed extension registry. Package the VS Code client with
   its dependencies (`cd solidity-vscode && npm run compile && npx vsce
   package`), never `--no-dependencies`.

The VS Code client downloads the server from
`releases/download/v<package.json version>/…`, so a published extension
version must equal a release tag that actually has binaries attached.
