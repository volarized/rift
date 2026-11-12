//! The contract a language-specific fact extractor implements.
//!
//! An extractor reads one language's source units of a package publication and writes the
//! [`ProducerFacts`] an external producer would. Rift ships no extractor: an implementation
//! lives outside this repository and wraps whatever program does the analysis. Rift owns the
//! contract alone, the input it hands over and the import of what comes back.

use rift_error::RiftError;
use rift_protocol::index::{PackagePublication, PackageSourceUnit};
use rift_protocol::producer::{FactProducer, ProducerFacts};
use rift_protocol::read::Language;
use rift_syntax::SyntaxLimits;

use super::{accept_producer_facts, invalid};

/// What an extractor reads: one language's source units of one publication.
///
/// Ranges an extractor writes are UTF-8 byte offsets into each unit's retained `source`.
#[derive(Clone, Copy, Debug)]
pub struct ExtractionInput<'publication> {
    publication: &'publication PackagePublication,
    language: &'publication Language,
}

impl<'publication> ExtractionInput<'publication> {
    /// The language whose units the extractor reads.
    #[must_use]
    pub fn language(&self) -> &'publication Language {
        self.language
    }

    /// The publication's units of [`Self::language`], in unit order. A unit whose
    /// `source_complete` is `false` holds cut source, and a range inside it does not import.
    pub fn units(
        &self,
    ) -> impl Iterator<Item = &'publication PackageSourceUnit> + use<'publication> {
        let language = self.language;
        self.publication
            .units
            .iter()
            .filter(move |unit| &unit.language == language)
    }

    /// The whole publication, for the declarations and symbols a fact names.
    #[must_use]
    pub fn publication(&self) -> &'publication PackagePublication {
        self.publication
    }
}

/// A language-specific program that extracts facts from a publication's source units.
///
/// An implementation names the producer and the language up front, so a caller can tell
/// which extractor covers which language before running any of them.
pub trait FactExtractor {
    /// The producer every fact this extractor writes names.
    fn producer(&self) -> &FactProducer;

    /// The language whose source this extractor reads.
    fn language(&self) -> &Language;

    /// Extracts the facts of [`Self::language`] from `input`.
    ///
    /// # Errors
    /// Returns the extractor's own refusal; [`accept_extracted_facts`] returns it unchanged.
    fn extract(&self, input: &ExtractionInput<'_>) -> Result<ProducerFacts, RiftError>;
}

/// Runs `extractor` over `publication` and imports its facts with [`accept_producer_facts`].
///
/// The extractor runs only when the publication covers its language, and its output must
/// name the extractor's own producer and language.
///
/// # Errors
/// Returns `producer_fact_invalid` naming `language` with `uncovered` before the extractor
/// runs when the publication has no coverage for its language, and naming `producer` or
/// `language` with `mismatch` when the output names another one; the extractor's refusal
/// unchanged; every refusal of [`accept_producer_facts`].
pub fn accept_extracted_facts<E: FactExtractor + ?Sized>(
    publication: PackagePublication,
    extractor: &E,
    limits: SyntaxLimits,
) -> Result<PackagePublication, RiftError> {
    let language = extractor.language();
    let covered = publication
        .coverage
        .iter()
        .any(|coverage| &coverage.language == language);
    if !covered {
        return Err(invalid("language", "uncovered"));
    }
    let input = ExtractionInput {
        publication: &publication,
        language,
    };
    let facts = extractor.extract(&input)?;
    if &facts.producer != extractor.producer() {
        return Err(invalid("producer", "mismatch"));
    }
    if &facts.language != language {
        return Err(invalid("language", "mismatch"));
    }
    accept_producer_facts(publication, facts, limits)
}
