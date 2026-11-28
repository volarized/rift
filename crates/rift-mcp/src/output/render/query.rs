//! Plain-text tables for SPARQL answers.

use std::collections::{BTreeMap, BTreeSet};

use rift_protocol::query::{QueryAnswer, QueryReadResult, QueryResult, RdfTerm};
use rift_protocol::revision::{REVISION_FIELD, is_commit_id};
use rift_protocol::search_read::SearchUnavailable;
use rift_search::sparql::VOCABULARY_NAMESPACE;
use rmcp::model::MetaObject;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use unicode_width::UnicodeWidthStr as _;

use super::facts::cut_hash;
use super::{Render, TextError, TextWriter};
use crate::output::text::visible;

const BLANK_PREFIX_MIN: usize = 12;
const PRESENTATION_KEY: &str = "rift/query-presentation";
const XSD_NAMESPACE: &str = "http://www.w3.org/2001/XMLSchema#";
const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
const XSD_BOOLEAN: &str = "http://www.w3.org/2001/XMLSchema#boolean";
const RDF_NAMESPACE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const RDFS_NAMESPACE: &str = "http://www.w3.org/2000/01/rdf-schema#";

type Row = BTreeMap<String, RdfTerm>;

/// Blank node label width retained across pages of a query.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct QueryDisplay {
    blank_prefix: usize,
}

impl QueryDisplay {
    pub(crate) fn for_answer(answer: &QueryAnswer) -> Self {
        Self {
            blank_prefix: blank_prefix(answer),
        }
    }
}

/// Picks a shared byte width that distinguishes every blank node in the retained answer.
/// Adjacent labels in lexical order have the longest common prefix of any pair. The result
/// retains just the width; rendering rounds it up to a character boundary per label.
fn blank_prefix(answer: &QueryAnswer) -> usize {
    fn blank(term: &RdfTerm) -> Option<&str> {
        match term {
            RdfTerm::BlankNode { value, .. } => Some(value),
            _ => None,
        }
    }
    let labels: BTreeSet<_> = match answer {
        QueryAnswer::Bindings { bindings, .. } => bindings
            .iter()
            .flat_map(Row::values)
            .filter_map(blank)
            .collect(),
        QueryAnswer::Graph { triples } => triples
            .iter()
            .flat_map(|triple| [&triple.subject, &triple.predicate, &triple.object])
            .filter_map(blank)
            .collect(),
        QueryAnswer::Boolean { .. } => return 0,
    };
    labels
        .iter()
        .zip(labels.iter().skip(1))
        .map(|(left, right)| {
            left.bytes()
                .zip(right.bytes())
                .take_while(|(left, right)| left == right)
                .count()
                + 1
        })
        .filter(|width| *width > BLANK_PREFIX_MIN)
        .max()
        .unwrap_or(0)
}

// Keep the QueryReadResult schema and examples on the wire. Display data is used by text.
#[derive(JsonSchema, Serialize)]
#[serde(transparent)]
#[schemars(with = "QueryReadResult")]
pub(crate) struct QueryOutput {
    pub(crate) result: QueryReadResult,
    #[serde(skip)]
    display: QueryDisplay,
}

impl QueryOutput {
    pub(crate) const fn new(result: QueryReadResult, display: QueryDisplay) -> Self {
        Self { result, display }
    }

    /// Recovers the retained blank node label width when the proxy qualifies a result.
    pub(crate) fn take_display(meta: &mut Option<MetaObject>) -> QueryDisplay {
        let value = meta
            .as_mut()
            .and_then(|meta| meta.0.remove(PRESENTATION_KEY));
        if meta.as_ref().is_some_and(|meta| meta.0.is_empty()) {
            *meta = None;
        }
        value
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default()
    }
}

impl From<QueryReadResult> for QueryOutput {
    fn from(result: QueryReadResult) -> Self {
        Self::new(result, QueryDisplay::default())
    }
}

impl Render for QueryOutput {
    fn metadata(&self) -> Result<Option<MetaObject>, serde_json::Error> {
        if self.display == QueryDisplay::default() {
            return Ok(None);
        }
        let mut meta = MetaObject::new();
        meta.0.insert(
            PRESENTATION_KEY.to_owned(),
            serde_json::to_value(&self.display)?,
        );
        Ok(Some(meta))
    }

    fn log_warnings(&mut self) {
        let warnings = match &mut self.result {
            QueryReadResult::Results(result) => &mut result.warnings,
            QueryReadResult::Unavailable(SearchUnavailable::Unavailable { warnings, .. }) => {
                warnings
            }
        };
        crate::output::record_warnings(warnings);
    }

    fn render(&self, out: &mut TextWriter) -> Result<(), TextError> {
        match &self.result {
            QueryReadResult::Results(result) => render_result(out, result, &self.display),
            QueryReadResult::Unavailable(SearchUnavailable::Unavailable { reason, .. }) => {
                out.raw_line(0, &super::unavailable::line(*reason))
            }
        }
    }
}

fn render_result(
    out: &mut TextWriter,
    result: &QueryResult,
    display: &QueryDisplay,
) -> Result<(), TextError> {
    let rows = match &result.answer {
        QueryAnswer::Bindings { bindings, .. } => bindings.len(),
        QueryAnswer::Graph { triples } => triples.len(),
        QueryAnswer::Boolean { .. } => 1,
    };
    let (total, page, pages, offset) = result
        .pagination
        .as_ref()
        .map_or((rows as u64, 1, 1, 0), |page| {
            (page.total, page.page, page.pages, page.offset())
        });
    if total == 0 {
        return out.raw_line(
            0,
            "No results were found. Please check the query examples to build it correctly.",
        );
    }

    let first = offset.saturating_add(1);
    let range = match (rows, total) {
        (0, _) => format!("0 rows of {total}"),
        (1, 1) => "row 1".to_owned(),
        (1, _) => format!("row {first} of {total}"),
        _ => {
            let last = offset.saturating_add(rows);
            format!("rows {first}-{last} of {total}")
        }
    };
    out.raw_line(0, &format!("{range} | page {page}/{pages}"))?;
    out.blank_line()?;

    let terms = Terms {
        blank_prefix: display.blank_prefix.max(BLANK_PREFIX_MIN),
    };
    match &result.answer {
        QueryAnswer::Boolean { value } => {
            render_table(out, ["result"], [Ok(vec![value.to_string()])])
        }
        QueryAnswer::Bindings {
            variables,
            bindings,
        } => render_table(
            out,
            variables.iter().map(|variable| format!("?{variable}")),
            bindings.iter().map(|row| {
                variables
                    .iter()
                    .map(|variable| {
                        row.get(variable).map_or_else(
                            || Ok(String::new()),
                            |value| terms.term(value, Some(variable)),
                        )
                    })
                    .collect()
            }),
        ),
        QueryAnswer::Graph { triples } => render_table(
            out,
            ["subject", "predicate", "object"],
            triples.iter().map(|triple| {
                [&triple.subject, &triple.predicate, &triple.object]
                    .into_iter()
                    .map(|term| terms.term(term, None))
                    .collect()
            }),
        ),
    }
}

const DIVIDER_WIDTH_MIN: usize = 3;

fn render_table(
    out: &mut TextWriter,
    headers: impl IntoIterator<Item = impl Into<String>>,
    rows: impl IntoIterator<Item = Result<Vec<String>, TextError>>,
) -> Result<(), TextError> {
    let mut headers = headers.into_iter().map(Into::into).collect::<Vec<_>>();
    // SELECT * can return one empty binding when the query has no variables.
    if headers.is_empty() {
        headers.push("result".to_owned());
    }
    let columns = headers.len();
    let mut widths = vec![DIVIDER_WIDTH_MIN; columns];
    // Each row has an opening pipe and line feed, and three delimiter bytes per cell.
    let row_bytes = columns.saturating_mul(3).saturating_add(2);
    let mut occupied_width = columns.saturating_mul(DIVIDER_WIDTH_MIN);
    let mut compact_bytes = row_bytes.saturating_add(occupied_width);
    let mut table_rows = Vec::new();
    for row in std::iter::once(Ok(headers)).chain(rows) {
        let mut row = row?;
        if row.is_empty() {
            row.push(String::new());
        }
        compact_bytes = compact_bytes.saturating_add(row_bytes);
        out.check_capacity(compact_bytes)?;
        let mut cells = Vec::with_capacity(columns);
        for (value, column_width) in row.into_iter().zip(&mut widths) {
            let cell = table_cell(&value);
            compact_bytes = compact_bytes.saturating_add(cell.text.len());
            out.check_capacity(compact_bytes)?;
            *column_width = (*column_width).max(cell.width);
            occupied_width = occupied_width.saturating_add(cell.width);
            cells.push(cell);
        }
        table_rows.push(cells);
    }
    let aligned_width: usize = widths.iter().sum();
    let padding = aligned_width
        .saturating_mul(table_rows.len() + 1)
        .saturating_sub(occupied_width);
    let aligned = compact_bytes.saturating_add(padding) <= out.remaining_bytes();
    let widths = if aligned { widths.as_slice() } else { &[] };
    let mut rows = table_rows.into_iter();
    let header = rows.next().expect("the table has a header");
    table_row(out, &header, widths)?;
    let divider = (0..columns)
        .map(|column| {
            let width = widths.get(column).copied().unwrap_or(DIVIDER_WIDTH_MIN);
            TableCell {
                text: "-".repeat(width),
                width,
            }
        })
        .collect::<Vec<_>>();
    table_row(out, &divider, widths)?;
    for row in rows {
        table_row(out, &row, widths)?;
    }
    Ok(())
}

struct TableCell {
    text: String,
    width: usize,
}

fn table_cell(value: &str) -> TableCell {
    let text = visible(value).into_owned();
    let width = text.width();
    TableCell { text, width }
}

fn table_row(out: &mut TextWriter, cells: &[TableCell], widths: &[usize]) -> Result<(), TextError> {
    let mut line = String::from("|");
    for (column, cell) in cells.iter().enumerate() {
        line.push(' ');
        line.push_str(&cell.text);
        let width = widths.get(column).copied().unwrap_or(0);
        line.extend(std::iter::repeat_n(' ', width.saturating_sub(cell.width)));
        line.push_str(" |");
    }
    out.raw_line(0, &line)
}

struct Terms {
    blank_prefix: usize,
}

impl Terms {
    fn term(&self, term: &RdfTerm, column: Option<&str>) -> Result<String, TextError> {
        let quoted = |value: &str| {
            serde_json::to_string(value).map_err(|error| TextError::Custom(error.to_string()))
        };
        match term {
            RdfTerm::Iri { value } => Ok(iri(value)),
            RdfTerm::BlankNode { value, providers } => {
                let end = value.ceil_char_boundary(self.blank_prefix);
                let analyses = if providers.is_empty() {
                    String::new()
                } else {
                    format!(
                        " (providers: {})",
                        providers
                            .iter()
                            .map(AsRef::as_ref)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                Ok(format!("_:{}{analyses}", &value[..end]))
            }
            RdfTerm::Literal { value, datatype } if datatype == XSD_STRING => {
                let text = if column == Some(REVISION_FIELD) && is_commit_id(value) {
                    cut_hash(value)
                } else {
                    value
                };
                quoted(text)
            }
            RdfTerm::Literal { value, datatype }
                if (datatype == XSD_INTEGER && is_integer(value))
                    || (datatype == XSD_BOOLEAN && matches!(value.as_str(), "true" | "false")) =>
            {
                Ok(value.clone())
            }
            RdfTerm::Literal { value, datatype } => {
                Ok(format!("{}^^{}", quoted(value)?, iri(datatype)))
            }
            RdfTerm::LanguageLiteral { value, language } => {
                Ok(format!("{}@{language}", quoted(value)?))
            }
        }
    }
}

fn is_integer(value: &str) -> bool {
    let digits = value.strip_prefix(['+', '-']).unwrap_or(value);
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

fn iri(value: &str) -> String {
    for (namespace, prefix) in [
        (VOCABULARY_NAMESPACE, ":"),
        (XSD_NAMESPACE, "xsd:"),
        (RDF_NAMESPACE, "rdf:"),
        (RDFS_NAMESPACE, "rdfs:"),
    ] {
        if let Some(name) = value.strip_prefix(namespace) {
            return format!("{prefix}{name}");
        }
    }
    format!("<{value}>")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_answer(value: serde_json::Value) -> Result<String, Box<dyn std::error::Error>> {
        let result = QueryOutput::from(serde_json::from_value::<QueryReadResult>(value)?);
        Ok(super::super::text_of(&result)?)
    }

    #[test]
    fn raw_rdf_text_compacts_terms_and_preserves_the_original_projection()
    -> Result<(), Box<dyn std::error::Error>> {
        let output = raw_answer(serde_json::json!({
            "answer": {
                "result": "bindings",
                "variables": ["blank", "value", "typed", "predicate"],
                "bindings": [
                    {
                        "blank": { "type": "blank_node", "value": "first-retained-label" },
                        "value": { "type": "literal", "value": "42", "datatype": XSD_INTEGER },
                        "typed": { "type": "literal", "value": "invalid boolean", "datatype": XSD_BOOLEAN },
                        "predicate": { "type": "iri", "value": "urn:rift:vocabulary:name" }
                    },
                    {
                        "blank": { "type": "blank_node", "value": "different-retained-label" },
                        "value": { "type": "literal", "value": "true", "datatype": XSD_BOOLEAN },
                        "typed": { "type": "language_literal", "value": "source", "language": "en" }
                    },
                    { "blank": { "type": "blank_node", "value": "first-retained-label" } }
                ]
            },
            "pagination": {"page": 1, "pages": 1, "max_results": 10, "total": 3},
            "view": { "id": "retained.answer", "expires_at": "2030-01-01T00:15:00Z" }
        }))?;
        assert_eq!(
            output,
            concat!(
                "rows 1-3 of 3 | page 1/1\n\n",
                "| ?blank         | ?value | ?typed                         | ?predicate |\n",
                "| -------------- | ------ | ------------------------------ | ---------- |\n",
                "| _:first-retain | 42     | \"invalid boolean\"^^xsd:boolean | :name      |\n",
                "| _:different-re | true   | \"source\"@en                    |            |\n",
                "| _:first-retain |        |                                |            |\n",
            )
        );
        Ok(())
    }

    #[test]
    fn graph_text_keeps_distinct_triples_and_provider_sets()
    -> Result<(), Box<dyn std::error::Error>> {
        let blank = serde_json::json!({
            "type": "blank_node", "value": "anonymous", "providers": ["static", "lsp", "engine"]
        });
        let text = raw_answer(serde_json::json!({
            "answer": {
                "result": "graph",
                "triples": [
                    {
                        "subject": blank,
                        "predicate": {"type": "iri", "value": "urn:rift:vocabulary:name"},
                        "object": {"type": "literal", "value": "Rift", "datatype": XSD_STRING}
                    },
                    {
                        "subject": blank,
                        "predicate": {"type": "iri", "value": "urn:rift:vocabulary:count"},
                        "object": {"type": "literal", "value": "7", "datatype": XSD_INTEGER}
                    }
                ]
            },
            "pagination": { "page": 2, "pages": 2, "max_results": 2, "total": 4 }
        }))?;
        assert_eq!(
            text,
            concat!(
                "rows 3-4 of 4 | page 2/2\n\n",
                "| subject                                      | predicate | object |\n",
                "| -------------------------------------------- | --------- | ------ |\n",
                "| _:anonymous (providers: engine, lsp, static) | :name     | \"Rift\" |\n",
                "| _:anonymous (providers: engine, lsp, static) | :count    | 7      |\n",
            )
        );
        Ok(())
    }

    #[test]
    fn plain_text_cells_keep_punctuation_and_one_line_per_row()
    -> Result<(), Box<dyn std::error::Error>> {
        let text = raw_answer(serde_json::json!({
            "answer": {
                "result": "bindings",
                "variables": ["source", "rank", "symbol"],
                "bindings": [{
                    "source": {
                        "type": "literal",
                        "value": "a | b\\c\n<t>*x*_",
                        "datatype": XSD_STRING
                    },
                    "rank": {
                        "type": "literal",
                        "value": "1.0E0",
                        "datatype": "http://www.w3.org/2001/XMLSchema#double"
                    },
                    "symbol": { "type": "iri", "value": "rift://symbol/local/rust/app/retry" }
                }]
            }
        }))?;
        assert_eq!(
            text,
            concat!(
                "row 1 | page 1/1\n\n",
                "| ?source             | ?rank               | ?symbol                              |\n",
                "| ------------------- | ------------------- | ------------------------------------ |\n",
                "| \"a | b\\\\c\\n<t>*x*_\" | \"1.0E0\"^^xsd:double | <rift://symbol/local/rust/app/retry> |\n",
            )
        );
        Ok(())
    }

    #[test]
    fn only_full_git_commits_in_revision_columns_shorten_in_text()
    -> Result<(), Box<dyn std::error::Error>> {
        let sha1 = format!("12345678{}", "a".repeat(32));
        let sha256 = format!("abcdef01{}", "b".repeat(56));
        let literal = |value: &str| serde_json::json!({ "type": "literal", "value": value, "datatype": XSD_STRING });
        let release = "rift://revision/cargo/crates.io/demo@1.0.0";
        let structured = serde_json::json!({
            "answer": {
                "result": "bindings",
                "variables": ["revision", "message"],
                "bindings": [
                    {"revision": literal(&sha1), "message": literal(&sha1)},
                    {"revision": literal(&sha256)},
                    {"revision": literal("1234567890")},
                    {"revision": {"type": "iri", "value": release}},
                ]
            }
        });
        let output = QueryOutput::from(serde_json::from_value::<QueryReadResult>(
            structured.clone(),
        )?);
        assert_eq!(serde_json::to_value(&output)?, structured);
        let text = super::super::text_of(&output)?;
        let cells: Vec<_> = text
            .lines()
            .skip(4)
            .map(|line| line.split('|').nth(1).unwrap_or_default().trim())
            .collect();
        assert_eq!(
            cells,
            [
                r#""12345678""#,
                r#""abcdef01""#,
                r#""1234567890""#,
                &format!("<{release}>")
            ]
        );
        assert!(text.contains(&format!("\"{sha1}\"")));
        assert!(!text.contains(&sha256));
        Ok(())
    }

    #[test]
    fn ask_text_counts_false_as_one_result() -> Result<(), Box<dyn std::error::Error>> {
        for value in [true, false] {
            let text = raw_answer(serde_json::json!({
                "answer": { "result": "boolean", "value": value }
            }))?;
            assert_eq!(
                text,
                format!("row 1 | page 1/1\n\n| result |\n| ------ |\n| {value:<6} |\n")
            );
        }
        Ok(())
    }

    #[test]
    fn empty_answer_differs_from_empty_page_and_empty_projection()
    -> Result<(), Box<dyn std::error::Error>> {
        let text = |total, pages, bindings| {
            raw_answer(serde_json::json!({
                "answer": {"result": "bindings", "variables": [], "bindings": bindings},
                "pagination": {"page": 2, "pages": pages, "max_results": 1, "total": total}
            }))
        };
        assert_eq!(
            text(0, 0, serde_json::json!([]))?,
            "No results were found. Please check the query examples to build it correctly.\n"
        );
        assert_eq!(
            text(1, 1, serde_json::json!([]))?,
            "0 rows of 1 | page 2/1\n\n| result |\n| ------ |\n"
        );
        assert_eq!(
            text(2, 2, serde_json::json!([{}]))?,
            "row 2 of 2 | page 2/2\n\n| result |\n| ------ |\n|        |\n"
        );
        Ok(())
    }

    #[test]
    fn row_heading_uses_actual_page_rows() -> Result<(), Box<dyn std::error::Error>> {
        for (page, total, rows, heading) in [
            (2, 24, 10, "rows 11-20 of 24 | page 2/3"),
            (3, 24, 4, "rows 21-24 of 24 | page 3/3"),
            (3, 21, 1, "row 21 of 21 | page 3/3"),
            (4, 24, 0, "0 rows of 24 | page 4/3"),
        ] {
            let text = raw_answer(serde_json::json!({
                "answer": {
                    "result": "bindings",
                    "variables": ["value"],
                    "bindings": vec![serde_json::json!({}); rows]
                },
                "pagination": rift_protocol::read::Pagination::new(page, 10, total),
            }))?;
            assert_eq!(text.lines().next(), Some(heading));
            assert_eq!(text.lines().count(), 4 + rows);
        }
        Ok(())
    }

    #[test]
    fn marks_table_columns_by_display_width_and_drops_padding_at_the_byte_limit()
    -> Result<(), TextError> {
        let table = |writer: &mut TextWriter| {
            render_table(
                writer,
                ["name", "value"],
                [Ok(vec!["界界界".to_owned(), "e\u{301}".to_owned()])],
            )
        };
        let compact = "| name | value |\n| --- | --- |\n| 界界界 | e\u{301} |\n";
        let mut padded = TextWriter::new(100);
        table(&mut padded)?;
        assert_eq!(
            padded.finish()?,
            "| name   | value |\n| ------ | ----- |\n| 界界界 | e\u{301}     |\n"
        );
        let mut unpadded = TextWriter::new(compact.len());
        table(&mut unpadded)?;
        assert_eq!(unpadded.finish()?, compact);
        let mut refuses = TextWriter::new(compact.len() - 1);
        assert!(matches!(table(&mut refuses), Err(TextError::Overflow(_))));
        assert!(matches!(refuses.finish(), Err(TextError::Overflow(_))));
        Ok(())
    }

    #[test]
    fn blank_labels_keep_a_distinguishing_utf8_prefix_across_pages()
    -> Result<(), Box<dyn std::error::Error>> {
        let first = RdfTerm::BlankNode {
            value: "retained_node_á1_more".to_owned(),
            providers: std::collections::BTreeSet::default(),
        };
        let second = RdfTerm::BlankNode {
            value: "retained_node_ã2_more".to_owned(),
            providers: std::collections::BTreeSet::default(),
        };
        let row = |value| BTreeMap::from([("syntax_element".to_owned(), value)]);
        let variables = vec!["syntax_element".to_owned()];
        let full = QueryAnswer::Bindings {
            variables: variables.clone(),
            bindings: vec![row(first.clone()), row(second.clone()), row(first.clone())],
        };
        let display = QueryDisplay::for_answer(&full);
        let page = |page, value| {
            QueryOutput::new(
                QueryReadResult::Results(Box::new(QueryResult {
                    answer: QueryAnswer::Bindings {
                        variables: variables.clone(),
                        bindings: vec![row(value)],
                    },
                    pagination: Some(rift_protocol::read::Pagination::new(page, 1, 3)),
                    view: None,
                    revision: None,
                    warnings: Vec::new(),
                })),
                display.clone(),
            )
        };
        for (index, value, expected) in [
            (1, first.clone(), "| _:retained_node_á |"),
            (2, second, "| _:retained_node_ã |"),
            (3, first, "| _:retained_node_á |"),
        ] {
            let output = super::super::text_of(&page(index, value))?;
            assert_eq!(output.lines().nth(4), Some(expected));
        }
        Ok(())
    }
}
