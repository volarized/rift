This directory contains tree-sitter-angular from https://github.com/dlvandenberg/tree-sitter-angular at commit `38a8014ed5452cd6b7cf1399c00177a1f5374256`.

The parser, scanner, Rust bindings, and MIT license retain their upstream bytes. The manifest selects Rift's Tree-sitter runtime and keeps the dependency outside the Rift workspace.

## Dependency selection

Tree-sitter supplies the runtime; language grammars are distributed separately. Its [getting-started example](https://tree-sitter.github.io/tree-sitter/using-parsers/1-getting-started.html) links the runtime library and a grammar's generated parser independently.

The published Angular grammar and upstream commit above select `tree-sitter = "~0.25"`. Rift selects `tree-sitter = "=0.27.0"`. The Angular Rust binding returns its dependency's `Language`, so the published binding cannot supply Rift's current parser type.

The published MIT `tree-sitter-angular-codemod` 0.6.5 exports `LanguageFn`, which would permit conversion into Rift's `Language`. Its manifest also requires the Tree-sitter 0.24 runtime. A separate Cargo probe containing that package and Tree-sitter 0.27 failed dependency resolution:

```text
only one package in the dependency graph may specify the same links value
```

Both runtime packages declare `links = "tree-sitter"`; [Cargo permits only one such native library](https://doc.rust-lang.org/cargo/reference/resolver.html#links). The codemod grammar also lacks the latest upstream rules for `class_binding`, `style_unit`, `template_string`, `regular_expression`, and `arrow_function`.

Evaluation on 2026-10-09 checked the two published Angular packages and the 22 public upstream forks. No inspected binding selected Rift's runtime while retaining the latest Angular grammar. The codemod fork's last update was 2025-02-28; the upstream commit retained here is dated 2026-05-15.

The local manifest adjustment retains the latest grammar and one runtime without adding unsafe Rust to Rift. Replace this directory with a pinned published package or upstream Git dependency when its binding accepts Rift's runtime or exposes `LanguageFn` without an incompatible runtime dependency. Before replacement, run Angular template, embedded-range, source-identity, checked-restoration, aggregate-bound, package, and workspace fixtures, and regenerate the analyzer manifest.

Sources: [upstream manifest](https://github.com/dlvandenberg/tree-sitter-angular/blob/38a8014ed5452cd6b7cf1399c00177a1f5374256/Cargo.toml), [upstream grammar](https://github.com/dlvandenberg/tree-sitter-angular/blob/38a8014ed5452cd6b7cf1399c00177a1f5374256/grammar.js), [codemod binding](https://github.com/codemod/tree-sitter-angular/blob/5fa7aa1825bcc7774146e9b1b312fd18930fb494/bindings/rust/lib.rs), and [published codemod dependencies](https://crates.io/api/v1/crates/tree-sitter-angular-codemod/0.6.5/dependencies).
