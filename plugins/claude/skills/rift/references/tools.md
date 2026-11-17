# Rift MCP tools

Generated from the served tool surface.

## feedback

Sends a feedback report over HTTPS to the public Rift intake, or to the intake that
`RIFT_FEEDBACK_URL` names in the server's environment. The server sends the
caller's text and any selected diagnostic fields. The caller MUST omit secrets,
personal data, and private source material. The caller MUST obtain the user's
approval for any exact conversation excerpt before including it in a report.

Parameters:

- `text` (required) - Report text sent verbatim.
- `include_details` - Diagnostic fields to send alongside `text`.

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

Reads the selected symbol's declarations with their source and structure. A UTF-8
byte `position` selects its enclosing declaration, or a source excerpt if none holds
it. Use `scope` to select a registered project or profile for a path read.
Send `syntax: true` for the full syntax tree, including individual tokens. Use the
returned `node_cursor` with the same selector to continue declarations or syntax.
A node's `index` and `parent` address nodes
within the same declaration or source unit.
Use `revision` to read a commit or release, or to pin the served working tree.

Parameters:

- `symbol` - The symbol whose nodes the read lists: each declaration's item node, then the descendants that belong to that declaration and not to a declaration nested in it, outermost first per declaration.
- `path` - Project-relative file to inspect.
- `position` - UTF-8 byte offset inside the declaration whose nodes the read lists - one position, not a range; the nodes themselves carry the spans.
- `node_cursor` - Continuation a preceding read returned, bound to the symbol or unit its answer listed and the tree revision it read.
- `revision` - The revision to read: a git revision reads that commit, a tree revision the served working tree, and a release revision selects the release of a package or runtime `symbol`'s owner.
- `scope` - Project that owns `path`.
- `syntax` - Return a page of the full syntax tree.

## search

Evaluate SPARQL over captured code facts. Use `SERVICE :search` for text and regex
retrieval and `SERVICE :flows` for call paths around ranked symbols. Continue a
paged answer with its `next_query`.

Parameters:

- `query` (required) - Query text.
- `language` - Currently, only SPARQL is supported.

