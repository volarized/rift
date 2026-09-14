# Rift MCP tools

Generated from the served tool surface.

## get_symbol

Finds declarations and their source by exact symbol name. Each hit
carries the declaration and its source excerpt unless `include` omits
`source`. `include: ["history"]` adds each hit's version-control timeline,
walked from the served revision. `rev` serves the lookup from a
version-control revision instead of the current tree. `scope` reaches
past the project tree: `dependencies` answers from the public
declarations of the cataloged packages alone, `all` from both, project
hits first. Use `search` when the name is not exactly known.

Parameters:

- `include` - Optional hit fields to attach: `source`, `history`.
- `language` - Narrows the answer to one language.
- `limit` - Most hits to return in one page, capped by `max_page_items`.
- `name` (required) - The declaration name to look up - a name, not a full `SymbolId` or free-text query; `search` takes free text.
- `page_index` - Zero-based page of the result set to serve, sized by `limit`.
- `rev` - The version-control revision to read - a branch, tag, or commit id as the workspace's version control spells it.
- `scope` - Which declarations the lookup searches: the project tree, the cataloged dependency packages, or both.

## insert_node

Inserts new content beside a syntax node addressed through a witnessed address
from `nodes`. The server recomputes the witness before writing and refuses when
the bytes drifted, the same check `replace_node` runs. Unlike `insert_symbol`,
which separates a new declaration from its anchor with a blank line and preserves
the anchor's indentation, `body` lands verbatim at the node's own boundary with no
separator of its own: a node is not a declaration, so the caller supplies whatever
spacing and indentation the inserted bytes need.

Parameters:

- `anchor` (required) - The node identity returned by `nodes`, witness included.
- `body` (required) - The new content, spliced in verbatim with no added separator.
- `position` (required) - Which side of the node receives the new content.

## insert_symbol

Inserts a new declaration beside an anchor symbol, or content at a file
target. Anchored insertions land beside the anchor's whole declaration,
its attached outer attributes and doc comments included. A file target
lands the body verbatim at the file's start or end, creating it first
when `create_missing` is set and it is missing. A refusal names the
failed precondition and leaves the workspace untouched. A body
inserted `before` its anchor is spliced in at the anchor's start byte
and its first line inherits the anchor's column; a body inserted
`after` its anchor, or at a file target either side, always starts a
fresh line at column zero.

Parameters:

- `anchor` - The existing declaration identity returned by `get_symbol`.
- `body` (required) - The new content: a declaration beside `anchor`, or a file target's whole body.
- `create_missing` - Creates a missing `file` target instead of refusing.
- `file` - The destination file the content lands in, created first when `create_missing` is set and it does not exist.
- `position` (required) - Which side of the anchor or file target receives the new content.

## move_file

Moves one visible file to a new project path. When the configured
language engine advertises will-rename requests for the file, its
reference updates land in the same atomic change; without an engine
or the capability the move still lands and the result carries a
warning that references were not updated. A refusal names the
failed precondition and leaves the workspace untouched.

Parameters:

- `from` (required) - The file to move, as a project-relative path.
- `to` (required) - The destination path.

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

## patch

Applies unified-diff hunks to workspace files atomically. The target is any
file the workspace's `[source]` policy makes visible, parsed or not. Hunk
context guards the change: a header's line numbers are hints and
its line counts are read from the hunk's own body, as with
`git apply`. A `/dev/null` header creates or deletes the file. A body
that is not a unified diff, such as an `*** Begin Patch` envelope, is
refused naming the form to send. The result names each file the change
wrote with its size and line counts.

Parameters:

- `patch` (required) - A unified diff, inline or read from a file.

## remove_node

Removes one syntax node through a witnessed address from `nodes`.
The server recomputes the witness before writing and refuses when
the bytes drifted, so a stale address never removes moved code. When
the node names a declaration, the removal is checked against the
configured language engine's references the same way `remove_symbol`
checks them; a node naming no declaration applies unchecked, with a
warning saying so.

Parameters:

- `force` - Applies the removal even when references stand, carrying them as a warning instead of refusing.
- `node` (required) - The node identity returned by `nodes`, witness included.

## remove_symbol

Removes one declaration addressed by symbol. The whole declaration,
its attached outer attributes and doc comments included, is removed
together with the separator that followed it, so no blank-line run
stands where it stood. When the configured language engine
advertises `textDocument/references`, a standing reference refuses
`unmet_precondition` naming `no_references`, unless `force` applies
the removal anyway and carries the references as a warning. Without
such an engine, the removal applies and carries a warning naming why
it was not checked.

Parameters:

- `force` - Applies the removal even when references stand, carrying them as a warning instead of refusing.
- `symbol` (required) - The declaration identity returned by `get_symbol`.

## rename_symbol

Renames one declaration addressed by symbol through the configured
language engine. The engine proposes the edits; the server verifies
each one against the tree and writes them atomically, then reports
surviving occurrences of the old name as warning findings. Refused
as `unsupported` when no engine serves the declaration's language;
a refusal leaves the workspace untouched.

Parameters:

- `new_name` (required) - The declaration's new name.
- `symbol` (required) - The declaration identity returned by `get_symbol`.

## replace_node

Replaces one syntax node through a witnessed address from `nodes`.
The server recomputes the witness before writing and refuses when the
bytes drifted, so a stale address never splices into moved code.

Parameters:

- `body` (required) - The replacement source, inline or read from a file.
- `node` (required) - The node identity returned by `nodes`, witness included.
- `region` - Which named part of the node to replace.

## replace_symbol

Replaces one declaration addressed by symbol. The whole declaration
includes its attached outer attributes and doc comments. The parser
derives the span, so the caller supplies no offsets; a refusal
names the failed precondition and leaves the workspace untouched.
The body is spliced in verbatim at the declaration's own start byte:
its first line inherits the declaration's column, and every later
line carries whatever indentation it is written with.

Parameters:

- `body` (required) - The replacement source, inline or read from a file.
- `region` - Which part of the declaration to replace.
- `symbol` (required) - The declaration identity returned by `get_symbol`.

## search

Searches indexed declarations and source lines by lexical `query`, merged with
full-text matches from included `[search.text]` files and declaration bodies, and by a
bounded relationship `traversal` from one seed symbol. `rev` searches a
version-control revision instead of the current tree, and never combines with
`traversal`. Use `get_symbol` when the declaration name is known.
For a current-tree search, the published workspace is resolved exactly once and
threaded through both the search index's revision check and the executed
`ReadService::search` call: a concurrent rebuild between two separate resolutions
could otherwise validate ranked units against one snapshot and merge them into
results computed from another.

Parameters:

- `include` - Extra payload to attach to every hit.
- `limit` - Most hits to return in one page.
- `order` - Which total order the page comes back in.
- `page_index` - Zero-based page of the result set to serve, sized by `limit`.
- `paths` - Files eligible for the search, selected by project-relative globs.
- `query` - Text to match against file contents, symbol names, and rendered signatures.
- `rev` - The version-control revision to search - a branch, tag, or commit id as the workspace's version control spells it.
- `target` - Which entity kinds may be returned - a kind selector, never the text to search for; that is `query`.
- `traversal` - A bounded relationship walk, standing alone or beside `query`.

