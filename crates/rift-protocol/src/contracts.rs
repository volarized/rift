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

/// The `get_symbol` description an MCP client shows its agent.
pub const GET_SYMBOL_DESCRIPTION: &str = "Finds Rust declarations and their source by exact \
    symbol name. Each hit carries the declaration and its source excerpt; `include_body: false` \
    omits both. Use `search` when the name is not exactly known.";

/// The `search` description an MCP client shows its agent.
pub const SEARCH_DESCRIPTION: &str = "Searches indexed Rust declarations and source lines by \
    lexical `query`. Use `get_symbol` when the declaration name is known.";

/// Read-only MCP tools implemented by this release.
pub const TOOL_CONTRACTS: &[ToolContract] = &[
    ToolContract {
        name: "get_symbol",
        description: GET_SYMBOL_DESCRIPTION,
        request_model: "GetSymbolParams",
        result_model: "GetSymbolResult",
    },
    ToolContract {
        name: "search",
        description: SEARCH_DESCRIPTION,
        request_model: "SearchParams",
        result_model: "SearchResult",
    },
];
