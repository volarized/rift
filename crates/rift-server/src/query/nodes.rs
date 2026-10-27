use std::collections::BTreeMap;

use rift_analysis::IndexedFile;
use rift_error::RiftError;
use rift_index::WorkspaceSymbolView;
use rift_protocol::read::{SourceUnitId, SymbolId};
use rift_search::query::{QueryFacts, QueryNode, QueryNodeSource};
use rift_syntax::ByteRange;

use crate::QueryFactBudget;

use super::query_bound_error;

type DeclarationNodes<'a> = BTreeMap<(ByteRange, &'static str), Option<&'a rift_core::SymbolId>>;

/// Appends one complete original named tree after its source catalog record is admitted.
pub(super) fn project(
    view: &WorkspaceSymbolView,
    file: &IndexedFile,
    unit: &SourceUnitId,
    facts: &mut QueryFacts,
    budget: &mut QueryFactBudget<'_>,
) -> Result<bool, RiftError> {
    let Some(original) = file.syntax().nodes() else {
        return Ok(false);
    };
    let allocation = original
        .len()
        .checked_mul(size_of::<QueryNode>())
        .and_then(|bytes| bytes.checked_add(size_of::<Vec<QueryNode>>()))
        .and_then(|bytes| bytes.checked_add(size_of::<QueryNodeSource>()))
        .and_then(|bytes| bytes.checked_add(size_of::<Vec<QueryNodeSource>>()))
        .and_then(|bytes| bytes.checked_add(unit.as_str().len().checked_mul(3)?))
        .ok_or_else(|| query_bound_error("search.query.bytes"))?;
    budget.charge_material(allocation)?;
    budget.charge(&(unit, file.source().len()))?;
    facts
        .nodes
        .try_reserve_exact(1)
        .map_err(|_| query_bound_error("search.query.bytes"))?;
    let declarations = declaration_nodes(view, file, budget)?;
    let mut nodes: Vec<QueryNode> = Vec::new();
    nodes
        .try_reserve_exact(original.len())
        .map_err(|_| query_bound_error("search.query.bytes"))?;
    for (ordinal, row) in original.iter().enumerate() {
        budget.check()?;
        let symbol = declarations.get(&(row.range, row.kind)).copied().flatten();
        let parent = row
            .parent
            .and_then(|parent| nodes.get(parent))
            .map(|parent| &parent.node.id);
        let material = crate::read::wire_node_material_bytes(
            file,
            unit,
            row.kind,
            symbol.map(rift_core::SymbolId::as_str),
            parent,
        )
        .ok_or_else(|| query_bound_error("search.query.bytes"))?;
        budget.charge_material(material)?;
        let symbol = symbol.map(|identity| SymbolId(identity.as_str().to_owned()));
        let mut node = crate::read::wire_node_with_symbol(file, row.range, row.kind, symbol);
        node.parent = parent.cloned();
        let node = QueryNode {
            ordinal,
            parent: row.parent,
            node,
        };
        budget.charge(&node)?;
        nodes.push(node);
    }
    let bytes =
        u64::try_from(file.source().len()).map_err(|_| query_bound_error("search.query.bytes"))?;
    facts
        .nodes
        .push(QueryNodeSource::new(unit.clone(), bytes, nodes)?);
    Ok(true)
}

fn declaration_nodes<'a>(
    view: &'a WorkspaceSymbolView,
    file: &IndexedFile,
    budget: &mut QueryFactBudget<'_>,
) -> Result<DeclarationNodes<'a>, RiftError> {
    let entries = file
        .syntax()
        .symbols()
        .len()
        .checked_mul(2)
        .ok_or_else(|| query_bound_error("search.query.bytes"))?;
    budget.charge_material(rift_protocol::btree_material_bytes::<
        (ByteRange, &'static str),
        Option<&rift_core::SymbolId>,
    >(entries))?;
    let mut nodes = DeclarationNodes::new();
    for (position, declaration) in file.syntax().symbols().iter().enumerate() {
        budget.check()?;
        let Some(kind) = declaration.node_kind else {
            continue;
        };
        let Some(identity) = view.symbol_identity_at(file.path(), position) else {
            continue;
        };
        for range in [declaration.range, declaration.item_range] {
            nodes
                .entry((range, kind))
                .and_modify(|held| {
                    if held.is_some_and(|held| held != identity) {
                        *held = None;
                    }
                })
                .or_insert(Some(identity));
        }
    }
    Ok(nodes)
}
