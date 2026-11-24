//! Resolves captured call and import sites against admitted source declarations.

mod external;
mod javascript;
mod javascript_callbacks;
mod javascript_edges;
mod javascript_exports;
mod javascript_member_callbacks;
mod javascript_members;
mod javascript_reexports;
mod javascript_scopes;
mod python_edges;
mod rust;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use rift_core::{
    ContributionReference, DeclarationBinding, FileDigest, LoopBudget, ProjectPath, ProviderId,
    ProviderSymbolId, ReferenceRole, SourceRange,
};
use rift_protocol::read::SymbolKind;
use rift_provider::{NormalizedGraph, NormalizedTarget};
use rift_syntax::{
    ByteRange, DocumentPlacement, SYNTAX_PROVIDER_ID, StaticBinding, StaticBindingKind, StaticCall,
    StaticFacts, SyntaxFacts, SyntaxSymbol,
};

pub(crate) use external::ExternalOccurrence;

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
    /// Occurrences whose target a dependency or a standard library owns.
    pub(crate) external: Vec<ExternalOccurrence>,
    /// Edge kinds of sites left without a target.
    pub(crate) omitted: BTreeMap<rift_protocol::read::EdgeKind, u64>,
    pub(crate) analyzed_sources: usize,
    pub(crate) unavailable_sources: usize,
    pub(crate) unresolved_sites: usize,
    pub(crate) exhausted: bool,
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
    namespace: &NamespaceInput<'_>,
    work_max: usize,
) -> StaticResolution {
    resolve_by(
        graph,
        documents,
        namespace,
        work_max,
        |index, work, resolution| index.resolve_files(FileWork::CAPTURED, work, resolution),
    )
}

/// Runs one static pass whose files `files` resolves against the built index.
fn resolve_by<'source>(
    graph: &'source NormalizedGraph,
    documents: &'source [StaticSource<'source>],
    namespace: &NamespaceInput<'_>,
    work_max: usize,
    files: impl FnOnce(&Index<'source>, &mut Work, &mut StaticResolution),
) -> StaticResolution {
    let mut edge_calls: BTreeMap<_, _> = documents
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
    external::admit_declared(namespace, &mut resolution.external, work_max);
    resolution.exhausted = work.exhausted;
    resolution
}

/// What one file's pass adds, held apart until the files join in path order.
struct FileResolution {
    resolution: StaticResolution,
    /// Budget the pass consumed.
    used: usize,
    /// Whether the pass ended on its budget.
    exhausted: bool,
}

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

#[derive(Debug)]
struct Work {
    budget: LoopBudget,
    exhausted: bool,
    python_depth: u8,
    javascript_depth: u8,
}

impl Work {
    fn new(maximum: usize) -> Self {
        Self {
            budget: LoopBudget::new(maximum),
            exhausted: false,
            python_depth: 0,
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
            .logical_identity(&self.symbol.qualified_name)?;
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

struct File<'source> {
    document: &'source StaticSource<'source>,
    facts: &'source StaticFacts,
    by_name: Bindings<'source>,
    /// The names with more than [`SCANNED_BINDINGS_MAX`] bindings whose scopes nest.
    by_scope: BTreeMap<&'source str, ScopedBindings<'source>>,
    by_range: BTreeMap<ByteRange, Option<&'source SyntaxSymbol>>,
    by_qualified_name: BTreeMap<&'source str, Vec<&'source SyntaxSymbol>>,
    callables: EnclosingDefinitions<&'source SyntaxSymbol>,
    classes: EnclosingDefinitions<&'source SyntaxSymbol>,
    class_scopes: BTreeSet<ByteRange>,
    assignments: AssignmentTargets<'source>,
    python_values: BTreeMap<u64, StaticBindingKind>,
    /// Python annotation ranges by start, which can be evaluated after later statements.
    python_annotations: BTreeMap<u64, u64>,
    /// Python names with a non-None check and the branch it admits.
    python_non_none: BTreeMap<&'source str, Vec<(ByteRange, ByteRange)>>,
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
        work: &mut Work,
    ) -> Option<Self> {
        let mut file = Self {
            document,
            facts,
            by_name: BTreeMap::new(),
            by_scope: BTreeMap::new(),
            by_range: BTreeMap::new(),
            by_qualified_name: BTreeMap::new(),
            callables: EnclosingDefinitions::new([]),
            classes: EnclosingDefinitions::new([]),
            class_scopes: BTreeSet::new(),
            assignments: AssignmentTargets::default(),
            python_values: BTreeMap::new(),
            python_annotations: BTreeMap::new(),
            python_non_none: BTreeMap::new(),
            python_calls: BTreeMap::new(),
            declared_types: BTreeMap::new(),
            element_types: BTreeMap::new(),
            spread_values: BTreeMap::new(),
            function_writes: BTreeSet::new(),
            type_signatures: BTreeSet::new(),
            callbacks: javascript_callbacks::Callbacks::default(),
            members: BTreeMap::new(),
            export_writes: BTreeMap::new(),
            member_writes: BTreeMap::new(),
            unknown_member_writes: BTreeSet::new(),
            conditions: Conditions::default(),
            chain_modules: BTreeMap::new(),
        };
        if document.facts.language().name == "rust" {
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
        file.index_bindings(work)?;
        file.index_symbols(work)?;
        file.index_javascript_members(work)?;
        file.callbacks = javascript_callbacks::Callbacks::index(facts, work)?;
        Some(file)
    }

    fn index_bindings(&mut self, work: &mut Work) -> Option<()> {
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
        for binding in self.facts.bindings() {
            if !work.charge() {
                return None;
            }
            if self.inactive(binding.name, work) {
                continue;
            }
            let name = self.text(binding.name)?;
            if matches!(binding.kind, StaticBindingKind::PythonMemberWrite { .. }) {
                continue;
            }
            self.by_name.entry(name).or_default().push(binding);
            match binding.kind {
                StaticBindingKind::PythonAnnotation { .. }
                | StaticBindingKind::PythonFunction { .. }
                | StaticBindingKind::PythonAlias { .. }
                | StaticBindingKind::PythonElement { .. }
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
        }
        self.index_written_types(work)
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

    fn index_symbols(&mut self, work: &mut Work) -> Option<()> {
        let mut callables = Vec::new();
        let mut classes = Vec::new();
        let python = self.document.facts.language().name == "python";
        for symbol in self.document.facts.symbols() {
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

    /// Captured calls, references, bindings, heritage edges, and declarations.
    fn elements(&self) -> usize {
        [
            self.facts.calls().len(),
            self.facts.references().len(),
            self.facts.bindings().len(),
            self.facts.heritage_edges().len(),
            self.document.facts.symbols().len(),
        ]
        .into_iter()
        .fold(0, usize::saturating_add)
    }

    fn enclosing_callable(&self, range: ByteRange) -> Option<Target<'source>> {
        Some(Target {
            file: self.document,
            symbol: *self.callables.resolve(range.start, range.end)?,
        })
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

enum Binding<'source> {
    Found(&'source StaticBinding),
    Missing,
    Blocked,
}

struct Index<'source> {
    graph: &'source NormalizedGraph,
    provider: ProviderId,
    files: BTreeMap<&'source str, File<'source>>,
    rust: rust::Namespaces<'source>,
    javascript: javascript::PackageModes<'source>,
    owners: external::Owners,
    edge_python: python_edges::EdgePythonModules,
    edge_python_classes: python_edges::ClassOrders<'source>,
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
        documents: &'source [StaticSource<'source>],
        namespace: &NamespaceInput<'_>,
        work: &mut Work,
    ) -> Option<Self> {
        let mut files = BTreeMap::new();
        let configurations = crate::analyzer::namespace::RustConfigurations::new(namespace);
        let mut unavailable_sources = 0;
        for document in documents {
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
                work,
            )?;
            if files.insert(document.path.as_str(), file).is_some() {
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
        let rust = rust::Namespaces::new(&files, work)?;
        let javascript = javascript::PackageModes::new(namespace, &files, work)?;
        let owners = external::Owners::new(namespace, work)?;
        let provider = ProviderId::new(SYNTAX_PROVIDER_ID).ok()?;
        let syntax_publication = graph.publications().provider(&provider)?.revision();
        let mut index = Self {
            graph,
            provider,
            files,
            rust,
            javascript,
            owners,
            edge_python,
            edge_python_classes: python_edges::ClassOrders::default(),
            python_writes,
            syntax_publication,
            unavailable_sources,
            #[cfg(test)]
            performed: std::sync::atomic::AtomicUsize::new(0),
        };
        index.edge_python_classes = index.python_class_orders(work)?;
        Some(index)
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
                resolution.join(file, apart.resolution, over_share);
            }
        }
    }

    /// One file's pass under its own budget, with a result of its own.
    fn resolve_file_apart(
        &self,
        file: &File<'source>,
        budget: usize,
        edge_calls: &BTreeMap<rift_core::SourceUnitId, EdgeCallCoverage>,
    ) -> FileResolution {
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
        self.resolve_file(file, &mut work, &mut resolution);
        let used = budget.saturating_sub(work.budget.remaining());
        #[cfg(test)]
        self.performed
            .fetch_add(used, std::sync::atomic::Ordering::Relaxed);
        FileResolution {
            resolution,
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

    fn resolve_file(&self, file: &File<'source>, work: &mut Work, result: &mut StaticResolution) {
        for range in file.facts.unsupported_bindings() {
            if file.inactive(*range, work) {
                continue;
            }
            if !work.charge() {
                return;
            }
            if let Some(coverage) = result.edge_calls.get_mut(file.document.placement.unit()) {
                coverage.unresolved.push(*range);
            }
        }
        if !self.resolve_file_calls(file, work, result) {
            return;
        }
        for edge in file.facts.heritage_edges() {
            if file.inactive(edge.from, work) {
                continue;
            }
            if !work.charge() {
                return;
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
        let mut written_python_imports = BTreeSet::new();
        for binding in file.facts.bindings() {
            if file.inactive(binding.name, work) {
                continue;
            }
            if !work.charge() {
                return;
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
                result.record_kind(occurrence, rift_protocol::read::EdgeKind::Imports);
                if let Some(written) = written
                    && written_python_imports.insert(written.callee_range)
                {
                    result.record_kind(Some(written), rift_protocol::read::EdgeKind::Imports);
                }
            }
        }
        if matches!(
            file.document.facts.language().name.as_str(),
            "javascript" | "typescript"
        ) {
            self.javascript_edges(file, work, result);
        }
        if file.document.facts.language().name == "python" {
            self.python_references(file, work, result);
        }
        if file.document.facts.language().name == "rust" {
            self.rust_references(file, work, result);
        } else if file.document.facts.language().name == "python" {
            self.named_type_references(file, work, result);
        }
        if !work.exhausted {
            result.analyzed_sources += 1;
        }
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
        let target = if file.document.facts.language().name == "python" {
            self.edge_python_base(file, edge.target, work)?
        } else {
            self.edge_type_target(file, edge.target, work)?
        };
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
            "python" => self.edge_python_name(file, range, work),
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
        let address = target.symbol.provider_address(
            &target.file.facts.language().identity_segment(),
            target.file.placement.identity_path(),
        );
        let reference = ContributionReference::new(
            self.provider.clone(),
            ProviderSymbolId::for_symbol(&address).ok()?,
        );
        let key = rift_core::ContributionKey::new(
            self.provider.clone(),
            self.syntax_publication,
            reference.symbol().clone(),
        );
        let target_binding = self.graph.contribution(&key)?.source()?;
        if target_binding.unit() != target.file.placement.unit()
            || target_binding.range().start() != target.symbol.item_range.start
            || target_binding.range().end() != target.symbol.item_range.end
        {
            return None;
        }
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
    /// Adds one file's result after the results of the files before it. A file whose
    /// pass ended on its own share keeps what it resolved and reports unavailable call
    /// coverage for its unit.
    fn join(&mut self, file: &File<'_>, mut apart: Self, over_share: bool) {
        self.occurrences.append(&mut apart.occurrences);
        self.external.append(&mut apart.external);
        for (kind, count) in apart.omitted {
            let total = self.omitted.entry(kind).or_default();
            *total = total.saturating_add(count);
        }
        self.analyzed_sources += apart.analyzed_sources;
        self.unresolved_sites += apart.unresolved_sites;
        if over_share {
            self.unavailable_sources += 1;
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
