//! An index that holds its documents in memory.
//!
//! This is the third reader the contract names, beside the project index and a
//! local package index. It ranks the same documents through the same BM25
//! weights without a database, which is what lets a test prove that one
//! publication produces one order through every adapter, and what gives the
//! later database-backed global reader a peer to be compared against.
//!
//! Tokenization matches the `unicode61` tokenizer the stored corpus is built
//! with: text folds to lowercase and splits on every non-alphanumeric
//! character. Diacritics are kept on both sides, which is why the corpus
//! declares `remove_diacritics 0` rather than the tokenizer default.

use std::collections::BTreeMap;

use crate::document::{DocumentFields, DocumentIdentity, FieldSet, IndexDocument, SearchableField};
use crate::error::RankingError;
use crate::fusion::{RankedIdentity, RankingInput, RankingInputKind, RankingInputSet};
use crate::identifier::{IdentifierRanking, match_class};
use crate::query::{ParsedQuery, QueryMember, QueryPhase};
use crate::reader::{IndexCapabilities, IndexReader, PublicationFormat, RankRequest, ReaderFuture};

/// The BM25 term-frequency saturation constant `SQLite` FTS5 uses.
const BM25_K1: f64 = 1.2;
/// The BM25 length-normalization constant `SQLite` FTS5 uses.
const BM25_B: f64 = 0.75;
/// The inverse document frequency FTS5 substitutes when the computed value
/// would be zero or negative, which happens once a term appears in more than
/// half the rows.
const BM25_IDF_FLOOR: f64 = 1e-6;

/// Splits text the way the corpus tokenizer does.
#[must_use]
pub fn tokenize(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// One document's tokens, per field.
#[derive(Clone, Debug, Default)]
struct TokenizedFields {
    per_field: BTreeMap<SearchableField, Vec<String>>,
    length: usize,
}

impl TokenizedFields {
    /// Tokenizes every field a document filled.
    fn of(fields: &DocumentFields) -> Self {
        let mut per_field = BTreeMap::new();
        let mut length = 0;
        for field in SearchableField::ALL {
            let Some(value) = fields.get(field) else {
                continue;
            };
            let tokens = tokenize(value);
            length += tokens.len();
            per_field.insert(field, tokens);
        }
        Self { per_field, length }
    }

    /// How often `member` occurs in `field`, counting a phrase by its
    /// occurrences as a consecutive run.
    fn frequency(&self, field: SearchableField, member: &Member) -> usize {
        let Some(tokens) = self.per_field.get(&field) else {
            return 0;
        };
        match member {
            Member::Term { token, prefix } => tokens
                .iter()
                .filter(|held| {
                    if *prefix {
                        held.starts_with(token)
                    } else {
                        *held == token
                    }
                })
                .count(),
            Member::Phrase(phrase) => phrase_occurrences(tokens, phrase),
        }
    }
}

/// How often `phrase` occurs as a consecutive run inside `tokens`.
fn phrase_occurrences(tokens: &[String], phrase: &[String]) -> usize {
    if phrase.is_empty() || phrase.len() > tokens.len() {
        return 0;
    }
    tokens
        .windows(phrase.len())
        .filter(|window| window == &phrase)
        .count()
}

/// One query member, tokenized for matching.
#[derive(Clone, Debug)]
enum Member {
    /// One token, matched whole or as a prefix.
    Term {
        /// The token, folded to lowercase.
        token: String,
        /// Whether the term widens to a prefix.
        prefix: bool,
    },
    /// A consecutive run of tokens.
    Phrase(Vec<String>),
}

impl Member {
    /// Tokenizes one parsed member.
    fn of(member: &QueryMember) -> Option<Self> {
        match member {
            QueryMember::Term(text) => {
                let mut tokens = tokenize(text);
                let token = tokens.pop()?;
                let prefix = text
                    .chars()
                    .filter(|character| character.is_alphanumeric())
                    .count()
                    >= crate::query::QUERY_PREFIX_ALPHANUMERIC_MIN;
                Some(Self::Term { token, prefix })
            }
            QueryMember::Phrase(text) => {
                let tokens = tokenize(text);
                (!tokens.is_empty()).then_some(Self::Phrase(tokens))
            }
        }
    }

    /// Whether this member is a quoted phrase, which stays required in both
    /// phases.
    const fn is_phrase(&self) -> bool {
        matches!(self, Self::Phrase(_))
    }
}

/// One document held in memory, with its tokens already derived.
#[derive(Clone, Debug)]
struct HeldDocument {
    document: IndexDocument,
    tokens: TokenizedFields,
    vector: Option<Vec<f32>>,
}

/// An index that answers from memory.
#[derive(Clone, Debug)]
pub struct MemoryIndex {
    held: Vec<HeldDocument>,
    average_length: f64,
    analyzer_revision: String,
}

impl MemoryIndex {
    /// Holds `documents`, deriving their tokens once.
    #[must_use]
    pub fn new(documents: Vec<IndexDocument>, analyzer_revision: impl Into<String>) -> Self {
        let held: Vec<HeldDocument> = documents
            .into_iter()
            .map(|document| {
                let tokens = TokenizedFields::of(document.fields());
                HeldDocument {
                    document,
                    tokens,
                    vector: None,
                }
            })
            .collect();
        let total: usize = held.iter().map(|entry| entry.tokens.length).sum();
        let average_length = if held.is_empty() {
            0.0
        } else {
            as_float(total) / as_float(held.len())
        };
        Self {
            held,
            average_length,
            analyzer_revision: analyzer_revision.into(),
        }
    }

    /// Attaches one document's vector, so the vector input can answer.
    ///
    /// A document this index does not hold is ignored: the caller's vector
    /// store and this index are published separately, and a vector with no
    /// document ranks nothing.
    pub fn attach_vector(&mut self, identity: &DocumentIdentity, vector: Vec<f32>) {
        if let Some(entry) = self
            .held
            .iter_mut()
            .find(|entry| entry.document.identity() == identity)
        {
            entry.vector = Some(vector);
        }
    }

    /// The documents this index holds.
    pub fn documents(&self) -> impl Iterator<Item = &IndexDocument> {
        self.held.iter().map(|entry| &entry.document)
    }

    /// Ranks the caller's identifiers against the held declarations.
    fn rank_identifiers(&self, query: &ParsedQuery, bound: usize) -> Vec<RankedIdentity> {
        let candidates = query.candidates();
        let mut ranking = IdentifierRanking::new();
        for entry in &self.held {
            let fields = entry.document.fields();
            let name = fields
                .get(SearchableField::Name)
                .unwrap_or_default()
                .to_lowercase();
            let qualified = fields
                .get(SearchableField::QualifiedName)
                .unwrap_or_default()
                .to_lowercase();
            for candidate in &candidates {
                if let Some(class) = match_class(candidate.text(), &name, &qualified) {
                    ranking.observe(entry.document.identity().clone(), class, candidate);
                }
            }
        }
        ranking.finish(bound)
    }

    /// The BM25 value this index computed for every document one phase kept,
    /// best first.
    ///
    /// A score is the arithmetic itself rather than a rank, so nothing outside
    /// a comparison between two adapters should read it. It exists because a
    /// reader that must match the stored corpus has to be able to see where
    /// the two disagree.
    #[must_use]
    pub fn scored(&self, query: &ParsedQuery, phase: QueryPhase) -> Vec<(DocumentIdentity, f64)> {
        let members: Vec<Member> = query.members().iter().filter_map(Member::of).collect();
        if members.is_empty() || self.held.is_empty() {
            return Vec::new();
        }
        let inverse = self.inverse_frequencies(&members);
        let mut scored: Vec<(DocumentIdentity, f64)> = self
            .held
            .iter()
            .filter_map(|entry| {
                let (score, _) = self.score(entry, &members, &inverse, phase)?;
                Some((entry.document.identity().clone(), score))
            })
            .collect();
        scored.sort_by(|left, right| right.1.total_cmp(&left.1).then(left.0.cmp(&right.0)));
        scored
    }

    /// Ranks the held documents by BM25 for one phase.
    fn rank_lexical(
        &self,
        query: &ParsedQuery,
        phase: QueryPhase,
        bound: usize,
    ) -> Vec<RankedIdentity> {
        let members: Vec<Member> = query.members().iter().filter_map(Member::of).collect();
        if members.is_empty() || self.held.is_empty() {
            return Vec::new();
        }
        let inverse = self.inverse_frequencies(&members);
        let mut scored: Vec<(f64, &DocumentIdentity, FieldSet)> = Vec::new();
        for entry in &self.held {
            let Some((score, fields)) = self.score(entry, &members, &inverse, phase) else {
                continue;
            };
            scored.push((score, entry.document.identity(), fields));
        }
        scored.sort_by(|left, right| right.0.total_cmp(&left.0).then(left.1.cmp(right.1)));
        scored.truncate(bound);
        scored
            .into_iter()
            .map(|(_, identity, fields)| RankedIdentity::new(identity.clone(), fields))
            .collect()
    }

    /// The inverse document frequency of every member, floored the way FTS5
    /// floors it.
    fn inverse_frequencies(&self, members: &[Member]) -> Vec<f64> {
        let total = as_float(self.held.len());
        members
            .iter()
            .map(|member| {
                let holding = as_float(
                    self.held
                        .iter()
                        .filter(|entry| Self::holds(entry, member))
                        .count(),
                );
                let value = ((total - holding + 0.5) / (holding + 0.5)).ln();
                if value > 0.0 { value } else { BM25_IDF_FLOOR }
            })
            .collect()
    }

    /// Whether one document carries `member` in any field.
    fn holds(entry: &HeldDocument, member: &Member) -> bool {
        SearchableField::ALL
            .into_iter()
            .any(|field| entry.tokens.frequency(field, member) > 0)
    }

    /// One document's BM25 score for `phase`, or `None` when the phase's own
    /// rule leaves the document out.
    ///
    /// The arithmetic follows FTS5's own `bm25` exactly: for each member the
    /// per-column occurrence counts are weighted and summed first, and the
    /// saturation applies once to that sum. The two forms coincide only where
    /// every column weight is one, so a corpus that exercises a single
    /// weight cannot tell them apart.
    fn score(
        &self,
        entry: &HeldDocument,
        members: &[Member],
        inverse: &[f64],
        phase: QueryPhase,
    ) -> Option<(f64, FieldSet)> {
        let mut matched = FieldSet::EMPTY;
        let mut score = 0.0;
        let mut terms_matched = 0;
        let mut terms_total = 0;
        for (member, idf) in members.iter().zip(inverse) {
            // FTS5 weights each column's occurrence count, sums those across the
            // columns, and saturates the sum once. Saturating per column and
            // weighting the result instead agrees only where every weight is one,
            // and diverges by the weight itself everywhere else.
            let mut weighted = 0.0;
            for field in SearchableField::ALL {
                let frequency = as_float(entry.tokens.frequency(field, member));
                if frequency <= 0.0 {
                    continue;
                }
                matched = matched.with(field);
                weighted += field.rank_weight() * frequency;
            }
            let member_matched = weighted > 0.0;
            if member_matched {
                score += idf * saturation(weighted, entry, self.average_length);
            }
            if member.is_phrase() {
                if !member_matched {
                    return None;
                }
                continue;
            }
            terms_total += 1;
            if member_matched {
                terms_matched += 1;
            } else if phase == QueryPhase::Precise {
                return None;
            }
        }
        if terms_total > 0 && terms_matched == 0 {
            return None;
        }
        Some((score, matched))
    }

    /// Ranks the held vectors against one query vector by cosine similarity.
    fn rank_vectors(&self, query: &[f32], bound: usize) -> Vec<RankedIdentity> {
        let mut scored: Vec<(f64, &DocumentIdentity)> = self
            .held
            .iter()
            .filter_map(|entry| {
                let vector = entry.vector.as_deref()?;
                Some((cosine(query, vector), entry.document.identity()))
            })
            .collect();
        scored.sort_by(|left, right| right.0.total_cmp(&left.0).then(left.1.cmp(right.1)));
        scored.truncate(bound);
        scored
            .into_iter()
            .map(|(_, identity)| {
                RankedIdentity::new(
                    identity.clone(),
                    FieldSet::of(SearchableField::DeclarationSource),
                )
            })
            .collect()
    }
}

/// The BM25 term-frequency saturation for one field occurrence count.
fn saturation(frequency: f64, entry: &HeldDocument, average_length: f64) -> f64 {
    let length = as_float(entry.tokens.length);
    let normalized = if average_length > 0.0 {
        length / average_length
    } else {
        1.0
    };
    (frequency * (BM25_K1 + 1.0)) / (frequency + BM25_K1 * (1.0 - BM25_B + BM25_B * normalized))
}

/// Cosine similarity of two vectors, zero when either has no magnitude or the
/// two are of different widths.
fn cosine(left: &[f32], right: &[f32]) -> f64 {
    if left.len() != right.len() {
        return 0.0;
    }
    let mut dot = 0.0_f64;
    let mut left_norm = 0.0_f64;
    let mut right_norm = 0.0_f64;
    for (one, other) in left.iter().zip(right) {
        dot += f64::from(*one) * f64::from(*other);
        left_norm += f64::from(*one) * f64::from(*one);
        right_norm += f64::from(*other) * f64::from(*other);
    }
    let magnitude = left_norm.sqrt() * right_norm.sqrt();
    if magnitude > 0.0 {
        dot / magnitude
    } else {
        0.0
    }
}

/// Widens a bounded in-memory count into the floating-point domain BM25 works
/// in. Every count here is bounded by the documents the caller published.
fn as_float(value: usize) -> f64 {
    f64::from(u32::try_from(value).unwrap_or(u32::MAX))
}

/// The query vector one request carries, set by a caller that embedded the
/// query itself. The in-memory index runs no model.
#[derive(Clone, Debug, Default)]
pub struct MemoryQueryVector(Option<Vec<f32>>);

impl MemoryQueryVector {
    /// Names the vector a caller embedded the query into.
    #[must_use]
    pub fn new(vector: Vec<f32>) -> Self {
        Self(Some(vector))
    }
}

impl IndexReader for MemoryIndex {
    fn capabilities(&self) -> IndexCapabilities {
        let available = self.held.iter().fold(FieldSet::EMPTY, |held, entry| {
            held.union(entry.document.fields().filled())
        });
        let mut inputs =
            RankingInputSet::of(RankingInputKind::Identifier).with(RankingInputKind::Lexical);
        if self.held.iter().any(|entry| entry.vector.is_some()) {
            inputs = inputs.with(RankingInputKind::Vector);
        }
        IndexCapabilities::new(
            PublicationFormat::CURRENT,
            self.analyzer_revision.clone(),
            crate::document::CorpusRevision::current(),
            available,
            inputs,
        )
    }

    fn rank<'a>(
        &'a self,
        request: RankRequest<'a>,
    ) -> ReaderFuture<'a, Result<RankingInput, RankingError>> {
        Box::pin(async move { Ok(self.ranked(request)) })
    }

    fn document<'a>(
        &'a self,
        identity: &'a DocumentIdentity,
    ) -> ReaderFuture<'a, Result<Option<IndexDocument>, RankingError>> {
        Box::pin(async move {
            Ok(self
                .held
                .iter()
                .find(|entry| entry.document.identity() == identity)
                .map(|entry| entry.document.clone()))
        })
    }
}

impl MemoryIndex {
    /// The ranking [`IndexReader::rank`] answers, computed without a runtime.
    ///
    /// This index reads no storage, so its answer needs no await. A caller
    /// already inside an async function reaches it through the contract; one
    /// that is not calls this and gets the same value.
    #[must_use]
    pub fn ranked(&self, request: RankRequest<'_>) -> RankingInput {
        let order = match request.input() {
            RankingInputKind::Identifier => self.rank_identifiers(request.query(), request.bound()),
            RankingInputKind::Lexical => {
                self.rank_lexical(request.query(), request.phase(), request.bound())
            }
            RankingInputKind::Vector => Vec::new(),
        };
        RankingInput::new(request.input(), order)
    }

    /// Ranks the held vectors against one embedded query, best first.
    ///
    /// The vector input is separate from [`IndexReader::rank`] because this
    /// index runs no model: a caller that embedded the query supplies the
    /// vector, and a caller that did not has no vector ranking at all.
    #[must_use]
    pub fn vector_input(&self, query: &MemoryQueryVector, bound: usize) -> RankingInput {
        match query.0.as_deref() {
            Some(vector) => {
                RankingInput::new(RankingInputKind::Vector, self.rank_vectors(vector, bound))
            }
            None => RankingInput::unanswered(RankingInputKind::Vector),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MemoryIndex, MemoryQueryVector, cosine, phrase_occurrences, tokenize};
    use crate::document::{
        DocumentFields, DocumentIdentity, DocumentKind, DocumentLocation, IndexDocument,
        SearchableField,
    };
    use crate::fusion::{RankingInputKind, RankingWeights, fuse};
    use crate::identifier::identifier_terms;
    use crate::query::{ParsedQuery, QueryPhase};
    use crate::reader::{IndexReader, PublicationFormat, RankRequest};
    use rift_core::ProjectPath;

    fn identity(value: &str) -> DocumentIdentity {
        DocumentIdentity::new(value).expect("identity must be accepted")
    }

    fn symbol(name: &str, qualified_name: &str, source: &str) -> IndexDocument {
        let fields = DocumentFields::empty()
            .with(SearchableField::Name, name)
            .with(SearchableField::QualifiedName, qualified_name)
            .with(
                SearchableField::IdentifierTerms,
                identifier_terms([name, qualified_name], 256),
            )
            .with(SearchableField::DeclarationSource, source);
        IndexDocument::new(
            identity(&format!("symbol:{qualified_name}")),
            DocumentLocation::Project(
                ProjectPath::new("src/lib.rs").expect("path must be accepted"),
            ),
            DocumentKind::Symbol,
            "0f1e2d3c",
            fields,
        )
        .expect("document must be accepted")
    }

    fn text_file(path: &str, content: &str) -> IndexDocument {
        let name = path.rsplit('/').next().unwrap_or(path);
        let fields = DocumentFields::empty()
            .with(SearchableField::Name, name)
            .with(SearchableField::FileContent, content);
        IndexDocument::new(
            identity(&format!("file:{path}")),
            DocumentLocation::Project(ProjectPath::new(path).expect("path must be accepted")),
            DocumentKind::TextFile,
            "1a2b3c4d",
            fields,
        )
        .expect("document must be accepted")
    }

    fn corpus() -> MemoryIndex {
        MemoryIndex::new(
            vec![
                symbol(
                    "SearchHit",
                    "read::SearchHit",
                    "pub struct SearchHit { score: f64 }",
                ),
                symbol("search", "read::search", "pub fn search(query: &str) {}"),
                symbol(
                    "impactRadius",
                    "graph::impactRadius",
                    "fn impactRadius() -> usize { 0 }",
                ),
                text_file("docs/vision.mdx", "the impact radius of one change"),
            ],
            "analyzer-0",
        )
    }

    async fn ranked(index: &MemoryIndex, query: &str, phase: QueryPhase) -> Vec<String> {
        let parsed = ParsedQuery::parse(query).expect("query must parse");
        let request = RankRequest::new(&parsed, RankingInputKind::Lexical, phase, 20);
        index
            .rank(request)
            .await
            .expect("the in-memory index never refuses")
            .order()
            .iter()
            .map(|entry| entry.identity().as_str().to_owned())
            .collect()
    }

    #[test]
    fn test_tokenizing_folds_case_and_splits_on_non_alphanumerics() {
        assert_eq!(tokenize("read::SearchHit"), ["read", "searchhit"]);
        assert_eq!(tokenize("  "), Vec::<String>::new());
        assert_eq!(tokenize("caf\u{e9}"), ["caf\u{e9}"]);
    }

    #[test]
    fn test_a_phrase_counts_only_consecutive_runs() {
        let tokens = tokenize("impact radius of impact radius");
        assert_eq!(phrase_occurrences(&tokens, &tokenize("impact radius")), 2);
        assert_eq!(phrase_occurrences(&tokens, &tokenize("radius impact")), 0);
        assert_eq!(phrase_occurrences(&tokens, &[]), 0);
        assert_eq!(
            phrase_occurrences(&tokenize("short"), &tokenize("much longer phrase")),
            0
        );
    }

    #[test]
    fn test_a_rarer_term_outscores_a_common_one_in_the_same_document() {
        // "alpha" sits in every document; "beta" sits in one. Both reach the same
        // document through the same field at the same frequency, so the only thing
        // that can separate them is the inverse document frequency.
        let documents: Vec<IndexDocument> = (0..4)
            .map(|index| {
                let content = if index == 0 { "alpha beta" } else { "alpha" };
                text_file(&format!("docs/{index}.md"), content)
            })
            .collect();
        let held = MemoryIndex::new(documents, "idf-fixture");
        let common = ParsedQuery::parse("alpha").expect("query must parse");
        let rare = ParsedQuery::parse("beta").expect("query must parse");
        let common_score = held.scored(&common, QueryPhase::Precise)[0].1;
        let rare_score = held.scored(&rare, QueryPhase::Precise)[0].1;
        assert!(
            rare_score > common_score,
            "a term three documents do not carry must outweigh one every document \
             carries: rare={rare_score}, common={common_score}"
        );
    }

    #[test]
    fn test_cosine_refuses_a_width_mismatch_and_a_zero_vector() {
        assert!(cosine(&[1.0, 0.0], &[1.0]).abs() < 1e-12);
        assert!(cosine(&[0.0, 0.0], &[1.0, 0.0]).abs() < 1e-12);
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-12);
    }

    #[tokio::test]
    async fn test_a_name_hit_outranks_a_source_hit() {
        let index = corpus();
        let order = ranked(&index, "SearchHit", QueryPhase::Precise).await;
        assert_eq!(
            order.first().map(String::as_str),
            Some("symbol:read::SearchHit")
        );
    }

    #[tokio::test]
    async fn test_the_precise_phase_requires_every_term() {
        let index = corpus();
        assert!(
            ranked(&index, "SearchHit impactRadius", QueryPhase::Precise)
                .await
                .is_empty(),
            "no document carries both names"
        );
    }

    #[tokio::test]
    async fn test_the_broad_phase_answers_what_the_precise_phase_could_not() {
        let index = corpus();
        let order = ranked(&index, "SearchHit impactRadius", QueryPhase::Broad).await;
        assert_eq!(order.len(), 2);
    }

    #[tokio::test]
    async fn test_a_quoted_phrase_stays_required_in_the_broad_phase() {
        let index = corpus();
        let order = ranked(&index, "\"impact radius\" beacon change", QueryPhase::Broad).await;
        assert_eq!(
            order,
            ["file:docs/vision.mdx"],
            "widening reaches one of the two terms, and the phrase still binds"
        );
        assert!(
            ranked(
                &index,
                "\"impact radius\" beacon change",
                QueryPhase::Precise
            )
            .await
            .is_empty(),
            "the precise phase still requires every term"
        );
    }

    #[tokio::test]
    async fn test_an_inner_case_split_term_reaches_its_declaration() {
        let index = corpus();
        let order = ranked(&index, "radius", QueryPhase::Precise).await;
        assert!(order.contains(&"symbol:graph::impactRadius".to_owned()));
    }

    #[tokio::test]
    async fn test_a_file_name_term_reaches_its_file() {
        let index = corpus();
        let order = ranked(&index, "vision", QueryPhase::Precise).await;
        assert_eq!(order, ["file:docs/vision.mdx"]);
    }

    #[tokio::test]
    async fn test_a_term_no_document_carries_ranks_nothing() {
        let index = corpus();
        assert!(
            ranked(&index, "absent", QueryPhase::Precise)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn test_an_empty_corpus_ranks_nothing() {
        let index = MemoryIndex::new(Vec::new(), "analyzer-0");
        assert!(
            ranked(&index, "search", QueryPhase::Precise)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn test_an_all_punctuation_query_ranks_nothing() {
        let index = corpus();
        assert!(ranked(&index, "---", QueryPhase::Precise).await.is_empty());
    }

    #[tokio::test]
    async fn test_the_identifier_input_orders_an_exact_name_first() {
        let index = corpus();
        let parsed = ParsedQuery::parse("where is SearchHit built").expect("query must parse");
        let request = RankRequest::new(
            &parsed,
            RankingInputKind::Identifier,
            QueryPhase::Precise,
            20,
        );
        let input = index.rank(request).await.expect("the index never refuses");
        assert_eq!(
            input.order().first().map(|entry| entry.identity().as_str()),
            Some("symbol:read::SearchHit")
        );
    }

    #[tokio::test]
    async fn test_the_vector_input_is_unanswered_until_a_caller_embeds_the_query() {
        let index = corpus();
        let parsed = ParsedQuery::parse("search").expect("query must parse");
        let request = RankRequest::new(&parsed, RankingInputKind::Vector, QueryPhase::Precise, 20);
        assert!(!index.rank(request).await.expect("no refusal").answered());
        assert!(
            !index
                .vector_input(&MemoryQueryVector::default(), 20)
                .answered()
        );
    }

    #[test]
    fn test_an_attached_vector_ranks_by_cosine_similarity() {
        let mut index = corpus();
        index.attach_vector(&identity("symbol:read::search"), vec![0.0, 1.0]);
        index.attach_vector(&identity("symbol:read::SearchHit"), vec![1.0, 0.0]);
        index.attach_vector(&identity("unheld"), vec![1.0, 1.0]);
        let input = index.vector_input(&MemoryQueryVector::new(vec![1.0, 0.0]), 20);
        let order: Vec<&str> = input
            .order()
            .iter()
            .map(|entry| entry.identity().as_str())
            .collect();
        assert_eq!(order, ["symbol:read::SearchHit", "symbol:read::search"]);
    }

    #[test]
    fn test_capabilities_report_the_filled_fields_and_runnable_inputs() {
        let mut index = corpus();
        let capabilities = index.capabilities();
        assert_eq!(
            capabilities.publication_format(),
            PublicationFormat::CURRENT
        );
        assert_eq!(capabilities.analyzer_revision(), "analyzer-0");
        assert!(capabilities.available_fields().holds(SearchableField::Name));
        assert!(
            !capabilities
                .available_fields()
                .holds(SearchableField::Signature)
        );
        assert!(
            !capabilities
                .ranking_inputs()
                .holds(RankingInputKind::Vector)
        );
        index.attach_vector(&identity("symbol:read::search"), vec![1.0]);
        assert!(
            index
                .capabilities()
                .ranking_inputs()
                .holds(RankingInputKind::Vector)
        );
    }

    #[tokio::test]
    async fn test_a_document_reads_back_by_its_identity() {
        let index = corpus();
        let held = identity("symbol:read::search");
        let found = index.document(&held).await.expect("no refusal");
        assert_eq!(
            found.map(|document| document.identity().clone()),
            Some(held)
        );
        assert!(
            index
                .document(&identity("absent"))
                .await
                .expect("no refusal")
                .is_none()
        );
        assert_eq!(index.documents().count(), 4);
    }

    #[tokio::test]
    async fn test_the_same_publication_ranks_alike_below_two_host_roots() {
        let index = corpus();
        let parsed = ParsedQuery::parse("search").expect("query must parse");
        let request = RankRequest::new(&parsed, RankingInputKind::Lexical, QueryPhase::Precise, 20);
        let first = index.rank(request).await.expect("no refusal");
        let cloned = MemoryIndex::new(index.documents().cloned().collect(), "analyzer-0");
        let request = RankRequest::new(&parsed, RankingInputKind::Lexical, QueryPhase::Precise, 20);
        let second = cloned.rank(request).await.expect("no refusal");
        assert_eq!(first.order(), second.order());
    }

    #[tokio::test]
    async fn test_two_inputs_fuse_into_one_order() {
        let index = corpus();
        let parsed = ParsedQuery::parse("SearchHit").expect("query must parse");
        let identifier = index
            .rank(RankRequest::new(
                &parsed,
                RankingInputKind::Identifier,
                QueryPhase::Precise,
                20,
            ))
            .await
            .expect("no refusal");
        let lexical = index
            .rank(RankRequest::new(
                &parsed,
                RankingInputKind::Lexical,
                QueryPhase::Precise,
                20,
            ))
            .await
            .expect("no refusal");
        let weights = RankingWeights::new(0.35, 0.35, 0.30, 60).expect("weights must be accepted");
        let fused = fuse(&[identifier, lexical], weights, QueryPhase::Precise, 20);
        assert_eq!(
            fused
                .candidates()
                .first()
                .map(|candidate| candidate.identity().as_str()),
            Some("symbol:read::SearchHit")
        );
        assert!(
            fused.candidates()[0]
                .inputs()
                .holds(RankingInputKind::Identifier)
        );
    }
}
