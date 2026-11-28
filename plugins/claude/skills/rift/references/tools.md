# Rift MCP tools

Generated from the served tool surface.

## facets

Reads every declaration of `symbol` with its location, source, and syntax structure.
The result includes each declaration's syntax tree.
`page` selects declarations at the revision the answer names.
Use `revision` to read a commit or release, or to pin the served working tree.

Parameters:

- `symbol` (required) - The symbol whose declarations the server reads.
- `max_results` - Results per page, at most 100.
- `page` - The 1-based page to read.
- `revision` - The revision to read.

## feedback

Sends a feedback report over HTTPS to the public Rift intake, or to the intake that
`RIFT_FEEDBACK_URL` names in the server's environment. The server sends the
caller's text and any selected diagnostic fields. The caller MUST omit secrets,
personal data, and private source material. The caller MUST obtain the user's
approval for any exact conversation excerpt before including it in a report.

Parameters:

- `text` (required) - Report text sent verbatim.
- `include` - Diagnostic fields to send alongside `text`.

## get_symbol

Reads one logical symbol by its canonical `id` and reports the selected `revision`.
`page` selects declarations at that revision. Documentation and declaration source are
selected by default. `include` adds sections and `exclude` removes them. A complete miss
returns no content. An unavailable selection returns its typed cause. Use `search` to
discover identities and `facets` to read syntax structure.

Parameters:

- `id` (required) - Canonical identity.
- `include` - Sections to add to the default `documentation` and `source` selections.
- `exclude` - Sections to remove after applying the defaults and `include`.
- `revision` - Selects a commit, release, or served tree.
- `page` - The 1-based declaration page.
- `max_results` - Declarations per page, at most 10,000.

## search

Performs search across the index using a query language. `language` names the query
language and defaults to `sparql`.
A query over the captured inventory answers from partly covered facts. A query whose
answer needs every fact answers `unavailable` with `insufficient_coverage` when the
selected coverage cannot establish that answer. A bound ends the answer with the rows
it reached. The server records read warnings and bounds in its logs.

Parameters:

- `include` - Optional sections to attach to the query result.
- `query` (required) - Query text in `language`.
- `language` - The language of `query`.
- `page` - The 1-based page to read.
- `max_results` - Answer rows per page, at most 1000.
- `revision` - The indexed revision to read.

