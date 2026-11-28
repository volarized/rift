# TypeScript and TSX grammar in Rift

This crate is based on tree-sitter-typescript 0.21.1 at commit `35a3762218ca481cd85f14f4db11b563bc805c83`. The local patch accepts `global` as a contextual identifier while preserving the `global` block used in ambient declarations. One grammar conflict lets both uses coexist when a brace follows an identifier. Both generated parsers use ABI 14. [Upstream source](https://github.com/tree-sitter/tree-sitter-typescript/tree/35a3762218ca481cd85f14f4db11b563bc805c83)

The local scanner also recognizes a line break before a generic signature when an object type expects another member. Type arguments within a member and less-than expressions continue to use their existing grammar rules.

Both grammars also accept `export type * from` as a type-only re-export. Qualified inline imports with a written string specifier are types, including when the selected member has type arguments.

`LICENSE` covers the TypeScript grammar and `LICENSE.tree-sitter-javascript` covers its JavaScript grammar input. Cargo builds both parsers from the checked-in C sources.

## Regenerate the parsers

Run these commands from this directory with Node.js, Tree-sitter CLI 0.22.6 at `b40f342067a89cd6331bf4c27407588320f3c263`, and the exact tree-sitter-javascript 0.21.2 grammar used by the upstream lockfile. [Tree-sitter CLI 0.22.6](https://github.com/tree-sitter/tree-sitter/releases/tag/v0.22.6)

```sh
set -eu
grammar_archive=$(mktemp)
trap 'rm -f "$grammar_archive"' EXIT
curl --fail --location \
  https://registry.npmjs.org/tree-sitter-javascript/-/tree-sitter-javascript-0.21.2.tgz \
  --output "$grammar_archive"
printf '%s  %s\n' \
  ffd32f5045c7fe9ba9434173fe5c43bfc11ea960dfa741a05201e875ea46242c \
  "$grammar_archive" | shasum -a 256 --check
mkdir -p node_modules/tree-sitter-javascript
tar -xOzf "$grammar_archive" package/grammar.js \
  > node_modules/tree-sitter-javascript/grammar.js
(cd typescript && tree-sitter generate --abi 14 --no-bindings)
(cd tsx && tree-sitter generate --abi 14 --no-bindings)
```

Run `just generate` from the Rift workspace to update its analyzer manifest.
