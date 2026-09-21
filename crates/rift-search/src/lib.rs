//! Hybrid lexical and vector search for Rift.
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
//! is the stronger side, and the vector ranking is measurably worse at exactly
//! that.

mod acquisition;
mod document;
mod embedding;
mod encoder;
mod error;
mod fusion;
mod index;
mod similarity;

pub use acquisition::{AcquisitionLimits, FetchedFile, ModelSource, acquire};
pub use document::{
    DOCUMENT_SOURCE_BYTES_MAX, Declaration, Document, DocumentDigest, digests, document,
};
pub use embedding::{
    BatchSchedule, EmbeddingModels, EmbeddingSpace, LOCAL_INPUTS_MAX, LocalEncoder,
    REMOTE_INPUTS_MAX, RemoteEmbeddingSettings, RetrievalModels, RiftLocalDocumentModel,
    RiftLocalQueryModel, RiftOpenAiEmbeddingModel,
};
pub use encoder::{Encoder, EncoderLimits, ModelFiles};
pub use error::{SearchError, SearchFault, SearchViolation};
pub use fusion::{DeclarationMatch, best_per_file, spread_per_file};
pub use index::{
    DescribedUnit, Embedding, SearchIndex, SearchIndexLimits, SearchIndexLimitsBuilder,
    StoreRanking, VectorReadiness, local_embedding_space,
};
pub use rift_index::RevisionScoped;
pub use similarity::{VectorMatch, nearest};

/// Compile-time marker for search-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchLayer;
