use std::collections::BTreeSet;

use rift_error::{RiftError, errors};
use rift_protocol::identity::{SourceDigest, SymbolIdentity, parse_source_unit_identity};
use rift_protocol::read::{SourceUnitId, SymbolId};
use rift_search::sparql::QueryIdentities;

use crate::server::QueryFactSelection;

pub(super) fn identities(
    backend: &str,
    selected: &QueryFactSelection,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<QueryIdentities, RiftError> {
    let context = rift_protocol::canonical::canonical_json(&(backend, &selected.selection))
        .map_err(encoding_failure)?;
    let blank_prefix = SourceDigest::parse(
        &data_encoding::HEXLOWER.encode(rift_core::FileDigest::of(context.as_bytes()).as_bytes()),
    )?;
    budget.charge(&(&selected.facts, &selected.services))?;
    let value =
        serde_json::to_value((&selected.facts, &selected.services)).map_err(encoding_failure)?;
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
            let public = source_unit(backend, &blank_prefix, &original)?;
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

fn source_unit(
    backend: &str,
    selection: &SourceDigest,
    original: &SourceUnitId,
) -> Result<SourceUnitId, RiftError> {
    let (owner, _) = parse_source_unit_identity(original.as_str())?;
    if backend == "local" || owner.is_some() {
        return Ok(original.clone());
    }
    let original = data_encoding::HEXLOWER
        .encode(rift_core::FileDigest::of(original.as_str().as_bytes()).as_bytes());
    let unit = rift_core::SourceUnitId::new(
        rift_core::SourceResolverId::new("rift.sources.project".to_owned())?,
        rift_core::SourcePath::new(format!("{}/{original}", selection.as_str()))?,
    )?;
    SourceUnitId::parse(&unit.to_string())
}

fn encoding_failure(source: serde_json::Error) -> RiftError {
    errors::mcp::forward_failed().source(source).error()
}
