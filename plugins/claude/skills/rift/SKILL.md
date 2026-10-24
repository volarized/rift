---
name: rift
description: "Use when finding or reading code in a workspace Rift serves: structured search across declarations and source text, symbol reads by exact identity, and syntax inspection."
---

# Rift

Rift indexes this workspace's source and serves it over MCP: structured search, symbol reads by exact identity, and syntax inspection.

Rift writes no source: apply a change with your own edit tools.

Read `rift://map` first for served languages, workspace layout, and language server recommendations. When configuring language servers, run listed `rift config set` commands from the workspace root. After creating a named profile, reconnect MCP to select it in the SPARQL statement. Profiles are defined in the root `rift.toml`; accepted datasets and services select extra workspaces registered through `[mcp.extra]`.

## Which tool

| Situation | Tool |
| --- | --- |
| The target is unknown. | `search` |
| Every match of a regex in the project's text is needed. | `search` (with `:pattern` in `SERVICE :search`) |
| The exact symbol identity is known. | `get_symbol` |
| A package or runtime declaration is needed. | `search` (with `:scope "global"` or `"all"` in `SERVICE :search`; read the returned symbol IRI with `get_symbol`) |
| A package version the project does not use, or a package it lacks, is needed. | `search` (with `:packages` and `:scope "global"` or `"all"` in `SERVICE :search`; read the returned symbol IRI with `get_symbol`) |
| The syntax structure at one position is needed. | `nodes` |
| The declarations that reference a symbol, its impact (what changes when it does), or a path between two symbols is needed. | `search` (with SPARQL property paths) |
| The declarations two revisions, or a revision and the working tree, hold differently are needed. | `search` (with `:change` in `SERVICE :search`) |
| The commit that made a change, found by its message, is needed. | `search` (with `:target "commit"` in `SERVICE :search`) |

## Query recipes

`search` takes required SPARQL `query` and optional `language`, default `sparql`. Prefixes `:`, `r:`, `rdf:`, `rdfs:`, and `xsd:` are supplied. Replace `<symbol-id>` below with an observed canonical symbol IRI.

- Names: `SELECT ?symbol {?symbol a :Symbol; :name "load_config"}`
- Usage identities: `SELECT ?usage {<symbol-id> :usages ?usage}`
- Usage locations: `SELECT * {<symbol-id> :usages ?usage . ?usage :unit ?unit; :start ?start; :end ?end}`
- Distinct callers: `SELECT DISTINCT ?caller {?caller :references <symbol-id>}`
- Direct callees: `SELECT ?callee {<symbol-id> :calls ?callee}`
- Third-level callees: `SELECT ?callee {<symbol-id> :calls/:calls/:calls ?callee}`
- Substring: `SELECT ?symbol {?symbol a :Symbol; :name ?name FILTER(CONTAINS(?name, "config"))}`
- Regex: `SELECT ?path ?start {SERVICE :search {?hit :pattern "TODO|FIXME"; :target "file"; :path ?path; :start ?start}}`
- Count: `SELECT (COUNT(?usage) AS ?count) {<symbol-id> :usages ?usage}`

Ranking uses `SERVICE :search {?hit :query "project settings"; :symbol ?symbol}`. Use outer projection, `ORDER BY`, `LIMIT`, and `OFFSET`. Follow returned `pagination.next_query` unchanged for transport pages. Carry the returned `view` with exact symbol and source reads. Missing required facts return unavailability, never a false boolean or empty complete answer.

The `:` vocabulary is `urn:rift:vocabulary:`. The local named graph is `urn:rift:graph:local`.

## When a call refuses

Read `rift server logs` when a refusal alone does not say why; it carries the workspace's own recorded diagnostics.

See [references/tools.md](references/tools.md) for every served tool's parameters.

## Without the rift CLI

The plugin starts its MCP server by running `rift mcp`, so the `rift` executable must be on `PATH`. When Claude Code lists no rift tools, install it:

- Linux and macOS: `curl --proto '=https' --tlsv1.2 -fsSL https://volar.sh/rift/install.sh | bash`
- Windows PowerShell: `irm https://volar.sh/rift/install.ps1 | iex`

The installer downloads the latest verified release for your platform and installs it for the current user. Reconnect with `/mcp` after installing.

A workspace that already carries a `rift` entry in its own `.mcp.json` (written by `rift install claude`) runs a second proxy beside the plugin's; both reach the same workspace server. Keep one: remove the project entry or uninstall the plugin.
