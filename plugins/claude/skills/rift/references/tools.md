# Rift MCP tools

Generated from the served tool surface.

## get_symbol

Finds declarations and their source by exact symbol name. Each hit
carries the declaration and its source excerpt unless `include` omits
`source`. `include: ["history"]` adds each hit's version-control timeline,
walked from the served revision. `rev` serves the lookup from a
version-control revision instead of the current tree. `scope` reaches
past the project tree: `global` answers from the public declarations of
the cataloged packages alone, `all` from both, project hits first. Use
`search` when the name is not exactly known.

Parameters:

- `include` - Optional hit fields to attach: `source`, `history`.
- `language` - Narrows the answer to one language.
- `limit` - Most hits to return in one page, at most 10,000; the server refuses a larger `limit` naming the field.
- `name` (required) - The declaration name to look up - a name, not a full `SymbolId` or free-text query; `search` takes free text.
- `page_index` - Zero-based page of the result set to serve, sized by `limit`.
- `rev` - The version-control revision to read - a branch, tag, or commit id as the workspace's version control spells it.
- `scope` - Which declarations the lookup searches: the project tree, the dependency packages, or both.

## nodes

Lists the syntax nodes covering one UTF-8 byte position in one file,
outermost first. Each identity carries a witness, so an address taken
from this listing refuses cleanly once the file's bytes drift. `rev`
lists the nodes as of a version-control revision instead of the
current tree. A visible path no syntax provider parses refuses
`capability_unavailable`, naming the extension.

Parameters:

- `path` (required) - Project-relative file to inspect.
- `position` (required) - UTF-8 byte offset the listed nodes must cover - one position, not a range; the nodes themselves carry the spans.
- `rev` - The version-control revision to read - a branch, tag, or commit id as the workspace's version control spells it.

## search

Searches indexed declarations and source lines by lexical `query`, merged with
full-text matches from included `[search.text]` files and declaration bodies, and by a
bounded relationship `traversal` from one seed symbol. `change` answers the
declarations two committed revisions hold differently, in place of `query` and
`traversal`. `rev` searches a version-control revision instead of the current tree,
and never combines with `traversal` or `change`. `scope` reaches past the project
tree: `global` answers `query` from the public declarations of the cataloged
packages alone, `all` from both, ordered together. Use `get_symbol` when the
declaration name is known.
For a current-tree search, the published workspace is resolved exactly once and
threaded through both the search index's revision check and the executed
`ReadService::search` call: a concurrent rebuild between two separate resolutions
could otherwise validate ranked units against one snapshot and merge them into
results computed from another.

Parameters:

- `change` - Two committed revisions to compare, standing alone.
- `include` - Extra payload to attach to every hit.
- `limit` - Most hits to return in one page, at most 10,000; the server refuses a larger `limit` naming the field.
- `order` - Which total order the page comes back in.
- `page_index` - Zero-based page of the result set to serve, sized by `limit`.
- `paths` - Files eligible for the search, selected by project-relative globs.
- `query` - Text to match against file contents, symbol names, and rendered signatures.
- `rev` - The version-control revision to search - a branch, tag, or commit id as the workspace's version control spells it.
- `scope` - Which declarations `query` searches: the project tree, the public declarations of the dependency packages, or both.
- `target` - Which entity kinds may be returned - a kind selector, never the text to search for; that is `query`.
- `traversal` - A bounded relationship walk from `seed`, standing alone or beside `query`.

