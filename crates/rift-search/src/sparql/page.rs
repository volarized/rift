//! Exact retained-page requests identified from the library's parsed algebra.

use spargebra::Query;
use spargebra::algebra::QueryExpression;
use spargebra::term::{NamedNodePattern, TermPattern, TermTemplate};

use rift_error::{RiftError, errors};
use rift_protocol::symbol_read::CapturedViewId;

use super::GraphQuery;

const RESULT_SERVICE_PREFIX: &str = "urn:rift:result:";

/// Builds the next retained-page request using library RDF term formatting.
///
/// # Errors
/// Returns invalid retained identity or projected variable names, or a boolean result.
pub fn continuation_query(
    answer: &rift_protocol::query::QueryAnswer,
    view: &rift_protocol::symbol_read::CapturedViewId,
    page: u64,
) -> Result<String, RiftError> {
    use oxrdf::{NamedNode, Variable, VariableRef};
    let service = NamedNode::new(format!(
        "{RESULT_SERVICE_PREFIX}{}?page={page}",
        view.as_str()
    ))
    .map_err(invalid)?;
    match answer {
        rift_protocol::query::QueryAnswer::Bindings { variables, .. } => {
            let variables = variables
                .iter()
                .map(|name| VariableRef::new(name.as_str()).map(|variable| variable.to_string()))
                .collect::<Result<Vec<_>, _>>()
                .map_err(invalid)?;
            let projected = if variables.is_empty() {
                "*".to_owned()
            } else {
                variables.join(" ")
            };
            Ok(format!("SELECT {projected} {{ SERVICE {service} {{}} }}"))
        }
        rift_protocol::query::QueryAnswer::Graph { .. } => {
            let variables = ["s", "p", "o"]
                .into_iter()
                .map(Variable::new)
                .collect::<Result<Vec<_>, _>>()
                .map_err(invalid)?;
            let triple = variables
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(" ");
            Ok(format!(
                "CONSTRUCT {{ {triple} }} WHERE {{ SERVICE {service} {{ {triple} }} }}"
            ))
        }
        rift_protocol::query::QueryAnswer::Boolean { .. } => {
            Err(invalid("boolean result has no continuation"))
        }
    }
}

/// Result form requested by an exact retained-page projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PageForm {
    /// Original projected variables, in their requested order.
    Bindings {
        /// Projected variable names, in the original order.
        variables: Vec<String>,
    },
    /// Original RDF graph triples without another graph transformation.
    Graph,
}

/// One exact retained-page request whose owner still validates its original result form.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PageSelection {
    /// Retained view identity, without the page selector.
    pub view: CapturedViewId,
    /// Zero-based page in the original execution.
    pub page: u64,
    /// Requested result form and projection.
    pub form: PageForm,
}

impl GraphQuery {
    /// Recognizes an exact page projection without interpreting SPARQL text.
    ///
    /// The serving owner must compare this projection with the retained execution.
    /// Other queries keep ordinary library evaluation over the selected page service.
    #[must_use]
    pub fn page_selection(&self) -> Option<PageSelection> {
        selection(self.query())
    }
}

pub(super) fn selection(query: &Query) -> Option<PageSelection> {
    if query.dataset().is_some() {
        return None;
    }
    let (pattern, form) = match query {
        Query::Select(spargebra::query::SelectQuery {
            expression: QueryExpression::Project { inner, variables },
            ..
        }) => (
            inner.as_ref(),
            PageForm::Bindings {
                variables: variables
                    .iter()
                    .map(|variable| variable.as_str().to_owned())
                    .collect(),
            },
        ),
        Query::Construct(spargebra::query::ConstructQuery {
            template,
            expression: pattern,
            ..
        }) if template.len() == 1 => {
            let triple = &template[0];
            if !matches!(&triple.subject, TermTemplate::Variable(variable) if variable.as_str() == "s")
                || !matches!(&triple.predicate, NamedNodePattern::Variable(variable) if variable.as_str() == "p")
                || !matches!(&triple.object, TermTemplate::Variable(variable) if variable.as_str() == "o")
            {
                return None;
            }
            let pattern = match pattern {
                QueryExpression::Project { inner, variables } => {
                    if variables.len() != 3
                        || !["s", "p", "o"]
                            .iter()
                            .all(|name| variables.iter().any(|variable| variable.as_str() == *name))
                    {
                        return None;
                    }
                    inner.as_ref()
                }
                pattern => pattern,
            };
            (pattern, PageForm::Graph)
        }
        _ => return None,
    };
    let QueryExpression::Service {
        name: NamedNodePattern::NamedNode(name),
        inner,
        silent: false,
    } = pattern
    else {
        return None;
    };
    let QueryExpression::Bgp { patterns } = inner.as_ref() else {
        return None;
    };
    let passthrough = match &form {
        PageForm::Bindings { .. } => patterns.is_empty(),
        PageForm::Graph => {
            patterns.len() == 1
                && matches!(&patterns[0].subject, TermPattern::Variable(variable) if variable.as_str() == "s")
                && matches!(&patterns[0].predicate, NamedNodePattern::Variable(variable) if variable.as_str() == "p")
                && matches!(&patterns[0].object, TermPattern::Variable(variable) if variable.as_str() == "o")
        }
    };
    if !passthrough {
        return None;
    }
    let suffix = name.as_str().strip_prefix(RESULT_SERVICE_PREFIX)?;
    let (view, page) = suffix.rsplit_once("?page=")?;
    if view.is_empty()
        || view.contains('?')
        || page.is_empty()
        || !page.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    Some(PageSelection {
        view: CapturedViewId::parse(view).ok()?,
        page: page.parse().ok()?,
        form,
    })
}

fn invalid(error: impl std::fmt::Display) -> RiftError {
    errors::search::query_syntax_invalid()
        .detail(error.to_string())
        .error()
}
