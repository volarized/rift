//! A consumer that builds the shared retrieval surface with no storage and no
//! embedding runtime.
//!
//! The example imports every decision a `PostgreSQL` or REST adapter needs -
//! documents, bounded query construction, identifier ranking, the reader
//! contract, and rank fusion - and runs them end to end, so an adapter that
//! consumes ordered identities copies no algorithm. It reaches them through
//! this crate's public surface alone, which is the half a compile proves; the
//! architecture check proves the other half, that the surface pulls in no
//! `SQLite`, Candle, model download, MCP, or server state.

use rift_ranking::{
    DocumentFields, DocumentIdentity, DocumentKind, DocumentLocation, IdentifierRanking,
    IndexDocument, IndexReader, MemoryIndex, ParsedQuery, QueryPhase, RankRequest,
    RankedCandidates, RankingInput, RankingInputKind, RankingWeights, SearchableField, fuse,
    identifier_terms, match_class,
};

/// Candidates one answer keeps.
const KEEP_MAX: usize = 20;
/// Bytes of derived terms one document carries.
const TERM_BYTES_MAX: usize = 256;

/// Publishes one declaration as this crate's document shape.
fn published(identity: &str, name: &str, qualified_name: &str) -> IndexDocument {
    let fields = DocumentFields::empty()
        .with(SearchableField::Name, name)
        .with(SearchableField::QualifiedName, qualified_name)
        .with(
            SearchableField::IdentifierTerms,
            identifier_terms([name, qualified_name], TERM_BYTES_MAX),
        );
    IndexDocument::new(
        DocumentIdentity::new(identity).expect("identity must be accepted"),
        DocumentLocation::Unit(
            rift_core::SourceUnitId::parse("rift://source/cargo/helper@0.1.0/src/lib.rs")
                .expect("unit must parse"),
        ),
        DocumentKind::Symbol,
        "0f1e2d3c",
        fields,
    )
    .expect("document must be accepted")
}

/// The identifier ranking an adapter derives from the names it holds.
fn identifier_input(documents: &[IndexDocument], query: &ParsedQuery) -> RankingInput {
    let candidates = query.candidates();
    let mut ranking = IdentifierRanking::new();
    for document in documents {
        let Some(name) = document.fields().get(SearchableField::Name) else {
            continue;
        };
        let name = name.to_lowercase();
        let qualified_name = document
            .fields()
            .get(SearchableField::QualifiedName)
            .unwrap_or(&name)
            .to_lowercase();
        for candidate in &candidates {
            if let Some(class) = match_class(candidate.text(), &name, &qualified_name) {
                ranking.observe(document.identity().clone(), class, candidate);
            }
        }
    }
    ranking.into_input(KEEP_MAX)
}

/// Ranks one query over an adapter's own documents and returns ordered
/// identities the adapter resolves afterwards.
async fn ranked(documents: Vec<IndexDocument>, query: &str) -> RankedCandidates {
    let parsed = ParsedQuery::parse(query).expect("query must parse");
    let identifier = identifier_input(&documents, &parsed);
    let index = MemoryIndex::new(documents, "analyzer-0");
    let request = RankRequest::new(
        &parsed,
        RankingInputKind::Lexical,
        QueryPhase::Precise,
        KEEP_MAX,
    );
    let lexical = index.rank(request).await.expect("the reader must answer");
    let weights = RankingWeights::new(0.35, 0.35, 0.30, 60).expect("weights must be accepted");
    fuse(
        &[identifier, lexical],
        weights,
        QueryPhase::Precise,
        KEEP_MAX,
    )
}

#[tokio::main]
async fn main() {
    let documents = vec![
        published(
            "rift://source/cargo/helper@0.1.0/src/lib.rs#SearchHit",
            "SearchHit",
            "search::SearchHit",
        ),
        published(
            "rift://source/cargo/helper@0.1.0/src/lib.rs#Cursor",
            "Cursor",
            "page::Cursor",
        ),
    ];
    let answer = ranked(documents, "SearchHit").await;
    let placed: Vec<&str> = answer
        .candidates()
        .iter()
        .map(|candidate| candidate.identity().as_str())
        .collect();
    assert_eq!(
        placed,
        ["rift://source/cargo/helper@0.1.0/src/lib.rs#SearchHit"],
        "the shared surface must answer the declaration the query names"
    );
    println!("candidates: {}", answer.len());
}
