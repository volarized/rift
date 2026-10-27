use std::collections::BTreeSet;

use rift_error::{RiftError, errors};
use rift_protocol::identity::{SourceDigest, SymbolIdentity};
use rift_protocol::read::{SourceUnitId, SymbolId};
use rift_search::sparql::QueryIdentities;

use crate::server::QueryFactSelection;

pub(super) fn identities(
    backend: &str,
    selected: &QueryFactSelection,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<QueryIdentities, RiftError> {
    identities_for(backend, selected, selected, budget)
}

pub(super) fn identities_for(
    backend: &str,
    selected: &QueryFactSelection,
    source_owner: &QueryFactSelection,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<QueryIdentities, RiftError> {
    budget.charge(&(backend, &source_owner.selection))?;
    let context =
        serde_json::to_vec(&(backend, &source_owner.selection)).map_err(encoding_failure)?;
    let blank_prefix = SourceDigest::parse(
        &data_encoding::HEXLOWER.encode(rift_core::FileDigest::of(&context).as_bytes()),
    )?;
    budget.charge(&(&selected.facts, &selected.services, &selected.requirements))?;
    let value = serde_json::to_value((&selected.facts, &selected.services, &selected.requirements))
        .map_err(encoding_failure)?;
    let mut pending = vec![value];
    let mut symbols = BTreeSet::new();
    let mut sources = BTreeSet::new();
    while let Some(value) = pending.pop() {
        budget.check()?;
        match value {
            serde_json::Value::String(value) => {
                if SymbolIdentity::parse(&value).is_ok() {
                    symbols.insert(value);
                } else if SourceUnitId::parse(&value).is_ok() {
                    sources.insert(value);
                }
            }
            serde_json::Value::Array(values) => pending.extend(values),
            serde_json::Value::Object(values) => pending.extend(values.into_values()),
            _ => {}
        }
    }
    include_declaration_sources(backend, selected, &mut sources, budget)?;
    for service in &selected.services.retrievals {
        for hit in &service.result.results {
            budget.check()?;
            if hit.unit.is_none()
                && let Some(path) = &hit.path
            {
                include_project_source(&mut sources, path, budget)?;
            }
        }
    }
    let symbols: Vec<_> = symbols
        .into_iter()
        .map(|original| {
            let original = SymbolId::from_identity(&SymbolIdentity::parse(&original)?);
            let mut public = original.clone();
            crate::proxy::identity::qualify_id(&mut public, backend)?;
            Ok((original, public))
        })
        .collect::<Result<_, RiftError>>()?;
    let sources: Vec<_> = sources
        .into_iter()
        .map(|original| {
            let original = SourceUnitId::parse(&original)?;
            let public = if source_owner.graph.is_none() {
                rift_search::query::qualify_source_unit(&original, Some(&blank_prefix))?
            } else {
                source_unit(backend, &blank_prefix, &original)?
            };
            Ok((original, public))
        })
        .collect::<Result<_, RiftError>>()?;
    let mut public = std::collections::BTreeMap::new();
    for (original, advertised) in symbols
        .iter()
        .map(|(original, advertised)| (original.as_str(), advertised.as_str()))
        .chain(
            sources
                .iter()
                .map(|(original, advertised)| (original.as_str(), advertised.as_str())),
        )
    {
        if public
            .insert(advertised, original)
            .is_some_and(|previous| previous != original)
        {
            return Err(super::retained::unavailable());
        }
    }
    let result = QueryIdentities {
        symbols,
        sources,
        blank_prefix,
    };
    budget.charge(&result)?;
    Ok(result)
}

fn include_declaration_sources(
    backend: &str,
    selected: &QueryFactSelection,
    sources: &mut BTreeSet<String>,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<(), RiftError> {
    if backend != "global"
        && matches!(
            selected.owner,
            rift_protocol::identity::SymbolOwner::Local
                | rift_protocol::identity::SymbolOwner::NamedLocal { .. }
        )
    {
        for symbol in &selected.facts.symbols {
            if let Some(id) = &symbol.symbol.id
                && !matches!(
                    SymbolIdentity::parse(id.as_str())?.owner(),
                    rift_protocol::identity::SymbolOwner::Local
                        | rift_protocol::identity::SymbolOwner::NamedLocal { .. }
                )
            {
                continue;
            }
            for declaration in &symbol.declarations {
                budget.check()?;
                if declaration.unit.is_none()
                    && let Some(path) = &declaration.path
                {
                    include_project_source(sources, path, budget)?;
                }
            }
        }
    }
    Ok(())
}

fn include_project_source(
    sources: &mut BTreeSet<String>,
    path: &rift_protocol::read::ProjectPath,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<(), RiftError> {
    budget.charge(path)?;
    let unit = rift_core::SourceUnitId::new(
        rift_core::SourceResolverId::new("project")?,
        rift_core::SourcePath::new(path.0.as_str())?,
    )?;
    let original = unit.to_string();
    budget.charge(&original)?;
    sources.insert(original);
    Ok(())
}

fn source_unit(
    backend: &str,
    selection: &SourceDigest,
    original: &SourceUnitId,
) -> Result<SourceUnitId, RiftError> {
    rift_search::query::qualify_source_unit(original, (backend != "local").then_some(selection))
}

fn encoding_failure(source: serde_json::Error) -> RiftError {
    errors::mcp::forward_failed().source(source).error()
}
