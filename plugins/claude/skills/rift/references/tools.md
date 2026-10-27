# Rift MCP tools

Generated from the served tool surface.

## get_symbol

Reads one logical symbol by its canonical `id`. The caller may select a preceding
immutable `view`; continuations retain that selection and its original expiry.
A complete exact miss returns `symbol_not_found` and up to three alternatives.
An unavailable selection returns its typed cause. Use `search` for discovery.

Parameters:

- `id` (required) - Canonical identity.
- `include` - Requested projections.
- `rev` - A local Git revision.
- `view` - A view returned by a preceding read.
- `declaration_limit` - Most declarations returned in this read.
- `declaration_cursor` - Continuation bound to this exact symbol, projections and serving view.

## nodes

Evaluates SPARQL Query over selected syntax facts.
The server supplies `:` and `r:` vocabulary prefixes and standard RDF prefixes.
`SELECT`, `ASK`, `CONSTRUCT`, and `DESCRIBE` preserve RDF terms and projected fields.
Required unavailable facts prevent evaluation.

Parameters:

- `query` (required) - Query text.
- `language` - Currently, only SPARQL is supported.

## search

Evaluates SPARQL Query over selected workspace facts.
The server supplies `:` and `r:` vocabulary prefixes and standard RDF prefixes.
`SELECT`, `ASK`, `CONSTRUCT`, and `DESCRIBE` preserve RDF terms and projected fields.
For example, `SELECT ?symbol { ?symbol :name "ReadService" }` finds declarations.
`SELECT ?usage { <rift://symbol/local/rust/rift_server/read/ReadService> :usages ?usage }`
selects every reference occurrence. Required unavailable facts prevent evaluation.

Parameters:

- `query` (required) - Query text.
- `language` - Currently, only SPARQL is supported.

