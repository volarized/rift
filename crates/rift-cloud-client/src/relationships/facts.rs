//! Exact projections supplied by the same bounded raw relationship pages.

use std::collections::{BTreeMap, BTreeSet};

use rift_error::RiftError;
use rift_protocol::identity::{SymbolIdentity, SymbolOwner, parse_source_unit_identity};
use rift_protocol::read::{PAGE_LIMIT_MAX, SourceKind, SourceUnitId, SymbolId};
use rift_protocol::relationship_read::{
    QuerySource, QuerySymbol, RelationshipInclude, RelationshipSearchPage,
    RelationshipSearchRequest,
};

use super::invalid;

pub(super) fn validate_page(
    request: &RelationshipSearchRequest,
    page: &RelationshipSearchPage,
    source_bytes_max: usize,
    symbols_max: usize,
    sources_max: usize,
) -> Result<(), RiftError> {
    if page.symbols.len() > symbols_max || page.sources.len() > sources_max {
        return Err(invalid(
            "relationship fact projections exceed their page bound",
        ));
    }
    let mut versions = BTreeMap::new();
    for package in &request.packages {
        versions.insert(package.owner()?, package.version.as_str());
    }
    for runtime in &request.runtimes {
        versions.insert(runtime.owner()?, runtime.version.as_str());
    }
    for symbol in &page.symbols {
        let id = symbol
            .symbol
            .id
            .clone()
            .ok_or_else(|| invalid("relationship symbol identity is absent"))?;
        let identity = SymbolIdentity::parse(id.as_str())?;
        let owner = identity.owner();
        let origin = &symbol.symbol.origin;
        let version = versions.get(owner).copied().or_else(|| {
            origin
                .package
                .as_ref()
                .map(|package| package.version.as_str())
                .or_else(|| {
                    origin
                        .runtime
                        .as_ref()
                        .map(|runtime| runtime.version.as_str())
                })
        });
        let synthetic = symbol.declarations.is_empty()
            && origin.source_kind == SourceKind::Synthetic
            && origin.location.is_none()
            && origin.package.is_none()
            && origin.runtime.is_none();
        if identity.language() != &symbol.symbol.language
            || !(synthetic || crate::symbol::valid_origin(origin, owner, version))
            || u64::try_from(symbol.declarations.len()).map_or(true, |len| len > PAGE_LIMIT_MAX)
            || !symbol.declarations.iter().all(|declaration| {
                declaration.is_valid_for(&symbol.symbol)
                    && crate::symbol::valid_origin(&declaration.origin, owner, version)
                    && declaration.unit.as_ref().is_some_and(|unit| {
                        parse_source_unit_identity(unit.as_str())
                            .is_ok_and(|(owner, _)| owner.as_ref() == Some(identity.owner()))
                    })
            })
        {
            return Err(invalid(
                "relationship symbol projections differ from their selected identity or source owner",
            ));
        }
    }
    for source in &page.sources {
        if source.text.as_ref().is_some_and(|text| {
            source.digest.as_str()
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
            !request.include.contains(&RelationshipInclude::Source) || text.len() > source_bytes_max
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
    if range.start > range.end || range.end > 9_007_199_254_740_991 {
        return Err(invalid(
            "relationship range exceeds the accepted byte range",
        ));
    }
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
        None if request.include.contains(&RelationshipInclude::Source) => {
            return Err(invalid(
                "complete relationship pages omitted requested source bytes",
            ));
        }
        None => {}
    }
    Ok(())
}

#[derive(Default)]
pub(super) struct Catalog {
    symbols: BTreeMap<SymbolId, QuerySymbol>,
    sources: BTreeMap<SourceUnitId, QuerySource>,
}

impl Catalog {
    pub(super) fn merge(&mut self, page: &mut RelationshipSearchPage) -> Result<(), RiftError> {
        use std::collections::btree_map::Entry;

        for symbol in std::mem::take(&mut page.symbols) {
            let id = symbol
                .symbol
                .id
                .clone()
                .ok_or_else(|| invalid("relationship symbol identity is absent"))?;
            match self.symbols.entry(id) {
                Entry::Occupied(mut entry) => {
                    let held = entry.get_mut();
                    if held.symbol != symbol.symbol {
                        return Err(invalid("relationship symbol changed between pages"));
                    }
                    for declaration in symbol.declarations {
                        if !held.declarations.contains(&declaration) {
                            held.declarations.push(declaration);
                        }
                    }
                }
                Entry::Vacant(entry) => {
                    entry.insert(symbol);
                }
            }
        }
        for source in std::mem::take(&mut page.sources) {
            match self.sources.entry(source.unit.clone()) {
                Entry::Occupied(mut entry) => {
                    let held = entry.get_mut();
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
                Entry::Vacant(entry) => {
                    entry.insert(source);
                }
            }
        }
        Ok(())
    }

    pub(super) fn finish(self, page: &mut RelationshipSearchPage) {
        page.symbols = self.symbols.into_values().collect();
        page.sources = self.sources.into_values().collect();
    }
}
