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

Reads one logical symbol by its canonical `id` at one `revision`; the answer names the
revision it read, and `page` selects its declarations at that revision.
A complete exact miss returns `symbol_not_found` and up to three alternatives.
An unavailable selection returns its typed cause. A found symbol carries its facts and
each declaration's location; `facets` with `symbol` reads a declaration's source. Use
`search` for discovery.

Parameters:

- `id` (required) - Canonical identity.
- `include` - Requested projections.
- `revision` - The revision to read.
- `max_results` - Results per page, at most 10,000.
- `page` - The 1-based page to read.

## search

Performs search across the index using a query language. `language` names the query
language and defaults to `sparql`.
A query over the captured inventory answers from partly covered facts. A query whose
answer needs every fact answers `unavailable` with `insufficient_coverage` when the
selected coverage cannot establish that answer. A bound ends the answer with the rows
it reached. The server records read warnings and bounds in its logs.

Parameters:

- `include` - Attaches the sorted analyses that produced each returned edge.
- `query` (required) - Query text in `language`.
- `language` - The language of `query`.
- `page` - The 1-based page to read.
- `max_results` - Answer rows per page, at most 1000.
- `revision` - The indexed revision to read.

