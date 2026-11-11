//! Compact text for the RDF terms and result forms of one query page.

use rift_protocol::query::{QueryAnswer, QueryReadResult, QueryResult, RdfTerm};

use super::{Render, TextError, TextWriter, warning};

impl Render for QueryReadResult {
    fn render(&self, out: &mut TextWriter) -> Result<(), TextError> {
        match self {
            Self::Results(result) => result.render(out),
            Self::Unavailable(reason) => reason.render(out),
        }
    }
}

impl Render for QueryResult {
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
        warning::section(out, &self.warnings)?;
        if let Some(view) = &self.view {
            out.raw_line(
                0,
                &format!("view {} · expires {}", view.id.as_str(), view.expires_at),
            )?;
        }
        Ok(())
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
