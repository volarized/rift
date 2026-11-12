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

Lists syntax nodes with their source text: every node of one `symbol`, or, for one
UTF-8 byte `position` in one file, every node of the symbol whose declaration holds
it, or the file's unit nodes when no declaration holds it. A symbol read is the way to
read a declaration's source, and a package or runtime symbol's nodes come from the
global API, each in a source unit of that symbol's release. The answer names `symbol`;
each node is addressed by its place: node `index` of declaration `declaration` of
that symbol, or node `index` of its unit, from the file node at 0, with `parent` an
index in the same declaration or unit. An edit renumbers the nodes after it. An answer
whose nodes cross its node count or source byte bound returns `node_cursor` for the
rest. A file no syntax provider parses answers its file node alone. `rev` lists the
nodes as of a version-control revision instead of the current tree. If discovery is
incomplete or a targeted read spends the request's remaining time, the result has
empty `nodes` and carries `local_index_preparing`. A selected source path can answer
within that remaining time, with the same warning until the publication records the
path: workspace byte and declaration bounds remain unchecked.

Parameters:

- `symbol` - The symbol whose nodes the read lists: each declaration's item node, then the descendants that belong to that declaration and not to a declaration nested in it, outermost first per declaration.
- `path` - Project-relative file to inspect.
- `position` - UTF-8 byte offset inside the declaration whose nodes the read lists - one position, not a range; the nodes themselves carry the spans.
- `node_cursor` - Continuation a preceding read returned, bound to the symbol or unit its answer listed and the tree revision it read.
- `view` - Captured view selected by discovery.
- `rev` - The version-control revision to read - a branch, tag, or commit id as the workspace's version control spells it.

## search

Searches indexed declarations and source lines by lexical `query`, merged with
full-text matches from included `[search.text]` files and declaration bodies, and by a
bounded relationship `traversal` from one seed symbol: `incoming` reaches the
declarations referencing it, `outgoing` the declarations it calls. `pattern` matches a
regex against the text of every indexed file, line by line as ripgrep reads it, and
answers each match and each declaration holding one, in place of `query` and
`traversal`. `change` answers the declarations a committed revision and another
revision, or the working tree, hold differently, in place of `query` and `traversal`.
`rev` searches a version-control revision instead of the current tree, and never
combines with `pattern`, `traversal`, or `change`. `scope` reaches past the project
tree: `global` answers collected package and runtime facts selected by the project's
dependency context. Explicit `packages` replaces versions of named packages or adds
packages the context lacks. `local@name` selects an accepted extra project; `local:name` selects a root-defined profile;
`local@name:profile` selects a profile defined in that extra project's own `rift.toml`.
`all` merges the primary project, profiles, accepted extra projects with their profiles, and global results. `target: "commit"` matches
`query` alone against the messages of the commits the history store holds.
Use `get_symbol` with an exact canonical identity returned by search.
For a current-tree search, the published workspace is resolved exactly once and
threaded through both the search index's revision check and the executed
`ReadService::search` call: a concurrent rebuild between two separate resolutions
could otherwise validate ranked units against one snapshot and merge them into
results computed from another.

Parameters:

- `view` - The captured serving context returned by an earlier search.
- `target` - Which entity kinds may be returned - a kind selector, never the text to search for; that is `query`.
- `order` - Which total order the page comes back in.
- `query` - Text to match against declaration names, qualified names, signatures, attached documentation, and file contents.
- `pattern` - A regex matched against the text of every indexed file, in the syntax of the Rust `regex` crate that ripgrep reads.
- `scope` - Which corpus is selected.
- `packages` - Packages `query` and `pattern` search beside the ones the workspace's manifests and lockfiles name, at most 64.
- `paths` - Files eligible for the search, selected by project-relative globs.
- `include` - Extra payload to attach to every hit.
- `limit` - Most hits to return in one page, at most 10,000; the server refuses a larger `limit` naming the field.
- `page_index` - Zero-based page of the result set to serve, sized by `limit`.
- `rev` - The version-control revision to search - a branch, tag, or commit id as the workspace's version control spells it.
- `traversal` - A bounded relationship walk from `seed`, standing alone or beside `query`.
- `change` - A committed revision compared against another revision or the working tree, standing alone.

