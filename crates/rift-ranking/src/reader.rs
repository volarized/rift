//! The contract every Rift index answers under.
//!
//! One reader runs one ranking input for one query phase and returns ordered
//! identities. The project index, a local package index, an in-memory fixture,
//! and a later database-backed global index implement the same three methods,
//! so the coordination that runs precise before broad across selected indexes
//! is written once. Storage-specific rows stay behind the implementation.

use std::pin::Pin;

use crate::document::{CorpusRevision, DocumentIdentity, FieldSet, IndexDocument};
use crate::error::{RankingError, RankingFault, RankingViolation};
use crate::fusion::{RankingInput, RankingInputKind, RankingInputSet};
use crate::query::{ParsedQuery, QueryPhase};

/// A future a reader returns. Readers are asynchronous because some of them
/// hold a database; declaring the future here keeps the contract usable
/// through a trait object, which is what lets one request coordinate several
/// selected indexes.
pub type ReaderFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The shape a publication was written in.
///
/// The format changes when the stored columns, the identity spelling, or the
/// document shape change in a way an older reader cannot answer from. A
/// reader meeting another format refuses rather than ranking rows it would
/// read wrong.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PublicationFormat(u32);

impl PublicationFormat {
    /// The format this build publishes and reads.
    pub const CURRENT: Self = Self(1);

    /// Reads a format a store already holds.
    #[must_use]
    pub const fn stored(value: u32) -> Self {
        Self(value)
    }

    /// The format as stored and compared.
    #[must_use]
    pub const fn value(self) -> u32 {
        self.0
    }
}

/// What one index can answer, and what its rows mean.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexCapabilities {
    publication_format: PublicationFormat,
    analyzer_revision: String,
    corpus_revision: CorpusRevision,
    available_fields: FieldSet,
    ranking_inputs: RankingInputSet,
}

impl IndexCapabilities {
    /// States what one index publishes and which inputs it can run.
    #[must_use]
    pub fn new(
        publication_format: PublicationFormat,
        analyzer_revision: impl Into<String>,
        corpus_revision: CorpusRevision,
        available_fields: FieldSet,
        ranking_inputs: RankingInputSet,
    ) -> Self {
        Self {
            publication_format,
            analyzer_revision: analyzer_revision.into(),
            corpus_revision,
            available_fields,
            ranking_inputs,
        }
    }

    /// The shape this index's rows were written in.
    #[must_use]
    pub const fn publication_format(&self) -> PublicationFormat {
        self.publication_format
    }

    /// The analyzer revision that derived the documents.
    #[must_use]
    pub fn analyzer_revision(&self) -> &str {
        &self.analyzer_revision
    }

    /// The corpus revision the stored rows carry.
    #[must_use]
    pub const fn corpus_revision(&self) -> &CorpusRevision {
        &self.corpus_revision
    }

    /// The fields this index's documents actually filled.
    #[must_use]
    pub const fn available_fields(&self) -> FieldSet {
        self.available_fields
    }

    /// The ranking inputs this index can run.
    #[must_use]
    pub const fn ranking_inputs(&self) -> RankingInputSet {
        self.ranking_inputs
    }

    /// Whether this index can rank alongside `other`.
    ///
    /// A field `other` filled and this one did not is accepted: a provider
    /// that started publishing signatures extends the same corpus rather than
    /// defining another one. A different publication format or corpus
    /// revision is refused, because the two indexes' ranks would be derived
    /// from different column shapes or different weights and could not be
    /// fused.
    ///
    /// # Errors
    ///
    /// Returns [`RankingError`] naming the mismatch.
    pub fn accepts(&self, other: &Self) -> Result<(), RankingError> {
        if let Some(mismatch) = self.mismatch(other) {
            return Err(RankingError::new(
                RankingFault::new(RankingViolation::CapabilitiesIncompatible)
                    .about(mismatch.subject()),
            ));
        }
        Ok(())
    }

    /// Classifies the first incompatibility between two publications, or
    /// `None` when they can rank together.
    #[must_use]
    pub fn mismatch(&self, other: &Self) -> Option<CapabilityMismatch> {
        if self.publication_format != other.publication_format {
            return Some(CapabilityMismatch::PublicationFormat {
                held: self.publication_format,
                offered: other.publication_format,
            });
        }
        (self.corpus_revision != other.corpus_revision).then(|| {
            CapabilityMismatch::CorpusRevision {
                held: self.corpus_revision.clone(),
                offered: other.corpus_revision.clone(),
            }
        })
    }
}

/// Why two publications cannot rank together.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapabilityMismatch {
    /// The two were written in different publication formats.
    PublicationFormat {
        /// The format the reading index holds.
        held: PublicationFormat,
        /// The format the other publication carries.
        offered: PublicationFormat,
    },
    /// The two were built under different corpus revisions, so their columns,
    /// derivation, tokenizer, or weights differ.
    CorpusRevision {
        /// The revision the reading index holds.
        held: CorpusRevision,
        /// The revision the other publication carries.
        offered: CorpusRevision,
    },
}

impl CapabilityMismatch {
    /// The value the refusal names.
    #[must_use]
    pub fn subject(&self) -> String {
        match self {
            Self::PublicationFormat { held, offered } => {
                format!(
                    "publication_format held={} offered={}",
                    held.value(),
                    offered.value()
                )
            }
            Self::CorpusRevision { held, offered } => {
                format!("corpus_revision held={held} offered={offered}")
            }
        }
    }
}

/// One ranking request against one index.
#[derive(Clone, Copy, Debug)]
pub struct RankRequest<'a> {
    query: &'a ParsedQuery,
    input: RankingInputKind,
    phase: QueryPhase,
    bound: usize,
}

impl<'a> RankRequest<'a> {
    /// Names the query, the input to run, the phase to run it in, and the
    /// candidates the caller will keep.
    #[must_use]
    pub const fn new(
        query: &'a ParsedQuery,
        input: RankingInputKind,
        phase: QueryPhase,
        bound: usize,
    ) -> Self {
        Self {
            query,
            input,
            phase,
            bound,
        }
    }

    /// The parsed query every input reads.
    #[must_use]
    pub const fn query(&self) -> &'a ParsedQuery {
        self.query
    }

    /// Which input to run.
    #[must_use]
    pub const fn input(&self) -> RankingInputKind {
        self.input
    }

    /// Which phase to run it in.
    #[must_use]
    pub const fn phase(&self) -> QueryPhase {
        self.phase
    }

    /// Candidates the caller will keep from this input.
    #[must_use]
    pub const fn bound(&self) -> usize {
        self.bound
    }
}

/// One index a query phase can run against.
pub trait IndexReader: Send + Sync {
    /// What this index publishes and which inputs it can run.
    fn capabilities(&self) -> IndexCapabilities;

    /// Runs one ranking input for one phase, best first.
    ///
    /// An input this index does not carry answers with
    /// [`RankingInput::unanswered`] rather than refusing: a missing vector
    /// ranking must leave the identifier and full-text answers usable.
    fn rank<'a>(
        &'a self,
        request: RankRequest<'a>,
    ) -> ReaderFuture<'a, Result<RankingInput, RankingError>>;

    /// Reads one document by its stable identity, or `None` when this index
    /// does not hold it.
    fn document<'a>(
        &'a self,
        identity: &'a DocumentIdentity,
    ) -> ReaderFuture<'a, Result<Option<IndexDocument>, RankingError>>;
}

#[cfg(test)]
mod tests {
    use super::{CapabilityMismatch, IndexCapabilities, PublicationFormat};
    use crate::document::{CorpusRevision, FieldSet, SearchableField};
    use crate::error::RankingViolation;
    use crate::fusion::{RankingInputKind, RankingInputSet};

    fn capabilities(format: PublicationFormat, revision: CorpusRevision) -> IndexCapabilities {
        IndexCapabilities::new(
            format,
            "analyzer-0",
            revision,
            FieldSet::of(SearchableField::Name),
            RankingInputSet::of(RankingInputKind::Lexical),
        )
    }

    #[test]
    fn test_one_publication_format_round_trips_through_its_stored_value() {
        assert_eq!(PublicationFormat::stored(1), PublicationFormat::CURRENT);
        assert_eq!(PublicationFormat::CURRENT.value(), 1);
    }

    #[test]
    fn test_capabilities_report_what_they_were_built_with() {
        let held = capabilities(PublicationFormat::CURRENT, CorpusRevision::current());
        assert_eq!(held.publication_format(), PublicationFormat::CURRENT);
        assert_eq!(held.analyzer_revision(), "analyzer-0");
        assert_eq!(held.corpus_revision(), &CorpusRevision::current());
        assert!(held.available_fields().holds(SearchableField::Name));
        assert!(held.ranking_inputs().holds(RankingInputKind::Lexical));
    }

    #[test]
    fn test_a_newly_filled_field_is_accepted() {
        let held = capabilities(PublicationFormat::CURRENT, CorpusRevision::current());
        let richer = IndexCapabilities::new(
            PublicationFormat::CURRENT,
            "analyzer-1",
            CorpusRevision::current(),
            FieldSet::of(SearchableField::Name).with(SearchableField::Signature),
            RankingInputSet::of(RankingInputKind::Lexical),
        );
        assert_eq!(held.mismatch(&richer), None);
        assert!(held.accepts(&richer).is_ok());
    }

    #[test]
    fn test_another_publication_format_is_refused() {
        let held = capabilities(PublicationFormat::CURRENT, CorpusRevision::current());
        let other = capabilities(PublicationFormat::stored(2), CorpusRevision::current());
        assert_eq!(
            held.mismatch(&other),
            Some(CapabilityMismatch::PublicationFormat {
                held: PublicationFormat::CURRENT,
                offered: PublicationFormat::stored(2),
            })
        );
        assert_eq!(
            held.accepts(&other)
                .expect_err("another format must be refused")
                .fault()
                .violation(),
            RankingViolation::CapabilitiesIncompatible
        );
    }

    #[test]
    fn test_another_corpus_revision_is_refused() {
        let held = capabilities(PublicationFormat::CURRENT, CorpusRevision::current());
        let other = capabilities(
            PublicationFormat::CURRENT,
            CorpusRevision::stored("00000000"),
        );
        assert!(matches!(
            held.mismatch(&other),
            Some(CapabilityMismatch::CorpusRevision { .. })
        ));
        assert!(held.accepts(&other).is_err());
    }

    #[test]
    fn test_a_mismatch_names_the_two_values_it_compared() {
        let format = CapabilityMismatch::PublicationFormat {
            held: PublicationFormat::CURRENT,
            offered: PublicationFormat::stored(2),
        };
        assert_eq!(format.subject(), "publication_format held=1 offered=2");
        let revision = CapabilityMismatch::CorpusRevision {
            held: CorpusRevision::stored("aaaaaaaa"),
            offered: CorpusRevision::stored("bbbbbbbb"),
        };
        assert_eq!(
            revision.subject(),
            "corpus_revision held=aaaaaaaa offered=bbbbbbbb"
        );
    }
}
