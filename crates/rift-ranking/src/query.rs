//! Bounded query construction: one parser, two rendered phases.
//!
//! A caller's text never reaches `SQLite` FTS5 as written. It is parsed once
//! into quoted phrases and unquoted terms, bounded, deduplicated, and rendered
//! into an expression whose every member is a quoted string literal, so an
//! FTS5 operator a caller typed cannot become an operator the store executes.
//!
//! Two phases come out of one parse. The precise phase requires every member,
//! which answers a multi-term question exactly. The broad phase keeps every
//! phrase required and widens only the unquoted terms, which answers the same
//! question when the precise phase found too little. Every selected index runs
//! the precise phase first, so one index's broad hit cannot displace another
//! index's precise hit.

use crate::error::{RankingError, RankingViolation, refuse, refuse_over_limit};
use crate::identifier::{IdentifierCandidate, identifier_candidates};

/// Bytes a query must carry, at least.
pub const QUERY_BYTES_MIN: usize = 1;
/// Bytes a query may carry, at most.
pub const QUERY_BYTES_MAX: usize = 4_096;
/// Terms and quoted phrases one rendered phase may carry, at most.
pub const PARSED_QUERY_MEMBERS_MAX: usize = 32;
/// Bytes one term or quoted phrase may carry, at most.
pub const QUERY_TERM_BYTES_MAX: usize = 256;
/// Alphanumeric characters an unquoted term needs before it is rendered as a
/// prefix. Below this a prefix matches most of the corpus and ranks nothing.
pub const QUERY_PREFIX_ALPHANUMERIC_MIN: usize = 3;

/// The quote character that opens and closes a phrase in caller text, and the
/// one that delimits a string literal in a rendered FTS5 expression.
const QUOTE: char = '"';

/// One member of a parsed query.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QueryMember {
    /// An unquoted term. It may widen to a prefix and may be joined by `OR`.
    Term(String),
    /// A quoted phrase. It stays one phrase and stays required in both phases.
    Phrase(String),
}

impl QueryMember {
    /// The member's text, without quotes.
    #[must_use]
    pub fn text(&self) -> &str {
        match self {
            Self::Term(text) | Self::Phrase(text) => text,
        }
    }

    /// Whether the caller quoted this member.
    #[must_use]
    pub const fn is_phrase(&self) -> bool {
        matches!(self, Self::Phrase(_))
    }
}

/// Which expression one execution runs.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum QueryPhase {
    /// Every member required.
    Precise,
    /// Every phrase required, unquoted terms widened.
    Broad,
}

impl QueryPhase {
    /// The spelling a log field and a warning carry.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Precise => "precise",
            Self::Broad => "broad",
        }
    }
}

/// One caller query, parsed and bounded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedQuery {
    source: String,
    members: Vec<QueryMember>,
    narrowed: bool,
}

impl ParsedQuery {
    /// Parses one caller query.
    ///
    /// Quoted spans become phrases and everything else splits on
    /// non-alphanumeric boundaries, matching how the `unicode61` tokenizer
    /// divides the stored text. Members deduplicate case-insensitively,
    /// keeping the first spelling and source order.
    ///
    /// A query carrying more members than [`PARSED_QUERY_MEMBERS_MAX`]
    /// narrows rather than failing: every quoted phrase is kept, the longest
    /// unquoted terms fill the remaining slots with source order breaking
    /// ties, and source order is restored before rendering. The result says
    /// it narrowed, so an answer can report it.
    ///
    /// # Errors
    ///
    /// Returns [`RankingError`] naming `query` when the text is empty or past
    /// [`QUERY_BYTES_MAX`], when a quote opens and never closes, when one
    /// member runs past [`QUERY_TERM_BYTES_MAX`], or when the text carries
    /// more quoted phrases than [`PARSED_QUERY_MEMBERS_MAX`]: phrases cannot
    /// be dropped without changing what the caller asked for.
    pub fn parse(query: &str) -> Result<Self, RankingError> {
        if query.len() < QUERY_BYTES_MIN || query.len() > QUERY_BYTES_MAX {
            return Err(refuse_over_limit(
                RankingViolation::QueryLength,
                "query",
                "query",
                QUERY_BYTES_MAX,
                query.len(),
            ));
        }
        let scanned = deduplicate(scan(query)?);
        if let Some(refusal) = phrase_limit(&scanned) {
            return Err(refusal);
        }
        let members = narrow(scanned);
        Ok(Self {
            source: query.to_owned(),
            narrowed: members.narrowed,
            members: members.members,
        })
    }

    /// The members this query kept, in source order.
    #[must_use]
    pub fn members(&self) -> &[QueryMember] {
        &self.members
    }

    /// Whether the bound dropped an unquoted term the caller wrote.
    #[must_use]
    pub const fn is_narrowed(&self) -> bool {
        self.narrowed
    }

    /// Whether the query carries no member at all: all punctuation, or an
    /// empty phrase. Such a query matches nothing rather than refusing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// The unquoted terms, in source order.
    pub fn terms(&self) -> impl Iterator<Item = &str> {
        self.members
            .iter()
            .filter(|member| !member.is_phrase())
            .map(QueryMember::text)
    }

    /// The identifiers the caller's text carried, for the identifier ranking
    /// input and for locating a query term inside a source excerpt.
    ///
    /// Extraction reads the original text rather than the split members: a
    /// dotted name and a camel-case token are identifiers precisely because
    /// of the boundaries term splitting removes.
    #[must_use]
    pub fn candidates(&self) -> Vec<IdentifierCandidate> {
        identifier_candidates(&self.source)
    }

    /// Whether a broad phase exists.
    ///
    /// Widening needs at least two unquoted terms to widen between. A
    /// phrase-only query and a query with one unquoted term render the same
    /// expression in both phases, so the second execution is skipped rather
    /// than repeated.
    #[must_use]
    pub fn has_broad_phase(&self) -> bool {
        self.terms().count() >= 2
    }

    /// Renders one phase as an FTS5 `MATCH` expression, or `None` when the
    /// query carries no member.
    #[must_use]
    pub fn render(&self, phase: QueryPhase) -> Option<String> {
        if self.members.is_empty() {
            return None;
        }
        match phase {
            QueryPhase::Precise => Some(render_precise(&self.members)),
            QueryPhase::Broad => Some(render_broad(&self.members)),
        }
    }
}

/// Every member rendered as a required conjunct.
fn render_precise(members: &[QueryMember]) -> String {
    members
        .iter()
        .map(render_member)
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// Every phrase required, the unquoted terms widened into one alternation.
fn render_broad(members: &[QueryMember]) -> String {
    let mut conjuncts: Vec<String> = members
        .iter()
        .filter(|member| member.is_phrase())
        .map(render_member)
        .collect();
    let widened: Vec<String> = members
        .iter()
        .filter(|member| !member.is_phrase())
        .map(render_member)
        .collect();
    match widened.len() {
        0 => {}
        1 => conjuncts.extend(widened),
        _ => conjuncts.push(format!("({})", widened.join(" OR "))),
    }
    conjuncts.join(" AND ")
}

/// One member as an FTS5 string literal, widened to a prefix when it is an
/// unquoted term carrying enough alphanumeric characters to narrow anything.
fn render_member(member: &QueryMember) -> String {
    let literal = quote(member.text());
    match member {
        QueryMember::Term(text) if takes_prefix(text) => format!("{literal}*"),
        QueryMember::Phrase(_) | QueryMember::Term(_) => literal,
    }
}

/// Whether an unquoted term is long enough to widen to a prefix.
fn takes_prefix(term: &str) -> bool {
    term.chars()
        .filter(|character| character.is_alphanumeric())
        .count()
        >= QUERY_PREFIX_ALPHANUMERIC_MIN
}

/// One FTS5 string literal: the value in double quotes, with an embedded
/// double quote doubled, which is how FTS5 escapes one inside a literal.
fn quote(value: &str) -> String {
    let mut literal = String::with_capacity(value.len() + 2);
    literal.push(QUOTE);
    for character in value.chars() {
        if character == QUOTE {
            literal.push(QUOTE);
        }
        literal.push(character);
    }
    literal.push(QUOTE);
    literal
}

/// Splits caller text into quoted phrases and unquoted terms, refusing an
/// unterminated quote and an overlong member as it goes.
fn scan(query: &str) -> Result<Vec<QueryMember>, RankingError> {
    let mut members = Vec::new();
    let mut rest = query;
    while let Some(opening) = rest.find(QUOTE) {
        push_terms(&mut members, &rest[..opening])?;
        let after = &rest[opening + QUOTE.len_utf8()..];
        let Some(closing) = after.find(QUOTE) else {
            return Err(refuse(RankingViolation::QueryQuoteUnterminated, "query"));
        };
        let phrase = after[..closing].trim();
        if !phrase.is_empty() {
            members.push(QueryMember::Phrase(bounded(phrase)?.to_owned()));
        }
        rest = &after[closing + QUOTE.len_utf8()..];
    }
    push_terms(&mut members, rest)?;
    Ok(members)
}

/// Splits one unquoted span on non-alphanumeric boundaries, matching how the
/// stored text was tokenized, and appends every non-empty term.
fn push_terms(members: &mut Vec<QueryMember>, span: &str) -> Result<(), RankingError> {
    for term in span.split(|character: char| !character.is_alphanumeric()) {
        if term.is_empty() {
            continue;
        }
        members.push(QueryMember::Term(bounded(term)?.to_owned()));
    }
    Ok(())
}

/// Refuses one member past the byte bound, naming `query` as the parameter at
/// fault.
fn bounded(value: &str) -> Result<&str, RankingError> {
    if value.len() > QUERY_TERM_BYTES_MAX {
        return Err(refuse_over_limit(
            RankingViolation::QueryTermLength,
            "query",
            "query.term",
            QUERY_TERM_BYTES_MAX,
            value.len(),
        ));
    }
    Ok(value)
}

/// Drops a member whose lowercase text already appeared at the same kind,
/// keeping the first spelling and source order.
fn deduplicate(members: Vec<QueryMember>) -> Vec<QueryMember> {
    let mut seen: Vec<(bool, String)> = Vec::new();
    let mut kept = Vec::new();
    for member in members {
        let key = (member.is_phrase(), member.text().to_lowercase());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        kept.push(member);
    }
    kept
}

/// What narrowing produced.
struct Narrowed {
    members: Vec<QueryMember>,
    narrowed: bool,
}

/// Cuts a query to [`PARSED_QUERY_MEMBERS_MAX`] members.
///
/// Every quoted phrase survives; the longest unquoted terms fill what is left,
/// with source order breaking a length tie. Source order is restored before
/// rendering, so the expression reads in the order the caller wrote.
fn narrow(members: Vec<QueryMember>) -> Narrowed {
    if members.len() <= PARSED_QUERY_MEMBERS_MAX {
        return Narrowed {
            members,
            narrowed: false,
        };
    }
    let phrases = members.iter().filter(|member| member.is_phrase()).count();
    let slots = PARSED_QUERY_MEMBERS_MAX.saturating_sub(phrases);
    let mut ordered: Vec<(usize, &QueryMember)> = members
        .iter()
        .enumerate()
        .filter(|(_, member)| !member.is_phrase())
        .collect();
    ordered.sort_by(|left, right| {
        right
            .1
            .text()
            .len()
            .cmp(&left.1.text().len())
            .then(left.0.cmp(&right.0))
    });
    ordered.truncate(slots);
    let mut retained: Vec<usize> = ordered.into_iter().map(|(position, _)| position).collect();
    retained.sort_unstable();
    let kept = members
        .into_iter()
        .enumerate()
        .filter(|(position, member)| member.is_phrase() || retained.binary_search(position).is_ok())
        .map(|(_, member)| member)
        .collect();
    Narrowed {
        members: kept,
        narrowed: true,
    }
}

/// Refuses a query carrying more quoted phrases than one phase accepts.
///
/// Unquoted terms narrow, so a long question is answered rather than refused.
/// A phrase cannot narrow the same way: dropping one changes what the caller
/// asked for, and keeping a partial set would answer a question nobody asked.
fn phrase_limit(members: &[QueryMember]) -> Option<RankingError> {
    let phrases = members.iter().filter(|member| member.is_phrase()).count();
    (phrases > PARSED_QUERY_MEMBERS_MAX).then(|| {
        refuse_over_limit(
            RankingViolation::QueryPhraseLimit,
            "query",
            "query.phrases",
            PARSED_QUERY_MEMBERS_MAX,
            phrases,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{
        PARSED_QUERY_MEMBERS_MAX, ParsedQuery, QUERY_BYTES_MAX, QUERY_TERM_BYTES_MAX, QueryMember,
        QueryPhase, quote, takes_prefix,
    };
    use crate::error::RankingViolation;

    fn parsed(query: &str) -> ParsedQuery {
        ParsedQuery::parse(query).expect("query must parse")
    }

    fn violation(query: &str) -> RankingViolation {
        ParsedQuery::parse(query)
            .expect_err("query must be refused")
            .fault()
            .violation()
    }

    fn texts(query: &ParsedQuery) -> Vec<&str> {
        query.members().iter().map(QueryMember::text).collect()
    }

    #[test]
    fn test_an_empty_query_is_refused_for_its_length() {
        assert_eq!(violation(""), RankingViolation::QueryLength);
    }

    #[test]
    fn test_a_query_past_the_byte_bound_is_refused() {
        let query = "a".repeat(QUERY_BYTES_MAX + 1);
        assert_eq!(violation(&query), RankingViolation::QueryLength);
    }

    #[test]
    fn test_a_query_at_the_byte_bound_is_accepted() {
        let query = format!("{}c", "ab ".repeat((QUERY_BYTES_MAX - 1) / 3));
        assert_eq!(query.len(), QUERY_BYTES_MAX);
        assert_eq!(texts(&parsed(&query)), ["ab", "c"]);
    }

    #[test]
    fn test_an_unterminated_quote_is_refused() {
        assert_eq!(
            violation("\"impact radius"),
            RankingViolation::QueryQuoteUnterminated
        );
    }

    #[test]
    fn test_a_term_past_the_term_bound_is_refused() {
        let query = "b".repeat(QUERY_TERM_BYTES_MAX + 1);
        assert_eq!(violation(&query), RankingViolation::QueryTermLength);
    }

    #[test]
    fn test_a_phrase_past_the_term_bound_is_refused() {
        let phrase = "b".repeat(QUERY_TERM_BYTES_MAX + 1);
        assert_eq!(
            violation(&format!("\"{phrase}\"")),
            RankingViolation::QueryTermLength
        );
    }

    #[test]
    fn test_more_quoted_phrases_than_the_member_bound_are_refused() {
        let query = (0..=PARSED_QUERY_MEMBERS_MAX)
            .map(|index| format!("\"phrase {index}\""))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(violation(&query), RankingViolation::QueryPhraseLimit);
    }

    #[test]
    fn test_terms_split_on_non_alphanumeric_boundaries() {
        assert_eq!(
            texts(&parsed("parse_config.file")),
            ["parse", "config", "file"]
        );
    }

    #[test]
    fn test_terms_deduplicate_case_insensitively_keeping_the_first_spelling() {
        let query = parsed("Graph graph GRAPH search");
        assert_eq!(texts(&query), ["Graph", "search"]);
    }

    #[test]
    fn test_a_phrase_and_a_term_of_one_spelling_stay_distinct() {
        let query = parsed("\"graph\" graph");
        assert_eq!(query.members().len(), 2);
        assert!(query.members()[0].is_phrase());
        assert!(!query.members()[1].is_phrase());
    }

    #[test]
    fn test_an_all_punctuation_query_carries_no_member_and_matches_nothing() {
        let query = parsed("--- ...");
        assert!(query.is_empty());
        assert_eq!(query.render(QueryPhase::Precise), None);
        assert_eq!(query.render(QueryPhase::Broad), None);
    }

    #[test]
    fn test_an_empty_phrase_carries_no_member() {
        assert!(parsed("\"\"").is_empty());
    }

    #[test]
    fn test_the_precise_phase_requires_every_member() {
        let query = parsed("\"impact radius\" graph search");
        assert_eq!(
            query.render(QueryPhase::Precise).as_deref(),
            Some("\"impact radius\" AND \"graph\"* AND \"search\"*")
        );
    }

    #[test]
    fn test_the_broad_phase_keeps_phrases_required_and_widens_terms() {
        let query = parsed("\"impact radius\" graph search");
        assert_eq!(
            query.render(QueryPhase::Broad).as_deref(),
            Some("\"impact radius\" AND (\"graph\"* OR \"search\"*)")
        );
    }

    #[test]
    fn test_a_short_term_is_not_widened_to_a_prefix() {
        assert!(!takes_prefix("ab"));
        assert!(takes_prefix("abc"));
        let query = parsed("ab search");
        assert_eq!(
            query.render(QueryPhase::Precise).as_deref(),
            Some("\"ab\" AND \"search\"*")
        );
    }

    #[test]
    fn test_a_phrase_only_query_has_no_broad_phase() {
        assert!(!parsed("\"impact radius\"").has_broad_phase());
    }

    #[test]
    fn test_a_single_term_query_has_no_broad_phase() {
        assert!(!parsed("search").has_broad_phase());
    }

    #[test]
    fn test_two_terms_have_a_broad_phase() {
        assert!(parsed("graph search").has_broad_phase());
    }

    #[test]
    fn test_one_widened_term_beside_a_phrase_renders_without_an_alternation() {
        let query = parsed("\"impact radius\" graph");
        assert_eq!(
            query.render(QueryPhase::Broad).as_deref(),
            Some("\"impact radius\" AND \"graph\"*")
        );
    }

    #[test]
    fn test_an_fts_operator_typed_as_a_term_is_rendered_as_a_literal() {
        let query = parsed("AND OR NOT NEAR");
        assert_eq!(
            query.render(QueryPhase::Precise).as_deref(),
            Some("\"AND\"* AND \"OR\" AND \"NOT\"* AND \"NEAR\"*")
        );
    }

    #[test]
    fn test_a_quote_inside_a_phrase_is_doubled_in_the_literal() {
        assert_eq!(quote("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn test_narrowing_keeps_every_phrase_and_the_longest_terms_in_source_order() {
        let mut written = vec!["\"held phrase\"".to_owned(), "zz".to_owned()];
        for index in 0..PARSED_QUERY_MEMBERS_MAX + 8 {
            written.push(format!("term{index:02}"));
        }
        let query = parsed(&written.join(" "));
        assert!(query.is_narrowed());
        assert_eq!(query.members().len(), PARSED_QUERY_MEMBERS_MAX);
        assert!(query.members()[0].is_phrase());
        assert_eq!(query.members()[1].text(), "term00");
        assert_eq!(
            query.members()[PARSED_QUERY_MEMBERS_MAX - 1].text(),
            "term30"
        );
        assert!(
            !texts(&query).contains(&"zz"),
            "the shortest term is the one the bound drops"
        );
    }

    #[test]
    fn test_a_query_at_the_member_bound_does_not_narrow() {
        let written = (0..PARSED_QUERY_MEMBERS_MAX)
            .map(|index| format!("t{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        let query = parsed(&written);
        assert!(!query.is_narrowed());
        assert_eq!(query.members().len(), PARSED_QUERY_MEMBERS_MAX);
    }

    #[test]
    fn test_the_terms_iterator_omits_quoted_phrases() {
        let query = parsed("\"impact radius\" graph");
        assert_eq!(query.terms().collect::<Vec<_>>(), ["graph"]);
    }

    #[test]
    fn test_candidates_read_the_original_text_rather_than_the_split_terms() {
        let query = parsed("where does rift_core::SourceUnitId come from");
        let candidates = query.candidates();
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.text() == "sourceunitid"),
            "the dotted name's final segment must reach the identifier ranking: {candidates:?}"
        );
    }

    #[test]
    fn test_the_phase_label_names_each_phase() {
        assert_eq!(QueryPhase::Precise.label(), "precise");
        assert_eq!(QueryPhase::Broad.label(), "broad");
    }
}
