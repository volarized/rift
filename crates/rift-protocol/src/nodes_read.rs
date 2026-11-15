//! Declaration summaries and syntax pages returned by the MCP nodes tool.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::read::{
    ExactKind, NodeRegion, NodesResult, ReadWarning, SourceExcerpt, SymbolId, TextRange,
};
use crate::revision::Revision;

/// The selected declaration summary, or the syntax tree page requested with `syntax`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(untagged)]
#[schemars(transform = crate::schema::require_result_object)]
#[schemars(extend("examples" = [{
    "symbol": "rift://symbol/local/rust/rift_server/read/nodes/cursor_offset",
    "declarations": [{
        "declaration": 0,
        "kind": "function_item",
        "range": { "start": 3698, "end": 3862 },
        "source": {
            "span": {
                "unit": "rift://source/project/crates/rift-server/src/read/nodes.rs",
                "range": { "start": 3698, "end": 3862 }
            },
            "text": "pub(crate) fn cursor_offset(cursor: &str, key: &str, revision: &str) -> Result<usize, RiftError> {\n    super::cursor::offset(cursor, \"node_cursor\", key, revision)\n}"
        },
        "parts": [{
            "index": 4,
            "kind": "parameters",
            "range": { "start": 3725, "end": 3766 }
        }]
    }],
    "revision": "rift://revision/tree/41619978851e527321ce95f42acb8699a96f00882484f1e9c1d404a8fde27470"
}]))]
pub enum NodesToolResult {
    /// Source and structure for the selected declarations or file position.
    Summary(NodesSummary),
    /// Syntax nodes with their exact source and addresses.
    Syntax(NodesResult),
}

/// Source and structure selected by a nodes request.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodesSummary {
    /// The symbol whose declarations were selected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<SymbolId>,
    /// Declarations in symbol order.
    pub declarations: Vec<NodeDeclaration>,
    /// Source around the selected file position when no declaration holds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<SourceExcerpt>,
    /// Continues the declaration summaries with the same selector.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_cursor: Option<String>,
    /// True when the summary clipped a declaration's source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    /// The revision this answer was read at.
    pub revision: Revision,
    /// Conditions affecting this answer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<ReadWarning>,
}

/// One declaration, with its source and the parts the syntax provider identified.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodeDeclaration {
    /// Declaration ordinal in the symbol, from zero.
    pub declaration: u64,
    /// Kind of its root syntax node.
    pub kind: ExactKind,
    /// Full byte range of the declaration.
    pub range: TextRange,
    /// Source from the declaration. Its span identifies the returned bytes.
    pub source: SourceExcerpt,
    /// Named regions of the root node, when supplied by the syntax provider.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub regions: Vec<NodeRegion>,
    /// Selected children of the root node.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<NodePart>,
}

/// A syntax node directly inside a declaration, addressed at the answer's revision.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodePart {
    /// Index in the declaration's full syntax tree.
    pub index: u64,
    /// Kind of the node in the provider's vocabulary.
    pub kind: ExactKind,
    /// Byte range in the declaration's source unit.
    pub range: TextRange,
}
