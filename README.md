# Rift

[![Rift - agentic development toolkit for codebases](docs/public/og.png)](https://volar.sh/rift/)

Rift is an agentic development toolkit for reading and discovering codebases.

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

Run `rift mcp` from a codebase. Rift exposes structured reads through `search`, `get_symbol`,
and `nodes` over stdio MCP. Later reads include filesystem changes made by a formatter or
another process.

This repository's `.mcp.json` runs the local build through Cargo. An installed client configuration
uses `rift` as command and `["mcp"]` as arguments.

## Claude Code

Run `rift install claude` from a codebase to write a Claude Code skill at `.claude/skills/rift/`
teaching the agent to reach for Rift's tools. The skill is regenerated from the served tool
surface, so its tool names track whatever this build serves; rerun the command after an upgrade.
Pass `--user` to install under the operator's home directory instead of the workspace, and
`--remove` to delete the generated skill. The command also writes a `PreToolUse` hook that runs
`rift steer`, redirecting the agent's first `Grep` or `Glob` call in a session to the rift search
tool; set `RIFT_STEER=0` to disable it.

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
`cargo-deny`, `cargo-llvm-cov`, `cargo-nextest`, and `oas3-gen 0.28.0`, then run checks from the
repository root.
Python developer tooling lives in the locked `rift-dev` package under `dev/`:

| Command | Check |
| --- | --- |
| `just format` | Rust formatting |
| `just generate-check` | Generated protocol drift |
| `just check` | Lock freshness, crate edges, and binary ownership |
| `just clippy` | Strict Clippy policy |
| `just docs` | Warning-free Rust documentation |
| `just audit` | Advisory, license, ban, and source policy |
| `just test` | Unit tests without model downloads or corpus checkouts |
| `just doctest` | Rust documentation examples |
| `just release-test` | Release archive contract |
| `just installer-test` | Offline installer contract |
| `just testing-check` | Python lint, types, and tests |
| `just rust-gate` | Local formatting, static checks, and unit tests |
| `just corpus-sync [name]` | Fetch the pinned Bun, Next.js, and FastAPI trees |
| `just corpus-test <name>` | Corpus suite for `bun`, `nextjs`, or `fastapi` |
| `just artifact-test` | Build and serve the native release binary |
| `just agent-test` | Validate three MCP tools and three resources |
| `just live-test` | Live language-engine and embedding model suites |
| `just integration-archive` | Build one CLI and test archive for integration jobs |
| `just integration-test` | Live language engines, models, corpus repositories, and served binary checks |

Unit tests, live language-engine suites, the corpus, artifact, and agent checks, and the native
checks on every release target all run on pull requests and pushes to `main`. Each target builds
once, and every check that can share a build runs from it: CI reuses compiled test archives and
caches dependencies.
See [Testing](docs/content/docs/developer/testing.mdx) for prerequisites and commands.

Run the developer package directly with:

```sh
uv run --locked --project dev rift-dev --help
```

Install the pre-commit hook with `uvx pre-commit install`. It runs `just rust-gate`.

## Rift releases

Pushing a `vX.Y.Z` tag on a commit from `main` starts the release pipeline. The tag version
must match the workspace version. The pipeline builds native binaries, packages checksummed
archives, and publishes the release.

Each release contains archives with GitHub artifact attestations for Linux, macOS, and Windows
on x86-64 and Arm64. Unix archives contain `rift`; Windows archives contain `rift.exe`.
The pipeline also deploys documentation and installers to `volar.sh/rift`.
