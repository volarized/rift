use crate::{ExtensionKey, ExtensionValue, Extensions};
use serde_json::Value;

// serde_json 1.0.151 uses indexmap 2.14.0. Rebuilding each owned object through
// FromIterator fixes its initial capacity. No public Map capacity is assumed.
pub(super) fn compact(extensions: &mut Extensions) -> usize {
    let mut bytes =
        rift_protocol::btree_material_bytes::<ExtensionKey, ExtensionValue>(extensions.0.len());
    for (key, extension) in &mut extensions.0 {
        bytes = bytes.saturating_add(key.0.capacity());
        bytes = bytes.saturating_add(compact_value(&mut extension.data));
    }
    bytes
}

fn compact_value(root: &mut Value) -> usize {
    let mut pending = vec![root];
    let mut bytes = 0usize;
    while let Some(value) = pending.pop() {
        match value {
            Value::Object(map) => {
                *map = std::mem::take(map).into_iter().collect();
                bytes = bytes.saturating_add(object_material_bytes(map.len()));
                for (key, value) in map.iter_mut() {
                    bytes = bytes.saturating_add(key.capacity());
                    pending.push(value);
                }
            }
            Value::Array(array) => {
                bytes = bytes.saturating_add(
                    array
                        .capacity()
                        .saturating_mul(std::mem::size_of::<Value>()),
                );
                pending.extend(array);
            }
            Value::String(text) => bytes = bytes.saturating_add(text.capacity()),
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }
    bytes
}

fn object_material_bytes(len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    // hashbrown 0.17.1 reserves 1/8 buckets for large tables. Small usize tables
    // use 4, 8 or 16 buckets. Group width is at most 16 on supported targets.
    let buckets = if len < 4 {
        Some(4)
    } else if len < 8 {
        Some(8)
    } else if len < 15 {
        Some(16)
    } else {
        len.checked_mul(8)
            .and_then(|adjusted| (adjusted / 7).checked_next_power_of_two())
    };
    let alignment = std::mem::align_of::<usize>()
        .max(std::mem::align_of::<String>())
        .max(std::mem::align_of::<Value>());
    // IndexMap's Bucket owns hash, key and value. Allow padding around each
    // field independently. Clone may grow the entry Vec to rounded table
    // capacity, so charge all buckets rather than the initial entry count.
    let entry = [
        std::mem::size_of::<usize>(),
        std::mem::size_of::<String>(),
        std::mem::size_of::<Value>(),
    ]
    .into_iter()
    .try_fold(0usize, |sum, size| {
        size.checked_add(alignment - 1)
            .map(|padded| padded / alignment * alignment)
            .and_then(|padded| sum.checked_add(padded))
    });
    buckets
        .and_then(|buckets| {
            let entries = buckets.checked_mul(entry?)?;
            let indices = buckets.checked_mul(std::mem::size_of::<usize>())?;
            entries
                .checked_add(indices)?
                .checked_add(15)?
                .checked_add(buckets)?
                .checked_add(16)
        })
        .unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_objects_keep_order_values_and_finite_clone_charge() {
        let mut value = serde_json::json!({"first": {"nested": ["held", 1]}, "last": true});
        let before = value.clone();
        let bytes = compact_value(&mut value);
        assert_eq!(value, before);
        assert!(bytes > 0 && bytes < usize::MAX);
        assert!(compact_value(&mut value.clone()) <= bytes);
    }

    #[test]
    fn empty_object_and_overflow_have_exact_admission_outcomes() {
        assert_eq!(object_material_bytes(0), 0);
        assert_eq!(object_material_bytes(usize::MAX), usize::MAX);
        assert!(object_material_bytes(15) > object_material_bytes(14));
    }
}
