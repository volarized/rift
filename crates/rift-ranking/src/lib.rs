//! The retrieval decisions every Rift index shares: what a searchable document
//! holds, how a caller's query becomes bounded terms, how an identifier match
//! is classed, how ordered identities fuse into one answer, and which trigrams
//! a regex pattern requires of the text it matches.
//!
//! Nothing here opens a database, loads a model, or resolves a project path. A
//! reader hands this crate ordered identities and receives ordered identities
//! back, so the project index and a later database-backed global index rank
//! through one implementation instead of two that drift.
//!
//! The boundary is deliberate. Resolving an identity to a declaration needs the
//! store that published it; deciding which identities win does not.

mod body;
mod document;
mod fusion;
mod identifier;
mod pattern;
mod query;
mod reader;
mod tokenizer;
mod trigram;

pub use body::{BODY_MATCH_FILE_ROWS_MAX, BodyMatchPool, BodyTerms, FileRowFrequencies};
pub use document::{
    CORPUS_TOKENIZER, CorpusRevision, DOCUMENTATION_BYTES_MAX, DocumentFields, DocumentIdentity,
    DocumentKind, DocumentLocation, FieldSet, IDENTIFIER_TERMS_BYTES_MAX, IDENTITY_BYTES_MAX,
    IndexDocument, NAME_BYTES_MAX, SIGNATURE_BYTES_MAX, SearchableField,
};
pub use fusion::{
    FUSION_K_MAX, FUSION_K_MIN, FileRowAnswer, FusedCandidate, RankedCandidates, RankedIdentity,
    RankingInput, RankingInputKind, RankingInputSet, RankingWeights, fuse,
};
pub use identifier::{
    IDENTIFIER_CANDIDATES_MAX, IdentifierCandidate, IdentifierMatch, IdentifierMatchClass,
    IdentifierRanking, identifier_candidates, identifier_match, identifier_terms, match_class,
    split_identifier_words,
};
pub use pattern::{
    CLASS_EXPANSION_MAX, EXACT_STRINGS_MAX, Pattern, Prefilter, ROW_EXPRESSION_DEPTH_MAX, prefilter,
};
pub use query::{
    PARSED_QUERY_MEMBERS_MAX, ParsedQuery, QUERY_BYTES_MAX, QUERY_BYTES_MIN,
    QUERY_PREFIX_ALPHANUMERIC_MIN, QUERY_TERM_BYTES_MAX, QueryMember, QueryPhase,
};
pub use reader::{
    CapabilityMismatch, IndexCapabilities, IndexReader, PublicationFormat, RankRequest,
    ReaderFuture,
};
pub use tokenizer::tokenize;
pub use trigram::{TRIGRAM_TOKENIZER, fold, trigram_set, trigrams};

/// Compile-time marker for ranking-layer ownership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RankingLayer;
