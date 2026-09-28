//! The stub and module join: one module's stub and implementation answer as one
//! declaration set.
//!
//! A package can ship one module twice: as implementation source (`mod.py`, `index.js`)
//! and as a stub declaring its types (`mod.pyi`, `index.d.ts`). Analyzed apart, a function
//! both files declare answers twice, under two identities that differ only by the file
//! extension. The join pairs the two files of one module and joins their declarations by
//! qualified name, so one symbol answers at the implementation address with the stub's
//! signatures and types.
//!
//! The join runs on the parsed documents before the semantic build and acts on the records
//! the analyzer emits. Both documents still reach the build under their own identities, so
//! no two contributions share one identity.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::symbol_identity;
use rift_protocol::read::{ProjectPath, Symbol, SymbolId, TextRange};
use rift_syntax::SyntaxSymbol;

use super::AnalyzedFile;
use crate::semantic::WorkspaceSemantics;

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

/// The provider's suffix marker: a repeated qualified name `f` becomes `f~1`, `f~2`.
const FORM_SUFFIX_MARKER: char = '~';

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
    /// A stub whose joined declarations answer at the implementation address.
    Stub {
        /// Each joined stub declaration's qualified name, and the implementation identity
        /// that answers for it.
        answered_by: BTreeMap<String, SymbolId>,
    },
    /// An implementation whose joined declarations take the stub's signatures and types.
    Implementation {
        /// Each joined implementation declaration's qualified name, and the stub forms it
        /// joins, in the stub's source order.
        stub_forms: BTreeMap<String, Vec<StubForm>>,
    },
}

impl ModuleRole {
    /// Whether the declaration spelled `qualified_name` answers at another address.
    pub(super) fn answers_elsewhere(&self, qualified_name: &str) -> bool {
        match self {
            Self::Stub { answered_by } => answered_by.contains_key(qualified_name),
            Self::Unpaired | Self::Implementation { .. } => false,
        }
    }

    /// The identity answering for a stub container that joined its implementation.
    fn joined_container(&self, container: &str) -> Option<&SymbolId> {
        match self {
            Self::Stub { answered_by } => answered_by.get(container),
            Self::Unpaired | Self::Implementation { .. } => None,
        }
    }

    /// The stub forms one implementation declaration joins; empty for any other.
    pub(super) fn stub_forms(&self, qualified_name: &str) -> &[StubForm] {
        match self {
            Self::Implementation { stub_forms } => {
                stub_forms.get(qualified_name).map_or(&[], Vec::as_slice)
            }
            Self::Unpaired | Self::Stub { .. } => &[],
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
    /// Joins the pair's declarations by qualified name, with a name's `~N` forms grouped.
    ///
    /// A stub's overloads (`f~1`, `f~2`) join one implementation declaration. When the
    /// implementation repeats the name, as a Python module repeating its `typing.overload`
    /// forms before the implementation does, the last form in source order is the one the
    /// module binds, so it answers.
    fn join(self, analyzed: &[AnalyzedFile]) -> JoinedModule {
        let stub = &analyzed[self.stub];
        let implementation = &analyzed[self.implementation];
        let bound = bound_names(implementation);
        let mut answered_by = BTreeMap::new();
        let mut stub_forms = BTreeMap::new();
        let mut implementation_public = BTreeSet::new();
        for (name, declared) in stub_forms_by_name(stub) {
            let Some(target) = bound.get(name) else {
                continue;
            };
            let target_identity = identity_of(implementation, &target.qualified_name);
            for form in &declared {
                answered_by.insert(form.qualified_name.clone(), target_identity.clone());
            }
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
            stub: ModuleRole::Stub { answered_by },
            implementation: ModuleRole::Implementation { stub_forms },
            implementation_public,
        }
    }
}

/// Each name the implementation binds, `~N` forms grouped, and the declaration that binds
/// it: the last form in source order.
fn bound_names(implementation: &AnalyzedFile) -> BTreeMap<&str, &SyntaxSymbol> {
    implementation
        .file
        .syntax()
        .symbols()
        .iter()
        .map(|symbol| (base_name(&symbol.qualified_name), symbol))
        .collect()
}

/// Each name the stub declares, `~N` forms grouped, and its forms in source order.
fn stub_forms_by_name(stub: &AnalyzedFile) -> BTreeMap<&str, Vec<&SyntaxSymbol>> {
    let mut forms: BTreeMap<&str, Vec<&SyntaxSymbol>> = BTreeMap::new();
    for symbol in stub.file.syntax().symbols() {
        forms
            .entry(base_name(&symbol.qualified_name))
            .or_default()
            .push(symbol);
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

/// Lays what the join decided for `declaration` over its assembled presentation, and
/// reports whether the declaration joined stub forms.
///
/// A joined implementation declaration takes the stub's facts; a stub declaration whose
/// container joined names the implementation identity that answers for it as its
/// container.
pub(super) fn lay_join(
    presentation: &mut Symbol,
    semantics: &WorkspaceSemantics,
    role: &ModuleRole,
    declaration: &SyntaxSymbol,
) -> bool {
    if let Some(container) = declaration
        .container
        .as_deref()
        .and_then(|container| role.joined_container(container))
    {
        presentation.container = Some(container.clone());
    }
    let forms = role.stub_forms(&declaration.qualified_name);
    if forms.is_empty() {
        return false;
    }
    lay_stub_facts(presentation, semantics, forms);
    true
}

/// Lays the stub's facts over one joined implementation declaration.
///
/// The stub's signatures and types replace the implementation's when the stub renders any;
/// a stub declaring a name without a signature, such as a `.d.ts` variable over a `.js`
/// function, leaves the implementation's own. Source, range, and documentation stay the
/// implementation's.
fn lay_stub_facts(presentation: &mut Symbol, semantics: &WorkspaceSemantics, forms: &[StubForm]) {
    let stub: Vec<Symbol> = forms
        .iter()
        .filter_map(|form| {
            let assembled = semantics.assembled(&form.identity.0)?;
            assembled
                .facts()
                .map(|facts| assembled.to_protocol_symbol(facts))
        })
        .collect();
    let signatures: Vec<_> = stub
        .iter()
        .flat_map(|form| form.signatures.iter().cloned())
        .collect();
    if !signatures.is_empty() {
        presentation.signatures = signatures;
    }
    let types: Vec<_> = stub
        .iter()
        .flat_map(|form| form.types.iter().cloned())
        .collect();
    if !types.is_empty() {
        presentation.types = types;
    }
}

/// The name a declaration's `~N` forms share: `f~2` is a form of `f`.
///
/// The paired languages spell no `~` in an identifier, so a `~` followed by digits alone is
/// always the provider's suffix.
fn base_name(qualified_name: &str) -> &str {
    match qualified_name.rsplit_once(FORM_SUFFIX_MARKER) {
        Some((base, number))
            if !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            base
        }
        _ => qualified_name,
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
    use super::{ModulePair, base_name, module_pairs};

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
    fn test_base_name_strips_only_a_numeric_form_suffix() {
        assert_eq!(base_name("readFile~5"), "readFile");
        assert_eq!(base_name("C.m~12"), "C.m");
        assert_eq!(base_name("f"), "f");
        assert_eq!(base_name("f~"), "f~");
        assert_eq!(base_name("f~x"), "f~x");
    }
}
