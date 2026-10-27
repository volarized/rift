//! Domain acquisition requirements derived from the library's parsed algebra.

use crate::query::FactRequirement;
use rift_protocol::read::{RelationshipFacet, SymbolId, TraversalDirection};
use spargebra::Query;
use spargebra::algebra::{Expression, PropertyPathExpression, QueryExpression};
use spargebra::term::{NamedNodePattern, TermPattern};

use super::failure;
use super::{QueryError, VOCABULARY};
use crate::query::{AdjacencyRequirement, FactRequirements};

enum Pending<'a> {
    Pattern(&'a QueryExpression, bool),
    Project(&'a QueryExpression),
    Expression(&'a Expression),
}

struct SubjectProof<'a> {
    allowed: bool,
    service: Option<&'a str>,
    variable: Option<&'a str>,
}

impl<'a> SubjectProof<'a> {
    const fn new(allowed: bool) -> Self {
        Self {
            allowed,
            service: None,
            variable: None,
        }
    }

    fn pattern(&mut self, pattern: &'a QueryExpression, mandatory: bool) {
        match pattern {
            QueryExpression::Service {
                name,
                inner,
                silent,
            } => {
                let NamedNodePattern::NamedNode(name) = name else {
                    self.allowed = false;
                    return;
                };
                if *silent
                    || !mandatory
                    || self.service.is_some_and(|known| known != name.as_str())
                    || name
                        .as_str()
                        .strip_prefix(super::RESULT_SERVICE_PREFIX)
                        .is_none_or(str::is_empty)
                    || !matches!(inner.as_ref(), QueryExpression::Bgp { patterns } if patterns.is_empty())
                {
                    self.allowed = false;
                    return;
                }
                self.service = Some(name.as_str());
            }
            QueryExpression::Path { .. }
            | QueryExpression::Union { .. }
            | QueryExpression::Minus { .. }
            | QueryExpression::Lateral { .. }
            | QueryExpression::Graph { .. }
            | QueryExpression::Extend { .. }
            | QueryExpression::Group { .. }
            | QueryExpression::Values { .. } => self.allowed = false,
            _ => {}
        }
    }

    fn subject(&mut self, subject: &'a TermPattern) {
        let TermPattern::Variable(variable) = subject else {
            self.allowed = false;
            return;
        };
        if self
            .variable
            .is_some_and(|known| known != variable.as_str())
        {
            self.allowed = false;
        }
        self.variable = Some(variable.as_str());
    }

    fn project(&mut self, pattern: &QueryExpression) {
        let QueryExpression::Project { variables, .. } = pattern else {
            self.allowed = false;
            return;
        };
        if !self.variable.is_some_and(|subject| {
            variables
                .iter()
                .any(|variable| variable.as_str() == subject)
        }) {
            self.allowed = false;
        }
    }

    fn finish(
        self,
        text_bytes_max: usize,
    ) -> Result<Option<super::RetainedSubjectSelection>, QueryError> {
        let (Some(service), Some(variable)) = (self.service, self.variable) else {
            return Ok(None);
        };
        if !self.allowed {
            return Ok(None);
        }
        super::RetainedSubjectSelection::new(service, variable, text_bytes_max).map(Some)
    }
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
                PropertyPathExpression::Link(name) => {
                    let Some(facet) = name.as_str().strip_prefix(VOCABULARY).and_then(facet) else {
                        return Ok(None);
                    };
                    values.push(UniformPath {
                        facet,
                        incoming,
                        depth: Some(1),
                    });
                }
                PropertyPathExpression::Inv(inner) => {
                    pending.push(PathPending::Visit(inner, !incoming));
                }
                PropertyPathExpression::Seq(left, right) => {
                    pending.extend([
                        PathPending::Sequence,
                        PathPending::Visit(right, incoming),
                        PathPending::Visit(left, incoming),
                    ]);
                }
                PropertyPathExpression::Alt(left, right) => {
                    pending.extend([
                        PathPending::Alternative,
                        PathPending::Visit(right, incoming),
                        PathPending::Visit(left, incoming),
                    ]);
                }
                PropertyPathExpression::ZeroOrMorePath(inner)
                | PropertyPathExpression::OneOrMorePath(inner) => {
                    pending.extend([PathPending::Closure, PathPending::Visit(inner, incoming)]);
                }
                PropertyPathExpression::ZeroOrOnePath(inner) => {
                    pending.push(PathPending::Visit(inner, incoming));
                }
                PropertyPathExpression::Nps(_) => return Ok(None),
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
    pub(super) fn from_query(
        query: &Query,
        limits: super::QueryLimits,
    ) -> Result<(Self, Option<super::RetainedSubjectSelection>), QueryError> {
        let nodes_max = limits.algebra_nodes_max;
        let pattern = match query {
            Query::Select(query) => &query.expression,
            Query::Ask(query) => &query.expression,
            Query::Construct(query) => &query.expression,
            Query::Describe(query) => &query.pattern,
        };
        let mut result = Self::default();
        if matches!(query, Query::Describe(_)) {
            result.require(FactRequirement::Domain);
            result.require(FactRequirement::Description);
            result.require(FactRequirement::Source);
        }
        if !result.requires(FactRequirement::Description) {
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
        let mut subject =
            SubjectProof::new(!matches!(query, Query::Describe(_)) && query.dataset().is_none());
        let mut pending = vec![Pending::Pattern(pattern, true)];
        let mut visited = 0usize;
        while let Some(node) = pending.pop() {
            if !matches!(&node, Pending::Project(_)) {
                visited = visited
                    .checked_add(1)
                    .ok_or_else(|| failure::bound("algebra_nodes_max"))?;
            }
            if visited > nodes_max || pending.len() > nodes_max {
                return Err(failure::bound("algebra_nodes_max"));
            }
            match node {
                Pending::Pattern(pattern, mandatory) => {
                    subject.pattern(pattern, mandatory);
                    result.pattern(pattern, mandatory, &mut pending, nodes_max, &mut subject)?;
                }
                Pending::Project(pattern) => subject.project(pattern),
                Pending::Expression(expression) => {
                    if matches!(expression, Expression::Exists(_)) {
                        subject.allowed = false;
                    }
                    Self::expression(expression, &mut pending);
                }
            }
        }
        result.occurrences.sort();
        result.occurrences.dedup();
        result.datasets.sort();
        result.datasets.dedup();
        Ok((result, subject.finish(limits.query_bytes_max)?))
    }

    fn pattern<'a>(
        &mut self,
        pattern: &'a QueryExpression,
        mandatory: bool,
        pending: &mut Vec<Pending<'a>>,
        nodes_max: usize,
        subject: &mut SubjectProof<'a>,
    ) -> Result<(), QueryError> {
        match pattern {
            QueryExpression::Bgp { patterns } => self.bgp(patterns, nodes_max, subject)?,
            QueryExpression::Path {
                subject,
                path,
                object,
            } => {
                self.require(FactRequirement::Domain);
                self.path(subject, path, object, nodes_max)?;
            }
            QueryExpression::Join { left, right }
            | QueryExpression::Union { left, right }
            | QueryExpression::Minus { left, right }
            | QueryExpression::Lateral { left, right } => {
                pending.extend([
                    Pending::Pattern(left, mandatory),
                    Pending::Pattern(right, mandatory),
                ]);
            }
            QueryExpression::LeftJoin {
                left,
                right,
                expression,
            } => {
                pending.extend([
                    Pending::Pattern(left, mandatory),
                    Pending::Pattern(right, false),
                ]);
                pending.extend(expression.iter().map(Pending::Expression));
            }
            QueryExpression::Filter { expr, inner } => {
                pending.extend([
                    Pending::Pattern(inner, mandatory),
                    Pending::Expression(expr),
                ]);
            }
            QueryExpression::Extend {
                expression, inner, ..
            } => pending.extend([
                Pending::Pattern(inner, mandatory),
                Pending::Expression(expression),
            ]),
            QueryExpression::Graph { name, inner } => {
                self.selection(name, false);
                pending.push(Pending::Pattern(inner, mandatory));
            }
            QueryExpression::Service { name, inner, .. } => self.service(name, inner, nodes_max)?,
            QueryExpression::OrderBy { inner, expression } => {
                pending.push(Pending::Pattern(inner, mandatory));
                for order in expression {
                    let (spargebra::algebra::OrderExpression::Asc(value)
                    | spargebra::algebra::OrderExpression::Desc(value)) = order;
                    pending.push(Pending::Expression(value));
                }
            }
            QueryExpression::Project { inner, .. } => {
                pending.extend([
                    Pending::Project(pattern),
                    Pending::Pattern(inner, mandatory),
                ]);
            }
            QueryExpression::Distinct { inner }
            | QueryExpression::Reduced { inner }
            | QueryExpression::Slice { inner, .. } => {
                pending.push(Pending::Pattern(inner, mandatory));
            }
            QueryExpression::Group {
                inner, aggregates, ..
            } => {
                pending.push(Pending::Pattern(inner, mandatory));
                for (_, aggregate) in aggregates {
                    if let spargebra::algebra::AggregateExpression::FunctionCall { expr, .. } =
                        aggregate
                    {
                        pending.push(Pending::Expression(expr));
                    }
                }
            }
            QueryExpression::Values { .. } => {}
        }
        Ok(())
    }

    fn bgp<'a>(
        &mut self,
        patterns: &'a [spargebra::term::TriplePattern],
        nodes_max: usize,
        subject: &mut SubjectProof<'a>,
    ) -> Result<(), QueryError> {
        if !patterns.is_empty() {
            self.require(FactRequirement::Domain);
        }
        if patterns.len() > nodes_max {
            return Err(failure::bound("algebra_nodes_max"));
        }
        let chain = linear_adjacency(patterns);
        if let Some(requirement) = &chain {
            for facet in &requirement.facets {
                if !self.facets.contains(facet) {
                    self.facets.push(*facet);
                }
            }
            self.adjacency.push(requirement.clone());
        }
        for triple in patterns {
            subject.subject(&triple.subject);
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
                    self.triple(&triple.subject, predicate.as_str(), &triple.object);
                }
                NamedNodePattern::Variable(_) => {
                    self.require(FactRequirement::CompleteDomain);
                    self.require(FactRequirement::AllOccurrences);
                    self.all_families();
                }
            }
        }
        Ok(())
    }

    fn selection(&mut self, name: &NamedNodePattern, service: bool) {
        match name {
            NamedNodePattern::NamedNode(name) if service => {
                self.services.push(name.as_str().to_owned());
            }
            NamedNodePattern::NamedNode(name) => self.datasets.push(name.as_str().to_owned()),
            NamedNodePattern::Variable(_) => self.require(FactRequirement::CompleteDomain),
        }
    }

    fn service(
        &mut self,
        name: &NamedNodePattern,
        inner: &QueryExpression,
        nodes_max: usize,
    ) -> Result<(), QueryError> {
        if matches!(name, NamedNodePattern::Variable(_)) {
            return Err(failure::parse("local service names must be constant IRIs"));
        }
        self.selection(name, true);
        if let NamedNodePattern::NamedNode(name) = name {
            if name.as_str() == super::SEARCH_SERVICE {
                let request = super::services::request(inner, nodes_max)?;
                if !self.retrievals.contains(&request) {
                    self.retrievals.push(request);
                }
            } else if name.as_str() == super::SOURCE_SERVICE {
                let request = super::services::source_request(inner, nodes_max)?;
                if !self.source_reads.contains(&request) {
                    self.source_reads.push(request);
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
            "usages" => {
                if let Some(identity) = symbol(subject) {
                    self.occurrences.push(identity);
                } else {
                    self.require(FactRequirement::CompleteDomain);
                    self.require(FactRequirement::AllOccurrences);
                }
            }
            "source" | "text" => self.require(FactRequirement::Source),
            "parent" | "regions" => self.require(FactRequirement::Nodes),
            "history" => {
                self.require(FactRequirement::History);
                if let Some(identity) = symbol(subject) {
                    if !self.history_symbols.contains(&identity) {
                        self.history_symbols.push(identity);
                    }
                } else {
                    self.require(FactRequirement::AllHistories);
                }
            }
            "versions" | "complete" => self.require(FactRequirement::History),
            "documentation_revision"
            | "selection_digest"
            | "content_digest"
            | "media_type"
            | "unresolved_references"
            | "resolution"
            | "references_to"
            | "format"
            | "target"
            | "heading_path"
            | "chunks"
            | "authored"
            | "block"
            | "selection"
            | "physical_ranges"
            | "byte_length"
            | "license" => self.require(FactRequirement::Documentation),
            "from" | "to" | "derivation" | "evidence" | "occurrence" | "confidence" => {
                self.require(FactRequirement::CompleteDomain);
                if name == "evidence" {
                    self.require(FactRequirement::Documentation);
                }
            }
            _ => self.relationship(subject, name, object),
        }
    }

    fn type_requirement(&mut self, object: &TermPattern) {
        let TermPattern::NamedNode(name) = object else {
            self.require(FactRequirement::CompleteDomain);
            self.require(FactRequirement::AllOccurrences);
            self.all_families();
            return;
        };
        match name.as_str().strip_prefix(VOCABULARY) {
            Some("Relationship") => self.require(FactRequirement::CompleteDomain),
            Some("Node" | "NodeRegion") => self.require(FactRequirement::Nodes),
            Some("SymbolHistory" | "SymbolVersion") => {
                self.require(FactRequirement::History);
                self.require(FactRequirement::AllHistories);
            }
            Some(
                "DocumentationSource"
                | "DocumentationBlock"
                | "DocumentationLink"
                | "DocumentationReference"
                | "DocumentationReferenceCandidate",
            ) => self.require(FactRequirement::Documentation),
            Some("SourceUnitSpan") => {
                self.require(FactRequirement::CompleteDomain);
                self.require(FactRequirement::AllOccurrences);
            }
            _ => {}
        }
    }

    fn all_families(&mut self) {
        self.require(FactRequirement::Nodes);
        self.require(FactRequirement::History);
        self.require(FactRequirement::AllHistories);
        self.require(FactRequirement::Documentation);
        self.require(FactRequirement::Source);
    }

    fn relationship(&mut self, subject: &TermPattern, name: &str, object: &TermPattern) {
        let Some(facet) = facet(name) else {
            return;
        };
        if !self.facets.contains(&facet) {
            self.facets.push(facet);
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
            None => self.require(FactRequirement::CompleteDomain),
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
                    self.facets.push(summary.facet);
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
                PropertyPathExpression::Link(name) => {
                    let Some(value) = name.as_str().strip_prefix(VOCABULARY).and_then(facet) else {
                        self.require(FactRequirement::CompleteDomain);
                        self.triple(subject, name.as_str(), object);
                        continue;
                    };
                    if !facets.contains(&value) {
                        facets.push(value);
                    }
                    depth = depth.and_then(|value| value.checked_add(1));
                }
                PropertyPathExpression::Inv(inner) => {
                    reverse = !reverse;
                    mixed = visited != 1;
                    pending.push(inner);
                }
                PropertyPathExpression::Seq(left, right)
                | PropertyPathExpression::Alt(left, right) => {
                    pending.extend([left.as_ref(), right.as_ref()]);
                }
                PropertyPathExpression::ZeroOrMorePath(inner)
                | PropertyPathExpression::OneOrMorePath(inner) => {
                    depth = None;
                    pending.push(inner);
                }
                PropertyPathExpression::ZeroOrOnePath(inner) => pending.push(inner),
                PropertyPathExpression::Nps(_) => {
                    self.require(FactRequirement::CompleteDomain);
                    self.require(FactRequirement::AllOccurrences);
                    self.all_families();
                }
            }
        }
        for value in &facets {
            if !self.facets.contains(value) {
                self.facets.push(*value);
            }
        }
        if facets.len() > 1 {
            self.require(FactRequirement::CompleteDomain);
        }
        let anchored = symbol(subject)
            .map(|seed| (seed, reverse))
            .or_else(|| symbol(object).map(|seed| (seed, !reverse)));
        let Some((seed, incoming)) = anchored else {
            self.require(FactRequirement::CompleteDomain);
            return Ok(());
        };
        if facets.is_empty() {
            return Ok(());
        }
        if mixed {
            self.require(FactRequirement::CompleteDomain);
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
            Expression::Exists(pattern) => pending.push(Pending::Pattern(pattern, false)),
            Expression::Or(left, right) | Expression::And(left, right) => {
                pending.extend([Pending::Expression(left), Pending::Expression(right)]);
            }
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
                pending.extend(values.iter().map(Pending::Expression));
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

fn single_name_pattern(mut pattern: &QueryExpression) -> Option<String> {
    loop {
        match pattern {
            QueryExpression::Project { inner, .. }
            | QueryExpression::Distinct { inner }
            | QueryExpression::Reduced { inner }
            | QueryExpression::Slice { inner, .. }
            | QueryExpression::OrderBy { inner, .. } => pattern = inner,
            QueryExpression::Bgp { patterns } if patterns.len() == 2 => {
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

fn single_usage_pattern(mut pattern: &QueryExpression) -> Option<Vec<SymbolId>> {
    loop {
        match pattern {
            QueryExpression::Project { inner, .. }
            | QueryExpression::Distinct { inner }
            | QueryExpression::Reduced { inner }
            | QueryExpression::Slice { inner, .. }
            | QueryExpression::OrderBy { inner, .. } => pattern = inner,
            QueryExpression::Bgp { patterns } => {
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

#[cfg(test)]
mod service_name_tests {
    use crate::sparql::{PreparedQuery, QueryLimits};

    #[test]
    fn variable_service_names_require_proven_acquisition_before_silent_evaluation()
    -> Result<(), Box<dyn std::error::Error>> {
        let limits = QueryLimits::try_from(&rift_protocol::query::QueryConfiguration::default())?;
        for silent in ["", "SILENT "] {
            let query = format!(
                "ASK {{ VALUES ?service {{ :source }} SERVICE {silent}?service {{ \
                    ?read :unit <rift://source/project/notes.txt>; :outcome ?outcome \
                }} }}"
            );
            let error = PreparedQuery::parse(&query, limits)
                .expect_err("variable service acquisition is not established");
            assert_eq!(error.slug().as_str(), "rift.server.read_invalid");
        }
        Ok(())
    }
}
