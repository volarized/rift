# Rift MCP tools

Generated from the served tool surface.

## facets

Reads every declaration of `symbol` with its location, source, and syntax structure.
The result includes each declaration's syntax tree.
Use `facet_cursor` with the same symbol and revision to continue declarations.
Use `revision` to read a commit or release, or to pin the served working tree.

Parameters:

- `symbol` (required) - The symbol whose declarations the server reads.
- `facet_cursor` - Continuation returned by a preceding read of this symbol at the same revision.
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
revision it read, and `declaration_cursor` continues its declarations at that revision.
A complete exact miss returns `symbol_not_found` and up to three alternatives.
An unavailable selection returns its typed cause. A found symbol carries its facts and
each declaration's location; `facets` with `symbol` reads a declaration's source. Use
`search` for discovery.

Parameters:

- `id` (required) - Canonical identity.
- `include` - Requested projections.
- `revision` - The revision to read.
- `declaration_limit` - Most declarations returned in this read.
- `declaration_cursor` - Continuation a preceding read returned, bound to this symbol and the revision it read.

## search

Performs search across the index using a query language. `language` names the query
language and defaults to `sparql`.

Parameters:

- `query` (required) - Query text in `language`.
- `language` - The language of `query`.

