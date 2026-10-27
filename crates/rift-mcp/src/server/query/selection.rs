//! Compatible upstream source selections retained by the existing query response owner.

use rift_error::RiftError;
use rift_protocol::configuration::GlobalConfiguration;
use rift_protocol::identity::{SymbolIdentity, SymbolOwner, parse_source_unit_identity};
use rift_protocol::read::{SearchHit, SearchHitTarget};
use rift_protocol::symbol_read::SymbolUnavailableReason;

use super::super::candidates::QueryGlobalSelection;

pub(super) fn hit_selection(
    hit: &SearchHit,
    configuration: &GlobalConfiguration,
) -> Result<Option<QueryGlobalSelection>, RiftError> {
    let mut owners = Vec::new();
    if let SearchHitTarget::Symbol { symbol } = &hit.hit
        && let Some(identity) = &symbol.id
    {
        let owner = SymbolIdentity::parse(identity.as_str())?.owner().clone();
        if matches!(
            owner,
            SymbolOwner::Package { .. } | SymbolOwner::Runtime { .. }
        ) {
            owners.push(owner);
        }
    }
    if let Some(unit) = &hit.unit {
        let (owner, _) = parse_source_unit_identity(unit.as_str())?;
        if let Some(owner) = owner
            && !owners.contains(&owner)
        {
            owners.push(owner);
        }
    }
    if owners.is_empty() {
        return Ok(None);
    }
    let view = hit.view.clone().ok_or_else(|| {
        super::acquisition_failure("a global retrieval hit omitted its selected source context")
    })?;
    Ok(Some(QueryGlobalSelection {
        owners,
        view,
        configuration: configuration.clone(),
    }))
}

pub(super) fn include(
    selected: &mut Vec<QueryGlobalSelection>,
    mut added: QueryGlobalSelection,
) -> Result<(), SymbolUnavailableReason> {
    for held in selected.iter() {
        if held.owners.iter().any(|owner| added.owners.contains(owner))
            && (held.view != added.view || held.configuration != added.configuration)
        {
            return Err(SymbolUnavailableReason::ViewContext);
        }
    }
    if let Some(held) = selected
        .iter_mut()
        .find(|held| held.view == added.view && held.configuration == added.configuration)
    {
        for owner in added.owners.drain(..) {
            if !held.owners.contains(&owner) {
                held.owners.push(owner);
            }
        }
    } else {
        selected.push(added);
    }
    Ok(())
}

pub(super) fn source_selection(
    unit: &rift_protocol::read::SourceUnitId,
    view: &rift_protocol::symbol_read::CapturedView,
    configuration: &GlobalConfiguration,
) -> Result<Option<QueryGlobalSelection>, RiftError> {
    let (owner, _) = parse_source_unit_identity(unit.as_str())?;
    Ok(owner.map(|owner| QueryGlobalSelection {
        owners: vec![owner],
        view: view.clone(),
        configuration: configuration.clone(),
    }))
}
