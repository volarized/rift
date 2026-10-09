# Rift MCP tools

Generated from the served tool surface.

## get_symbol

Finds declarations and their source by exact symbol name. Each hit
carries the declaration and its source excerpt unless `include` omits
`source`. `include: ["history"]` adds each hit's version-control timeline,
walked from the served revision. `rev` serves the lookup from a
version-control revision instead of the current tree. `scope` reaches
past the project tree: `global` answers from the public declarations the
global index holds for the workspace's dependencies alone, `all` from both,
project hits first. `packages` names package versions the lookup reads beside the
workspace's own, such as an upgrade target or a package the project does not use
yet. A name with no match carries `symbol_not_found`, naming the lookup and
up to three closest project declaration identities. A `global` lookup carries
no alternatives; `all` proposes project declarations alone. A closest ranking
past its fixed work bound leaves alternatives empty and explains why in `detail`.
Use `search` when
the name is not exactly known.

Parameters:

- `name` (required) - The declaration name to look up - a name, not a full `SymbolId` or free-text query; `search` takes free text.
- `language` - Narrows the answer to one language.
- `scope` - Which declarations the lookup searches: the project tree, the dependency packages, or both.
- `packages` - Packages this lookup reads beside the ones the workspace's manifests and lockfiles name, at most 64.
- `include` - Optional hit fields to attach: `source`, `history`.
- `limit` - Most hits to return in one page, at most 10,000; the server refuses a larger `limit` naming the field.
- `page_index` - Zero-based page of the result set to serve, sized by `limit`.
- `rev` - The version-control revision to read - a branch, tag, or commit id as the workspace's version control spells it.

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
tree: `global` answers `query` from the public declarations the global index holds for
the workspace's dependencies alone and `pattern` from their source, `all` from both,
ordered together. `packages` names package versions `query` and `pattern` search beside
the workspace's own, such as an upgrade target or a package the project does not use
yet. `target: "commit"` matches `query` alone against the messages of the commits the
history store holds. Use `get_symbol` when the declaration name is known.
For a current-tree search, the published workspace is resolved exactly once and
threaded through both the search index's revision check and the executed
`ReadService::search` call: a concurrent rebuild between two separate resolutions
could otherwise validate ranked units against one snapshot and merge them into
results computed from another.

Parameters:

- `target` - Which entity kinds may be returned - a kind selector, never the text to search for; that is `query`.
- `order` - Which total order the page comes back in.
- `query` - Text to match against declaration names, qualified names, signatures, attached documentation, and file contents.
- `pattern` - A regex matched against the text of every indexed file, in the syntax of the Rust `regex` crate that ripgrep reads.
- `scope` - Which sources `query` and `pattern` search: the project tree, the dependency packages, or both.
- `packages` - Packages `query` and `pattern` search beside the ones the workspace's manifests and lockfiles name, at most 64.
- `paths` - Files eligible for the search, selected by project-relative globs.
- `include` - Extra payload to attach to every hit.
- `limit` - Most hits to return in one page, at most 10,000; the server refuses a larger `limit` naming the field.
- `page_index` - Zero-based page of the result set to serve, sized by `limit`.
- `rev` - The version-control revision to search - a branch, tag, or commit id as the workspace's version control spells it.
- `traversal` - A bounded relationship walk from `seed`, standing alone or beside `query`.
- `change` - A committed revision compared against another revision or the working tree, standing alone.

