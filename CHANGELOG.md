# Changelog

All notable, user-facing changes to this project are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versions follow [Semantic Versioning](https://semver.org/) as described in
[CONTRIBUTING.md](CONTRIBUTING.md#versioning): while pre-1.0, patch releases
carry both fixes and backward-compatible features; a minor bump signals a
breaking change.

## [Unreleased]

## [0.3.1] - 2026-07-07

### Added

- Run-test code lens on Foundry test and invariant functions, running
  `forge test` for the one you click.
- Quick-fixes for common solc errors: add a visibility or data-location
  keyword, `override`/`virtual`, `abstract`, a checksummed address, or a
  `view`/`pure` mutability.
- Project-wide navigation backed by a background parse, so go-to-definition,
  references, and symbols work before the first successful compile and while
  the build is broken.
- Signature help, inlay hints, and document highlight fall back to the live
  parser, so they keep working during cold start, mid-edit, and without a
  `foundry.toml`.
- Completion adds sized integer/bytes types, unit and boolean literals, member
  builtins for elementary and array receivers, and NatSpec documentation;
  import completion suggests remapping prefixes and triggers on `/`, `"`, `'`.
- As-you-type diagnostics fall back to the buffer's `pragma` version when the
  project pins no solc, and resolve relative imports for config-less files.
- Hover shows inherited NatSpec for functions documented only with
  `@inheritdoc`.
- `FOUNDRY_PROFILE` selects the active `foundry.toml` profile, and files
  matched by `skip` are excluded from diagnostics to match `forge build`.
- The Zed extension ships an outline (breadcrumbs and outline panel) plus
  richer comment and word-character configuration.
- Release binaries publish SHA-256 checksums, which the VS Code client
  verifies after download.

### Changed

- A release is published only after every platform binary builds (drafted
  until complete), so a partial build can't strand users on a version missing
  their binary.
- Release binaries are stripped, shrinking the download.
- Diagnostics compiles are debounced and coalesced per project root, and live
  type-checks are bounded, so a burst of edits or an external change can't
  spawn unbounded compiles.
- Resolved config and remappings are memoized per root, and the navigation
  index skips rebuilding when the sources haven't changed.

### Fixed

- Opening a file that nests a type inside a same-named container no longer
  crashes the server.
- Rename no longer produces duplicate edits, corrupts a qualified path, or
  stops at a function's override family; it leaves import aliases intact and
  refuses to edit library-dependency sources.
- Compiling one project no longer clears another root's or a standalone file's
  diagnostics and quick-fixes.
- Warnings on files served from the compile cache survive an incremental
  compile.
- Closing a tab with unsaved edits clears its now-stale diagnostics, and
  deleted or renamed sources have theirs cleared too.
- As-you-type diagnostics work on Windows, where drive-letter paths are cased
  and percent-encoded differently.
- Hovering a `mapping` state variable no longer truncates the type at `=>`.
- A slow, superseded live check no longer republishes over newer diagnostics
  or revives a fixed error.
- The Zed client downloads the server matching the extension's version,
  prefers a cached binary so it starts offline, and supports arm64 Linux.
- The VS Code client's server download handles a premature connection close,
  a corrupt cached binary, and two windows activating at once, and recovers a
  failed start on Restart.

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
