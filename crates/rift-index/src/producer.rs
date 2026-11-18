//! Facts an external producer emitted, entering a package publication.
//!
//! A producer addresses source the way its own tools do: by source unit and byte range.
//! [`PublicationSyntax`] resolves such a range to the syntax element address every read uses, and
//! [`accept_producer_facts`] adds the producer's edges and occurrences to the
//! publication under that producer's name. [`FactExtractor`] is the interface an external
//! extractor implements, and [`accept_extracted_facts`] runs one and imports its output.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use rift_error::{RiftError, errors};
use rift_protocol::index::{PACKAGE_SYMBOLS_CEILING, PackageOccurrence, PackagePublication};
use rift_protocol::producer::{
    FactCoverage, FactKind, FactProducer, PRODUCER_FACTS_FORMAT_REVISION, ProducerEdge,
    ProducerFacts, ProducerOccurrence,
};
use rift_protocol::read::{Edge, SourceUnitId, SourceUnitSpan, SymbolId};
use rift_syntax::{ByteRange, SyntaxDocument, SyntaxLimits, SyntaxMatch, SyntaxSource};

use crate::located_syntax_element::{LocatedSyntax, SyntaxAddress};

mod extractor;

pub use extractor::{ExtractionInput, FactExtractor, accept_extracted_facts};

/// Edges and occurrences one [`accept_producer_facts`] call adds, together: the
/// declaration ceiling a publication supports.
pub const PRODUCER_FACTS_MAX: u32 = PACKAGE_SYMBOLS_CEILING;

/// One source unit parsed for resolution, with the address of each of its declarations.
struct ParsedUnit<'publication> {
    source: &'publication str,
    document: Option<SyntaxDocument>,
    declarations: Vec<Option<(rift_core::SymbolId, usize)>>,
}

/// Resolves byte ranges inside a publication's source units to syntax element addresses.
///
/// A declaration's address is its symbol and its ordinal among that symbol's declarations in
/// publication order, the order a symbol read lists them. Each unit is parsed once, under
/// `limits`, the first time a range names it.
pub struct PublicationSyntax<'publication> {
    publication: &'publication PackagePublication,
    limits: SyntaxLimits,
    addresses: BTreeMap<(&'publication str, u64, u64), (&'publication SymbolId, usize)>,
    parsed: BTreeMap<SourceUnitId, ParsedUnit<'publication>>,
}

impl std::fmt::Debug for PublicationSyntax<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PublicationSyntax")
            .field("units_parsed", &self.parsed.len())
            .finish_non_exhaustive()
    }
}

impl<'publication> PublicationSyntax<'publication> {
    /// Prepares resolution over `publication`'s units, parsed under `limits`.
    #[must_use]
    pub fn new(publication: &'publication PackagePublication, limits: SyntaxLimits) -> Self {
        let mut ordinals = BTreeMap::<&SymbolId, usize>::new();
        let mut addresses = BTreeMap::new();
        for declaration in &publication.declarations {
            let ordinal = ordinals.entry(&declaration.symbol).or_default();
            addresses.insert(
                (
                    declaration.unit.0.as_str(),
                    declaration.range.start,
                    declaration.range.end,
                ),
                (&declaration.symbol, *ordinal),
            );
            *ordinal += 1;
        }
        Self {
            publication,
            limits,
            addresses,
            parsed: BTreeMap::new(),
        }
    }

    /// The syntax element at `range` of `unit`: the innermost syntax element spanning it exactly, or under
    /// [`SyntaxMatch::Enclosing`] the innermost one containing it.
    ///
    /// # Errors
    /// Returns `producer_range_unresolved` when the publication carries no `unit`, its
    /// retained source was cut, or no syntax element answers `range`; the provider's refusal when the
    /// retained source no longer parses.
    pub fn syntax_element_at(
        &mut self,
        unit: &SourceUnitId,
        range: ByteRange,
        matching: SyntaxMatch,
    ) -> Result<LocatedSyntax, RiftError> {
        if !self.parsed.contains_key(unit) {
            let parsed = self.parse(unit, range)?;
            self.parsed.insert(unit.clone(), parsed);
        }
        let located = self.parsed.get(unit).and_then(|parsed| {
            let declarations = parsed.declarations.as_slice();
            match &parsed.document {
                Some(document) => LocatedSyntax::in_document(
                    document,
                    parsed.source,
                    declarations,
                    range,
                    matching,
                ),
                None => LocatedSyntax::file(parsed.source, range, matching),
            }
        });
        located.ok_or_else(|| unresolved(unit, range))
    }

    fn parse(
        &self,
        unit: &SourceUnitId,
        range: ByteRange,
    ) -> Result<ParsedUnit<'publication>, RiftError> {
        let record = self
            .publication
            .units
            .iter()
            .find(|record| &record.unit == unit)
            .filter(|record| record.source_complete)
            .ok_or_else(|| unresolved(unit, range))?;
        let path = rift_core::ProjectPath::new(record.path.0.clone())?;
        let document = rift_syntax::registry::provider_for_language(&record.language)
            .map(|provider| {
                provider.analyze(
                    SyntaxSource {
                        path: &path,
                        text: &record.source,
                    },
                    self.limits,
                )
            })
            .transpose()?;
        let declarations = document
            .as_ref()
            .map(|document| self.declarations_of(unit, document))
            .transpose()?
            .unwrap_or_default();
        Ok(ParsedUnit {
            source: &record.source,
            document,
            declarations,
        })
    }

    /// The address of each declaration of `document`, matched to the publication's
    /// declaration whose range is the declaration's item range.
    fn declarations_of(
        &self,
        unit: &SourceUnitId,
        document: &SyntaxDocument,
    ) -> Result<Vec<Option<(rift_core::SymbolId, usize)>>, RiftError> {
        document
            .symbols()
            .iter()
            .map(|symbol| {
                let key = (
                    unit.0.as_str(),
                    symbol.item_range.start,
                    symbol.item_range.end,
                );
                self.addresses
                    .get(&key)
                    .map(|(identity, ordinal)| {
                        rift_core::SymbolId::new(identity.as_str()).map(|id| (id, *ordinal))
                    })
                    .transpose()
            })
            .collect()
    }
}

/// Adds one producer's facts to `publication`.
///
/// Every added edge and occurrence names `facts.producer`, and the coverage entry of
/// `facts.language` gains one entry per covered kind. An edge that names only its
/// `occurrence` starts at the symbol whose declaration encloses it; every range a fact names
/// must resolve to a syntax element of its unit. The coverage entry's `edges` stay as package
/// analysis wrote them: an imported kind states who emitted its facts, never that they are
/// complete.
///
/// The added edges follow the built-in ones, sorted by `from`, `to`, `kind`,
/// occurrence unit and range, `derivation`, `kinds`, then `confidence`, so the order the
/// producer emitted them in does not reach the publication's bytes. Occurrences are kept in
/// unit, range, and symbol order. The publication is checked whole with
/// [`PackagePublication::validate_facts`] before it is returned.
///
/// # Errors
/// Returns `producer_fact_invalid` for a `format` other than
/// [`PRODUCER_FACTS_FORMAT_REVISION`], more than [`PRODUCER_FACTS_MAX`] facts, a language
/// the publication has no coverage for, a fact of a kind `facts.kinds` does not name, a kind
/// already covered by this producer, an edge naming neither `from` nor `occurrence`,
/// or one naming only an `occurrence` outside every declaration; `producer_range_unresolved`
/// for a range that resolves to no syntax element.
pub fn accept_producer_facts(
    mut publication: PackagePublication,
    facts: ProducerFacts,
    limits: SyntaxLimits,
) -> Result<PackagePublication, RiftError> {
    let ProducerFacts {
        format,
        producer,
        language,
        kinds,
        edges,
        occurrences,
    } = facts;
    if format != PRODUCER_FACTS_FORMAT_REVISION {
        return Err(invalid("format", "unsupported"));
    }
    let (coverage, kinds) = admitted_kinds(&publication, &producer, &language, kinds)?;
    admit_counts(&kinds, edges.len(), occurrences.len())?;
    let (edges, occurrences) = {
        let mut syntax_elements = PublicationSyntax::new(&publication, limits);
        let edges = edges
            .into_iter()
            .map(|edge| resolve_edge(&mut syntax_elements, edge, &producer))
            .collect::<Result<Vec<_>, _>>()?;
        let occurrences = occurrences
            .into_iter()
            .map(|occurrence| resolve_occurrence(&mut syntax_elements, occurrence, &producer))
            .collect::<Result<Vec<_>, _>>()?;
        (edges, occurrences)
    };
    let built_in = publication.edges.len();
    publication.edges.extend(edges);
    publication.edges[built_in..].sort_by(canonical_order);
    publication.occurrences.extend(occurrences);
    publication.occurrences.sort_by(|left, right| {
        (&left.unit, left.range.start, left.range.end, &left.symbol).cmp(&(
            &right.unit,
            right.range.start,
            right.range.end,
            &right.symbol,
        ))
    });
    publication.coverage[coverage]
        .facts
        .extend(kinds.into_iter().map(|kind| FactCoverage {
            kind,
            producer: Some(producer.clone()),
        }));
    publication.validate_facts()?;
    Ok(publication)
}

/// The position of `language`'s coverage entry and the kinds `facts` covers, refused when
/// the publication has no coverage for `language` or the producer already covers one of the
/// kinds there.
fn admitted_kinds(
    publication: &PackagePublication,
    producer: &FactProducer,
    language: &rift_protocol::read::Language,
    kinds: Vec<FactKind>,
) -> Result<(usize, BTreeSet<FactKind>), RiftError> {
    let position = publication
        .coverage
        .iter()
        .position(|coverage| &coverage.language == language)
        .ok_or_else(|| invalid("language", "uncovered"))?;
    let coverage = &publication.coverage[position];
    let mut admitted = BTreeSet::new();
    for kind in kinds {
        if coverage.covers(kind, Some(producer)) || !admitted.insert(kind) {
            return Err(invalid("kinds", "duplicate"));
        }
    }
    Ok((position, admitted))
}

fn admit_counts(
    kinds: &BTreeSet<FactKind>,
    edges: usize,
    occurrences: usize,
) -> Result<(), RiftError> {
    let bound = usize::try_from(PRODUCER_FACTS_MAX).unwrap_or(usize::MAX);
    if edges.saturating_add(occurrences) > bound {
        return Err(invalid("facts", "length"));
    }
    let uncovered = (edges > 0 && !kinds.contains(&FactKind::Edges))
        || (occurrences > 0 && !kinds.contains(&FactKind::Occurrences));
    if uncovered {
        return Err(invalid("kinds", "uncovered"));
    }
    Ok(())
}

fn resolve_edge(
    syntax_elements: &mut PublicationSyntax<'_>,
    edge: ProducerEdge,
    producer: &FactProducer,
) -> Result<Edge, RiftError> {
    let ProducerEdge {
        from,
        kind,
        kinds,
        to,
        occurrence,
        provider,
        derivation,
        confidence,
        extensions,
    } = edge;
    let enclosing = occurrence
        .as_ref()
        .map(|span| {
            syntax_elements.syntax_element_at(&span.unit, byte_range(span), SyntaxMatch::Enclosing)
        })
        .transpose()?;
    let from = match (from, enclosing) {
        (Some(from), _) => from,
        (
            None,
            Some(LocatedSyntax {
                address: SyntaxAddress::Declaration { symbol, .. },
                ..
            }),
        ) => SymbolId(symbol.as_str().to_owned()),
        (None, Some(_)) => return Err(invalid("edges.occurrence", "outside_declaration")),
        (None, None) => return Err(invalid("edges.from", "missing")),
    };
    Ok(Edge {
        from,
        kind,
        kinds,
        to,
        occurrence,
        provider,
        derivation,
        confidence,
        producer: Some(producer.clone()),
        extensions,
    })
}

fn resolve_occurrence(
    syntax_elements: &mut PublicationSyntax<'_>,
    occurrence: ProducerOccurrence,
    producer: &FactProducer,
) -> Result<PackageOccurrence, RiftError> {
    let ProducerOccurrence {
        symbol,
        unit,
        range,
    } = occurrence;
    let span = SourceUnitSpan { unit, range };
    syntax_elements.syntax_element_at(&span.unit, byte_range(&span), SyntaxMatch::Enclosing)?;
    Ok(PackageOccurrence {
        symbol,
        unit: span.unit,
        range: span.range,
        producer: Some(producer.clone()),
    })
}

/// The order of one producer's imported edges. Two edges equal under it differ only
/// in their extensions and keep the order the producer wrote them.
fn canonical_order(left: &Edge, right: &Edge) -> Ordering {
    fn span(edge: &Edge) -> Option<(&SourceUnitId, u64, u64)> {
        edge.occurrence
            .as_ref()
            .map(|span| (&span.unit, span.range.start, span.range.end))
    }
    (&left.from, &left.to, &left.kind)
        .cmp(&(&right.from, &right.to, &right.kind))
        .then_with(|| span(left).cmp(&span(right)))
        .then_with(|| left.provider.cmp(&right.provider))
        .then_with(|| left.derivation.cmp(&right.derivation))
        .then_with(|| left.kinds.cmp(&right.kinds))
        .then_with(|| match (left.confidence, right.confidence) {
            (Some(left), Some(right)) => left.total_cmp(&right),
            (left, right) => left.is_some().cmp(&right.is_some()),
        })
}

fn byte_range(span: &SourceUnitSpan) -> ByteRange {
    ByteRange {
        start: span.range.start,
        end: span.range.end,
    }
}

fn invalid(field: &str, violation: &str) -> RiftError {
    errors::analysis::producer_fact_invalid()
        .field(field)
        .violation(violation)
        .error()
}

fn unresolved(unit: &SourceUnitId, range: ByteRange) -> RiftError {
    errors::analysis::producer_range_unresolved()
        .unit(unit.0.as_str())
        .start(range.start)
        .end(range.end)
        .error()
}

#[cfg(test)]
mod tests;
