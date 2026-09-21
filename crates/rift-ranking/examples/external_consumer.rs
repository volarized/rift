//! A consumer that builds the shared retrieval surface with no storage and no
//! embedding runtime.
//!
//! The example exists to be compiled, not run: it imports every decision a
//! `PostgreSQL` or REST adapter needs - documents, bounded query construction,
//! identifier ranking, and rank fusion - and proves those decisions are
//! reachable without `SQLite`, Candle, a model download, MCP, or server state.
//! An adapter that consumes ordered identities therefore copies no algorithm.

use rift_ranking::{
    DocumentFields, DocumentIdentity, DocumentKind, DocumentLocation, IndexDocument, IndexReader,
    MemoryIndex, ParsedQuery, QueryPhase, RankedCandidates, RankingInput, RankingInputKind,
    RankingWeights, SearchableField, fuse, identifier_terms,
};

/// Publishes one declaration as this crate's document shape.
fn published(identity: &str, name: &str, qualified_name: &str) -> IndexDocument {
    let fields = DocumentFields::empty()
        .with(SearchableField::Name, name)
        .with(SearchableField::QualifiedName, qualified_name)
        .with(
            SearchableField::IdentifierTerms,
            identifier_terms([name, qualified_name], 256),
        );
    IndexDocument::new(
        DocumentIdentity::new(identity).expect("identity must be accepted"),
        DocumentLocation::Unit(
            rift_core::SourceUnitId::parse("rift://source/cargo/helper%400.1.0%2Fsrc%2Flib.rs")
                .expect("unit must parse"),
        ),
        DocumentKind::Symbol,
        "0f1e2d3c",
        fields,
    )
    .expect("document must be accepted")
}

/// Ranks one query over an adapter's own documents and returns ordered
/// identities the adapter resolves afterwards.
fn ranked(documents: Vec<IndexDocument>, query: &str) -> RankedCandidates {
    let parsed = ParsedQuery::parse(query).expect("query must parse");
    let index = MemoryIndex::new(documents, "analyzer-0");
    let identifier = RankingInput::new(RankingInputKind::Identifier, Vec::new());
    let weights = RankingWeights::new(0.35, 0.35, 0.30, 60).expect("weights must be accepted");
    let _ = index.capabilities();
    let _ = parsed.render(QueryPhase::Precise);
    fuse(&[identifier], weights, QueryPhase::Precise, 20)
}

fn main() {
    let documents = vec![published(
        "rift://source/cargo/helper%400.1.0%2Fsrc%2Flib.rs#SearchHit",
        "SearchHit",
        "search::SearchHit",
    )];
    let answer = ranked(documents, "SearchHit");
    println!("candidates: {}", answer.len());
}
