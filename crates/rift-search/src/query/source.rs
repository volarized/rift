//! Complete source witnesses shared by acquisition and dataset validation.

use std::collections::BTreeMap;

use rift_error::{RiftError, errors};
use rift_protocol::identity::SourceDigest;

use super::QuerySource;

/// Qualifies a project source unit under its original captured selection.
///
/// A missing selection keeps the original project unit. Canonical package and runtime
/// units keep their existing identity in every selection.
///
/// # Errors
/// Returns the existing source identity, resolver, path or digest encoding refusal.
pub fn qualify_source_unit(
    original: &rift_protocol::read::SourceUnitId,
    selection: Option<&SourceDigest>,
) -> Result<rift_protocol::read::SourceUnitId, RiftError> {
    let (owner, _) = rift_protocol::identity::parse_source_unit_identity(original.as_str())?;
    let Some(selection) = selection.filter(|_| owner.is_none()) else {
        return Ok(original.clone());
    };
    let original = data_encoding::HEXLOWER
        .encode(rift_core::FileDigest::of(original.as_str().as_bytes()).as_bytes());
    let unit = rift_core::SourceUnitId::new(
        rift_core::SourceResolverId::new("rift.sources.project".to_owned())?,
        rift_core::SourcePath::new(format!("{}/{original}", selection.as_str()))?,
    )?;
    rift_protocol::read::SourceUnitId::parse(&unit.to_string())
}

/// Checks each source's full bytes and compares repeated canonical units.
///
/// Each source is hashed once. The map retains borrowed text and one digest per unit;
/// later metadata-only selections do not hash the original text again.
///
/// # Errors
/// Refuses bytes differing from their full digest, or conflicting captured selections.
pub fn validate_source_selections<'source>(
    sources: impl IntoIterator<Item = &'source QuerySource>,
) -> Result<(), RiftError> {
    validate_source_selections_by_unit(
        sources
            .into_iter()
            .map(|source| Ok((source.unit.clone(), source))),
    )
}

/// Checks original source captures under their checked advertised unit identities.
///
/// The caller supplies identities before matching owner graphs. Original captured
/// records and the full byte agreement policy remain unchanged.
///
/// # Errors
/// Refuses an identity failure, invalid full digest or conflicting source selection.
pub fn validate_source_selections_by_unit<'source>(
    sources: impl IntoIterator<
        Item = Result<(rift_protocol::read::SourceUnitId, &'source QuerySource), RiftError>,
    >,
) -> Result<(), RiftError> {
    let mut captured = BTreeMap::new();
    for selected in sources {
        let (unit, source) = selected?;
        let witness = SourceWitness {
            digest: source_digest(source)?,
            text: source.text.as_deref(),
        };
        match captured.entry(unit) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(witness);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let original: &mut SourceWitness<'source> = entry.get_mut();
                require_source_agreement(
                    original.digest.as_ref(),
                    original.text,
                    witness.digest.as_ref(),
                    witness.text,
                )?;
                if original.digest.is_none() {
                    original.digest = witness.digest;
                }
                original.text = original.text.or(witness.text);
            }
        }
    }
    Ok(())
}

struct SourceWitness<'source> {
    digest: Option<SourceDigest>,
    text: Option<&'source str>,
}

/// Returns the full source digest after checking every supplied byte.
///
/// # Errors
/// Refuses supplied text differing from its declared full digest.
pub fn source_digest(source: &QuerySource) -> Result<Option<SourceDigest>, RiftError> {
    let Some(text) = &source.text else {
        return Ok(source.digest.clone());
    };
    let digest = SourceDigest::parse(
        &data_encoding::HEXLOWER.encode(rift_core::FileDigest::of(text.as_bytes()).as_bytes()),
    )?;
    if source
        .digest
        .as_ref()
        .is_some_and(|selected| selected != &digest)
    {
        return unavailable("selected source bytes differ from their full digest").fail();
    }
    Ok(Some(digest))
}

/// Compares two already checked witnesses for the same canonical source unit.
///
/// Missing bytes or digests may be supplied by the other witness. Existing values
/// must agree byte-for-byte; source normalization never changes this comparison.
///
/// # Errors
/// Refuses conflicting full digests or captured text.
pub fn require_source_agreement(
    original_digest: Option<&SourceDigest>,
    original_text: Option<&str>,
    added_digest: Option<&SourceDigest>,
    added_text: Option<&str>,
) -> Result<(), RiftError> {
    if original_digest
        .zip(added_digest)
        .is_some_and(|(original, added)| original != added)
    {
        return unavailable("source digest changed during query acquisition").fail();
    }
    if original_text
        .zip(added_text)
        .is_some_and(|(original, added)| original != added)
    {
        return unavailable("source changed during query acquisition").fail();
    }
    Ok(())
}

fn unavailable(detail: &'static str) -> RiftError {
    errors::server::read_unavailable()
        .operation("query source selection")
        .detail(detail)
        .error()
}
