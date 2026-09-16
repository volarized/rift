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
`cargo-deny`, `cargo-llvm-cov`, and `cargo-nextest`, then run the same gates as CI from the
repository root. The testing scripts use Python 3.12 and the locked environment in `scripts/`:

| Command | Gate |
| --- | --- |
| `just format` | Rust formatting |
| `just generate-check` | Generated protocol drift |
| `just check` | Lock freshness, crate edges, and binary ownership |
| `just clippy` | Strict Clippy policy |
| `just docs` | Warning-free Rust documentation |
| `just audit` | Advisory, license, ban, and source policy |
| `just test` | One instrumented fast-tier run, live engines included; holds an 86% line floor |
| `just release-test` | Deterministic release archive contract |
| `just installer-test` | Offline curl and irm installer contract |
| `just testing-check` | Python lint, types, and tests |
| `just rust-gate` | Every gate above |
| `just corpus-sync [name]` | Fetch the pinned Bun, Next.js, and FastAPI trees |
| `just corpus-test <name>` | Instrumented corpus suite for `bun`, `nextjs`, or `fastapi` |
| `just artifact-test` | Build and serve the native release binary |
| `just agent-test` | Validate all 12 MCP tools, nine edits, and three resources |
| `just coldstart-test --binary <linux-binary>` | First use in a Linux container without network access |
| `just full-gate [linux-binary]` | Fast tier, corpus, artifact, agent, and cold checks in sequence |

GitHub Actions runs each target separately, so a failed gate stays visible by name.
The fast Rust job has a 12-minute budget; each fast test has a 60-second deadline.
The full tier runs nightly, on demand, on PRs labeled `full-gate`, and before release promotion.
Each full-tier job has a 20-minute budget. Local corpus suites run one at a time.
See [Testing](docs/content/docs/testing.mdx) for prerequisites, evidence, and release commands.

Install the pre-commit hook with `uvx pre-commit install`. It runs `just rust-gate`, including
the same coverage and Cargo policy checks CI enforces.

## Rift releases

Pushing a `vX.Y.Z` tag on a commit from `main` starts one Rift release pipeline. The tag version
must match the workspace version. The pipeline creates a draft after native binaries and current
documentation pass their validation gates. The release stays in draft until the full tier and
installer and upgrade checks pass on all six targets. Before promotion, recorded checksums must
still match the draft assets. The existing release environment gates draft creation and promotion.

Each release contains checksummed archives with GitHub artifact attestations for Linux, macOS,
and Windows on x86-64 and Arm64. Unix archives contain `rift`; Windows archives contain
`rift.exe`. After release publication, the same pipeline deploys the latest documentation and
installers to `volar.sh/rift`.
