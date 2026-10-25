use std::collections::BTreeMap;
use std::ops::Deref;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use super::{ExtensionKey, ExtensionValue};

/// Facts a provider carries that the model has no field for, under a reverse-domain key.
/// Keys and values use RFC 8785 canonical JSON. Consumers skip entries they do not
/// implement.
#[derive(Debug, Default, JsonSchema, Serialize)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct Extensions {
    values: BTreeMap<ExtensionKey, ExtensionValue>,
    #[serde(skip)]
    heap_bytes: usize,
}

impl Extensions {
    /// Whether this holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub(crate) const fn retained_heap_size(&self) -> usize {
        self.heap_bytes
    }
}

impl Deref for Extensions {
    type Target = BTreeMap<ExtensionKey, ExtensionValue>;

    fn deref(&self) -> &Self::Target {
        &self.values
    }
}

impl From<BTreeMap<ExtensionKey, ExtensionValue>> for Extensions {
    fn from(mut values: BTreeMap<ExtensionKey, ExtensionValue>) -> Self {
        if values.is_empty() {
            return Self::default();
        }
        let mut heap_bytes =
            crate::btree_material_bytes::<ExtensionKey, ExtensionValue>(values.len());
        for (key, extension) in &mut values {
            heap_bytes = heap_bytes.saturating_add(key.0.capacity());
            heap_bytes = heap_bytes.saturating_add(compact_value(&mut extension.data));
        }
        Self { values, heap_bytes }
    }
}

impl Clone for Extensions {
    fn clone(&self) -> Self {
        Self::from(self.values.clone())
    }
}

impl PartialEq for Extensions {
    fn eq(&self, other: &Self) -> bool {
        self.values == other.values
    }
}

impl<'de> Deserialize<'de> for Extensions {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        BTreeMap::deserialize(deserializer).map(Self::from)
    }
}

// serde_json 1.0.154 selects BTreeMap or, with preserve_order, indexmap 2.14.0.
// Rebuilding each owned object through FromIterator fixes its initial capacity.
// Admission covers both backings without assuming a public Map capacity.
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
    let indexed = buckets
        .and_then(|buckets| {
            let entries = buckets.checked_mul(entry?)?;
            let indices = buckets.checked_mul(std::mem::size_of::<usize>())?;
            entries
                .checked_add(indices)?
                .checked_add(15)?
                .checked_add(buckets)?
                .checked_add(16)
        })
        .unwrap_or(usize::MAX);
    indexed.max(crate::btree_material_bytes::<String, Value>(len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use get_size2::GetSize as _;

    #[test]
    fn nested_objects_with_spare_capacity_keep_value_and_finite_clone_admission() {
        let mut nested = serde_json::Map::with_capacity(128);
        nested.insert(
            "path".to_owned(),
            serde_json::json!(["src/lib.rs", "beacon"]),
        );
        let mut outer = serde_json::Map::with_capacity(128);
        outer.insert("declaration".to_owned(), Value::Object(nested));
        let data = Value::Object(outer);
        let before = data.clone();
        let extensions = Extensions::from(BTreeMap::from([(
            ExtensionKey("org.lsp.symbol".to_owned()),
            ExtensionValue { version: 1, data },
        )]));
        assert_eq!(
            extensions
                .values
                .values()
                .next()
                .expect("LSP extension")
                .data,
            before
        );
        let bytes = extensions.get_heap_size();
        assert!(bytes > 0 && bytes < 16_384);
        let cloned = extensions.clone();
        assert_eq!(cloned, extensions);
        assert!(cloned.get_heap_size() <= bytes);
        let encoded = serde_json::to_vec(&extensions).expect("extension JSON");
        let wire: Value = serde_json::from_slice(&encoded).expect("extension JSON");
        assert_eq!(wire["org.lsp.symbol"]["data"], before);
        assert!(wire.get("values").is_none());
        assert!(wire.get("heap_bytes").is_none());
        let schema =
            serde_json::to_value(schemars::schema_for!(Extensions)).expect("extension schema");
        assert!(
            jsonschema::validator_for(&schema)
                .expect("extension schema")
                .is_valid(&wire)
        );
        let decoded: Extensions = serde_json::from_slice(&encoded).expect("extension JSON");
        assert_eq!(decoded, extensions);
        assert!(decoded.get_heap_size() > 0 && decoded.get_heap_size() < 16_384);
        assert_eq!(
            serde_json::to_vec(&decoded).expect("extension JSON"),
            encoded
        );
    }

    #[test]
    fn compact_objects_preserve_order_and_values() {
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
