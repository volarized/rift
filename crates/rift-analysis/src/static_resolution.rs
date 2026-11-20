//! Resolves captured call and import sites against admitted source declarations.

mod javascript;
mod javascript_edges;
mod javascript_exports;
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

/// Runs one static pass over a captured source selection.
pub(crate) fn resolve(
    graph: &NormalizedGraph,
    documents: &[StaticSource<'_>],
    namespace: &NamespaceInput<'_>,
    work_max: usize,
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
    for file in index.files.values() {
        if !work.charge() {
            break;
        }
        index.resolve_file(file, &mut work, &mut resolution);
    }
    resolution.exhausted = work.exhausted;
    resolution
}

#[derive(Debug)]
struct Work {
    budget: LoopBudget,
    exhausted: bool,
}

impl Work {
    fn new(maximum: usize) -> Self {
        Self {
            budget: LoopBudget::new(maximum),
            exhausted: false,
        }
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

struct File<'source> {
    document: &'source StaticSource<'source>,
    facts: &'source StaticFacts,
    by_name: Bindings<'source>,
    by_range: BTreeMap<ByteRange, Option<&'source SyntaxSymbol>>,
    by_qualified_name: BTreeMap<&'source str, Vec<&'source SyntaxSymbol>>,
    callables: EnclosingDefinitions<&'source SyntaxSymbol>,
    assignments: AssignmentTargets<'source>,
    declared_types: BTreeMap<ByteRange, Option<ByteRange>>,
    members: BTreeMap<ByteRange, Option<&'source rift_syntax::StaticMember>>,
    export_writes: BTreeMap<javascript_exports::ExportSlot<'source>, Option<u64>>,
}

#[derive(Default)]
struct AssignmentTargets<'source> {
    names: BTreeSet<&'source str>,
    members: BTreeSet<(&'source str, &'source str)>,
    properties: BTreeSet<&'source str>,
    unknown: bool,
}

impl<'source> AssignmentTargets<'source> {
    fn add(&mut self, target: &'source str, work: &mut Work) -> Option<()> {
        let target = target.trim();
        if target.starts_with('(') || target.contains(['[', ']', '\\', '/']) {
            self.unknown = true;
        }
        let Some((receiver, path)) = target.split_once('.') else {
            self.names.insert(target);
            return Some(());
        };
        let mut properties = path.split('.').map(str::trim);
        if let Some(first) = properties.next() {
            self.members.insert((receiver.trim(), first));
            self.properties.insert(first);
        }
        for property in properties {
            if !work.charge() {
                return None;
            }
            self.properties.insert(property);
        }
        Some(())
    }

    fn may_replace_method(&self, method: &str) -> bool {
        self.unknown
            || [method, "constructor", "prototype"]
                .iter()
                .any(|property| self.properties.contains(property))
    }
}

impl<'source> File<'source> {
    fn new(
        document: &'source StaticSource<'source>,
        facts: &'source StaticFacts,
        work: &mut Work,
    ) -> Option<Self> {
        let mut file = Self {
            document,
            facts,
            by_name: BTreeMap::new(),
            by_range: BTreeMap::new(),
            by_qualified_name: BTreeMap::new(),
            callables: EnclosingDefinitions::new([]),
            assignments: AssignmentTargets::default(),
            declared_types: BTreeMap::new(),
            members: BTreeMap::new(),
            export_writes: BTreeMap::new(),
        };
        file.index_bindings(work)?;
        file.index_symbols(work)?;
        file.index_javascript_members(work)?;
        Some(file)
    }

    fn index_bindings(&mut self, work: &mut Work) -> Option<()> {
        for binding in self.facts.bindings() {
            if !work.charge() {
                return None;
            }
            let name = self.text(binding.name)?;
            self.by_name.entry(name).or_default().push(binding);
        }
        for &(name, annotation) in self.facts.declared_types() {
            if !work.charge() {
                return None;
            }
            self.declared_types
                .entry(name)
                .and_modify(|entry| *entry = None)
                .or_insert(Some(annotation));
        }
        for range in self.facts.assignments() {
            if !work.charge() {
                return None;
            }
            self.assignments.add(self.text(*range)?, work)?;
        }
        Some(())
    }

    fn index_symbols(&mut self, work: &mut Work) -> Option<()> {
        let mut callables = Vec::new();
        for symbol in self.document.facts.symbols() {
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
        Some(())
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
        if candidates.len() != 1 {
            return None;
        }
        Some(Target {
            file: self.document,
            symbol: candidates[0],
        })
    }

    fn blocked(&self, position: u64, work: &mut Work) -> bool {
        self.facts
            .unsupported_bindings()
            .iter()
            .any(|scope| !work.charge() || scope.contains(position))
    }

    fn binding(
        &self,
        name: &str,
        position: u64,
        accepts: impl Fn(StaticBindingKind) -> bool,
        work: &mut Work,
    ) -> Binding<'source> {
        let Some(bindings) = self.by_name.get(name) else {
            return Binding::Missing;
        };
        let mut best: Option<&StaticBinding> = None;
        let mut ambiguous = false;
        for &binding in bindings {
            if !work.charge() {
                return Binding::Blocked;
            }
            if !binding.scope.contains(position) || !accepts(binding.kind) {
                continue;
            }
            let rust = self.document.facts.language().name == "rust";
            if rust && binding.available_from > position {
                continue;
            }
            match best {
                Some(current)
                    if rust
                        && current.scope == binding.scope
                        && binding.available_from > current.available_from =>
                {
                    best = Some(binding);
                    ambiguous = false;
                }
                Some(current)
                    if rust
                        && current.scope == binding.scope
                        && binding.available_from < current.available_from => {}
                Some(current) if current.scope == binding.scope => ambiguous = true,
                Some(current) if !current.scope.encloses(binding.scope) => {}
                _ => {
                    best = Some(binding);
                    ambiguous = false;
                }
            }
        }
        match best {
            None => Binding::Missing,
            Some(binding) if !ambiguous && binding.available_from <= position => {
                Binding::Found(binding)
            }
            Some(_) => Binding::Blocked,
        }
    }

    fn reassigned(&self, name: &str, member: Option<&str>) -> bool {
        self.assignments.unknown
            || self.assignments.names.contains(name)
            || member.is_some_and(|member| self.assignments.members.contains(&(name, member)))
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
    edge_python: python_edges::EdgePythonModules,
    syntax_publication: rift_core::ProviderRevision,
    unavailable_sources: usize,
}

impl<'source> Index<'source> {
    fn new(
        graph: &'source NormalizedGraph,
        documents: &'source [StaticSource<'source>],
        namespace: &NamespaceInput<'_>,
        work: &mut Work,
    ) -> Option<Self> {
        let mut files = BTreeMap::new();
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
            let file = File::new(document, facts, work)?;
            if files.insert(document.path.as_str(), file).is_some() {
                return None;
            }
        }
        let edge_python = python_edges::EdgePythonModules::build_edge(&files, work)?;
        let rust = rust::Namespaces::new(&files, work)?;
        let javascript = javascript::PackageModes::new(namespace, &files, work)?;
        let provider = ProviderId::new(SYNTAX_PROVIDER_ID).ok()?;
        let syntax_publication = graph.publications().provider(&provider)?.revision();
        Some(Self {
            graph,
            provider,
            files,
            rust,
            javascript,
            edge_python,
            syntax_publication,
            unavailable_sources,
        })
    }

    fn resolve_file(&self, file: &File<'source>, work: &mut Work, result: &mut StaticResolution) {
        for range in file.facts.unsupported_bindings() {
            if !work.charge() {
                return;
            }
            if let Some(coverage) = result.edge_calls.get_mut(file.document.placement.unit()) {
                coverage.unresolved.push(*range);
            }
        }
        for call in file.facts.calls() {
            if !work.charge() {
                return;
            }
            let target = self.call_target(file, call, work);
            let mut occurrence = target.and_then(|target| {
                self.occurrence(
                    file,
                    call.range,
                    call.callee_range,
                    ReferenceRole::Call,
                    target,
                )
            });
            if matches!(
                file.document.facts.language().name.as_str(),
                "javascript" | "typescript"
            ) && let Some(occurrence) = &mut occurrence
            {
                occurrence.edge_from = self.javascript_call_from(file, call.range, work);
            }
            if occurrence
                .as_ref()
                .is_none_or(|occurrence| !matches!(occurrence.target, NormalizedTarget::Symbol(_)))
                && let Some(coverage) = result.edge_calls.get_mut(file.document.placement.unit())
            {
                coverage.unresolved.push(call.range);
            }
            result.record(occurrence);
        }
        for edge in file.facts.heritage_edges() {
            if !work.charge() {
                return;
            }
            let occurrence = self.edge_heritage_occurrence(file, edge, work);
            result.record(occurrence);
        }
        for binding in file.facts.bindings() {
            if !work.charge() {
                return;
            }
            if let Some((range, target)) = self.import_target(file, binding, work) {
                let occurrence = target.and_then(|target| {
                    self.occurrence(file, range, range, ReferenceRole::Import, target)
                });
                result.record(occurrence);
            }
        }
        if matches!(
            file.document.facts.language().name.as_str(),
            "javascript" | "typescript"
        ) {
            self.javascript_edges(file, work, result);
        }
        if file.document.facts.language().name == "rust" {
            self.rust_references(file, work, result);
        }
        if !work.exhausted {
            result.analyzed_sources += 1;
        }
    }

    fn edge_heritage_occurrence(
        &self,
        file: &File<'source>,
        edge: &rift_syntax::HeritageEdge,
        work: &mut Work,
    ) -> Option<StaticOccurrence> {
        let target = self.edge_type_target(file, edge.target, work)?;
        let from = if file.document.facts.language().name == "rust" {
            self.edge_type_target(file, edge.from, work)?
        } else {
            file.target_at(edge.from)
                .or_else(|| self.edge_type_target(file, edge.from, work))?
        };
        let mut occurrence =
            self.occurrence(file, edge.target, edge.target, ReferenceRole::Type, target)?;
        let from = self.occurrence(file, edge.from, edge.from, ReferenceRole::Type, from)?;
        if let NormalizedTarget::Symbol(from) = from.target {
            occurrence.edge_from = Some(from);
        }
        occurrence.edge_kind = Some(edge.kind);
        Some(occurrence)
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
            | StaticBindingKind::JavaScriptRequire { .. } => {
                Some((binding.name, self.javascript_import(file, binding, work)))
            }
            StaticBindingKind::RustUse { path } => Some((path, self.rust.import(file, path, work))),
            StaticBindingKind::PythonImport {
                export: Some(_), ..
            } => Some((
                binding.name,
                self.edge_python_binding_target(file, binding, work),
            )),
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
                    "javascript" | "typescript"
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
