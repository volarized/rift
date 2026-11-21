//! Agreement between a publication's facts and the coverage that states them.

use std::collections::{BTreeMap, BTreeSet};

use rift_error::{RiftError, errors};

use super::{PackageCoverage, PackageOccurrence, PackagePublication, PackageSourceUnit};
use crate::identity::SymbolIdentity;
use crate::index::StoredEdge;
use crate::producer::{FactKind, FactProducer};
use crate::read::Language;

/// Reason a publication's facts disagree with its coverage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FactViolation {
    /// Two coverage entries of one language name the same kind and producer.
    Duplicate,
    /// No coverage entry of the fact's language names its kind and producer.
    Uncovered,
    /// An occurrence names a unit the publication does not carry.
    Unit,
    /// An occurrence's range falls outside its unit's source.
    Range,
    /// Occurrences are not in strict unit, range, symbol, and producer order.
    Order,
    /// An edge starts at a symbol whose identity does not parse.
    Symbol,
}

impl FactViolation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Duplicate => "duplicate",
            Self::Uncovered => "uncovered",
            Self::Unit => "unit",
            Self::Range => "range",
            Self::Order => "order",
            Self::Symbol => "symbol",
        }
    }

    fn error(self, field: &str) -> RiftError {
        errors::analysis::producer_fact_invalid()
            .field(field)
            .violation(self.as_str())
            .error()
    }
}

impl PackagePublication {
    /// Checks every edge and occurrence against the coverage that states it.
    ///
    /// Each fact's language, kind, and producer must be named by one coverage entry, and
    /// each occurrence must lie inside a unit the publication carries, in unit, range, and
    /// symbol and producer order. The work is linear in the facts and units the publication holds.
    ///
    /// # Errors
    /// Returns `producer_fact_invalid` naming the field and the rule it breaks.
    pub fn validate_facts(&self) -> Result<(), RiftError> {
        validate_coverage(&self.coverage)?;
        let covered = |language: &Language, kind, producer: Option<&FactProducer>| {
            self.coverage
                .iter()
                .any(|coverage| &coverage.language == language && coverage.covers(kind, producer))
        };
        for edge in &self.edges {
            let language = edge_language(edge)?;
            if !covered(&language, FactKind::Edges, edge.producer.as_ref()) {
                return Err(FactViolation::Uncovered.error("edges"));
            }
        }
        let units = self
            .units
            .iter()
            .map(|unit| (&unit.unit, unit))
            .collect::<BTreeMap<_, _>>();
        for pair in self.occurrences.windows(2) {
            if occurrence_key(&pair[0]) >= occurrence_key(&pair[1]) {
                return Err(FactViolation::Order.error("occurrences"));
            }
        }
        for occurrence in &self.occurrences {
            let unit = units
                .get(&occurrence.unit)
                .ok_or_else(|| FactViolation::Unit.error("occurrences.unit"))?;
            if !range_inside(unit, occurrence) {
                return Err(FactViolation::Range.error("occurrences.range"));
            }
            if !covered(
                &unit.language,
                FactKind::Occurrences,
                occurrence.producer.as_ref(),
            ) {
                return Err(FactViolation::Uncovered.error("occurrences"));
            }
        }
        Ok(())
    }
}

fn validate_coverage(coverage: &[PackageCoverage]) -> Result<(), RiftError> {
    for language in coverage {
        let mut seen = BTreeSet::new();
        for fact in &language.facts {
            if !seen.insert((fact.kind, fact.producer.as_ref())) {
                return Err(FactViolation::Duplicate.error("coverage.facts"));
            }
        }
    }
    Ok(())
}

fn edge_language(edge: &StoredEdge) -> Result<Language, RiftError> {
    SymbolIdentity::parse(edge.from.as_str())
        .map(|identity| identity.language().clone())
        .map_err(|_| FactViolation::Symbol.error("edges.from"))
}

fn occurrence_key(occurrence: &PackageOccurrence) -> (&str, u64, u64, &str, Option<&FactProducer>) {
    (
        occurrence.unit.0.as_str(),
        occurrence.range.start,
        occurrence.range.end,
        occurrence.symbol.as_str(),
        occurrence.producer.as_ref(),
    )
}

/// Whether the occurrence's range is ordered and inside its unit. A unit whose retained
/// source was cut bounds the range by the bytes it kept.
fn range_inside(unit: &PackageSourceUnit, occurrence: &PackageOccurrence) -> bool {
    let range = &occurrence.range;
    let length = u64::try_from(unit.source.len()).unwrap_or(u64::MAX);
    range.start <= range.end && range.end <= length
}
