use rift_error::{RiftError, errors};
use rift_protocol::read::{Node, SourceUnitId};
use serde::{Deserialize, Deserializer, Serialize};

/// One named syntax row at its original position in a complete captured tree.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryNode {
    /// Original named-node ordinal, before query filtering or paging.
    pub ordinal: usize,
    /// Original parent ordinal in this same captured source tree.
    #[serde(deserialize_with = "required_parent")]
    pub parent: Option<usize>,
    /// Existing node payload, including its recorded source-relative address.
    pub node: Node,
}

/// Complete original named-node rows for one captured physical source.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct QueryNodeSource {
    /// Canonical source catalog identity under the selected owner.
    pub unit: SourceUnitId,
    /// Byte length of the original captured UTF-8 source.
    pub bytes: u64,
    /// Original named-tree order and lexical parent associations.
    pub nodes: Vec<QueryNode>,
}

impl QueryNodeSource {
    /// Retains complete original named rows after validating their source and parent graph.
    ///
    /// # Errors
    /// Refuses changed ordinals, missing parents or invalid source ranges.
    pub fn new(unit: SourceUnitId, bytes: u64, nodes: Vec<QueryNode>) -> Result<Self, RiftError> {
        let source = Self { unit, bytes, nodes };
        source.validate()?;
        Ok(source)
    }

    /// Checks original named-tree order without treating recorded addresses as global identity.
    ///
    /// # Errors
    /// Refuses invalid source ownership, rows or lexical parent associations.
    pub fn validate(&self) -> Result<(), RiftError> {
        let unit = rift_core::SourceUnitId::parse(self.unit.as_str())?;
        let file = format!(
            "rift://file/{}",
            rift_core::encode_path(unit.key().as_str())
        );
        let within_source = |range: &rift_protocol::read::TextRange| {
            range.start <= range.end && range.end <= self.bytes
        };
        let first = self.nodes.first().map(|row| &row.node);
        for (ordinal, row) in self.nodes.iter().enumerate() {
            if row.ordinal != ordinal
                || row.node.id.0.is_empty()
                || row.node.kind.0.is_empty()
                || row.node.unit.0 != file
                || !within_source(&row.node.range)
                || row
                    .node
                    .regions
                    .iter()
                    .any(|region| !within_source(&region.range))
                || first.is_some_and(|first| {
                    row.node.unit != first.unit || row.node.language != first.language
                })
            {
                return Err(invalid());
            }
            match row.parent {
                Some(parent) => {
                    let parent = self
                        .nodes
                        .get(parent)
                        .filter(|_| parent < ordinal)
                        .ok_or_else(invalid)?;
                    if row.node.parent.as_ref() != Some(&parent.node.id)
                        || parent.node.range.start > row.node.range.start
                        || row.node.range.end > parent.node.range.end
                    {
                        return Err(invalid());
                    }
                }
                None if row.node.parent.is_some() => return Err(invalid()),
                None => {}
            }
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for QueryNodeSource {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Captured {
            unit: SourceUnitId,
            bytes: u64,
            nodes: Vec<QueryNode>,
        }
        let captured = Captured::deserialize(deserializer)?;
        Self::new(captured.unit, captured.bytes, captured.nodes).map_err(serde::de::Error::custom)
    }
}

fn required_parent<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<usize>, D::Error> {
    Option::deserialize(deserializer)
}

fn invalid() -> RiftError {
    errors::server::read_unavailable()
        .operation("query acquisition")
        .detail("captured syntax source, ordinal or parent association is invalid")
        .error()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn source() -> QueryNodeSource {
        let shared = "rift://node/rust/lib.rs@0-11#1c281acb";
        serde_json::from_value(serde_json::json!({
            "unit":"rift://source/project/lib.rs","bytes":11,"nodes":[
                {"ordinal":0,"parent":null,"node":{
                    "id":shared,"unit":"rift://file/lib.rs","language":"rust",
                    "kind":"source_file","range":{"start":0,"end":11}
                }},
                {"ordinal":1,"parent":0,"node":{
                    "id":shared,"unit":"rift://file/lib.rs","language":"rust",
                    "kind":"function_item","range":{"start":0,"end":11},"parent":shared,
                    "regions":[{"role":"name","range":{"start":3,"end":6}},
                        {"role":"body","range":{"start":9,"end":11}}]
                }},
                {"ordinal":2,"parent":1,"node":{
                    "id":"rift://node/rust/lib.rs@3-6#7692c3ad","unit":"rift://file/lib.rs",
                    "language":"rust","kind":"identifier","range":{"start":3,"end":6},
                    "parent":shared
                }}
            ]
        }))
        .expect("original named tree with equal recorded addresses")
    }

    #[test]
    fn original_ordinals_keep_same_address_parent_and_child_distinct() {
        let source = source();
        assert_eq!(source.nodes[0].node.id, source.nodes[1].node.id);
        assert_ne!(source.nodes[0].ordinal, source.nodes[1].ordinal);
        assert_eq!(source.nodes[1].parent, Some(0));
        assert_eq!(source.nodes[2].parent, Some(1));
        let encoded = serde_json::to_vec(&source).expect("captured tree");
        assert_eq!(
            serde_json::from_slice::<QueryNodeSource>(&encoded).expect("same tree"),
            source
        );
    }

    #[test]
    fn source_refuses_changed_original_order_parents_ranges_and_required_fields() {
        let original = serde_json::to_value(source()).expect("original rows");
        for change in 0..8 {
            let mut changed = original.clone();
            match change {
                0 => changed["nodes"].as_array_mut().expect("nodes").swap(0, 1),
                1 => changed["nodes"][1]["parent"] = serde_json::json!(1),
                2 => changed["nodes"][1]["parent"] = serde_json::json!(3),
                3 => changed["nodes"][1]["node"]["range"]["end"] = serde_json::json!(12),
                4 => changed["nodes"][1]["node"]["kind"] = serde_json::json!(""),
                5 => {
                    changed["nodes"][1]
                        .as_object_mut()
                        .expect("row")
                        .remove("parent");
                }
                6 => {
                    changed.as_object_mut().expect("source").remove("bytes");
                }
                _ => changed["nodes"][1]["unknown"] = serde_json::json!(true),
            }
            assert!(
                serde_json::from_value::<QueryNodeSource>(changed).is_err(),
                "change {change}"
            );
        }
    }
}
