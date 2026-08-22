# Rift

[![Rift — agentic development toolkit for codebases](docs/public/og.png)](https://volar.sh/rift/)

Rift is an agentic development toolkit for reading, discovering, and editing codebases.

📖 [Read the documentation](https://volar.sh/rift/docs/)

## Install

Linux and macOS:

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://volar.sh/rift/install.sh | bash
```

Pass `--version` after `bash -s --` to install an exact release:

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://volar.sh/rift/install.sh | bash -s -- --version v0.0.2
```

Windows PowerShell:

```powershell
irm https://volar.sh/rift/install.ps1 | iex
```

Invoke the downloaded script block with `-Version` to install an exact release:

```powershell
& ([scriptblock]::Create((irm https://volar.sh/rift/install.ps1))) -Version v0.0.2
```

Installers select native x86-64 or Arm64 archive, verify release checksum, and install under current
user account. Without a version argument or `RIFT_VERSION`, each installer resolves latest release.

## MCP

Run `rift mcp` from a Rust codebase. Rift scans real `.rs` files into an in-memory tree-sitter index
and exposes `search` and `get_symbol` over stdio MCP.

This repository's `.mcp.json` runs the local build through Cargo. An installed client configuration
uses `rift` as command and `["mcp"]` as arguments.

## Update

```sh
rift update
```

The command downloads latest checksummed native release, validates its version, and atomically
replaces current executable.

## Protocol development

The protocol models are Rust types in `crates/rift-protocol/src`, and the MCP server in
`crates/rift-mcp` serves tools built from them. The document at `docs/public/mcp.json` is the
served tool surface — names, descriptions, and JSON Schemas serialized from the same tool router
the server runs. Regenerate and verify the export with:

```sh
just generate
just generate-check
```

## Rust development

Rust uses toolchain pinned by `rust-toolchain.toml`. Install `uv`, `just`, `cargo-audit`,
`cargo-deny`, and `cargo-llvm-cov`, then run same gates as CI from repository root:

| Command | Gate |
| --- | --- |
| `just format` | Rust formatting |
| `just generate-check` | Generated protocol drift |
| `just check` | All targets, features, crate edges, and binary ownership |
| `just test` | Workspace tests |
| `just clippy` | Strict Clippy policy |
| `just docs` | Warning-free Rust documentation |
| `just audit` | Advisory, license, ban, and source policy |
| `just coverage` | CLI coverage report and hard 86% line floor |
| `just release-test` | Deterministic release archive contract |
| `just installer-test` | Offline curl and irm installer contract |
| `just rust-gate` | Every gate above |

GitHub Actions runs each target separately so failed gate remains visible by name.

Install pre-commit hook with `uvx pre-commit install`. It runs `just rust-gate`, including same
coverage and Cargo policy checks enforced by CI.

## Rift releases

Pushing a `vX.Y.Z` tag on a commit from `main` starts one Rift release pipeline. Tag version must
match workspace version. Release environment approval gates publication after native binaries and
current documentation pass their validation gates.

Each release contains checksummed, provenance-attested archives for Linux, macOS, and Windows on
x86-64 and Arm64. Unix archives contain `rift`; Windows archives contain `rift.exe`. After release
publication, same pipeline deploys latest documentation and installers to `volar.sh/rift`.
