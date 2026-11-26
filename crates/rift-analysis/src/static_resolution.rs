//! Resolves captured call and import sites against admitted source declarations.

mod external;
mod javascript;
mod javascript_callbacks;
mod javascript_edges;
mod javascript_exports;
mod javascript_member_callbacks;
mod javascript_members;
mod javascript_modules;
mod javascript_parameters;
mod javascript_reexports;
mod javascript_scopes;
mod python_edges;
mod rust;
#[cfg(test)]
mod tests;

use javascript_parameters::{ParameterReads, ScopedParameterReads};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, OnceLock};

use rift_core::{
    ContributionReference, DeclarationBinding, FileDigest, LoopBudget, ProjectPath, ProviderId,
    ProviderSymbolId, ReferenceRole, SourceRange,
};
use rift_protocol::read::SymbolKind;
use rift_provider::{NormalizedGraph, NormalizedTarget};
use rift_syntax::{
    ByteRange, DocumentPlacement, PythonDeletion, SYNTAX_PROVIDER_ID, StaticBinding,
    StaticBindingKind, StaticCall, StaticFacts, StaticParameter, SyntaxFacts, SyntaxSymbol,
};

pub(crate) use external::ExternalOccurrence;
pub(crate) use javascript_exports::is_declaration_file;

use crate::{EnclosingDefinitions, NamespaceInput};

// Containment can use retained declarations without captured call facts.
static EMPTY_STATIC_FACTS: OnceLock<StaticFacts> = OnceLock::new();

/// One file from the same selection as the normalized graph.
#[derive(Debug)]
pub(crate) struct StaticSource<'source> {
    pub(crate) path: &'source ProjectPath,
    pub(crate) source: &'source str,
    pub(crate) facts: &'source SyntaxFacts,
    pub(crate) placement: &'source DocumentPlacement,
}

/// A source occurrence and its established declaration target.
#[derive(Debug)]
pub(crate) struct StaticOccurrence {
    pub(crate) binding: DeclarationBinding,
    pub(crate) target: NormalizedTarget,
    pub(crate) target_binding: Option<DeclarationBinding>,
    pub(crate) source_digest: FileDigest,
    pub(crate) callee_range: ByteRange,
    pub(crate) role: ReferenceRole,
    pub(crate) edge_from: Option<rift_core::SymbolId>,
    pub(crate) edge_kind: Option<rift_protocol::read::EdgeKind>,
}

/// Observations with the coverage of the bounded pass that produced them.
#[derive(Debug, Default)]
pub(crate) struct StaticResolution {
    pub(crate) occurrences: Vec<StaticOccurrence>,
    /// First internal and external occurrence from a pass that uses the capacity left by primary edges.
    pub(crate) supplemental_start: Option<(usize, usize)>,
    /// First internal occurrence added after the earlier supplemental passes.
    pub(crate) late_supplemental_start: Option<usize>,
    /// First reassigned local read, after all earlier reference passes.
    pub(crate) python_reassignment_start: Option<usize>,
    /// First captured local read, after the same-function reads.
    pub(crate) python_capture_start: Option<usize>,
    /// First method receiver read, after earlier local and captured reads.
    pub(crate) python_receiver_start: Option<usize>,
    /// First variadic parameter read, after the earlier parameter reads.
    pub(crate) python_variadic_start: Option<usize>,
    /// First generated initializer keyword reference, after earlier references.
    pub(crate) python_initializer_start: Option<usize>,
    /// First reference inside a compound Python string annotation.
    pub(crate) python_quoted_type_start: Option<usize>,
    pub(crate) python_overload_parameter_start: Option<usize>,
    /// First reference to an authored file module from an ECMAScript module pass.
    pub(crate) file_module_start: Option<usize>,
    /// First qualifier reference after the file module specifiers.
    pub(crate) file_module_qualifier_start: Option<usize>,
    /// First containment edge of a physically scoped literal declaration.
    pub(crate) literal_start: Option<usize>,
    /// Containment awaiting the capacity left by existing references.
    pub(crate) deferred_literal_containment: Vec<StaticOccurrence>,
    /// First callback parameter reference after the previously admitted edge classes.
    pub(crate) callback_parameter_start: Option<usize>,
    /// Callback parameter reads awaiting their place after containment.
    pub(crate) deferred_callback_parameters: Vec<StaticOccurrence>,
    /// First TypeScript overload parameter read after callback parameter reads.
    pub(crate) overload_parameter_start: Option<usize>,
    /// Overload parameter reads awaiting capacity after earlier package edges.
    pub(crate) deferred_overload_parameters: Vec<StaticOccurrence>,
    /// First internal and external occurrence resolved through authored Rust extern crate aliases.
    pub(crate) rust_extern_alias_start: Option<(usize, usize)>,
    /// First occurrence resolved to a Rust receiver parameter.
    pub(crate) rust_receiver_start: Option<usize>,
    /// First occurrence resolved to an authored Rust generic parameter.
    pub(crate) rust_generic_start: Option<usize>,
    /// First occurrence derived from the physical Cargo library root.
    pub(crate) rust_library_root_start: Option<usize>,
    /// First path occurrence resolved to the physical Cargo library root.
    pub(crate) rust_crate_paths_start: Option<usize>,
    /// First restricted visibility occurrence resolved to the Cargo library root.
    pub(crate) rust_crate_visibility_start: Option<usize>,
    /// First occurrence resolved from a Rust argument value.
    pub(crate) rust_argument_value_start: Option<usize>,
    /// First use-path qualifier resolved to a Rust module or type.
    pub(crate) rust_import_qualifier_start: Option<usize>,
    /// First occurrence that reads a checked Rust shorthand pattern binding.
    pub(crate) rust_shorthand_start: Option<usize>,
    /// First occurrence resolved in an explicitly placed shared Rust source.
    pub(crate) rust_shared_start: Option<usize>,
    /// Existing call sites left unresolved in files with authored extern bindings.
    pub(crate) rust_extern_candidates: BTreeMap<rift_core::SourceUnitId, Vec<StaticCall>>,
    /// First JavaScript method parameter read after earlier parameter reads.
    pub(crate) javascript_method_parameter_start: Option<usize>,
    /// JavaScript method parameter reads awaiting capacity after earlier facts.
    pub(crate) deferred_javascript_method_parameters: Vec<StaticOccurrence>,
    /// First parameter read from a separately anchored JavaScript function.
    pub(crate) scoped_javascript_function_start: Option<usize>,
    /// Reads of scoped JavaScript parameters awaiting capacity after earlier facts.
    pub(crate) deferred_scoped_javascript_parameters: Vec<StaticOccurrence>,
    /// First occurrence of a private binding with only a physical scope anchor.
    pub(crate) scoped_javascript_lexical_start: Option<usize>,
    /// Occurrences of separately anchored private bindings after earlier facts.
    pub(crate) deferred_scoped_javascript_lexical: Vec<StaticOccurrence>,
    /// First occurrence of a written TypeScript type parameter after earlier facts.
    pub(crate) typescript_type_parameter_start: Option<usize>,
    /// Type uses awaiting the capacity left by earlier package edges.
    pub(crate) deferred_typescript_type_parameters: Vec<StaticOccurrence>,
    /// First Read of a JavaScript parameter that shares a name with a shorthand property.
    pub(crate) javascript_shorthand_parameter_start: Option<usize>,
    /// Shorthand parameter Reads awaiting capacity after earlier package edges.
    pub(crate) deferred_javascript_shorthand_parameters: Vec<StaticOccurrence>,
    /// Occurrences whose target a dependency or a standard library owns.
    pub(crate) external: Vec<ExternalOccurrence>,
    /// Edge kinds of sites left without a target.
    pub(crate) omitted: BTreeMap<rift_protocol::read::EdgeKind, u64>,
    pub(crate) analyzed_sources: usize,
    pub(crate) unavailable_sources: usize,
    pub(crate) unresolved_sites: usize,
    pub(crate) exhausted: bool,
    /// Whether the pass, the class orders, or any file ended on its work budget.
    pub(crate) ended: bool,
    pub(crate) edge_calls: BTreeMap<rift_core::SourceUnitId, EdgeCallCoverage>,
}

#[derive(Debug)]
pub(crate) struct EdgeCallCoverage {
    pub(crate) language: rift_protocol::read::Language,
    pub(crate) unavailable: bool,
    pub(crate) unresolved: Vec<ByteRange>,
}

impl EdgeCallCoverage {
    fn captured(document: &StaticSource<'_>) -> Self {
        let mut coverage = Self {
            language: document.facts.language().clone(),
            unavailable: admitted_facts(document).is_none(),
            unresolved: Vec::new(),
        };
        if document.facts.has_errors()
            && matches!(
                document.facts.language().name.as_str(),
                "javascript" | "typescript"
            )
        {
            if let Ok(end) = u64::try_from(document.source.len()) {
                coverage.unresolved.push(ByteRange { start: 0, end });
            } else {
                coverage.unavailable = true;
            }
        }
        coverage
    }
}

pub(crate) fn prepare_javascript_reexports(
    input: &NamespaceInput<'_>,
    prepared: &mut crate::analyzer::namespace::PreparedNamespace,
    selected: &[crate::analyzer::namespace::SelectedFile<'_>],
    placements: &[&DocumentPlacement],
    aliases_max: usize,
) {
    javascript_reexports::prepare(input, prepared, selected, placements, aliases_max);
}

/// The coverage of a selection whose static pass has not run: no occurrence, and every
/// supported unit reports unavailable call coverage.
pub(crate) fn deferred(documents: &[StaticSource<'_>]) -> StaticResolution {
    let edge_calls: BTreeMap<_, _> = documents
        .iter()
        .filter(|document| supported_language(document.facts))
        .map(|document| {
            let mut coverage = EdgeCallCoverage::captured(document);
            coverage.unavailable = true;
            (document.placement.unit().clone(), coverage)
        })
        .collect();
    StaticResolution {
        unavailable_sources: edge_calls.len(),
        edge_calls,
        ..StaticResolution::default()
    }
}

/// Runs one static pass over a captured source selection.
pub(crate) fn resolve(
    graph: &NormalizedGraph,
    documents: &[StaticSource<'_>],
    shared: &[StaticSource<'_>],
    namespace: &NamespaceInput<'_>,
    work_max: usize,
) -> StaticResolution {
    resolve_by(
        graph,
        (documents, shared),
        namespace,
        work_max,
        |index, work, resolution| index.resolve_files(FileWork::CAPTURED, work, resolution),
    )
}

/// Runs one static pass whose files `files` resolves against the built index.
fn resolve_by<'source>(
    graph: &'source NormalizedGraph,
    documents: (
        &'source [StaticSource<'source>],
        &'source [StaticSource<'source>],
    ),
    namespace: &NamespaceInput<'_>,
    work_max: usize,
    files: impl FnOnce(&Index<'source>, &mut Work, &mut StaticResolution),
) -> StaticResolution {
    let mut edge_calls: BTreeMap<_, _> = documents
        .0
        .iter()
        .filter(|document| supported_language(document.facts))
        .map(|document| {
            (
                document.placement.unit().clone(),
                EdgeCallCoverage::captured(document),
            )
        })
        .collect();
    let mut work = Work::new(work_max);
    let Some(index) = Index::new(graph, documents, namespace, &mut work) else {
        for coverage in edge_calls.values_mut() {
            coverage.unavailable = true;
        }
        return StaticResolution {
            exhausted: work.exhausted,
            ended: true,
            edge_calls,
            ..StaticResolution::default()
        };
    };
    let mut resolution = StaticResolution {
        unavailable_sources: index.unavailable_sources,
        edge_calls,
        ..StaticResolution::default()
    };
    files(&index, &mut work, &mut resolution);
    resolution.literal_start = append_late_occurrences(
        &mut resolution.occurrences,
        &mut resolution.deferred_literal_containment,
    );
    resolution.callback_parameter_start = append_late_occurrences(
        &mut resolution.occurrences,
        &mut resolution.deferred_callback_parameters,
    );
    resolution.overload_parameter_start = append_late_occurrences(
        &mut resolution.occurrences,
        &mut resolution.deferred_overload_parameters,
    );
    resolution.javascript_method_parameter_start = append_late_occurrences(
        &mut resolution.occurrences,
        &mut resolution.deferred_javascript_method_parameters,
    );
    resolution.scoped_javascript_function_start = append_late_occurrences(
        &mut resolution.occurrences,
        &mut resolution.deferred_scoped_javascript_parameters,
    );
    resolution.scoped_javascript_lexical_start = append_late_occurrences(
        &mut resolution.occurrences,
        &mut resolution.deferred_scoped_javascript_lexical,
    );
    resolution.typescript_type_parameter_start = append_late_occurrences(
        &mut resolution.occurrences,
        &mut resolution.deferred_typescript_type_parameters,
    );
    resolution.javascript_shorthand_parameter_start = append_late_occurrences(
        &mut resolution.occurrences,
        &mut resolution.deferred_javascript_shorthand_parameters,
    );
    external::admit_declared(namespace, &mut resolution.external, work_max);
    index.resolve_rust_extern_aliases(&mut work, &mut resolution);
    index.resolve_rust_receivers(&mut work, &mut resolution);
    index.resolve_rust_generics(&mut work, &mut resolution);
    index.resolve_rust_library_root(&mut work, &mut resolution);
    resolution.rust_crate_paths_start = index.resolve_rust_crate_references(
        &mut work,
        &mut resolution,
        StaticFacts::rust_crate_paths,
    );
    resolution.rust_crate_visibility_start = index.resolve_rust_crate_references(
        &mut work,
        &mut resolution,
        StaticFacts::rust_crate_visibility,
    );
    index.resolve_rust_argument_values(&mut work, &mut resolution);
    index.resolve_rust_import_qualifiers(&mut work, &mut resolution);
    index.resolve_rust_shorthand(&mut work, &mut resolution);
    index.resolve_rust_shared_sources(documents.1, namespace, &mut work, &mut resolution);
    resolution.exhausted = work.exhausted || index.edge_python_classes.ended;
    resolution.ended |= resolution.exhausted;
    resolution
}

fn append_late_occurrences(
    occurrences: &mut Vec<StaticOccurrence>,
    pending: &mut Vec<StaticOccurrence>,
) -> Option<usize> {
    if pending.is_empty() {
        return None;
    }
    let start = occurrences.len();
    occurrences.extend(std::mem::take(pending));
    Some(start)
}

/// What one file's pass adds, held apart until the files join in path order.
struct FileResolution<'source> {
    resolution: StaticResolution,
    deferred_references: Option<DeferredReferences<'source>>,
    /// Budget the pass consumed.
    used: usize,
    /// Whether the pass ended on its budget.
    exhausted: bool,
}

enum DeferredReferences<'source> {
    Python {
        names: Option<python_edges::PythonNameSites>,
        imports: Vec<python_edges::PythonImportQualifier<'source>>,
    },
    PythonQualifiers {
        imports: Vec<python_edges::PythonImportQualifier<'source>>,
        names: Vec<ByteRange>,
    },
    PythonLocals,
    PythonParameters(Vec<python_edges::ReassignedLocalRead<'source>>),
    PythonReassignments(Vec<python_edges::ReassignedLocalRead<'source>>),
    PythonCaptures,
    PythonReceivers,
    PythonVariadics,
    EcmaScriptParameters {
        reads: ParameterReads,
        file_module: bool,
    },
    TypeScriptMethodParameters {
        sites: Vec<ByteRange>,
        file_module: bool,
    },
    EcmaScriptModuleSpecifiers,
    EcmaScriptModuleQualifiers,
    ScopedParameters(ScopedParameterReads),
    Rust {
        pass: RustReferencePass,
        macro_imports: Vec<ByteRange>,
    },
}

enum RustReferencePass {
    Paths,
    RootMacros,
    TextualMacros,
    MacroImports,
    LocalBindings,
}

struct DeferredImports<'source> {
    python_qualifiers: Vec<python_edges::PythonImportQualifier<'source>>,
    rust_macros: Vec<ByteRange>,
}

struct PendingPythonReferences<'index, 'source> {
    file: &'index File<'source>,
    remaining: usize,
    initializers: Option<python_edges::InitializerCalls>,
    quoted_types: bool,
    overload_reads: bool,
}

impl<'index, 'source> PendingPythonReferences<'index, 'source> {
    fn capture(
        file: &'index File<'source>,
        remaining: usize,
        resolution: &StaticResolution,
        has_initializers: bool,
    ) -> Option<Self> {
        let initializers = if has_initializers {
            python_edges::InitializerCalls::capture(file, resolution)
        } else {
            None
        };
        let quoted_types = file.document.facts.language().name == "python"
            && file.facts.references().iter().any(|reference| {
                reference.role == ReferenceRole::Type
                    && python_edges::quoted_type_text(file, reference.target).is_some()
            });
        let overload_reads = file.facts.python_local_reads().iter().any(|(_, name)| {
            file.python_parameters.get(name).is_some_and(|symbol| {
                symbol.is_some_and(rift_syntax::SyntaxSymbol::is_python_overload_parameter)
            })
        });
        (initializers.is_some() || quoted_types || overload_reads).then_some(Self {
            file,
            remaining,
            initializers,
            quoted_types,
            overload_reads,
        })
    }
}

type PendingPythonFiles<'index, 'source> =
    BTreeMap<&'index str, PendingPythonReferences<'index, 'source>>;

/// Budget one file's pass may spend: a floor and an amount for each captured element.
///
/// The pass budget bounds the whole selection. Without a bound of its own, one file whose
/// lookups run long spends what every later file needs, and the selection loses its
/// static facts from that file on. With it that file alone ends early: its unit reports
/// unavailable call coverage, and the files after it keep their facts.
#[derive(Debug, Clone, Copy)]
struct FileWork {
    floor: usize,
    per_element: usize,
}

impl FileWork {
    /// The share of a captured source.
    const CAPTURED: Self = Self {
        floor: FILE_WORK_FLOOR,
        per_element: FILE_WORK_PER_ELEMENT,
    };

    fn max(self, file: &File<'_>) -> usize {
        self.floor
            .saturating_add(file.elements().saturating_mul(self.per_element))
    }

    /// The share of a captured source in the pass that orders every Python class: 128 for
    /// each element leaves a class of three elements about 400 ancestors to merge.
    const CLASS_ORDERS: Self = Self {
        floor: FILE_WORK_FLOOR,
        per_element: 128,
    };

    /// How many of `files`, from the first, may run apart with `left` of the pass budget:
    /// the longest run whose shares, with the one charge each file costs the pass, fit.
    fn run(self, files: &[&File<'_>], left: usize) -> usize {
        let mut reserved = 0_usize;
        files
            .iter()
            .take_while(|file| {
                reserved = reserved.saturating_add(self.max(file)).saturating_add(1);
                reserved <= left
            })
            .count()
    }
}

/// Budget every file's pass may spend whatever it holds.
///
/// A module that only re-exports holds none of the counted elements and still follows
/// every export chain: the heaviest one measured spent 51,937 charges for 27 re-exports.
/// The floor stands five times above it.
const FILE_WORK_FLOOR: usize = 262_144;

/// Budget a file's pass may spend for each call, reference, binding, heritage edge, and
/// declaration it holds.
///
/// Over two large workspaces of 12,646 and 10,024 analyzed files the heaviest file spent
/// 144 charges per element, and a minified bundle 6. The share stands seven times above
/// the heaviest: it ends a pass whose lookups run away, not one that is merely large.
const FILE_WORK_PER_ELEMENT: usize = 1_024;

/// The `append` calls on one Python list, each with its appended expression.
type PythonAppends = Vec<(ByteRange, ByteRange)>;

#[derive(Debug)]
struct Work {
    budget: LoopBudget,
    exhausted: bool,
    python_depth: u8,
    /// Python expressions whose stated type is being read, by file and range.
    python_expressions: Vec<(usize, ByteRange)>,
    /// The `append` calls read for each Python list, by file and name.
    python_appends: BTreeMap<(usize, ByteRange), Option<PythonAppends>>,
    /// Whether the assignments of one Python attribute are being compared.
    python_comparing: bool,
    javascript_depth: u8,
}

impl Work {
    fn new(maximum: usize) -> Self {
        Self {
            budget: LoopBudget::new(maximum),
            exhausted: false,
            python_depth: 0,
            python_expressions: Vec::new(),
            python_appends: BTreeMap::new(),
            python_comparing: false,
            javascript_depth: 0,
        }
    }

    /// Runs one nested callback parameter lookup, refusing past a fixed nesting depth.
    fn javascript_lookup<T>(&mut self, lookup: impl FnOnce(&mut Self) -> Option<T>) -> Option<T> {
        const DEPTH_MAX: u8 = 16;
        if self.javascript_depth >= DEPTH_MAX {
            return None;
        }
        self.javascript_depth += 1;
        let result = lookup(self);
        self.javascript_depth -= 1;
        result
    }

    fn python_lookup<T>(&mut self, lookup: impl FnOnce(&mut Self) -> Option<T>) -> Option<T> {
        const DEPTH_MAX: u8 = 32;
        if self.python_depth >= DEPTH_MAX {
            return None;
        }
        self.python_depth += 1;
        let result = lookup(self);
        self.python_depth -= 1;
        result
    }

    /// Reads the type an expression of one file states. An expression whose own type depends
    /// on it states none.
    fn python_expression<T>(
        &mut self,
        file: &str,
        range: ByteRange,
        lookup: impl FnOnce(&mut Self) -> Option<T>,
    ) -> Option<T> {
        let expression = (file.as_ptr() as usize, range);
        if self.python_expressions.contains(&expression) {
            return None;
        }
        self.python_expressions.push(expression);
        let result = self.python_lookup(lookup);
        self.python_expressions.pop();
        result
    }

    /// Compares the assignments of one Python attribute. The attributes their values read are
    /// not compared in turn.
    fn python_comparison(&mut self, compare: impl FnOnce(&mut Self) -> bool) -> bool {
        if self.python_comparing {
            return false;
        }
        self.python_comparing = true;
        let result = compare(self);
        self.python_comparing = false;
        result
    }

    fn charge(&mut self) -> bool {
        self.exhausted |= self.budget.consume().is_err();
        !self.exhausted
    }
}

#[derive(Debug, Clone, Copy)]
struct Target<'source> {
    file: &'source StaticSource<'source>,
    symbol: &'source SyntaxSymbol,
}

impl Target<'_> {
    fn callable(self) -> Option<Self> {
        self.symbol
            .kinds
            .contains(&SymbolKind::Callable)
            .then_some(self)
    }

    fn admitted_path(self) -> Option<Vec<String>> {
        let anchor = self
            .file
            .placement
            .logical_identity_for_symbol(self.symbol)?;
        let identity = rift_protocol::identity::SymbolIdentity::parse(anchor.as_str()).ok()?;
        Some(identity.qualified_path().to_vec())
    }
}

type Bindings<'source> = BTreeMap<&'source str, Vec<&'source StaticBinding>>;

/// Bindings of one name by scope, each scope's bindings in declaration order.
type ScopedBindings<'source> = EnclosingDefinitions<Vec<&'source StaticBinding>>;

/// Bindings of one name a lookup still reads in declaration order. A longer list is read
/// through its scopes, so a lookup costs the scopes around the position.
const SCANNED_BINDINGS_MAX: usize = 16;

/// Conditional items of one file with what the configuration admits for each.
#[derive(Default)]
struct Conditions {
    items: Vec<(ByteRange, Option<bool>)>,
    /// The items by range when they nest, so a lookup reads the items around a position.
    nested: Option<EnclosingDefinitions<Option<bool>>>,
}

impl Conditions {
    fn new(items: Vec<(ByteRange, Option<bool>)>) -> Self {
        let nested = (items.len() > SCANNED_BINDINGS_MAX).then(|| {
            EnclosingDefinitions::new(
                items
                    .iter()
                    .map(|(item, admitted)| (item.start, item.end, *admitted)),
            )
        });
        Self { items, nested }
    }

    /// Whether an item containing `position` satisfies `holds`, or the budget ended.
    fn any(&self, position: u64, work: &mut Work, holds: impl Fn(Option<bool>) -> bool) -> bool {
        let containing = self
            .nested
            .as_ref()
            .and_then(|nested| nested.containing(position, position.checked_add(1)?));
        match containing {
            Some(mut containing) => containing.any(|admitted| !work.charge() || holds(*admitted)),
            None => self.items.iter().any(|(item, admitted)| {
                !work.charge() || item.contains(position) && holds(*admitted)
            }),
        }
    }
}

/// The facet that stands for a member declared several times (D38): one declaration
/// beside members assigned to its class (`on: Handler` and `this.on = ...`), or members
/// assigned only. Any other repetition, such as overloads, stays ambiguous.
fn member_facet<'source>(candidates: &[&'source SyntaxSymbol]) -> Option<&'source SyntaxSymbol> {
    let assigned =
        |symbol: &&SyntaxSymbol| symbol.syntax_element_kind == Some("assignment_expression");
    if !candidates.iter().any(assigned)
        || !candidates
            .iter()
            .all(|symbol| matches!(symbol.kind, "method" | "property"))
    {
        return None;
    }
    let mut declared = candidates.iter().filter(|symbol| !assigned(symbol));
    match (declared.next(), declared.next()) {
        (Some(symbol), None) => Some(*symbol),
        (None, _) => candidates.first().copied(),
        (Some(_), Some(_)) => None,
    }
}

struct EcmaScriptParameter<'source> {
    symbol: &'source SyntaxSymbol,
    site: &'source StaticParameter,
}

type EcmaScriptParameters<'source> =
    BTreeMap<&'source str, BTreeMap<ByteRange, EcmaScriptParameter<'source>>>;

struct File<'source> {
    document: &'source StaticSource<'source>,
    facts: &'source StaticFacts,
    by_name: Bindings<'source>,
    /// Extern crate aliases are resolved after existing name, call and publication work.
    rust_extern_aliases: Vec<&'source StaticBinding>,
    /// Authored bindings that can shadow an implicit runtime spelling.
    rust_runtime_aliases: BTreeMap<&'source str, EnclosingDefinitions<&'source StaticBinding>>,
    /// Exact class initializer bodies, by the stable variable's physical name.
    javascript_class_bodies: BTreeMap<ByteRange, ByteRange>,
    /// The names with more than [`SCANNED_BINDINGS_MAX`] bindings whose scopes nest.
    by_scope: BTreeMap<&'source str, ScopedBindings<'source>>,
    by_range: BTreeMap<ByteRange, Option<&'source SyntaxSymbol>>,
    by_qualified_name: BTreeMap<&'source str, Vec<&'source SyntaxSymbol>>,
    callables: EnclosingDefinitions<&'source SyntaxSymbol>,
    /// The reference prefix and declaration count before additional Rust bindings.
    primary_reference_count: usize,
    rust_local_count: usize,
    rust_shorthand: bool,
    rust_receivers: Vec<&'source SyntaxSymbol>,
    rust_generics: BTreeMap<ByteRange, &'source SyntaxSymbol>,
    rust_library_root: Option<&'source SyntaxSymbol>,
    package_binding_count: usize,
    ecmascript_parameters: EcmaScriptParameters<'source>,
    typescript_method_parameters: EcmaScriptParameters<'source>,
    javascript_callback_parameters: BTreeMap<ByteRange, EcmaScriptParameter<'source>>,
    typescript_overload_parameters: BTreeMap<ByteRange, EcmaScriptParameter<'source>>,
    javascript_method_parameters: BTreeMap<ByteRange, EcmaScriptParameter<'source>>,
    scoped_javascript_parameters: BTreeMap<ByteRange, EcmaScriptParameter<'source>>,
    typescript_type_parameters: BTreeMap<ByteRange, &'source SyntaxSymbol>,
    javascript_shorthand_parameters: BTreeMap<ByteRange, EcmaScriptParameter<'source>>,
    /// Supplemental parameter witnesses do not participate in primary binding lookup.
    python_parameters: BTreeMap<ByteRange, Option<&'source SyntaxSymbol>>,
    classes: EnclosingDefinitions<&'source SyntaxSymbol>,
    class_scopes: BTreeSet<ByteRange>,
    assignments: AssignmentTargets<'source>,
    python_values: BTreeMap<u64, StaticBindingKind>,
    python_whole: python_edges::WholeFile,
    /// Deleted Python names, by spelling and function.
    python_deletions: BTreeMap<(&'source str, ByteRange), Vec<&'source PythonDeletion>>,
    /// The end of each part that the enclosing scope evaluates, by scope and start.
    python_evaluated_outside: BTreeMap<(ByteRange, u64), u64>,
    /// The Python lambdas, comprehensions and generator expressions around a position.
    python_expressions: EnclosingDefinitions<ByteRange>,
    /// The Python loop statements around a position.
    python_loops: EnclosingDefinitions<ByteRange>,
    /// The bindings of each Python name in the scopes that declare it `global`.
    python_globals: Bindings<'source>,
    /// Python annotation ranges by start, which can be evaluated after later statements.
    python_annotations: BTreeMap<u64, u64>,
    /// Python names with a non-None check and the branch it admits.
    python_non_none: BTreeMap<&'source str, Vec<(ByteRange, ByteRange)>>,
    /// The receiver parameters of the methods of a Python file, by the method's range.
    python_receivers: BTreeMap<ByteRange, Vec<&'source StaticBinding>>,
    /// The calls of a Python file by their first byte, the shortest first.
    python_calls: BTreeMap<u64, Vec<&'source StaticCall>>,
    declared_types: BTreeMap<ByteRange, Option<ByteRange>>,
    /// Declaration name to the named element type its written array type states.
    element_types: BTreeMap<ByteRange, Option<ByteRange>>,
    /// Declaration name to the name its object literal initializer spreads last.
    spread_values: BTreeMap<ByteRange, ByteRange>,
    /// Start offsets of plain name writes whose value is a function written in place.
    function_writes: BTreeSet<u64>,
    type_signatures: BTreeSet<(ByteRange, bool)>,
    callbacks: javascript_callbacks::Callbacks<'source>,
    members: BTreeMap<ByteRange, Option<&'source rift_syntax::StaticMember>>,
    export_writes: BTreeMap<javascript_exports::ExportSlot<'source>, Option<u64>>,
    /// The writes of `module.exports` and what they leave the module with.
    module_value: javascript_exports::ModuleValues<'source>,
    /// Names written as computed keys of the module's exports: `name` and its position in
    /// `module.exports[name] = value`.
    export_keys: Vec<(&'source str, u64)>,
    /// The file's export bindings by exported name; absent for facts without bindings.
    exports: Option<javascript_exports::ExportBindings<'source>>,
    member_writes: BTreeMap<(ByteRange, Option<&'source str>), Option<u64>>,
    unknown_member_writes: BTreeSet<Option<&'source str>>,
    conditions: Conditions,
    /// Rust modules this file's conditional item chains declare, with their admission.
    chain_modules: BTreeMap<&'source str, Option<bool>>,
}

#[derive(Default)]
struct AssignmentTargets<'source> {
    names: BTreeSet<&'source str>,
    /// Start offsets of the writes to each plain name.
    name_writes: BTreeMap<&'source str, Vec<u64>>,
    members: BTreeSet<(&'source str, &'source str)>,
    /// Every member or subscript target, with its start offset.
    member_writes: Vec<(u64, &'source str)>,
    unknown: bool,
}

impl<'source> AssignmentTargets<'source> {
    fn add(&mut self, target: &'source str, start: u64) {
        let target = target.trim();
        if target.starts_with('(') || target.contains(['[', ']', '\\', '/']) {
            self.unknown = true;
        }
        if target.contains(['.', '[']) {
            self.member_writes.push((start, target));
        }
        let Some((receiver, path)) = target.split_once('.') else {
            self.names.insert(target);
            self.name_writes.entry(target).or_default().push(start);
            return;
        };
        if let Some(first) = path.split('.').map(str::trim).next() {
            self.members.insert((receiver.trim(), first));
        }
    }
}

/// The object expression and the property one assignment target writes.
///
/// Syntax capture splits a destructuring pattern into its member and name targets, so a
/// target is a name, a member expression or a subscript. `None` is a target without a
/// member. `Some((receiver, None))` writes a computed property whose name source does not
/// state; a quoted literal key names its property like a dotted one.
fn written_member(target: &str) -> Option<(&str, Option<&str>)> {
    let target = target.trim();
    if let Some(inner) = target.strip_suffix(']') {
        let mut depth = 0_usize;
        let mut open = None;
        for (index, character) in inner.char_indices().rev() {
            match character {
                ']' => depth += 1,
                '[' if depth == 0 => {
                    open = Some(index);
                    break;
                }
                '[' => depth -= 1,
                _ => {}
            }
        }
        let open = open?;
        let receiver = inner[..open].trim().trim_end_matches("?.").trim();
        if receiver.is_empty() {
            return None;
        }
        let key = inner[open + 1..].trim();
        let literal = ['\'', '"', '`'].into_iter().find_map(|quote| {
            key.strip_prefix(quote)?
                .strip_suffix(quote)
                .filter(|name| !name.contains([quote, '\\', '$']))
        });
        return Some((receiver, literal));
    }
    let (receiver, property) = target.rsplit_once('.')?;
    let receiver = receiver.trim().trim_end_matches('?').trim();
    let property = property.trim();
    let named = !property.is_empty()
        && property
            .chars()
            .all(|character| character.is_alphanumeric() || matches!(character, '_' | '$' | '#'));
    named.then_some((receiver, Some(property)))
}

/// Whether the assignment after `target` stores the member itself, bound to its receiver.
///
/// `this.run = this.run.bind(this)` keeps the declared member: the value is that member
/// with a fixed receiver, not another function.
fn rebinds_itself(source: &str, target: &str, receiver: &str, end: u64) -> bool {
    let rebound = || {
        let rest = source.get(usize::try_from(end).ok()?..)?.trim_start();
        let rest = rest.strip_prefix('=')?.trim_start();
        let rest = rest.strip_prefix(target)?.strip_prefix(".bind(")?;
        let rest = rest.strip_prefix(receiver)?.strip_prefix(')')?;
        Some(
            rest.trim_start_matches([' ', '\t'])
                .starts_with([';', '\n', '\r', '}', ',', ')']),
        )
    };
    rebound().unwrap_or(false)
}

impl<'source> File<'source> {
    fn new(
        document: &'source StaticSource<'source>,
        facts: &'source StaticFacts,
        configuration: Option<&crate::analyzer::namespace::RustConfiguration<'_>>,
        admitted_binding: &impl Fn(Target<'source>) -> bool,
        work: &mut Work,
    ) -> Option<Self> {
        let mut file = Self {
            document,
            facts,
            by_name: BTreeMap::new(),
            rust_extern_aliases: Vec::new(),
            rust_runtime_aliases: BTreeMap::new(),
            javascript_class_bodies: BTreeMap::new(),
            by_scope: BTreeMap::new(),
            by_range: BTreeMap::new(),
            by_qualified_name: BTreeMap::new(),
            callables: EnclosingDefinitions::new([]),
            primary_reference_count: facts.references().len(),
            rust_local_count: 0,
            rust_shorthand: false,
            rust_receivers: Vec::new(),
            rust_generics: BTreeMap::new(),
            rust_library_root: None,
            package_binding_count: 0,
            ecmascript_parameters: BTreeMap::new(),
            typescript_method_parameters: BTreeMap::new(),
            javascript_callback_parameters: BTreeMap::new(),
            typescript_overload_parameters: BTreeMap::new(),
            javascript_method_parameters: BTreeMap::new(),
            scoped_javascript_parameters: BTreeMap::new(),
            typescript_type_parameters: BTreeMap::new(),
            javascript_shorthand_parameters: BTreeMap::new(),
            python_parameters: BTreeMap::new(),
            classes: EnclosingDefinitions::new([]),
            class_scopes: BTreeSet::new(),
            assignments: AssignmentTargets::default(),
            python_values: BTreeMap::new(),
            python_whole: python_edges::WholeFile::Module,
            python_deletions: BTreeMap::new(),
            python_evaluated_outside: BTreeMap::new(),
            python_expressions: EnclosingDefinitions::new([]),
            python_loops: EnclosingDefinitions::new([]),
            python_globals: BTreeMap::new(),
            python_annotations: BTreeMap::new(),
            python_non_none: BTreeMap::new(),
            python_receivers: BTreeMap::new(),
            python_calls: BTreeMap::new(),
            declared_types: BTreeMap::new(),
            element_types: BTreeMap::new(),
            spread_values: BTreeMap::new(),
            function_writes: BTreeSet::new(),
            type_signatures: BTreeSet::new(),
            callbacks: javascript_callbacks::Callbacks::default(),
            members: BTreeMap::new(),
            export_writes: BTreeMap::new(),
            module_value: javascript_exports::ModuleValues::default(),
            export_keys: Vec::new(),
            exports: None,
            member_writes: BTreeMap::new(),
            unknown_member_writes: BTreeSet::new(),
            conditions: Conditions::default(),
            chain_modules: BTreeMap::new(),
        };
        if document.facts.language().name == "rust" {
            file.primary_reference_count = facts.rust_local_reference_start(document.source);
            file.chain_modules = crate::analyzer::namespace::rust_chain_modules(
                document.source,
                document.facts,
                configuration,
            );
        }
        let mut conditions = Vec::new();
        for item in facts.conditional_items() {
            if !work.charge() {
                return None;
            }
            conditions.push((
                item.range,
                file.text(item.attribute)
                    .and_then(|text| configuration?.attribute(text)),
            ));
        }
        file.conditions = Conditions::new(conditions);
        file.index_python_deletions(work)?;
        file.index_bindings(work)?;
        file.index_symbols(admitted_binding, work)?;
        file.index_javascript_members(work)?;
        file.callbacks = javascript_callbacks::Callbacks::index(facts, work)?;
        Some(file)
    }

    /// Deleted names indexed by function and spelling.
    fn index_python_deletions(&mut self, work: &mut Work) -> Option<()> {
        for deletion in self.facts.python_deletions() {
            if !work.charge() {
                return None;
            }
            let name = self.text(deletion.name)?;
            self.python_deletions
                .entry((name, deletion.function))
                .or_default()
                .push(deletion);
        }
        Some(())
    }

    /// Python guards, scopes, expressions and loops, indexed for lookup by position.
    fn index_python_flow(&mut self, work: &mut Work) -> Option<()> {
        for &(name, body) in self.facts.non_none_guards() {
            if !work.charge() {
                return None;
            }
            let written = self.text(name)?;
            self.python_non_none
                .entry(written)
                .or_default()
                .push((name, body));
        }
        for &(scope, part) in self.facts.evaluated_outside() {
            if !work.charge() {
                return None;
            }
            self.python_evaluated_outside
                .insert((scope, part.start), part.end);
        }
        let mut expressions = Vec::new();
        for &range in self.facts.expression_scopes() {
            if !work.charge() {
                return None;
            }
            expressions.push((range.start, range.end, range));
        }
        self.python_expressions = EnclosingDefinitions::new(expressions);
        let mut loops = Vec::new();
        for &range in self.facts.loops() {
            if !work.charge() {
                return None;
            }
            loops.push((range.start, range.end, range));
        }
        self.python_loops = EnclosingDefinitions::new(loops);
        Some(())
    }

    fn index_bindings(&mut self, work: &mut Work) -> Option<()> {
        self.index_python_flow(work)?;
        let mut runtime_aliases = Bindings::new();
        for binding in self.facts.bindings() {
            if matches!(binding.kind, StaticBindingKind::RustExternCrate { .. }) {
                self.rust_extern_aliases.push(binding);
                if let Some(name) = self.text(binding.name)
                    && external::RUST_RUNTIME_CRATES.contains(&name)
                {
                    runtime_aliases.entry(name).or_default().push(binding);
                }
                continue;
            }
            if !work.charge() {
                return None;
            }
            if self.inactive(binding.name, work) {
                continue;
            }
            let name = self.text(binding.name)?;
            if let StaticBindingKind::JavaScriptClassValue { body } = binding.kind {
                self.javascript_class_bodies.insert(binding.name, body);
            }
            if matches!(binding.kind, StaticBindingKind::PythonMemberWrite { .. }) {
                continue;
            }
            self.by_name.entry(name).or_default().push(binding);
            match binding.kind {
                StaticBindingKind::PythonReceiver { .. } => {
                    self.python_receivers
                        .entry(binding.scope)
                        .or_default()
                        .push(binding);
                }
                StaticBindingKind::PythonAnnotation { .. }
                | StaticBindingKind::PythonFunction { .. }
                | StaticBindingKind::PythonAlias { .. }
                | StaticBindingKind::PythonElement { .. }
                | StaticBindingKind::PythonUnpacked { .. }
                | StaticBindingKind::PythonInstance { .. } => {
                    self.python_values.insert(binding.name.end, binding.kind);
                }
                _ => {}
            }
            if let StaticBindingKind::PythonAnnotation { annotation }
            | StaticBindingKind::PythonFunction { annotation, .. } = binding.kind
            {
                self.python_annotations
                    .insert(annotation.start, annotation.end);
            }
        }
        self.index_rust_runtime_aliases(runtime_aliases);
        self.index_python_globals(work)?;
        for (name, bindings) in &self.by_name {
            if bindings.len() <= SCANNED_BINDINGS_MAX {
                continue;
            }
            let mut scopes = BTreeMap::<ByteRange, Vec<&StaticBinding>>::new();
            for &binding in bindings {
                if !work.charge() {
                    return None;
                }
                scopes.entry(binding.scope).or_default().push(binding);
            }
            self.by_scope.insert(
                name,
                EnclosingDefinitions::new(
                    scopes
                        .into_iter()
                        .map(|(scope, bindings)| (scope.start, scope.end, bindings)),
                ),
            );
        }
        if self.document.facts.language().name == "python" {
            for call in self.facts.calls() {
                if !work.charge() {
                    return None;
                }
                if call.range == call.callee_range {
                    // A bare decorator application is no call expression inside a receiver.
                    continue;
                }
                self.python_calls
                    .entry(call.range.start)
                    .or_default()
                    .push(call);
            }
            for calls in self.python_calls.values_mut() {
                calls.sort_by_key(|call| call.range.end);
            }
        }
        self.index_written_types(work)
    }

    /// The bindings that a `global` statement gives to the module.
    fn index_python_globals(&mut self, work: &mut Work) -> Option<()> {
        for (name, bindings) in &self.by_name {
            let global = bindings
                .iter()
                .filter(|binding| binding.kind == StaticBindingKind::PythonGlobal)
                .map(|binding| binding.scope)
                .collect::<BTreeSet<_>>();
            if global.is_empty() {
                continue;
            }
            for &binding in bindings {
                if !work.charge() {
                    return None;
                }
                let bound = !matches!(
                    binding.kind,
                    StaticBindingKind::PythonGlobal | StaticBindingKind::PythonGuard { .. }
                );
                if bound && global.contains(&binding.scope) {
                    self.python_globals.entry(name).or_default().push(binding);
                }
            }
        }
        Some(())
    }

    /// Declared and element types, function writes, spread values, type signatures and
    /// assignment targets of the file, indexed for lookup by position.
    fn index_written_types(&mut self, work: &mut Work) -> Option<()> {
        for &(name, annotation) in self.facts.declared_types() {
            if !work.charge() {
                return None;
            }
            self.declared_types
                .entry(name)
                .and_modify(|entry| *entry = None)
                .or_insert(Some(annotation));
        }
        for &(name, element) in self.facts.element_types() {
            if !work.charge() {
                return None;
            }
            self.element_types
                .entry(name)
                .and_modify(|entry| *entry = None)
                .or_insert(Some(element));
        }
        for range in self.facts.function_writes() {
            if !work.charge() {
                return None;
            }
            self.function_writes.insert(range.start);
        }
        for &(name, spread) in self.facts.spread_values() {
            if !work.charge() {
                return None;
            }
            self.spread_values.insert(name, spread);
        }
        for &signature in self.facts.type_signatures() {
            if !work.charge() {
                return None;
            }
            self.type_signatures.insert(signature);
        }
        for range in self.facts.assignments() {
            if !work.charge() {
                return None;
            }
            self.assignments.add(self.text(*range)?, range.start);
        }
        Some(())
    }

    fn index_package_binding(
        &mut self,
        symbol: &'source SyntaxSymbol,
        site: Option<&'source rift_syntax::StaticParameter>,
        methods: &BTreeSet<&str>,
        admitted: bool,
    ) {
        self.package_binding_count += 1;
        if symbol.is_rust_library_root() {
            self.rust_library_root = admitted.then_some(symbol);
        } else if symbol.is_rust_receiver_binding() {
            self.rust_receivers.extend(admitted.then_some(symbol));
        } else if symbol.is_rust_generic_parameter() {
            if let Some(name) = symbol.name_range.filter(|_| admitted) {
                self.rust_generics.insert(name, symbol);
            }
        } else if symbol.is_rust_shorthand_binding() {
            self.rust_shorthand |= admitted;
        } else if symbol.is_rust_local_binding() {
            self.rust_local_count +=
                usize::from(admitted && !symbol.is_rust_explicit_local_binding());
        } else if let Some(name) = symbol.name_range {
            if self.document.facts.language().name == "python" && symbol.is_python_parameter() {
                self.python_parameters
                    .insert(name, admitted.then_some(symbol));
            } else if admitted && symbol.is_typescript_type_parameter() {
                self.typescript_type_parameters.insert(name, symbol);
            } else if admitted && let Some(site) = site {
                if symbol.is_typescript_signature_parameter()
                    || symbol.is_typescript_callable_type_parameter()
                {
                    return;
                }
                let parameter = EcmaScriptParameter { symbol, site };
                if symbol.is_ecmascript_callback_parameter() {
                    self.javascript_callback_parameters.insert(name, parameter);
                    return;
                }
                if symbol.is_ecmascript_overload_parameter() {
                    self.typescript_overload_parameters.insert(name, parameter);
                    return;
                }
                if symbol.is_javascript_method_parameter() {
                    self.javascript_method_parameters.insert(name, parameter);
                    return;
                }
                if symbol.is_javascript_scoped_function_parameter() {
                    self.scoped_javascript_parameters.insert(name, parameter);
                    return;
                }
                if symbol.is_javascript_shorthand_parameter() {
                    self.javascript_shorthand_parameters.insert(name, parameter);
                    return;
                }
                let parameters = if symbol
                    .container
                    .as_deref()
                    .is_some_and(|owner| methods.contains(owner))
                {
                    &mut self.typescript_method_parameters
                } else {
                    &mut self.ecmascript_parameters
                };
                parameters
                    .entry(symbol.name.as_str())
                    .or_default()
                    .insert(name, parameter);
            }
        }
    }

    fn index_symbols(
        &mut self,
        admitted_binding: &impl Fn(Target<'source>) -> bool,
        work: &mut Work,
    ) -> Option<()> {
        let mut callables = Vec::new();
        let mut classes = Vec::new();
        let python = self.document.facts.language().name == "python";
        if python
            && self
                .facts
                .expression_scopes()
                .iter()
                .any(|scope| self.python_whole_range() == Some(*scope))
        {
            self.python_whole = python_edges::WholeFile::Expression;
        }
        let parameters = javascript_parameters::parameter_sites(self.document.facts, self.facts);
        let methods = self.document.facts.typescript_method_names();
        for symbol in self.document.facts.symbols() {
            if symbol.is_package_binding() {
                let admitted = admitted_binding(Target {
                    file: self.document,
                    symbol,
                });
                let site = symbol
                    .name_range
                    .and_then(|range| parameters.get(&range).copied());
                self.index_package_binding(symbol, site, &methods, admitted);
                continue;
            }
            if self.inactive(symbol.item_range, work) {
                continue;
            }
            if !work.charge() {
                return None;
            }
            let candidates = self
                .by_qualified_name
                .entry(&symbol.qualified_name)
                .or_default();
            match (self.document.facts.language().name.as_str(), symbol.kind) {
                ("rust", "impl_item") if !candidates.is_empty() => {}
                ("rust", _)
                    if candidates
                        .first()
                        .is_some_and(|candidate| candidate.kind == "impl_item") =>
                {
                    candidates.clear();
                    candidates.push(symbol);
                }
                _ => candidates.push(symbol),
            }
            if symbol.name == "default" && symbol.name_range != Some(symbol.item_range) {
                self.by_range
                    .entry(symbol.item_range)
                    .and_modify(|entry| *entry = None)
                    .or_insert(Some(symbol));
            }
            if python
                && symbol.container.is_none()
                && self.python_whole_range() == Some(symbol.item_range)
                && let Some(name) = symbol.name_range
                && (symbol.kinds.contains(&SymbolKind::Constructible)
                    || symbol.kinds.contains(&SymbolKind::Callable))
            {
                self.python_whole = python_edges::WholeFile::Definition(name);
            }
            if python && symbol.kinds.contains(&SymbolKind::Constructible) {
                self.class_scopes.insert(symbol.item_range);
                classes.push((symbol.range.start, symbol.range.end, symbol));
            }
            if symbol.kinds.contains(&SymbolKind::Callable) {
                callables.push((symbol.range.start, symbol.range.end, symbol));
            }
            if let Some(range) = symbol.name_range {
                self.by_range
                    .entry(range)
                    .and_modify(|entry| *entry = None)
                    .or_insert(Some(symbol));
            }
        }
        self.callables = EnclosingDefinitions::new(callables);
        self.classes = EnclosingDefinitions::new(classes);
        Some(())
    }

    /// Captured calls, references, bindings, deleted names, heritage edges, and declarations.
    fn elements(&self) -> usize {
        [
            self.facts.calls().len(),
            self.primary_reference_count,
            self.facts.bindings().len() - self.rust_extern_aliases.len(),
            self.facts.python_deletions().len(),
            self.facts.heritage_edges().len(),
            self.document.facts.symbols().len() - self.package_binding_count,
        ]
        .into_iter()
        .fold(0, usize::saturating_add)
    }

    fn scoped_parameter_pass(
        &self,
        earlier: Option<ScopedParameterReads>,
    ) -> Option<ScopedParameterReads> {
        [
            (
                ScopedParameterReads::Callback,
                !self.javascript_callback_parameters.is_empty(),
            ),
            (
                ScopedParameterReads::Overload,
                !self.typescript_overload_parameters.is_empty(),
            ),
            (
                ScopedParameterReads::JavaScriptMethod,
                !self.javascript_method_parameters.is_empty(),
            ),
            (
                ScopedParameterReads::JavaScriptScopedFunction,
                !self.scoped_javascript_parameters.is_empty(),
            ),
            (
                ScopedParameterReads::TypeScriptType,
                !self.typescript_type_parameters.is_empty(),
            ),
            (
                ScopedParameterReads::JavaScriptShorthand,
                !self.javascript_shorthand_parameters.is_empty(),
            ),
        ]
        .into_iter()
        .skip(earlier.map_or(0, |class| class as usize + 1))
        .find_map(|(class, present)| present.then_some(class))
    }

    fn typescript_type_parameter_pass(&self) -> Option<DeferredReferences<'source>> {
        (!self.typescript_type_parameters.is_empty()).then_some(
            DeferredReferences::ScopedParameters(ScopedParameterReads::TypeScriptType),
        )
    }

    fn has_python_local_reads(&self) -> bool {
        self.facts.python_local_reads().iter().any(|(_, name)| {
            match self.python_parameters.get(name) {
                Some(Some(symbol)) => !symbol.is_python_overload_parameter(),
                Some(None) => false,
                None => true,
            }
        })
    }

    fn python_local_pass(&self) -> Option<DeferredReferences<'source>> {
        self.has_python_local_reads()
            .then_some(DeferredReferences::PythonLocals)
            .or_else(|| self.python_after_local_pass())
    }

    fn python_after_local_pass(&self) -> Option<DeferredReferences<'source>> {
        (!self.facts.python_captured_local_reads().is_empty())
            .then_some(DeferredReferences::PythonCaptures)
            .or_else(|| self.python_after_captures_pass())
    }

    fn python_after_captures_pass(&self) -> Option<DeferredReferences<'source>> {
        self.facts
            .python_receiver_reads()
            .iter()
            .any(|(_, name)| matches!(self.python_parameters.get(name), Some(Some(_))))
            .then_some(DeferredReferences::PythonReceivers)
            .or_else(|| self.python_variadic_pass())
    }

    fn python_variadic_pass(&self) -> Option<DeferredReferences<'source>> {
        self.facts
            .python_variadic_reads()
            .iter()
            .any(|(_, name)| matches!(self.python_parameters.get(name), Some(Some(_))))
            .then_some(DeferredReferences::PythonVariadics)
    }

    fn enclosing_callable(&self, range: ByteRange) -> Option<Target<'source>> {
        Some(Target {
            file: self.document,
            symbol: *self.callables.resolve(range.start, range.end)?,
        })
    }

    /// The range a Python module's bindings have as their scope.
    fn python_whole_range(&self) -> Option<ByteRange> {
        Some(ByteRange {
            start: 0,
            end: u64::try_from(self.document.source.len()).ok()?,
        })
    }

    /// Whether a Python binding belongs to the module. One definition or expression may cover
    /// the whole file too, and then the range alone does not tell.
    fn python_module_binding(&self, binding: &StaticBinding) -> bool {
        self.python_whole_range() == Some(binding.scope)
            && match self.python_whole {
                python_edges::WholeFile::Module => true,
                python_edges::WholeFile::Definition(name) => binding.name == name,
                python_edges::WholeFile::Expression => false,
            }
    }

    fn parent_callable(&self, symbol: &SyntaxSymbol) -> Option<Target<'source>> {
        self.enclosing_callable(ByteRange {
            start: symbol.range.start.checked_sub(1)?,
            end: symbol.range.end,
        })
        .filter(|parent| Some(parent.symbol.qualified_name.as_str()) == symbol.container.as_deref())
    }

    fn text(&self, range: ByteRange) -> Option<&'source str> {
        source_text(self.document.source, range)
    }

    fn target_at(&self, range: ByteRange) -> Option<Target<'source>> {
        Some(Target {
            file: self.document,
            symbol: self.by_range.get(&range).copied().flatten()?,
        })
    }

    fn qualified_target(&self, name: &str) -> Option<Target<'source>> {
        let candidates = self.by_qualified_name.get(name)?;
        let language = self.document.facts.language().name.as_str();
        let symbol = if language == "rust" {
            let mut declarations = candidates
                .iter()
                .filter(|symbol| symbol.kind != "impl_item");
            let selected = declarations.next().or_else(|| candidates.first())?;
            if declarations.next().is_some() {
                return None;
            }
            *selected
        } else if language != "python" && candidates.len() != 1 {
            member_facet(candidates)?
        } else {
            *candidates.first()?
        };
        Some(Target {
            file: self.document,
            symbol,
        })
    }

    /// Whether `position` lies in a Python parameter, variable or return annotation.
    fn in_python_annotation(&self, position: u64) -> bool {
        self.python_annotations
            .range(..=position)
            .next_back()
            .is_some_and(|(_, end)| position < *end)
    }

    fn blocked(&self, position: u64, work: &mut Work) -> bool {
        self.facts
            .unsupported_bindings()
            .iter()
            .any(|scope| !work.charge() || scope.contains(position))
    }

    /// The bindings of `name` whose scope encloses `range`, each scope's in declaration
    /// order, and with few bindings of the name every other one too.
    fn bindings_around(
        &self,
        name: &str,
        range: ByteRange,
    ) -> impl Iterator<Item = &'source StaticBinding> + Clone + use<'source> {
        let scopes = self
            .by_scope
            .get(name)
            .and_then(|scopes| scopes.containing(range.start, range.end));
        match scopes {
            Some(scopes) => scopes.flatten().copied().collect::<Vec<_>>(),
            None => self.by_name.get(name).cloned().unwrap_or_default(),
        }
        .into_iter()
    }

    /// Reads the bindings of `name` in the smallest scope that holds `position` and a
    /// binding `admits`, in declaration order. `read` receives each with whether it is the
    /// first of its scope; a scope read before a smaller one is found starts over there.
    ///
    /// A name with few bindings is read in declaration order. A name with many is read
    /// through its scopes, the smallest first, so a file that binds one short name in
    /// thousands of functions pays the scopes around the position for each lookup.
    /// `None` when the budget ended; `Some(false)` when the file binds no such name.
    fn innermost_bindings(
        &self,
        name: &str,
        position: u64,
        work: &mut Work,
        admits: impl Fn(&'source StaticBinding) -> bool,
        mut read: impl FnMut(&'source StaticBinding, bool),
    ) -> Option<bool> {
        let Some(bindings) = self.by_name.get(name) else {
            return Some(false);
        };
        let scopes = self
            .by_scope
            .get(name)
            .and_then(|scopes| scopes.containing(position, position.checked_add(1)?));
        if let Some(scopes) = scopes {
            for scope in scopes {
                let mut first = true;
                for &binding in scope {
                    if !work.charge() {
                        return None;
                    }
                    if admits(binding) {
                        read(binding, first);
                        first = false;
                    }
                }
                if !first {
                    break;
                }
            }
            return Some(true);
        }
        let mut innermost: Option<ByteRange> = None;
        for &binding in bindings {
            if !work.charge() {
                return None;
            }
            if !binding.scope.contains(position) || !admits(binding) {
                continue;
            }
            match innermost {
                Some(scope) if scope == binding.scope => read(binding, false),
                Some(scope) if !scope.encloses(binding.scope) => {}
                _ => {
                    innermost = Some(binding.scope);
                    read(binding, true);
                }
            }
        }
        Some(true)
    }

    fn binding(
        &self,
        name: &str,
        position: u64,
        accepts: impl Fn(StaticBindingKind) -> bool,
        work: &mut Work,
    ) -> Binding<'source> {
        let rust = self.document.facts.language().name == "rust";
        let mut best: Option<&StaticBinding> = None;
        let mut ambiguous = false;
        let found = self.innermost_bindings(
            name,
            position,
            work,
            |binding| accepts(binding.kind) && !(rust && binding.available_from > position),
            |binding, first| match best {
                Some(current)
                    if !first && rust && binding.available_from > current.available_from =>
                {
                    best = Some(binding);
                    ambiguous = false;
                }
                Some(current)
                    if !first && rust && binding.available_from < current.available_from => {}
                Some(_) if !first => ambiguous = true,
                _ => {
                    best = Some(binding);
                    ambiguous = false;
                }
            },
        );
        match found {
            None => return Binding::Blocked,
            Some(false) => return Binding::Missing,
            Some(true) => {}
        }
        match best {
            None => Binding::Missing,
            Some(binding) if !ambiguous && binding.available_from <= position => {
                Binding::Found(binding)
            }
            Some(_) => Binding::Blocked,
        }
    }
}

/// The exact source binding of a captured declaration in the admitted syntax publication.
fn syntax_target<'graph>(
    graph: &'graph NormalizedGraph,
    provider: &ProviderId,
    publication: rift_core::ProviderRevision,
    target: Target<'_>,
) -> Option<(ContributionReference, &'graph DeclarationBinding)> {
    let address = target.symbol.provider_address(
        &target.file.facts.language().identity_segment(),
        target.file.placement.identity_path(),
    );
    let reference = ContributionReference::new(
        provider.clone(),
        ProviderSymbolId::for_symbol(&address).ok()?,
    );
    let key =
        rift_core::ContributionKey::new(provider.clone(), publication, reference.symbol().clone());
    let target_binding = graph.contribution(&key)?.source()?;
    if target_binding.unit() != target.file.placement.unit()
        || target_binding.range().start() != target.symbol.item_range.start
        || target_binding.range().end() != target.symbol.item_range.end
    {
        return None;
    }
    Some((reference, target_binding))
}

enum Binding<'source> {
    Found(&'source StaticBinding),
    Missing,
    Blocked,
}

struct Index<'source> {
    graph: &'source NormalizedGraph,
    provider: ProviderId,
    files: Arc<BTreeMap<&'source str, File<'source>>>,
    rust_extern_files: Vec<&'source str>,
    rust: rust::Namespaces<'source>,
    javascript: javascript::PackageModes<'source>,
    owners: external::Owners,
    edge_python: python_edges::EdgePythonModules,
    edge_python_classes: python_edges::ClassOrders<'source>,
    python_imports: python_edges::ImportTargets<'source>,
    python_aliases: python_edges::ClassAliases<'source>,
    /// The classes that declare a member, by the member's name.
    python_members: python_edges::MemberOwners<'source>,
    python_writes: BTreeMap<&'source str, Vec<(&'source str, ByteRange)>>,
    syntax_publication: rift_core::ProviderRevision,
    unavailable_sources: usize,
    /// Budget every file pass consumed, joined or not.
    #[cfg(test)]
    performed: std::sync::atomic::AtomicUsize,
}

impl<'source> Index<'source> {
    fn new(
        graph: &'source NormalizedGraph,
        documents: (
            &'source [StaticSource<'source>],
            &'source [StaticSource<'source>],
        ),
        namespace: &NamespaceInput<'_>,
        work: &mut Work,
    ) -> Option<Self> {
        let mut files = BTreeMap::new();
        let provider = ProviderId::new(SYNTAX_PROVIDER_ID).ok();
        let syntax_publication = provider
            .as_ref()
            .and_then(|provider| graph.publications().provider(provider))
            .map(rift_provider::ProviderPublication::revision);
        let admitted_binding = |target| {
            Self::admitted_binding(graph, provider.as_ref().zip(syntax_publication), target)
        };
        let configurations = crate::analyzer::namespace::RustConfigurations::new(namespace);
        let mut unavailable_sources = 0;
        let mut rust_extern_files = Vec::new();
        for document in documents.0 {
            if !work.charge() {
                return None;
            }
            if !supported_language(document.facts) {
                continue;
            }
            let facts = if let Some(facts) = admitted_facts(document) {
                facts
            } else {
                unavailable_sources += 1;
                if document.facts.language().name != "rust"
                    || document.facts.source_digest().is_none()
                {
                    continue;
                }
                EMPTY_STATIC_FACTS.get_or_init(StaticFacts::default)
            };
            let file = File::new(
                document,
                facts,
                Some(&configurations.file(document.path.as_str())),
                &admitted_binding,
                work,
            )?;
            let path = document.path.as_str();
            if !file.rust_extern_aliases.is_empty() {
                rust_extern_files.push(path);
            }
            if files.insert(path, file).is_some() {
                return None;
            }
        }
        let edge_python = python_edges::EdgePythonModules::build_edge(&files, work)?;
        let mut python_writes = BTreeMap::<_, Vec<_>>::new();
        for file in files
            .values()
            .filter(|file| file.document.facts.language().name == "python")
        {
            for binding in file.facts.bindings() {
                if !work.charge() {
                    return None;
                }
                if let StaticBindingKind::PythonMemberWrite { receiver } = binding.kind {
                    python_writes
                        .entry(file.text(binding.name)?)
                        .or_default()
                        .push((file.document.path.as_str(), receiver));
                }
            }
        }
        rust_extern_files.sort_unstable();
        let files = Arc::new(files);
        let rust = rust::Namespaces::new(&files, documents.1, &admitted_binding, work)?;
        let javascript = javascript::PackageModes::new(namespace, &files, work)?;
        let owners = external::Owners::new(namespace, work)?;
        let mut index = Self {
            graph,
            provider: provider?,
            files,
            rust_extern_files,
            rust,
            javascript,
            owners,
            edge_python,
            edge_python_classes: python_edges::ClassOrders::default(),
            python_imports: python_edges::ImportTargets::default(),
            python_aliases: python_edges::ClassAliases::default(),
            python_members: python_edges::MemberOwners::default(),
            python_writes,
            syntax_publication: syntax_publication?,
            unavailable_sources,
            #[cfg(test)]
            performed: std::sync::atomic::AtomicUsize::new(0),
        };
        let (imports, aliases) = index.python_import_targets(work)?;
        index.python_imports = imports;
        index.python_members = index.python_member_owners(work)?;
        index.edge_python_classes = index.python_class_orders(FileWork::CLASS_ORDERS, work);
        index.python_aliases = index.python_class_alias_targets(aliases, work);
        Some(index)
    }

    fn admitted_binding(
        graph: &NormalizedGraph,
        syntax: Option<(&ProviderId, rift_core::ProviderRevision)>,
        target: Target<'_>,
    ) -> bool {
        syntax
            .and_then(|(provider, publication)| syntax_target(graph, provider, publication, target))
            .and_then(|(reference, _)| graph.record_for(&reference))
            .is_some_and(|record| record.resolution() == rift_core::SymbolResolution::Established)
    }

    /// The site of an import whose declaration a dependency or the standard library owns.
    fn external_import_occurrence(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<ExternalOccurrence> {
        let target = self.external_import(file, binding, work)?;
        let mut external =
            Self::external_occurrence(file, range, range, ReferenceRole::Import, target)?;
        if file.document.facts.language().name == "rust" {
            external.edge_from = self.rust_module_from(file, range, work);
        }
        Some(external)
    }

    /// Resolves every file and joins the results in path order.
    ///
    /// One file's pass reads the index and that file alone, so passes run across the
    /// worker pool with the `parallel` feature. A pass spends at most its file's share,
    /// so the files start in runs: the longest run, in path order, whose shares together
    /// fit what is left of the pass budget. Every pass of a run then has its whole share
    /// whatever the files before it spend, and its result is the one a pass over the
    /// files in order gives. When not even the next file's share fits, that file runs
    /// alone with exactly what is left. The work done is therefore the work joined: no
    /// pass starts on budget another file may still claim, and none is thrown away.
    fn resolve_files(
        &self,
        file_work: FileWork,
        work: &mut Work,
        resolution: &mut StaticResolution,
    ) {
        let files = self.files.values().collect::<Vec<_>>();
        let mut pending = files.as_slice();
        let mut deferred_references = Vec::new();
        let mut rust_qualifiers = Vec::new();
        let mut pending_python = PendingPythonFiles::new();
        let has_initializers = files
            .iter()
            .any(|file| !file.facts.python_initializers().is_empty());
        while !pending.is_empty() && !work.exhausted {
            let run = file_work.run(pending, work.budget.remaining());
            let (run, rest) = pending.split_at(run.max(1));
            pending = rest;
            let alone = run.len() == 1;
            let left = work.budget.remaining().saturating_sub(1);
            let resolve = |file: &&File<'source>| {
                let share = file_work.max(file);
                let budget = if alone { share.min(left) } else { share };
                self.resolve_file_apart(file, budget, &resolution.edge_calls)
            };
            #[cfg(feature = "parallel")]
            let resolved = {
                use rayon::prelude::{IntoParallelRefIterator, ParallelIterator};
                run.par_iter().map(resolve).collect::<Vec<_>>()
            };
            #[cfg(not(feature = "parallel"))]
            let resolved = run.iter().map(resolve).collect::<Vec<_>>();
            // One allocation holds the run's occurrences: growing by doubling while the
            // files join would hold the list up to twice at its largest.
            resolution.occurrences.reserve(
                resolved
                    .iter()
                    .map(|apart| apart.resolution.occurrences.len())
                    .fold(0, usize::saturating_add),
            );
            for (file, apart) in run.iter().zip(resolved) {
                if !work.charge() {
                    return;
                }
                let left = work.budget.remaining();
                // Only a file running alone can end on the pass budget: a run reserves
                // every share before it starts.
                work.exhausted = apart.exhausted && left < file_work.max(file);
                let over_share = apart.exhausted && !work.exhausted;
                work.budget = LoopBudget::new(left.saturating_sub(apart.used));
                let remaining = file_work.max(file).saturating_sub(apart.used);
                if let Some(pending) = PendingPythonReferences::capture(
                    file,
                    remaining,
                    &apart.resolution,
                    has_initializers,
                ) {
                    pending_python.insert(file.document.path.as_str(), pending);
                }
                if let Some(sites) = apart.deferred_references {
                    let queue = match sites {
                        DeferredReferences::Python { .. }
                        | DeferredReferences::PythonQualifiers { .. }
                        | DeferredReferences::PythonLocals
                        | DeferredReferences::PythonParameters(_)
                        | DeferredReferences::PythonReassignments(_)
                        | DeferredReferences::PythonCaptures
                        | DeferredReferences::PythonReceivers
                        | DeferredReferences::PythonVariadics
                        | DeferredReferences::EcmaScriptParameters { .. }
                        | DeferredReferences::TypeScriptMethodParameters { .. }
                        | DeferredReferences::EcmaScriptModuleSpecifiers
                        | DeferredReferences::EcmaScriptModuleQualifiers
                        | DeferredReferences::ScopedParameters(_) => &mut deferred_references,
                        DeferredReferences::Rust { .. } => &mut rust_qualifiers,
                    };
                    queue.push((*file, remaining, sites));
                }
                resolution.join(file, apart.resolution, over_share);
            }
        }
        if !work.exhausted {
            deferred_references.extend(rust_qualifiers);
            self.resolve_deferred_references(
                deferred_references,
                work,
                resolution,
                &mut pending_python,
            );
        }
    }

    /// Reads additional references after resolving calls, member accesses, and types in every file.
    /// A cut here affects the static pass, but it cannot make a file's call coverage unavailable.
    fn resolve_deferred_references(
        &self,
        references: Vec<(&File<'source>, usize, DeferredReferences<'source>)>,
        work: &mut Work,
        resolution: &mut StaticResolution,
        pending_python: &mut PendingPythonFiles<'_, 'source>,
    ) {
        if !references.is_empty() {
            resolution.supplemental_start =
                Some((resolution.occurrences.len(), resolution.external.len()));
        }
        let (ecmascript_parameter_sites, mut existing): (VecDeque<_>, VecDeque<_>) =
            references.into_iter().partition(|(_, _, sites)| {
                matches!(sites, DeferredReferences::EcmaScriptParameters { .. })
            });
        let (mut javascript_parameter_sites, mut typescript_parameter_sites): (
            VecDeque<_>,
            VecDeque<_>,
        ) = ecmascript_parameter_sites
            .into_iter()
            .partition(|(file, _, _)| file.document.facts.language().name == "javascript");
        let mut macro_sites = VecDeque::new();
        let mut rust_local_sites = VecDeque::new();
        let mut python_local_sites = VecDeque::new();
        let mut python_parameter_sites = VecDeque::new();
        let mut typescript_method_sites = VecDeque::new();
        let mut python_reassigned_sites = VecDeque::new();
        let mut python_captured_sites = VecDeque::new();
        let mut python_receiver_sites = VecDeque::new();
        let mut python_variadic_sites = VecDeque::new();
        let mut module_specifiers = VecDeque::new();
        let mut module_qualifiers = VecDeque::new();
        let mut scoped_parameters = std::array::from_fn::<_, 6, _>(|_| VecDeque::new());
        while let Some((file, remaining, sites)) = existing
            .pop_front()
            .or_else(|| macro_sites.pop_front())
            .or_else(|| rust_local_sites.pop_front())
            .or_else(|| python_local_sites.pop_front())
            .or_else(|| python_parameter_sites.pop_front())
            .or_else(|| typescript_parameter_sites.pop_front())
            .or_else(|| javascript_parameter_sites.pop_front())
            .or_else(|| typescript_method_sites.pop_front())
            .or_else(|| python_reassigned_sites.pop_front())
            .or_else(|| python_captured_sites.pop_front())
            .or_else(|| python_receiver_sites.pop_front())
            .or_else(|| python_variadic_sites.pop_front())
            .or_else(|| {
                self.resolve_python_tail(std::mem::take(pending_python), work, resolution);
                None
            })
            .or_else(|| module_specifiers.pop_front())
            .or_else(|| module_qualifiers.pop_front())
            .or_else(|| scoped_parameters.iter_mut().find_map(VecDeque::pop_front))
        {
            resolution.mark_late_admission(&sites);
            let left = work.budget.remaining();
            let budget = left.min(remaining);
            let mut deferred = Work::new(budget);
            let next = self.resolve_deferred_sites(file, sites, &mut deferred, resolution);
            let used = budget.saturating_sub(deferred.budget.remaining());
            #[cfg(test)]
            self.performed
                .fetch_add(used, std::sync::atomic::Ordering::Relaxed);
            work.budget = LoopBudget::new(left.saturating_sub(used));
            resolution.ended |= deferred.exhausted;
            if let Some(pending) = pending_python.get_mut(file.document.path.as_str()) {
                pending.remaining = pending.remaining.saturating_sub(used);
            }
            if let Some(next) = next {
                let queue = match next {
                    DeferredReferences::PythonLocals => &mut python_local_sites,
                    DeferredReferences::PythonParameters(_) => &mut python_parameter_sites,
                    DeferredReferences::PythonReassignments(_) => &mut python_reassigned_sites,
                    DeferredReferences::PythonCaptures => &mut python_captured_sites,
                    DeferredReferences::PythonReceivers => &mut python_receiver_sites,
                    DeferredReferences::PythonVariadics => &mut python_variadic_sites,
                    DeferredReferences::EcmaScriptParameters { .. }
                        if file.document.facts.language().name == "javascript" =>
                    {
                        &mut javascript_parameter_sites
                    }
                    DeferredReferences::EcmaScriptParameters { .. } => {
                        &mut typescript_parameter_sites
                    }
                    DeferredReferences::TypeScriptMethodParameters { .. } => {
                        &mut typescript_method_sites
                    }
                    DeferredReferences::EcmaScriptModuleSpecifiers => &mut module_specifiers,
                    DeferredReferences::EcmaScriptModuleQualifiers => &mut module_qualifiers,
                    DeferredReferences::ScopedParameters(class) => {
                        &mut scoped_parameters[class as usize]
                    }
                    DeferredReferences::Python { .. }
                    | DeferredReferences::PythonQualifiers { .. } => &mut existing,
                    DeferredReferences::Rust {
                        pass: RustReferencePass::LocalBindings,
                        ..
                    } => &mut rust_local_sites,
                    DeferredReferences::Rust { .. } => &mut macro_sites,
                };
                queue.push_back((file, remaining.saturating_sub(used), next));
            }
        }
    }

    fn resolve_python_tail(
        &self,
        mut pending: PendingPythonFiles<'_, 'source>,
        work: &mut Work,
        resolution: &mut StaticResolution,
    ) {
        if pending.is_empty() {
            return;
        }
        #[cfg(test)]
        let initial_work = work.budget.remaining();
        resolution.python_initializer_start = Some(resolution.occurrences.len());
        for site in pending.values_mut() {
            let Some(calls) = site.initializers.take() else {
                continue;
            };
            Self::resolve_python_shared_file_work(
                site,
                work,
                resolution,
                |file, work, resolution| {
                    self.python_initializer_keywords(file, &calls, work, resolution);
                },
            );
        }
        resolution.python_quoted_type_start = Some(resolution.occurrences.len());
        for site in pending.values_mut().filter(|site| site.quoted_types) {
            Self::resolve_python_shared_file_work(
                site,
                work,
                resolution,
                |file, work, resolution| {
                    self.python_quoted_type_references(file, work, resolution);
                },
            );
        }
        resolution.python_overload_parameter_start = Some(resolution.occurrences.len());
        for site in pending.values_mut().filter(|site| site.overload_reads) {
            Self::resolve_python_shared_file_work(
                site,
                work,
                resolution,
                |file, work, resolution| {
                    self.python_overload_parameter_reads(file, work, resolution);
                },
            );
        }
        #[cfg(test)]
        self.performed.fetch_add(
            initial_work.saturating_sub(work.budget.remaining()),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    fn resolve_python_shared_file_work(
        site: &mut PendingPythonReferences<'_, 'source>,
        work: &mut Work,
        resolution: &mut StaticResolution,
        resolve: impl FnOnce(&File<'source>, &mut Work, &mut StaticResolution),
    ) {
        let left = work.budget.remaining();
        let budget = left.min(site.remaining);
        let mut pending_work = Work::new(budget);
        resolve(site.file, &mut pending_work, resolution);
        let used = budget.saturating_sub(pending_work.budget.remaining());
        work.budget = LoopBudget::new(left.saturating_sub(used));
        site.remaining = site.remaining.saturating_sub(used);
        resolution.ended |= pending_work.exhausted;
    }

    fn resolve_deferred_sites(
        &self,
        file: &File<'source>,
        sites: DeferredReferences<'source>,
        work: &mut Work,
        resolution: &mut StaticResolution,
    ) -> Option<DeferredReferences<'source>> {
        match sites {
            DeferredReferences::Python { names, imports } => {
                self.resolve_python_names(file, names, imports, work, resolution)
            }
            DeferredReferences::PythonQualifiers { imports, names } => {
                self.python_module_qualifiers(file, imports, names, work, resolution);
                file.python_local_pass()
            }
            DeferredReferences::PythonLocals => self.resolve_python_locals(file, work, resolution),
            DeferredReferences::PythonParameters(reassigned) => {
                let _ = self.python_local_reads(file, work, resolution, true);
                (!reassigned.is_empty())
                    .then_some(DeferredReferences::PythonReassignments(reassigned))
                    .or_else(|| file.python_after_local_pass())
            }
            DeferredReferences::PythonReassignments(reassigned) => {
                self.python_reassigned_local_reads(file, reassigned, work, resolution);
                file.python_after_local_pass()
            }
            DeferredReferences::PythonCaptures => {
                self.python_captured_local_reads(file, work, resolution);
                file.python_after_captures_pass()
            }
            DeferredReferences::PythonReceivers => {
                self.python_receiver_reads(file, work, resolution);
                file.python_variadic_pass()
            }
            DeferredReferences::PythonVariadics => {
                self.python_variadic_reads(file, work, resolution);
                None
            }
            DeferredReferences::EcmaScriptParameters { reads, file_module } => {
                self.ecmascript_parameter_reads(
                    file,
                    reads.functions,
                    &file.ecmascript_parameters,
                    work,
                    resolution,
                );
                if reads.methods.is_empty() {
                    file_module
                        .then_some(DeferredReferences::EcmaScriptModuleSpecifiers)
                        .or_else(|| file.typescript_type_parameter_pass())
                } else {
                    Some(DeferredReferences::TypeScriptMethodParameters {
                        sites: reads.methods,
                        file_module,
                    })
                }
            }
            DeferredReferences::TypeScriptMethodParameters { sites, file_module } => {
                self.ecmascript_parameter_reads(
                    file,
                    sites,
                    &file.typescript_method_parameters,
                    work,
                    resolution,
                );
                file_module
                    .then_some(DeferredReferences::EcmaScriptModuleSpecifiers)
                    .or_else(|| file.typescript_type_parameter_pass())
            }
            DeferredReferences::EcmaScriptModuleSpecifiers => {
                resolution
                    .file_module_start
                    .get_or_insert(resolution.occurrences.len());
                self.javascript_module_specifiers(file, work, resolution);
                Some(DeferredReferences::EcmaScriptModuleQualifiers)
            }
            DeferredReferences::EcmaScriptModuleQualifiers => {
                resolution
                    .file_module_qualifier_start
                    .get_or_insert(resolution.occurrences.len());
                self.javascript_module_qualifiers(file, work, resolution);
                file.scoped_parameter_pass(None)
                    .map(DeferredReferences::ScopedParameters)
            }
            DeferredReferences::ScopedParameters(class) => {
                self.ecmascript_scoped_parameter_reads(file, class, work, resolution);
                file.scoped_parameter_pass(Some(class))
                    .map(DeferredReferences::ScopedParameters)
            }
            DeferredReferences::Rust {
                pass,
                macro_imports,
            } => self
                .resolve_deferred_rust(file, &pass, &macro_imports, work, resolution)
                .map(|pass| DeferredReferences::Rust {
                    pass,
                    macro_imports,
                }),
        }
    }

    fn resolve_python_names(
        &self,
        file: &File<'source>,
        names: Option<python_edges::PythonNameSites>,
        imports: Vec<python_edges::PythonImportQualifier<'source>>,
        work: &mut Work,
        resolution: &mut StaticResolution,
    ) -> Option<DeferredReferences<'source>> {
        let names = names.map_or_else(Vec::new, |sites| {
            self.python_name_references(file, sites, work, resolution)
        });
        (!names.is_empty() || !imports.is_empty())
            .then_some(DeferredReferences::PythonQualifiers { imports, names })
            .or_else(|| file.python_local_pass())
    }

    fn resolve_python_locals(
        &self,
        file: &File<'source>,
        work: &mut Work,
        resolution: &mut StaticResolution,
    ) -> Option<DeferredReferences<'source>> {
        let reassigned = self.python_local_reads(file, work, resolution, false);
        if file.python_parameters.values().any(Option::is_some) {
            Some(DeferredReferences::PythonParameters(reassigned))
        } else {
            (!reassigned.is_empty())
                .then_some(DeferredReferences::PythonReassignments(reassigned))
                .or_else(|| file.python_after_local_pass())
        }
    }

    /// Records one Rust reference class and returns the next class that has source evidence.
    fn resolve_deferred_rust(
        &self,
        file: &File<'source>,
        pass: &RustReferencePass,
        macro_imports: &[ByteRange],
        work: &mut Work,
        resolution: &mut StaticResolution,
    ) -> Option<RustReferencePass> {
        let locals = (file.rust_local_count != 0).then_some(RustReferencePass::LocalBindings);
        match pass {
            RustReferencePass::Paths => {
                self.rust_call_qualifiers(file, work, resolution);
                self.rust_value_reads(file, work, resolution);
                self.rust_field_sites(file, work, resolution);
                (!file.facts.rust_root_macro_calls().is_empty()
                    || !file.facts.rust_textual_macro_calls().is_empty()
                    || !macro_imports.is_empty())
                .then_some(RustReferencePass::RootMacros)
                .or(locals)
            }
            RustReferencePass::RootMacros => {
                self.rust_root_macro_calls(file, work, resolution);
                (!file.facts.rust_textual_macro_calls().is_empty() || !macro_imports.is_empty())
                    .then_some(RustReferencePass::TextualMacros)
                    .or(locals)
            }
            RustReferencePass::TextualMacros => {
                self.rust_textual_macro_calls(file, work, resolution);
                (!macro_imports.is_empty())
                    .then_some(RustReferencePass::MacroImports)
                    .or(locals)
            }
            RustReferencePass::MacroImports => {
                self.rust_macro_imports(file, macro_imports, work, resolution);
                locals
            }
            RustReferencePass::LocalBindings => {
                self.rust_local_bindings(file, false, work, resolution);
                None
            }
        }
    }

    /// One file's pass under its own budget, with a result of its own.
    fn resolve_file_apart(
        &self,
        file: &File<'source>,
        budget: usize,
        edge_calls: &BTreeMap<rift_core::SourceUnitId, EdgeCallCoverage>,
    ) -> FileResolution<'source> {
        let mut work = Work::new(budget);
        let mut resolution = StaticResolution::default();
        let unit = file.document.placement.unit();
        if let Some(coverage) = edge_calls.get(unit) {
            resolution.edge_calls.insert(
                unit.clone(),
                EdgeCallCoverage {
                    language: coverage.language.clone(),
                    unavailable: coverage.unavailable,
                    unresolved: Vec::new(),
                },
            );
        }
        let deferred_references = self.resolve_file(file, &mut work, &mut resolution);
        let used = budget.saturating_sub(work.budget.remaining());
        #[cfg(test)]
        self.performed
            .fetch_add(used, std::sync::atomic::Ordering::Relaxed);
        FileResolution {
            resolution,
            deferred_references,
            used,
            exhausted: work.exhausted,
        }
    }

    /// Resolves one file's calls; `false` when the budget ended the pass.
    fn resolve_file_calls(
        &self,
        file: &File<'source>,
        work: &mut Work,
        result: &mut StaticResolution,
    ) -> bool {
        for call in file.facts.calls() {
            if file.inactive(call.range, work) {
                continue;
            }
            if !work.charge() {
                return false;
            }
            let mut occurrence = self.call_occurrence(file, call, work);
            if matches!(
                file.document.facts.language().name.as_str(),
                "javascript" | "typescript"
            ) && let Some(occurrence) = &mut occurrence
            {
                occurrence.edge_from = self.javascript_call_from(file, call.range, work);
            }
            if occurrence.is_none()
                && let Some(external) = self.external_call(file, call, work).and_then(|target| {
                    Self::external_occurrence(
                        file,
                        call.range,
                        call.callee_range,
                        ReferenceRole::Call,
                        target,
                    )
                })
            {
                result.external.push(external);
                continue;
            }
            if occurrence.is_none()
                && !file.rust_extern_aliases.is_empty()
                && call.callee == rift_syntax::StaticCallee::Qualified
            {
                result
                    .rust_extern_candidates
                    .entry(file.document.placement.unit().clone())
                    .or_default()
                    .push(*call);
            }
            if occurrence
                .as_ref()
                .is_none_or(|occurrence| !matches!(occurrence.target, NormalizedTarget::Symbol(_)))
                && let Some(coverage) = result.edge_calls.get_mut(file.document.placement.unit())
            {
                coverage.unresolved.push(call.range);
            }
            result.record_kind(occurrence, rift_protocol::read::EdgeKind::Calls);
        }
        true
    }

    fn resolve_file(
        &self,
        file: &File<'source>,
        work: &mut Work,
        result: &mut StaticResolution,
    ) -> Option<DeferredReferences<'source>> {
        for range in file.facts.unsupported_bindings() {
            if file.inactive(*range, work) {
                continue;
            }
            if !work.charge() {
                return None;
            }
            if let Some(coverage) = result.edge_calls.get_mut(file.document.placement.unit()) {
                coverage.unresolved.push(*range);
            }
        }
        if !self.resolve_file_calls(file, work, result) {
            return None;
        }
        for edge in file.facts.heritage_edges() {
            if file.inactive(edge.from, work) {
                continue;
            }
            if !work.charge() {
                return None;
            }
            let occurrence = self.edge_heritage_occurrence(file, edge, work);
            if occurrence.is_none()
                && let Some(external) = self.external_heritage(file, edge, work)
            {
                result.external.push(external);
                continue;
            }
            result.record_kind(occurrence, edge.kind);
        }
        let DeferredImports {
            python_qualifiers: imports,
            rust_macros,
        } = self.resolve_imports(file, work, result)?;
        let mut parameter_reads = ParameterReads::default();
        if matches!(
            file.document.facts.language().name.as_str(),
            "javascript" | "typescript"
        ) {
            self.javascript_edges(file, work, result, &mut parameter_reads);
        }
        let deferred = match file.document.facts.language().name.as_str() {
            "javascript" | "typescript" => {
                let file_module = self.javascript_has_file_module(file);
                (!work.exhausted
                    && (file_module
                        || !parameter_reads.functions.is_empty()
                        || !parameter_reads.methods.is_empty()
                        || !file.typescript_type_parameters.is_empty()))
                .then_some(DeferredReferences::EcmaScriptParameters {
                    reads: parameter_reads,
                    file_module,
                })
            }
            "python" => {
                let names = self.python_references(file, work, result);
                (!work.exhausted
                    && (names.is_some()
                        || !imports.is_empty()
                        || file.has_python_local_reads()
                        || file.python_after_local_pass().is_some()))
                .then_some(DeferredReferences::Python { names, imports })
            }
            "rust" => {
                self.rust_references(file, work, result);
                (!file.facts.calls().is_empty()
                    || !file.facts.rust_value_reads().is_empty()
                    || !file.facts.rust_field_sites().is_empty()
                    || !file.facts.rust_root_macro_calls().is_empty()
                    || !file.facts.rust_textual_macro_calls().is_empty()
                    || !rust_macros.is_empty()
                    || file.rust_local_count != 0)
                    .then_some(DeferredReferences::Rust {
                        pass: RustReferencePass::Paths,
                        macro_imports: rust_macros,
                    })
            }
            _ => None,
        };
        if !work.exhausted {
            result.analyzed_sources += 1;
        }
        deferred
    }

    fn resolve_imports(
        &self,
        file: &File<'source>,
        work: &mut Work,
        result: &mut StaticResolution,
    ) -> Option<DeferredImports<'source>> {
        let mut written_python_imports = BTreeSet::new();
        let mut qualifiers = Vec::new();
        let mut rust_macros = Vec::new();
        for binding in file.facts.bindings() {
            if matches!(binding.kind, StaticBindingKind::RustExternCrate { .. }) {
                continue;
            }
            if file.inactive(binding.name, work) {
                continue;
            }
            if !work.charge() {
                return None;
            }
            if let Some((range, target)) = self.import_target(file, binding, work) {
                let mut written = None;
                let occurrence = target.and_then(|target| {
                    if file.document.facts.language().name == "rust" {
                        return self.rust_import_occurrence(file, range, target, work);
                    }
                    let mut occurrence =
                        self.occurrence(file, range, range, ReferenceRole::Import, target)?;
                    if file.document.facts.language().name == "python" {
                        occurrence.edge_from =
                            Some(self.python_identity(file, range, file.qualified_target(".")?)?);
                        written =
                            self.python_written_import(file, binding, target, &occurrence, work);
                        if written.is_none()
                            && let Some(qualifier) =
                                python_edges::PythonImportQualifier::from_import(
                                    binding,
                                    target,
                                    &occurrence,
                                )
                        {
                            qualifiers.push(qualifier);
                        }
                    } else if matches!(
                        file.document.facts.language().name.as_str(),
                        "javascript" | "typescript"
                    ) {
                        occurrence.edge_from = self.javascript_module_from(file, range, work);
                    }
                    Some(occurrence)
                });
                if occurrence.is_none()
                    && let Some(external) =
                        self.external_import_occurrence(file, binding, range, work)
                {
                    result.external.push(external);
                    continue;
                }
                if occurrence.is_none() && matches!(binding.kind, StaticBindingKind::RustUse { .. })
                {
                    rust_macros.push(range);
                }
                result.record_kind(occurrence, rift_protocol::read::EdgeKind::Imports);
                if let Some(written) = written
                    && written_python_imports.insert(written.callee_range)
                {
                    result.record_kind(Some(written), rift_protocol::read::EdgeKind::Imports);
                }
            }
        }
        qualifiers.retain(|site| !written_python_imports.contains(&site.module));
        Some(DeferredImports {
            python_qualifiers: qualifiers,
            rust_macros,
        })
    }

    fn call_occurrence(
        &self,
        file: &File<'source>,
        call: &StaticCall,
        work: &mut Work,
    ) -> Option<StaticOccurrence> {
        let target = self.call_target(file, call, work)?;
        let mut occurrence = self.occurrence(
            file,
            call.range,
            call.callee_range,
            ReferenceRole::Call,
            target,
        )?;
        if file.document.facts.language().name == "python"
            && let Some(caller) = file.enclosing_callable(call.range).or_else(|| {
                file.classes
                    .resolve(call.range.start, call.range.end)
                    .map(|symbol| Target {
                        file: file.document,
                        symbol,
                    })
                    .or_else(|| file.qualified_target("."))
            })
        {
            let caller = self.occurrence(
                file,
                call.range,
                call.callee_range,
                ReferenceRole::Call,
                caller,
            )?;
            let NormalizedTarget::Symbol(caller) = caller.target else {
                return None;
            };
            occurrence.edge_from = Some(caller);
        }
        Some(occurrence)
    }

    fn edge_heritage_occurrence(
        &self,
        file: &File<'source>,
        edge: &rift_syntax::HeritageEdge,
        work: &mut Work,
    ) -> Option<StaticOccurrence> {
        let target = match file.document.facts.language().name.as_str() {
            "python" => self.edge_python_base(file, edge.target, work),
            "javascript" | "typescript" => self.javascript_heritage_target(file, edge, work),
            _ => self.edge_type_target(file, edge.target, work),
        }?;
        let mut occurrence =
            self.occurrence(file, edge.target, edge.target, ReferenceRole::Type, target)?;
        if let NormalizedTarget::Symbol(from) = self.heritage_source(file, edge, work)? {
            occurrence.edge_from = Some(from);
        }
        occurrence.edge_kind = Some(edge.kind);
        Some(occurrence)
    }

    /// The selected declaration a heritage header belongs to.
    fn heritage_source(
        &self,
        file: &File<'source>,
        edge: &rift_syntax::HeritageEdge,
        work: &mut Work,
    ) -> Option<NormalizedTarget> {
        let from = if file.document.facts.language().name == "rust" {
            self.edge_type_target(file, edge.from, work)?
        } else {
            file.target_at(edge.from)
                .or_else(|| self.edge_type_target(file, edge.from, work))?
        };
        let from = self.occurrence(file, edge.from, edge.from, ReferenceRole::Type, from)?;
        Some(from.target)
    }

    /// A heritage header from a selected declaration to a dependency or standard-library
    /// type.
    fn external_heritage(
        &self,
        file: &File<'source>,
        edge: &rift_syntax::HeritageEdge,
        work: &mut Work,
    ) -> Option<ExternalOccurrence> {
        let NormalizedTarget::Symbol(from) = self.heritage_source(file, edge, work)? else {
            return None;
        };
        let target = self.external_type(file, edge.target, work)?;
        let mut occurrence =
            Self::external_occurrence(file, edge.target, edge.target, ReferenceRole::Type, target)?;
        occurrence.edge_from = Some(from);
        occurrence.edge_kind = Some(edge.kind);
        Some(occurrence)
    }

    pub(super) fn external_occurrence(
        file: &File<'source>,
        range: ByteRange,
        callee_range: ByteRange,
        role: ReferenceRole,
        target: rift_core::SymbolId,
    ) -> Option<ExternalOccurrence> {
        file.text(range)?;
        file.text(callee_range)?;
        let range = SourceRange::new(range.start, range.end).ok()?;
        Some(ExternalOccurrence {
            binding: DeclarationBinding::new(file.document.placement.unit().clone(), range),
            target,
            callee_range,
            role,
            edge_from: None,
            edge_kind: None,
            declared: false,
        })
    }

    /// Python annotation sites without a local target that name a dependency or
    /// standard-library type. Local annotation targets are recorded by `python_references`.
    fn named_type_references(
        &self,
        file: &File<'source>,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        for reference in file
            .facts
            .references()
            .iter()
            .filter(|reference| reference.role == ReferenceRole::Type)
        {
            if !work.charge() {
                return;
            }
            if self
                .edge_type_target(file, reference.target, work)
                .is_some()
            {
                continue;
            }
            if let Some(target) = self.external_type(file, reference.target, work)
                && let Some(external) = Self::external_occurrence(
                    file,
                    reference.target,
                    reference.target,
                    reference.role,
                    target,
                )
            {
                result.external.push(external);
                continue;
            }
            let count = result
                .omitted
                .entry(rift_protocol::read::EdgeKind::HasType)
                .or_default();
            *count = count.saturating_add(1);
        }
    }

    fn edge_type_target(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        match file.document.facts.language().name.as_str() {
            "javascript" | "typescript" => self.edge_javascript_target(file, range, work),
            "python" => self.python_type(file, range, work),
            "rust" => self.rust.edge_type_target(file, range, work),
            _ => None,
        }
    }

    fn call_target(
        &self,
        file: &File<'source>,
        call: &StaticCall,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if file.blocked(call.range.start, work) {
            return None;
        }
        match file.document.facts.language().name.as_str() {
            "javascript" | "typescript" => self.javascript_call(file, call, work),
            "rust" => self.rust.call(file, call, work),
            "python" => self.edge_python_call(file, call, work),
            _ => None,
        }
    }

    fn import_target(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        work: &mut Work,
    ) -> Option<(ByteRange, Option<Target<'source>>)> {
        match binding.kind {
            StaticBindingKind::JavaScriptImport { .. }
            | StaticBindingKind::JavaScriptRequire {
                export: Some(_), ..
            } => Some((binding.name, self.javascript_import(file, binding, work))),
            StaticBindingKind::RustUse { path } => Some((path, self.rust.import(file, path, work))),
            StaticBindingKind::PythonImport { .. } | StaticBindingKind::PythonModule { .. } => {
                Some((
                    binding.name,
                    self.edge_python_binding_target(file, binding, work),
                ))
            }
            _ => None,
        }
    }

    fn occurrence(
        &self,
        file: &File<'source>,
        range: ByteRange,
        callee_range: ByteRange,
        role: ReferenceRole,
        target: Target<'source>,
    ) -> Option<StaticOccurrence> {
        file.text(range)?;
        file.text(callee_range)?;
        let range = SourceRange::new(range.start, range.end).ok()?;
        let (reference, target_binding) =
            syntax_target(self.graph, &self.provider, self.syntax_publication, target)?;
        let record = self.graph.record_for(&reference)?;
        let normalized = match record.resolution() {
            rift_core::SymbolResolution::Established => {
                NormalizedTarget::Symbol(record.identity()?.clone())
            }
            rift_core::SymbolResolution::Unresolved => NormalizedTarget::Contribution(reference),
            rift_core::SymbolResolution::Conflicting => return None,
        };
        Some(StaticOccurrence {
            binding: DeclarationBinding::new(file.document.placement.unit().clone(), range),
            target: normalized,
            target_binding: Some(target_binding.clone()),
            source_digest: *file.document.facts.source_digest()?,
            callee_range,
            role,
            edge_from: None,
            edge_kind: None,
        })
    }
}

impl StaticResolution {
    fn mark_late_admission(&mut self, sites: &DeferredReferences<'_>) {
        if matches!(sites, DeferredReferences::PythonReassignments(_)) {
            self.python_reassignment_start
                .get_or_insert(self.occurrences.len());
        }
        if matches!(sites, DeferredReferences::PythonCaptures) {
            self.python_capture_start
                .get_or_insert(self.occurrences.len());
        }
        if matches!(sites, DeferredReferences::PythonReceivers) {
            self.python_receiver_start
                .get_or_insert(self.occurrences.len());
        }
        if matches!(sites, DeferredReferences::PythonVariadics) {
            self.python_variadic_start
                .get_or_insert(self.occurrences.len());
        }
        if matches!(
            sites,
            DeferredReferences::Rust {
                pass: RustReferencePass::LocalBindings,
                ..
            } | DeferredReferences::PythonLocals
                | DeferredReferences::PythonParameters(_)
                | DeferredReferences::EcmaScriptParameters { .. }
                | DeferredReferences::TypeScriptMethodParameters { .. }
                | DeferredReferences::EcmaScriptModuleSpecifiers
        ) {
            self.late_supplemental_start
                .get_or_insert(self.occurrences.len());
        }
    }

    /// Adds one file's result after the results of the files before it. A file whose
    /// pass ended on its own share keeps what it resolved and reports unavailable call
    /// coverage for its unit.
    fn join(&mut self, file: &File<'_>, mut apart: Self, over_share: bool) {
        self.occurrences.append(&mut apart.occurrences);
        self.deferred_literal_containment
            .append(&mut apart.deferred_literal_containment);
        self.deferred_scoped_javascript_lexical
            .append(&mut apart.deferred_scoped_javascript_lexical);
        self.external.append(&mut apart.external);
        self.rust_extern_candidates
            .append(&mut apart.rust_extern_candidates);
        for (kind, count) in apart.omitted {
            let total = self.omitted.entry(kind).or_default();
            *total = total.saturating_add(count);
        }
        self.analyzed_sources += apart.analyzed_sources;
        self.unresolved_sites += apart.unresolved_sites;
        if over_share {
            self.unavailable_sources += 1;
            self.ended = true;
        }
        let unit = file.document.placement.unit();
        if let Some((coverage, apart)) = self
            .edge_calls
            .get_mut(unit)
            .zip(apart.edge_calls.get_mut(unit))
        {
            coverage.unresolved.append(&mut apart.unresolved);
            coverage.unavailable |= over_share;
        }
    }

    /// Records one site of `kind`, counting it as left out when it has no target.
    fn record_kind(
        &mut self,
        occurrence: Option<StaticOccurrence>,
        kind: rift_protocol::read::EdgeKind,
    ) {
        if occurrence.is_none() {
            let count = self.omitted.entry(kind).or_default();
            *count = count.saturating_add(1);
        }
        self.record(occurrence);
    }

    fn record(&mut self, occurrence: Option<StaticOccurrence>) {
        match occurrence {
            Some(occurrence) => self.occurrences.push(occurrence),
            None => self.unresolved_sites += 1,
        }
    }
}

fn admitted_facts<'source>(
    document: &'source StaticSource<'source>,
) -> Option<&'source StaticFacts> {
    if document.facts.source_digest().is_none()
        || (document.facts.has_errors()
            && (document.facts.symbols().is_empty()
                || !matches!(
                    document.facts.language().name.as_str(),
                    "javascript" | "typescript" | "rust"
                )))
    {
        return None;
    }
    document.facts.static_facts()
}

fn supported_language(facts: &SyntaxFacts) -> bool {
    let language = facts.language();
    matches!(
        language.name.as_str(),
        "javascript" | "typescript" | "rust" | "python"
    )
}

fn source_text(source: &str, range: ByteRange) -> Option<&str> {
    let start = usize::try_from(range.start).ok()?;
    let end = usize::try_from(range.end).ok()?;
    source.get(start..end).filter(|text| !text.is_empty())
}
