//! Checks of the compact text of a tool result against its structured content.
//!
//! The text is a curated layout that leaves the normal state implicit, so [`layout`] checks
//! what the layout promises for each tool: `search`, `get_symbol`, and `facets`. The layout
//! rules live in `crates/rift-mcp/src/output/render.rs` and `render/*.rs`.

mod layout;

use rmcp::model::CallToolResult;

use layout::Tool;

/// Name of the tool whose answer the layout check handles first.
const SEARCH_TOOL: &str = "search";
/// Name of the tool whose answer the layout check handles second.
const GET_SYMBOL_TOOL: &str = "get_symbol";
/// Name of the tool whose answer the layout check handles third.
const FACETS_TOOL: &str = "facets";

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
        FACETS_TOOL if structured.get("declarations").is_some() => {
            syntax_elements_summary_text_states(text, structured)
        }
        FACETS_TOOL => syntax_elements_summary_text_states(text, structured),
        other => Err(format!("no text check for the tool `{other}`")),
    };
    verdict.map_err(|failure| format!("{failure}\n--- text ---\n{text}"))
}

/// Checks that a compact syntax elements read carries its exact excerpt and named structure.
fn syntax_elements_summary_text_states(
    text: &str,
    structured: &serde_json::Value,
) -> Result<(), String> {
    token(text, &structured["symbol"], "symbol")?;
    let declarations = structured["declarations"]
        .as_array()
        .ok_or("declarations")?;
    for declaration in declarations {
        source_lines(text, &declaration["source"]["text"], "\t")?;
        token(text, &declaration["kind"], "declaration kind")?;
        for element in declaration["syntax"].as_array().ok_or("syntax structure")? {
            token(text, &element["kind"], "syntax kind")?;
            let index = element["index"].as_u64().ok_or("syntax index")?;
            if !text.contains(&format!("index {index}")) {
                return Err(format!("missing syntax index {index}"));
            }
        }
    }
    if let Some(excerpt) = structured.get("excerpt") {
        source_lines(text, &excerpt["text"], "\t")?;
    }
    if let Some(revision) = structured["revision"].as_str()
        && revision.starts_with("rift://revision/tree/")
        && text.contains(revision)
    {
        return Err("working tree revision in compact text".to_owned());
    }
    Ok(())
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
            if let Some(syntax_element) = declaration.get("syntax_element") {
                token(text, syntax_element, "declaration syntax element")?;
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
    if let Some(revision) = structured["revision"].as_str()
        && revision.starts_with("rift://revision/tree/")
        && text.contains(revision)
    {
        return Err("working tree revision in compact text".to_owned());
    }
    Ok(())
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

fn query_continuation_text(text: &str, _structured: &serde_json::Value) -> Result<(), String> {
    let continuations: Vec<_> = text
        .lines()
        .filter_map(|line| line.strip_prefix("next query | "))
        .collect();
    if !continuations.is_empty() {
        return Err("query continuation differs".into());
    }
    Ok(())
}

fn query_term(term: &serde_json::Value, blank_width: usize) -> Result<String, String> {
    let value = term["value"].as_str().ok_or("query term value")?;
    let quoted = || serde_json::to_string(value).map_err(|error| error.to_string());
    match term["type"].as_str() {
        Some("iri") => Ok(query_iri(value)),
        Some("blank_node") => {
            let mut display = format!(
                "_:{}",
                &value[..value.ceil_char_boundary(blank_width.max(12))]
            );
            if let Some(providers) = term["providers"]
                .as_array()
                .filter(|values| !values.is_empty())
            {
                let providers = providers
                    .iter()
                    .map(|value| value.as_str().ok_or("query provider"))
                    .collect::<Result<Vec<_>, _>>()?;
                display.push_str(" (providers: ");
                display.push_str(&providers.join(", "));
                display.push(')');
            }
            Ok(display)
        }
        Some("language_literal") => Ok(format!(
            "{}@{}",
            quoted()?,
            term["language"].as_str().ok_or("term language")?
        )),
        Some("literal") => {
            let datatype = term["datatype"].as_str().ok_or("term datatype")?;
            match datatype {
                "http://www.w3.org/2001/XMLSchema#string" => quoted(),
                "http://www.w3.org/2001/XMLSchema#integer" if value.parse::<i128>().is_ok() => {
                    Ok(value.to_owned())
                }
                "http://www.w3.org/2001/XMLSchema#boolean" if matches!(value, "true" | "false") => {
                    Ok(value.to_owned())
                }
                _ => Ok(format!("{}^^{}", quoted()?, query_iri(datatype))),
            }
        }
        _ => Err("unknown RDF term".into()),
    }
}

fn query_iri(value: &str) -> String {
    for (namespace, prefix) in [
        ("urn:rift:vocabulary:", ":"),
        ("http://www.w3.org/2001/XMLSchema#", "xsd:"),
        ("http://www.w3.org/1999/02/22-rdf-syntax-ns#", "rdf:"),
        ("http://www.w3.org/2000/01/rdf-schema#", "rdfs:"),
    ] {
        if let Some(local) = value.strip_prefix(namespace) {
            return format!("{prefix}{local}");
        }
    }
    format!("<{value}>")
}

// Spaces around column separators belong to the table, so cell whitespace stays intact.
fn query_cells(line: Option<&str>) -> Result<Vec<String>, String> {
    let body = line
        .and_then(|line| line.strip_prefix("| "))
        .and_then(|line| line.strip_suffix(" |"))
        .ok_or("query table row")?;
    body.split(" | ")
        .map(|cell| {
            let mut chars = cell.trim_ascii().chars();
            let mut value = String::new();
            while let Some(character) = chars.next() {
                value.push(match character {
                    '\\' => chars.next().ok_or("incomplete Markdown escape")?,
                    '|' => return Err("unescaped table delimiter".into()),
                    other => other,
                });
            }
            Ok(value)
        })
        .collect()
}

fn query_rdf_row<'a>(
    line: Option<&str>,
    terms: impl IntoIterator<Item = Option<&'a serde_json::Value>>,
) -> Result<(), String> {
    let cells = query_cells(line)?;
    let mut cells = cells.iter();
    for term in terms {
        let actual = cells.next().ok_or("missing query cell")?;
        let blank_width = actual
            .strip_prefix("_:")
            .and_then(|label| label.split_ascii_whitespace().next())
            .map_or(12, str::len);
        let expected =
            term.map_or_else(|| Ok(String::new()), |term| query_term(term, blank_width))?;
        if *actual != expected {
            return Err(format!(
                "query cell differs: expected {expected}, got {actual}"
            ));
        }
    }
    if cells.next().is_some() {
        return Err("unexpected query cell".into());
    }
    Ok(())
}

fn query_heading(
    lines: &mut std::str::Lines<'_>,
    structured: &serde_json::Value,
) -> Result<bool, String> {
    let pagination = &structured["pagination"];
    let (total, page, pages, max_results) = if structured["answer"]["result"] == "boolean" {
        (1, 1, 1, 1)
    } else {
        (
            pagination["total"].as_u64().ok_or("query total")?,
            pagination["page"].as_u64().ok_or("query page")?,
            pagination["pages"].as_u64().ok_or("query total pages")?,
            pagination["max_results"]
                .as_u64()
                .ok_or("query results per page")?,
        )
    };
    if total == 0 {
        if lines.next()
            != Some("No results were found. Please check the query examples to build it correctly.")
            || lines.next().is_some()
        {
            return Err("empty query report differs".into());
        }
        return Ok(false);
    }
    let heading = format!(
        "Query finished: {total} results found on {page}/{pages} pages with {max_results} results per page."
    );
    if lines.next() != Some(heading.as_str()) || lines.next() != Some("") {
        return Err("query heading differs".into());
    }
    Ok(true)
}

fn query_columns(
    lines: &mut std::str::Lines<'_>,
    headers: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<(), String> {
    let found = query_cells(lines.next())?;
    let expected = headers
        .into_iter()
        .map(|header| header.as_ref().to_owned())
        .collect::<Vec<_>>();
    let divider = query_cells(lines.next())?;
    if found != expected
        || divider.len() != found.len()
        || divider
            .iter()
            .any(|cell| cell.len() < 3 || !cell.bytes().all(|byte| byte == b'-'))
    {
        return Err("query table header differs".into());
    }
    Ok(())
}

fn bindings_text(
    answer: &serde_json::Value,
    lines: &mut std::str::Lines<'_>,
) -> Result<(), String> {
    let variables = answer["variables"]
        .as_array()
        .ok_or("query variables")?
        .iter()
        .map(|value| value.as_str().ok_or("query variable"))
        .collect::<Result<Vec<_>, _>>()?;
    let rows = answer["bindings"].as_array().ok_or("binding rows")?;
    if variables.is_empty() {
        query_columns(lines, ["result"])?;
        for _ in rows {
            query_rdf_row(lines.next(), [None])?;
        }
    } else {
        query_columns(lines, variables.iter().map(|name| format!("?{name}")))?;
        for row in rows {
            query_rdf_row(lines.next(), variables.iter().map(|name| row.get(*name)))?;
        }
    }
    Ok(())
}

fn query_table(lines: &mut std::str::Lines<'_>, answer: &serde_json::Value) -> Result<(), String> {
    match answer["result"].as_str() {
        Some("bindings") => bindings_text(answer, lines)?,
        Some("boolean") => {
            query_columns(lines, ["result"])?;
            let value = answer["value"].as_bool().ok_or("query boolean value")?;
            if query_cells(lines.next())? != [value.to_string()] {
                return Err("query boolean differs".into());
            }
        }
        Some("graph") => {
            let columns = ["subject", "predicate", "object"];
            query_columns(lines, columns)?;
            for triple in answer["triples"].as_array().ok_or("query triples")? {
                query_rdf_row(lines.next(), columns.map(|column| triple.get(column)))?;
            }
        }
        _ => return Err("unknown query result".into()),
    }
    if lines.next().is_some() {
        return Err("text after the query table".into());
    }
    Ok(())
}

fn query_text_states(text: &str, structured: &serde_json::Value) -> Result<(), String> {
    if structured["outcome"] == "unavailable" {
        if !text.contains("unavailable") {
            return Err("missing unavailable outcome".into());
        }
    } else {
        let mut lines = text.lines();
        if query_heading(&mut lines, structured)? {
            query_table(&mut lines, &structured["answer"])?;
        }
    }
    query_continuation_text(text, structured)?;
    if text.lines().any(|line| line.starts_with("view ")) {
        return Err("routine view in query text".into());
    }
    if structured.get("warnings").is_some()
        || text.lines().any(|line| line.starts_with("warning | "))
    {
        return Err("read warnings belong in the logs".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rmcp::model::ContentBlock;
    use serde_json::{Value, json};

    use super::*;

    const ASK_TRUE: &str = concat!(
        "Query finished: 1 results found on 1/1 pages with 1 results per page.\n\n",
        "| result |\n| ------ |\n| true   |\n",
    );

    /// A successful result holding `text` and `structured`.
    fn result_of(text: &str, structured: Value) -> CallToolResult {
        let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
        result.structured_content = Some(structured);
        result
    }

    #[test]
    fn query_provider_sets_are_present_in_text_only_when_projected() {
        let answer = json!({
            "answer": {"result": "bindings", "variables": ["edge"], "bindings": [{
                "edge": {"type": "blank_node", "value": "edge", "providers": ["engine", "lsp", "static"]}
            }]},
            "pagination": {"page": 1, "pages": 1, "max_results": 10, "total": 1}
        });
        let text = concat!(
            "Query finished: 1 results found on 1/1 pages with 10 results per page.\n\n",
            "| ?edge                                    |\n| ---------------------------------------- |\n| \\_:edge (providers: engine, lsp, static) |\n",
        );
        let included = result_of(text, answer.clone());
        assert_eq!(text_states(SEARCH_TOOL, &included), Ok(()));
        let missing = result_of(
            &text.replace(" (providers: engine, lsp, static)", ""),
            answer,
        );
        assert!(text_states(SEARCH_TOOL, &missing).is_err());
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
        let no_structured =
            CallToolResult::success(vec![ContentBlock::text("0 syntax elements\n")]);
        let no_content = CallToolResult::success(Vec::new());
        assert!(text_states(FACETS_TOOL, &no_structured).is_err());
        assert!(text_states(FACETS_TOOL, &no_content).is_err());
    }

    #[test]
    fn a_non_text_block_is_refused() {
        let mut result = CallToolResult::success(vec![ContentBlock::image("AAAA", "image/png")]);
        result.structured_content = Some(json!({ "kinds": [], "source": [] }));
        assert!(text_states(FACETS_TOOL, &result).is_err());
    }

    #[test]
    fn each_tool_takes_its_layout_check_and_an_unknown_tool_is_refused() {
        let search = result_of(
            "0 results\n",
            json!({
                "results": [],
                "pagination": {"page": 1, "pages": 0, "max_results": 10, "total": 0}
            }),
        );
        assert_eq!(text_states(SEARCH_TOOL, &search), Ok(()));
        assert!(text_states("other", &search).is_err());
        assert!(text_states(GET_SYMBOL_TOOL, &search).is_err());
        assert!(text_states(FACETS_TOOL, &search).is_err());
        let syntax_elements = result_of(
            "rift://symbol/local/rust/app/beacon\nNo declaration source available.\n",
            json!({
                "symbol": "rift://symbol/local/rust/app/beacon",
                "declarations": [],
                "revision": "rift://revision/tree/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            }),
        );
        assert_eq!(text_states(FACETS_TOOL, &syntax_elements), Ok(()));
        let answer = result_of(
            ASK_TRUE,
            json!({
                "answer": { "result": "boolean", "value": true }
            }),
        );
        assert!(text_states(FACETS_TOOL, &answer).is_err());
        assert_eq!(text_states(SEARCH_TOOL, &answer), Ok(()));
    }

    #[test]
    fn query_text_omits_routine_view_metadata() {
        let structured = json!({
            "answer": { "result": "boolean", "value": true },
            "view": { "id": "claims.syntax_elements", "expires_at": "2030-01-01T00:15:00Z" }
        });
        assert_eq!(
            text_states(SEARCH_TOOL, &result_of(ASK_TRUE, structured.clone())),
            Ok(())
        );
        let with_view =
            format!("{ASK_TRUE}view claims.syntax_elements | expires 2030-01-01T00:15:00Z\n");
        assert!(text_states(SEARCH_TOOL, &result_of(&with_view, structured)).is_err());
    }

    #[test]
    fn query_answers_refuse_diagnostics_in_either_representation() {
        let structured = json!({
            "answer": { "result": "boolean", "value": true },
            "warnings": [{ "code": "traversal_truncated", "detail": "flow discovery reached the provider request bound" }]
        });
        assert!(text_states(SEARCH_TOOL, &result_of(ASK_TRUE, structured.clone())).is_err());
        let mut without_warnings = structured;
        without_warnings
            .as_object_mut()
            .expect("query result")
            .remove("warnings");
        let warned =
            format!("{ASK_TRUE}warning | flow discovery reached the provider request bound\n");
        assert!(text_states(SEARCH_TOOL, &result_of(&warned, without_warnings)).is_err());
    }
}
