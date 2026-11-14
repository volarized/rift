# Rift MCP tools

Generated from the served tool surface.

## get_symbol

Reads one logical symbol by its canonical `id` at one `revision`; the answer names the
revision it read, and `declaration_cursor` continues its declarations at that revision.
A complete exact miss returns `symbol_not_found` and up to three alternatives.
An unavailable selection returns its typed cause. A found symbol carries its facts and
each declaration's location; `nodes` with `symbol` reads a declaration's source. Use
`search` for discovery.

Parameters:

- `id` (required) - Canonical identity.
- `include` - Requested projections.
- `revision` - The revision to read.
- `declaration_limit` - Most declarations returned in this read.
- `declaration_cursor` - Continuation a preceding read returned, bound to this symbol and the revision it read.

## nodes

Lists syntax nodes with their source text: every node of one `symbol`, or, for one
UTF-8 byte `position` in one file, every node of the symbol whose declaration holds
it, or the file's unit nodes when no declaration holds it. A symbol read is the way to
read a declaration's source, and a package or runtime symbol's nodes come from the
global API, each in a source unit of that symbol's release. A path read can select
a registered project or profile with `scope`. The answer names `symbol`;
each node is addressed by its place: node `index` of declaration `declaration` of
that symbol, or node `index` of its unit, from the file node at 0, with `parent` an
index in the same declaration or unit. An edit renumbers the nodes after it. An answer
whose nodes cross its node count or source byte bound returns `node_cursor` for the
rest. A file no syntax provider parses answers its file node alone. `revision` lists
the nodes at a git commit or pins the served working tree, and the answer names the
revision it was read at. If discovery is incomplete or a targeted read spends the
request's remaining time, the request returns `temporarily_unavailable` and can be
retried. A selected source path can answer within the remaining time. The server logs
`local_index_preparing` while the publication is being prepared.

Parameters:

- `symbol` - The symbol whose nodes the read lists: each declaration's item node, then the descendants that belong to that declaration and not to a declaration nested in it, outermost first per declaration.
- `path` - Project-relative file to inspect.
- `position` - UTF-8 byte offset inside the declaration whose nodes the read lists - one position, not a range; the nodes themselves carry the spans.
- `node_cursor` - Continuation a preceding read returned, bound to the symbol or unit its answer listed and the tree revision it read.
- `revision` - The revision to read: a git revision reads that commit, a tree revision the served working tree, and a release revision selects the release of a package or runtime `symbol`'s owner.
- `scope` - Project that owns `path`.

## search

Evaluate SPARQL over captured code facts. Use `SERVICE :search` for ranked text and
regex retrieval. Continue a paged answer with its `next_query`.

Parameters:

- `query` (required) - Query text.
- `language` - Currently, only SPARQL is supported.

