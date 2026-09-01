---
name: rift
description: Use when finding, reading, or editing code in a workspace Rift serves: structured search across declarations and source text, symbol reads by exact name, syntax inspection, and witnessed edits that recompute their address before writing and run the workspace's configured hooks.
---

# Rift

Rift indexes this workspace's source and serves it over MCP: structured search, symbol reads by exact name, syntax inspection, and edits that carry their own address so a stale one refuses instead of splicing into moved code.

Start an unfamiliar repository at `rift://map`; it names the served languages and the workspace's own layout before any tool call.

## Which tool

| Situation | Tool |
| --- | --- |
| The target is unknown. | `search` |
| The declaration name is known. | `get_symbol` |
| A dependency's public declaration is needed. | `get_symbol` (with `scope: "dependencies"`, or `"all"` to list the project's own first) |
| The syntax structure at one position is needed. | `nodes` |
| A symbol's neighbors, its impact (who breaks when it changes), or a path between two symbols is needed. | `search` (with a `traversal` block) |
| Code needs to change. | `patch`, `replace_node`, `insert_node`, `insert_symbol`, `replace_symbol`, `rename_symbol`, `remove_node`, `remove_symbol`, `move_file` (over raw file writes: the server recomputes witnesses and runs the workspace's configured hooks) |

The edit tools apply through the server, which recomputes each address's witness before writing and refuses when the source moved since the address was read. Prefer them over writing files directly: a raw write bypasses that check and the workspace's configured hooks.

## When a call refuses

Read `rift://logs` when a refusal alone does not say why; it carries the workspace's own recorded diagnostics.

See [references/tools.md](references/tools.md) for every served tool's parameters.

## Without the rift CLI

The plugin starts its MCP server by running `rift mcp`, so the `rift` executable must be on `PATH`. When Claude Code lists no rift tools, install it:

- Linux and macOS: `curl --proto '=https' --tlsv1.2 -fsSL https://volar.sh/rift/install.sh | bash`
- Windows PowerShell: `irm https://volar.sh/rift/install.ps1 | iex`

The installer downloads the latest verified release for your platform and installs it for the current user. Reconnect with `/mcp` after installing.

A workspace that already carries a `rift` entry in its own `.mcp.json` (written by `rift install claude`) runs a second proxy beside the plugin's; both reach the same workspace server. Keep one: remove the project entry or uninstall the plugin.
