//! Stub and implementation associations retained beside normalized declarations.
//!
//! Physical suffixes select candidate files. Declarations join only when current
//! placement assigns the same established canonical identity to both declarations.
//! Original source bindings and stub diagnostic identities remain separate.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::symbol_identity;
use rift_protocol::read::{ProjectPath, SymbolId, TextRange};
use rift_syntax::SyntaxSymbol;

use super::AnalyzedFile;

/// Stub suffixes and the implementation suffixes each pairs with, in pairing order.
///
/// Every stub pairs with its first candidate before any stub falls back to a later one, so
/// an exact build (`index.d.mts` with `index.mjs`) claims its file first. A `.d.ts` beside
/// no `.js` then pairs with its `.mjs`, then its `.cjs`: a package shipping an ESM build
/// beside a bundled one declares its names in the ESM build.
const STUB_IMPLEMENTATIONS: [(&str, &[&str]); 4] = [
    (".pyi", &[".py"]),
    (".d.ts", &[".js", ".mjs", ".cjs"]),
    (".d.mts", &[".mjs"]),
    (".d.cts", &[".cjs"]),
];

/// The most implementation candidates one stub suffix names.
const CANDIDATES_MAX: usize = 3;

/// One stub declaration a joined declaration answers for: its identity, and where the stub
/// declares it.
///
/// The publication addresses a joined declaration at its implementation; the stub's own
/// identity and range stay beside it here, so a position inside the stub names the joined
/// declaration.
#[derive(Debug, Clone, PartialEq)]
pub struct StubForm {
    identity: SymbolId,
    path: ProjectPath,
    range: TextRange,
}

impl StubForm {
    /// The identity the stub declaration carries in the semantic build.
    #[must_use]
    pub const fn identity(&self) -> &SymbolId {
        &self.identity
    }

    /// The stub file's package-relative path.
    #[must_use]
    pub const fn path(&self) -> &ProjectPath {
        &self.path
    }

    /// The stub declaration's byte range within its file.
    #[must_use]
    pub const fn range(&self) -> &TextRange {
        &self.range
    }
}

/// What the join decided for one file.
#[derive(Debug, Default)]
pub(super) enum ModuleRole {
    /// No stub or implementation pairs with the file; its declarations answer as parsed.
    #[default]
    Unpaired,
    /// A stub with established implementation associations.
    Stub,
    /// An implementation whose joined declarations take the stub's signatures and types.
    Implementation {
        /// Each joined implementation declaration's qualified name, and the stub forms it
        /// joins, in the stub's source order.
        stub_forms: BTreeMap<String, Vec<StubForm>>,
    },
}

impl ModuleRole {
    /// The stub forms one implementation declaration joins; empty for any other.
    pub(super) fn stub_forms(&self, qualified_name: &str) -> &[StubForm] {
        match self {
            Self::Implementation { stub_forms } => {
                stub_forms.get(qualified_name).map_or(&[], Vec::as_slice)
            }
            Self::Unpaired | Self::Stub => &[],
        }
    }
}

/// One module's stub and implementation, as positions in the analyzed files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ModulePair {
    stub: usize,
    implementation: usize,
}

/// Pairs every stub with the implementation of its module and joins their declarations.
///
/// In a paired module the stub defines the public set: an implementation declaration no
/// stub declaration names stays in the publication and leaves the public set. A stub
/// declaration no implementation declaration names answers from the stub.
pub(super) fn join_modules(analyzed: &mut [AnalyzedFile]) {
    let pairs = module_pairs(analyzed.iter().map(|held| held.file.path().as_str()));
    for pair in pairs {
        let joined = pair.join(analyzed);
        if matches!(&joined.implementation, ModuleRole::Implementation { stub_forms } if stub_forms.is_empty())
        {
            continue;
        }
        analyzed[pair.stub].role = joined.stub;
        let implementation = &mut analyzed[pair.implementation];
        implementation.role = joined.implementation;
        implementation.public_names = joined.implementation_public;
    }
}

/// Pairs stub paths with implementation paths of the same module, in the stubs' path
/// order.
///
/// An implementation pairs with one stub at most. Work is one lookup per stub and
/// candidate rank, so the pass is linear in the file count.
pub(super) fn module_pairs<'path>(paths: impl Iterator<Item = &'path str>) -> Vec<ModulePair> {
    let positions: BTreeMap<&str, usize> = paths
        .enumerate()
        .map(|(position, path)| (path, position))
        .collect();
    let stubs: Vec<StubPath<'_>> = positions
        .iter()
        .filter_map(|(path, position)| StubPath::of(path, *position))
        .collect();
    let mut claimed = BTreeSet::new();
    let mut paired = BTreeSet::new();
    let mut pairs = Vec::new();
    for rank in 0..CANDIDATES_MAX {
        for stub in &stubs {
            let Some(implementation) = stub.candidate(rank, &positions) else {
                continue;
            };
            if !paired.contains(&stub.position) && claimed.insert(implementation) {
                paired.insert(stub.position);
                pairs.push(ModulePair {
                    stub: stub.position,
                    implementation,
                });
            }
        }
    }
    pairs.sort_by_key(|pair| pair.stub);
    pairs
}

/// One stub path: where it sits among the analyzed files, its path without the stub
/// suffix, and the implementation suffixes it pairs with.
struct StubPath<'path> {
    position: usize,
    stem: &'path str,
    candidates: &'static [&'static str],
}

impl<'path> StubPath<'path> {
    /// The stub `path` is, when its suffix is one a stub carries.
    fn of(path: &'path str, position: usize) -> Option<Self> {
        STUB_IMPLEMENTATIONS
            .iter()
            .find_map(|(suffix, candidates)| {
                path.strip_suffix(suffix).map(|stem| (stem, *candidates))
            })
            .map(|(stem, candidates)| Self {
                position,
                stem,
                candidates,
            })
    }

    /// The position of this stub's implementation candidate of `rank`, when the package
    /// holds it.
    fn candidate(&self, rank: usize, positions: &BTreeMap<&str, usize>) -> Option<usize> {
        let suffix = self.candidates.get(rank)?;
        positions
            .get(format!("{}{suffix}", self.stem).as_str())
            .copied()
    }
}

/// The roles and public set one joined module leaves its two files.
struct JoinedModule {
    stub: ModuleRole,
    implementation: ModuleRole,
    implementation_public: BTreeSet<String>,
}

impl ModulePair {
    /// Joins declarations carrying the same current established canonical identity.
    /// The last implementation form in source order supplies the diagnostic target.
    fn join(self, analyzed: &[AnalyzedFile]) -> JoinedModule {
        let stub = &analyzed[self.stub];
        let implementation = &analyzed[self.implementation];
        let bound = bound_names(implementation);
        let mut stub_forms = BTreeMap::new();
        let mut implementation_public = BTreeSet::new();
        for (name, declared) in stub_forms_by_name(stub) {
            let Some(target) = bound.get(name) else {
                continue;
            };
            if declared
                .iter()
                .any(|form| stub.is_public(&form.qualified_name))
            {
                implementation_public.insert(target.qualified_name.clone());
            }
            let forms = declared.iter().map(|form| stub_form(stub, form)).collect();
            stub_forms.insert(target.qualified_name.clone(), forms);
        }
        JoinedModule {
            stub: ModuleRole::Stub,
            implementation: ModuleRole::Implementation { stub_forms },
            implementation_public,
        }
    }
}

/// Each current established identity and its last implementation form in source order.
fn bound_names(implementation: &AnalyzedFile) -> BTreeMap<&rift_core::SymbolId, &SyntaxSymbol> {
    implementation
        .file
        .syntax()
        .symbols()
        .iter()
        .filter_map(|symbol| {
            implementation
                .placement
                .logical_identity(&symbol.qualified_name)
                .map(|identity| (identity, symbol))
        })
        .collect()
}

/// Stub forms grouped only by current established canonical identity.
fn stub_forms_by_name(stub: &AnalyzedFile) -> BTreeMap<&rift_core::SymbolId, Vec<&SyntaxSymbol>> {
    let mut forms: BTreeMap<&rift_core::SymbolId, Vec<&SyntaxSymbol>> = BTreeMap::new();
    for symbol in stub.file.syntax().symbols() {
        if let Some(identity) = stub.placement.logical_identity(&symbol.qualified_name) {
            forms.entry(identity).or_default().push(symbol);
        }
    }
    forms
}

/// One stub declaration as the form a joined declaration answers for.
fn stub_form(stub: &AnalyzedFile, form: &SyntaxSymbol) -> StubForm {
    StubForm {
        identity: identity_of(stub, &form.qualified_name),
        path: ProjectPath(stub.file.path().as_str().to_owned()),
        range: TextRange {
            start: form.range.start,
            end: form.range.end,
        },
    }
}

/// The identity one declaration of `held` carries.
fn identity_of(held: &AnalyzedFile, qualified_name: &str) -> SymbolId {
    SymbolId(symbol_identity(
        &held.file.syntax().language().identity_segment(),
        held.placement.identity_path(),
        qualified_name,
    ))
}

#[cfg(test)]
mod tests {
    use super::{ModulePair, module_pairs};

    fn pairs(paths: &[&str]) -> Vec<(String, String)> {
        module_pairs(paths.iter().copied())
            .into_iter()
            .map(
                |ModulePair {
                     stub,
                     implementation,
                 }| (paths[stub].to_owned(), paths[implementation].to_owned()),
            )
            .collect()
    }

    #[test]
    fn test_each_stub_pairs_with_its_module_by_suffix() {
        let found = pairs(&[
            "attr/__init__.py",
            "attr/__init__.pyi",
            "attr/_make.py",
            "lib/fs.d.ts",
            "lib/fs.js",
            "dist/cli.d.mts",
            "dist/cli.mjs",
            "dist/cli.d.cts",
            "dist/cli.cjs",
            "types/orphan.d.ts",
        ]);

        assert_eq!(
            found,
            [
                ("attr/__init__.pyi", "attr/__init__.py"),
                ("lib/fs.d.ts", "lib/fs.js"),
                ("dist/cli.d.mts", "dist/cli.mjs"),
                ("dist/cli.d.cts", "dist/cli.cjs"),
            ]
            .map(|(stub, implementation)| (stub.to_owned(), implementation.to_owned()))
        );
    }

    /// prettier 3.9.9 ships `index.mjs` and `index.cjs` beside one `index.d.ts`, and the
    /// ESM build is the one declaring the names.
    #[test]
    fn test_a_declaration_file_without_a_js_build_pairs_the_esm_build_first() {
        assert_eq!(
            pairs(&["index.cjs", "index.d.ts", "index.mjs"]),
            [("index.d.ts".to_owned(), "index.mjs".to_owned())]
        );
        assert_eq!(
            pairs(&["index.cjs", "index.d.ts"]),
            [("index.d.ts".to_owned(), "index.cjs".to_owned())]
        );
    }

    /// An exact build claims its implementation before a `.d.ts` falls back to it.
    #[test]
    fn test_an_exact_build_claims_its_implementation_before_a_fallback() {
        assert_eq!(
            pairs(&["index.d.mts", "index.d.ts", "index.mjs"]),
            [("index.d.mts".to_owned(), "index.mjs".to_owned())]
        );
    }

    #[test]
    fn test_repeated_declarations_without_overload_proof_do_not_join() {
        let analysis = super::super::fixture::package_result(
            rift_syntax::ShippedLanguage::Python,
            vec![
                (
                    "mod.py",
                    "def repeat(value): return value\ndef repeat(value): return value + 1\n",
                ),
                (
                    "mod.pyi",
                    "def repeat(value: int) -> int: ...\ndef repeat(value: str) -> str: ...\n",
                ),
            ],
            None,
        )
        .expect("bounded repeated declarations");
        let implementation = analysis
            .files()
            .iter()
            .find(|held| held.file.path().as_str() == "mod.py")
            .expect("retained implementation");
        for symbol in implementation.file.syntax().symbols() {
            assert!(implementation.stub_forms(&symbol.qualified_name).is_empty());
        }
        assert_eq!(analysis.publication().declarations.len(), 4);
        let identities = analysis
            .publication()
            .declarations
            .iter()
            .map(|declaration| &declaration.symbol)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(identities.len(), 4);
    }
}
