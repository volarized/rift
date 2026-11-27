//! Physical caller ownership for directly passed anonymous ECMAScript functions.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::{PortableSymbolFacts, SourceUnitId as CoreSourceUnitId, SymbolId as CoreSymbolId};
use rift_error::RiftError;
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::index::{PackageAnalysisWarning, PackageSymbol, StoredEdge};
use rift_protocol::read::{
    Digest, EdgeKind, EdgeProvider, ExactKind, SymbolId, SymbolKind, TextRange,
};
use rift_provider::AssembledSymbol;
use rift_syntax::{ByteRange, StaticFunctionContext};

use crate::{EnclosingDefinitions, ExactPackageLimits, WorkspaceSemantics, edge::Edge};

use super::{
    AnalyzedFile, Records, bound, digest_of, line_of, line_starts, truncation, wire_path, wire_unit,
};

/// One written function value and its ordinal among the unit's function expressions.
#[derive(Clone, Copy)]
struct Callback {
    range: ByteRange,
    ordinal: usize,
    kind: &'static str,
}

struct CallbackFile<'file> {
    file: &'file AnalyzedFile,
    module: &'file CoreSymbolId,
    unit: rift_protocol::read::SourceUnitId,
    lines: Vec<usize>,
    scopes: EnclosingDefinitions<Option<Callback>>,
    member_receivers: BTreeMap<ByteRange, ByteRange>,
}

struct CallbackEdge<'call, 'file> {
    site: &'call Edge,
    file: &'call CallbackFile<'file>,
    caller: Callback,
    existing: Option<usize>,
}

type CallbackEdgeKey<'site> = (&'site str, u64, u64, &'site str, &'site str);

impl Records {
    /// Uses already resolved targets, and corrects admitted sites before adding any more edges.
    pub(super) fn retain_ecmascript_callback_edges(
        &mut self,
        owner: &SymbolOwner,
        analyzed: &[AnalyzedFile],
        semantics: &WorkspaceSemantics,
        limits: ExactPackageLimits,
        kind: EdgeKind,
    ) -> Result<(), RiftError> {
        if self.symbols_truncated {
            return Ok(());
        }
        let files = self.ecmascript_callback_files(analyzed, kind);
        if files.is_empty() {
            return Ok(());
        }
        let objects = self
            .objects
            .iter()
            .filter_map(|object| object.id.clone())
            .collect::<BTreeSet<_>>();
        let mut edges = checked_callback_edges(&files, semantics, &objects, kind);
        if kind == EdgeKind::Reads {
            retain_checked_member_reads(&mut edges);
        }
        for (index, edge) in self.edges.iter().enumerate() {
            if let Some(key) = stored_callback_edge_key(edge, kind)
                && let Some(candidate) = edges.get_mut(&key)
            {
                candidate.existing = Some(index);
            }
        }
        let mut callers = BTreeMap::<_, Option<SymbolId>>::new();
        let declaration_limit = limits
            .declarations_max()
            .min(bound(self.publication.symbols));
        let edges = edges
            .into_values()
            .filter(|edge| edge.existing.is_some() || edge.site.from() == edge.file.module)
            .collect::<Vec<_>>();
        for existing in [true, false] {
            for edge in edges
                .iter()
                .filter(|edge| edge.existing.is_some() == existing)
            {
                if !existing && self.edges.len() >= limits.edges_max() {
                    self.warn(truncation(
                        "edges",
                        u64::try_from(limits.edges_max()).unwrap_or(u64::MAX),
                    ));
                    return Ok(());
                }
                let key = (&edge.file.unit, edge.caller.range);
                let caller = match callers.entry(key) {
                    std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(self.publish_ecmascript_callback(
                            owner,
                            edge.file,
                            edge.caller,
                            &objects,
                            declaration_limit,
                            kind == EdgeKind::Reads,
                        )?)
                    }
                };
                if let Some(caller) = caller {
                    if let Some(index) = edge.existing {
                        self.edges[index].from.clone_from(caller);
                    } else {
                        let mut edge = edge.site.to_protocol();
                        edge.from.clone_from(caller);
                        self.edges.push(edge);
                    }
                }
            }
        }
        Ok(())
    }

    fn ecmascript_callback_files<'file>(
        &self,
        analyzed: &'file [AnalyzedFile],
        kind: EdgeKind,
    ) -> BTreeMap<&'file CoreSourceUnitId, CallbackFile<'file>> {
        let modules = self
            .objects
            .iter()
            .filter(|object| object.kinds.contains(&SymbolKind::Module))
            .filter_map(|object| object.id.as_ref())
            .collect::<BTreeSet<_>>();
        let mut complete = BTreeMap::new();
        for row in &self.declarations {
            if row.source_complete && modules.contains(&row.symbol) {
                complete
                    .entry(row.symbol.as_str())
                    .and_modify(|candidate| *candidate = None)
                    .or_insert(Some(row));
            }
        }
        analyzed
            .iter()
            .filter_map(|file| {
                let origin = file.placement.unit();
                let unit = wire_unit(origin);
                let module = file.placement.logical_identity(".")?;
                let declaration = (*complete.get(module.as_str())?)?;
                if declaration.unit != unit
                    || declaration.range.start != 0
                    || declaration.source != file.file.source()
                {
                    return None;
                }
                let scopes = callback_scopes(file)?;
                let lines = line_starts(file.file.source());
                let member_receivers = if kind == EdgeKind::Reads {
                    checked_member_receivers(file)
                } else {
                    BTreeMap::new()
                };
                Some((
                    origin,
                    CallbackFile {
                        file,
                        module,
                        unit,
                        lines,
                        scopes,
                        member_receivers,
                    },
                ))
            })
            .collect()
    }

    fn publish_ecmascript_callback(
        &mut self,
        owner: &SymbolOwner,
        file: &CallbackFile<'_>,
        caller: Callback,
        objects: &BTreeSet<SymbolId>,
        declaration_limit: usize,
        reuse_existing: bool,
    ) -> Result<Option<SymbolId>, RiftError> {
        if self.symbols_truncated {
            return Ok(None);
        }
        let module = SymbolIdentity::parse(file.module.as_str())?;
        let name = format!("$callback#{}", caller.ordinal);
        let mut qualified = module.qualified_path().to_vec();
        qualified.push(name.clone());
        let language = file.file.file.syntax().language();
        let Ok(identity) = SymbolIdentity::new(owner.clone(), language.clone(), qualified) else {
            return Ok(None);
        };
        let id = SymbolId(identity.wire_identity());
        if objects.contains(&id) {
            return Ok((reuse_existing && self.retained_callback(file, caller, &id)).then_some(id));
        }
        let limit = bound(self.publication.symbols);
        if self.objects.len() >= limit || self.declarations.len() >= declaration_limit {
            self.symbols_truncated = true;
            self.warn(truncation(
                "declarations",
                u64::try_from(declaration_limit).unwrap_or(u64::MAX),
            ));
            return Ok(None);
        }
        let Some(source) = source_text(file.file.file.source(), caller.range) else {
            return Ok(None);
        };
        let path = wire_path(file.file.file.path());
        let bytes = caller.range.end - caller.range.start;
        if !self.retained_source_fits(bytes) {
            self.warn(PackageAnalysisWarning::SourceTruncated {
                path,
                dropped: bytes,
            });
            return Ok(None);
        }
        let facts = PortableSymbolFacts::new(
            language.clone(),
            &name,
            &name,
            ExactKind("function".to_owned()),
        )
        .kinds(vec![SymbolKind::Value, SymbolKind::Callable]);
        let origin = file.file.placement.origin();
        let mut object = AssembledSymbol::wire_facts(&facts, origin);
        object.id = Some(id.clone());
        object.container = Some(SymbolId(file.module.as_str().to_owned()));
        object.document_local = true;
        let retained = self.retained(source, &path, 1)?;
        let mut declaration = PackageSymbol {
            symbol: id.clone(),
            origin: object.origin.clone(),
            unit: file.unit.clone(),
            range: TextRange {
                start: caller.range.start,
                end: caller.range.end,
            },
            syntax_element_kind: Some(caller.kind.to_owned()),
            line: line_of(&file.lines, caller.range.start),
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
        Ok(Some(id))
    }

    fn retained_callback(&self, file: &CallbackFile<'_>, caller: Callback, id: &SymbolId) -> bool {
        let object = self.objects.iter().any(|object| {
            object.id.as_ref() == Some(id)
                && object.document_local
                && object
                    .container
                    .as_ref()
                    .is_some_and(|owner| owner.as_str() == file.module.as_str())
        });
        object
            && self.declarations.iter().any(|declaration| {
                declaration.symbol == *id
                    && declaration.unit == file.unit
                    && declaration.range.start == caller.range.start
                    && declaration.range.end == caller.range.end
                    && declaration.source_complete
                    && declaration.syntax_element_kind.as_deref() == Some(caller.kind)
                    && source_text(file.file.file.source(), caller.range)
                        == Some(declaration.source.as_str())
            })
    }
}

fn checked_callback_edges<'call, 'file>(
    files: &'call BTreeMap<&CoreSourceUnitId, CallbackFile<'file>>,
    semantics: &'call WorkspaceSemantics,
    objects: &BTreeSet<SymbolId>,
    kind: EdgeKind,
) -> BTreeMap<CallbackEdgeKey<'call>, CallbackEdge<'call, 'file>> {
    let objects = objects
        .iter()
        .map(SymbolId::as_str)
        .collect::<BTreeSet<_>>();
    semantics
        .edges()
        .edges()
        .filter_map(|site| {
            if site.kind() != kind || site.is_graph_edge() || !objects.contains(site.to().as_str())
            {
                return None;
            }
            let occurrence = site.occurrence();
            let file = files.get(occurrence.unit())?;
            let range = occurrence.range();
            if kind == EdgeKind::Calls {
                let callee_span = site.static_callee()?;
                if callee_span.start < range.start() || range.end() < callee_span.end {
                    return None;
                }
            }
            let caller = file
                .scopes
                .resolve(range.start(), range.end())
                .copied()
                .flatten()?;
            let key = (
                file.unit.as_str(),
                range.start(),
                range.end(),
                site.from().as_str(),
                site.to().as_str(),
            );
            Some((
                key,
                CallbackEdge {
                    site,
                    file,
                    caller,
                    existing: None,
                },
            ))
        })
        .collect()
}

/// A qualifier is retained only when the complete written access has a resolved Read.
fn retain_checked_member_reads(edges: &mut BTreeMap<CallbackEdgeKey<'_>, CallbackEdge<'_, '_>>) {
    let mut written = BTreeSet::new();
    for candidate in edges.values() {
        let range = candidate.site.occurrence().range();
        let member = ByteRange {
            start: range.start(),
            end: range.end(),
        };
        let Some(receiver) = candidate.file.member_receivers.get(&member) else {
            continue;
        };
        let unit = candidate.file.unit.as_str();
        let from = candidate.site.from().as_str();
        for site in [member, *receiver] {
            written.insert((unit, site.start, site.end, from));
        }
    }
    edges.retain(|&(unit, start, end, from, _), _| written.contains(&(unit, start, end, from)));
}

fn checked_member_receivers(file: &AnalyzedFile) -> BTreeMap<ByteRange, ByteRange> {
    let Some(facts) = file.file.syntax().static_facts() else {
        return BTreeMap::new();
    };
    facts
        .members()
        .iter()
        .filter(|member| member.member.is_some())
        .map(|member| (member.range, member.receiver))
        .collect()
}

fn stored_callback_edge_key(edge: &StoredEdge, kind: EdgeKind) -> Option<CallbackEdgeKey<'_>> {
    if edge.provider != EdgeProvider::Static || edge.kinds != [kind] {
        return None;
    }
    let site = edge.occurrence.as_ref()?;
    Some((
        site.unit.as_str(),
        site.range.start,
        site.range.end,
        edge.from.as_str(),
        edge.to.as_str(),
    ))
}

fn callback_scopes(file: &AnalyzedFile) -> Option<EnclosingDefinitions<Option<Callback>>> {
    let syntax = file.file.syntax();
    if !matches!(syntax.language().name.as_str(), "javascript" | "typescript")
        || syntax.has_errors()
        || syntax.left_out_declaration_count() != 0
    {
        return None;
    }
    let facts = syntax.static_facts()?;
    let calls = facts
        .calls()
        .iter()
        .map(|call| call.range)
        .collect::<BTreeSet<_>>();
    let arguments = facts
        .function_contexts()
        .iter()
        .filter_map(|(range, context)| {
            matches!(context, StaticFunctionContext::Argument { call, .. } if calls.contains(call))
                .then_some(*range)
        })
        .collect::<BTreeSet<_>>();
    if arguments.is_empty() {
        return None;
    }
    let block_functions = facts
        .function_scopes()
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let function_values = facts
        .function_values()
        .iter()
        .filter_map(|(range, value)| (range == value).then_some(*range))
        .collect::<BTreeSet<_>>();
    let scopes = function_values
        .into_iter()
        .enumerate()
        .map(|(ordinal, range)| {
            let callback = if arguments.contains(&range) {
                anonymous_kind(file.file.source(), range, block_functions.contains(&range)).map(
                    |kind| Callback {
                        range,
                        ordinal,
                        kind,
                    },
                )
            } else {
                None
            };
            (range.start, range.end, callback)
        });
    let named = syntax
        .symbols()
        .iter()
        .filter(|symbol| symbol.kinds.contains(&SymbolKind::Callable))
        .map(|symbol| (symbol.range.start, symbol.range.end, None));
    let deferred = facts
        .static_blocks()
        .iter()
        .map(|range| (range.start, range.end, None));
    Some(EnclosingDefinitions::new(
        named.chain(deferred).chain(scopes),
    ))
}

fn anonymous_kind(source: &str, range: ByteRange, block: bool) -> Option<&'static str> {
    if !block {
        return Some("arrow_function");
    }
    // The captured range is a checked function expression. A name or ambiguous written
    // prefix before its parameter list leaves any established caller unchanged.
    let written = source_text(source, range)?;
    let written = written
        .strip_prefix("async")
        .map_or(written, str::trim_start);
    let parameters = written.strip_prefix("function")?.trim_start();
    parameters.starts_with('(').then_some("function_expression")
}

fn source_text(source: &str, range: ByteRange) -> Option<&str> {
    source.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
}
