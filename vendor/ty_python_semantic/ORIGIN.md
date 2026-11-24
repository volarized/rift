# ty_python_semantic in Rift

This crate is based on the `ty_python_semantic` 0.0.20 crate of https://github.com/astral-sh/ruff at
commit `fde7c6debc8ab1507d1e2bd74e5d8d8c394acef1`, the revision the other ruff and ty crates of
the embedded Python engine are pinned to in the workspace manifest. The git tree of
`crates/ty_python_semantic/src` at that commit is `3fdfe1667aba307faf1f1f9b70d6fd447ab36500` and
the tree of `crates/ty_python_semantic/resources/lint_docs` is
`757877a5b6fda7a8deae5985e931f631a2997ce5`. The license comes from the same revision.

## The local patch

The version carries the build suffix `+rift.1`. One source file, `src/types/infer.rs`, carries
the change in `alias-cycle.patch`. Every other file under `src` and `resources/lint_docs` keeps
its upstream bytes.

The change is in the cycle function of `infer_legacy_type_alias`. Two implicit type aliases that
name each other through containers, read from a function signature first, never converge at this
revision:

```python
JsonValue = Union[str, "JsonDict", Tuple["JsonValue"]]
JsonDict = Dict[str, JsonValue]
```

Each iteration of the cycle wraps the previous value of the alias in one more layer, the value
never repeats, and each walk of it visits the previous layer twice, so a definition, reference,
hierarchy, or diagnostic request on such a function does not end. The patch reduces the nesting
with the crate's own `Type::recursive_type_normalized` once the alias has not converged within
`TAINTED_CYCLES` iterations, and leaves a value with an unguarded cycle as inferred so that an
invalid alias is still reported. The alias then converges with its recursive positions replaced
by the `Divergent` marker: `JsonDict` is `dict[str, Divergent]`.

An alias cycle that converges within `TAINTED_CYCLES` iterations answers as upstream does, and so
does an invalid alias; the crate's own `mdtest` suite passes with the change (506 files). The
iteration count belongs to the whole cycle, not to one alias: a valid alias that shares a cycle
with another one running longer is reduced too, and loses structure it had upstream. In
`A0: TypeAlias = Union["A0", "A2"]`, `A1 = Tuple["A2", Optional[A0]]`, `A2 = Dict[str, "A1"]`,
upstream answers `A2 = dict[str, tuple[A2, Divergent | None]]` and this crate `dict[str, Divergent]`.

The answer depends on which type request reaches the aliases first. When the type of an alias
itself is asked first, the aliases converge as upstream's recursive types, with or without the
patch: a type definition request on `JsonValue`, or on a value of that type, answers no target.
When the type of a function that returns the alias is asked first, the cycle is cut, and the same
request answers `str`, `dict`, and `tuple`; a value indexed out of the dictionary is the
`Divergent` marker, which the engine adapter in `rift-server` leaves out of a type answer.
Definitions, references, and hierarchies are the same in both orders; of 87 answers over a
two-file tree, 16 type definition answers differ (volarized/rift#1111). Aliases that converge
without the patch answer the same in both orders.

The patch ends this family only. An alias that reaches itself through `Type[...]`, such as
`A = List[Type["A"]]`, grows in a definition query the patch does not reach, and still does not
converge (volarized/rift#1079).

## What is left out

`tests`, `resources/mdtest`, `resources/corpus`, `resources/primer`, `resources/README.md`,
`mdtest.py`, `mdtest.py.lock`, and `README.md`: the library target reads none of them. `resources/lint_docs` stays, since the
lint declarations include those files as documentation.

The manifest is the upstream one with every `workspace = true` replaced by the value the ruff
workspace gives it at this revision, lints included, and with `publish = false` added. Dependencies on other ruff and ty crates
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
3. Run the regression test `recursive_aliases_end_a_definition` of `rift-server` without the
   patch. If it passes, upstream ends the request: drop the three directories and return to the
   git dependency.
4. Otherwise apply the patch from the repository root with
   `git apply --directory=vendor/ty_python_semantic vendor/ty_python_semantic/alias-cycle.patch`,
   adapt it where the cycle function changed, and write the adapted patch back. Run upstream's
   `cargo test -p ty_python_semantic --test mdtest` on a ruff checkout with the patch applied:
   that suite, not this repository's, covers converging and invalid alias cycles.
5. Move the revision in the workspace manifest, in the three vendored manifests, and in the three
   origin notes together. Raise the `+rift.N` suffix when the patch changes.
6. Confirm `cargo tree -d` names no ruff or ty crate.
