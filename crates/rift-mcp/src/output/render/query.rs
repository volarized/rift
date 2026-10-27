use rift_protocol::query::{QueryAnswer, QueryReadResult};

use super::super::text::{TextError, TextWriter};
use super::Render;

impl Render for QueryReadResult {
    fn render(&self, out: &mut TextWriter) -> Result<(), TextError> {
        let result = match self {
            Self::Results(result) => result,
            Self::Unavailable(reason) => return reason.render(out),
        };
        match &result.answer {
            QueryAnswer::Bindings {
                variables,
                bindings,
            } => {
                out.raw_line(
                    0,
                    &format!(
                        "{} results · page {}/{}",
                        bindings.len(),
                        result.pagination.page.page_index + 1,
                        result.pagination.page.total_pages
                    ),
                )?;
                out.raw_line(1, &variables.join(" "))?;
                for row in bindings {
                    out.raw_line(
                        1,
                        &serde_json::to_string(row)
                            .map_err(|error| TextError::Custom(error.to_string()))?,
                    )?;
                }
            }
            QueryAnswer::Boolean { value } => out.raw_line(0, &value.to_string())?,
            QueryAnswer::Graph { triples } => {
                out.raw_line(
                    0,
                    &format!(
                        "{} triples · page {}/{}",
                        triples.len(),
                        result.pagination.page.page_index + 1,
                        result.pagination.page.total_pages
                    ),
                )?;
                for triple in triples {
                    out.raw_line(
                        1,
                        &serde_json::to_string(triple)
                            .map_err(|error| TextError::Custom(error.to_string()))?,
                    )?;
                }
            }
        }
        if let Some(query) = &result.pagination.next_query {
            out.raw_line(
                0,
                &format!(
                    "next_query {}",
                    serde_json::to_string(query)
                        .map_err(|error| TextError::Custom(error.to_string()))?
                ),
            )?;
        }
        if let Some(view) = &result.view {
            out.raw_line(
                0,
                &format!("view {} · expires {}", view.id.as_str(), view.expires_at),
            )?;
        }
        super::warning::section(out, &result.warnings)
    }
}

#[cfg(test)]
mod tests {
    use rift_protocol::query::{QueryAnswer, QueryPagination, QueryReadResult, QueryResult};
    use rift_protocol::read::Pagination;
    use rift_protocol::search_read::SearchUnavailable;
    use rift_protocol::symbol_read::{CapturedView, CapturedViewId, SymbolUnavailableReason};

    use super::super::text_of;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn query_text_preserves_every_result_form_terms_and_exact_continuation() -> TestResult {
        let next = "SELECT ?value\n{ SERVICE <urn:rift:result:selected> { } } # \"\\\t";
        let view = CapturedView {
            id: CapturedViewId::parse(&"a".repeat(64))?,
            expires_at: "2099-01-01T00:00:00Z".to_owned(),
        };
        for answer in [
            serde_json::json!({"result":"bindings","variables":["iri","blank","literal","language","unbound"],"bindings":[{
                "iri":{"type":"iri","value":"urn:example:value"},
                "blank":{"type":"blank_node","value":"same"},
                "literal":{"type":"literal","value":"a\tb\n","datatype":"http://www.w3.org/2001/XMLSchema#string"},
                "language":{"type":"language_literal","value":"bonjour","language":"fr"}
            }]}),
            serde_json::json!({"result":"boolean","value":false}),
            serde_json::json!({"result":"graph","triples":[{
                "subject":{"type":"blank_node","value":"same"},
                "predicate":{"type":"iri","value":"urn:example:name"},
                "object":{"type":"literal","value":"a\tb\n","datatype":"http://www.w3.org/2001/XMLSchema#string"}
            }]}),
        ] {
            let answer: QueryAnswer = serde_json::from_value(answer)?;
            let result = QueryReadResult::Results(Box::new(QueryResult {
                answer,
                view: Some(view.clone()),
                warnings: Vec::new(),
                pagination: QueryPagination {
                    page: Pagination {
                        page_index: 0,
                        total_pages: 2,
                    },
                    next_query: Some(next.to_owned()),
                },
            }));
            let text = text_of(&result)?;
            let encoded = text
                .lines()
                .find_map(|line| line.strip_prefix("next_query "))
                .ok_or("text continuation missing")?;
            assert_eq!(serde_json::from_str::<String>(encoded)?, next);
            assert!(text.contains(view.id.as_str()));
            assert!(text.contains(&view.expires_at));
        }
        Ok(())
    }

    #[test]
    fn unavailable_query_text_has_no_continuation() -> TestResult {
        let result = QueryReadResult::Unavailable(SearchUnavailable::Unavailable {
            reason: SymbolUnavailableReason::ViewExpired,
            view: None,
            warnings: Vec::new(),
        });
        let text = text_of(&result)?;
        assert!(text.contains("unavailable"));
        assert!(!text.contains("next_query"));
        Ok(())
    }
}
