---
name: rift
description: "Use when finding or reading code in a workspace Rift serves: structured search across declarations and source text, symbol reads by exact identity, and syntax inspection."
---

# Rift

Rift indexes this workspace's source and serves it over MCP: structured search, symbol reads by exact identity, and syntax inspection.

Rift writes no source: apply a change with your own edit tools.

Read `rift://map` first for served languages, workspace layout, and language server recommendations. When configuring language servers, run listed `rift config set` commands from the workspace root. After creating a named profile, reconnect MCP to use its `local:name` scope. Profiles are defined in the root `rift.toml`; `local@name` selects an extra workspace registered through `[mcp.extra]`, and `local@name:profile` selects a profile defined in that workspace's own `rift.toml`.

## Which tool

| Situation | Tool |
| --- | --- |
| The target is unknown. | `search` |
| Every match of a regex in the project's text is needed. | `search` (with `pattern`) |
| The exact symbol identity is known. | `get_symbol` |
| A package or runtime declaration is needed. | `search` (with `scope: "global"` or `"all"`; read the returned `id` with `get_symbol`) |
| A package version the project does not use, or a package it lacks, is needed. | `search` (with `packages` and `scope: "global"` or `"all"`; read the returned `id` with `get_symbol`) |
| A symbol's source, or the syntax nodes of its declarations, is needed. | `nodes` (with `symbol`, a project, package, or runtime one; send `node_cursor` back for the rest of a long answer) |
| The syntax structure at one position is needed. | `nodes` |
| The declarations that reference a symbol, its impact (what changes when it does), or a path between two symbols is needed. | `search` (with a `traversal` block) |
| The declarations two revisions, or a revision and the working tree, hold differently are needed. | `search` (with a `change` block) |
| The commit that made a change, found by its message, is needed. | `search` (with `target: "commit"`) |

## When a call refuses

Read `rift server logs` when a refusal alone does not say why; it carries the workspace's own recorded diagnostics.

See [references/tools.md](references/tools.md) for every served tool's parameters.

## Without the rift CLI

The plugin starts its MCP server by running `rift mcp`, so the `rift` executable must be on `PATH`. When Claude Code lists no rift tools, install it:

- Linux and macOS: `curl --proto '=https' --tlsv1.2 -fsSL https://volar.sh/rift/install.sh | bash`
- Windows PowerShell: `irm https://volar.sh/rift/install.ps1 | iex`

The installer downloads the latest verified release for your platform and installs it for the current user. Reconnect with `/mcp` after installing.

A workspace that already carries a `rift` entry in its own `.mcp.json` (written by `rift install claude`) runs a second proxy beside the plugin's; both reach the same workspace server. Keep one: remove the project entry or uninstall the plugin.
