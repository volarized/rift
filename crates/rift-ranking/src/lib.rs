//! The retrieval decisions every Rift index shares: what a searchable document
//! holds, how a caller's query becomes bounded terms, how an identifier match
//! is classed, and how ordered identities fuse into one answer.
//!
//! Nothing here opens a database, loads a model, or resolves a project path. A
//! reader hands this crate ordered identities and receives ordered identities
//! back, so the project index, a local package index, an in-memory fixture, and
//! a later database-backed global index all rank through one implementation
//! instead of four that drift.
//!
//! The boundary is deliberate. Resolving an identity to a declaration needs the
//! store that published it; deciding which identities win does not.

mod document;
mod error;
mod fusion;
mod identifier;
mod memory;
mod query;
mod reader;

pub use document::{
    CORPUS_TOKENIZER, CorpusRevision, DOCUMENTATION_BYTES_MAX, DocumentFields, DocumentIdentity,
    DocumentKind, DocumentLocation, FieldSet, IDENTIFIER_TERMS_BYTES_MAX, IDENTITY_BYTES_MAX,
    IndexDocument, NAME_BYTES_MAX, SIGNATURE_BYTES_MAX, SearchableField,
};
pub use error::{RankingError, RankingFault, RankingViolation};
pub use fusion::{
    FUSION_K_MAX, FUSION_K_MIN, FusedCandidate, RankedCandidates, RankedIdentity, RankingInput,
    RankingInputKind, RankingInputSet, RankingWeights, fuse,
};
pub use identifier::{
    IDENTIFIER_CANDIDATES_MAX, IdentifierCandidate, IdentifierMatchClass, IdentifierRanking,
    identifier_candidates, identifier_terms, match_class, split_identifier_words,
};
pub use memory::{MemoryIndex, MemoryQueryVector, tokenize};
pub use query::{
    PARSED_QUERY_MEMBERS_MAX, ParsedQuery, QUERY_BYTES_MAX, QUERY_BYTES_MIN,
    QUERY_PREFIX_ALPHANUMERIC_MIN, QUERY_TERM_BYTES_MAX, QueryMember, QueryPhase,
};
pub use reader::{
    CapabilityMismatch, IndexCapabilities, IndexReader, PublicationFormat, RankRequest,
    ReaderFuture,
};

/// Compile-time marker for ranking-layer ownership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RankingLayer;
