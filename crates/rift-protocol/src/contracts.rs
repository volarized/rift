//! Contract metadata for the MCP tools this release implements.

/// Request and result models for one MCP tool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolContract {
    /// MCP tool name.
    pub name: &'static str,
    /// The tool description an MCP client shows its agent.
    pub description: &'static str,
    /// Protocol request model name.
    pub request_model: &'static str,
    /// Protocol result model name.
    pub result_model: &'static str,
}

/// Read-only MCP tools implemented by this release.
pub const TOOL_CONTRACTS: &[ToolContract] = &[
    ToolContract {
        name: "get_symbol",
        description: "Gets project, dependency, or standard-library declarations by name. \
            Each hit includes declaration source when the provider can read it; \
            `include_body: false` omits it, and `include_history` adds project \
            version-control history. Use `search` for lexical, filtered, or relationship \
            discovery.",
        request_model: "GetSymbolParams",
        result_model: "GetSymbolResult",
    },
    ToolContract {
        name: "search",
        description: "Searches symbols, nodes, and files by lexical `query`, provider \
            `filter`, or bounded relationship `traversal`. `scope` selects project, \
            dependency, or all sources. Use `traversal` for callers, callees, tests, edit \
            ripple, or review context; use `get_symbol` when the declaration name is known.",
        request_model: "SearchParams",
        result_model: "SearchResult",
    },
];
