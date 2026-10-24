//! Domain acquisition requirements derived from the library's parsed algebra.

use rift_protocol::read::{RelationshipFacet, SymbolId, TraversalDirection};
use spargebra::Query;
use spargebra::algebra::{Expression, GraphPattern, PropertyPathExpression};
use spargebra::term::{NamedNodePattern, TermPattern};

use super::failure;
use super::{QueryError, VOCABULARY};
use crate::query::{AdjacencyRequirement, FactRequirements};

enum Pending<'a> {
    Pattern(&'a GraphPattern),
    Expression(&'a Expression),
}

struct UniformPath {
    facet: RelationshipFacet,
    incoming: bool,
    depth: Option<u32>,
}

enum PathPending<'a> {
    Visit(&'a PropertyPathExpression, bool),
    Sequence,
    Alternative,
    Closure,
}

fn uniform_path(
    path: &PropertyPathExpression,
    nodes_max: usize,
) -> Result<Option<UniformPath>, QueryError> {
    let mut pending = vec![PathPending::Visit(path, false)];
    let mut values: Vec<UniformPath> = Vec::new();
    let mut visited = 0usize;
    while let Some(item) = pending.pop() {
        visited += usize::from(matches!(item, PathPending::Visit(..)));
        if visited > nodes_max || pending.len() > nodes_max || values.len() > nodes_max {
            return Err(failure::bound("algebra_nodes_max"));
        }
        match item {
            PathPending::Visit(path, incoming) => match path {
                PropertyPathExpression::NamedNode(name) => {
                    let Some(facet) = name.as_str().strip_prefix(VOCABULARY).and_then(facet) else {
                        return Ok(None);
                    };
                    values.push(UniformPath {
                        facet,
                        incoming,
                        depth: Some(1),
                    });
                }
                PropertyPathExpression::Reverse(inner) => {
                    pending.push(PathPending::Visit(inner, !incoming))
                }
                PropertyPathExpression::Sequence(left, right) => {
                    pending.extend([
                        PathPending::Sequence,
                        PathPending::Visit(right, incoming),
                        PathPending::Visit(left, incoming),
                    ]);
                }
                PropertyPathExpression::Alternative(left, right) => {
                    pending.extend([
                        PathPending::Alternative,
                        PathPending::Visit(right, incoming),
                        PathPending::Visit(left, incoming),
                    ]);
                }
                PropertyPathExpression::ZeroOrMore(inner)
                | PropertyPathExpression::OneOrMore(inner) => {
                    pending.extend([PathPending::Closure, PathPending::Visit(inner, incoming)]);
                }
                PropertyPathExpression::ZeroOrOne(inner) => {
                    pending.push(PathPending::Visit(inner, incoming))
                }
                PropertyPathExpression::NegatedPropertySet(_) => return Ok(None),
            },
            PathPending::Closure => {
                let value = values
                    .last_mut()
                    .ok_or_else(|| failure::bound("algebra_nodes_max"))?;
                value.depth = None;
            }
            operation => {
                let right = values
                    .pop()
                    .ok_or_else(|| failure::bound("algebra_nodes_max"))?;
                let mut left = values
                    .pop()
                    .ok_or_else(|| failure::bound("algebra_nodes_max"))?;
                if left.facet != right.facet || left.incoming != right.incoming {
                    return Ok(None);
                }
                left.depth = match (left.depth, right.depth) {
                    (Some(left), Some(right)) => {
                        Some(if matches!(operation, PathPending::Sequence) {
                            left.checked_add(right)
                                .ok_or_else(|| failure::bound("algebra_nodes_max"))?
                        } else {
                            left.max(right)
                        })
                    }
                    _ => None,
                };
                values.push(left);
            }
        }
    }
    Ok(values.pop())
}

impl FactRequirements {
    pub(super) fn from_query(query: &Query, nodes_max: usize) -> Result<Self, QueryError> {
        let pattern = match query {
            Query::Select { pattern, .. }
            | Query::Ask { pattern, .. }
            | Query::Construct { pattern, .. }
            | Query::Describe { pattern, .. } => pattern,
        };
        let mut result = Self::default();
        if matches!(query, Query::Describe { .. }) {
            result.domain = true;
            result.description = true;
            result.source = true;
        }
        if !result.description {
            result.symbol_name = single_name_pattern(pattern);
            result.symbol_ids = single_usage_pattern(pattern);
        }
        if let Some(dataset) = query.dataset() {
            result
                .datasets
                .extend(dataset.default.iter().map(|name| name.as_str().to_owned()));
            if let Some(named) = &dataset.named {
                result
                    .datasets
                    .extend(named.iter().map(|name| name.as_str().to_owned()));
            }
        }
        let mut pending = vec![Pending::Pattern(pattern)];
        let mut visited = 0usize;
        while let Some(node) = pending.pop() {
            visited = visited
                .checked_add(1)
                .ok_or_else(|| failure::bound("algebra_nodes_max"))?;
            if visited > nodes_max || pending.len() > nodes_max {
                return Err(failure::bound("algebra_nodes_max"));
            }
            match node {
                Pending::Pattern(pattern) => result.pattern(pattern, &mut pending, nodes_max)?,
                Pending::Expression(expression) => Self::expression(expression, &mut pending),
            }
        }
        result.occurrences.sort();
        result.occurrences.dedup();
        result.datasets.sort();
        result.datasets.dedup();
        Ok(result)
    }

    fn pattern<'a>(
        &mut self,
        pattern: &'a GraphPattern,
        pending: &mut Vec<Pending<'a>>,
        nodes_max: usize,
    ) -> Result<(), QueryError> {
        match pattern {
            GraphPattern::Bgp { patterns } => {
                self.domain |= !patterns.is_empty();
                if patterns.len() > nodes_max {
                    return Err(failure::bound("algebra_nodes_max"));
                }
                let chain = linear_adjacency(patterns);
                if let Some(requirement) = &chain {
                    for facet in &requirement.facets {
                        if !self.facets.contains(facet) {
                            self.facets.push(facet.clone());
                        }
                    }
                    self.adjacency.push(requirement.clone());
                }
                for triple in patterns {
                    match &triple.predicate {
                        NamedNodePattern::NamedNode(predicate) => {
                            if chain.is_some()
                                && predicate
                                    .as_str()
                                    .strip_prefix(VOCABULARY)
                                    .and_then(facet)
                                    .is_some()
                            {
                                continue;
                            }
                            self.triple(&triple.subject, predicate.as_str(), &triple.object)
                        }
                        NamedNodePattern::Variable(_) => {
                            self.complete_domain = true;
                            self.all_occurrences = true;
                            self.all_families();
                        }
                    }
                }
            }
            GraphPattern::Path {
                subject,
                path,
                object,
            } => {
                self.domain = true;
                self.path(subject, path, object, nodes_max)?;
            }
            GraphPattern::Join { left, right }
            | GraphPattern::Union { left, right }
            | GraphPattern::Minus { left, right }
            | GraphPattern::Lateral { left, right } => {
                pending.extend([Pending::Pattern(left), Pending::Pattern(right)])
            }
            GraphPattern::LeftJoin {
                left,
                right,
                expression,
            } => {
                pending.extend([Pending::Pattern(left), Pending::Pattern(right)]);
                pending.extend(expression.iter().map(Pending::Expression));
            }
            GraphPattern::Filter { expr, inner } => {
                pending.extend([Pending::Pattern(inner), Pending::Expression(expr)])
            }
            GraphPattern::Extend {
                expression, inner, ..
            } => pending.extend([Pending::Pattern(inner), Pending::Expression(expression)]),
            GraphPattern::Graph { name, inner } => {
                self.selection(name, false);
                pending.push(Pending::Pattern(inner));
            }
            GraphPattern::Service { name, inner, .. } => self.service(name, inner, nodes_max)?,
            GraphPattern::OrderBy { inner, expression } => {
                pending.push(Pending::Pattern(inner));
                for order in expression {
                    let (spargebra::algebra::OrderExpression::Asc(value)
                    | spargebra::algebra::OrderExpression::Desc(value)) = order;
                    pending.push(Pending::Expression(value));
                }
            }
            GraphPattern::Project { inner, .. }
            | GraphPattern::Distinct { inner }
            | GraphPattern::Reduced { inner }
            | GraphPattern::Slice { inner, .. } => pending.push(Pending::Pattern(inner)),
            GraphPattern::Group {
                inner, aggregates, ..
            } => {
                pending.push(Pending::Pattern(inner));
                for (_, aggregate) in aggregates {
                    if let spargebra::algebra::AggregateExpression::FunctionCall { expr, .. } =
                        aggregate
                    {
                        pending.push(Pending::Expression(expr));
                    }
                }
            }
            GraphPattern::Values { .. } => {}
        }
        Ok(())
    }

    fn selection(&mut self, name: &NamedNodePattern, service: bool) {
        match name {
            NamedNodePattern::NamedNode(name) if service => {
                self.services.push(name.as_str().to_owned())
            }
            NamedNodePattern::NamedNode(name) => self.datasets.push(name.as_str().to_owned()),
            NamedNodePattern::Variable(_) => self.complete_domain = true,
        }
    }

    fn service(
        &mut self,
        name: &NamedNodePattern,
        inner: &GraphPattern,
        nodes_max: usize,
    ) -> Result<(), QueryError> {
        self.selection(name, true);
        if let NamedNodePattern::NamedNode(name) = name {
            if name.as_str() == super::SEARCH_SERVICE {
                let request = super::services::request(inner, nodes_max)?;
                if !self.retrievals.contains(&request) {
                    self.retrievals.push(request);
                }
            }
        }
        Ok(())
    }

    fn triple(&mut self, subject: &TermPattern, predicate: &str, object: &TermPattern) {
        if predicate == "http://www.w3.org/1999/02/22-rdf-syntax-ns#type" {
            self.type_requirement(object);
            return;
        }
        let Some(name) = predicate.strip_prefix(VOCABULARY) else {
            return;
        };
        match name {
            "usages" => match symbol(subject) {
                Some(identity) => self.occurrences.push(identity),
                None => {
                    self.complete_domain = true;
                    self.all_occurrences = true;
                }
            },
            "source" | "text" => self.source = true,
            "parent" | "regions" => self.nodes = true,
            "history" => {
                self.history = true;
                if let Some(identity) = symbol(subject) {
                    if !self.history_symbols.contains(&identity) {
                        self.history_symbols.push(identity);
                    }
                } else {
                    self.all_histories = true;
                }
            }
            "versions" | "complete" => self.history = true,
            "documentation_revision"
            | "selection_digest"
            | "content_digest"
            | "media_type"
            | "unresolved_references"
            | "resolution"
            | "references_to" => self.documentation = true,
            "format" | "target" => self.documentation = true,
            "heading_path" | "chunks" | "authored" | "block" | "selection" | "physical_ranges"
            | "byte_length" | "license" => self.documentation = true,
            "from" | "to" | "derivation" | "evidence" | "occurrence" | "confidence" => {
                self.complete_domain = true;
                if name == "evidence" {
                    self.documentation = true;
                }
            }
            _ => self.relationship(subject, name, object),
        }
    }

    fn type_requirement(&mut self, object: &TermPattern) {
        let TermPattern::NamedNode(name) = object else {
            self.complete_domain = true;
            self.all_occurrences = true;
            self.all_families();
            return;
        };
        match name.as_str().strip_prefix(VOCABULARY) {
            Some("Relationship") => self.complete_domain = true,
            Some("Node" | "NodeRegion") => self.nodes = true,
            Some("SymbolHistory" | "SymbolVersion") => {
                self.history = true;
                self.all_histories = true;
            }
            Some(
                "DocumentationSource"
                | "DocumentationBlock"
                | "DocumentationLink"
                | "DocumentationReference"
                | "DocumentationReferenceCandidate",
            ) => self.documentation = true,
            Some("SourceUnitSpan") => {
                self.complete_domain = true;
                self.all_occurrences = true;
            }
            _ => {}
        }
    }

    fn all_families(&mut self) {
        self.nodes = true;
        self.history = true;
        self.all_histories = true;
        self.documentation = true;
        self.source = true;
    }

    fn relationship(&mut self, subject: &TermPattern, name: &str, object: &TermPattern) {
        let Some(facet) = facet(name) else {
            return;
        };
        if !self.facets.contains(&facet) {
            self.facets.push(facet.clone());
        }
        let anchored = symbol(subject)
            .map(|seed| (seed, TraversalDirection::Outgoing))
            .or_else(|| symbol(object).map(|seed| (seed, TraversalDirection::Incoming)));
        match anchored {
            Some((seed, direction)) => self.adjacency.push(AdjacencyRequirement {
                seed,
                direction,
                facets: vec![facet],
                depth: Some(1),
            }),
            None => self.complete_domain = true,
        }
    }

    fn path(
        &mut self,
        subject: &TermPattern,
        path: &PropertyPathExpression,
        object: &TermPattern,
        nodes_max: usize,
    ) -> Result<(), QueryError> {
        if let Some(summary) = uniform_path(path, nodes_max)? {
            let anchored = symbol(subject)
                .map(|seed| (seed, summary.incoming))
                .or_else(|| symbol(object).map(|seed| (seed, !summary.incoming)));
            if let Some((seed, incoming)) = anchored {
                if !self.facets.contains(&summary.facet) {
                    self.facets.push(summary.facet.clone());
                }
                self.adjacency.push(AdjacencyRequirement {
                    seed,
                    direction: if incoming {
                        TraversalDirection::Incoming
                    } else {
                        TraversalDirection::Outgoing
                    },
                    facets: vec![summary.facet],
                    depth: summary.depth,
                });
                return Ok(());
            }
        }
        let mut pending = vec![path];
        let mut facets = Vec::new();
        let mut depth = Some(0u32);
        let mut reverse = false;
        let mut mixed = false;
        let mut visited = 0usize;
        while let Some(path) = pending.pop() {
            visited += 1;
            if visited > nodes_max {
                return Err(failure::bound("algebra_nodes_max"));
            }
            match path {
                PropertyPathExpression::NamedNode(name) => {
                    let Some(value) = name.as_str().strip_prefix(VOCABULARY).and_then(facet) else {
                        self.complete_domain = true;
                        self.triple(subject, name.as_str(), object);
                        continue;
                    };
                    if !facets.contains(&value) {
                        facets.push(value);
                    }
                    depth = depth.and_then(|value| value.checked_add(1));
                }
                PropertyPathExpression::Reverse(inner) => {
                    reverse = !reverse;
                    mixed = visited != 1;
                    pending.push(inner);
                }
                PropertyPathExpression::Sequence(left, right)
                | PropertyPathExpression::Alternative(left, right) => {
                    pending.extend([left.as_ref(), right.as_ref()])
                }
                PropertyPathExpression::ZeroOrMore(inner)
                | PropertyPathExpression::OneOrMore(inner) => {
                    depth = None;
                    pending.push(inner);
                }
                PropertyPathExpression::ZeroOrOne(inner) => pending.push(inner),
                PropertyPathExpression::NegatedPropertySet(_) => {
                    self.complete_domain = true;
                    self.all_occurrences = true;
                    self.all_families();
                }
            }
        }
        for value in &facets {
            if !self.facets.contains(value) {
                self.facets.push(value.clone());
            }
        }
        if facets.len() > 1 {
            self.complete_domain = true;
        }
        let anchored = symbol(subject)
            .map(|seed| (seed, reverse))
            .or_else(|| symbol(object).map(|seed| (seed, !reverse)));
        let Some((seed, incoming)) = anchored else {
            self.complete_domain = true;
            return Ok(());
        };
        if facets.is_empty() {
            return Ok(());
        }
        if mixed {
            self.complete_domain = true;
        }
        let direction = if incoming {
            TraversalDirection::Incoming
        } else {
            TraversalDirection::Outgoing
        };
        self.adjacency.push(AdjacencyRequirement {
            seed,
            direction,
            facets,
            depth,
        });
        Ok(())
    }

    fn expression<'a>(expression: &'a Expression, pending: &mut Vec<Pending<'a>>) {
        match expression {
            Expression::Exists(pattern) => pending.push(Pending::Pattern(pattern)),
            Expression::Or(left, right)
            | Expression::And(left, right)
            | Expression::Equal(left, right)
            | Expression::SameTerm(left, right)
            | Expression::Greater(left, right)
            | Expression::GreaterOrEqual(left, right)
            | Expression::Less(left, right)
            | Expression::LessOrEqual(left, right)
            | Expression::Add(left, right)
            | Expression::Subtract(left, right)
            | Expression::Multiply(left, right)
            | Expression::Divide(left, right) => {
                pending.extend([Pending::Expression(left), Pending::Expression(right)])
            }
            Expression::UnaryPlus(inner)
            | Expression::UnaryMinus(inner)
            | Expression::Not(inner) => pending.push(Pending::Expression(inner)),
            Expression::In(value, others) => {
                pending.push(Pending::Expression(value));
                pending.extend(others.iter().map(Pending::Expression));
            }
            Expression::If(condition, yes, no) => pending.extend([
                Pending::Expression(condition),
                Pending::Expression(yes),
                Pending::Expression(no),
            ]),
            Expression::Coalesce(values) | Expression::FunctionCall(_, values) => {
                pending.extend(values.iter().map(Pending::Expression))
            }
            Expression::NamedNode(_)
            | Expression::Literal(_)
            | Expression::Variable(_)
            | Expression::Bound(_) => {}
        }
    }
}

fn symbol(term: &TermPattern) -> Option<SymbolId> {
    let TermPattern::NamedNode(identity) = term else {
        return None;
    };
    let identity = rift_protocol::identity::SymbolIdentity::parse(identity.as_str()).ok()?;
    Some(SymbolId::from_identity(&identity))
}

fn linear_adjacency(patterns: &[spargebra::term::TriplePattern]) -> Option<AdjacencyRequirement> {
    let mut edges = Vec::new();
    let mut relationship = None;
    for triple in patterns {
        let NamedNodePattern::NamedNode(predicate) = &triple.predicate else {
            return None;
        };
        let name = predicate.as_str().strip_prefix(VOCABULARY);
        if let Some(facet) = name.and_then(facet) {
            if relationship.as_ref().is_some_and(|known| *known != facet) {
                return None;
            }
            relationship = Some(facet);
            edges.push(triple);
        } else if !matches!(name, Some("name" | "kind" | "language" | "path")) {
            return None;
        }
    }
    if edges.len() < 2 {
        return None;
    }
    let facet = relationship?;
    let preferred = if facet.as_ref() == "references" {
        [TraversalDirection::Incoming, TraversalDirection::Outgoing]
    } else {
        [TraversalDirection::Outgoing, TraversalDirection::Incoming]
    };
    for direction in preferred {
        for edge in &edges {
            let anchor = match &direction {
                TraversalDirection::Outgoing => &edge.subject,
                TraversalDirection::Incoming => &edge.object,
            };
            let Some(seed) = symbol(anchor) else {
                continue;
            };
            let mut current = anchor;
            let mut used = vec![false; edges.len()];
            let mut traversed = 0usize;
            loop {
                let mut following = None;
                for (index, edge) in edges.iter().enumerate() {
                    if used[index] {
                        continue;
                    }
                    let (from, to) = match &direction {
                        TraversalDirection::Outgoing => (&edge.subject, &edge.object),
                        TraversalDirection::Incoming => (&edge.object, &edge.subject),
                    };
                    if from == current {
                        if following.is_some() {
                            return None;
                        }
                        following = Some((index, to));
                    }
                }
                let Some((index, next)) = following else {
                    break;
                };
                used[index] = true;
                traversed += 1;
                current = next;
            }
            if traversed == edges.len() {
                return Some(AdjacencyRequirement {
                    seed,
                    direction,
                    facets: vec![facet],
                    depth: Some(u32::try_from(traversed).ok()?),
                });
            }
        }
    }
    None
}

fn single_name_pattern(mut pattern: &GraphPattern) -> Option<String> {
    loop {
        match pattern {
            GraphPattern::Project { inner, .. }
            | GraphPattern::Distinct { inner }
            | GraphPattern::Reduced { inner }
            | GraphPattern::Slice { inner, .. }
            | GraphPattern::OrderBy { inner, .. } => pattern = inner,
            GraphPattern::Bgp { patterns } if patterns.len() == 2 => {
                let triple = patterns.iter().find(|triple| matches!(&triple.predicate, NamedNodePattern::NamedNode(predicate) if predicate.as_str().strip_prefix(VOCABULARY) == Some("name")))?;
                let TermPattern::Literal(value) = &triple.object else {
                    return None;
                };
                let symbol_class = patterns.iter().any(|candidate| candidate.subject == triple.subject
                    && matches!(&candidate.predicate, NamedNodePattern::NamedNode(predicate) if predicate.as_str() == "http://www.w3.org/1999/02/22-rdf-syntax-ns#type")
                    && matches!(&candidate.object, TermPattern::NamedNode(class) if class.as_str().strip_prefix(VOCABULARY) == Some("Symbol")));
                return symbol_class.then(|| value.value().to_owned());
            }
            _ => return None,
        }
    }
}

fn single_usage_pattern(mut pattern: &GraphPattern) -> Option<Vec<SymbolId>> {
    loop {
        match pattern {
            GraphPattern::Project { inner, .. }
            | GraphPattern::Distinct { inner }
            | GraphPattern::Reduced { inner }
            | GraphPattern::Slice { inner, .. }
            | GraphPattern::OrderBy { inner, .. } => pattern = inner,
            GraphPattern::Bgp { patterns } => {
                let (anchor, usage) = patterns.iter().find_map(|triple| {
                    let NamedNodePattern::NamedNode(predicate) = &triple.predicate else {
                        return None;
                    };
                    (predicate.as_str().strip_prefix(VOCABULARY) == Some("usages"))
                        .then(|| symbol(&triple.subject).map(|identity| (identity, &triple.object)))
                        .flatten()
                })?;
                for triple in patterns {
                    let NamedNodePattern::NamedNode(predicate) = &triple.predicate else {
                        return None;
                    };
                    let name = predicate.as_str().strip_prefix(VOCABULARY)?;
                    if name == "usages"
                        && symbol(&triple.subject).as_ref() == Some(&anchor)
                        && &triple.object == usage
                    {
                        continue;
                    }
                    if &triple.subject != usage
                        || !["unit", "start", "end", "enclosing", "facets", "path"].contains(&name)
                    {
                        return None;
                    }
                }
                return Some(vec![anchor]);
            }
            _ => return None,
        }
    }
}

fn facet(value: &str) -> Option<RelationshipFacet> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).ok()
}
