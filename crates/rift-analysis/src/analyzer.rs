//! The package analyzer: one package's source bytes into one canonical publication.
//!
//! [`PackageAnalyzer`] holds no I/O. It takes the bytes a caller already read, parses
//! them with the shipped syntax providers, places each file under the package's own
//! identity, assembles the declarations through [`WorkspaceSemantics`], and renders the
//! result as a [`PackagePublication`], the shape global ingestion consumes.
//!
//! Every record carries the digest of its own canonical content, and the publication as a
//! whole renders as RFC 8785 canonical JSON, so two runs over the same bytes under the
//! same analyzer revision compare byte for byte.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;

use crate::documentation::{
    DocumentationDeclaration, DocumentationInput, DocumentationSourceSet, collect_documentation,
    content_chunk_identity, content_digest,
};
use crate::input::{ExactPackageInput, ExactPackageLimits, RetainedSourceLimits};
use crate::revision::analyzer_revision;
use crate::selection::documentation_format;
use crate::semantic::{PlacedFacts, WorkspaceSemantics};
use crate::source::IndexedFile;
use rift_core::constants::DIGEST_WIRE_CHARS;
use rift_core::line::{line_of, line_starts};
use rift_core::{
    ContributionOrigin, PortableSymbolFacts, ProjectPath as CoreProjectPath, SourceKind,
    SourceUnitId as CoreSourceUnitId, SymbolId as CoreSymbolId,
};
use rift_error::{RiftError, errors};
use rift_protocol::canonical::canonical_json;
use rift_protocol::documentation::{
    DocumentationChunk, DocumentationContentIdentity, DocumentationSelectionReason,
    DocumentationSource, DocumentationSourceFormat, DocumentationSourceIdentity,
    DocumentationWarningKind, NotebookCellKind,
};
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::index::{
    PACKAGE_PUBLICATION_FORMAT_REVISION, PACKAGE_SOURCE_BYTES_MAX, PackageAnalysisWarning,
    PackageCoverage, PackageDocument, PackageDocumentKind, PackageOccurrence, PackagePublication,
    PackageSourceUnit, PackageSymbol,
};
use rift_protocol::producer::{FactCoverage, FactKind};
use rift_protocol::read::{
    Digest, EdgeKind, ExactKind, Language, ProjectPath, SourceLocationKind, SourceUnitId, Symbol,
    SymbolId, SymbolKind, SymbolOrigin, TextRange,
};
use rift_syntax::{
    ByteRange, DocumentPlacement, ShippedLanguage, StaticBindingKind, SyntaxFacts, SyntaxSymbol,
};
use serde::Serialize;

use rift_ranking::split_identifier_words;

#[cfg(test)]
mod coverage_tests;
mod file_modules;
#[cfg(test)]
mod fixture;
mod join;
#[cfg(test)]
mod join_tests;
#[cfg(test)]
mod limits_tests;
pub(crate) mod namespace;
#[cfg(test)]
mod publication_tests;

#[cfg(test)]
mod retained_tests;
mod rust_lsp_declarations;
mod rust_lsp_macro_items;
pub use rust_lsp_macro_items::{
    RustLspMacroAdmission, RustLspMacroDefinition, RustLspMacroFile, RustLspMacroItems,
    RustLspMacroSite,
};
mod rust_lsp_operators;

use join::ModuleRole;
pub use join::StubForm;
pub use rust_lsp_declarations::{
    RustLspDeclaration, RustLspDeclarationAdmission, RustLspDeclarationFile,
};
pub use rust_lsp_operators::{
    RustLspOperatorDefinition, RustLspOperatorEdges, RustLspOperatorSite,
};

pub(super) fn package_label(owner: &SymbolOwner) -> String {
    match owner {
        SymbolOwner::Package { manager, name, .. } => format!("{manager}/{name}"),
        SymbolOwner::Runtime { runtime } => format!("stdlib/{runtime}"),
        SymbolOwner::Local => "local".to_owned(),
        SymbolOwner::NamedLocal { name } => format!("local@{name}"),
        SymbolOwner::ProfileLocal { name } => format!("local:{name}"),
        SymbolOwner::NamedProfileLocal { project, name } => format!("local@{project}:{name}"),
    }
}

/// One package file as the analyzer holds it: the parsed document, where it is filed, the
/// public declarations it carries, and what the stub and module join decided for it.
#[derive(Debug)]
pub struct AnalyzedFile {
    file: IndexedFile,
    /// A Cargo library declaration belongs to this placement, outside cached syntax.
    placed_syntax: Option<Box<SyntaxFacts>>,
    rust_module_source: Option<Arc<SyntaxFacts>>,
    placement: DocumentPlacement,
    shared_rust_placements: Vec<DocumentPlacement>,
    public_names: BTreeSet<String>,
    /// Whether a consumer can import the file's own module.
    reached: bool,
    item_kinds: BTreeMap<(u64, u64), &'static str>,
    role: ModuleRole,
}

impl AnalyzedFile {
    fn placed_syntax(&self) -> &SyntaxFacts {
        self.placed_syntax
            .as_deref()
            .unwrap_or_else(|| self.file.syntax())
    }
    fn primary_declaration_count(&self) -> usize {
        self.placed_syntax()
            .primary_symbols_len()
            .saturating_mul(self.shared_rust_placements.len().max(1))
            .saturating_add(self.placement.aliases_count())
    }

    fn static_source(&self) -> crate::static_resolution::StaticSource<'_> {
        crate::static_resolution::StaticSource {
            path: self.file.path(),
            source: self.file.source(),
            facts: self.placed_syntax(),
            placement: &self.placement,
        }
    }

    fn placed_facts(&self) -> PlacedFacts<'_> {
        PlacedFacts {
            facts: self.placed_syntax(),
            path: self.file.path(),
            placement: self.placement.clone(),
        }
    }

    /// The parsed file.
    #[must_use]
    pub const fn file(&self) -> &IndexedFile {
        &self.file
    }

    /// The grammar kind of the item syntax element of the declaration bound at `range`; absent when
    /// the syntax provider names none.
    fn item_kind(&self, range: rift_core::SourceRange, qualified_name: &str) -> Option<String> {
        if matches!(qualified_name, "." | "..") && self.file.syntax().language().name == "rust" {
            return Some("source_file".to_owned());
        }
        let key = (range.start(), range.end());
        self.item_kinds.get(&key).map(|kind| (*kind).to_owned())
    }

    /// The unit, origin, and identity path the file's declarations carry.
    #[must_use]
    pub const fn placement(&self) -> &DocumentPlacement {
        &self.placement
    }

    /// Withdraws the export rule's answer for a file no consumer imports, except the names
    /// a reached module aliases or the file declares in an ambient module, with their
    /// members.
    fn retain_exposed(&mut self, exposed: Option<&BTreeSet<String>>) {
        self.reached = exposed.is_some_and(|names| names.contains("."));
        if self.reached {
            return;
        }
        self.public_names.retain(|name| {
            exposed.is_some_and(|exposed| {
                exposed.contains(name)
                    || name
                        .match_indices('.')
                        .any(|(end, _)| exposed.contains(&name[..end]))
            })
        });
    }

    /// Whether the package exports the declaration spelled by `qualified_name`.
    #[must_use]
    pub fn is_public(&self, qualified_name: &str) -> bool {
        self.public_names.contains(qualified_name)
    }

    /// The stub declarations the symbol spelled by `qualified_name` answers for, in the
    /// stub's source order: each one's identity, stub path, and range. Every declaration
    /// of the symbol in this file answers for the same forms. Empty unless this file is an
    /// implementation whose declaration joined its stub.
    #[must_use]
    pub fn stub_forms(&self, qualified_name: &str) -> &[StubForm] {
        self.role.stub_forms(qualified_name)
    }
}

/// What one analyzer run produced: the canonical publication, and the parsed material the
/// same pass built it from.
///
/// The publication is the artifact a consumer stores and compares. The parsed files and
/// the assembled graph are the same pass's working values, handed on so a consumer reads
/// them without parsing the package a second time.
#[derive(Debug)]
pub struct PackageAnalysis {
    publication: PackagePublication,
    files: Vec<AnalyzedFile>,
    semantics: WorkspaceSemantics,
    notebook_cells: BTreeMap<DocumentationContentIdentity, String>,
    syntax_work: crate::PackageSyntaxWork,
    warnings: Vec<rift_protocol::read::ReadWarning>,
}

impl PackageAnalysis {
    /// The canonical publication this run produced.
    #[must_use]
    pub const fn publication(&self) -> &PackagePublication {
        &self.publication
    }

    /// Returns actual syntax provider calls and accepted reuse for this analysis.
    #[must_use]
    pub const fn syntax_work(&self) -> crate::PackageSyntaxWork {
        self.syntax_work
    }

    /// Framework context that could not be resolved from captured sources.
    #[must_use]
    pub fn warnings(&self) -> &[rift_protocol::read::ReadWarning] {
        &self.warnings
    }

    /// Every analyzed file, in path order.
    #[must_use]
    pub fn files(&self) -> &[AnalyzedFile] {
        &self.files
    }

    /// Takes the run apart for a consumer that keeps every part.
    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        PackagePublication,
        Vec<AnalyzedFile>,
        WorkspaceSemantics,
        BTreeMap<DocumentationContentIdentity, String>,
    ) {
        (
            self.publication,
            self.files,
            self.semantics,
            self.notebook_cells,
        )
    }
}

/// Turns one package's source bytes into one canonical publication.
#[derive(Debug, Default)]
pub struct PackageAnalyzer;

fn analyzed_file(
    input: &ExactPackageInput<'_>,
    file: crate::PackageSource<'_>,
    supplied: &mut impl FnMut(&crate::PackageSyntaxSource<'_>) -> Option<crate::PackageSyntax>,
    work: &mut crate::PackageSyntaxWork,
    context: &crate::FrameworkContext,
    warnings: &mut Vec<rift_protocol::read::ReadWarning>,
) -> Result<AnalyzedFile, RiftError> {
    let source = crate::PackageSyntaxSource::new(
        file,
        input.owner(),
        input.language(),
        input.limits().syntax(),
    );
    let candidate = supplied(&source).filter(|facts| source.accepts(facts));
    let accepted = candidate.is_some();
    let syntax = match candidate {
        Some(syntax) => syntax,
        None => source.parse()?,
    };
    work.provider_calls = work
        .provider_calls
        .checked_add(source.provider_calls())
        .expect("package syntax provider call count must fit u64");
    if accepted && source.provider_calls() == 0 && context.for_path(file.path()).is_none() {
        work.reused_files += 1;
    }
    let facts = if context.for_path(file.path()).is_some() {
        let document = source.parse_document()?;
        let (document, found, calls) = context.apply_for_publication(
            rift_syntax::SyntaxSource {
                path: file.path(),
                text: file.text(),
            },
            input.limits().syntax(),
            document,
        )?;
        work.provider_calls += 1 + calls;
        warnings.extend(found);
        document.shared_facts()
    } else {
        std::sync::Arc::clone(syntax.facts())
    };
    let parsed = IndexedFile::new_with_shared_syntax(
        file.path().clone(),
        file.text().to_owned().into(),
        source.identity().source_digest,
        false,
        facts,
    );
    let placement =
        placement_of(input.owner(), input.origin(), file)?.with_identity_anchors(BTreeMap::new());
    let public_names =
        public_qualified_names(parsed.syntax().language(), parsed.source(), parsed.syntax());
    let item_kinds = item_kinds(parsed.syntax().symbols());
    Ok(AnalyzedFile {
        file: parsed,
        placed_syntax: None,
        rust_module_source: None,
        placement,
        shared_rust_placements: Vec::new(),
        public_names,
        reached: true,
        item_kinds,
        role: ModuleRole::Unpaired,
    })
}

/// The grammar kind of each declaration's item syntax element, keyed by the item range a declaration
/// binding spans. One pass over the file's declarations, so a binding's lookup is
/// logarithmic.
fn item_kinds(symbols: &[rift_syntax::SyntaxSymbol]) -> BTreeMap<(u64, u64), &'static str> {
    symbols
        .iter()
        .filter_map(|symbol| {
            let kind = symbol.syntax_element_kind?;
            Some(((symbol.item_range.start, symbol.item_range.end), kind))
        })
        .collect()
}

fn external_work_limits(input: &ExactPackageInput<'_>) -> (std::cell::Cell<usize>, usize) {
    (
        std::cell::Cell::new(0),
        input
            .limits()
            .syntax()
            .syntax_elements_max()
            .checked_mul(input.limits().syntax().syntax_depth_max())
            .unwrap_or(0),
    )
}

impl PackageAnalyzer {
    /// Analyzes one exact package's selected files.
    ///
    /// Each file retains its original path under the exact package or runtime source
    /// owner. Its origin is the input origin with the same complete owner.
    /// A stub and the module it declares (`mod.pyi` and `mod.py`, `index.d.ts` and
    /// `index.js`) join: each name both declare answers once, at the module, with the
    /// stub's signatures and types. Records are emitted in unit, symbol, and document
    /// identity order, and a collection that reaches its bound stops there and reports the
    /// stop as a warning.
    ///
    /// The work is proportional to the selected bytes: raw parsing and framework
    /// context passes, one scan per file for its line starts, one assembly pass over the
    /// parsed declarations, and one canonical rendering per record. Files with framework
    /// context parse current source before applying framework facts.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the identity cannot spell a resolver or unit,
    /// when no provider parses a file or its parse fails, or when publication or
    /// normalization refuses the package graph.
    pub fn analyze(
        input: ExactPackageInput<'_>,
        revision: u64,
    ) -> Result<PackageAnalysis, RiftError> {
        Self::analyze_with_syntax(input, revision, |_| None)
    }

    /// Assembles a package with optional per-file syntax supplied by the caller.
    ///
    /// The hook runs once per selected file in input order. Its source exposes the actual
    /// provider identity and bounds before parsing. A missing or incompatible value runs
    /// the current provider. Call `source.parse()` inside the hook to retain successful
    /// file work before a later failure; parser calls remain included in `syntax_work()`.
    ///
    /// Every call rebuilds current placement, module joins, semantics, documentation
    /// edges, and publication. Retention, encoding, and lookup remain caller-owned.
    /// The work is proportional to selected bytes and declarations plus caller hook work.
    ///
    /// # Errors
    ///
    /// Returns the same source and assembly failures as [`Self::analyze`].
    pub fn analyze_with_syntax(
        input: ExactPackageInput<'_>,
        revision: u64,
        mut supplied: impl FnMut(&crate::PackageSyntaxSource<'_>) -> Option<crate::PackageSyntax>,
    ) -> Result<PackageAnalysis, RiftError> {
        let sources = input
            .files()
            .iter()
            .chain(input.context_sources())
            .copied()
            .collect::<Vec<_>>();
        let context = crate::FrameworkContext::resolve(
            &sources,
            input.frameworks(),
            input.limits().syntax(),
            &|| false,
        )?;
        let mut warnings = context.warnings().to_vec();
        let mut syntax_work = crate::PackageSyntaxWork {
            provider_calls: context.provider_calls(),
            ..crate::PackageSyntaxWork::default()
        };
        let mut analyzed = Vec::with_capacity(input.files().len());
        for file in input.files() {
            analyzed.push(analyzed_file(
                &input,
                *file,
                &mut supplied,
                &mut syntax_work,
                &context,
                &mut warnings,
            )?);
        }
        analyzed.sort_by(|left, right| left.file.path().cmp(right.file.path()));
        let namespace = place_namespace(&input, &mut analyzed)?;
        join::join_modules(&mut analyzed);
        let (semantics, local_declaration_limit) =
            assemble_package_graph(&input, &analyzed, &namespace, revision)?;
        let (publication, notebook_cells) = publish(
            &input,
            &analyzed,
            &semantics,
            namespace.unresolved_exports,
            local_declaration_limit,
        )?;
        Ok(PackageAnalysis {
            publication,
            files: analyzed,
            semantics,
            notebook_cells,
            syntax_work,
            warnings,
        })
    }
}

/// Assembles the captured package graph and reserves its primary declaration budget.
fn assemble_package_graph(
    input: &ExactPackageInput<'_>,
    analyzed: &[AnalyzedFile],
    namespace: &namespace::PreparedNamespace,
    revision: u64,
) -> Result<(WorkspaceSemantics, Option<usize>), RiftError> {
    let mut placed = analyzed
        .iter()
        .filter(|held| held.shared_rust_placements.is_empty())
        .map(AnalyzedFile::placed_facts)
        .collect::<Vec<_>>();
    placed.extend(analyzed.iter().filter_map(|held| {
        let facts = held.rust_module_source.as_deref()?;
        let facet = facts.symbols().first()?;
        let module = held.placement.module()?.clone();
        Some(PlacedFacts {
            facts,
            path: held.file.path(),
            placement: DocumentPlacement::new(
                held.placement.origin().clone(),
                held.placement.unit().clone(),
                held.placement.identity_path(),
            )
            .with_identity_anchors(BTreeMap::from([(facet.qualified_name.clone(), module)])),
        })
    }));
    let shared = analyzed
        .iter()
        .flat_map(|held| {
            held.shared_rust_placements.iter().map(|placement| {
                crate::static_resolution::StaticSource {
                    placement,
                    ..held.static_source()
                }
            })
        })
        .collect::<Vec<_>>();
    placed.extend(shared.iter().map(|source| PlacedFacts {
        facts: source.facts,
        path: source.path,
        // The source unit has no single module owner; only its declarations do.
        placement: source.placement.clone().with_module(None),
    }));
    let (bindings, mut binding_slots) = binding_capacity(
        analyzed,
        input.limits(),
        usize::from(namespace.rust_package_root.is_some()),
    );
    let binding_limit = binding_slots.remaining();
    let (external_work, external_work_max) = external_work_limits(input);
    let static_sources = analyzed
        .iter()
        .map(AnalyzedFile::static_source)
        .collect::<Vec<_>>();
    let built = WorkspaceSemantics::build_facts_placed_with_external(
        &placed,
        input.limits().declarations_max(),
        input.limits().edges_max(),
        revision,
        &|reference| {
            if reference.provider().as_str() != rift_syntax::SYNTAX_PROVIDER_ID {
                return None;
            }
            namespace.exports.iter().find_map(|export| {
                let work = external_work
                    .get()
                    .checked_add(1)?
                    .checked_add(export.target.as_str().len())?;
                external_work.set(work);
                if work > external_work_max {
                    return None;
                }
                let parsed =
                    rift_protocol::identity::SymbolIdentity::parse(export.target.as_str()).ok()?;
                if parsed.owner() == input.owner() {
                    return None;
                }
                let target =
                    rift_core::ProviderSymbolId::for_symbol(export.target.as_str()).ok()?;
                (reference.symbol() == &target).then(|| export.target.clone())
            })
        },
        (&static_sources, &shared, &input.namespace_input()),
        &mut binding_slots,
    )
    .map_err(|error| {
        errors::analysis::package_provider_failed()
            .package(package_label(input.owner()))
            .cause(error)
            .error()
    })?;
    if let Some(path) = built.beyond_declaration_bound.first() {
        return errors::analysis::package_declarations_exceeded()
            .package(package_label(input.owner()))
            .path(path.as_str())
            .fail();
    }
    let truncated = (bindings > binding_limit - binding_slots.remaining()).then_some(binding_limit);
    Ok((built.semantics, truncated))
}

#[derive(Default)]
struct BindingDemands {
    rust_locals: BindingDemand,
    rust_parameters: BindingDemand,
    rust_receivers: BindingDemand,
    rust_generics: BindingDemand,
    rust_library: BindingDemand,
    rust_explicit: BindingDemand,
    rust_shorthand: BindingDemand,
    rust_macro_methods: BindingDemand,
    rust_disjoint: BindingDemand,
    python_parameters: BindingDemand,
    python_receivers: BindingDemand,
    python_loop_bindings: BindingDemand,
    python_comprehension_bindings: BindingDemand,
    python_variadic_parameters: BindingDemand,
    python_overload_parameters: BindingDemand,
    ecmascript: EcmaScriptBindingDemand,
    primary: usize,
    unnamed: usize,
}

impl BindingDemands {
    fn collect(documents: &[AnalyzedFile]) -> Self {
        let mut demands = Self::default();
        for document in documents {
            let facts = document.placed_syntax();
            demands.primary = demands
                .primary
                .saturating_add(document.primary_declaration_count());
            demands.unnamed = demands
                .unnamed
                .saturating_add(facts.left_out_symbols().len());
            if !document.shared_rust_placements.is_empty() {
                continue;
            }
            let receiver_names = facts.python_receiver_names();
            let methods = facts.typescript_method_names();
            for symbol in facts.symbols().iter().filter(|symbol| {
                symbol.is_package_binding()
                    && document
                        .placement
                        .logical_identity(&symbol.qualified_name)
                        .is_some()
            }) {
                let language = facts.language().name.as_str();
                let demand = if matches!(language, "javascript" | "typescript")
                    && symbol.is_ecmascript_package_binding()
                {
                    demands
                        .ecmascript
                        .classify(symbol, language == "javascript", &methods)
                } else if symbol.is_python_variadic_parameter() {
                    &mut demands.python_variadic_parameters
                } else if symbol.is_python_overload_parameter() {
                    &mut demands.python_overload_parameters
                } else if symbol.is_python_parameter() {
                    if symbol
                        .name_range
                        .is_some_and(|name| receiver_names.contains(&name))
                    {
                        &mut demands.python_receivers
                    } else {
                        &mut demands.python_parameters
                    }
                } else if symbol.is_rust_receiver_binding() {
                    &mut demands.rust_receivers
                } else if facts.is_disjoint_rust_binding(symbol) {
                    &mut demands.rust_disjoint
                } else if symbol.is_rust_shorthand_binding() {
                    &mut demands.rust_shorthand
                } else if language == "rust" && symbol.is_rust_macro_method() {
                    &mut demands.rust_macro_methods
                } else if symbol.is_rust_explicit_local_binding() {
                    &mut demands.rust_explicit
                } else if symbol.is_rust_library_root() {
                    &mut demands.rust_library
                } else if symbol.is_rust_generic_parameter() {
                    &mut demands.rust_generics
                } else if symbol.is_rust_parameter_binding() {
                    &mut demands.rust_parameters
                } else if symbol.is_python_loop_binding() {
                    &mut demands.python_loop_bindings
                } else if symbol.is_python_comprehension_binding() {
                    &mut demands.python_comprehension_bindings
                } else {
                    &mut demands.rust_locals
                };
                demand.observe(symbol.item_range);
            }
        }
        demands
    }

    fn count(&self) -> usize {
        self.rust_locals.count
            + self.rust_parameters.count
            + self.python_parameters.count
            + self.python_receivers.count
            + self.python_loop_bindings.count
            + self.python_comprehension_bindings.count
            + self.python_variadic_parameters.count
            + self.python_overload_parameters.count
            + self.rust_receivers.count
            + self.rust_generics.count
            + self.rust_library.count
            + self.rust_explicit.count
            + self.rust_shorthand.count
            + self.rust_macro_methods.count
            + self.rust_disjoint.count
            + self.ecmascript.count()
    }
}

fn binding_capacity(
    documents: &[AnalyzedFile],
    limits: ExactPackageLimits,
    synthetic_objects: usize,
) -> (usize, rift_syntax::PackageBindingSlots) {
    let demands = BindingDemands::collect(documents);
    let rust_module_sources = BindingDemand::module_sources(documents);
    let provider = limits.declarations_max().saturating_sub(demands.primary);
    let publication = bound(limits.publication().symbols).saturating_sub(
        demands
            .primary
            .saturating_add(demands.unnamed)
            .saturating_add(synthetic_objects),
    );
    let count = demands.count() + rust_module_sources.count;
    let (mut bytes, record) = binding_source_capacity(documents, limits, count);
    let mut slots = provider.min(publication);
    let local_slots = demands.rust_locals.reserve(&mut slots, &mut bytes, record);
    let rust_parameter_slots = demands
        .rust_parameters
        .reserve(&mut slots, &mut bytes, record);
    let python_parameter_slots = demands
        .python_parameters
        .reserve(&mut slots, &mut bytes, record);
    let typescript_parameter_slots = demands
        .ecmascript
        .typescript_functions
        .reserve(&mut slots, &mut bytes, record);
    let python_receiver_slots = demands
        .python_receivers
        .reserve(&mut slots, &mut bytes, record);
    let python_loop_binding_slots = demands
        .python_loop_bindings
        .reserve(&mut slots, &mut bytes, record);
    let python_variadic_slots = demands
        .python_variadic_parameters
        .reserve(&mut slots, &mut bytes, record);
    let python_overload_slots = demands
        .python_overload_parameters
        .reserve(&mut slots, &mut bytes, record);
    let javascript_parameter_slots = demands
        .ecmascript
        .javascript_functions
        .reserve(&mut slots, &mut bytes, record);
    let typescript_method_slots = demands
        .ecmascript
        .typescript_methods
        .reserve(&mut slots, &mut bytes, record);
    let rust_receiver_slots = demands
        .rust_receivers
        .reserve(&mut slots, &mut bytes, record);
    let rust_generic_slots = demands
        .rust_generics
        .reserve(&mut slots, &mut bytes, record);
    let root_slot = demands.rust_library.reserve(&mut slots, &mut bytes, record);
    let explicit_slots = demands
        .rust_explicit
        .reserve(&mut slots, &mut bytes, record);
    let established = rift_syntax::PackageBindingSlots::new(
        local_slots,
        rust_parameter_slots,
        python_parameter_slots,
        typescript_parameter_slots,
    )
    .with_rust_receivers(rust_receiver_slots)
    .with_rust_generics(rust_generic_slots)
    .with_rust_library_root(root_slot)
    .with_rust_explicit_locals(explicit_slots)
    .with_python_receiver_slots(python_receiver_slots)
    .with_python_loop_binding_slots(python_loop_binding_slots)
    .with_python_variadic_parameter_slots(python_variadic_slots)
    .with_python_overload_parameter_slots(python_overload_slots)
    .with_javascript_parameters(javascript_parameter_slots)
    .with_typescript_method_parameters(typescript_method_slots);
    (
        count,
        demands.ecmascript.reserve_late_bindings(
            documents,
            [
                &rust_module_sources,
                &demands.rust_shorthand,
                &demands.rust_macro_methods,
                &demands.rust_disjoint,
            ],
            &demands.python_comprehension_bindings,
            (slots, bytes),
            record,
            &established,
        ),
    )
}

#[derive(Default)]
struct EcmaScriptBindingDemand {
    typescript_functions: BindingDemand,
    javascript_functions: BindingDemand,
    typescript_methods: BindingDemand,
    javascript_callbacks: BindingDemand,
    typescript_overloads: BindingDemand,
    javascript_methods: BindingDemand,
    typescript_signatures: BindingDemand,
    javascript_scoped_functions: BindingDemand,
    typescript_types: BindingDemand,
    javascript_shorthand: BindingDemand,
    typescript_callable_types: BindingDemand,
    typescript_index_signatures: BindingDemand,
    javascript_wrapped: BindingDemand,
    javascript_destructured: BindingDemand,
    typescript_function_types: BindingDemand,
    typescript_patterns: BindingDemand,
    typescript_method_types: BindingDemand,
}

impl EcmaScriptBindingDemand {
    fn reserve_late_bindings(
        &self,
        documents: &[AnalyzedFile],
        rust: [&BindingDemand; 4],
        python_comprehensions: &BindingDemand,
        capacity: (usize, u64),
        record: u64,
        established: &rift_syntax::PackageBindingSlots,
    ) -> rift_syntax::PackageBindingSlots {
        let [
            rust_modules,
            rust_shorthand,
            rust_macro_methods,
            rust_disjoint,
        ] = rust;
        let (mut slots, mut bytes) = capacity;
        let [
            callback_slots,
            overload_slots,
            rust_module_slots,
            rust_shorthand_slots,
            method_slots,
            signature_slots,
            scoped_slots,
            type_slots,
            shorthand_slots,
            callable_type_slots,
            index_signature_slots,
            wrapped_slots,
            destructured_slots,
            method_type_slots,
            function_type_slots,
            typed_pattern_slots,
            macro_method_slots,
            disjoint_slots,
        ] = reserve_scoped_parameters(
            documents,
            [
                &self.javascript_callbacks,
                &self.typescript_overloads,
                rust_modules,
                rust_shorthand,
                &self.javascript_methods,
                &self.typescript_signatures,
                &self.javascript_scoped_functions,
                &self.typescript_types,
                &self.javascript_shorthand,
                &self.typescript_callable_types,
                &self.typescript_index_signatures,
                &self.javascript_wrapped,
                &self.javascript_destructured,
                &self.typescript_method_types,
                &self.typescript_function_types,
                &self.typescript_patterns,
                rust_macro_methods,
                rust_disjoint,
            ],
            &mut slots,
            &mut bytes,
            record,
        );
        established
            .with_javascript_callback_parameters(callback_slots)
            .with_typescript_overload_parameters(overload_slots)
            .with_rust_module_sources(rust_module_slots)
            .with_rust_shorthand(rust_shorthand_slots)
            .with_javascript_method_parameters(method_slots)
            .with_typescript_signature_parameters(signature_slots)
            .with_javascript_scoped_function_parameters(scoped_slots)
            .with_typescript_type_parameters(type_slots)
            .with_javascript_shorthand_parameters(shorthand_slots)
            .with_typescript_callable_type_parameters(callable_type_slots)
            .with_typescript_index_signature_parameters(index_signature_slots)
            .with_javascript_wrapped_parameters(wrapped_slots)
            .with_javascript_destructured_parameters(destructured_slots)
            .with_typescript_method_type_parameters(method_type_slots)
            .with_typescript_function_type_parameters(function_type_slots)
            .with_typescript_destructured_parameters(typed_pattern_slots)
            .with_rust_macro_methods(macro_method_slots)
            .with_disjoint_rust_bindings(disjoint_slots)
            .with_python_comprehension_bindings(
                python_comprehensions.reserve(&mut slots, &mut bytes, record),
            )
    }

    fn classify(
        &mut self,
        symbol: &SyntaxSymbol,
        javascript: bool,
        methods: &BTreeSet<&str>,
    ) -> &mut BindingDemand {
        if symbol.is_ecmascript_callback_parameter() {
            &mut self.javascript_callbacks
        } else if symbol.is_ecmascript_overload_parameter() {
            &mut self.typescript_overloads
        } else if symbol.is_javascript_method_parameter() {
            &mut self.javascript_methods
        } else if symbol.is_typescript_signature_parameter() {
            &mut self.typescript_signatures
        } else if symbol.is_javascript_scoped_function_parameter() {
            &mut self.javascript_scoped_functions
        } else if symbol.is_typescript_type_parameter() {
            &mut self.typescript_types
        } else if symbol.is_javascript_shorthand_parameter() {
            &mut self.javascript_shorthand
        } else if symbol.is_typescript_callable_type_parameter() {
            &mut self.typescript_callable_types
        } else if symbol.is_typescript_index_signature_parameter() {
            &mut self.typescript_index_signatures
        } else if symbol.is_javascript_wrapped_parameter() {
            &mut self.javascript_wrapped
        } else if symbol.is_javascript_destructured_parameter() {
            &mut self.javascript_destructured
        } else if symbol.is_typescript_method_type_parameter() {
            &mut self.typescript_method_types
        } else if symbol.is_typescript_function_type_parameter() {
            &mut self.typescript_function_types
        } else if symbol.is_typescript_destructured_parameter() {
            &mut self.typescript_patterns
        } else if javascript {
            &mut self.javascript_functions
        } else if symbol
            .container
            .as_deref()
            .is_some_and(|owner| methods.contains(owner))
        {
            &mut self.typescript_methods
        } else {
            &mut self.typescript_functions
        }
    }

    fn count(&self) -> usize {
        self.typescript_functions.count
            + self.javascript_functions.count
            + self.typescript_methods.count
            + self.javascript_callbacks.count
            + self.typescript_overloads.count
            + self.javascript_methods.count
            + self.typescript_signatures.count
            + self.javascript_scoped_functions.count
            + self.typescript_types.count
            + self.javascript_shorthand.count
            + self.typescript_callable_types.count
            + self.typescript_index_signatures.count
            + self.javascript_wrapped.count
            + self.javascript_destructured.count
            + self.typescript_method_types.count
            + self.typescript_function_types.count
            + self.typescript_patterns.count
    }
}

fn binding_source_capacity(
    documents: &[AnalyzedFile],
    limits: ExactPackageLimits,
    count: usize,
) -> (u64, u64) {
    limits
        .retained_source()
        .filter(|limits| count != 0 && limits.total_bytes_max != u64::MAX)
        .map_or((u64::MAX, u64::MAX), |limits| {
            let existing = documents.iter().fold(0_u64, |total, file| {
                total.saturating_add(primary_retained_source_max(file, limits.record_bytes_max))
            });
            (
                limits.total_bytes_max.saturating_sub(existing),
                u64::from(limits.record_bytes_max),
            )
        })
}

/// Leaves room for established ECMAScript file declarations before scoped parameters.
fn reserve_scoped_parameters<const N: usize>(
    documents: &[AnalyzedFile],
    demands: [&BindingDemand; N],
    slots: &mut usize,
    bytes: &mut u64,
    record: u64,
) -> [usize; N] {
    if demands.iter().all(|demand| demand.count == 0) {
        return [0; N];
    }
    for file in documents {
        if let Some(source) = complete_module_bytes(file, record) {
            *slots = slots.saturating_sub(1);
            *bytes = bytes.saturating_sub(source);
        }
    }
    demands.map(|demand| demand.reserve(slots, bytes, record))
}

/// Candidate count and the longest retained name in one class of source-local binding.
#[derive(Default)]
struct BindingDemand {
    count: usize,
    largest: u64,
}

impl BindingDemand {
    fn observe(&mut self, range: ByteRange) {
        self.count += 1;
        self.largest = self.largest.max(range.end.saturating_sub(range.start));
    }

    fn module_sources(documents: &[AnalyzedFile]) -> Self {
        let mut demand = Self::default();
        for facet in documents
            .iter()
            .filter_map(|file| file.rust_module_source.as_ref()?.symbols().first())
        {
            demand.count += 1;
            demand.largest = demand.largest.max(facet.item_range.end);
        }
        demand
    }

    fn reserve(&self, slots: &mut usize, bytes: &mut u64, record: u64) -> usize {
        let each = self.largest.min(record).max(1);
        let fits = usize::try_from(*bytes / each).unwrap_or(usize::MAX);
        let count = self.count.min(*slots).min(fits);
        *slots -= count;
        *bytes -= u64::try_from(count)
            .unwrap_or(u64::MAX)
            .saturating_mul(each);
        count
    }
}

/// Upper bound on the original units, declarations and their search documents.
fn primary_retained_source_max(file: &AnalyzedFile, record_limit: u32) -> u64 {
    let maximum = u64::from(record_limit);
    let source = u64::try_from(file.file.source().len()).unwrap_or(u64::MAX);
    let mut bytes = source.min(maximum).saturating_mul(2);
    if documentation_format(file.file.path().as_str()) == Some(DocumentationSourceFormat::Notebook)
    {
        bytes = bytes.saturating_add(source);
    }
    for range in file
        .file
        .syntax()
        .symbols()
        .iter()
        .filter(|symbol| !symbol.is_package_binding())
        .map(|symbol| symbol.item_range)
        .chain(file.placement.aliases().iter().map(|alias| alias.range))
    {
        let record = range.end.saturating_sub(range.start).min(maximum);
        bytes = bytes.saturating_add(record.saturating_mul(2));
    }
    bytes
}

pub(crate) fn edge_kind(kind: rift_core::EdgeKind) -> rift_protocol::read::EdgeKind {
    match kind {
        rift_core::EdgeKind::Reference => rift_protocol::read::EdgeKind::References,
        rift_core::EdgeKind::Definition => rift_protocol::read::EdgeKind::Declares,
        rift_core::EdgeKind::Implementation => rift_protocol::read::EdgeKind::Implements,
        rift_core::EdgeKind::TypeDefinition => rift_protocol::read::EdgeKind::HasType,
        rift_core::EdgeKind::Alias => rift_protocol::read::EdgeKind::Aliases,
    }
}

fn place_namespace(
    input: &ExactPackageInput<'_>,
    analyzed: &mut [AnalyzedFile],
) -> Result<namespace::PreparedNamespace, RiftError> {
    let package = input.owner();
    let selected = analyzed
        .iter()
        .map(|held| namespace::SelectedFile {
            path: held.file.path(),
            source: held.file.source(),
            syntax: held.file.syntax(),
        })
        .collect::<Vec<_>>();
    let mut namespace = namespace::prepare(input, &selected);
    let placements = analyzed
        .iter()
        .map(|held| &held.placement)
        .collect::<Vec<_>>();
    let mut aliases = namespace::placed_aliases(
        &input.namespace_input(),
        &mut namespace,
        &selected,
        &placements,
        input.limits().declarations_max(),
        || {
            errors::analysis::package_provider_failed()
                .package(package_label(package))
                .error()
        },
    )?;
    let exposed = namespace.exposed_names();
    for held in analyzed.iter_mut() {
        held.public_names.extend(
            namespace
                .public_names
                .remove(held.file.path().as_str())
                .unwrap_or_default(),
        );
        let path = held.file.path().clone();
        if namespace.unreached_modules.contains_key(path.as_str()) {
            held.retain_exposed(exposed.get(path.as_str()));
        }
        let mut assigned = namespace
            .anchors
            .remove(held.file.path().as_str())
            .unwrap_or_default();
        if let Some((path, root)) = namespace.rust_package_root.as_ref()
            && path == held.file.path().as_str()
        {
            assigned.insert(".".to_owned(), root.clone());
            place_rust_library_root(held, root);
        }
        if namespace.rust_module_sources.remove(path.as_str())
            && let Some(module) = namespace.rust_modules.get(path.as_str())
        {
            place_rust_module_source(held, module);
        }
        if let Some(methods) = namespace.rust_macro_methods.remove(path.as_str()) {
            held.item_kinds.extend(item_kinds(&methods));
            let facts = held
                .placed_syntax
                .take()
                .map_or_else(|| held.file.syntax().clone(), |facts| *facts);
            held.placed_syntax = Some(Box::new(facts.with_rust_macro_methods(methods)));
        }
        held.shared_rust_placements = namespace
            .shared_rust_modules
            .remove(held.file.path().as_str())
            .into_iter()
            .flatten()
            .map(|scope| scope.placement(&held.placement))
            .collect();
        held.placement = held
            .placement
            .clone()
            .with_identity_anchors(assigned)
            .with_module(namespace.rust_modules.remove(held.file.path().as_str()))
            .with_logical_declarations(
                namespace
                    .mappings
                    .remove(held.file.path().as_str())
                    .unwrap_or_default(),
            )
            .with_aliases(
                aliases
                    .remove(held.file.path().as_str())
                    .unwrap_or_default(),
            );
    }

    place_scoped_anchors(analyzed, &mut namespace, input.limits());
    Ok(namespace)
}

/// Optional physical anchors reuse captured source, but each new declaration retains a
/// bounded copy. Leave the earlier unresolved rows when the existing sources may need
/// the caller's full budget.
fn place_scoped_anchors(
    analyzed: &mut [AnalyzedFile],
    namespace: &mut namespace::PreparedNamespace,
    limits: ExactPackageLimits,
) {
    if namespace.scoped_anchors.is_empty() {
        return;
    }
    let reserve_fits = limits.retained_source().is_none_or(|limit| {
        limit.total_bytes_max == u64::MAX
            || !analyzed.iter().any(|held| {
                namespace
                    .scoped_anchors
                    .get(held.file.path().as_str())
                    .is_some_and(|anchors| {
                        anchors
                            .keys()
                            .any(|name| held.placement.logical_identity(name).is_none())
                    })
            })
            || analyzed.iter().fold(0_u64, |total, held| {
                total.saturating_add(scoped_anchor_source_max(held, limit.record_bytes_max))
            }) <= limit.total_bytes_max
    });
    for held in analyzed {
        let Some(mut anchors) = namespace.scoped_anchors.remove(held.file.path().as_str()) else {
            continue;
        };
        if !reserve_fits {
            anchors.retain(|name, _| held.placement.logical_identity(name).is_some());
        }
        held.placement = held.placement.clone().with_scoped_identity_anchors(anchors);
    }
}

/// Existing package bindings and complete file modules can also retain source.
fn scoped_anchor_source_max(file: &AnalyzedFile, record_limit: u32) -> u64 {
    let primary = primary_retained_source_max(file, record_limit);
    let syntax = file.file.syntax();
    let maximum = u64::from(record_limit);
    let bindings = syntax
        .symbols()
        .iter()
        .filter(|symbol| symbol.is_package_binding())
        .map(|symbol| {
            symbol
                .item_range
                .end
                .saturating_sub(symbol.item_range.start)
                .min(maximum)
        })
        .fold(0, u64::saturating_add);
    primary
        .saturating_add(bindings)
        .saturating_add(complete_module_bytes(file, maximum).unwrap_or(0))
}

fn complete_module_bytes(file: &AnalyzedFile, maximum: u64) -> Option<u64> {
    let syntax = file.file.syntax();
    let source = u64::try_from(file.file.source().len()).unwrap_or(u64::MAX);
    (matches!(syntax.language().name.as_str(), "javascript" | "typescript")
        && file.placement.logical_identity(".").is_some()
        && !syntax.has_errors()
        && syntax.left_out_declaration_count() == 0
        && syntax.primary_symbols_len() > 0
        && source <= maximum)
        .then_some(source)
}

fn place_rust_library_root(held: &mut AnalyzedFile, root: &CoreSymbolId) {
    let Ok(identity) = SymbolIdentity::parse(root.as_str()) else {
        return;
    };
    let [name] = identity.qualified_path() else {
        return;
    };
    let source = held.file.source();
    let Some(syntax) = held
        .file
        .syntax()
        .clone()
        .with_rust_library_root(name, source)
    else {
        return;
    };
    held.placed_syntax = Some(Box::new(syntax));
}

fn place_rust_module_source(held: &mut AnalyzedFile, module: &CoreSymbolId) {
    let Ok(identity) = SymbolIdentity::parse(module.as_str()) else {
        return;
    };
    let Some(name) = identity.qualified_path().last() else {
        return;
    };
    held.rust_module_source =
        held.file
            .syntax()
            .rust_module_source(name, held.file.path(), held.file.source());
}

/// Renders the canonical publication over the analyzed files.
///
/// One pass over the files emits every record: a file's unit, the declarations it
/// carries, and the documents a ranking reads. Each collection is sorted by its own
/// identity afterwards, so the publication's order is the identities' order and not the
/// order the files arrived in.
fn publish(
    input: &ExactPackageInput<'_>,
    analyzed: &[AnalyzedFile],
    semantics: &WorkspaceSemantics,
    unresolved_exports: bool,
    local_declaration_limit: Option<usize>,
) -> Result<
    (
        PackagePublication,
        BTreeMap<DocumentationContentIdentity, String>,
    ),
    RiftError,
> {
    let package = input.owner();
    let package_language = input.language();
    let limits = input.limits();
    let origin = symbol_origin(package, input.origin())?;
    let mut records = Records {
        retained_source: limits.retained_source(),
        publication: limits.publication(),
        documentation: limits.documentation(),
        ..Records::default()
    };
    for held in analyzed {
        if records.units.len() >= bound(records.publication.units) {
            records.warn(truncation("units", u64::from(records.publication.units)));
            break;
        }
        records.file(package, held)?;
    }
    records.graph(package, &origin, semantics, analyzed)?;
    let retained_syntax_complete = records.retained_syntax(analyzed);
    records.edges(package, semantics, analyzed, limits.edges_max())?;
    records.static_occurrences(semantics, analyzed, limits.edges_max());
    let (documentation, notebook_cells) = package_documentation(
        package,
        package_language,
        &origin,
        analyzed,
        semantics,
        &mut records,
    )?;
    records.retain_ecmascript_modules(package, analyzed, semantics, limits)?;
    records
        .units
        .sort_by(|left, right| left.unit.0.cmp(&right.unit.0));
    records.declarations.sort_by(|left, right| {
        (
            &left.symbol.0,
            &left.unit.0,
            left.range.start,
            left.range.end,
        )
            .cmp(&(
                &right.symbol.0,
                &right.unit.0,
                right.range.start,
                right.range.end,
            ))
    });
    records
        .objects
        .sort_by(|left, right| left.id.cmp(&right.id));
    records
        .documents
        .sort_by(|left, right| (left.kind, &left.identity).cmp(&(right.kind, &right.identity)));
    let source_digest = digest_of(
        &SourceFingerprint {
            units: records
                .units
                .iter()
                .map(|unit| (unit.path.0.as_str(), unit.content_digest.0.as_str()))
                .collect(),
        },
        package,
    )?;
    if let Some(limit) = local_declaration_limit {
        records.symbols_truncated = true;
        records.warn(truncation(
            "local_declarations",
            u64::try_from(limit).unwrap_or(u64::MAX),
        ));
    }
    let coverage = publication_coverage(
        analyzed,
        &records,
        unresolved_exports,
        retained_syntax_complete,
    );
    Ok((
        PackagePublication {
            occurrences: records.occurrences,
            format_revision: PACKAGE_PUBLICATION_FORMAT_REVISION,
            identity_format: rift_protocol::identity::SYMBOL_IDENTITY_FORMAT_REVISION,
            analyzer_revision: analyzer_revision(),
            owner: package.clone(),
            revision: input.release()?,
            source_digest,
            units: records.units,
            coverage,
            objects: records.objects,
            declarations: records.declarations,
            edges: records.edges,
            artifact: input.artifact().cloned(),
            documents: records.documents,
            documentation,
            warnings: records.warnings,
        },
        notebook_cells,
    ))
}

fn stub_document_signature<'graph>(
    semantics: &'graph WorkspaceSemantics,
    contributions: &[rift_core::ContributionKey],
    files: &BTreeMap<String, (&AnalyzedFile, Vec<usize>)>,
    admitted: &BTreeSet<String>,
) -> Option<&'graph str> {
    contributions
        .iter()
        .filter_map(|key| {
            let contribution = semantics.graph().contribution(key)?;
            let binding = contribution.source()?;
            let unit = binding.unit().to_string();
            let (held, _) = files.get(&unit).filter(|_| admitted.contains(&unit))?;
            if !matches!(held.role, ModuleRole::Stub) {
                return None;
            }
            let signature = contribution.facts()?.signatures_slice().first()?;
            Some((unit, binding.range().start(), signature.display.as_str()))
        })
        .min_by(|left, right| (&left.0, left.1).cmp(&(&right.0, right.1)))
        .map(|(_, _, signature)| signature)
}

fn retained_declaration_ranges(source: &str, symbol: &rift_syntax::SyntaxSymbol) -> bool {
    let valid = |range: rift_syntax::ByteRange| {
        usize::try_from(range.start)
            .ok()
            .zip(usize::try_from(range.end).ok())
            .is_some_and(|(start, end)| {
                start <= end
                    && end <= source.len()
                    && source.is_char_boundary(start)
                    && source.is_char_boundary(end)
            })
    };
    valid(symbol.range)
        && std::iter::once(symbol.item_range)
            .chain(symbol.name_range)
            .chain(symbol.body_range)
            .chain(symbol.documentation_ranges.iter().copied())
            .all(|range| {
                valid(range) && symbol.range.start <= range.start && range.end <= symbol.range.end
            })
}

fn publication_coverage(
    analyzed: &[AnalyzedFile],
    records: &Records,
    unresolved_exports: bool,
    retained_syntax_complete: bool,
) -> Vec<PackageCoverage> {
    let languages = analyzed
        .iter()
        .map(|held| {
            let language = held.file.syntax().language().clone();
            ((language.name.clone(), language.dialect.clone()), language)
        })
        .collect::<BTreeMap<_, _>>();
    let units = records
        .units
        .iter()
        .map(|unit| (&unit.unit.0, unit.source_complete))
        .collect::<BTreeMap<_, _>>();
    languages
        .into_values()
        .map(|language| {
            let selected = analyzed
                .iter()
                .filter(|held| held.file.syntax().language() == &language);
            let mut inventory_complete = true;
            let mut syntax_complete = true;
            for held in selected {
                inventory_complete &= units.get(&held.placement.unit().to_string()) == Some(&true);
                inventory_complete &= held.file.syntax().left_out_declaration_count() == 0
                    || retained_syntax_complete;
                syntax_complete &= held.file.syntax().declaration_syntax_complete()
                    && held.file.syntax().left_out_declaration_count() == 0;
            }
            let mut facts = vec![FactCoverage {
                kind: FactKind::Edges,
                producer: None,
            }];
            if records
                .occurrence_languages
                .contains(&language.identity_segment())
            {
                facts.push(FactCoverage {
                    kind: FactKind::Occurrences,
                    producer: None,
                });
            }
            PackageCoverage {
                facts,
                inventory_complete,
                identity_complete: !records.symbols_truncated
                    && !records.identity_incomplete
                    && records
                        .objects
                        .iter()
                        .filter(|object| object.language == language)
                        .all(|object| object.id.is_some())
                    && syntax_complete
                    && !unresolved_exports,
                language,
                applicability_complete: false,
                edges: Vec::new(),
            }
        })
        .collect::<Vec<_>>()
}

fn package_documentation(
    package: &SymbolOwner,
    package_language: &Language,
    origin: &SymbolOrigin,
    analyzed: &[AnalyzedFile],
    semantics: &WorkspaceSemantics,
    records: &mut Records,
) -> Result<
    (
        rift_protocol::documentation::DocumentationIndex,
        BTreeMap<DocumentationContentIdentity, String>,
    ),
    RiftError,
> {
    let DecodedPackageNotebooks {
        notebooks,
        omissions,
    } = decode_package_notebooks(analyzed, &records.documentation).map_err(|error| {
        errors::analysis::package_provider_failed()
            .package(package_label(package))
            .cause(error)
            .error()
    })?;
    let mut inputs = PackageDocumentationInputs {
        inputs: Vec::new(),
        omissions,
        input_bytes: 0,
        limits: records.documentation,
        retained_bytes_max: u64::from(
            records
                .retained_source
                .map_or(PACKAGE_SOURCE_BYTES_MAX, |limits| limits.record_bytes_max),
        ),
    };
    append_notebook_inputs(
        package,
        package_language,
        origin,
        analyzed,
        &notebooks,
        records,
        &mut inputs,
    )?;
    append_regular_inputs(package, origin, analyzed, &mut inputs)?;
    append_attached_comment_inputs(analyzed, origin, package, &mut inputs)?;
    let sources =
        DocumentationSourceSet::with_limits(inputs.inputs, &inputs.limits).map_err(|error| {
            errors::analysis::package_provider_failed()
                .package(package_label(package))
                .cause(error)
                .error()
        })?;
    let declarations = package_declarations(package, semantics, records)?;
    let notebook_cells = package_notebook_cells(analyzed, &notebooks);
    let collection = collect_documentation(&sources, &declarations)
        .map_err(|error| {
            errors::analysis::package_provider_failed()
                .package(package_label(package))
                .cause(error)
                .error()
        })?
        .with_source_omissions(inputs.omissions)
        .map_err(|error| {
            errors::analysis::package_provider_failed()
                .package(package_label(package))
                .cause(error)
                .error()
        })?;
    Ok((collection.into_index(), notebook_cells))
}

struct PackageDocumentationInputs<'source> {
    inputs: Vec<DocumentationInput<'source>>,
    omissions: Vec<(DocumentationContentIdentity, DocumentationWarningKind)>,
    input_bytes: u64,
    limits: crate::documentation::DocumentationLimits,
    retained_bytes_max: u64,
}

impl PackageDocumentationInputs<'_> {
    fn ensure_next(&self) -> Result<(), RiftError> {
        self.limits.check_source_count(
            self.inputs
                .len()
                .saturating_add(self.omissions.len())
                .saturating_add(1),
        )
    }

    fn omit(
        &mut self,
        identity: DocumentationContentIdentity,
        kind: DocumentationWarningKind,
    ) -> Result<(), RiftError> {
        self.ensure_next()?;
        self.omissions.push((identity, kind));
        Ok(())
    }
}

fn append_notebook_inputs<'source>(
    package: &SymbolOwner,
    package_language: &Language,
    origin: &SymbolOrigin,
    analyzed: &[AnalyzedFile],
    notebooks: &'source [Option<crate::documentation::notebook::NotebookContent>],
    records: &mut Records,
    inputs: &mut PackageDocumentationInputs<'source>,
) -> Result<(), RiftError> {
    for (held, notebook) in analyzed.iter().zip(notebooks) {
        let Some(notebook) = notebook else {
            continue;
        };
        for cell in notebook.cells() {
            append_notebook_cell(
                package,
                package_language,
                origin,
                held,
                cell,
                records,
                inputs,
            )?;
        }
    }
    Ok(())
}

fn append_notebook_cell<'source>(
    package: &SymbolOwner,
    package_language: &Language,
    origin: &SymbolOrigin,
    held: &AnalyzedFile,
    cell: &'source crate::documentation::notebook::NotebookCellContent,
    records: &mut Records,
    inputs: &mut PackageDocumentationInputs<'source>,
) -> Result<(), RiftError> {
    inputs.ensure_next().map_err(|error| {
        errors::analysis::package_provider_failed()
            .package(package_label(package))
            .cause(error)
            .error()
    })?;
    let unit = wire_unit(held.placement.unit());
    let identity = DocumentationContentIdentity {
        source: DocumentationSourceIdentity::Package { unit: unit.clone() },
        cell: Some(cell.cell().clone()),
    };
    let source_bytes = u64::try_from(cell.text().len()).unwrap_or(u64::MAX);
    if source_bytes > inputs.limits.source_bytes_max()
        || inputs.input_bytes.saturating_add(source_bytes) > inputs.limits.total_bytes_max()
    {
        inputs
            .omit(identity, DocumentationWarningKind::SourceUnavailable)
            .map_err(|error| {
                errors::analysis::package_provider_failed()
                    .package(package_label(package))
                    .cause(error)
                    .error()
            })?;
        return Ok(());
    }
    let source = documentation_source(
        identity.clone(),
        cell.text(),
        origin,
        DocumentationSourceFormat::Notebook,
        cell.declared_language().cloned(),
        cell.physical_ranges().to_vec(),
    );
    let mut input = match DocumentationInput::with_limits(source, cell.text(), &inputs.limits) {
        Ok(input) => input,
        Err(error) => {
            inputs
                .omit(identity, documentation_warning_kind(&error))
                .map_err(|error| {
                    errors::analysis::package_provider_failed()
                        .package(package_label(package))
                        .cause(error)
                        .error()
                })?;
            return Ok(());
        }
    };
    let document_id = match content_chunk_identity(&identity, 0) {
        Ok(document_id) => document_id,
        Err(error) => {
            inputs
                .omit(identity, documentation_warning_kind(&error))
                .map_err(|error| {
                    errors::analysis::package_provider_failed()
                        .package(package_label(package))
                        .cause(error)
                        .error()
                })?;
            return Ok(());
        }
    };
    if source_bytes <= inputs.retained_bytes_max && !cell.text().is_empty() {
        input = match input.with_chunks(vec![DocumentationChunk {
            identity: document_id.clone(),
            range: TextRange {
                start: 0,
                end: u64::try_from(cell.text().len()).unwrap_or(u64::MAX),
            },
        }]) {
            Ok(input) => input,
            Err(error) => {
                inputs
                    .omit(identity, documentation_warning_kind(&error))
                    .map_err(|error| {
                        errors::analysis::package_provider_failed()
                            .package(package_label(package))
                            .cause(error)
                            .error()
                    })?;
                return Ok(());
            }
        };
    }
    inputs.inputs.push(input);
    inputs.input_bytes = inputs.input_bytes.saturating_add(source_bytes);
    add_notebook_document(
        package,
        package_language,
        held,
        cell,
        &unit,
        document_id,
        records,
    )
}

fn add_notebook_document(
    package: &SymbolOwner,
    package_language: &Language,
    held: &AnalyzedFile,
    cell: &crate::documentation::notebook::NotebookCellContent,
    unit: &SourceUnitId,
    document_id: String,
    records: &mut Records,
) -> Result<(), RiftError> {
    if !records.document_capacity() {
        records.warn_document_full();
        return Ok(());
    }
    let language = match cell.cell().kind {
        NotebookCellKind::Markdown => ShippedLanguage::Markdown.language(),
        NotebookCellKind::Code => cell
            .declared_language()
            .cloned()
            .unwrap_or_else(|| package_language.clone()),
    };
    let path = wire_path(held.file.path());
    let retained = records.retained(cell.text(), &path, 1)?;
    let name = file_name(&path);
    records.document(PackageDocument {
        identity: document_id,
        kind: PackageDocumentKind::File,
        unit: unit.clone(),
        language,
        owner: package.clone(),
        content_digest: text_digest(&retained.text),
        identifier_terms: identifier_terms_with_limit(
            &[&name],
            records.publication.identifier_terms,
        ),
        name,
        qualified_name: None,
        signature: None,
        documentation: None,
        declaration_source: None,
        file_content: Some(retained.text),
        digest: Digest(String::new()),
    })
}

fn append_regular_inputs<'source>(
    package: &SymbolOwner,
    origin: &SymbolOrigin,
    analyzed: &'source [AnalyzedFile],
    inputs: &mut PackageDocumentationInputs<'source>,
) -> Result<(), RiftError> {
    for held in analyzed {
        let Some(format) = documentation_format(held.file.path().as_str()) else {
            continue;
        };
        if format == DocumentationSourceFormat::Notebook {
            continue;
        }
        inputs.ensure_next().map_err(|error| {
            errors::analysis::package_provider_failed()
                .package(package_label(package))
                .cause(error)
                .error()
        })?;
        let identity = DocumentationContentIdentity {
            source: DocumentationSourceIdentity::Package {
                unit: wire_unit(held.placement.unit()),
            },
            cell: None,
        };
        let source_bytes = u64::try_from(held.file.source().len()).unwrap_or(u64::MAX);
        if source_bytes > inputs.limits.source_bytes_max()
            || inputs.input_bytes.saturating_add(source_bytes) > inputs.limits.total_bytes_max()
        {
            inputs
                .omit(identity, DocumentationWarningKind::SourceUnavailable)
                .map_err(|error| {
                    errors::analysis::package_provider_failed()
                        .package(package_label(package))
                        .cause(error)
                        .error()
                })?;
            continue;
        }
        match package_file_input(
            origin,
            held,
            format,
            &inputs.limits,
            inputs.retained_bytes_max,
        ) {
            Ok(input) => {
                inputs.inputs.push(input);
                inputs.input_bytes = inputs.input_bytes.saturating_add(source_bytes);
            }
            Err(error) => inputs
                .omit(identity, documentation_warning_kind(&error))
                .map_err(|error| {
                    errors::analysis::package_provider_failed()
                        .package(package_label(package))
                        .cause(error)
                        .error()
                })?,
        }
    }
    Ok(())
}

fn package_file_input<'source>(
    origin: &SymbolOrigin,
    held: &'source AnalyzedFile,
    format: DocumentationSourceFormat,
    limits: &crate::documentation::DocumentationLimits,
    retained_bytes_max: u64,
) -> Result<DocumentationInput<'source>, RiftError> {
    let unit = wire_unit(held.placement.unit());
    let identity = DocumentationContentIdentity {
        source: DocumentationSourceIdentity::Package { unit: unit.clone() },
        cell: None,
    };
    let text = held.file.source();
    let source = documentation_source(identity, text, origin, format, None, Vec::new());
    let mut input = DocumentationInput::with_limits(source, text, limits)?;
    if matches!(
        format,
        DocumentationSourceFormat::Markdown | DocumentationSourceFormat::Mdx
    ) {
        input = input.with_indexed_syntax(held.file.path(), held.file.syntax_facts())?;
    }
    if u64::try_from(text.len()).unwrap_or(u64::MAX) <= retained_bytes_max && !text.is_empty() {
        input = input.with_chunks(vec![DocumentationChunk {
            identity: unit.0,
            range: TextRange {
                start: 0,
                end: u64::try_from(text.len()).unwrap_or(u64::MAX),
            },
        }])?;
    }
    Ok(input)
}

struct DecodedPackageNotebooks {
    notebooks: Vec<Option<crate::documentation::notebook::NotebookContent>>,
    omissions: Vec<(DocumentationContentIdentity, DocumentationWarningKind)>,
}

fn decode_package_notebooks(
    analyzed: &[AnalyzedFile],
    limits: &crate::documentation::DocumentationLimits,
) -> Result<DecodedPackageNotebooks, RiftError> {
    let mut notebooks = Vec::with_capacity(analyzed.len());
    let mut omissions = Vec::new();
    let mut selected = 0_usize;
    for held in analyzed {
        if documentation_format(held.file.path().as_str())
            != Some(DocumentationSourceFormat::Notebook)
        {
            notebooks.push(None);
            continue;
        }
        let identity = DocumentationContentIdentity {
            source: DocumentationSourceIdentity::Package {
                unit: wire_unit(held.placement.unit()),
            },
            cell: None,
        };
        match crate::documentation::notebook::decode_notebook_with_limits(
            held.file.source(),
            &identity,
            limits,
        ) {
            Ok(notebook) => {
                selected = selected.saturating_add(notebook.cells().len());
                limits.check_source_count(selected)?;
                notebooks.push(Some(notebook));
            }
            Err(error) => {
                selected = selected.saturating_add(1);
                limits.check_source_count(selected)?;
                omissions.push((identity, documentation_warning_kind(&error)));
                notebooks.push(None);
            }
        }
    }
    Ok(DecodedPackageNotebooks {
        notebooks,
        omissions,
    })
}

fn append_attached_comment_inputs<'source>(
    analyzed: &'source [AnalyzedFile],
    origin: &SymbolOrigin,
    package: &SymbolOwner,
    inputs: &mut PackageDocumentationInputs<'source>,
) -> Result<(), RiftError> {
    for held in analyzed {
        if !held
            .file
            .syntax()
            .symbols()
            .iter()
            .any(|symbol| !symbol.documentation_ranges.is_empty())
        {
            continue;
        }
        inputs.ensure_next().map_err(|error| {
            errors::analysis::package_provider_failed()
                .package(package_label(package))
                .cause(error)
                .error()
        })?;
        append_attached_comment(held, origin, package, inputs)?;
    }
    Ok(())
}

fn append_attached_comment<'source>(
    held: &'source AnalyzedFile,
    origin: &SymbolOrigin,
    package: &SymbolOwner,
    inputs: &mut PackageDocumentationInputs<'source>,
) -> Result<(), RiftError> {
    let source_text = held.file.source();
    let unit = wire_unit(held.placement.unit());
    let identity = DocumentationContentIdentity {
        source: DocumentationSourceIdentity::Package { unit: unit.clone() },
        cell: None,
    };
    let source_bytes = u64::try_from(source_text.len()).unwrap_or(u64::MAX);
    if source_bytes > inputs.limits.source_bytes_max()
        || inputs.input_bytes.saturating_add(source_bytes) > inputs.limits.total_bytes_max()
    {
        inputs
            .omit(identity, DocumentationWarningKind::SourceUnavailable)
            .map_err(|error| {
                errors::analysis::package_provider_failed()
                    .package(package_label(package))
                    .cause(error)
                    .error()
            })?;
        return Ok(());
    }
    let source = documentation_source(
        identity.clone(),
        source_text,
        origin,
        DocumentationSourceFormat::AttachedComment,
        Some(held.file.syntax().language().clone()),
        Vec::new(),
    );
    let mut input = match DocumentationInput::with_limits(source, source_text, &inputs.limits)
        .and_then(|input| input.with_indexed_syntax(held.file.path(), held.file.syntax_facts()))
    {
        Ok(input) => input,
        Err(error) => {
            inputs
                .omit(identity, documentation_warning_kind(&error))
                .map_err(|error| {
                    errors::analysis::package_provider_failed()
                        .package(package_label(package))
                        .cause(error)
                        .error()
                })?;
            return Ok(());
        }
    };
    if source_bytes <= inputs.retained_bytes_max && !source_text.is_empty() {
        input = match input.with_chunks(vec![DocumentationChunk {
            identity: unit.0,
            range: TextRange {
                start: 0,
                end: u64::try_from(source_text.len()).unwrap_or(u64::MAX),
            },
        }]) {
            Ok(input) => input,
            Err(error) => {
                inputs
                    .omit(identity, documentation_warning_kind(&error))
                    .map_err(|error| {
                        errors::analysis::package_provider_failed()
                            .package(package_label(package))
                            .cause(error)
                            .error()
                    })?;
                return Ok(());
            }
        };
    }
    inputs.inputs.push(input);
    inputs.input_bytes = inputs.input_bytes.saturating_add(source_bytes);
    Ok(())
}

fn package_declarations<'declaration>(
    owner: &SymbolOwner,
    semantics: &'declaration WorkspaceSemantics,
    records: &'declaration Records,
) -> Result<Vec<DocumentationDeclaration<'declaration>>, RiftError> {
    let mut bindings = BTreeMap::new();
    for record in semantics.graph().records() {
        let Some(id) = record.identity() else {
            continue;
        };
        for key in record.contributions() {
            let Some(contribution) = semantics.graph().contribution(key) else {
                continue;
            };
            let (Some(binding), Some(facts)) = (contribution.source(), contribution.facts()) else {
                continue;
            };
            bindings.insert(
                (
                    id.as_str(),
                    binding.unit().to_string(),
                    binding.range().start(),
                    binding.range().end(),
                ),
                facts,
            );
        }
    }
    let mut declarations = Vec::with_capacity(records.declarations.len());
    for declaration in &records.declarations {
        let facts = bindings
            .get(&(
                declaration.symbol.as_str(),
                declaration.unit.0.clone(),
                declaration.range.start,
                declaration.range.end,
            ))
            .ok_or_else(|| {
                errors::analysis::package_provider_failed()
                    .package(package_label(owner))
                    .error()
            })?;
        let identity = DocumentationContentIdentity {
            source: DocumentationSourceIdentity::Package {
                unit: declaration.unit.clone(),
            },
            cell: None,
        };
        declarations.push(
            DocumentationDeclaration::new(
                &declaration.symbol,
                facts.language(),
                facts.name(),
                facts.qualified_name(),
                &identity,
                declaration.range.clone(),
            )
            .map_err(|error| {
                errors::analysis::package_provider_failed()
                    .package(package_label(owner))
                    .cause(error)
                    .error()
            })?,
        );
    }
    Ok(declarations)
}

fn package_notebook_cells(
    analyzed: &[AnalyzedFile],
    notebooks: &[Option<crate::documentation::notebook::NotebookContent>],
) -> BTreeMap<DocumentationContentIdentity, String> {
    let mut cells = BTreeMap::new();
    for (held, notebook) in analyzed.iter().zip(notebooks) {
        let Some(notebook) = notebook else {
            continue;
        };
        let owner = DocumentationSourceIdentity::Package {
            unit: wire_unit(held.placement.unit()),
        };
        for cell in notebook.cells() {
            cells.insert(
                DocumentationContentIdentity {
                    source: owner.clone(),
                    cell: Some(cell.cell().clone()),
                },
                cell.text().to_owned(),
            );
        }
    }
    cells
}

fn documentation_source(
    identity: DocumentationContentIdentity,
    text: &str,
    origin: &SymbolOrigin,
    format: DocumentationSourceFormat,
    language: Option<Language>,
    physical_ranges: Vec<TextRange>,
) -> DocumentationSource {
    let media_type = match format {
        DocumentationSourceFormat::Markdown | DocumentationSourceFormat::AttachedComment => {
            "text/markdown"
        }
        DocumentationSourceFormat::Mdx => "text/mdx",
        DocumentationSourceFormat::RestructuredText => "text/x-rst",
        DocumentationSourceFormat::Text => "text/plain",
        DocumentationSourceFormat::Notebook => "application/x-ipynb+json",
    };
    let digest = content_digest(text.as_bytes());
    DocumentationSource {
        identity,
        revision: digest.clone(),
        content_digest: digest,
        origin: origin.clone(),
        format,
        media_type: media_type.to_owned(),
        selection: if format == DocumentationSourceFormat::AttachedComment {
            DocumentationSelectionReason::AttachedComment
        } else {
            DocumentationSelectionReason::PackageArchive
        },
        byte_length: u64::try_from(text.len()).unwrap_or(u64::MAX),
        language,
        physical_ranges,
        license: None,
    }
}

fn documentation_warning_kind(error: &RiftError) -> DocumentationWarningKind {
    if error.slug() == errors::analysis::documentation_limit_exceeded::SLUG {
        DocumentationWarningKind::SourceUnavailable
    } else {
        DocumentationWarningKind::MalformedSource
    }
}

/// The bytes a publication's `source_digest` covers: every unit's path and content digest,
/// in unit order.
#[derive(Serialize)]
struct SourceFingerprint<'analysis> {
    units: Vec<(&'analysis str, &'analysis str)>,
}

/// The records one publication is assembled from, and what the assembly could not do.
///
/// A collection that reaches its bound reports the cut once. Every record past the bound
/// meets the same full collection, so warning per record would fill the warning list with
/// one repeated entry and drop everything else the analysis met.
#[derive(Default)]
struct Records {
    units: Vec<PackageSourceUnit>,
    objects: Vec<Symbol>,
    declarations: Vec<PackageSymbol>,
    edges: Vec<rift_protocol::index::StoredEdge>,
    occurrences: Vec<PackageOccurrence>,
    occurrence_languages: BTreeSet<String>,
    documents: Vec<PackageDocument>,
    warnings: Vec<PackageAnalysisWarning>,
    symbols_truncated: bool,
    identity_incomplete: bool,
    symbol_documents: BTreeSet<String>,
    documents_truncated: bool,
    retained_source: Option<RetainedSourceLimits>,
    retained_source_bytes: u64,
    publication: rift_protocol::configuration::PackageConfiguration,
    documentation: crate::documentation::DocumentationLimits,
}

fn javascript_module_source(owner: &SymbolOwner, held: &AnalyzedFile) -> bool {
    matches!(owner, SymbolOwner::Package { manager, .. } if manager == "npm")
        && held.file.syntax().static_facts().is_some_and(|facts| {
            let suffix = held.file.path().as_str().rsplit_once('.');
            matches!(suffix, Some((_, "mjs" | "cjs")))
                || facts.has_javascript_module_statement()
                || facts.commonjs_export().is_some()
        })
}

fn javascript_declaration_scopes(syntax: &SyntaxFacts) -> Option<crate::EnclosingDefinitions<()>> {
    let expressions = syntax
        .static_facts()?
        .function_values()
        .iter()
        .filter(|(range, value)| range == value)
        .map(|(range, _)| *range);
    // The module fallback also covers missing owners inside authored declarations.
    let declarations = syntax
        .symbols()
        .iter()
        .filter(|symbol| matches!(symbol.kind, "function" | "method" | "class"))
        .map(|symbol| symbol.range);
    Some(crate::EnclosingDefinitions::new(
        expressions
            .chain(declarations)
            .map(|range| (range.start, range.end, ())),
    ))
}

impl Records {
    fn static_occurrences(
        &mut self,
        semantics: &WorkspaceSemantics,
        analyzed: &[AnalyzedFile],
        limit: usize,
    ) {
        let sources = self
            .units
            .iter()
            .map(|unit| (unit.unit.as_str(), (unit.source.as_str(), &unit.language)))
            .collect::<BTreeMap<_, _>>();
        let digests = analyzed
            .iter()
            .map(|held| (held.placement.unit(), held.file.digest()))
            .collect::<BTreeMap<_, _>>();
        let mut occurrences = BTreeSet::new();
        for occurrence in semantics.static_occurrences() {
            let rift_provider::NormalizedTarget::Symbol(target) = &occurrence.target else {
                continue;
            };
            let unit = occurrence.binding.unit().to_string();
            if digests.get(occurrence.binding.unit()) != Some(&occurrence.source_digest) {
                continue;
            }
            let range = occurrence.callee_range;
            let Some((source, language)) = sources.get(unit.as_str()) else {
                continue;
            };
            let bytes = usize::try_from(range.start)
                .ok()
                .zip(usize::try_from(range.end).ok())
                .and_then(|(start, end)| source.get(start..end));
            if bytes.is_none() {
                continue;
            }
            let key = (unit, range.start, range.end, target.as_str().to_owned());
            if occurrences.len() == limit && !occurrences.contains(&key) {
                break;
            }
            self.occurrence_languages
                .insert(language.identity_segment());
            occurrences.insert(key);
        }
        self.occurrences = occurrences
            .into_iter()
            .map(|(unit, start, end, symbol)| PackageOccurrence {
                symbol: SymbolId(symbol),
                unit: SourceUnitId(unit),
                range: TextRange { start, end },
                producer: None,
            })
            .collect();
    }

    /// Records one warning, dropping it once the publication's warning bound is reached:
    /// the bound is what a reader can act on, and the log carries the rest.
    fn warn(&mut self, warning: PackageAnalysisWarning) {
        if self.warnings.len() < bound(self.publication.warnings) {
            self.warnings.push(warning);
        }
    }

    /// Emits one file's unit record, its declarations, and the documents they rank under.
    fn file(&mut self, package: &SymbolOwner, held: &AnalyzedFile) -> Result<(), RiftError> {
        let language = held.file.syntax().language().clone();
        let path = ProjectPath(held.placement.unit().key().as_str().to_owned());
        let unit = wire_unit(held.placement.unit());
        let source = held.file.source();
        let needs_document = documentation_format(held.file.path().as_str())
            != Some(DocumentationSourceFormat::Notebook);
        let publishes_document = needs_document && self.document_capacity();
        let mut retained = self.retained(source, &path, if publishes_document { 2 } else { 1 })?;
        let content_digest = file_digest(held.file.digest());
        let mut record = PackageSourceUnit {
            unit: unit.clone(),
            path: path.clone(),
            language: language.clone(),
            content_digest: content_digest.clone(),
            source: if publishes_document {
                retained.text.clone()
            } else {
                std::mem::take(&mut retained.text)
            },
            source_complete: retained.complete,
            digest: Digest(String::new()),
        };
        record.digest = digest_of(&record, package)?;
        self.units.push(record);
        if publishes_document {
            let document_digest = if retained.complete {
                content_digest
            } else {
                text_digest(&retained.text)
            };
            self.file_document(package, &language, &unit, &path, retained, document_digest)?;
        } else if needs_document {
            self.warn_document_full();
        }
        Ok(())
    }

    /// Emits one file's own search document, ranked by its content.
    fn file_document(
        &mut self,
        package: &SymbolOwner,
        language: &Language,
        unit: &SourceUnitId,
        path: &ProjectPath,
        retained: RetainedSource,
        content_digest: Digest,
    ) -> Result<(), RiftError> {
        let name = file_name(path);
        let document = PackageDocument {
            identity: unit.0.clone(),
            kind: PackageDocumentKind::File,
            unit: unit.clone(),
            language: language.clone(),
            owner: package.clone(),
            content_digest,
            identifier_terms: identifier_terms_with_limit(
                &[&name],
                self.publication.identifier_terms,
            ),
            name,
            qualified_name: None,
            signature: None,
            documentation: None,
            declaration_source: None,
            file_content: Some(retained.text),
            digest: Digest(String::new()),
        };
        self.document(document)
    }

    /// Retains separate reference occurrences from the captured adjacency.
    fn edges(
        &mut self,
        owner: &SymbolOwner,
        semantics: &WorkspaceSemantics,
        analyzed: &[AnalyzedFile],
        limit: usize,
    ) -> Result<(), RiftError> {
        let objects = self
            .objects
            .iter()
            .filter_map(|object| object.id.as_ref())
            .map(|id| id.0.clone())
            .collect::<BTreeSet<_>>();
        let units = self
            .units
            .iter()
            .map(|unit| (unit.unit.0.clone(), unit.source_complete))
            .collect::<BTreeMap<_, _>>();
        let mut modules = BTreeMap::new();
        let sources = analyzed
            .iter()
            .map(|held| {
                let unit = held.placement.unit().to_string();
                let syntax = held.file.syntax();
                let language = syntax.language().name.as_str();
                if !self.symbols_truncated
                    && matches!(language, "javascript" | "typescript" | "rust")
                    && let Some(module) = held.placement.logical_identity(".")
                    && !objects.contains(module.as_str())
                    && semantics
                        .edges()
                        .outgoing(module)
                        .iter()
                        .any(|edge| !edge.is_file_module_reference())
                {
                    let proven = (language != "javascript"
                        || javascript_module_source(owner, held))
                        && units.get(&unit) == Some(&true)
                        && !syntax.has_errors()
                        && syntax.left_out_declaration_count() == 0
                        && semantics.edges().edge_module(held.placement.unit()) == Some(module);
                    modules
                        .entry(module)
                        .and_modify(|held| *held = None)
                        .or_insert(proven.then_some(held));
                }
                (unit, held.file.source())
            })
            .collect::<BTreeMap<_, _>>();
        for record in semantics.graph().records() {
            let Some(id) = record.identity() else {
                continue;
            };
            if !modules.is_empty() {
                // Even a graph record refused by object publication owns this identity.
                modules.remove(id);
            }
            if !objects.contains(id.as_str()) {
                continue;
            }
            for edge in semantics
                .edges()
                .outgoing(id)
                .iter()
                .filter(|edge| !edge.is_graph_edge() && !edge.is_file_module_reference())
            {
                if self.edges.len() >= limit {
                    self.warn(truncation(
                        "edges",
                        u64::try_from(limit).unwrap_or(u64::MAX),
                    ));
                    return Ok(());
                }
                let edge = edge.to_protocol();
                if SymbolId::parse(&edge.from.0).is_err() || SymbolId::parse(&edge.to.0).is_err() {
                    self.identity_incomplete = true;
                    continue;
                }
                if let Some(span) = edge.occurrence.as_ref() {
                    let valid = usize::try_from(span.range.start)
                        .ok()
                        .zip(usize::try_from(span.range.end).ok())
                        .filter(|(start, end)| start <= end)
                        .and_then(|(start, end)| sources.get(&span.unit.0)?.get(start..end))
                        .is_some();
                    if !units.contains_key(&span.unit.0) {
                        self.identity_incomplete = true;
                        continue;
                    }
                    if !valid {
                        return Err(errors::analysis::package_provider_failed()
                            .package(package_label(owner))
                            .error());
                    }
                }
                self.edges.push(edge);
            }
        }
        self.graph_edges(owner, semantics, &objects, &units, &sources, limit)?;
        self.module_edges(owner, semantics, &objects, &modules, limit);
        self.publish_empty_file_modules(owner, semantics, analyzed, &objects, &units);
        if semantics.edges().dropped_edges() > 0 {
            self.warn(truncation(
                "edges",
                u64::try_from(limit).unwrap_or(u64::MAX),
            ));
        }
        Ok(())
    }

    /// A checked file module can own an import without a written declaration.
    /// Direct runtime references of admitted JavaScript and TypeScript modules follow the
    /// prior module imports and JavaScript calls under the package publication bounds.
    fn module_edges(
        &mut self,
        owner: &SymbolOwner,
        semantics: &WorkspaceSemantics,
        objects: &BTreeSet<String>,
        modules: &BTreeMap<&CoreSymbolId, Option<&AnalyzedFile>>,
        limit: usize,
    ) {
        let checked = modules
            .iter()
            .filter_map(|(&module, &held)| Some((module, held?)));
        let javascript = |(_, held): &(&CoreSymbolId, &AnalyzedFile)| {
            held.file.syntax().language().name == "javascript"
        };
        // Existing TypeScript and Rust imports retain their place under the package caps.
        let existing = checked
            .clone()
            .filter(|entry| !javascript(entry))
            .chain(checked.clone().filter(javascript));
        let runtime = checked.filter(|(_, held)| {
            let site = held.file.syntax().language().name.as_str();
            matches!(site, "javascript" | "typescript")
                && !crate::static_resolution::is_declaration_file(held.file.path().as_str())
                && javascript_module_source(owner, held)
        });
        let passes = existing
            .map(|entry| (entry, false))
            .chain(runtime.map(|entry| (entry, true)));
        let mut published = BTreeSet::new();
        for ((module, held), supplemental) in passes {
            let language = held.file.syntax().language();
            let Ok(identity) = SymbolIdentity::parse(module.as_str()) else {
                continue;
            };
            let [name] = identity.qualified_path() else {
                continue;
            };
            if identity.owner() != owner || identity.language() != language {
                continue;
            }
            let direct_roles: &[EdgeKind] = match (language.name.as_str(), supplemental) {
                ("javascript", false) => &[EdgeKind::Calls],
                ("javascript", true) => &[EdgeKind::Reads, EdgeKind::Writes],
                ("typescript", true) => &[EdgeKind::Calls, EdgeKind::Reads, EdgeKind::Writes],
                _ => &[],
            };
            let declarations = (!direct_roles.is_empty())
                .then(|| javascript_declaration_scopes(held.file.syntax()))
                .flatten();
            for edge in semantics.edges().outgoing(module) {
                let occurrence = edge.occurrence();
                let range = occurrence.range();
                let import = !supplemental && edge.kind() == EdgeKind::Imports;
                let direct = direct_roles.contains(&edge.kind())
                    && declarations
                        .as_ref()
                        .is_some_and(|scopes| scopes.resolve(range.start(), range.end()).is_none());
                if edge.is_graph_edge()
                    || edge.is_file_module_reference()
                    || !(import || direct)
                    || occurrence.unit() != held.placement.unit()
                    || held
                        .file
                        .source()
                        .get(offset_in(range.start())..offset_in(range.end()))
                        .is_none_or(str::is_empty)
                {
                    continue;
                }
                let local = objects.contains(edge.to().as_str());
                if !local && !import {
                    continue;
                }
                let external = !local
                    && SymbolIdentity::parse(edge.to().as_str()).is_ok_and(|target| {
                        target.owner() != owner
                            && matches!(
                                target.owner(),
                                SymbolOwner::Package { .. } | SymbolOwner::Runtime { .. }
                            )
                    });
                // The crate-root projection is limited to imports owned outside this package.
                if language.name == "rust" && !external {
                    continue;
                }
                if !local && !external {
                    self.identity_incomplete = true;
                    continue;
                }
                if self.edges.len() >= limit {
                    self.warn(truncation(
                        "edges",
                        u64::try_from(limit).unwrap_or(u64::MAX),
                    ));
                    return;
                }
                if !published.contains(module) {
                    if !self.publish_file_module(language, name, module) {
                        return;
                    }
                    published.insert(module);
                }
                self.edges.push(edge.to_protocol());
            }
        }
    }

    /// Retains complete explicit file modules that declare nothing and own no stored edge.
    /// Their optional objects follow every earlier declaration and module-edge owner.
    fn publish_empty_file_modules(
        &mut self,
        owner: &SymbolOwner,
        semantics: &WorkspaceSemantics,
        analyzed: &[AnalyzedFile],
        objects: &BTreeSet<String>,
        units: &BTreeMap<String, bool>,
    ) {
        if self.symbols_truncated
            || !matches!(owner, SymbolOwner::Package { manager, .. } if manager == "npm")
        {
            return;
        }
        let mut candidates = BTreeMap::new();
        for held in analyzed {
            let Some(module) = held.placement.logical_identity(".") else {
                continue;
            };
            if objects.contains(module.as_str())
                || semantics
                    .edges()
                    .outgoing(module)
                    .iter()
                    .any(|edge| !edge.is_file_module_reference())
            {
                continue;
            }
            let syntax = held.file.syntax();
            let language = syntax.language();
            let path = held.file.path().as_str();
            let module_extension = matches!(
                (language.name.as_str(), path.rsplit_once('.')),
                ("javascript", Some((_, "mjs" | "cjs"))) | ("typescript", Some((_, "mts" | "cts")))
            );
            let proven = matches!(language.name.as_str(), "javascript" | "typescript")
                && (module_extension || javascript_module_source(owner, held))
                && !crate::static_resolution::is_declaration_file(path)
                && units.get(&held.placement.unit().to_string()) == Some(&true)
                && !syntax.has_errors()
                && syntax.left_out_declaration_count() == 0
                && syntax.symbols().is_empty();
            candidates
                .entry(module)
                .and_modify(|held| *held = None)
                .or_insert(proven.then_some(language));
        }
        if candidates.is_empty() {
            return;
        }
        for identity in semantics
            .graph()
            .records()
            .iter()
            .filter_map(|row| row.identity())
        {
            // Refused graph records still own their established identities.
            candidates.remove(identity);
        }
        for (module, language) in candidates {
            let (Some(language), Ok(identity)) = (language, SymbolIdentity::parse(module.as_str()))
            else {
                continue;
            };
            let [name] = identity.qualified_path() else {
                continue;
            };
            if identity.owner() == owner
                && identity.language() == language
                && !self.publish_file_module(language, name, module)
            {
                return;
            }
        }
    }

    fn publish_file_module(
        &mut self,
        language: &Language,
        name: &str,
        module: &CoreSymbolId,
    ) -> bool {
        if self.objects.len() >= bound(self.publication.symbols) {
            self.symbols_truncated = true;
            self.warn(truncation("objects", u64::from(self.publication.symbols)));
            return false;
        }
        let facts =
            PortableSymbolFacts::new(language.clone(), name, name, ExactKind("module".to_owned()))
                .kinds(vec![SymbolKind::Module]);
        let origin = ContributionOrigin::new(None, SourceKind::Synthetic)
            .expect("source-less module origin");
        let mut object = rift_provider::AssembledSymbol::wire_facts(&facts, &origin);
        object.id = Some(SymbolId(module.as_str().to_owned()));
        self.object_unavailable(&object, name, None);
        self.objects.push(object);
        true
    }

    fn graph_edges(
        &mut self,
        owner: &SymbolOwner,
        semantics: &WorkspaceSemantics,
        objects: &BTreeSet<String>,
        units: &BTreeMap<String, bool>,
        sources: &BTreeMap<String, &str>,
        limit: usize,
    ) -> Result<(), RiftError> {
        for edge in semantics.graph().edges() {
            let Some(from) = semantics
                .graph()
                .record_for(edge.source().reference())
                .and_then(rift_core::SymbolRecord::identity)
                .filter(|id| objects.contains(id.as_str()))
            else {
                self.identity_incomplete = true;
                continue;
            };
            let rift_provider::NormalizedTarget::Symbol(to) = edge.target() else {
                self.identity_incomplete = true;
                continue;
            };
            let Some(derivation) = edge
                .derivation()
                .filter(|derivation| *derivation != rift_protocol::read::EdgeDerivation::Heuristic)
            else {
                continue;
            };
            if self.edges.len() >= limit {
                self.warn(truncation(
                    "edges",
                    u64::try_from(limit).unwrap_or(u64::MAX),
                ));
                break;
            }
            let occurrence = edge
                .occurrence()
                .map(|binding| rift_protocol::read::SourceUnitSpan {
                    unit: wire_unit(binding.unit()),
                    range: TextRange {
                        start: binding.range().start(),
                        end: binding.range().end(),
                    },
                });
            if let Some(span) = occurrence.as_ref() {
                let valid = usize::try_from(span.range.start)
                    .ok()
                    .zip(usize::try_from(span.range.end).ok())
                    .filter(|(start, end)| start < end)
                    .and_then(|(start, end)| sources.get(&span.unit.0)?.get(start..end))
                    .is_some();
                if !units.contains_key(&span.unit.0) {
                    self.identity_incomplete = true;
                    continue;
                }
                if !valid {
                    return errors::analysis::package_provider_failed()
                        .package(package_label(owner))
                        .fail();
                }
            }
            let kind = edge_kind(edge.kind());
            self.edges.push(rift_protocol::index::StoredEdge {
                producer: None,
                from: SymbolId::parse(from.as_str()).map_err(|_| {
                    errors::analysis::package_provider_failed()
                        .package(package_label(owner))
                        .error()
                })?,
                to: SymbolId::parse(to.as_str()).map_err(|_| {
                    errors::analysis::package_provider_failed()
                        .package(package_label(owner))
                        .error()
                })?,
                kind: rift_protocol::read::ExactKind(kind.as_ref().to_owned()),
                kinds: vec![kind],
                occurrence,
                provider: rift_protocol::read::EdgeProvider::Static,
                derivation,
                confidence: None,
                extensions: rift_protocol::read::Extensions::default(),
            });
        }
        Ok(())
    }

    /// Projects captured logical records and every established physical binding.
    fn graph(
        &mut self,
        owner: &SymbolOwner,
        origin: &rift_protocol::read::SymbolOrigin,
        semantics: &WorkspaceSemantics,
        analyzed: &[AnalyzedFile],
    ) -> Result<(), RiftError> {
        let files = analyzed
            .iter()
            .map(|held| {
                (
                    held.placement.unit().to_string(),
                    (held, line_starts(held.file.source())),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let admitted = self
            .units
            .iter()
            .map(|unit| unit.unit.0.clone())
            .collect::<BTreeSet<_>>();
        for record in semantics.graph().records() {
            if self.objects.len() >= bound(self.publication.symbols) {
                self.symbols_truncated = true;
                self.warn(truncation("objects", u64::from(self.publication.symbols)));
                break;
            }
            let assembled = semantics.assembled_record(record).ok_or_else(|| {
                errors::analysis::package_provider_failed()
                    .package(package_label(owner))
                    .error()
            })?;
            let facts = assembled.facts().ok_or_else(|| {
                errors::analysis::package_provider_failed()
                    .package(package_label(owner))
                    .error()
            })?;
            let mut object = assembled.to_protocol_symbol(facts);
            self.object_identity(owner, origin, &mut object);
            let document_signature =
                stub_document_signature(semantics, assembled.contributions(), &files, &admitted);
            let mut has_source = false;
            for key in assembled.contributions() {
                let contribution = semantics.graph().contribution(key).ok_or_else(|| {
                    errors::analysis::package_provider_failed()
                        .package(package_label(owner))
                        .error()
                })?;
                let Some(binding) = contribution.source() else {
                    continue;
                };
                has_source = true;
                let unit = binding.unit().to_string();
                let Some((held, starts)) = files.get(&unit).filter(|_| admitted.contains(&unit))
                else {
                    self.identity_incomplete = true;
                    continue;
                };
                self.object_unavailable(&object, facts.qualified_name(), Some(contribution));
                let Some(_) = object
                    .id
                    .as_ref()
                    .filter(|id| SymbolId::parse(&id.0).is_ok())
                else {
                    self.identity_incomplete = true;
                    self.warn(PackageAnalysisWarning::IdentityUnresolved {
                        unit: Some(wire_unit(binding.unit())),
                        range: Some(TextRange {
                            start: binding.range().start(),
                            end: binding.range().end(),
                        }),
                        qualified_name: contribution
                            .facts()
                            .map_or_else(String::new, |facts| facts.qualified_name().to_owned()),
                    });
                    continue;
                };
                self.declaration(
                    owner,
                    &object,
                    document_signature,
                    contribution,
                    held,
                    starts,
                )?;
            }
            if object.id.is_none() && !has_source {
                self.unresolved_object(facts.qualified_name());
            }
            if !has_source {
                self.object_unavailable(&object, facts.qualified_name(), None);
            }
            self.objects.push(object);
        }
        Ok(())
    }

    fn retained_syntax(&mut self, analyzed: &[AnalyzedFile]) -> bool {
        let mut complete = true;
        let admitted = self
            .units
            .iter()
            .map(|unit| unit.unit.0.clone())
            .collect::<BTreeSet<_>>();
        for held in analyzed {
            let unit = wire_unit(held.placement.unit());
            complete &= held.file.syntax().left_out_declaration_count()
                == held.file.syntax().left_out_symbols().len();
            if !admitted.contains(&unit.0) {
                complete = false;
                continue;
            }
            for symbol in held.file.syntax().left_out_symbols() {
                complete &= retained_declaration_ranges(held.file.source(), symbol);
                if self.objects.len() >= bound(self.publication.symbols) {
                    self.symbols_truncated = true;
                    self.warn(truncation("objects", u64::from(self.publication.symbols)));
                    return false;
                }
                let facts = symbol.facts(held.file.syntax().language());
                let object =
                    rift_provider::AssembledSymbol::wire_facts(&facts, held.placement.origin());
                let range = TextRange {
                    start: symbol.item_range.start,
                    end: symbol.item_range.end,
                };
                self.identity_incomplete = true;
                self.warn(PackageAnalysisWarning::IdentityUnresolved {
                    unit: Some(unit.clone()),
                    range: Some(range.clone()),
                    qualified_name: symbol.qualified_name.clone(),
                });
                if !object.name_is_public() {
                    self.warn(PackageAnalysisWarning::ObjectUnavailable {
                        unit: Some(unit.clone()),
                        range: Some(range),
                        qualified_name: symbol.qualified_name.clone(),
                        field: "name".to_owned(),
                        bound: u64::try_from(rift_protocol::read::SYMBOL_NAME_CHARACTERS_MAX)
                            .expect("public symbol name bound fits u64"),
                    });
                }
                self.objects.push(object);
            }
        }
        complete
    }

    fn object_identity(&mut self, owner: &SymbolOwner, origin: &SymbolOrigin, object: &mut Symbol) {
        if object.id.as_ref().is_some_and(|id| {
            !rift_protocol::identity::SymbolIdentity::parse(id.as_str()).is_ok_and(|identity| {
                identity.owner() == owner && identity.language() == &object.language
            })
        }) {
            object.id = None;
            self.identity_incomplete = true;
        }
        if object.container.as_ref().is_some_and(|id| {
            !rift_protocol::identity::SymbolIdentity::parse(id.as_str()).is_ok_and(|identity| {
                identity.owner() == owner && identity.language() == &object.language
            })
        }) {
            object.container = None;
        }
        object.origin.location = origin.location;
        object.origin.package.clone_from(&origin.package);
        object.origin.runtime.clone_from(&origin.runtime);
    }

    fn unresolved_object(&mut self, qualified_name: &str) {
        self.identity_incomplete = true;
        self.warn(PackageAnalysisWarning::IdentityUnresolved {
            unit: None,
            range: None,
            qualified_name: qualified_name.to_owned(),
        });
    }

    fn object_unavailable(
        &mut self,
        object: &Symbol,
        qualified_name: &str,
        contribution: Option<&rift_core::Contribution>,
    ) {
        if object.name_is_public() {
            return;
        }
        self.identity_incomplete = true;
        let (unit, range) = contribution
            .and_then(rift_core::Contribution::source)
            .map_or((None, None), |binding| {
                (
                    Some(wire_unit(binding.unit())),
                    Some(TextRange {
                        start: binding.range().start(),
                        end: binding.range().end(),
                    }),
                )
            });
        self.warn(PackageAnalysisWarning::ObjectUnavailable {
            unit,
            range,
            qualified_name: contribution
                .and_then(rift_core::Contribution::facts)
                .map_or(
                    qualified_name,
                    rift_core::PortableSymbolFacts::qualified_name,
                )
                .to_owned(),
            field: "name".to_owned(),
            bound: u64::try_from(rift_protocol::read::SYMBOL_NAME_CHARACTERS_MAX)
                .expect("public symbol name bound fits u64"),
        });
    }

    /// Stores one binding and its array references without duplicating logical facts.
    fn declaration(
        &mut self,
        owner: &SymbolOwner,
        object: &Symbol,
        document_signature: Option<&str>,
        contribution: &rift_core::Contribution,
        held: &AnalyzedFile,
        starts: &[usize],
    ) -> Result<(), RiftError> {
        let id = object
            .id
            .as_ref()
            .expect("established declaration identity");
        if self.declarations.len() >= bound(self.publication.symbols) {
            self.symbols_truncated = true;
            self.warn(truncation(
                "declarations",
                u64::from(self.publication.symbols),
            ));
            return Ok(());
        }
        let binding = contribution.source().expect("captured source binding");
        let facts = contribution.facts().ok_or_else(|| {
            errors::analysis::package_provider_failed()
                .package(package_label(owner))
                .error()
        })?;
        let start = offset_in(binding.range().start());
        let end = offset_in(binding.range().end());
        let source = held.file.source().get(start..end).ok_or_else(|| {
            errors::analysis::package_provider_failed()
                .package(package_label(owner))
                .path(Path::new(held.file.path().as_str()))
                .error()
        })?;
        let unit = wire_unit(binding.unit());
        let path = wire_path(held.file.path());
        let public = held.is_public(facts.qualified_name())
            || (held.reached
                && facts
                    .symbol_kinds()
                    .contains(&rift_protocol::read::SymbolKind::Alias)
                && facts
                    .symbol_kinds()
                    .contains(&rift_protocol::read::SymbolKind::Public));
        let publishes_document = public
            && object.name_is_public()
            && self.document_capacity()
            && self.symbol_documents.insert(id.0.clone());
        let retained = self.retained(source, &path, if publishes_document { 2 } else { 1 })?;
        let mut record = PackageSymbol {
            symbol: id.clone(),
            origin: rift_provider::AssembledSymbol::wire_origin(contribution.origin()),
            unit: unit.clone(),
            range: TextRange {
                start: binding.range().start(),
                end: binding.range().end(),
            },
            syntax_element_kind: held.item_kind(binding.range(), facts.qualified_name()),
            line: line_of(starts, binding.range().start()),
            signature_indices: fact_indices(owner, &object.signatures, facts.signatures_slice())?,
            type_indices: fact_indices(owner, &object.types, facts.type_bindings())?,
            documentation_indices: fact_indices(
                owner,
                &object.documentation,
                facts.documentation_blocks(),
            )?,
            source: retained.text,
            source_complete: retained.complete,
            public,
            digest: Digest(String::new()),
        };
        record.digest = digest_of(&record, owner)?;
        if publishes_document {
            let document = PackageDocument {
                identity: id.0.clone(),
                kind: PackageDocumentKind::Symbol,
                unit,
                language: facts.language().clone(),
                owner: owner.clone(),
                content_digest: text_digest(&record.source),
                identifier_terms: identifier_terms_with_limit(
                    &[facts.name(), facts.qualified_name()],
                    self.publication.identifier_terms,
                ),
                name: facts.name().to_owned(),
                qualified_name: Some(facts.qualified_name().to_owned()),
                signature: document_signature
                    .or_else(|| {
                        facts
                            .signatures_slice()
                            .first()
                            .map(|signature| signature.display.as_str())
                    })
                    .map(str::to_owned),
                documentation: facts
                    .documentation_blocks()
                    .first()
                    .map(|documentation| documentation.text.clone()),
                declaration_source: Some(record.source.clone()),
                file_content: None,
                digest: Digest(String::new()),
            };
            self.document(document)?;
        } else if public && object.name_is_public() && !self.document_capacity() {
            self.warn_document_full();
        }
        self.declarations.push(record);
        Ok(())
    }

    /// Takes one document, or reports the bound once the collection reaches it.
    fn document(&mut self, mut document: PackageDocument) -> Result<(), RiftError> {
        if !self.document_capacity() {
            self.warn_document_full();
            return Ok(());
        }
        document.digest = digest_of(&document, &document.owner.clone())?;
        self.documents.push(document);
        Ok(())
    }

    fn document_capacity(&self) -> bool {
        self.documents.len() < bound(self.publication.documents)
    }

    fn warn_document_full(&mut self) {
        if !self.documents_truncated {
            self.documents_truncated = true;
            self.warn(truncation(
                "documents",
                u64::from(self.publication.documents),
            ));
        }
    }

    /// The bytes one record retains, reporting a cut once for the record that made it.
    fn retained(
        &mut self,
        source: &str,
        path: &ProjectPath,
        copies: u64,
    ) -> Result<RetainedSource, RiftError> {
        let maximum = self
            .retained_source
            .map_or(PACKAGE_SOURCE_BYTES_MAX, |limits| limits.record_bytes_max);
        let mut kept = source.len().min(bound(maximum));
        while kept > 0 && !source.is_char_boundary(kept) {
            kept -= 1;
        }
        if let Some(limits) = self.retained_source {
            let observed = u64::try_from(kept)
                .ok()
                .and_then(|bytes| bytes.checked_mul(copies))
                .and_then(|bytes| self.retained_source_bytes.checked_add(bytes));
            let bytes = observed
                .filter(|bytes| *bytes <= limits.total_bytes_max)
                .ok_or_else(|| {
                    errors::analysis::package_retained_source_bytes_exceeded()
                        .bound(limits.total_bytes_max)
                        .observed(observed.unwrap_or(u64::MAX))
                        .error()
                })?;
            self.retained_source_bytes = bytes;
        }
        let complete = kept == source.len();
        if !complete {
            self.warn(PackageAnalysisWarning::SourceTruncated {
                path: path.clone(),
                dropped: u64::try_from(source.len() - kept).unwrap_or(u64::MAX),
            });
        }
        Ok(RetainedSource {
            text: source[..kept].to_owned(),
            complete,
        })
    }
}

fn fact_indices<T: PartialEq>(
    owner: &SymbolOwner,
    combined: &[T],
    supplied: &[T],
) -> Result<Vec<u64>, RiftError> {
    rift_core::fact_indices(combined, supplied, || {
        errors::analysis::package_provider_failed()
            .package(package_label(owner))
            .error()
    })
}

/// One record's retained source: the bytes it keeps, and whether they are the whole
/// content.
#[derive(Clone)]
struct RetainedSource {
    text: String,
    complete: bool,
}

/// One byte offset into a file this process holds in memory. An offset past `usize` names
/// bytes no in-memory file carries, so it reads as the end of every file.
fn offset_in(offset: u64) -> usize {
    usize::try_from(offset).unwrap_or(usize::MAX)
}

/// Where each line of one file starts, in ascending offset order, taken once per file.
///
/// One pass over the file, so a file's declarations cost one scan between them rather
/// than one scan each.
/// The file name a document ranks under, including its extension.
fn file_name(path: &ProjectPath) -> String {
    Path::new(path.0.as_str())
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or(path.0.as_str())
        .to_owned()
}

/// The words a ranking splits identifiers into: each name split on case and separator
/// boundaries, lowercased, deduplicated, in first-seen order, at most
/// `PACKAGE_IDENTIFIER_TERMS_MAX` of them by default.
///
/// The cut is a work bound, not a decision: the terms a ranking reads come from a
/// declaration's own name, and a name splitting into more than a thousand words ranks on
/// the first thousand.
#[cfg(test)]
fn identifier_terms(names: &[&str]) -> Vec<String> {
    identifier_terms_with_limit(names, rift_protocol::index::PACKAGE_IDENTIFIER_TERMS_MAX)
}

/// Splits identifier words under the accepted publication bound.
fn identifier_terms_with_limit(names: &[&str], maximum: u32) -> Vec<String> {
    let terms_max = bound(maximum);
    let mut terms: Vec<String> = Vec::new();
    for name in names {
        for word in split_identifier_words(name) {
            if terms.len() >= terms_max {
                return terms;
            }
            let word = word.to_lowercase();
            if !terms.contains(&word) {
                terms.push(word);
            }
        }
    }
    terms
}

/// The origin every declaration of one analyzed package carries.
fn symbol_origin(
    owner: &SymbolOwner,
    origin: &ContributionOrigin,
) -> Result<SymbolOrigin, RiftError> {
    let result = match origin.location() {
        Some(rift_core::SourceLocation::Dependency { package })
            if package.owner().ok().as_ref() == Some(owner) =>
        {
            SymbolOrigin {
                location: Some(SourceLocationKind::Dependency),
                package: Some(package.clone()),
                runtime: None,
                source_kind: SourceKind::Authored,
            }
        }
        Some(rift_core::SourceLocation::Stdlib {
            runtime: Some(runtime),
        }) if runtime.owner().ok().as_ref() == Some(owner) => SymbolOrigin {
            location: Some(SourceLocationKind::Stdlib),
            package: None,
            runtime: Some(runtime.clone()),
            source_kind: SourceKind::Authored,
        },
        _ => {
            return errors::analysis::package_identity_invalid()
                .package(package_label(owner))
                .fail();
        }
    };
    Ok(result)
}

/// The warning a collection that reached its bound reports.
fn truncation(collection: &str, bound: u64) -> PackageAnalysisWarning {
    PackageAnalysisWarning::PublicationTruncated {
        collection: collection.to_owned(),
        bound,
    }
}

/// The digest of one record's canonical content: its own JSON without the `digest`
/// member, rendered as RFC 8785 canonical JSON.
///
/// The member is dropped rather than left empty so a record's digest covers what the
/// record says and nothing about the digest field itself.
fn digest_of<T: Serialize>(record: &T, package: &SymbolOwner) -> Result<Digest, RiftError> {
    let mut value = serde_json::to_value(record).map_err(|error| {
        errors::analysis::package_provider_failed()
            .package(package_label(package))
            .source(error)
            .error()
    })?;
    if let Some(object) = value.as_object_mut() {
        object.remove("digest");
    }
    let rendered = canonical_json(&value).map_err(|error| {
        errors::analysis::package_provider_failed()
            .package(package_label(package))
            .source(error)
            .error()
    })?;
    Ok(text_digest(&rendered))
}

/// One publication bound as the in-memory count a collection compares against.
///
/// Every bound this module reads is a `u32` the protocol states, and `u32` always fits
/// `usize` on every platform Rift targets.
const fn bound(value: u32) -> usize {
    value as usize
}

/// The wire digest of one text, in the eight-character form every wire digest takes.
fn text_digest(text: &str) -> Digest {
    file_digest(crate::FileDigest::of(text.as_bytes()))
}

fn file_digest(digest: crate::FileDigest) -> Digest {
    use std::fmt::Write as _;

    let mut rendered = String::with_capacity(DIGEST_WIRE_CHARS);
    for byte in digest.as_bytes().iter().take(DIGEST_WIRE_CHARS.div_ceil(2)) {
        write!(&mut rendered, "{byte:02x}").expect("String accepts formatted bytes");
    }
    rendered.truncate(DIGEST_WIRE_CHARS);
    Digest(rendered)
}

fn wire_unit(unit: &CoreSourceUnitId) -> SourceUnitId {
    SourceUnitId(unit.to_string())
}

fn wire_path(path: &CoreProjectPath) -> ProjectPath {
    ProjectPath(path.as_str().to_owned())
}

/// One package file parsed by the provider its extension names, under `syntax_limits`.
#[cfg(test)]
fn parsed_file(
    file: crate::input::PackageSource<'_>,
    package: &SymbolOwner,
    package_language: &Language,
    syntax_limits: rift_syntax::SyntaxLimits,
) -> Result<IndexedFile, RiftError> {
    Ok(IndexedFile::new_with_shared_syntax(
        file.path().clone(),
        file.text().to_owned().into(),
        crate::FileDigest::of(file.text().as_bytes()),
        false,
        std::sync::Arc::clone(
            crate::PackageSyntaxSource::new(file, package, package_language, syntax_limits)
                .parse()?
                .facts(),
        ),
    ))
}

pub(super) fn source_language(extension: &str, package_language: &Language) -> Language {
    match extension {
        "md" | "markdown" | "mdx" => ShippedLanguage::Markdown.language(),
        "rst" => Language {
            name: "rst".to_owned(),
            dialect: None,
        },
        "txt" => Language {
            name: "text".to_owned(),
            dialect: None,
        },
        "ipynb" => Language {
            name: "json".to_owned(),
            dialect: None,
        },
        _ => package_language.clone(),
    }
}

/// The placement of one package file: its unit, identity path, and origin.
fn placement_of(
    package: &SymbolOwner,
    origin: &ContributionOrigin,
    file: crate::PackageSource<'_>,
) -> Result<DocumentPlacement, RiftError> {
    let path = file.path();
    let unit = match file.source_unit() {
        Some(unit) => unit.clone(),
        None => CoreSourceUnitId::for_owner(package.clone(), path.as_str()).map_err(|error| {
            errors::analysis::package_identity_invalid()
                .package(package_label(package))
                .path(Path::new(path.as_str()))
                .cause(error)
                .error()
        })?,
    };
    let address = unit.to_string();
    let identity_path = address
        .strip_prefix("rift://source/")
        .ok_or_else(|| {
            errors::analysis::package_identity_invalid()
                .package(package_label(package))
                .path(Path::new(path.as_str()))
                .error()
        })?
        .to_owned();
    Ok(DocumentPlacement::new(
        file.origin().unwrap_or(origin).clone(),
        unit,
        identity_path,
    ))
}

/// The languages package analysis reads an API from, and each one's rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageLanguage {
    /// Rust declarations use `pub`; items of a public trait and variants of a public enum
    /// also count.
    Rust,
    /// Python public declarations and protocol methods of visible classes.
    Python,
    /// TypeScript declarations exclude private and protected members. A TypeScript package
    /// ships its JavaScript builds beside its declaration files.
    TypeScript,
    /// HTML elements and attributes are public declarations.
    Html,
    /// Angular template elements and attributes are public declarations.
    HtmlAngular,
    /// CSS selectors and properties are public declarations.
    Css,
    /// Vue elements and script declarations are public unless marked private or protected.
    Vue,
    /// Svelte elements and script declarations are public unless marked private or protected.
    Svelte,
    /// C declarations are public.
    C,
    /// C++ declarations are public.
    Cpp,
    /// Cython public declarations and protocol methods of visible classes.
    Cython,
    /// JSONC properties are public declarations.
    Jsonc,
}

/// JavaScript container kinds whose members an exported container exports: a class's
/// methods carry no `export` of their own.
const JAVASCRIPT_PUBLIC_MEMBER_CONTAINERS: [&str; 1] = ["class"];

/// The prefix of an ES private element's name (`#secret`), which nothing outside its
/// class reaches.
const PRIVATE_ELEMENT_PREFIX: char = '#';

/// The shipped language a definition serves `language` under.
fn shipped_language(language: &Language) -> Option<ShippedLanguage> {
    rift_syntax::definitions()
        .iter()
        .map(|definition| definition.shipped())
        .find(|shipped| &shipped.language() == language)
}

impl PackageLanguage {
    /// Rules for one language, when a shipped definition serves it.
    #[must_use]
    pub fn for_language(language: &Language) -> Option<Self> {
        match shipped_language(language)? {
            ShippedLanguage::Rust => Some(Self::Rust),
            ShippedLanguage::Python => Some(Self::Python),
            ShippedLanguage::TypeScript | ShippedLanguage::TypeScriptTsx => Some(Self::TypeScript),
            ShippedLanguage::Html => Some(Self::Html),
            ShippedLanguage::HtmlAngular => Some(Self::HtmlAngular),
            ShippedLanguage::Css => Some(Self::Css),
            ShippedLanguage::Vue => Some(Self::Vue),
            ShippedLanguage::Svelte => Some(Self::Svelte),
            ShippedLanguage::C => Some(Self::C),
            ShippedLanguage::Cpp => Some(Self::Cpp),
            ShippedLanguage::Cython => Some(Self::Cython),
            ShippedLanguage::Jsonc => Some(Self::Jsonc),
            ShippedLanguage::JavaScript
            | ShippedLanguage::Markdown
            | ShippedLanguage::Json
            | ShippedLanguage::Yaml
            | ShippedLanguage::Toml => None,
        }
    }

    /// The shipped languages whose files a package in this language holds as source.
    pub(crate) const fn source_languages(self) -> &'static [ShippedLanguage] {
        match self {
            Self::Rust => &[ShippedLanguage::Rust],
            Self::Python => &[ShippedLanguage::Python, ShippedLanguage::Cython],
            Self::TypeScript => &[
                ShippedLanguage::JavaScript,
                ShippedLanguage::TypeScript,
                ShippedLanguage::TypeScriptTsx,
                ShippedLanguage::Html,
                ShippedLanguage::Css,
                ShippedLanguage::Vue,
                ShippedLanguage::Svelte,
            ],
            Self::Html | Self::HtmlAngular => &[ShippedLanguage::Html, ShippedLanguage::Css],
            Self::Css => &[ShippedLanguage::Css],
            Self::Vue => &[
                ShippedLanguage::Vue,
                ShippedLanguage::Html,
                ShippedLanguage::Css,
                ShippedLanguage::JavaScript,
                ShippedLanguage::TypeScript,
                ShippedLanguage::TypeScriptTsx,
            ],
            Self::Svelte => &[
                ShippedLanguage::Svelte,
                ShippedLanguage::Html,
                ShippedLanguage::Css,
                ShippedLanguage::JavaScript,
                ShippedLanguage::TypeScript,
                ShippedLanguage::TypeScriptTsx,
            ],
            Self::C | Self::Cpp => &[ShippedLanguage::C, ShippedLanguage::Cpp],
            Self::Cython => &[ShippedLanguage::Cython, ShippedLanguage::Python],
            Self::Jsonc => &[ShippedLanguage::Jsonc],
        }
    }

    fn is_public(self, symbol: &SyntaxSymbol, python: Option<&PythonExports<'_>>) -> bool {
        match self {
            Self::Rust => symbol.visibility.as_deref() == Some("pub"),
            Self::Python | Self::Cython => python.is_some_and(|exports| exports.is_public(symbol)),
            Self::TypeScript | Self::Vue | Self::Svelte => !symbol
                .visibility
                .as_deref()
                .is_some_and(|visibility| matches!(visibility, "private" | "protected")),
            Self::Html | Self::HtmlAngular | Self::Css | Self::C | Self::Cpp | Self::Jsonc => true,
        }
    }
}

/// The rule deciding which declarations of one package file are public.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExportRule {
    /// A package language's own rule.
    Language(PackageLanguage),
    /// A JavaScript module exports a declaration the provider marks `Public`, one an
    /// `export` wraps or an export names (`export { a }`, `module.exports = { a }`), and a
    /// member of a class so marked, or one assigned to a declaration so marked, other than
    /// an ES private element.
    JavaScriptModule,
}

impl ExportRule {
    /// The rule for files in `language`. Absent for a language with no export rule, whose
    /// every declaration is public.
    fn for_language(language: &Language) -> Option<Self> {
        if shipped_language(language)? == ShippedLanguage::JavaScript {
            return Some(Self::JavaScriptModule);
        }
        PackageLanguage::for_language(language).map(Self::Language)
    }

    fn is_public(
        self,
        symbol: &SyntaxSymbol,
        by_name: &BTreeMap<&str, &SyntaxSymbol>,
        classes: &BTreeMap<ByteRange, ByteRange>,
        python: Option<&PythonExports<'_>>,
    ) -> bool {
        match self {
            Self::Language(rules) => rules.is_public(symbol, python),
            Self::JavaScriptModule => is_javascript_export(symbol, by_name, classes),
        }
    }
}

/// What decides whether a Python declaration is public: its enclosing definitions and the
/// names the module lists.
struct PythonExports<'facts> {
    scopes: crate::EnclosingDefinitions<&'facts SyntaxSymbol>,
    /// The names `__all__` lists, when the module states them.
    listed: Option<BTreeSet<&'facts str>>,
}

impl<'facts> PythonExports<'facts> {
    fn new(source: &'facts str, facts: &'facts SyntaxFacts) -> Self {
        Self {
            scopes: crate::EnclosingDefinitions::new(
                facts
                    .symbols()
                    .iter()
                    .filter(|symbol| matches!(symbol.kind, "class" | "function" | "method"))
                    .map(|symbol| (symbol.item_range.start, symbol.item_range.end, symbol)),
            ),
            listed: Self::listed(source, facts),
        }
    }

    /// A binding without a module source belongs to `__all__`: the first names the list, the
    /// others its entries.
    fn listed(source: &'facts str, facts: &'facts SyntaxFacts) -> Option<BTreeSet<&'facts str>> {
        let mut bindings = facts
            .export_bindings()?
            .iter()
            .filter(|binding| binding.source.is_none());
        bindings.next()?;
        bindings
            .map(|binding| {
                let name = binding.exported?;
                source.get(usize::try_from(name.start).ok()?..usize::try_from(name.end).ok()?)
            })
            .collect()
    }

    /// A declaration is public when its own name and each container around it are public. A
    /// method that assigns an attribute of its class holds the assignment without containing
    /// the attribute.
    fn is_public(&self, symbol: &SyntaxSymbol) -> bool {
        let Some(containing) = self
            .scopes
            .containing(symbol.item_range.start, symbol.item_range.end)
        else {
            return symbol.container.is_none() && self.is_name_public(symbol, None);
        };
        let mut current = symbol;
        for owner in containing.filter(|owner| owner.item_range != symbol.item_range) {
            if !self.is_name_public(current, Some(owner)) {
                return false;
            }
            if current.container.as_deref() == Some(owner.qualified_name.as_str()) {
                current = owner;
            }
        }
        self.is_name_public(current, None)
    }

    /// Whether `symbol`'s own name is public directly inside `owner`, or in the module.
    ///
    /// A leading underscore makes a name private, except a name the module lists and the
    /// special method of a class. A name the module omits from its list stays public: another
    /// module may re-export it or extend the class.
    fn is_name_public(&self, symbol: &SyntaxSymbol, owner: Option<&SyntaxSymbol>) -> bool {
        let listed = owner.is_none()
            && self
                .listed
                .as_ref()
                .is_some_and(|listed| listed.contains(symbol.name.as_str()));
        if listed || !symbol.name.starts_with('_') {
            return true;
        }
        let special = symbol
            .name
            .strip_prefix("__")
            .and_then(|name| name.strip_suffix("__"))
            .is_some_and(|name| !name.is_empty() && !name.starts_with('_') && !name.ends_with('_'));
        special
            && matches!(symbol.kind, "method" | "function")
            && owner.is_some_and(|owner| {
                owner.kind == "class"
                    && Some(owner.qualified_name.as_str()) == symbol.container.as_deref()
            })
    }
}

/// Whether a JavaScript module exports `symbol`: the provider marks it `Public`, or it is a
/// member of a declaration so marked and no ES private element. A class declares members in
/// its body. An assignment declares a member of any declaration it writes to, as
/// `app.init = function () {}` and `Router.prototype.handle = function () {}` do.
fn is_javascript_export(
    symbol: &SyntaxSymbol,
    by_name: &BTreeMap<&str, &SyntaxSymbol>,
    classes: &BTreeMap<ByteRange, ByteRange>,
) -> bool {
    let exported = is_exported(symbol);
    let private_element = symbol.name.starts_with(PRIVATE_ELEMENT_PREFIX);
    let assigned_member = symbol.syntax_element_kind == Some("assignment_expression")
        && matches!(symbol.kind, "method" | "property");
    let member_of_exported_declaration = symbol
        .container
        .as_deref()
        .and_then(|container| by_name.get(container))
        .is_some_and(|container| {
            let class_member = container
                .name_range
                .and_then(|name| classes.get(&name))
                .is_some_and(|body| {
                    body.encloses(symbol.item_range)
                        && container
                            .body_range
                            .is_some_and(|initializer| initializer.encloses(*body))
                });
            (assigned_member
                || class_member
                || JAVASCRIPT_PUBLIC_MEMBER_CONTAINERS.contains(&container.kind))
                && is_exported(container)
        });
    exported || (member_of_exported_declaration && !private_element)
}

/// Whether the provider marks `symbol` public, as an export of it does.
fn is_exported(symbol: &SyntaxSymbol) -> bool {
    symbol.kinds.contains(&SymbolKind::Public)
}

/// Qualified names of declarations a package file exposes under its language's export
/// rule, read from `source` and its syntax facts.
///
/// A paired module's set is replaced afterwards by the one its stub defines, so this rule
/// decides an unpaired file alone.
#[must_use]
pub fn public_qualified_names(
    language: &Language,
    source: &str,
    facts: &SyntaxFacts,
) -> BTreeSet<String> {
    let symbols = facts.symbols();
    let Some(rule) = ExportRule::for_language(language) else {
        return symbols
            .iter()
            .map(|symbol| symbol.qualified_name.clone())
            .collect();
    };
    let python = matches!(
        rule,
        ExportRule::Language(PackageLanguage::Python | PackageLanguage::Cython)
    )
    .then(|| PythonExports::new(source, facts));
    let by_name = if rule == ExportRule::JavaScriptModule {
        symbols
            .iter()
            .map(|symbol| (symbol.qualified_name.as_str(), symbol))
            .collect()
    } else {
        BTreeMap::new()
    };
    let classes = if rule == ExportRule::JavaScriptModule {
        facts
            .static_facts()
            .into_iter()
            .flat_map(rift_syntax::StaticFacts::bindings)
            .filter_map(|binding| match binding.kind {
                StaticBindingKind::JavaScriptClassValue { body } => Some((binding.name, body)),
                _ => None,
            })
            .collect()
    } else {
        BTreeMap::new()
    };
    let public = |symbol| rule.is_public(symbol, &by_name, &classes, python.as_ref());
    if python.is_some() {
        // Name-keyed visibility must hold for every physical declaration that shares it.
        let mut visibility = BTreeMap::new();
        for symbol in symbols {
            let visible = public(symbol);
            visibility
                .entry(symbol.qualified_name.as_str())
                .and_modify(|previous| *previous &= visible)
                .or_insert(visible);
        }
        return visibility
            .into_iter()
            .filter(|(_, visible)| *visible)
            .map(|(name, _)| name.to_owned())
            .collect();
    }
    symbols
        .iter()
        .filter(|symbol| public(symbol))
        .map(|symbol| symbol.qualified_name.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use rift_core::{ContributionOrigin, ProjectPath, SourceKind, SourceLocation};
    use rift_protocol::canonical::canonical_json;
    use rift_protocol::documentation::{
        DocumentationContentIdentity, DocumentationSourceIdentity, DocumentationStage,
        DocumentationWarningKind,
    };
    use rift_protocol::index::{
        PACKAGE_IDENTIFIER_TERMS_MAX, PACKAGE_PUBLICATION_FORMAT_REVISION,
        PACKAGE_SOURCE_BYTES_MAX, PackageAnalysisWarning, PackageDocumentKind,
    };
    use rift_protocol::schema::package_index_schema_document;
    use rift_syntax::{ShippedLanguage, SyntaxLimits};

    use super::fixture::{analyzed, identity, language, package_analysis, package_result};
    use super::{PackageAnalyzer, RiftError, bound, package_label};
    use crate::{ExactPackageInput, ExactPackageLimits, PackageSource};
    use rift_error::errors;

    /// One package of `files` in `shipped`, analyzed, keeping every part of the analysis.
    fn analysis(shipped: ShippedLanguage, files: Vec<(&str, &str)>) -> super::PackageAnalysis {
        package_analysis(shipped, files)
    }

    #[test]
    fn an_exact_cargo_library_accepts_legacy_project_coordinates() {
        let package = identity();
        let owner = package.owner().expect("fixture owner");
        let origin = ContributionOrigin::new(
            Some(SourceLocation::Dependency { package }),
            SourceKind::Authored,
        )
        .expect("exact release origin");
        let language = language(ShippedLanguage::Rust);
        let manifest_path = ProjectPath::new("Cargo.toml").expect("manifest path");
        let root_path = ProjectPath::new("src/chosen.rs").expect("library path");
        let root = PackageSource::new(&root_path, "pub fn serve() {}\n");
        let sources = [root];
        let cases = [
            ("[project]\nname = 'beacon'\nversion = '1.0.0'\n", true),
            ("[project]\nname = 'beacon'\nversion = '2.0.0'\n", false),
            (
                "[package]\nname = 'beacon'\nversion = '1.0.0'\n[project]\ndescription = 'historical'\n",
                true,
            ),
            (
                "[package]\nname = 'other'\nversion = '1.0.0'\n[project]\nname = 'beacon'\nversion = '1.0.0'\n",
                false,
            ),
        ];
        for (sections, accepted) in cases {
            let metadata = format!("{sections}[lib]\nname = 'chosen'\npath = 'src/chosen.rs'\n");
            let manifest = PackageSource::new(&manifest_path, &metadata);
            let context = [manifest];
            let input = ExactPackageInput::new(
                &owner,
                &language,
                &origin,
                &sources,
                ExactPackageLimits::new(2, 4096),
            )
            .expect("bounded release input")
            .with_framework_context(&context, &[])
            .expect("original metadata");
            let selected = input.with_library(&owner, manifest, root);
            if accepted {
                let publication = PackageAnalyzer::analyze(selected.expect("library witness"), 1)
                    .expect("release analysis");
                assert!(publication.publication().declarations.iter().any(|row| {
                    row.symbol.as_str().ends_with("/chosen/serve")
                        && row.unit.as_str().ends_with("/src/chosen.rs")
                }));
            } else {
                assert!(selected.is_err(), "manifest release differs from the owner");
            }
        }
    }

    #[test]
    fn runtime_publication_preserves_exact_owner_and_document_origin() {
        let runtime = rift_protocol::read::RuntimeIdentity {
            runtime: "cpython".to_owned(),
            version: "3.14.3".to_owned(),
        };
        let owner = runtime.owner().expect("runtime owner");
        let origin = ContributionOrigin::new(
            Some(SourceLocation::Stdlib {
                runtime: Some(runtime.clone()),
            }),
            SourceKind::Authored,
        )
        .expect("exact runtime origin");
        let source_language = language(ShippedLanguage::Python);
        let path = ProjectPath::new("sys/__init__.pyi").expect("runtime unit");
        let readme_path = ProjectPath::new("README.md").expect("runtime documentation");
        let source = "def exit(status: int) -> None: ...\n";
        let readme = "# Runtime\n\nExact selected runtime documentation.\n";
        let files = [
            PackageSource::new(&path, source),
            PackageSource::new(&readme_path, readme),
        ];
        let input = ExactPackageInput::new(
            &owner,
            &source_language,
            &origin,
            &files,
            ExactPackageLimits::new(
                2,
                u64::try_from(source.len() + readme.len()).expect("source bytes"),
            ),
        )
        .expect("runtime input");
        let analysis = PackageAnalyzer::analyze(input, 1).expect("runtime publication");
        let publication = analysis.publication();
        assert_eq!(publication.owner, owner);
        assert_eq!(publication.units.len(), 2);
        assert!(
            publication.units.iter().any(|unit| {
                unit.unit.as_str() == "rift://source/stdlib/cpython/sys/__init__.pyi"
            })
        );
        assert!(!publication.objects.is_empty());
        assert!(publication.objects.iter().all(|object| {
            object.origin.package.is_none() && object.origin.runtime.as_ref() == Some(&runtime)
        }));
        assert!(!publication.documents.is_empty());
        assert!(
            publication
                .documents
                .iter()
                .all(|document| document.owner == owner)
        );
        assert!(publication.documentation.sources.iter().any(|source| {
            source.origin.package.is_none()
                && source.origin.runtime.as_ref() == Some(&runtime)
                && matches!(&source.identity.source,
                    DocumentationSourceIdentity::Package { unit }
                    if unit.as_str() == "rift://source/stdlib/cpython/README.md")
        }));
    }

    #[test]
    fn package_failures_keep_registered_identity_and_ambient_context() {
        let package = identity();
        let child = RiftError::new(
            rift_error::ErrorSlug::new("rift.analysis.documentation_limit_exceeded"),
            "documentation limit exceeded",
            "reduce documentation and retry",
            vec![],
        );
        let provider = errors::analysis::package_provider_failed()
            .package(package_label(&package.owner().expect("fixture owner")))
            .cause(child)
            .error();
        assert_eq!(
            provider.slug().as_str(),
            "rift.analysis.package_provider_failed"
        );
        assert!(
            provider
                .context()
                .any(|(key, value)| key == "package" && value == "cargo/beacon")
        );
        let child = std::error::Error::source(&provider)
            .and_then(|source| source.downcast_ref::<RiftError>())
            .expect("provider failure retains registered child");
        assert_eq!(
            child.slug().as_str(),
            "rift.analysis.documentation_limit_exceeded"
        );

        let path = super::CoreProjectPath::new("src/lib.rs").expect("path");
        let syntax = RiftError::new(
            rift_error::ErrorSlug::new("rift.syntax.parse_cancelled"),
            "parse cancelled",
            "retry",
            vec![],
        );
        let wrapped = syntax
            .with(rift_error::ErrorContext::new(
                "package",
                rift_error::ErrorValue::formatted(package_label(
                    &package.owner().expect("fixture owner"),
                )),
            ))
            .with(rift_error::ErrorContext::new(
                "path",
                rift_error::ErrorValue::path(path.as_str()),
            ));
        assert_eq!(wrapped.slug().as_str(), "rift.syntax.parse_cancelled");
        assert!(
            wrapped
                .context()
                .any(|(key, value)| key == "path" && value == "src/lib.rs")
        );

        let unsupported = super::parsed_file(
            PackageSource::new(&ProjectPath::new("guide.unknown").expect("path"), "source"),
            &package.owner().expect("fixture owner"),
            &language(ShippedLanguage::Rust),
            SyntaxLimits::default(),
        )
        .expect_err("unsupported source extension");
        assert_eq!(
            unsupported.slug().as_str(),
            "rift.analysis.package_syntax_unavailable"
        );
    }

    #[test]
    fn a_package_past_the_declaration_bound_names_the_limit() {
        use std::fmt::Write as _;

        let per_file = rift_provider::CONTRIBUTIONS_PER_PROVIDER_MAX_DEFAULT / 2 + 1;
        let source = (0..per_file).fold(String::new(), |mut source, index| {
            writeln!(source, "def f{index}():\n    pass").expect("a string write succeeds");
            source
        });
        let files = vec![("pkg/a.py", source.as_str()), ("pkg/b.py", source.as_str())];
        let raised = rift_syntax::SyntaxLimits::new(8 << 20, 4_000_000, 512).expect("bounds");
        let mut parsed: Option<crate::PackageSyntax> = None;
        let mut provider_calls = 0;
        let mut reused_files = 0;
        let result = super::fixture::package_result_with_syntax(
            ShippedLanguage::Python,
            files,
            Some(raised),
            None,
            |source| {
                if let Some(syntax) = &parsed {
                    assert!(source.accepts(syntax));
                    reused_files += 1;
                    return Some(syntax.clone());
                }
                let syntax = source.parse().expect("fixture syntax");
                assert_eq!(syntax.facts().symbols().len(), per_file + 1);
                provider_calls += 1;
                parsed = Some(syntax.clone());
                Some(syntax)
            },
        );
        assert_eq!(provider_calls, 1);
        assert_eq!(reused_files, 1);
        let Err(error) = result else {
            panic!("a package past the declaration bound must be refused");
        };
        assert_eq!(
            error.slug().as_str(),
            "rift.analysis.package_declarations_exceeded"
        );
        assert!(
            error
                .context()
                .any(|(key, value)| key == "path" && value == "pkg/b.py")
        );
    }

    #[test]
    fn caller_syntax_limits_replace_each_provider_declared_bounds() {
        let generated = format!("values = [{}]\n", "1, ".repeat(300_000));
        let files = vec![("pkg/generated.py", generated.as_str())];
        let Err(declared) = package_result(ShippedLanguage::Python, files.clone(), None) else {
            panic!("a source past the declared syntax element bound must be refused");
        };
        assert!(declared.slug().as_str().starts_with("rift.syntax."));

        let raised = rift_syntax::SyntaxLimits::new(4 << 20, 1_000_000, 512).expect("bounds");
        assert!(package_result(ShippedLanguage::Python, files, Some(raised)).is_ok());

        let tight = rift_syntax::SyntaxLimits::new(4 << 20, 1, 512).expect("bounds");
        let small = vec![("pkg/small.py", "value = 1\n")];
        assert!(package_result(ShippedLanguage::Python, small.clone(), None).is_ok());
        assert!(package_result(ShippedLanguage::Python, small, Some(tight)).is_err());
    }

    #[test]
    fn plain_text_package_source_keeps_text_language_and_documentation() {
        let publication = analyzed(
            ShippedLanguage::Rust,
            vec![("guide.txt", "Package documentation.\n")],
        );
        let document = publication
            .documents
            .iter()
            .find(|document| document.kind == PackageDocumentKind::File)
            .expect("text file document");
        assert_eq!(document.language.name, "text");
        let documentation = &publication.documentation;
        assert_eq!(documentation.sources[0].media_type, "text/plain");
        assert_eq!(documentation.blocks.len(), 1);
    }

    /// The `identity` of every document of one kind, in publication order.
    fn document_identities(
        publication: &rift_protocol::index::PackagePublication,
        kind: PackageDocumentKind,
    ) -> Vec<&str> {
        publication
            .documents
            .iter()
            .filter(|document| document.kind == kind)
            .map(|document| document.identity.as_str())
            .collect()
    }

    #[test]
    fn test_rust_package_publishes_units_symbols_and_public_documents() {
        let publication = analyzed(
            ShippedLanguage::Rust,
            vec![("src/lib.rs", "pub fn spawn() {}\nfn hidden() {}\n")],
        );

        assert_eq!(
            publication.format_revision,
            PACKAGE_PUBLICATION_FORMAT_REVISION
        );
        assert_eq!(publication.units.len(), 1);
        assert_eq!(publication.units[0].path.0, "src/lib.rs");
        assert!(publication.units[0].source_complete);
        let named: Vec<&str> = publication
            .objects
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect();
        assert_eq!(named, ["beacon", "hidden", "spawn"]);
        let public: Vec<&str> = publication
            .objects
            .iter()
            .filter(|symbol| symbol.visibility.as_deref() == Some("pub"))
            .map(|symbol| symbol.name.as_str())
            .collect();
        assert_eq!(public, ["spawn"], "the export rule keeps `pub` alone");
        assert_eq!(
            document_identities(&publication, PackageDocumentKind::Symbol),
            ["rift://symbol/cargo/crates.io/beacon/rust/beacon/spawn"],
            "a document stands for every public declaration and no other"
        );
        assert_eq!(
            document_identities(&publication, PackageDocumentKind::File),
            ["rift://source/cargo/crates.io/beacon/src/lib.rs"]
        );
        assert_eq!(
            publication.revision.to_string(),
            "rift://revision/cargo/crates.io/beacon@1.0.0",
            "the release lives on the publication, never in its ids"
        );
        let kinds = publication
            .declarations
            .iter()
            .map(|declaration| declaration.syntax_element_kind.as_deref())
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [
                Some("source_file"),
                Some("function_item"),
                Some("function_item")
            ],
            "each declaration names its item syntax element's grammar kind"
        );
    }

    #[test]
    fn test_rust_publication_keeps_inherent_member_visibility_in_both_orders() {
        let definition = "pub enum Task { Ready }";
        let implementation = "impl Task { fn helper(&self) {} }";
        for items in [[definition, implementation], [implementation, definition]] {
            let source = format!(
                "{}\n{}\npub trait Run {{ fn execute(&self); }}\n",
                items[0], items[1]
            );
            let publication = analyzed(ShippedLanguage::Rust, vec![("src/lib.rs", &source)]);
            let documents = document_identities(&publication, PackageDocumentKind::Symbol);
            for (name, expected_public) in [
                ("Task/helper", false),
                ("Task/Ready", true),
                ("Run/execute", true),
            ] {
                let symbol = format!("rift://symbol/cargo/crates.io/beacon/rust/beacon/{name}");
                let declaration = publication
                    .declarations
                    .iter()
                    .find(|declaration| declaration.symbol.as_str() == symbol)
                    .expect("source declaration is retained");
                assert_eq!(declaration.public, expected_public, "{name} in {source}");
                assert_eq!(
                    documents.contains(&symbol.as_str()),
                    expected_public,
                    "{name} in {source}"
                );
            }
        }
    }

    #[test]
    fn test_symbol_document_carries_the_fields_a_ranking_reads() {
        let publication = analyzed(
            ShippedLanguage::Rust,
            vec![(
                "src/lib.rs",
                "/// Spawns a task.\npub fn spawn_task(count: u8) {}\n",
            )],
        );

        let document = publication
            .documents
            .iter()
            .find(|document| document.kind == PackageDocumentKind::Symbol)
            .expect("the public declaration has a document");
        assert_eq!(document.name, "spawn_task");
        assert_eq!(document.qualified_name.as_deref(), Some("spawn_task"));
        assert_eq!(
            document.identifier_terms,
            ["spawn", "task"],
            "the terms are the words the names split into; the names themselves are \
             their own fields"
        );
        assert!(
            document
                .documentation
                .as_deref()
                .is_some_and(|text| text.contains("Spawns a task")),
            "{document:?}"
        );
        assert!(
            document
                .declaration_source
                .as_deref()
                .is_some_and(|source| source.contains("pub fn spawn_task")),
            "{document:?}"
        );
        assert!(
            document.file_content.is_none(),
            "a symbol document ranks the declaration, not the file"
        );
    }

    #[test]
    fn test_attached_comment_publishes_exact_range_and_symbol_owner() {
        let source = "/// Spawns a task.\npub fn spawn_task() {}\n";
        let analysis = package_analysis(ShippedLanguage::Rust, vec![("src/lib.rs", source)]);
        let index = &analysis.publication().documentation;
        let block = index.blocks.first().expect("attached comment block");
        let expected_end = u64::try_from("/// Spawns a task.\n".len()).expect("fixture length");
        let expected_symbol = rift_protocol::identity::SymbolIdentity::new(
            super::fixture::identity().owner().expect("fixture owner"),
            ShippedLanguage::Rust.language(),
            vec!["beacon".to_owned(), "spawn_task".to_owned()],
        )
        .expect("current logical identity")
        .wire_identity();

        assert_eq!(
            block.kind,
            rift_protocol::documentation::DocumentationBlockKind::Prose
        );
        assert_eq!(
            block.range,
            rift_protocol::read::TextRange {
                start: 0,
                end: expected_end
            }
        );
        assert_eq!(
            &source[usize::try_from(block.range.start).expect("range start")
                ..usize::try_from(block.range.end).expect("range end")],
            "/// Spawns a task.\n"
        );
        assert_eq!(
            block.symbol.as_ref().map(|symbol| symbol.0.as_str()),
            Some(expected_symbol.as_str())
        );
        assert_eq!(
            block.chunks[0].identity,
            "rift://source/cargo/crates.io/beacon/src/lib.rs"
        );
    }

    /// Every shipped package language reaches the publication through the same pass,
    /// each under its own export rule: JavaScript exports what `export` wraps, while
    /// TypeScript and TSX drop a `private` member.
    #[test]
    fn test_every_package_language_publishes_its_public_declarations() {
        let cases = [
            (
                ShippedLanguage::JavaScript,
                "index.js",
                "export function open() {}\nfunction helper() {}\n",
                vec!["open"],
            ),
            (
                ShippedLanguage::TypeScript,
                "index.ts",
                "export class Client {\n  private secret(): void {}\n  open(): void {}\n}\n",
                vec!["Client", "Client.open"],
            ),
            (
                ShippedLanguage::TypeScriptTsx,
                "index.tsx",
                "export class Panel {\n  private state(): void {}\n  render(): null {\n    return null;\n  }\n}\n",
                vec!["Panel", "Panel.render"],
            ),
        ];
        for (shipped, path, source, expected) in cases {
            assert_eq!(public_names(shipped, path, source), expected, "{path}");
            let publication = analyzed(shipped, vec![(path, source)]);
            assert_eq!(publication.units.len(), 1, "{path}");
            assert!(!publication.objects.is_empty(), "{path}");
        }
    }

    /// A dialect's identity segment carries a colon (`typescript:tsx`), which `ExactKind`'s
    /// pattern refuses, so a declaration's kind is the provider's own word and the language
    /// rides in `presentation.language`. The whole publication then validates against the
    /// package index schema Rift serves.
    #[test]
    fn test_a_tsx_declaration_kind_is_the_provider_word_the_schema_accepts() {
        let source = "export class Panel {\n  render(): null {\n    return null;\n  }\n}\nexport function mount(): void {}\n";
        let analysis =
            package_analysis(ShippedLanguage::TypeScriptTsx, vec![("index.tsx", source)]);
        let kinds: Vec<(&str, &str)> = analysis.files()[0]
            .file()
            .syntax()
            .symbols()
            .iter()
            .map(|symbol| (symbol.qualified_name.as_str(), symbol.kind))
            .collect();
        assert_eq!(
            kinds,
            [
                ("Panel", "class"),
                ("Panel.render", "method"),
                ("mount", "function")
            ]
        );
        let publication = analysis.publication();
        let mut object_kinds: Vec<(&str, &str)> = publication
            .objects
            .iter()
            .map(|symbol| (symbol.name.as_str(), symbol.kind.0.as_str()))
            .collect();
        object_kinds.sort_unstable();
        assert_eq!(
            object_kinds,
            [
                ("Panel", "class"),
                ("mount", "function"),
                ("render", "method")
            ]
        );
        assert!(
            publication
                .objects
                .iter()
                .all(|symbol| symbol.language == language(ShippedLanguage::TypeScriptTsx))
        );
        let schema: serde_json::Value =
            serde_json::from_str(&package_index_schema_document()).expect("schema parses");
        let validator = jsonschema::validator_for(&schema).expect("schema compiles");
        let instance = serde_json::to_value(publication).expect("publication serializes");
        let refusals: Vec<String> = validator
            .iter_errors(&instance)
            .map(|refusal| format!("{}: {refusal}", refusal.instance_path()))
            .collect();
        assert!(
            refusals.is_empty(),
            "TSX publication validates: {refusals:#?}"
        );
    }

    /// The public names of the one file `path` holds, analyzed alone under `shipped`.
    fn public_names(shipped: ShippedLanguage, path: &str, source: &str) -> Vec<String> {
        let analysis = package_analysis(shipped, vec![(path, source)]);
        analysis
            .files()
            .iter()
            .flat_map(|file| {
                file.file()
                    .syntax()
                    .symbols()
                    .iter()
                    .filter(move |symbol| file.is_public(&symbol.qualified_name))
                    .map(|symbol| symbol.qualified_name.clone())
            })
            .collect()
    }

    #[test]
    fn test_cython_protocol_methods_use_the_actual_class_scope() {
        let source = r"def __module__(): pass

class Session:
    def __enter__(self): return self
    def __mangled(self): pass

    def run(self):
        def __nested__(): pass
        return __nested__()

class _Private:
    def visible(self): pass
    def __exit__(self, *args): pass
";
        let analysis = package_analysis(ShippedLanguage::Cython, vec![("mod.pyx", source)]);
        let held = &analysis.files()[0];
        let symbols = held.file().syntax().symbols();
        for (name, public) in [
            ("__module__", false),
            ("Session", true),
            ("Session::__enter__", true),
            ("Session::__mangled", false),
            ("Session::run", true),
            ("Session::__nested__", false),
            ("_Private", false),
            ("_Private::visible", false),
            ("_Private::__exit__", false),
        ] {
            assert!(
                symbols.iter().any(|symbol| symbol.qualified_name == name),
                "{name}"
            );
            assert_eq!(held.is_public(name), public, "{name}");
        }
    }

    /// An unpaired JavaScript module exports what an ES `export` wraps or names, a default
    /// export included, and the members of an exported class other than a `#` private
    /// element. A helper no export wraps or names stays private.
    #[test]
    fn test_an_unpaired_javascript_module_exports_what_export_wraps_or_names() {
        let source = "export function open() {}\n\
                      function helper() {}\n\
                      export class Client {\n  connect() {}\n  static make() {}\n  #hidden() {}\n}\n\
                      class Internal {\n  run() {}\n}\n\
                      export const answer = 42, other = 1;\n\
                      export default function main() {}\n\
                      function later() {}\n\
                      class Session {\n  close() {}\n  #token() {}\n}\n\
                      export { later, Session as Connection };\n";
        let mut public = public_names(ShippedLanguage::JavaScript, "index.js", source);
        public.sort();
        assert_eq!(
            public,
            [
                "Client",
                "Client.connect",
                "Client.make",
                "Session",
                "Session.close",
                "answer",
                "later",
                "main",
                "open",
                "other"
            ]
        );
        for shipped in [ShippedLanguage::TypeScript, ShippedLanguage::TypeScriptTsx] {
            let module_public = public_names(shipped, "lib/index.mjs", source).len();
            assert_eq!(
                module_public,
                public.len(),
                "a TypeScript package's JavaScript build takes the same rule"
            );
        }
    }

    /// Export assignments retain their named declarations, exported class members other
    /// than private fields, and exported object methods. An additional captured function
    /// assignment remains in the declaration inventory without changing this public set.
    #[test]
    fn test_a_module_exports_assignment_exports_the_declarations_it_names() {
        let source = "function helper() {}\n\
                      class Runner {\n  run() {}\n  #secret() {}\n}\n\
                      function internal() {}\n\
                      function load() {}\n\
                      module.exports = { helper, Runner, start() {} };\n\
                      exports.read = load;\n\
                      exports.extra = function extra() {};\n";
        let analysis = package_analysis(ShippedLanguage::JavaScript, vec![("index.cjs", source)]);
        let mut declared: Vec<(&str, bool)> = analysis
            .files()
            .iter()
            .flat_map(|held| {
                held.file().syntax().symbols().iter().map(move |symbol| {
                    (
                        symbol.qualified_name.as_str(),
                        held.is_public(&symbol.qualified_name),
                    )
                })
            })
            .collect();
        declared.sort_unstable();
        assert_eq!(
            declared,
            [
                ("Runner", true),
                ("Runner.#secret", false),
                ("Runner.run", true),
                ("default", true),
                ("default.Runner", true),
                ("default.helper", true),
                ("default.start", true),
                ("extra", false),
                ("helper", true),
                ("internal", false),
                ("load", true),
            ]
        );
    }

    /// The public set of express's three files: a synchronized function export, a module
    /// value named by an alias, a constructor with prototype methods, and named exports of
    /// values. A helper and a function's local stay private.
    #[test]
    fn test_commonjs_modules_export_module_values_members_and_assigned_values() {
        for (path, source, expected) in [
            (
                "lib/express.js",
                "var proto = require('./application');\n\
                 var Router = require('./router');\n\
                 var unused = require('./unused');\n\
                 exports = module.exports = createApplication;\n\
                 function createApplication() { var app = {}; return app; }\n\
                 exports.application = proto;\n\
                 exports.Route = Router.Route;\n\
                 exports.Router = Router;\n\
                 exports.json = function json() {};\n\
                 exports.static = require('serve-static');\n",
                vec![
                    "Route",
                    "Router",
                    "createApplication",
                    "json",
                    "proto",
                    "static",
                ],
            ),
            (
                "lib/application.js",
                "var app = exports = module.exports = {};\n\
                 var trust = '@@trust';\n\
                 app.init = function init() { this.cache = {}; };\n\
                 app.set = function set(value) { var local = value; return helper(local); };\n\
                 function helper(value) { return value }\n",
                vec!["app", "app.init", "app.set"],
            ),
            (
                "lib/view.js",
                "module.exports = View;\n\
                 function View(name) { var local = 1; this.name = name; this.size = local; }\n\
                 View.prototype.lookup = function lookup() { var found = 1; return found; };\n\
                 View.make = function make() {};\n\
                 function Other() {}\n\
                 Other.prototype.run = function run() {};\n",
                vec!["View", "View.lookup", "View.make", "View.name", "View.size"],
            ),
        ] {
            let analysis = package_analysis(ShippedLanguage::JavaScript, vec![(path, source)]);
            let held = &analysis.files()[0];
            let mut public = held
                .file()
                .syntax()
                .symbols()
                .iter()
                .map(|symbol| symbol.qualified_name.as_str())
                .filter(|name| held.is_public(name))
                .collect::<Vec<_>>();
            public.sort_unstable();
            public.dedup();
            assert_eq!(public, expected, "{path}");
        }
    }

    /// A paired JavaScript module keeps the public set its declaration file defines: an
    /// `export` the stub does not declare leaves the set.
    #[test]
    fn test_a_paired_javascript_module_keeps_the_stub_public_set() {
        let analysis = package_analysis(
            ShippedLanguage::TypeScript,
            vec![
                ("index.d.ts", "export declare function open(): void;\n"),
                (
                    "index.js",
                    "export function open() {}\nexport function internal() {}\n",
                ),
            ],
        );
        let mut public: Vec<&str> = analysis
            .files()
            .iter()
            .flat_map(|file| {
                file.file()
                    .syntax()
                    .symbols()
                    .iter()
                    .filter(|symbol| file.is_public(&symbol.qualified_name))
                    .map(|symbol| symbol.qualified_name.as_str())
            })
            .collect();
        public.sort_unstable();
        public.dedup();
        assert_eq!(public, ["open"]);
        assert!(analysis.files().iter().any(|file| {
            file.file().syntax().symbols().iter().any(|symbol| {
                symbol.qualified_name == "internal" && !file.is_public(&symbol.qualified_name)
            })
        }));
        assert!(
            analysis
                .publication()
                .objects
                .iter()
                .any(|symbol| symbol.name == "internal")
        );
    }

    #[test]
    fn test_equal_package_bytes_produce_an_equal_publication() {
        let source = vec![("src/lib.rs", "pub fn spawn() {}\n")];
        let first = analyzed(ShippedLanguage::Rust, source.clone());
        let second = analyzed(ShippedLanguage::Rust, source);

        assert_eq!(first, second);
        assert_eq!(
            canonical_json(&first).expect("canonical"),
            canonical_json(&second).expect("canonical")
        );
    }

    #[test]
    fn test_input_file_order_does_not_change_the_publication() {
        let forward = analyzed(
            ShippedLanguage::Rust,
            vec![
                ("src/a.rs", "pub fn alpha() {}\n"),
                ("src/b.rs", "pub fn beta() {}\n"),
            ],
        );
        let reversed = analyzed(
            ShippedLanguage::Rust,
            vec![
                ("src/b.rs", "pub fn beta() {}\n"),
                ("src/a.rs", "pub fn alpha() {}\n"),
            ],
        );

        assert_eq!(
            canonical_json(&forward).expect("canonical"),
            canonical_json(&reversed).expect("canonical")
        );
    }

    #[test]
    fn test_changing_one_declaration_changes_only_its_own_records() {
        let before = analyzed(
            ShippedLanguage::Rust,
            vec![
                ("src/lib.rs", "mod a; mod b;\n"),
                ("src/a.rs", "pub fn alpha() {}\n"),
                ("src/b.rs", "pub fn beta() {}\n"),
            ],
        );
        let after = analyzed(
            ShippedLanguage::Rust,
            vec![
                ("src/lib.rs", "mod a; mod b;\n"),
                ("src/a.rs", "pub fn alpha(count: u8) {}\n"),
                ("src/b.rs", "pub fn beta() {}\n"),
            ],
        );

        let changed_units: Vec<&str> = before
            .units
            .iter()
            .zip(&after.units)
            .filter(|(before, after)| before.digest != after.digest)
            .map(|(before, _)| before.path.0.as_str())
            .collect();
        assert_eq!(changed_units, ["src/a.rs"]);
        let changed_symbols: Vec<&str> = before
            .declarations
            .iter()
            .filter(|original| {
                after
                    .declarations
                    .iter()
                    .find(|record| original.symbol == record.symbol && original.unit == record.unit)
                    .expect("retained original declaration")
                    .digest
                    != original.digest
            })
            .map(|original| original.symbol.as_str())
            .collect();
        assert_eq!(
            changed_symbols,
            [
                "rift://symbol/cargo/crates.io/beacon/rust/beacon/a",
                "rift://symbol/cargo/crates.io/beacon/rust/beacon/a/alpha",
            ]
        );
        let added = after
            .declarations
            .iter()
            .filter(|record| {
                !before
                    .declarations
                    .iter()
                    .any(|original| original.symbol == record.symbol)
            })
            .collect::<Vec<_>>();
        let [parameter] = added.as_slice() else {
            panic!("the new formal parameter is the sole addition: {added:?}");
        };
        let parsed = rift_protocol::identity::SymbolIdentity::parse(parameter.symbol.as_str())
            .expect("parameter identity");
        assert_eq!(
            parsed.qualified_path(),
            ["beacon", "a", "alpha", "count#parameter0"]
        );
        assert_eq!(parameter.source, "count");

        let changed_documents: Vec<&str> = before
            .documents
            .iter()
            .filter(|original| {
                after
                    .documents
                    .iter()
                    .find(|record| {
                        original.identity == record.identity && original.kind == record.kind
                    })
                    .expect("retained original search document")
                    .digest
                    != original.digest
            })
            .map(|original| original.identity.as_str())
            .collect();
        assert_eq!(
            changed_documents,
            [
                "rift://symbol/cargo/crates.io/beacon/rust/beacon/a/alpha",
                "rift://source/cargo/crates.io/beacon/src/a.rs",
            ],
            "the changed declaration's own document and its file's document move"
        );
        for document in &after.documents {
            if before
                .documents
                .iter()
                .all(|original| original.identity != document.identity)
            {
                assert_eq!(document.identity.as_str(), parameter.symbol.as_str());
            }
        }
        assert_ne!(before.source_digest, after.source_digest);
    }

    /// Two publications compare record by record through their stable identities and
    /// digests, which is what an adapter computing added, replaced, and removed records
    /// reads.
    #[test]
    fn test_publications_compare_by_stable_identity_and_digest() {
        let before = analyzed(
            ShippedLanguage::Rust,
            vec![
                ("src/lib.rs", "mod a;\n"),
                ("src/a.rs", "pub fn alpha() {}\n"),
            ],
        );
        let after = analyzed(
            ShippedLanguage::Rust,
            vec![
                ("src/lib.rs", "mod a; mod b;\n"),
                ("src/a.rs", "pub fn alpha() {}\n"),
                ("src/b.rs", "pub fn beta() {}\n"),
            ],
        );

        let held: Vec<(&str, &str)> = before
            .declarations
            .iter()
            .map(|symbol| (symbol.symbol.0.as_str(), symbol.digest.0.as_str()))
            .collect();
        let arrived: Vec<(&str, &str)> = after
            .declarations
            .iter()
            .map(|symbol| (symbol.symbol.0.as_str(), symbol.digest.0.as_str()))
            .collect();
        let added: Vec<&str> = arrived
            .iter()
            .filter(|(identity, _)| !held.iter().any(|(kept, _)| kept == identity))
            .map(|(identity, _)| *identity)
            .collect();
        assert_eq!(
            added,
            [
                "rift://symbol/cargo/crates.io/beacon/rust/beacon/b",
                "rift://symbol/cargo/crates.io/beacon/rust/beacon/b",
                "rift://symbol/cargo/crates.io/beacon/rust/beacon/b/beta"
            ]
        );
        let unchanged: Vec<&str> = arrived
            .iter()
            .filter(|entry| held.contains(entry))
            .map(|(identity, _)| *identity)
            .collect();
        assert_eq!(
            unchanged,
            [
                "rift://symbol/cargo/crates.io/beacon/rust/beacon/a",
                "rift://symbol/cargo/crates.io/beacon/rust/beacon/a",
                "rift://symbol/cargo/crates.io/beacon/rust/beacon/a/alpha"
            ],
            "an untouched declaration and source facet keep their digests"
        );
    }

    /// A publication addresses source by package-relative path and unit identity, so no
    /// machine's directory layout reaches a consumer.
    #[test]
    fn test_publication_carries_no_host_absolute_path() {
        let publication = analyzed(
            ShippedLanguage::Rust,
            vec![("src/lib.rs", "pub fn spawn() {}\n")],
        );

        let rendered = canonical_json(&publication).expect("canonical");
        assert!(!rendered.contains("/Users/"), "{rendered}");
        assert!(!rendered.contains("\\\\"), "{rendered}");
        assert!(rendered.contains("src/lib.rs"));
    }

    /// The analysis hands back the parsed files beside the publication, so a consumer
    /// reads what the publication was built from rather than parsing again.
    #[test]
    fn test_analysis_carries_the_parsed_files_beside_the_publication() {
        let analysis = analysis(
            ShippedLanguage::Rust,
            vec![("src/lib.rs", "pub fn spawn() {}\n")],
        );

        let files = analysis.files();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].file().path().as_str(), "src/lib.rs");
        assert!(files[0].is_public("spawn"));
        assert_eq!(analysis.publication().objects.len(), 2);
    }

    #[test]
    fn test_notebook_code_cell_uses_package_language_when_cell_language_is_absent() {
        let source =
            r#"{"cells":[{"cell_type":"code","source":"def spawn(): pass\n"}],"metadata":{}}"#;
        let publication = analyzed(
            ShippedLanguage::Python,
            vec![("notebooks/guide.ipynb", source)],
        );
        let document = publication
            .documents
            .iter()
            .find(|document| document.kind == PackageDocumentKind::File)
            .expect("notebook code cell document");

        assert_eq!(document.language.name, "python");
        assert_eq!(
            document.file_content.as_deref(),
            Some("def spawn(): pass\n")
        );
    }

    #[test]
    fn malformed_notebook_omission_keeps_package_documentation_and_code() {
        let publication = analyzed(
            ShippedLanguage::Rust,
            vec![
                (
                    "README.md",
                    "# Package guide\n\nValid package documentation.\n",
                ),
                ("src/lib.rs", "/// Code documentation.\npub fn serve() {}\n"),
                ("notebooks/broken.ipynb", "{"),
            ],
        );
        let documentation = &publication.documentation;

        assert!(
            publication
                .documents
                .iter()
                .all(|document| !document.unit.0.ends_with("/notebooks/broken.ipynb"))
        );
        assert!(documentation.coverage.selected >= 3);
        assert_eq!(documentation.coverage.omitted, 1);
        assert!(documentation.sources.iter().any(|source| {
            matches!(
                &source.identity.source,
                DocumentationSourceIdentity::Package { unit }
                    if unit.0.ends_with("/README.md")
            )
        }));
        assert!(documentation.blocks.iter().any(|block| {
            block.source.source
                == DocumentationSourceIdentity::Package {
                    unit: rift_protocol::read::SourceUnitId(
                        "rift://source/cargo/crates.io/beacon/README.md".to_owned(),
                    ),
                }
        }));
        assert!(
            publication
                .objects
                .iter()
                .any(|symbol| symbol.name == "serve")
        );
        assert!(documentation.warnings.iter().any(|warning| {
            warning.stage == DocumentationStage::Source
                && warning.kind == DocumentationWarningKind::MalformedSource
                && warning.count == 1
                && matches!(
                    &warning.source.source,
                    DocumentationSourceIdentity::Package { unit }
                        if unit.0.ends_with("/notebooks/broken.ipynb")
                )
        }));
    }

    #[test]
    fn long_package_identity_omits_notebook_cell_and_keeps_readme() {
        let mut package = identity();
        package.name = "p".repeat(4_040);
        package.registry = format!("r.example/{}", "a".repeat(4_086));
        let owner = package.owner().expect("bounded canonical owner");
        let language = language(ShippedLanguage::Rust);
        let origin = ContributionOrigin::new(
            Some(SourceLocation::Dependency {
                package: package.clone(),
            }),
            SourceKind::Authored,
        )
        .expect("package origin");
        let readme_path = ProjectPath::new("README.md").expect("README path");
        let notebook_path = ProjectPath::new("notebooks/guide.ipynb").expect("notebook path");
        let readme = "# Package guide\n\nValid package documentation.\n";
        let notebook = r#"{"cells":[{"cell_type":"markdown","id":"guide","source":"Notebook text."}],"metadata":{}}"#;
        let files = [
            PackageSource::new(&readme_path, readme),
            PackageSource::new(&notebook_path, notebook),
        ];
        let unit = rift_core::SourceUnitId::for_package(&package, &notebook_path)
            .expect("package source unit accepts exact package name and path");
        assert_eq!(
            unit.to_string(),
            format!(
                "rift://source/cargo/r.example%2F{}/{}/notebooks/guide.ipynb",
                "a".repeat(4_086),
                package.name
            )
        );
        let byte_limit = u64::try_from(readme.len() + notebook.len()).expect("byte count");
        let input = ExactPackageInput::new(
            &owner,
            &language,
            &origin,
            &files,
            ExactPackageLimits::new(2, byte_limit),
        )
        .expect("exact package input accepts bounded package identity");
        let analysis = PackageAnalyzer::analyze(input, 1).expect("package analysis");
        let publication = analysis.publication();
        let notebook_source = DocumentationContentIdentity {
            source: DocumentationSourceIdentity::Package {
                unit: rift_protocol::read::SourceUnitId(unit.to_string()),
            },
            cell: Some(rift_protocol::documentation::NotebookCell {
                identity: rift_protocol::documentation::NotebookCellIdentity::Authored {
                    id: "guide".to_owned(),
                },
                kind: rift_protocol::documentation::NotebookCellKind::Markdown,
            }),
        };

        assert_eq!(publication.documentation.coverage.selected, 2);
        assert_eq!(publication.documentation.coverage.parsed, 1);
        assert_eq!(publication.documentation.coverage.omitted, 1);
        assert!(publication.documentation.blocks.iter().any(|block| {
            matches!(
                &block.source.source,
                DocumentationSourceIdentity::Package { unit }
                    if unit.0.ends_with("/README.md")
            )
        }));
        assert!(
            !publication
                .documentation
                .sources
                .iter()
                .any(|source| source.identity == notebook_source)
        );
        assert!(
            publication
                .documents
                .iter()
                .all(|document| document.unit.0 != unit.to_string())
        );
        assert!(publication.documentation.warnings.iter().any(|warning| {
            warning.source == notebook_source
                && warning.stage == DocumentationStage::Source
                && warning.kind == DocumentationWarningKind::SourceUnavailable
                && warning.count == 1
        }));
    }

    #[test]
    fn oversized_selected_text_source_is_omitted_with_other_docs_retained() {
        let oversized =
            "x".repeat(rift_protocol::documentation::DOCUMENTATION_SOURCE_BYTES_MAX as usize + 1);
        let publication = analyzed(
            ShippedLanguage::Rust,
            vec![
                (
                    "README.md",
                    "# Package guide\n\nValid package documentation.\n",
                ),
                ("docs/large.txt", &oversized),
            ],
        );
        let documentation = &publication.documentation;

        assert_eq!(documentation.coverage.omitted, 1);
        assert!(documentation.sources.iter().any(|source| {
            matches!(
                &source.identity.source,
                DocumentationSourceIdentity::Package { unit }
                    if unit.0.ends_with("/README.md")
            )
        }));
        assert!(documentation.warnings.iter().any(|warning| {
            warning.stage == DocumentationStage::Source
                && warning.kind == DocumentationWarningKind::SourceUnavailable
                && matches!(
                    &warning.source.source,
                    DocumentationSourceIdentity::Package { unit }
                        if unit.0.ends_with("/docs/large.txt")
                )
        }));
    }

    #[test]
    fn package_markdown_and_rst_near_path_bound_keep_documentation() {
        let markdown_path = format!("{}README.md", "a".repeat(982));
        let rst_path = format!("{}guide.rst", "b".repeat(982));
        let attached_path = format!("{}lib.rs", "c".repeat(982));
        ProjectPath::new(&markdown_path).expect("bounded Markdown path");
        ProjectPath::new(&rst_path).expect("bounded RST path");
        ProjectPath::new(&attached_path).expect("bounded Rust path");
        let manifest = format!(
            "[package]\nname = \"beacon\"\nversion = \"1.0.0\"\n[lib]\npath = \"{attached_path}\"\n"
        );
        let analysis = super::fixture::package_result_with_metadata(
            ShippedLanguage::Rust,
            vec![
                (&markdown_path, "# Package guide\n\nMarkdown retained.\n"),
                (&rst_path, "Package guide\n=============\n\nRST retained.\n"),
                (
                    &attached_path,
                    "/// Attached retained.\npub fn serve() {}\n",
                ),
            ],
            None,
            Some(("Cargo.toml", &manifest)),
        )
        .expect("captured bounded library root");
        let publication = analysis.publication();
        let documentation = &publication.documentation;
        let markdown_unit = format!("rift://source/cargo/crates.io/beacon/{markdown_path}");
        let rst_unit = format!("rift://source/cargo/crates.io/beacon/{rst_path}");
        let attached_unit = format!("rift://source/cargo/crates.io/beacon/{attached_path}");

        assert_eq!(documentation.coverage.selected, 3);
        assert_eq!(documentation.coverage.parsed, 3);
        assert_eq!(documentation.coverage.omitted, 0);
        assert!(documentation.sources.iter().any(|source| {
            matches!(
                &source.identity.source,
                DocumentationSourceIdentity::Package { unit }
                    if unit.0 == markdown_unit
            )
        }));
        assert!(documentation.sources.iter().any(|source| {
            matches!(
                &source.identity.source,
                DocumentationSourceIdentity::Package { unit }
                    if unit.0 == rst_unit
            )
        }));
        assert!(documentation.sources.iter().any(|source| {
            matches!(
                &source.identity.source,
                DocumentationSourceIdentity::Package { unit }
                    if unit.0 == attached_unit
            )
        }));
        assert!(documentation.blocks.iter().any(|block| {
            matches!(
                &block.source.source,
                DocumentationSourceIdentity::Package { unit }
                    if unit.0.ends_with("README.md")
            )
        }));
        assert!(documentation.blocks.iter().any(|block| {
            matches!(
                &block.source.source,
                DocumentationSourceIdentity::Package { unit }
                    if unit.0.ends_with("guide.rst")
            )
        }));
        assert!(documentation.blocks.iter().any(|block| {
            block.symbol.is_some()
                && matches!(
                    &block.source.source,
                    DocumentationSourceIdentity::Package { unit }
                        if unit.0 == attached_unit
                )
        }));
    }

    /// A declaration past the retained-source bound keeps the bytes that fit, reports the
    /// count it dropped, and states that its source is not the whole declaration.
    #[test]
    fn test_a_declaration_past_the_source_bound_is_cut_and_reported() {
        // Three-byte characters, padded so the bound falls inside one: the cut then walks
        // back to the boundary rather than splitting the character.
        let mut prefix = String::from("pub fn spawn() {\n    // ");
        while (bound(PACKAGE_SOURCE_BYTES_MAX) - prefix.len()).is_multiple_of("\u{4e16}".len()) {
            prefix.push(' ');
        }
        let filler = "\u{4e16}".repeat(bound(PACKAGE_SOURCE_BYTES_MAX));
        let source = format!("{prefix}{filler}\n}}\n");
        assert!(
            !source.is_char_boundary(bound(PACKAGE_SOURCE_BYTES_MAX)),
            "the bound falls inside a character"
        );

        let publication = analyzed(ShippedLanguage::Rust, vec![("src/lib.rs", &source)]);

        let object = publication
            .objects
            .iter()
            .find(|object| object.name == "spawn")
            .expect("the declaration is published");
        let symbol = publication
            .declarations
            .iter()
            .find(|binding| Some(&binding.symbol) == object.id.as_ref())
            .expect("the declaration binding is published");
        assert!(!symbol.source_complete, "{symbol:?}");
        assert!(
            symbol.source.len() <= bound(PACKAGE_SOURCE_BYTES_MAX),
            "the retained source fits the bound: {}",
            symbol.source.len()
        );
        assert!(
            symbol.source.len() > bound(PACKAGE_SOURCE_BYTES_MAX) - 4,
            "the cut walks back to a character boundary, not further: {}",
            symbol.source.len()
        );
        let dropped: Vec<&PackageAnalysisWarning> = publication
            .warnings
            .iter()
            .filter(|warning| matches!(warning, PackageAnalysisWarning::SourceTruncated { .. }))
            .collect();
        assert!(!dropped.is_empty(), "{:?}", publication.warnings);
    }

    /// The ranking terms stop at their bound: a name splitting into more words than a
    /// document may carry ranks on the ones that fit.
    #[test]
    fn test_identifier_terms_stop_at_their_bound() {
        let terms_max = bound(PACKAGE_IDENTIFIER_TERMS_MAX);
        let name: String = (0..=terms_max)
            .map(|index| format!("w{index}"))
            .collect::<Vec<String>>()
            .join("_");

        let terms = super::identifier_terms(&[&name]);

        assert_eq!(terms.len(), terms_max);
        assert_eq!(terms[0], "w0");
    }
}
