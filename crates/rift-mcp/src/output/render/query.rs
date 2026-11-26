//! Compact text for retrieved code, call paths, and RDF terms.

use std::collections::{BTreeMap, BTreeSet};

use rift_protocol::identity::SymbolIdentity;
use rift_protocol::query::{QueryAnswer, QueryReadResult, QueryResult, RdfTerm, RdfTriple};
use rift_protocol::read::ReadWarning;
use rift_protocol::search_read::SearchUnavailable;
use rift_search::sparql::{QueryField, QueryPresentation, VOCABULARY_NAMESPACE};
use rmcp::model::MetaObject;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Render, TextError, TextWriter};
use crate::output::text::visible;

const BLANK_PREFIX_MIN: usize = 12;
const FIELD_SEPARATOR: &str = " | ";
const SCORE_DECIMAL_BYTES_MAX: usize = 32;
const PRESENTATION_KEY: &str = "rift/query-presentation";
const XSD_NAMESPACE: &str = "http://www.w3.org/2001/XMLSchema#";
const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
const XSD_DOUBLE: &str = "http://www.w3.org/2001/XMLSchema#double";
const XSD_BOOLEAN: &str = "http://www.w3.org/2001/XMLSchema#boolean";
const RDF_NAMESPACE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const RDFS_NAMESPACE: &str = "http://www.w3.org/2000/01/rdf-schema#";

type Row = BTreeMap<String, RdfTerm>;
type Fields = BTreeMap<QueryField, String>;

/// Query text choices retained with the evaluated rows, before they split into pages.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct QueryDisplay {
    presentation: QueryPresentation,
    blank_prefix: usize,
    multiple_flows: bool,
}

impl QueryDisplay {
    pub(crate) fn selected(answer: &QueryAnswer, presentation: QueryPresentation) -> Self {
        let multiple_flows = match (answer, &presentation) {
            (QueryAnswer::Bindings { bindings, .. }, QueryPresentation::Flows(fields)) => {
                let mut ids = bindings
                    .iter()
                    .filter_map(|row| field(row, fields, QueryField::Flow));
                ids.next()
                    .is_some_and(|first| ids.any(|other| other != first))
            }
            _ => false,
        };
        Self {
            presentation,
            blank_prefix: blank_prefix(answer),
            multiple_flows,
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

    /// Recovers the original projection when the proxy qualifies an extra project's result.
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
        match &mut self.result {
            QueryReadResult::Results(result) => {
                crate::output::log_warnings(&result.warnings);
                result
                    .warnings
                    .retain(|warning| query_warning_text(warning).is_some());
            }
            QueryReadResult::Unavailable(SearchUnavailable::Unavailable {
                reason,
                warnings,
                ..
            }) => {
                crate::output::log_warnings(warnings);
                warnings.retain(|warning| {
                    query_warning_text(warning).is_some()
                        || super::unavailable::reports(*reason, warning)
                });
            }
        }
    }

    fn render(&self, out: &mut TextWriter) -> Result<(), TextError> {
        match &self.result {
            QueryReadResult::Results(result) => render_result(out, result, &self.display),
            QueryReadResult::Unavailable(SearchUnavailable::Unavailable {
                reason,
                warnings,
                ..
            }) => {
                out.raw_line(0, &super::unavailable::line(*reason, warnings))?;
                write_query_warnings(out, warnings)
            }
        }
    }
}

fn render_result(
    out: &mut TextWriter,
    result: &QueryResult,
    display: &QueryDisplay,
) -> Result<(), TextError> {
    let page = result.pagination.as_ref();
    let terms = Terms {
        blank_prefix: display.blank_prefix.max(BLANK_PREFIX_MIN),
    };
    match &result.answer {
        QueryAnswer::Boolean { value } => {
            out.raw_line(0, if *value { "true" } else { "false" })?;
        }
        QueryAnswer::Bindings {
            variables,
            bindings,
        } => match &display.presentation {
            QueryPresentation::Matches(fields) => {
                matches(out, &terms, variables, bindings, fields)?;
            }
            QueryPresentation::Flows(fields) => {
                let page = FlowPage {
                    multiple_flows: display.multiple_flows,
                    linked_pages: page.is_some_and(|page| page.pages > 1),
                };
                flows(out, &terms, variables, bindings, fields, page)?;
            }
            QueryPresentation::Raw => bindings_table(out, &terms, variables, bindings)?,
        },
        QueryAnswer::Graph { triples } => {
            graph(
                out,
                &terms,
                triples,
                page.is_none_or(|page| page.pages == 1),
            )?;
        }
    }
    if let Some(page) = page.and_then(super::layout::page_fact) {
        out.raw_line(0, &page)?;
    }
    write_query_warnings(out, &result.warnings)
}

fn graph(
    out: &mut TextWriter,
    terms: &Terms,
    triples: &[RdfTriple],
    complete: bool,
) -> Result<(), TextError> {
    let Some(first) = triples.first() else {
        return out.raw_line(0, "No triples.");
    };
    // An isolated blank subject needs no label when its entire graph is in this answer.
    if complete
        && matches!(&first.subject, RdfTerm::BlankNode { providers, .. } if providers.is_empty())
        && triples
            .iter()
            .all(|triple| triple.subject == first.subject && triple.object != first.subject)
    {
        let properties = triples
            .iter()
            .map(|triple| {
                Ok(format!(
                    "{} {}",
                    terms.term(&triple.predicate)?,
                    terms.term(&triple.object)?
                ))
            })
            .collect::<Result<Vec<_>, TextError>>()?;
        return out.raw_line(0, &format!("[ {} ] .", properties.join(" ; ")));
    }
    for triple in triples {
        out.raw_line(
            0,
            &format!(
                "{} {} {} .",
                terms.term(&triple.subject)?,
                terms.term(&triple.predicate)?,
                terms.term(&triple.object)?
            ),
        )?;
    }
    Ok(())
}

fn bindings_table(
    out: &mut TextWriter,
    terms: &Terms,
    variables: &[String],
    bindings: &[Row],
) -> Result<(), TextError> {
    let header = variables
        .iter()
        .map(|variable| format!("?{variable}"))
        .collect::<Vec<_>>();
    out.raw_line(0, &header.join("\t"))?;
    for row in bindings {
        let cells = variables
            .iter()
            .map(|variable| {
                row.get(variable)
                    .map_or_else(|| Ok(String::new()), |value| terms.term(value))
            })
            .collect::<Result<Vec<_>, _>>()?;
        out.raw_line(0, &cells.join("\t"))?;
    }
    Ok(())
}

fn matches(
    out: &mut TextWriter,
    terms: &Terms,
    variables: &[String],
    bindings: &[Row],
    fields: &Fields,
) -> Result<(), TextError> {
    use QueryField::{End, Line, Name, Path, Score, Source, Start, Symbol};
    if bindings.is_empty() {
        return out.raw_line(0, "No matches.");
    }
    for row in bindings {
        let mut title = Vec::new();
        let name = field(row, fields, Name);
        let symbol = field(row, fields, Symbol);
        if let Some(name) = name {
            title.push(terms.label(name)?);
        } else if let Some(symbol) = symbol {
            title.push(symbol_name(symbol, terms)?);
        }
        if let Some(path) = field(row, fields, Path) {
            let path = terms.label(path)?;
            let location = if let Some(line) = field(row, fields, Line) {
                format!("{path}:{}", terms.term(line)?)
            } else {
                path
            };
            title.push(location);
        } else if let Some(line) = field(row, fields, Line) {
            title.push(format!("line {}", terms.term(line)?));
        }
        range(
            &mut title,
            terms,
            field(row, fields, Start),
            field(row, fields, End),
        )?;
        if let Some(score) = field(row, fields, Score) {
            title.push(format!("score {}", terms.score(score)?));
        }
        title.retain(|field| !field.is_empty());
        if !title.is_empty() {
            out.raw_line(0, &title.join(FIELD_SEPARATOR))?;
        }
        if let Some(source) = field(row, fields, Source) {
            if let RdfTerm::Literal { value, datatype } = source
                && datatype == XSD_STRING
            {
                out.raw_lines(1, value)?;
            } else {
                out.raw_line(1, &terms.term(source)?)?;
            }
        }
        if let Some(symbol) = symbol {
            out.raw_line(1, &terms.label(symbol)?)?;
        }
        other_fields(out, terms, variables, row, fields)?;
    }
    Ok(())
}

fn range(
    title: &mut Vec<String>,
    terms: &Terms,
    start: Option<&RdfTerm>,
    end: Option<&RdfTerm>,
) -> Result<(), TextError> {
    match (start, end) {
        (Some(start), Some(end)) => title.push(format!(
            "bytes {}..{}",
            terms.term(start)?,
            terms.term(end)?
        )),
        (Some(start), None) => title.push(format!("start byte {}", terms.term(start)?)),
        (None, Some(end)) => title.push(format!("end byte {}", terms.term(end)?)),
        (None, None) => {}
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct FlowPage {
    multiple_flows: bool,
    linked_pages: bool,
}

fn flows(
    out: &mut TextWriter,
    terms: &Terms,
    variables: &[String],
    bindings: &[Row],
    fields: &Fields,
    page: FlowPage,
) -> Result<(), TextError> {
    use QueryField::{Flow, Name, Score, Step, Symbol};
    if bindings.is_empty() {
        return out.raw_line(0, "No call paths.");
    }
    // Keep the query's row order. Step indexes stay absolute when a page splits a path.
    let mut previous = None;
    let mut path_numbers = BTreeMap::new();
    for row in bindings {
        let flow = field(row, fields, Flow);
        let score = field(row, fields, Score);
        let heading = (flow, score);
        if previous != Some(heading) {
            let mut title = vec![flow_title(flow, terms, page, &mut path_numbers)?];
            if let Some(score) = score {
                title.push(format!("score {}", terms.score(score)?));
            }
            out.raw_line(0, &title.join(FIELD_SEPARATOR))?;
        }
        let symbol = field(row, fields, Symbol);
        let name = match field(row, fields, Name) {
            Some(name) => terms.label(name)?,
            None => symbol.map_or_else(|| Ok(String::new()), |id| symbol_name(id, terms))?,
        };
        let mut step = match field(row, fields, Step) {
            Some(index) => format!("{} → {name}", terms.term(index)?),
            None => format!("→ {name}"),
        };
        if let Some(symbol) = symbol {
            step.push_str(FIELD_SEPARATOR);
            step.push_str(&terms.label(symbol)?);
        }
        out.raw_line(1, &step)?;
        other_fields(out, terms, variables, row, fields)?;
        previous = Some(heading);
    }
    Ok(())
}

fn flow_title<'a>(
    flow: Option<&'a RdfTerm>,
    terms: &Terms,
    page: FlowPage,
    path_numbers: &mut BTreeMap<&'a str, usize>,
) -> Result<String, TextError> {
    match flow {
        Some(RdfTerm::BlankNode { .. }) if !page.multiple_flows => Ok("Call path".to_owned()),
        Some(RdfTerm::BlankNode { value, .. }) if !page.linked_pages => {
            let next = path_numbers.len() + 1;
            let ordinal = path_numbers.entry(value).or_insert(next);
            Ok(format!("Call path {ordinal}"))
        }
        Some(flow) => Ok(format!("Call path {}", terms.label(flow)?)),
        None => Ok("Call path".to_owned()),
    }
}

fn field<'a>(row: &'a Row, fields: &Fields, field: QueryField) -> Option<&'a RdfTerm> {
    row.get(fields.get(&field)?)
}

fn symbol_name(symbol: &RdfTerm, terms: &Terms) -> Result<String, TextError> {
    if let RdfTerm::Iri { value } = symbol
        && let Ok(identity) = SymbolIdentity::parse(value)
        && let Some(name) = identity.qualified_path().last()
    {
        return Ok(visible(name).into_owned());
    }
    terms.label(symbol)
}

fn other_fields(
    out: &mut TextWriter,
    terms: &Terms,
    variables: &[String],
    row: &Row,
    fields: &Fields,
) -> Result<(), TextError> {
    for variable in variables
        .iter()
        .filter(|variable| !fields.values().any(|field| field == *variable))
    {
        if let Some(value) = row.get(variable) {
            out.raw_line(1, &format!("?{variable} = {}", terms.term(value)?))?;
        }
    }
    Ok(())
}

fn write_query_warnings(out: &mut TextWriter, warnings: &[ReadWarning]) -> Result<(), TextError> {
    for detail in warnings.iter().filter_map(query_warning_text) {
        out.raw_line(0, &format!("warning · {detail}"))?;
    }
    if warnings
        .iter()
        .any(|warning| matches!(warning, ReadWarning::TraversalTruncated { .. }))
    {
        out.raw_line(
            0,
            "Narrow the flow query to search again. A page continues the retained result.",
        )?;
    }
    Ok(())
}

/// The text of a warning a query answer carries: the coverage and discovery conditions that
/// name what is missing from it. Every other warning is logged and left out of the answer.
fn query_warning_text(warning: &ReadWarning) -> Option<String> {
    match warning {
        ReadWarning::LocalSymbolInventoryUnavailable => Some(
            "The local symbol inventory is unavailable. Use `SERVICE :search` to select package facts."
                .to_owned(),
        ),
        ReadWarning::EdgeCoverageMissing { detail, .. }
        | ReadWarning::TraversalTruncated { detail, .. }
        | ReadWarning::EngineAnalysisUnavailable { detail, .. }
        | ReadWarning::EngineReadinessUnconfirmed { detail, .. }
        | ReadWarning::CalleesDropped { detail, .. }
        | ReadWarning::SourceUnavailable { detail, .. }
        | ReadWarning::LargeFileUnparsed { detail, .. } => Some(detail.clone()),
        ReadWarning::FrameworkContextUnresolved { unit, detail, .. } => Some(
            unit.project_path()
                .map_or_else(|| detail.clone(), |path| format!("{path}: {detail}")),
        ),
        _ => None,
    }
}

struct Terms {
    blank_prefix: usize,
}

impl Terms {
    fn label(&self, term: &RdfTerm) -> Result<String, TextError> {
        match term {
            RdfTerm::Iri { value } => Ok(visible(value).into_owned()),
            RdfTerm::Literal { value, datatype } if datatype == XSD_STRING => {
                Ok(visible(value).into_owned())
            }
            _ => self.term(term),
        }
    }

    fn score(&self, term: &RdfTerm) -> Result<String, TextError> {
        match term {
            RdfTerm::Literal { value, datatype } if datatype == XSD_DOUBLE => {
                let score = value
                    .parse::<f64>()
                    .ok()
                    .filter(|score| score.is_finite())
                    .map(|score| score.to_string())
                    .filter(|decimal| decimal.len() <= SCORE_DECIMAL_BYTES_MAX);
                Ok(score.unwrap_or_else(|| visible(value).into_owned()))
            }
            _ => self.term(term),
        }
    }

    fn term(&self, term: &RdfTerm) -> Result<String, TextError> {
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
            RdfTerm::Literal { value, datatype } if datatype == XSD_STRING => quoted(value),
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
            "pagination": {"page": 1, "pages": 1, "max_results": 10, "total": 10},
            "view": { "id": "retained.answer", "expires_at": "2030-01-01T00:15:00Z" }
        }))?;
        assert_eq!(
            output,
            "?blank\t?value\t?typed\t?predicate\n_:first-retain\t42\t\"invalid boolean\"^^xsd:boolean\t:name\n_:different-re\ttrue\t\"source\"@en\t\n_:first-retain\t\t\t\n"
        );
        Ok(())
    }

    #[test]
    fn graph_text_omits_only_an_isolated_blank_subject_label()
    -> Result<(), Box<dyn std::error::Error>> {
        let query = |triples, pages| {
            raw_answer(serde_json::json!({
                "answer": { "result": "graph", "triples": triples },
                "pagination": { "page": 1, "pages": pages, "max_results": 1, "total": pages }
            }))
        };
        let blank = serde_json::json!({ "type": "blank_node", "value": "anonymous" });
        let predicate = serde_json::json!({ "type": "iri", "value": "urn:rift:vocabulary:name" });
        let triple = serde_json::json!({
            "subject": blank,
            "predicate": predicate,
            "object": { "type": "literal", "value": "Rift", "datatype": XSD_STRING }
        });
        let mut produced = triple.clone();
        produced["subject"]["providers"] = serde_json::json!(["static", "lsp", "engine"]);
        assert_eq!(
            query(vec![produced], 1)?,
            "_:anonymous (providers: engine, lsp, static) :name \"Rift\" .\n"
        );
        let count = serde_json::json!({
            "subject": blank,
            "predicate": { "type": "iri", "value": "urn:rift:vocabulary:count" },
            "object": { "type": "literal", "value": "7", "datatype": XSD_INTEGER }
        });
        assert_eq!(
            query(vec![triple.clone(), count.clone()], 1)?,
            "[ :name \"Rift\" ; :count 7 ] .\n"
        );
        assert_eq!(
            query(vec![triple.clone(), count], 2)?,
            "_:anonymous :name \"Rift\" .\n_:anonymous :count 7 .\npage 1/2, total 2 results\n"
        );
        let reference = serde_json::json!({
            "subject": { "type": "blank_node", "value": "other" },
            "predicate": { "type": "iri", "value": "urn:rift:vocabulary:ref" },
            "object": blank
        });
        assert_eq!(
            query(vec![triple, reference], 1)?,
            "_:anonymous :name \"Rift\" .\n_:other :ref _:anonymous .\n"
        );
        let self_reference = serde_json::json!({
            "subject": blank,
            "predicate": { "type": "iri", "value": "urn:rift:vocabulary:ref" },
            "object": blank
        });
        assert_eq!(
            query(vec![self_reference], 1)?,
            "_:anonymous :ref _:anonymous .\n"
        );
        Ok(())
    }

    #[test]
    fn call_paths_render_indexes_as_arrows_and_label_only_distinct_paths()
    -> Result<(), Box<dyn std::error::Error>> {
        let presentation = QueryPresentation::Flows(BTreeMap::from([
            (QueryField::Flow, "flow".to_owned()),
            (QueryField::Score, "rank".to_owned()),
            (QueryField::Step, "index".to_owned()),
            (QueryField::Symbol, "symbol".to_owned()),
        ]));
        let row = |flow: &str, index: &str, symbol: &str| {
            BTreeMap::from([
                (
                    "flow".to_owned(),
                    RdfTerm::BlankNode {
                        value: flow.to_owned(),
                        providers: std::collections::BTreeSet::default(),
                    },
                ),
                (
                    "rank".to_owned(),
                    RdfTerm::Literal {
                        value: "1.0E0".to_owned(),
                        datatype: XSD_DOUBLE.to_owned(),
                    },
                ),
                (
                    "index".to_owned(),
                    RdfTerm::Literal {
                        value: index.to_owned(),
                        datatype: XSD_INTEGER.to_owned(),
                    },
                ),
                (
                    "symbol".to_owned(),
                    RdfTerm::Iri {
                        value: format!("rift://symbol/local/rust/app/{symbol}"),
                    },
                ),
            ])
        };
        let first = row("first", "0", "checkout");
        let second = row("second", "0", "charge");
        let later = row("first", "2", "retry");
        let answer = |bindings| QueryAnswer::Bindings {
            variables: ["flow", "rank", "index", "symbol"]
                .map(str::to_owned)
                .to_vec(),
            bindings,
        };
        let page = |bindings, retained, page: u64, pages: u64| {
            let result = QueryOutput::new(
                QueryReadResult::Results(Box::new(QueryResult {
                    answer: answer(bindings),
                    pagination: rift_protocol::read::Pagination {
                        page: page + 1,
                        pages,
                        max_results: 1,
                        total: pages,
                    }
                    .into(),
                    view: None,
                    revision: None,
                    warnings: Vec::new(),
                })),
                QueryDisplay::selected(&answer(retained), presentation.clone()),
            );
            super::super::text_of(&result)
        };
        assert_eq!(
            page(
                vec![later.clone()],
                vec![first.clone(), later.clone()],
                1,
                2
            )?,
            "Call path | score 1\n\t2 → retry | rift://symbol/local/rust/app/retry\npage 2/2, total 2 results\n"
        );
        let retained = vec![first.clone(), second, later.clone()];
        let multiple = page(retained.clone(), retained.clone(), 0, 1)?;
        assert_eq!(
            multiple
                .lines()
                .filter(|line| line.starts_with("Call path"))
                .collect::<Vec<_>>(),
            [
                "Call path 1 | score 1",
                "Call path 2 | score 1",
                "Call path 1 | score 1"
            ]
        );
        for (index, binding) in [(0, first), (2, later)] {
            let output = page(vec![binding], retained.clone(), index, 3)?;
            assert!(output.starts_with("Call path _:first | score 1\n"));
        }
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
        let display = QueryDisplay::selected(&full, QueryPresentation::Raw);
        let page = |page, value| {
            QueryOutput::new(
                QueryReadResult::Results(Box::new(QueryResult {
                    answer: QueryAnswer::Bindings {
                        variables: variables.clone(),
                        bindings: vec![row(value)],
                    },
                    pagination: rift_protocol::read::Pagination {
                        page,
                        pages: 3,
                        max_results: 10,
                        total: 0,
                    }
                    .into(),
                    view: None,
                    revision: None,
                    warnings: Vec::new(),
                })),
                display.clone(),
            )
        };
        for (index, value, expected) in [
            (0, first.clone(), "_:retained_node_á"),
            (1, second, "_:retained_node_ã"),
            (2, first, "_:retained_node_á"),
        ] {
            let output = super::super::text_of(&page(index, value))?;
            assert_eq!(output.lines().nth(1), Some(expected));
        }
        Ok(())
    }
}
