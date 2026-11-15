//! Compact text for the RDF terms and result forms of one query page.

use rift_protocol::query::{QueryAnswer, QueryReadResult, QueryResult, RdfTerm};
use rift_protocol::read::ReadWarning;
use rift_protocol::search_read::SearchUnavailable;

use super::{Render, TextError, TextWriter};

impl Render for QueryReadResult {
    fn log_warnings(&mut self) {
        match self {
            Self::Results(result) => result.log_warnings(),
            Self::Unavailable(SearchUnavailable::Unavailable { warnings, .. }) => {
                record_query_warnings(warnings);
            }
        }
    }

    fn render(&self, out: &mut TextWriter) -> Result<(), TextError> {
        match self {
            Self::Results(result) => result.render(out),
            Self::Unavailable(reason @ SearchUnavailable::Unavailable { warnings, .. }) => {
                reason.render(out)?;
                write_query_warnings(out, warnings)
            }
        }
    }
}

impl Render for QueryResult {
    fn log_warnings(&mut self) {
        record_query_warnings(&mut self.warnings);
    }

    fn render(&self, out: &mut TextWriter) -> Result<(), TextError> {
        match &self.answer {
            QueryAnswer::Boolean { value } => {
                out.raw_line(0, if *value { "true" } else { "false" })?;
            }
            QueryAnswer::Bindings {
                variables,
                bindings,
            } => {
                let columns = variables
                    .iter()
                    .map(|variable| format!("?{variable}"))
                    .collect::<Vec<_>>();
                out.raw_line(0, &columns.join("\t"))?;
                for row in bindings {
                    let columns = variables
                        .iter()
                        .map(|variable| row.get(variable).map(term).transpose())
                        .collect::<Result<Vec<_>, _>>()?;
                    let values = columns
                        .iter()
                        .map(|value| value.as_deref().unwrap_or_default())
                        .collect::<Vec<_>>();
                    out.raw_line(0, &values.join("\t"))?;
                }
            }
            QueryAnswer::Graph { triples } => {
                for triple in triples {
                    out.raw_line(
                        0,
                        &format!(
                            "{} {} {} .",
                            term(&triple.subject)?,
                            term(&triple.predicate)?,
                            term(&triple.object)?
                        ),
                    )?;
                }
            }
        }
        if self.pagination.page.total_pages > 1 {
            out.raw_line(
                0,
                &format!(
                    "page {}/{}",
                    self.pagination.page.page_index.saturating_add(1),
                    self.pagination.page.total_pages
                ),
            )?;
        }
        if let Some(next) = &self.pagination.next_query {
            out.raw_line(0, &format!("next query · {next}"))?;
        }
        if let Some(view) = &self.view {
            out.raw_line(
                0,
                &format!("view {} · expires {}", view.id.as_str(), view.expires_at),
            )?;
        }
        write_query_warnings(out, &self.warnings)
    }
}

fn record_query_warnings(warnings: &mut Vec<ReadWarning>) {
    crate::output::log_warnings(warnings);
    warnings.retain(|warning| query_warning_detail(warning).is_some());
}

fn write_query_warnings(out: &mut TextWriter, warnings: &[ReadWarning]) -> Result<(), TextError> {
    for detail in warnings.iter().filter_map(query_warning_detail) {
        out.raw_line(0, &format!("warning · {detail}"))?;
    }
    Ok(())
}

fn query_warning_detail(warning: &ReadWarning) -> Option<&str> {
    match warning {
        ReadWarning::RelationshipCoverageMissing { detail, .. }
        | ReadWarning::TraversalTruncated { detail, .. } => Some(detail),
        _ => None,
    }
}

fn term(term: &RdfTerm) -> Result<String, TextError> {
    const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
    let lexical = |value: &str| {
        serde_json::to_string(value).map_err(|error| TextError::Custom(error.to_string()))
    };
    match term {
        RdfTerm::Iri { value } => Ok(format!("<{value}>")),
        RdfTerm::BlankNode { value } => Ok(format!("_:{value}")),
        RdfTerm::Literal { value, datatype } if datatype == XSD_STRING => lexical(value),
        RdfTerm::Literal { value, datatype } => Ok(format!("{}^^<{datatype}>", lexical(value)?)),
        RdfTerm::LanguageLiteral { value, language } => {
            Ok(format!("{}@{language}", lexical(value)?))
        }
    }
}
