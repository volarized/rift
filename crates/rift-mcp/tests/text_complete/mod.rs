//! Checks of the compact text of a tool result against its structured content.
//!
//! The text is a curated layout that leaves the normal state implicit, so [`layout`] checks
//! what the layout promises for each tool: `search`, `get_symbol`, and `nodes`. The layout
//! rules live in `crates/rift-mcp/src/output/render.rs` and `render/*.rs`.

mod layout;

use rmcp::model::CallToolResult;

use layout::Tool;

/// Name of the tool whose answer the layout check handles first.
const SEARCH_TOOL: &str = "search";
/// Name of the tool whose answer the layout check handles second.
const GET_SYMBOL_TOOL: &str = "get_symbol";
/// Name of the tool whose answer the layout check handles third.
const NODES_TOOL: &str = "nodes";

/// Panics unless the text of a successful `tool` result states what its form promises.
///
/// # Panics
///
/// Panics with the JSON path of the first fact the text does not state, and the text.
pub(crate) fn assert_text_states(tool: &str, result: &CallToolResult) {
    if let Err(failure) = text_states(tool, result) {
        panic!("{tool}: {failure}");
    }
}

/// The check behind [`assert_text_states`], returning the failure instead of panicking.
///
/// The failure names the JSON path of the missing fact and prints the text.
fn text_states(tool: &str, result: &CallToolResult) -> Result<(), String> {
    let [block] = result.content.as_slice() else {
        return Err(format!(
            "content must be one block, got {:?}",
            result.content
        ));
    };
    let text = block
        .as_text()
        .map(|text| text.text.as_str())
        .ok_or("the content block must be text")?;
    let structured = result
        .structured_content
        .as_ref()
        .ok_or("the result must carry structured content")?;
    let verdict = match tool {
        SEARCH_TOOL => layout::text_states(Tool::Search, text, structured),
        GET_SYMBOL_TOOL => layout::text_states(Tool::GetSymbol, text, structured),
        NODES_TOOL => layout::text_states(Tool::Nodes, text, structured),
        other => Err(format!("no text check for the tool `{other}`")),
    };
    verdict.map_err(|failure| format!("{failure}\n--- text ---\n{text}"))
}

#[cfg(test)]
mod tests {
    use rmcp::model::ContentBlock;
    use serde_json::{Value, json};

    use super::*;

    /// A successful result holding `text` and `structured`.
    fn result_of(text: &str, structured: Value) -> CallToolResult {
        let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
        result.structured_content = Some(structured);
        result
    }

    #[test]
    fn a_result_without_structured_content_or_text_is_refused() {
        let no_structured = CallToolResult::success(vec![ContentBlock::text("0 nodes\n")]);
        let no_content = CallToolResult::success(Vec::new());
        assert!(text_states(NODES_TOOL, &no_structured).is_err());
        assert!(text_states(NODES_TOOL, &no_content).is_err());
    }

    #[test]
    fn a_non_text_block_is_refused() {
        let mut result = CallToolResult::success(vec![ContentBlock::image("AAAA", "image/png")]);
        result.structured_content = Some(json!({ "nodes": [], "source": [] }));
        assert!(text_states(NODES_TOOL, &result).is_err());
    }

    #[test]
    fn each_tool_takes_its_layout_check_and_an_unknown_tool_is_refused() {
        let search = result_of(
            "0 results\n",
            json!({
                "results": [],
                "pagination": { "page_index": 0, "total_pages": 0 }
            }),
        );
        assert_eq!(text_states(SEARCH_TOOL, &search), Ok(()));
        assert!(text_states("other", &search).is_err());
        assert!(text_states(GET_SYMBOL_TOOL, &search).is_err());
        assert!(text_states(NODES_TOOL, &search).is_err());
        let nodes = result_of("0 nodes\n", json!({ "nodes": [], "source": [] }));
        assert_eq!(text_states(NODES_TOOL, &nodes), Ok(()));
        assert!(text_states(SEARCH_TOOL, &nodes).is_err());
    }
}
