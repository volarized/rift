//! Python containment, member accesses, overrides, and annotation uses.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::{ReferenceRole, SymbolId};
use rift_protocol::read::{EdgeKind, SymbolKind};
use rift_provider::NormalizedTarget;
use rift_syntax::{ByteRange, StaticReference};

use super::super::{File, Index, StaticOccurrence, StaticResolution, Target, Work};
use super::Value;

/// Plain names read after member, annotation, and containment facts have been resolved.
pub(in crate::static_resolution) struct PythonNameSites {
    names: Vec<StaticReference>,
    path_starts: BTreeMap<u64, bool>,
}

impl<'source> Index<'source> {
    pub(in crate::static_resolution) fn python_references(
        &self,
        file: &File<'source>,
        work: &mut Work,
        result: &mut StaticResolution,
    ) -> Option<PythonNameSites> {
        let callees = established_callees(result);
        let mut path_starts = callees
            .keys()
            .map(|callee| (callee.start, true))
            .collect::<BTreeMap<_, _>>();
        let mut names = Vec::new();
        for reference in file.facts.references() {
            // A member access spells its receiver. A name alone states no member.
            if reference.role != ReferenceRole::Type
                && file
                    .text(reference.target)
                    .is_some_and(|text| !text.contains('.'))
            {
                names.push(*reference);
                continue;
            }
            if !work.charge() {
                return None;
            }
            let target = if reference.role == ReferenceRole::Type {
                self.python_type(file, reference.target, work)
            } else {
                self.edge_python_name(file, reference.target, work)
            };
            let occurrence = target
                .or_else(|| {
                    (reference.role == ReferenceRole::Write)
                        .then(|| self.python_written_module_name(file, reference.target, work))
                        .flatten()
                })
                .and_then(|target| {
                    self.python_access(file, reference.target, reference.role, target)
                });
            if occurrence.is_some() {
                path_starts.entry(reference.target.start).or_insert(false);
            }
            if !names_callee(&callees, reference, occurrence.as_ref()) {
                result.record(occurrence);
            }
        }
        for symbol in file.document.facts.symbols() {
            if symbol.is_package_binding() {
                continue;
            }
            if !work.charge() {
                return None;
            }
            if symbol.qualified_name == "." {
                continue;
            }
            let Some(range) = symbol
                .name_range
                .or(Some(symbol.item_range))
                .filter(|range| range.start < range.end)
            else {
                continue;
            };
            let target = Target {
                file: file.document,
                symbol,
            };
            let parent = file.qualified_target(symbol.container.as_deref().unwrap_or("."));
            if let Some(parent) = parent {
                result.record(self.python_edge(file, range, EdgeKind::Contains, parent, target));
                if symbol.kinds.contains(&SymbolKind::Callable)
                    && parent.symbol.kinds.contains(&SymbolKind::Constructible)
                    && let Some(base) = self.python_declared_base_member(parent, &symbol.name, work)
                    && base.symbol.kinds.contains(&SymbolKind::Callable)
                {
                    result.record(self.python_edge(file, range, EdgeKind::Overrides, target, base));
                }
            }
        }
        self.named_type_references(file, work, result);
        (!work.exhausted && !names.is_empty()).then_some(PythonNameSites { names, path_starts })
    }

    /// Resolve plain names and retain suppressed module qualifier sites for later.
    pub(in crate::static_resolution) fn python_name_references(
        &self,
        file: &File<'source>,
        sites: PythonNameSites,
        work: &mut Work,
        result: &mut StaticResolution,
    ) -> Vec<(ByteRange, bool)> {
        let mut qualifiers = Vec::new();
        for reference in sites.names {
            if !work.charge() {
                break;
            }
            let name = reference.target;
            if reference.role == ReferenceRole::Write {
                self.python_name_write(file, name, work, result);
            } else if let Some(&call) = sites.path_starts.get(&name.start)
                && file.python_path_starts(name)
            {
                qualifiers.push((name, call));
            } else {
                self.python_name_read(file, name, work, result);
            }
        }
        qualifiers
    }

    /// Resolves qualifiers after the established edge passes have run in every file.
    pub(in crate::static_resolution) fn python_module_qualifiers(
        &self,
        file: &File<'source>,
        imports: Vec<super::PythonImportQualifier<'source>>,
        names: Vec<(ByteRange, bool)>,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        let mut written = BTreeSet::new();
        for import in imports {
            if !work.charge() {
                return;
            }
            if !written.contains(&import.module)
                && let Some(occurrence) = self.python_import_qualifier(file, import)
            {
                written.insert(occurrence.callee_range);
                result.record(Some(occurrence));
            }
        }
        for (name, call) in names {
            if !work.charge() {
                return;
            }
            if call {
                self.python_value_qualifier(file, name, work, result);
            } else if let Some(occurrence) = self.python_member_module_qualifier(file, name, work) {
                result.record(Some(occurrence));
            }
        }
    }

    /// Resolves the written first module segment of a checked type path.
    pub(in crate::static_resolution) fn resolve_python_annotation_qualifiers(
        &self,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        result.python_annotation_qualifier_start = Some(result.occurrences.len());
        self.python_annotation_module_reads(work, result, AnnotationModule::Nested);
    }

    /// Resolves a checked module alias written directly beside a type name.
    pub(in crate::static_resolution) fn resolve_python_direct_annotation_qualifiers(
        &self,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        result.python_direct_annotation_qualifier_start = Some(result.occurrences.len());
        self.python_annotation_module_reads(work, result, AnnotationModule::Direct);
    }

    /// Adds a module Read beside a previously established legacy alias type occurrence.
    pub(in crate::static_resolution) fn resolve_python_legacy_annotation_qualifiers(
        &self,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        let files = self
            .python_files()
            .map(|file| (file.document.placement.unit(), file))
            .collect::<BTreeMap<_, _>>();
        let mut added = BTreeSet::new();
        let mut reads = Vec::new();
        for known in &result.occurrences {
            if known.role != ReferenceRole::Type
                || !matches!(known.target, NormalizedTarget::Symbol(_))
            {
                continue;
            }
            let Some(file) = files.get(known.binding.unit()) else {
                continue;
            };
            let range = known.callee_range;
            if known.binding.range().start() != range.start
                || known.binding.range().end() != range.end
                || file.document.facts.source_digest() != Some(&known.source_digest)
            {
                continue;
            }
            let Some(module) = AnnotationModule::Direct.module_range(file, range) else {
                continue;
            };
            if !work.charge() {
                break;
            }
            if !added.insert((known.binding.unit(), range)) {
                continue;
            }
            if let Some(target) = self.python_type(file, range, work)
                && !target.symbol.kinds.contains(&SymbolKind::Type)
                && self.python_legacy_type_alias(target, work)
                && self
                    .occurrence(file, range, range, ReferenceRole::Type, target)
                    .is_some_and(|site| {
                        site.target == known.target && site.target_binding == known.target_binding
                    })
                && let Some(read) = self.python_member_module_qualifier(file, module, work)
            {
                reads.push(read);
            }
        }
        if !reads.is_empty() {
            result.python_legacy_annotation_qualifier_start = Some(result.occurrences.len());
            result.occurrences.extend(reads);
        }
    }

    fn python_annotation_module_reads(
        &self,
        work: &mut Work,
        result: &mut StaticResolution,
        path: AnnotationModule,
    ) {
        for file in self.python_files() {
            for reference in file
                .facts
                .references()
                .iter()
                .filter(|reference| reference.role == ReferenceRole::Type)
            {
                let module = path.module_range(file, reference.target);
                if (path == AnnotationModule::Nested || module.is_some()) && !work.charge() {
                    return;
                }
                if let Some(root) = module
                    && let Some(qualifier) = self.python_member_module_qualifier(file, root, work)
                    && let Some(target) = self.python_type(file, reference.target, work)
                    && (path == AnnotationModule::Nested
                        || target.symbol.kinds.contains(&SymbolKind::Type))
                {
                    result.record(Some(qualifier));
                }
            }
        }
    }

    /// An access belongs to the enclosing callable, an annotation to its declaration.
    pub(super) fn python_access(
        &self,
        file: &File<'source>,
        range: ByteRange,
        role: ReferenceRole,
        target: Target<'source>,
    ) -> Option<StaticOccurrence> {
        let mut occurrence = self.occurrence(file, range, range, role, target)?;
        if role != ReferenceRole::Type
            && let Some(caller) = file.enclosing_callable(range)
        {
            occurrence.edge_from = self.python_identity(file, range, caller);
        }
        Some(occurrence)
    }

    /// The declaration a written type names. A module or a function is no type.
    pub(in crate::static_resolution) fn python_type(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        self.edge_python_name(file, range, work).filter(|target| {
            let kinds = &target.symbol.kinds;
            kinds.contains(&SymbolKind::Constructible)
                || !(kinds.contains(&SymbolKind::Module) || kinds.contains(&SymbolKind::Callable))
        })
    }

    /// The declaration a write to `module.name` names: the module's own declaration of `name`,
    /// even when other writes replace its value at runtime.
    fn python_written_module_name(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let (receiver, member) = file.text(range)?.rsplit_once('.')?;
        let receiver = ByteRange {
            start: range.start,
            end: range.start + u64::try_from(receiver.len()).ok()?,
        };
        let Value::Module(name) =
            work.python_lookup(|work| self.python_name(file, receiver, work))?
        else {
            return None;
        };
        let module = self.files.get(self.edge_python.path(&name)?)?;
        let mut declaration: Option<Target<'source>> = None;
        for binding in module.by_name.get(member.trim())? {
            if !work.charge() {
                return None;
            }
            if !module.python_module_binding(binding) {
                continue;
            }
            let target = module.target_at(binding.name)?;
            if declaration
                .is_some_and(|known| known.symbol.qualified_name != target.symbol.qualified_name)
            {
                return None;
            }
            declaration.get_or_insert(target);
        }
        declaration
    }

    pub(in crate::static_resolution) fn python_identity(
        &self,
        file: &File<'source>,
        range: ByteRange,
        target: Target<'source>,
    ) -> Option<SymbolId> {
        let occurrence = self.occurrence(file, range, range, ReferenceRole::Read, target)?;
        match occurrence.target {
            NormalizedTarget::Symbol(symbol) => Some(symbol),
            NormalizedTarget::Contribution(_) => None,
        }
    }

    fn python_edge(
        &self,
        file: &File<'source>,
        range: ByteRange,
        kind: EdgeKind,
        from: Target<'source>,
        to: Target<'source>,
    ) -> Option<StaticOccurrence> {
        let mut occurrence = self.occurrence(file, range, range, ReferenceRole::Read, to)?;
        occurrence.edge_kind = Some(kind);
        occurrence.edge_from = Some(self.python_identity(file, range, from)?);
        Some(occurrence)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AnnotationModule {
    Direct,
    Nested,
}

impl AnnotationModule {
    fn module_range(self, file: &File<'_>, range: ByteRange) -> Option<ByteRange> {
        let (first, tail) = file.text(range)?.split_once('.')?;
        let first = first.trim_end();
        if tail.contains('.') != (self == Self::Nested)
            || !rift_protocol::index::python_identifier_is_valid(first)
            || (self == Self::Direct
                && !rift_protocol::index::python_identifier_is_valid(tail.trim()))
        {
            return None;
        }
        Some(ByteRange {
            start: range.start,
            end: range.start.checked_add(u64::try_from(first.len()).ok()?)?,
        })
    }
}

/// The callees whose calls in one file have a target, with the declaration each call names
/// in the selection.
fn established_callees(result: &StaticResolution) -> BTreeMap<ByteRange, Option<SymbolId>> {
    let external = result.external.iter();
    let mut callees = external
        .filter(|call| call.role == ReferenceRole::Call)
        .map(|call| (call.callee_range, None))
        .collect::<BTreeMap<_, _>>();
    for call in &result.occurrences {
        if call.role == ReferenceRole::Call
            && let NormalizedTarget::Symbol(called) = &call.target
        {
            callees.insert(call.callee_range, Some(called.clone()));
        }
    }
    callees
}

/// Whether the read of a callee names what its call names, or names nothing beside it. A
/// callee that is read through another name, such as an attribute that holds the called
/// class, is a fact of its own.
fn names_callee(
    callees: &BTreeMap<ByteRange, Option<SymbolId>>,
    reference: &StaticReference,
    occurrence: Option<&StaticOccurrence>,
) -> bool {
    let Some(called) = callees.get(&reference.target) else {
        return false;
    };
    let read = occurrence.map(|occurrence| &occurrence.target);
    reference.role == ReferenceRole::Read
        && match (called, read) {
            (Some(called), Some(NormalizedTarget::Symbol(read))) => called == read,
            (_, read) => read.is_none(),
        }
}

impl File<'_> {
    /// Whether the name is the first of a member path.
    fn python_path_starts(&self, name: ByteRange) -> bool {
        let rest = usize::try_from(name.end)
            .ok()
            .and_then(|end| self.document.source.get(end..));
        rest.is_some_and(|rest| {
            rest.trim_start_matches(|character: char| {
                character.is_whitespace() || character == '\\'
            })
            .starts_with('.')
        })
    }
}
