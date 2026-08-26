# Rift

[![Rift - agentic development toolkit for codebases](docs/public/og.png)](https://volar.sh/rift/)

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

The installers select the native x86-64 or Arm64 archive, verify the release checksum, and
install under the current user account. Without a version argument or `RIFT_VERSION`, each
installer resolves the latest release.

## MCP

Run `rift mcp` from a codebase. Rift exposes structured reads (`search`, `get_symbol`, `nodes`)
and precondition-guarded changes (`replace_symbol`, `insert_symbol`, `replace_node`,
`insert_node`, `patch`, `rename_symbol`, `move_file`, `remove_symbol`, `remove_node`) over stdio
MCP. Later reads include edits made through Rift or another filesystem tool, such as a formatter.

This repository's `.mcp.json` runs the local build through Cargo. An installed client configuration
uses `rift` as command and `["mcp"]` as arguments.

## Update

```sh
rift update
```

The command downloads the latest checksummed native release, validates its version, and
atomically replaces the current executable.

## Protocol development

The protocol models are Rust types in `crates/rift-protocol/src`, and the MCP server in
`crates/rift-mcp` serves tools built from them. The document at `docs/public/mcp.json` is the
served tool surface - names, descriptions, and JSON Schemas serialized from the same tool router
the server runs. Regenerate and verify the export with:

```sh
just generate
just generate-check
```

## Rust development

Rust uses the toolchain pinned by `rust-toolchain.toml`. Install `uv`, `just`, `cargo-audit`,
`cargo-deny`, and `cargo-llvm-cov`, then run the same gates as CI from the repository root:

| Command | Gate |
| --- | --- |
| `just format` | Rust formatting |
| `just generate-check` | Generated protocol drift |
| `just check` | Lock freshness, crate edges, and binary ownership |
| `just clippy` | Strict Clippy policy |
| `just docs` | Warning-free Rust documentation |
| `just audit` | Advisory, license, ban, and source policy |
| `just test` | Every suite, live engines included; writes coverage and holds an 86% line floor |
| `just release-test` | Deterministic release archive contract |
| `just installer-test` | Offline curl and irm installer contract |
| `just rust-gate` | Every gate above |

GitHub Actions runs each target separately, so a failed gate stays visible by name.

Install the pre-commit hook with `uvx pre-commit install`. It runs `just rust-gate`, including
the same coverage and Cargo policy checks CI enforces.

## Rift releases

Pushing a `vX.Y.Z` tag on a commit from `main` starts one Rift release pipeline. The tag version
must match the workspace version. Release environment approval gates publication after native
binaries and current documentation pass their validation gates.

Each release contains checksummed archives with GitHub artifact attestations for Linux, macOS,
and Windows on x86-64 and Arm64. Unix archives contain `rift`; Windows archives contain
`rift.exe`. After release publication, the same pipeline deploys the latest documentation and
installers to `volar.sh/rift`.
