# Changelog

All notable, user-facing changes to this project are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versions follow [Semantic Versioning](https://semver.org/) as described in
[CONTRIBUTING.md](CONTRIBUTING.md#versioning): while pre-1.0, patch releases
carry both fixes and backward-compatible features; a minor bump signals a
breaking change.

## [Unreleased]

## [0.3.0] - 2026-06-29

### Added

- Go-to-definition on an import path string jumps to the imported file.

## [0.2.0] - 2026-06-29

### Added

- Call-site inlay hints with parameter names (functions, events, errors,
  struct constructors), served from the live parser, with an experimental
  setting to toggle them.
- Member completion covers an instance's type members and inherited members.
- NatSpec hover documentation renders as structured Markdown.
- NatSpec comments auto-continue on Enter.
- `Solidity: Restart Server` command, a restart prompt when
  `solidity.serverPath` changes, and a `solidity.trace.server` setting.
- Prebuilt server binary for arm64 Linux.
- Diagnostics carry same-file related information (e.g. the other declaration
  in a conflict).

### Changed

- The server picks up `foundry.toml` and `remappings.txt` edits without a
  restart.
- The changed buffer reparses off the message loop for lower input latency.

### Fixed

- Duplicate global completions collapsed and completion order stabilized.
- Server-binary downloads are atomic (via a `.part` file) and time out when
  stalled.
- On-disk diagnostics no longer republish over dirty buffers, and semantic
  tokens wait for a matching index instead of coloring from a stale one.

## [0.1.0] - 2026-06-26

Initial release.

- Diagnostics byte-identical to `forge build`: the project's pinned solc
  auto-installed, imports and remappings resolved exactly like Foundry, and
  as-you-type type-checking of the unsaved buffer.
- Live navigation from a tree-sitter parse — definition, type definition,
  implementation, references, document highlight, hover, document and
  workspace symbols — sharpened by the typed solc AST after each successful
  compile.
- Completion (type-aware members, in-scope symbols, keywords and builtins,
  snippets, import paths), signature help, and rename with a pre-flight that
  rejects invalid names.
- Semantic tokens plus a TextMate grammar; code actions (`forge lint` fixes,
  missing SPDX or pragma, import an undeclared symbol); formatting via
  `forge fmt`; `forge lint` warnings inline.
- Single-file mode without any `foundry.toml`; multi-root monorepo support.
- VS Code and Zed clients that auto-download the matching server binary.

[Unreleased]: https://github.com/u-zzam/solidity-for-foundry/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/u-zzam/solidity-for-foundry/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/u-zzam/solidity-for-foundry/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/u-zzam/solidity-for-foundry/releases/tag/v0.1.0
