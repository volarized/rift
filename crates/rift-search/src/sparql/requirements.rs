//! Projection proofs for the facts a graph query needs.

use oxrdf::vocab::rdf;
use rift_protocol::read::{RelationshipFacet, SymbolId, TraversalDirection};
use spargebra::Query;
use spargebra::algebra::{Expression, OrderExpression, PropertyPathExpression, QueryExpression};
use spargebra::term::{NamedNodePattern, TermPattern, TriplePattern};

use super::{GraphQuery, VOCABULARY_NAMESPACE};

/// The symbol facts needed by a query that does not read other fact families.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SymbolSelection<'query> {
    /// All captured logical symbols.
    All,
    /// Captured symbols with this exact name.
    Named(&'query str),
}

/// The complete adjacency selected by an anchored query.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdjacencySelection {
    /// Canonical start of the walk.
    pub seed: SymbolId,
    /// Required adjacency relative to the seed.
    pub direction: TraversalDirection,
    /// Required relationship facet.
    pub facet: RelationshipFacet,
    /// Maximum number of hops, or a walk to exhaustion.
    pub depth: Option<u32>,
    /// Whether physical usage records must also be projected.
    pub usages: bool,
    /// Whether the physical occurrences require exact source bytes.
    pub source: bool,
}

impl GraphQuery {
    /// Proves the parsed expression can be evaluated using symbol facts alone.
    ///
    /// An expression that may inspect other facts requires the full acquisition path.
    #[must_use]
    pub fn symbol_selection(&self) -> Option<SymbolSelection<'_>> {
        let QueryExpression::Bgp { patterns } = query_pattern(self.query())? else {
            return None;
        };
        symbol_patterns(patterns)
    }

    /// Finds a statically anchored path or physical usage selection.
    #[must_use]
    pub fn adjacency_selection(&self) -> Option<AdjacencySelection> {
        let pattern = query_pattern(self.query())?;
        if let QueryExpression::Path {
            subject,
            path,
            object,
        } = pattern
        {
            let (facet, reversed, depth) = uniform_path(path)?;
            return adjacency(subject, object, facet, reversed, depth);
        }
        let mut expressions = vec![pattern];
        let mut triples = Vec::new();
        let mut locations = Vec::new();
        while let Some(expression) = expressions.pop() {
            match expression {
                QueryExpression::Bgp { patterns } => triples.extend(patterns),
                QueryExpression::Join { left, right } => {
                    expressions.extend([left.as_ref(), right.as_ref()])
                }
                QueryExpression::Path { subject, path, .. } => locations.push((subject, path)),
                _ => return None,
            }
        }
        if let Some(usage) = usage_selection(&triples, &locations) {
            return Some(usage);
        }
        if !locations.is_empty() {
            return None;
        }
        adjacency_patterns(&triples)
    }
}

fn query_pattern(query: &Query) -> Option<&QueryExpression> {
    let mut expression = match query {
        Query::Select(query) => &query.expression,
        Query::Ask(query) => &query.expression,
        Query::Construct(query) => &query.expression,
        Query::Describe(_) => return None,
    };
    loop {
        expression = match expression {
            QueryExpression::Project { inner, .. }
            | QueryExpression::Distinct { inner }
            | QueryExpression::Reduced { inner }
            | QueryExpression::Slice { inner, .. } => inner,
            QueryExpression::OrderBy { inner, expression }
                if expression.iter().all(|item| {
                    matches!(
                        item,
                        OrderExpression::Asc(Expression::Variable(_))
                            | OrderExpression::Desc(Expression::Variable(_))
                    )
                }) =>
            {
                inner
            }
            _ => return Some(expression),
        };
    }
}

fn property(pattern: &TriplePattern) -> Option<&str> {
    let NamedNodePattern::NamedNode(predicate) = &pattern.predicate else {
        return None;
    };
    predicate.as_str().strip_prefix(VOCABULARY_NAMESPACE)
}

fn facet(name: &str) -> Option<RelationshipFacet> {
    match name {
        "calls" => Some(RelationshipFacet::Calls),
        "references" => Some(RelationshipFacet::References),
        _ => None,
    }
}

fn symbol(term: &TermPattern) -> Option<SymbolId> {
    let TermPattern::NamedNode(name) = term else {
        return None;
    };
    SymbolId::parse(name.as_str()).ok()
}

fn adjacency(
    subject: &TermPattern,
    object: &TermPattern,
    facet: RelationshipFacet,
    reversed: bool,
    depth: Option<u32>,
) -> Option<AdjacencySelection> {
    let (source, target) = if reversed {
        (object, subject)
    } else {
        (subject, object)
    };
    let incoming = symbol(target).map(|seed| (seed, TraversalDirection::Incoming));
    let outgoing = symbol(source).map(|seed| (seed, TraversalDirection::Outgoing));
    let (seed, direction) = match facet {
        RelationshipFacet::References => incoming.or(outgoing)?,
        _ => outgoing.or(incoming)?,
    };
    Some(AdjacencySelection {
        seed,
        direction,
        facet,
        depth,
        usages: false,
        source: false,
    })
}

fn adjacency_patterns(triples: &[&TriplePattern]) -> Option<AdjacencySelection> {
    let first = triples.first()?;
    let facet = property(first).and_then(facet)?;
    if triples
        .iter()
        .any(|edge| property(edge).and_then(self::facet) != Some(facet))
    {
        return None;
    }
    let directions = match facet {
        RelationshipFacet::References => {
            [TraversalDirection::Incoming, TraversalDirection::Outgoing]
        }
        _ => [TraversalDirection::Outgoing, TraversalDirection::Incoming],
    };
    let depth = u32::try_from(triples.len()).ok()?;
    for direction in directions {
        fn subject(edge: &TriplePattern) -> &TermPattern {
            &edge.subject
        }
        fn object(edge: &TriplePattern) -> &TermPattern {
            &edge.object
        }
        type Endpoint = fn(&TriplePattern) -> &TermPattern;
        let (start, end): (Endpoint, Endpoint) = match direction {
            TraversalDirection::Outgoing => (subject, object),
            TraversalDirection::Incoming => (object, subject),
        };
        for edge in triples {
            let mut current = start(edge);
            let Some(seed) = symbol(current) else {
                continue;
            };
            let mut remaining = triples.to_vec();
            for _ in 0..triples.len() {
                let mut next = remaining
                    .iter()
                    .enumerate()
                    .filter(|(_, edge)| start(edge) == current);
                let Some((index, edge)) = next.next() else {
                    break;
                };
                if next.next().is_some() {
                    break;
                }
                current = end(edge);
                remaining.swap_remove(index);
            }
            if remaining.is_empty() {
                return Some(AdjacencySelection {
                    seed,
                    direction,
                    facet,
                    depth: Some(depth),
                    usages: false,
                    source: false,
                });
            }
        }
    }
    None
}

fn usage_selection(
    triples: &[&TriplePattern],
    locations: &[(&TermPattern, &PropertyPathExpression)],
) -> Option<AdjacencySelection> {
    let usage = triples
        .iter()
        .find(|triple| property(triple) == Some("usages"))?;
    let seed = symbol(&usage.subject)?;
    let ranges: Vec<_> = triples
        .iter()
        .filter(|triple| triple.subject == usage.object && property(triple) == Some("range"))
        .map(|triple| &triple.object)
        .collect();
    let mut source = false;
    for &triple in triples {
        match property(triple) {
            Some("usages") if triple == *usage => {}
            Some("source") if triple.subject == usage.object => source = true,
            Some("of" | "range" | "kind") if triple.subject == usage.object => {}
            Some("unit" | "start" | "end") if ranges.contains(&&triple.subject) => {}
            _ if is_occurrence_class(triple) && triple.subject == usage.object => {}
            _ => return None,
        }
    }
    for &(subject, path) in locations {
        let PropertyPathExpression::Seq(range, field) = path else {
            return None;
        };
        let (PropertyPathExpression::Link(range), PropertyPathExpression::Link(field)) =
            (range.as_ref(), field.as_ref())
        else {
            return None;
        };
        if subject != &usage.object
            || range.as_str().strip_prefix(VOCABULARY_NAMESPACE) != Some("range")
            || !matches!(
                field.as_str().strip_prefix(VOCABULARY_NAMESPACE),
                Some("start" | "end" | "unit")
            )
        {
            return None;
        }
    }
    Some(AdjacencySelection {
        seed,
        direction: TraversalDirection::Incoming,
        facet: RelationshipFacet::References,
        depth: Some(1),
        usages: true,
        source,
    })
}

fn is_occurrence_class(pattern: &TriplePattern) -> bool {
    matches!(&pattern.predicate, NamedNodePattern::NamedNode(predicate) if predicate.as_str() == rdf::TYPE.as_str())
        && matches!(&pattern.object, TermPattern::NamedNode(class) if class.as_str().strip_prefix(VOCABULARY_NAMESPACE) == Some("Occurrence"))
}

fn uniform_path(path: &PropertyPathExpression) -> Option<(RelationshipFacet, bool, Option<u32>)> {
    enum Work<'path> {
        Visit(&'path PropertyPathExpression, bool),
        Combine { sequence: bool },
        Closure,
    }
    let mut work = vec![Work::Visit(path, false)];
    let mut results: Vec<(RelationshipFacet, bool, Option<u32>)> = Vec::new();
    while let Some(next) = work.pop() {
        match next {
            Work::Visit(path, reversed) => match path {
                PropertyPathExpression::Link(name) => {
                    let facet = facet(name.as_str().strip_prefix(VOCABULARY_NAMESPACE)?)?;
                    results.push((facet, reversed, Some(1)));
                }
                PropertyPathExpression::Inv(path) => work.push(Work::Visit(path, !reversed)),
                PropertyPathExpression::Seq(left, right)
                | PropertyPathExpression::Alt(left, right) => {
                    work.extend([
                        Work::Combine {
                            sequence: matches!(path, PropertyPathExpression::Seq(..)),
                        },
                        Work::Visit(right, reversed),
                        Work::Visit(left, reversed),
                    ]);
                }
                PropertyPathExpression::ZeroOrMorePath(path)
                | PropertyPathExpression::OneOrMorePath(path) => {
                    work.extend([Work::Closure, Work::Visit(path, reversed)]);
                }
                PropertyPathExpression::ZeroOrOnePath(path) => {
                    work.push(Work::Visit(path, reversed))
                }
                PropertyPathExpression::Nps(_) => return None,
            },
            Work::Closure => results.last_mut()?.2 = None,
            Work::Combine { sequence } => {
                let (right, right_reversed, right_depth) = results.pop()?;
                let (left, left_reversed, left_depth) = results.pop()?;
                if left != right || left_reversed != right_reversed {
                    return None;
                }
                let depth = match (left_depth, right_depth) {
                    (Some(left), Some(right)) if sequence => Some(left.checked_add(right)?),
                    (Some(left), Some(right)) => Some(left.max(right)),
                    _ => None,
                };
                results.push((left, left_reversed, depth));
            }
        }
    }
    results.pop()
}

fn symbol_patterns(patterns: &[TriplePattern]) -> Option<SymbolSelection<'_>> {
    let mut name = None;
    let single_subject = patterns.first().is_some_and(|first| {
        patterns
            .iter()
            .all(|pattern| pattern.subject == first.subject)
    });
    for pattern in patterns {
        let NamedNodePattern::NamedNode(predicate) = &pattern.predicate else {
            return None;
        };
        match predicate.as_str().strip_prefix(VOCABULARY_NAMESPACE) {
            Some("name") => {
                if let TermPattern::Literal(value) = &pattern.object {
                    name = Some(value.value());
                }
            }
            Some("revision") => {}
            Some("kind" | "language")
                if patterns
                    .iter()
                    .any(|other| other.subject == pattern.subject && is_symbol_class(other)) => {}
            _ if is_symbol_class(pattern) => {}
            _ => return None,
        }
    }
    Some(match name.filter(|_| single_subject) {
        Some(name) => SymbolSelection::Named(name),
        None => SymbolSelection::All,
    })
}

fn is_symbol_class(pattern: &TriplePattern) -> bool {
    matches!(&pattern.predicate, NamedNodePattern::NamedNode(predicate) if predicate.as_str() == rdf::TYPE.as_str())
        && matches!(&pattern.object, TermPattern::NamedNode(class) if class.as_str().strip_prefix(VOCABULARY_NAMESPACE) == Some("Symbol"))
}
