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
| The syntax structure is needed. | `nodes` (with SPARQL Query) |
| A source unit or byte range is needed. | `search` (with `:unit` and optional `:range` in `SERVICE :source`) |
| The declarations that reference a symbol, its impact (what changes when it does), or a path between two symbols is needed. | `search` (with SPARQL property paths) |
| The declarations two revisions, or a revision and the working tree, hold differently are needed. | `search` (with `:change` in `SERVICE :search`) |
| The commit that made a change, found by its message, is needed. | `search` (with `:target "commit"` in `SERVICE :search`) |

## Query recipes

`search` and `nodes` take required `query` text and optional `language`, default `sparql`. Prefixes `:`, `r:`, `rdf:`, `rdfs:`, and `xsd:` are supplied. Replace `<symbol-id>` below with an observed canonical symbol IRI.

- Names: `SELECT ?symbol {?symbol a :Symbol; :name "load_config"}`
- Usage identities: `SELECT ?usage {<symbol-id> :usages ?usage}`
- Usage locations: `SELECT * {<symbol-id> :usages ?usage . ?usage :unit ?unit; :start ?start; :end ?end}`
- Distinct callers: `SELECT DISTINCT ?caller {?caller :references <symbol-id>}`
- Direct callees: `SELECT ?callee {<symbol-id> :calls ?callee}`
- Third-level callees: `SELECT ?callee {<symbol-id> :calls/:calls/:calls ?callee}`
- Substring: `SELECT ?symbol {?symbol a :Symbol; :name ?name FILTER(CONTAINS(?name, "config"))}`
- Regex: `SELECT ?path ?start {SERVICE :search {?hit :pattern "TODO|FIXME"; :target "file"; :path ?path; :start ?start}}`
- Count: `SELECT (COUNT(?usage) AS ?count) {<symbol-id> :usages ?usage}`

### Text search

Find declarations by text, keeping the retrieval order. Optional fields remain unbound when the selected hit does not carry them.

```sparql
SELECT ?symbol ?name ?path
WHERE {
SERVICE :search {
?hit :query "request deadline"; :target "symbol"; :index ?index .
OPTIONAL { ?hit :symbol ?symbol . OPTIONAL { ?symbol :name ?name } }
OPTIONAL { ?hit :path ?path }
}
}
ORDER BY ?index LIMIT 20 OFFSET 0
```

### Match search

Match a regex in selected files. Structured retrieval inputs use `rdf:JSON`.

```sparql
SELECT ?path ?line ?source
WHERE {
SERVICE :search {
?hit :pattern "TODO|FIXME"; :target "file"; :index ?index; :path ?path;
:paths '{"include":["crates/**"]}'^^rdf:JSON;
:include '["source"]'^^rdf:JSON .
OPTIONAL { ?hit :line ?line }
OPTIONAL { ?hit :source ?source }
}
}
ORDER BY ?index LIMIT 20 OFFSET 0
```

### Traversal

Select a declaration by its name and path, then follow two call edges. This example starts at Rift's CLI entry point; use a name and path from the served workspace. Required relationships must be available from the selected provider.

```sparql
SELECT ?callee ?name ?path
WHERE {
?seed a :Symbol; :name "main"; :path "crates/rift/src/main.rs" .
?seed :calls/:calls ?callee .
OPTIONAL { ?callee :name ?name }
OPTIONAL { ?callee :path ?path }
}
ORDER BY ?name ?path LIMIT 20
```

### Source reads

Read a complete source unit or a half-open UTF-8 byte range. Replace `<rift://source/project/crates/rift/src/main.rs>` with an observed source-unit IRI. Whole-file reads omit `:range`; captured reads add `:view` with the original captured view ID.

```sparql
SELECT ?outcome ?text ?start ?end ?complete
WHERE {
SERVICE :source {
?read :unit <rift://source/project/crates/rift/src/main.rs>; :range '{"start":0,"end":4}'^^rdf:JSON;
:outcome ?outcome .
OPTIONAL {
?read :source [ :text ?text; :span [ :range [ :start ?start; :end ?end ] ] ];
:source_complete ?complete .
}
}
}
```

Set `OFFSET 20` to select the next query window. Follow returned `pagination.next_query` unchanged for transport pages. Carry the returned `view` with exact symbol and source reads. Missing required facts return unavailability, never a false boolean or empty complete answer.

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
