# ty_python_semantic in Rift

This crate is based on the `ty_python_semantic` 0.0.20 crate of https://github.com/astral-sh/ruff at
commit `fde7c6debc8ab1507d1e2bd74e5d8d8c394acef1`, the revision the other ruff and ty crates of
the embedded Python engine are pinned to in the workspace manifest. The git tree of
`crates/ty_python_semantic/src` at that commit is `3fdfe1667aba307faf1f1f9b70d6fd447ab36500` and
the tree of `crates/ty_python_semantic/resources/lint_docs` is
`757877a5b6fda7a8deae5985e931f631a2997ce5`. The license comes from the same revision.

Every file under `src` and `resources/lint_docs` keeps its upstream bytes.

## What is left out

`tests`, `resources/mdtest`, `resources/corpus`, `resources/primer`, `mdtest.py`, `mdtest.py.lock`,
and `README.md`: the library target reads none of them. `resources/lint_docs` stays, since the
lint declarations include those files as documentation.

The manifest is the upstream one with every `workspace = true` replaced by the value the ruff
workspace gives it at this revision, lints included. Dependencies on other ruff and ty crates
name the same git revision, and `ty_python_semantic` is a path. The development dependencies and
the `mdtest` test target are left out. An empty workspace table keeps the crate outside the Rift
workspace.

`ty_ide` and `ty_project` are vendored beside this crate only because they depend on it: a
dependency graph holds one `ty_python_semantic`, and theirs has to be this one.

## Refreshing

1. Check out the new ruff revision and copy `crates/ty_python_semantic/src` and
   `crates/ty_python_semantic/resources/lint_docs` over this directory's, leaving out `snapshots`
   folders; do the same for `src` of `vendor/ty_ide` and `vendor/ty_project`.
2. Write the three manifests out again from the new upstream manifests and workspace tables.
3. Move the revision in the workspace manifest, in the three vendored manifests, and in the three
   origin notes together, and confirm `cargo tree -d` names no ruff or ty crate.
