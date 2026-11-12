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
        GET_SYMBOL_TOOL if structured.get("outcome").is_some() => {
            canonical_text_states(text, structured)
        }
        GET_SYMBOL_TOOL => layout::text_states(Tool::GetSymbol, text, structured),
        NODES_TOOL => layout::text_states(Tool::Nodes, text, structured),
        other => Err(format!("no text check for the tool `{other}`")),
    };
    verdict.map_err(|failure| format!("{failure}\n--- text ---\n{text}"))
}

/// Checks the exact symbol and its original declaration projections in canonical text.
fn canonical_text_states(text: &str, structured: &serde_json::Value) -> Result<(), String> {
    let outcome = structured["outcome"].as_str().ok_or("$.outcome")?;
    if outcome == "found" {
        token(text, &structured["symbol"]["id"], "$.symbol.id")?;
        for declaration in structured["declarations"]
            .as_array()
            .ok_or("$.declarations")?
        {
            let place = declaration
                .get("path")
                .or_else(|| declaration.get("unit"))
                .ok_or("declaration location")?;
            token(text, place, "declaration location")?;
            let range = format!(
                "{}..{}",
                declaration["range"]["start"], declaration["range"]["end"]
            );
            if !text.contains(&range) {
                return Err(format!("declaration range {range}"));
            }
            if let Some(source) = declaration.get("source") {
                source_lines(text, source, "\t\t")?;
            }
            if let Some(node) = declaration.get("node") {
                token(text, node, "declaration node")?;
            }
        }
        if let Some(history) = structured.get("history") {
            let label = if history["complete"] == true {
                "history:"
            } else {
                "history (incomplete):"
            };
            if !text.contains(label) {
                return Err("$.history.complete".to_owned());
            }
            for edge in history["edges"].as_array().ok_or("$.history.edges")? {
                let spaced = edge["edge"].as_str().map(|name| name.replace('_', " "));
                token(text, &serde_json::json!(spaced), "$.history.edges.edge")?;
            }
        }
        if let Some(cursor) = structured.get("declaration_cursor") {
            token(text, cursor, "$.declaration_cursor")?;
        }
    } else if outcome == "missing" {
        token(
            text,
            &structured["symbol_not_found"]["id"],
            "$.symbol_not_found.id",
        )?;
        if !text.contains("symbol_not_found") {
            return Err("$.outcome missing".to_owned());
        }
        for candidate in structured["symbol_not_found"]["alternatives"]
            .as_array()
            .ok_or("$.symbol_not_found.alternatives")?
        {
            token(text, &candidate["id"], "alternative id")?;
            token(text, &candidate["name"], "alternative name")?;
        }
    } else {
        token(text, &structured["id"], "$.id")?;
    }
    view_text(text, structured)
}

fn token(text: &str, value: &serde_json::Value, at: &str) -> Result<(), String> {
    let value = value.as_str().ok_or_else(|| at.to_owned())?;
    if !text.contains(value) {
        return Err(format!("{at}: missing {value}"));
    }
    Ok(())
}

fn source_lines(text: &str, value: &serde_json::Value, indent: &str) -> Result<(), String> {
    let source = value.as_str().ok_or("source string")?;
    for line in source.lines() {
        if !text.lines().any(|actual| {
            actual.strip_prefix(indent) == Some(line) || (line.is_empty() && actual.is_empty())
        }) {
            return Err(format!("original source line: {line}"));
        }
    }
    Ok(())
}

fn view_text(text: &str, structured: &serde_json::Value) -> Result<(), String> {
    if let Some(view) = structured.get("view") {
        token(text, &view["id"], "$.view.id")?;
        token(text, &view["expires_at"], "$.view.expires_at")?;
    }
    if let Some(revision) = structured.get("revision") {
        token(text, revision, "$.revision")?;
    }
    for warning in structured["warnings"].as_array().into_iter().flatten() {
        token(text, &warning["code"], "$.warnings.code")?;
    }
    Ok(())
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
    fn canonical_declaration_source_keeps_two_layout_tabs_and_original_bytes() {
        let source = json!("pub fn beacon() {\n\tlet original = 1;\n\n}\n");
        let text = "\t\tpub fn beacon() {\n\t\t\tlet original = 1;\n\n\t\t}\n";
        assert_eq!(source_lines(text, &source, "\t\t"), Ok(()));
        assert!(source_lines(&text.replace("original", "changed"), &source, "\t\t").is_err());
        assert!(source_lines(&text.replacen("\t\tpub", "\tpub", 1), &source, "\t\t").is_err());
    }

    #[test]
    fn exact_source_keeps_one_layout_tab_and_original_bytes() {
        let source = json!("    pub fn beacon() {}\n");
        let text = "\t    pub fn beacon() {}\n";
        assert_eq!(source_lines(text, &source, "\t"), Ok(()));
        assert!(source_lines(&text.replace("beacon", "changed"), &source, "\t").is_err());
        assert!(source_lines(&text.replacen('\t', "\t\t", 1), &source, "\t").is_err());
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
