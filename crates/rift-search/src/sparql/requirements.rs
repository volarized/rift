//! Projection proofs for the facts a graph query needs.

use oxrdf::vocab::rdf;
use rift_protocol::read::{RelationshipFacet, SymbolId, TraversalDirection};
use spargebra::Query;
use spargebra::algebra::{Expression, PropertyPathExpression, QueryExpression};
use spargebra::term::{NamedNodePattern, TermPattern, TriplePattern};

use super::{GraphQuery, OccurrenceSelection, VOCABULARY_NAMESPACE};

/// The symbol facts needed by a query that does not read other fact families.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SymbolSelection<'query> {
    /// All captured logical symbols.
    All,
    /// Captured symbols with this exact name.
    Named(&'query str),
    /// One established canonical identity.
    Identified(&'query str),
}

/// Captured syntax nodes for the selected physical declarations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NodeSelection<'query> {
    /// The logical symbols whose declarations supply the nodes.
    pub symbols: SymbolSelection<'query>,
    /// Whether the pattern requests exact node source text.
    pub source: bool,
}

/// One direction and facet acquired at a path position.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct AdjacencyStep {
    /// Required adjacency relative to the current symbol.
    pub direction: TraversalDirection,
    /// Required relationship facet.
    pub facet: RelationshipFacet,
}

/// The declaration item facts required by a reached-symbol pattern.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReachedNodes {
    /// The query can inspect the exact declaration excerpt.
    pub source: bool,
    /// The query can inspect grammar facts or the exact node declaration ordinal.
    pub syntax_metadata: bool,
}

/// The complete adjacency selected by an anchored query.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdjacencySelection {
    /// Canonical start of the walk.
    pub seed: SymbolId,
    first_step: AdjacencyStep,
    steps: Vec<AdjacencyStep>,
    /// Maximum number of hops, or a walk to exhaustion.
    pub depth: Option<u32>,
    /// Whether the pattern projects logical facts about reached symbols.
    pub symbol_details: bool,
    /// Declaration item facts selected for reached symbols.
    pub nodes: Option<ReachedNodes>,
    /// Whether physical usage records and their evidence must also be projected.
    pub occurrences: Option<OccurrenceSelection>,
}

impl AdjacencySelection {
    /// The operation required at this path position.
    #[must_use]
    pub fn step(&self, depth: u32) -> AdjacencyStep {
        usize::try_from(depth)
            .ok()
            .and_then(|position| self.steps.get(position))
            .copied()
            .unwrap_or(self.first_step)
    }

    /// Whether an occurrence's enclosing symbol is required for the graph result.
    #[must_use]
    pub fn requires_enclosing(&self) -> bool {
        self.occurrences.is_none_or(|usage| usage.enclosing)
    }

    /// Whether path positions require different acquisition operations.
    #[must_use]
    pub fn has_distinct_steps(&self) -> bool {
        !self.steps.is_empty()
    }
}

impl GraphQuery {
    /// Proves the parsed expression can be evaluated using symbol facts alone.
    ///
    /// An expression that may inspect other facts requires the full acquisition path.
    #[must_use]
    pub fn symbol_selection(&self) -> Option<SymbolSelection<'_>> {
        if !self.reads_base() {
            return None;
        }
        if let Query::Describe(query) = self.query()
            && let QueryExpression::Extend {
                inner,
                expression: Expression::NamedNode(target),
                ..
            } = &query.pattern
            && matches!(inner.as_ref(), QueryExpression::Bgp { patterns } if patterns.is_empty())
            && rift_protocol::identity::SymbolIdentity::parse(target.as_str()).is_ok()
        {
            return Some(SymbolSelection::Identified(target.as_str()));
        }
        let selection = match query_pattern(self) {
            QueryExpression::Bgp { patterns } => symbol_patterns(patterns),
            _ => None,
        };
        selection.or_else(|| self.symbol_only().then_some(SymbolSelection::All))
    }

    /// Proves that a graph pattern needs only the syntax nodes of selected declarations.
    #[must_use]
    pub fn node_selection(&self) -> Option<NodeSelection<'_>> {
        let QueryExpression::Bgp { patterns } = query_pattern(self) else {
            return None;
        };
        let owner = patterns
            .iter()
            .find(|triple| property(triple) == Some("of"))?;
        if !patterns.iter().any(|triple| {
            triple.subject == owner.subject
                && (property(triple) == Some("index") || is_class(triple, "Node"))
        }) {
            return None;
        }
        let ranges = patterns
            .iter()
            .filter(|triple| triple.subject == owner.subject && property(triple) == Some("range"))
            .map(|triple| &triple.object)
            .collect::<Vec<_>>();
        let mut name = None;
        let mut source = false;
        for triple in patterns {
            let predicate = property(triple);
            if triple.subject == owner.subject {
                match predicate {
                    Some("of") if triple == owner => {}
                    Some("source") => source = true,
                    Some("range" | "index" | "declaration" | "kind" | "language") => {}
                    _ if is_class(triple, "Node") => {}
                    _ => return None,
                }
            } else if triple.subject == owner.object {
                match predicate {
                    Some("name") => {
                        if let TermPattern::Literal(value) = &triple.object {
                            name = Some(value.value());
                        }
                    }
                    Some("kind" | "language" | "revision") => {}
                    _ if is_symbol_class(triple) => {}
                    _ => return None,
                }
            } else if !ranges.contains(&&triple.subject)
                || !matches!(predicate, Some("unit" | "start" | "end"))
            {
                return None;
            }
        }
        let symbols = if let TermPattern::NamedNode(node) = &owner.object {
            rift_protocol::identity::SymbolIdentity::parse(node.as_str()).ok()?;
            SymbolSelection::Identified(node.as_str())
        } else if let Some(name) = name {
            SymbolSelection::Named(name)
        } else {
            SymbolSelection::All
        };
        Some(NodeSelection { symbols, source })
    }

    /// Finds a statically anchored path or physical usage selection.
    #[must_use]
    pub fn adjacency_selection(&self) -> Option<AdjacencySelection> {
        let pattern = query_pattern(self);
        if let QueryExpression::Path {
            subject,
            path,
            object,
        } = pattern
        {
            if let Some((facet, reversed, depth)) = uniform_path(path) {
                return adjacency(subject, object, facet, reversed, depth);
            }
            let mut steps = fixed_path(path)?;
            if let Some(seed) = symbol(subject) {
                return layered_adjacency(seed, steps);
            }
            steps.reverse();
            for step in &mut steps {
                step.direction = opposite(step.direction);
            }
            return layered_adjacency(symbol(object)?, steps);
        }
        let mut expressions = vec![pattern];
        let mut triples = Vec::new();
        let mut locations = Vec::new();
        while let Some(expression) = expressions.pop() {
            match expression {
                QueryExpression::Bgp { patterns } => triples.extend(patterns),
                QueryExpression::Join { left, right } => {
                    expressions.extend([left.as_ref(), right.as_ref()]);
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
        let (edges, details): (Vec<_>, Vec<_>) = triples
            .into_iter()
            .partition(|edge| property(edge).and_then(facet).is_some());
        let (symbol_details, nodes) = endpoint_details(&edges, &details)?;
        let mut selected = adjacency_patterns(&edges).or_else(|| layered_patterns(&edges))?;
        selected.symbol_details = symbol_details;
        selected.nodes = nodes;
        Some(selected)
    }
}

fn query_pattern(query: &GraphQuery) -> &QueryExpression {
    let row_expressions_only = query.admission().embedded_patterns == 0;
    let mut expression = match query.query() {
        Query::Select(query) => &query.expression,
        Query::Ask(query) => &query.expression,
        Query::Construct(query) => &query.expression,
        Query::Describe(query) => &query.pattern,
    };
    loop {
        expression = match expression {
            QueryExpression::Graph {
                name: NamedNodePattern::NamedNode(name),
                inner,
            } if name.as_str() == super::LOCAL_GRAPH => inner,
            QueryExpression::Project { inner, .. }
            | QueryExpression::Distinct { inner, .. }
            | QueryExpression::Reduced { inner, .. }
            | QueryExpression::Slice { inner, .. } => inner,
            QueryExpression::Filter { inner, .. }
            | QueryExpression::Extend { inner, .. }
            | QueryExpression::Group { inner, .. }
            | QueryExpression::OrderBy { inner, .. }
                if row_expressions_only =>
            {
                inner
            }
            _ => return expression,
        };
    }
}

fn endpoint_details(
    edges: &[&TriplePattern],
    details: &[&TriplePattern],
) -> Option<(bool, Option<ReachedNodes>)> {
    let endpoint = |term: &TermPattern| {
        edges
            .iter()
            .any(|edge| &edge.subject == term || &edge.object == term)
    };
    let mut node_owner = details
        .iter()
        .filter(|detail| property(detail) == Some("of") && endpoint(&detail.object));
    let owner = node_owner.next().copied();
    if node_owner.next().is_some() {
        return None;
    }
    let mut ranges = Vec::new();
    if let Some(owner) = owner {
        if !details.iter().any(|detail| {
            detail.subject == owner.subject
                && property(detail) == Some("index")
                && matches!(&detail.object, TermPattern::Literal(value) if value.value() == "0")
        }) {
            return None;
        }
        ranges.extend(
            details
                .iter()
                .filter(|detail| {
                    detail.subject == owner.subject && property(detail) == Some("range")
                })
                .map(|detail| &detail.object),
        );
    }
    let mut symbols = false;
    let mut nodes = ReachedNodes::default();
    for &detail in details {
        let predicate = property(detail);
        if endpoint(&detail.subject)
            && (matches!(predicate, Some("name" | "kind" | "language" | "revision"))
                || is_symbol_class(detail))
        {
            symbols = true;
        } else if let Some(owner) = owner.filter(|owner| detail.subject == owner.subject) {
            match predicate {
                Some("of") if detail == owner => {}
                Some("source") => nodes.source = true,
                Some("declaration" | "kind" | "language") => nodes.syntax_metadata = true,
                Some("range" | "index") => {}
                _ if is_class(detail, "Node") => {}
                _ => return None,
            }
        } else if !ranges.contains(&&detail.subject)
            || !matches!(predicate, Some("unit" | "start" | "end"))
        {
            return None;
        }
    }
    Some((symbols, owner.map(|_| nodes)))
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
        first_step: AdjacencyStep { direction, facet },
        steps: Vec::new(),
        depth,
        symbol_details: false,
        nodes: None,
        occurrences: None,
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
                    first_step: AdjacencyStep { direction, facet },
                    steps: Vec::new(),
                    depth: Some(depth),
                    symbol_details: false,
                    nodes: None,
                    occurrences: None,
                });
            }
        }
    }
    None
}

fn layered_adjacency(seed: SymbolId, steps: Vec<AdjacencyStep>) -> Option<AdjacencySelection> {
    Some(AdjacencySelection {
        seed,
        first_step: *steps.first()?,
        depth: Some(u32::try_from(steps.len()).ok()?),
        steps,
        symbol_details: false,
        nodes: None,
        occurrences: None,
    })
}

fn opposite(direction: TraversalDirection) -> TraversalDirection {
    match direction {
        TraversalDirection::Incoming => TraversalDirection::Outgoing,
        TraversalDirection::Outgoing => TraversalDirection::Incoming,
    }
}

fn fixed_path(path: &PropertyPathExpression) -> Option<Vec<AdjacencyStep>> {
    let mut pending = vec![(path, TraversalDirection::Outgoing)];
    let mut steps = Vec::new();
    while let Some((path, direction)) = pending.pop() {
        match path {
            PropertyPathExpression::Link(predicate) => {
                let facet = facet(predicate.as_str().strip_prefix(VOCABULARY_NAMESPACE)?)?;
                steps.push(AdjacencyStep { direction, facet });
            }
            PropertyPathExpression::Inv(path) => pending.push((path, opposite(direction))),
            PropertyPathExpression::Seq(left, right) => {
                let (start, end) = if direction == TraversalDirection::Outgoing {
                    (left, right)
                } else {
                    (right, left)
                };
                pending.extend([(end.as_ref(), direction), (start.as_ref(), direction)]);
            }
            _ => return None,
        }
    }
    Some(steps)
}

fn layered_patterns(triples: &[&TriplePattern]) -> Option<AdjacencySelection> {
    for initial in triples
        .iter()
        .flat_map(|edge| [&edge.subject, &edge.object])
    {
        let Some(seed) = symbol(initial) else {
            continue;
        };
        let mut current = initial;
        let mut remaining = triples.to_vec();
        let mut steps = Vec::new();
        while !remaining.is_empty() {
            let mut next = remaining.iter().enumerate().filter_map(|(index, edge)| {
                let facet = property(edge).and_then(facet)?;
                if &edge.subject == current {
                    Some((
                        index,
                        &edge.object,
                        AdjacencyStep {
                            direction: TraversalDirection::Outgoing,
                            facet,
                        },
                    ))
                } else if &edge.object == current {
                    Some((
                        index,
                        &edge.subject,
                        AdjacencyStep {
                            direction: TraversalDirection::Incoming,
                            facet,
                        },
                    ))
                } else {
                    None
                }
            });
            let Some((index, end, step)) = next.next() else {
                break;
            };
            if next.next().is_some() {
                break;
            }
            current = end;
            steps.push(step);
            remaining.swap_remove(index);
        }
        if remaining.is_empty() {
            return layered_adjacency(seed, steps);
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
    let mut usage_enclosing = false;
    for &triple in triples {
        match property(triple) {
            Some("usages") if triple == *usage => {}
            Some("source") if triple.subject == usage.object => source = true,
            Some("of") if triple.subject == usage.object => usage_enclosing = true,
            Some("range" | "kind") if triple.subject == usage.object => {}
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
        first_step: AdjacencyStep {
            direction: TraversalDirection::Incoming,
            facet: RelationshipFacet::References,
        },
        steps: Vec::new(),
        depth: Some(1),
        symbol_details: false,
        nodes: None,
        occurrences: Some(OccurrenceSelection {
            source,
            enclosing: usage_enclosing,
        }),
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
                    work.push(Work::Visit(path, reversed));
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

pub(super) fn symbol_patterns(patterns: &[TriplePattern]) -> Option<SymbolSelection<'_>> {
    let mut identity = None;
    let mut released = false;
    let mut name = None;
    let single_subject = patterns.first().is_some_and(|first| {
        patterns
            .iter()
            .all(|pattern| pattern.subject == first.subject)
    });
    for pattern in patterns {
        if let TermPattern::NamedNode(subject) = &pattern.subject
            && let Ok(parsed) = rift_protocol::identity::SymbolIdentity::parse(subject.as_str())
        {
            released |= !parsed.owner().is_local();
            identity = Some(subject.as_str());
        }
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
    if let Some(identity) = identity.filter(|_| single_subject) {
        return Some(SymbolSelection::Identified(identity));
    }
    if released {
        return None;
    }
    Some(match name.filter(|_| single_subject) {
        Some(name) => SymbolSelection::Named(name),
        None => SymbolSelection::All,
    })
}

fn is_symbol_class(pattern: &TriplePattern) -> bool {
    is_class(pattern, "Symbol")
}

fn is_class(pattern: &TriplePattern, name: &str) -> bool {
    matches!(&pattern.predicate, NamedNodePattern::NamedNode(predicate) if predicate.as_str() == rdf::TYPE.as_str())
        && matches!(&pattern.object, TermPattern::NamedNode(class) if class.as_str().strip_prefix(VOCABULARY_NAMESPACE) == Some(name))
}
