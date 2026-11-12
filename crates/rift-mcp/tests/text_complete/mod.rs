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
        SEARCH_TOOL
            if structured.get("answer").is_some() || structured["outcome"] == "unavailable" =>
        {
            query_text_states(text, structured)
        }
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

fn query_continuation_text(text: &str, structured: &serde_json::Value) -> Result<(), String> {
    let continuations: Vec<_> = text
        .lines()
        .filter_map(|line| line.strip_prefix("next query · "))
        .collect();
    let expected = structured["pagination"]["next_query"].as_str();
    if continuations != expected.into_iter().collect::<Vec<_>>() {
        return Err("query continuation differs".into());
    }
    Ok(())
}

fn query_term(term: &serde_json::Value) -> Result<String, String> {
    let value = term["value"].as_str().ok_or("query term value")?;
    let quoted = || serde_json::to_string(value).map_err(|error| error.to_string());
    match term["type"].as_str() {
        Some("iri") => Ok(format!("<{value}>")),
        Some("blank_node") => Ok(format!("_:{value}")),
        Some("language_literal") => Ok(format!(
            "{}@{}",
            quoted()?,
            term["language"].as_str().ok_or("term language")?
        )),
        Some("literal") => {
            let datatype = term["datatype"].as_str().ok_or("term datatype")?;
            if datatype == "http://www.w3.org/2001/XMLSchema#string" {
                quoted()
            } else {
                Ok(format!("{}^^<{datatype}>", quoted()?))
            }
        }
        _ => Err("unknown RDF term".into()),
    }
}

fn bindings_text(answer: &serde_json::Value, text: &mut std::str::Lines<'_>) -> Result<(), String> {
    let variables = answer["variables"]
        .as_array()
        .ok_or("query variables")?
        .iter()
        .map(|value| value.as_str().ok_or("query variable"))
        .collect::<Result<Vec<_>, _>>()?;
    let header = variables
        .iter()
        .map(|name| format!("?{name}"))
        .collect::<Vec<_>>()
        .join("\t");
    if text.next() != Some(header.as_str()) {
        return Err("binding variables differ".into());
    }
    for row in answer["bindings"].as_array().ok_or("binding rows")? {
        let cells = variables
            .iter()
            .map(|name| row.get(*name).map_or_else(|| Ok(String::new()), query_term))
            .collect::<Result<Vec<_>, _>>()?;
        if text.next() != Some(cells.join("\t").as_str()) {
            return Err("binding row differs".into());
        }
    }
    Ok(())
}

fn query_text_states(text: &str, structured: &serde_json::Value) -> Result<(), String> {
    if structured["outcome"] == "unavailable" {
        if !text.contains("unavailable") {
            return Err("missing unavailable outcome".into());
        }
    } else {
        let answer = &structured["answer"];
        let mut lines = text.lines();
        match answer["result"].as_str() {
            Some("boolean")
                if lines.next().and_then(|value| value.parse::<bool>().ok())
                    == answer["value"].as_bool() => {}
            Some("boolean") => return Err("query boolean differs".into()),
            Some("bindings") => bindings_text(answer, &mut lines)?,
            Some("graph") => {
                for triple in answer["triples"].as_array().ok_or("query triples")? {
                    let expected = format!(
                        "{} {} {} .",
                        query_term(&triple["subject"])?,
                        query_term(&triple["predicate"])?,
                        query_term(&triple["object"])?
                    );
                    if lines.next() != Some(expected.as_str()) {
                        return Err("query triple differs".into());
                    }
                }
            }
            _ => return Err("unknown query result".into()),
        }
        let total = structured["pagination"]["total_pages"]
            .as_u64()
            .ok_or("query total pages")?;
        if total > 1 {
            let page = structured["pagination"]["page_index"]
                .as_u64()
                .ok_or("query page index")?;
            let expected = format!("page {}/{total}", page.saturating_add(1));
            if lines.next() != Some(expected.as_str()) {
                return Err("query page differs".into());
            }
        }
    }
    query_continuation_text(text, structured)?;
    view_text(text, structured)
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
        let answer = result_of(
            "true\n",
            json!({
                "answer": { "result": "boolean", "value": true },
                "pagination": { "page_index": 0, "total_pages": 1 },
                "warnings": []
            }),
        );
        assert!(text_states(NODES_TOOL, &answer).is_err());
        assert_eq!(text_states(SEARCH_TOOL, &answer), Ok(()));
    }

    #[test]
    fn search_query_requires_its_exact_captured_view_and_original_expiry() {
        let structured = json!({
            "answer": { "result": "boolean", "value": true },
            "pagination": { "page_index": 0, "total_pages": 1 },
            "warnings": [],
            "view": { "id": "claims.nodes", "expires_at": "2030-01-01T00:15:00Z" }
        });
        let original = "true\nview claims.nodes · expires 2030-01-01T00:15:00Z\n";
        assert_eq!(
            text_states(SEARCH_TOOL, &result_of(original, structured.clone())),
            Ok(())
        );
        for text in [
            "true\n",
            "true\nview claims.other · expires 2030-01-01T00:15:00Z\n",
            "true\nview claims.nodes · expires 2030-01-01T00:16:00Z\n",
        ] {
            assert!(text_states(SEARCH_TOOL, &result_of(text, structured.clone())).is_err());
        }
    }

    #[test]
    fn query_captured_view_does_not_replace_required_warning() {
        let structured = json!({
            "answer": { "result": "boolean", "value": true },
            "pagination": { "page_index": 0, "total_pages": 1 },
            "warnings": [{ "code": "stale_index", "detail": "index lags" }],
            "view": { "id": "claims.nodes", "expires_at": "2030-01-01T00:15:00Z" }
        });
        let missing = "true\nview claims.nodes · expires 2030-01-01T00:15:00Z\n";
        assert!(text_states(SEARCH_TOOL, &result_of(missing, structured.clone())).is_err());
        let complete = "true\n1 warning\n\tstale_index: index lags\nview claims.nodes · expires 2030-01-01T00:15:00Z\n";
        assert_eq!(
            text_states(SEARCH_TOOL, &result_of(complete, structured)),
            Ok(())
        );
    }
}
