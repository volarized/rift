# Rift MCP tools

Generated from the served tool surface.

## get_source

Reads a canonical source unit or a half-open UTF-8 byte range. A preceding `view`
retains its immutable selection and original expiry; `rev` selects a local Git revision.
The result returns source bytes, definite absence or typed selection unavailability.

Parameters:

- `unit` (required) - Canonical source unit, retaining its physical package or runtime owner.
- `range` - Requested half-open UTF-8 byte range.
- `view` - A preceding immutable selection.
- `rev` - A local Git revision.

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

Lists the syntax nodes covering one UTF-8 byte position in one file,
outermost first. Each identity carries a witness, so an address taken
from this listing refuses cleanly once the file's bytes drift. `rev`
lists the nodes as of a version-control revision instead of the
current tree. If discovery is incomplete or a targeted read spends the
request's remaining time, the result has empty `nodes` and `source` and
carries `local_index_preparing`. A selected source path can answer
within that remaining time, with the same warning until the publication
records the path: workspace byte and declaration bounds remain unchecked.
Per-file refusals keep their typed errors. A visible path no syntax
provider parses refuses `capability_unavailable`, naming the extension.

Parameters:

- `path` (required) - Project-relative file to inspect.
- `position` (required) - UTF-8 byte offset the listed nodes must cover - one position, not a range; the nodes themselves carry the spans.
- `view` - Captured view selected by discovery.
- `rev` - The version-control revision to read - a branch, tag, or commit id as the workspace's version control spells it.

## search

Evaluates SPARQL Query over selected workspace facts.
The server supplies `:` and `r:` vocabulary prefixes and standard RDF prefixes.
`SELECT`, `ASK`, `CONSTRUCT`, and `DESCRIBE` preserve RDF terms and projected fields.
For example, `SELECT ?symbol { ?symbol :name "ReadService" }` finds declarations.
`SELECT ?usage { <rift://symbol/local/rust/rift_server/read/ReadService> :usages ?usage }`
selects every reference occurrence. Required unavailable facts prevent evaluation.

Parameters:

- `query` (required) - Query text.
- `language` - Currently only SPARQL is supported.

