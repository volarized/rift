//! Written arguments of Calls owned by admitted anonymous JavaScript callbacks.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::PortableSymbolFacts;
use rift_error::RiftError;
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::index::{PackageAnalysisWarning, PackageSymbol, StoredEdge};
use rift_protocol::read::{
    Digest, EdgeDerivation, EdgeKind, EdgeProvider, ExactKind, Extensions, SourceUnitSpan,
    SymbolId, SymbolKind, TextRange,
};
use rift_provider::AssembledSymbol;
use rift_syntax::{ByteRange, StaticCallee, SyntaxSymbol};

use super::{Callback, CallbackFile, source_text, stored_callback_edge_key};
use crate::ExactPackageLimits;
use crate::analyzer::{AnalyzedFile, Records, bound, digest_of, line_of, truncation, wire_path};

struct ArgumentProperty<'file, 'source> {
    file: &'file CallbackFile<'source>,
    property: &'file SyntaxSymbol,
    name: ByteRange,
    callback: SymbolId,
}

impl Records {
    /// Places argument properties after the earlier declarations and their edges.
    pub(in crate::analyzer) fn retain_javascript_callback_argument_properties(
        &mut self,
        owner: &SymbolOwner,
        analyzed: &[AnalyzedFile],
        limits: ExactPackageLimits,
    ) -> Result<(), RiftError> {
        if self.symbols_truncated
            || !matches!(owner, SymbolOwner::Package { manager, .. } if manager == "npm")
        {
            return Ok(());
        }
        let files = self.ecmascript_callback_files(analyzed, EdgeKind::Calls);
        let mut occupied = BTreeSet::new();
        let mut callbacks = BTreeMap::new();
        for object in &self.objects {
            if let Some(id) = &object.id {
                occupied.insert(id.clone());
                if object.document_local
                    && object.name.starts_with("$callback#")
                    && object.kinds.contains(&SymbolKind::Callable)
                {
                    callbacks.insert(id.as_str(), object);
                }
            }
        }
        if callbacks.is_empty() {
            return Ok(());
        }
        let mut declarations = BTreeMap::new();
        let mut placed = BTreeMap::<_, BTreeSet<_>>::new();
        for declaration in &self.declarations {
            let range = ByteRange {
                start: declaration.range.start,
                end: declaration.range.end,
            };
            placed.entry(&declaration.unit).or_default().insert(range);
            if callbacks.contains_key(declaration.symbol.as_str()) {
                declarations
                    .entry(declaration.symbol.as_str())
                    .and_modify(|candidate| *candidate = None)
                    .or_insert(Some(declaration));
            }
        }
        let files_by_unit = files
            .values()
            .filter(|file| file.file.file.syntax().language().name == "javascript")
            .map(|file| (file.unit.as_str(), file))
            .collect::<BTreeMap<_, _>>();
        let mut calls = BTreeMap::<_, BTreeMap<_, Option<SymbolId>>>::new();
        for edge in &self.edges {
            let Some((unit, start, end, caller, _)) =
                stored_callback_edge_key(edge, EdgeKind::Calls)
            else {
                continue;
            };
            let (Some(file), Some(object), Some(Some(declaration))) = (
                files_by_unit.get(unit),
                callbacks.get(caller),
                declarations.get(caller),
            ) else {
                continue;
            };
            let Some(Some(callback)) = file.scopes.resolve(start, end) else {
                continue;
            };
            if declaration.unit != file.unit
                || declaration.range != wire_range(callback.range)
                || !declaration.source_complete
                || !declaration.source.ends_with('}')
                || source_text(file.file.file.source(), callback.range)
                    != Some(declaration.source.as_str())
                || object.container.as_ref().map(SymbolId::as_str) != Some(file.module.as_str())
                || object.name != format!("$callback#{}", callback.ordinal)
            {
                continue;
            }
            calls
                .entry(unit)
                .or_default()
                .entry(ByteRange { start, end })
                .and_modify(|candidate| *candidate = None)
                .or_insert_with(|| Some(edge.from.clone()));
        }
        let mut candidates = Vec::new();
        for (unit, calls) in calls {
            let file = files_by_unit[unit];
            if let Some(placed) = placed.get(&file.unit) {
                let caller = |range| calls.get(&range).and_then(Option::as_ref);
                for (property, name, callback) in argument_properties(file, caller, placed) {
                    candidates.push(ArgumentProperty {
                        file,
                        property,
                        name,
                        callback: callback.clone(),
                    });
                }
            }
        }
        self.publish_callback_argument_properties(owner, limits, candidates, &mut occupied)
    }

    /// Retains written literal properties under an exact anonymous function argument.
    pub(in crate::analyzer) fn retain_written_callback_argument_properties(
        &mut self,
        owner: &SymbolOwner,
        analyzed: &[AnalyzedFile],
        limits: ExactPackageLimits,
    ) -> Result<(), RiftError> {
        if self.symbols_truncated
            || !matches!(owner, SymbolOwner::Package { manager, .. } if manager == "npm")
        {
            return Ok(());
        }
        let files = self.ecmascript_callback_files(analyzed, EdgeKind::Calls);
        let mut placed = BTreeMap::<_, BTreeSet<_>>::new();
        for row in &self.declarations {
            let range = ByteRange {
                start: row.range.start,
                end: row.range.end,
            };
            placed.entry(row.unit.clone()).or_default().insert(range);
        }
        let mut occupied = self
            .objects
            .iter()
            .filter_map(|row| row.id.clone())
            .collect();
        let declaration_limit = limits
            .declarations_max()
            .min(bound(self.publication.symbols));
        for file in files
            .values()
            .filter(|file| file.file.file.syntax().language().name == "javascript")
        {
            let Some(placed) = placed.get(&file.unit) else {
                continue;
            };
            let mut callers = BTreeMap::<_, Option<SymbolId>>::new();
            let caller = |range| written_callback(file, range);
            for (property, name, callback) in argument_properties(file, caller, placed) {
                if self.symbols_truncated {
                    return Ok(());
                }
                let id = match callers.entry(callback.range) {
                    std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(self.publish_ecmascript_callback(
                            owner,
                            file,
                            callback,
                            &occupied,
                            declaration_limit,
                            false,
                        )?)
                    }
                };
                if let Some(id) = id {
                    occupied.insert(id.clone());
                    self.publish_callback_argument_properties(
                        owner,
                        limits,
                        [ArgumentProperty {
                            file,
                            property,
                            name,
                            callback: id.clone(),
                        }],
                        &mut occupied,
                    )?;
                }
            }
        }
        Ok(())
    }

    fn publish_callback_argument_properties<'file, 'source: 'file>(
        &mut self,
        owner: &SymbolOwner,
        limits: ExactPackageLimits,
        candidates: impl IntoIterator<Item = ArgumentProperty<'file, 'source>>,
        occupied: &mut BTreeSet<SymbolId>,
    ) -> Result<(), RiftError> {
        let symbol_limit = bound(self.publication.symbols);
        let declaration_limit = limits.declarations_max().min(symbol_limit);
        for ArgumentProperty {
            file,
            property,
            name,
            callback,
        } in candidates
        {
            let parent = SymbolIdentity::parse(callback.as_str())?;
            let mut path = parent.qualified_path().to_vec();
            path.push(format!(
                "{}#{}",
                property.name, property.declaration_ordinal
            ));
            let language = parent.language();
            let Ok(identity) = SymbolIdentity::new(owner.clone(), language.clone(), path) else {
                continue;
            };
            let id = SymbolId(identity.wire_identity());
            if !occupied.insert(id.clone()) {
                continue;
            }
            if self.objects.len() >= symbol_limit || self.declarations.len() >= declaration_limit {
                self.symbols_truncated = true;
                self.warn(truncation(
                    "declarations",
                    u64::try_from(declaration_limit).unwrap_or(u64::MAX),
                ));
                break;
            }
            let Some(source) = source_text(file.file.file.source(), property.range) else {
                continue;
            };
            let path = wire_path(file.file.file.path());
            let bytes = property.range.end - property.range.start;
            if !self.retained_source_fits(bytes) {
                self.warn(PackageAnalysisWarning::SourceTruncated {
                    path,
                    dropped: bytes,
                });
                continue;
            }
            let facts = PortableSymbolFacts::new(
                language.clone(),
                &property.name,
                &property.qualified_name,
                ExactKind(property.kind.to_owned()),
            )
            .kinds(property.kinds.clone());
            let mut object = AssembledSymbol::wire_facts(&facts, file.file.placement.origin());
            object.id = Some(id.clone());
            object.container = Some(callback.clone());
            object.document_local = true;
            let retained = self.retained(source, &path, 1)?;
            let mut declaration = PackageSymbol {
                symbol: id.clone(),
                origin: object.origin.clone(),
                unit: file.unit.clone(),
                range: wire_range(property.range),
                syntax_element_kind: property.syntax_element_kind.map(str::to_owned),
                line: line_of(&file.lines, property.range.start),
                signature_indices: Vec::new(),
                type_indices: Vec::new(),
                documentation_indices: Vec::new(),
                source: retained.text,
                source_complete: retained.complete,
                public: false,
                digest: Digest(String::new()),
            };
            declaration.digest = digest_of(&declaration, owner)?;
            self.objects.push(object);
            self.declarations.push(declaration);
            if self.edges.len() < limits.edges_max() {
                self.edges.push(StoredEdge {
                    producer: None,
                    from: callback,
                    to: id,
                    kind: ExactKind(EdgeKind::Contains.as_ref().to_owned()),
                    kinds: vec![EdgeKind::Contains],
                    occurrence: Some(SourceUnitSpan {
                        unit: file.unit.clone(),
                        range: wire_range(name),
                    }),
                    provider: EdgeProvider::Static,
                    derivation: EdgeDerivation::Resolution,
                    confidence: None,
                    extensions: Extensions::default(),
                });
            } else {
                self.warn(truncation(
                    "edges",
                    u64::try_from(limits.edges_max()).unwrap_or(u64::MAX),
                ));
            }
        }
        Ok(())
    }
}

fn written_callback(file: &CallbackFile<'_>, range: ByteRange) -> Option<Callback> {
    let callback = *file.scopes.resolve(range.start, range.end)?.as_ref()?;
    source_text(file.file.file.source(), callback.range)?
        .ends_with('}')
        .then_some(callback)
}

fn argument_properties<'file, Owner: Copy>(
    file: &'file CallbackFile<'_>,
    caller: impl Fn(ByteRange) -> Option<Owner>,
    placed: &BTreeSet<ByteRange>,
) -> Vec<(&'file SyntaxSymbol, ByteRange, Owner)> {
    let syntax = file.file.file.syntax();
    let Some(facts) = syntax.static_facts() else {
        return Vec::new();
    };
    let source = file.file.file.source();
    let mut calls = BTreeMap::new();
    for call in facts.calls() {
        if let Some(callback) = caller(call.range)
            && matches!(
                call.callee,
                StaticCallee::Direct | StaticCallee::Member { .. }
            )
        {
            calls
                .entry(call.range.end)
                .and_modify(|candidate| *candidate = None)
                .or_insert(Some((call, callback)));
        }
    }
    let members = facts
        .body_members()
        .iter()
        .map(|member| (member.member, member))
        .collect::<BTreeMap<_, _>>();
    let mut names = BTreeMap::<&str, usize>::new();
    let mut fields = BTreeMap::new();
    for property in syntax
        .symbols()
        .iter()
        .filter(|symbol| symbol.kind == "property")
    {
        *names.entry(property.qualified_name.as_str()).or_default() += 1;
        let (Some(member), Some(name)) = (members.get(&property.item_range), property.name_range)
        else {
            continue;
        };
        let Some(Some((call, callback))) = calls.get(&member.holder.end) else {
            continue;
        };
        if property.syntax_element_kind != Some("pair")
            || property.kinds.contains(&SymbolKind::Public)
            || !member.body.encloses(property.item_range)
            || !member.holder.encloses(member.body)
            || !call.range.encloses(member.holder)
            || call.callee_range.end > member.holder.start
            || !source_text(source, member.holder)
                .is_some_and(|text| text.starts_with('(') && text.ends_with(')'))
            || source_text(source, name) != Some(property.name.as_str())
            || property.name.contains(['\'', '"', '`', '['])
        {
            continue;
        }
        fields
            .entry((member.body, property.name.as_str()))
            .and_modify(|candidate| *candidate = None)
            .or_insert(Some((property, name, *callback)));
    }
    fields
        .into_values()
        .flatten()
        .filter(|(property, _, _)| {
            names[property.qualified_name.as_str()] > 1 && !placed.contains(&property.range)
        })
        .collect()
}

fn wire_range(range: ByteRange) -> TextRange {
    TextRange {
        start: range.start,
        end: range.end,
    }
}
