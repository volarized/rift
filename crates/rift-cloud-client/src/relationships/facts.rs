//! Exact projections supplied by the same bounded raw relationship pages.

use std::collections::{BTreeMap, BTreeSet};

use rift_error::RiftError;
use rift_protocol::identity::{SymbolIdentity, SymbolOwner, parse_source_unit_identity};
use rift_protocol::query::{QuerySource, QuerySymbol};
use rift_protocol::read::{GetSymbolInclude, PAGE_LIMIT_MAX, SourceUnitId, SymbolId};
use rift_protocol::relationship_read::{RelationshipSearchPage, RelationshipSearchRequest};
use rift_protocol::symbol_read::{GetSymbolParams, GetSymbolResult};

use super::invalid;

pub(super) fn validate_page(
    request: &RelationshipSearchRequest,
    page: &RelationshipSearchPage,
    source_bytes_max: usize,
) -> Result<(), RiftError> {
    if page.symbols.len() > 10_000 || page.sources.len() > 10_000 {
        return Err(invalid(
            "relationship fact projections exceed their page bound",
        ));
    }
    for symbol in &page.symbols {
        let id = symbol
            .symbol
            .id
            .clone()
            .ok_or_else(|| invalid("relationship symbol identity is absent"))?;
        let request = GetSymbolParams {
            id,
            include: request.include.clone(),
            rev: None,
            view: Some(page.view.id.clone()),
            declaration_limit: PAGE_LIMIT_MAX,
            declaration_cursor: None,
        };
        let result = GetSymbolResult::Found {
            symbol: Box::new(symbol.symbol.clone()),
            view: page.view.clone(),
            declarations: symbol.declarations.clone(),
            declaration_cursor: None,
            history: None,
            documentation: None,
            warnings: Vec::new(),
        };
        if !crate::symbol::valid_result(&request, &result, source_bytes_max) {
            return Err(invalid(
                "relationship symbol projections differ from their selected identity or source owner",
            ));
        }
    }
    for source in &page.sources {
        let digest = source
            .digest
            .as_ref()
            .ok_or_else(|| invalid("relationship source catalog omitted its full digest"))?;
        if source.text.as_ref().is_some_and(|text| {
            digest.as_str()
                != data_encoding::HEXLOWER
                    .encode(rift_core::FileDigest::of(text.as_bytes()).as_bytes())
        }) {
            return Err(invalid(
                "relationship source bytes differ from their selected full digest",
            ));
        }
        let (owner, _) = parse_source_unit_identity(source.unit.as_str())?;
        if !owner.is_some_and(|owner| {
            matches!(
                owner,
                SymbolOwner::Package { .. } | SymbolOwner::Runtime { .. }
            )
        }) || source.text.as_ref().is_some_and(|text| {
            !request.include.contains(&GetSymbolInclude::Source) || text.len() > source_bytes_max
        }) {
            return Err(invalid(
                "relationship source projections differ from their released owner or accepted byte bound",
            ));
        }
    }
    Ok(())
}

pub(super) fn validate_complete(
    request: &RelationshipSearchRequest,
    page: &RelationshipSearchPage,
) -> Result<(), RiftError> {
    let symbols = page
        .symbols
        .iter()
        .filter_map(|symbol| symbol.symbol.id.as_ref())
        .collect::<BTreeSet<_>>();
    let sources = page
        .sources
        .iter()
        .map(|source| (&source.unit, source))
        .collect::<BTreeMap<_, _>>();
    for seed in &request.relationships.seeds {
        require_symbol(seed, &symbols)?;
    }
    for group in &page.groups {
        for record in &group.relationships {
            require_symbol(&record.relationship.from, &symbols)?;
            require_symbol(&record.relationship.to, &symbols)?;
            if let Some(source) = &record.relationship.occurrence {
                require_source(&source.unit, &source.range, &sources, request)?;
            }
        }
        for record in &group.occurrences {
            let occurrence = &record.occurrence;
            require_symbol(&occurrence.target, &symbols)?;
            if let Some(enclosing) = &occurrence.enclosing {
                require_symbol(enclosing, &symbols)?;
            }
            require_source(
                &occurrence.source.unit,
                &occurrence.source.range,
                &sources,
                request,
            )?;
        }
    }
    for symbol in &page.symbols {
        for declaration in &symbol.declarations {
            if let Some(unit) = &declaration.unit {
                require_source(unit, &declaration.range, &sources, request)?;
            }
        }
    }
    Ok(())
}

fn require_symbol(id: &SymbolId, symbols: &BTreeSet<&SymbolId>) -> Result<(), RiftError> {
    if matches!(
        SymbolIdentity::parse(id.as_str())?.owner(),
        SymbolOwner::Package { .. } | SymbolOwner::Runtime { .. }
    ) && !symbols.contains(id)
    {
        return Err(invalid(
            "complete relationship pages omitted a required reached symbol",
        ));
    }
    Ok(())
}

fn require_source(
    unit: &SourceUnitId,
    range: &rift_protocol::read::TextRange,
    sources: &BTreeMap<&SourceUnitId, &QuerySource>,
    request: &RelationshipSearchRequest,
) -> Result<(), RiftError> {
    let source = sources
        .get(unit)
        .ok_or_else(|| invalid("complete relationship pages omitted a required physical source"))?;
    match source.text.as_deref() {
        Some(text) => {
            let start = usize::try_from(range.start)
                .map_err(|_| invalid("source range exceeds the platform bound"))?;
            let end = usize::try_from(range.end)
                .map_err(|_| invalid("source range exceeds the platform bound"))?;
            if text.get(start..end).is_none() {
                return Err(invalid(
                    "relationship range does not select complete captured UTF-8 bytes",
                ));
            }
        }
        None if request.include.contains(&GetSymbolInclude::Source) => {
            return Err(invalid(
                "complete relationship pages omitted requested source bytes",
            ));
        }
        None => {}
    }
    Ok(())
}

pub(super) fn merge(
    selected: &mut RelationshipSearchPage,
    symbols: Vec<QuerySymbol>,
    sources: Vec<QuerySource>,
) -> Result<(), RiftError> {
    for symbol in symbols {
        match selected
            .symbols
            .iter_mut()
            .find(|held| held.symbol.id == symbol.symbol.id)
        {
            Some(held) => {
                if held.symbol != symbol.symbol {
                    return Err(invalid("relationship symbol changed between pages"));
                }
                for declaration in symbol.declarations {
                    if !held.declarations.contains(&declaration) {
                        held.declarations.push(declaration);
                    }
                }
            }
            None => selected.symbols.push(symbol),
        }
    }
    for source in sources {
        match selected
            .sources
            .iter_mut()
            .find(|held| held.unit == source.unit)
        {
            Some(held) => {
                if held.digest != source.digest {
                    return Err(invalid("relationship source digest changed between pages"));
                }
                match (&held.text, source.text) {
                    (Some(previous), Some(next)) if previous != &next => {
                        return Err(invalid("relationship source changed between pages"));
                    }
                    (None, Some(text)) => held.text = Some(text),
                    _ => {}
                }
            }
            None => selected.sources.push(source),
        }
    }
    Ok(())
}
