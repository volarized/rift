//! Decodes typed projection sets with the uniqueness required by their schemas.

use std::collections::BTreeSet;

use serde::{Deserialize, Deserializer, de};

pub(crate) fn deserialize_set<'de, D, T>(deserializer: D) -> Result<BTreeSet<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Ord,
{
    let entries = Vec::<T>::deserialize(deserializer)?;
    let count = entries.len();
    let selected: BTreeSet<_> = entries.into_iter().collect();
    if selected.len() != count {
        return Err(de::Error::custom("projection entries must be distinct"));
    }
    Ok(selected)
}
