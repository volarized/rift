//! Hybrid lexical and semantic search for Rift.
//!
//! The lexical tier answers a query that shares a token with the code. A query
//! that shares none - a description of what a declaration does, in the words a
//! report would use - reaches nothing lexical, and no amount of ranking repairs
//! that: the words are not there. An embedding puts the query and the
//! declaration in one space, so a paraphrase can find code it has no word in
//! common with.
//!
//! The two tiers are fused, never substituted. Whenever the caller quotes a real
//! name - a symbol, a config key, a message from a traceback - the lexical tier
//! is the stronger side, and the semantic tier is measurably worse at exactly
//! that.

mod acquisition;
mod document;
mod encoder;
mod error;
mod fusion;
mod index;
mod similarity;

pub use acquisition::{AcquisitionLimits, FetchedFile, ModelSource, acquire};
pub use document::{
    DOCUMENT_SOURCE_BYTES_MAX, Declaration, Document, DocumentDigest, digests, document,
};
pub use encoder::{Encoder, EncoderLimits, ModelFiles};
pub use error::{SearchError, SearchFault, SearchViolation};
pub use fusion::{DeclarationMatch, FusedRank, Ranking, best_per_file, fuse, spread_per_file};
pub use index::{
    DescribedUnit, RankedUnit, SearchIndex, SearchIndexLimits, SearchIndexLimitsBuilder,
    SemanticReadiness,
};
pub use similarity::{SemanticMatch, nearest};

/// Compile-time marker for search-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchLayer;
