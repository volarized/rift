This directory contains the `ty_project` crate of https://github.com/astral-sh/ruff at commit
`fde7c6debc8ab1507d1e2bd74e5d8d8c394acef1`. The git tree of `crates/ty_project/src` at that commit is
`ffee9f052d79ccb9ac578d48daa9e130c7247c30`. The license comes from the same revision.

Every file under `src` keeps its upstream bytes. The crate is here only because it depends on
`ty_python_semantic`, which carries one patch (`vendor/ty_python_semantic/ORIGIN.md`): a
dependency graph holds one `ty_python_semantic`, and this crate's has to be the patched one.

The manifest is the upstream one with every `workspace = true` replaced by the value the ruff
workspace gives it at this revision, lints included. Dependencies on other ruff and ty crates name
the same git revision; the vendored crates are paths. The development dependencies are left out. An
empty workspace table keeps the crate outside the Rift workspace.

Refresh it together with `vendor/ty_python_semantic`, as that directory's `ORIGIN.md` describes.
