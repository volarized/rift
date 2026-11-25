# Python grammar in Rift

This crate is based on tree-sitter-python 0.25.0 at commit `293fdc02038ee2bf0e2e206711b69c90ac0d413f`. The local grammar reads Python 3.13 type parameter defaults in classes, functions and type aliases. Cargo builds the checked-in C sources under the upstream MIT license. [Upstream source](https://github.com/tree-sitter/tree-sitter-python/tree/293fdc02038ee2bf0e2e206711b69c90ac0d413f) [PEP 696](https://peps.python.org/pep-0696/)

## Regenerate the parser

Run the following from this directory with Node.js and Tree-sitter CLI 0.27.0. [Tree-sitter CLI 0.27.0](https://github.com/tree-sitter/tree-sitter/releases/tag/v0.27.0)

```sh
tree-sitter generate --abi 15
```

Run `just generate` from the Rift workspace to update its analyzer manifest.
