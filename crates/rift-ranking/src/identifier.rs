//! Identifier splitting, the terms it derives, and the ranking a caller's
//! identifiers produce.
//!
//! Two jobs share one splitter. At publication time the splitter derives the
//! `identifier_terms` field, so `getUserName` is reachable by `user`. At query
//! time the same splitter decides whether a token buried in prose is an
//! identifier worth ranking, so "where is the `SearchHit` score set" reaches
//! `SearchHit` without the caller quoting it.
//!
//! The ranking this module produces is one input among several. It orders
//! identities by match class and never contributes a score that is later
//! merged with a fused one.

use std::collections::BTreeMap;

use crate::document::{DocumentIdentity, FieldSet, SearchableField};
use crate::fusion::{RankedIdentity, RankingInput, RankingInputKind};

/// Identifier candidates one query may contribute.
///
/// A question carries a handful of names at most. The bound stops a pathological
/// query from turning one search into a scan per token.
pub const IDENTIFIER_CANDIDATES_MAX: usize = 16;

/// Characters that continue an identifier token inside prose, beyond the
/// alphanumerics every language shares.
///
/// The colon is here for the qualifier a language spells `read::search`: a
/// caller naming a declaration by its qualified name must reach it, and
/// splitting the token at the separator would leave the container and the
/// name as two unrelated candidates.
const IDENTIFIER_JOINERS: [char; 4] = ['_', '.', '$', ':'];
/// The separators a qualified name is written with. A token carrying one is
/// the whole name, and its final segment is the short name inside it.
const QUALIFIER_SEPARATORS: [char; 2] = ['.', ':'];

/// How precisely a caller's identifier matched an indexed declaration.
///
/// Declaration order is precedence order: an identity keeps its best class.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum IdentifierMatchClass {
    /// The candidate equals the complete qualified name.
    QualifiedExact,
    /// The candidate equals the short declaration name.
    NameExact,
    /// The short declaration name starts with the candidate.
    NamePrefix,
    /// The qualified name carries the candidate somewhere else.
    Substring,
}

impl IdentifierMatchClass {
    /// The field the match was proved against, which the answer reports as the
    /// field that placed the hit.
    #[must_use]
    pub const fn field(self) -> SearchableField {
        match self {
            Self::QualifiedExact | Self::Substring => SearchableField::QualifiedName,
            Self::NameExact | Self::NamePrefix => SearchableField::Name,
        }
    }
}

/// Classifies one lowercase candidate against one declaration's names.
///
/// `name` and `qualified_name` arrive lowercased, as `candidate` does: a
/// caller writing `searchhit` reaches `SearchHit`, and the class is the same
/// whichever spelling was typed. Returns `None` when the declaration carries
/// the candidate nowhere.
#[must_use]
pub fn match_class(
    candidate: &str,
    name: &str,
    qualified_name: &str,
) -> Option<IdentifierMatchClass> {
    if qualified_name == candidate {
        return Some(IdentifierMatchClass::QualifiedExact);
    }
    if name == candidate {
        return Some(IdentifierMatchClass::NameExact);
    }
    if name.starts_with(candidate) {
        return Some(IdentifierMatchClass::NamePrefix);
    }
    qualified_name
        .contains(candidate)
        .then_some(IdentifierMatchClass::Substring)
}

/// One identifier a caller's query carried, and where it sat.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdentifierCandidate {
    text: String,
    position: usize,
}

impl IdentifierCandidate {
    /// The candidate, lowercased for comparison.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The candidate's place in the query, counting from the front. Two
    /// identities matching at one class order by the earlier candidate.
    #[must_use]
    pub const fn position(&self) -> usize {
        self.position
    }
}

/// Extracts the identifiers a query carries, best first, at most
/// [`IDENTIFIER_CANDIDATES_MAX`] of them.
///
/// Three shapes qualify, in this order:
///
/// 1. the whole query, when it is one identifier-shaped token;
/// 2. a qualified name embedded in prose, spelled with `.` or `::`, together
///    with its final segment;
/// 3. a token that splits into more than one word: snake case, camel case,
///    Pascal case, or an acronym run.
///
/// A plain prose word contributes nothing: it reaches the full-text ranking
/// through its own input, and adding it here would rank every declaration
/// whose qualified name happens to carry it.
#[must_use]
pub fn identifier_candidates(query: &str) -> Vec<IdentifierCandidate> {
    let mut candidates: Vec<IdentifierCandidate> = Vec::new();
    let trimmed = query.trim();
    if is_identifier_shaped(trimmed) {
        push_candidate(&mut candidates, trimmed);
    }
    for token in trimmed.split(|character: char| !is_identifier_character(character)) {
        if candidates.len() >= IDENTIFIER_CANDIDATES_MAX {
            break;
        }
        let token = token.trim_matches(|character| IDENTIFIER_JOINERS.contains(&character));
        if token.is_empty() {
            continue;
        }
        if token.contains(QUALIFIER_SEPARATORS) {
            push_candidate(&mut candidates, token);
            if let Some(segment) = token.rsplit(QUALIFIER_SEPARATORS).next() {
                push_candidate(&mut candidates, segment);
            }
            continue;
        }
        if split_identifier_words(token).len() > 1 {
            push_candidate(&mut candidates, token);
        }
    }
    candidates.truncate(IDENTIFIER_CANDIDATES_MAX);
    candidates
}

/// Appends one lowercased candidate, keeping the first position a spelling
/// appeared at.
fn push_candidate(candidates: &mut Vec<IdentifierCandidate>, value: &str) {
    let text = value.to_lowercase();
    if text.is_empty() || candidates.iter().any(|held| held.text == text) {
        return;
    }
    let position = candidates.len();
    candidates.push(IdentifierCandidate { text, position });
}

/// Whether the whole value is one identifier token: no whitespace, at least
/// one alphanumeric, and nothing outside the identifier character class.
fn is_identifier_shaped(value: &str) -> bool {
    !value.is_empty()
        && value.chars().any(char::is_alphanumeric)
        && value.chars().all(is_identifier_character)
}

/// Whether one character continues an identifier token.
fn is_identifier_character(character: char) -> bool {
    character.is_alphanumeric() || IDENTIFIER_JOINERS.contains(&character)
}

/// Splits an identifier into words on case boundaries and non-alphanumeric
/// separators, preserving each word's original casing.
///
/// Three boundaries cut a word: a separator, a lowercase or digit followed by
/// an uppercase letter (`getUser`), and an uppercase run whose last letter
/// starts a new word (`HTTPServer` splits into `HTTP` and `Server`). A digit
/// stays attached to the letters before it, so `utf8_decode` splits into
/// `utf8` and `decode`.
#[must_use]
pub fn split_identifier_words(name: &str) -> Vec<String> {
    let characters: Vec<char> = name.chars().collect();
    let mut words = Vec::new();
    let mut current = String::new();
    for (index, character) in characters.iter().copied().enumerate() {
        if !character.is_alphanumeric() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            continue;
        }
        if !current.is_empty() && starts_word(&characters, index) {
            words.push(std::mem::take(&mut current));
        }
        current.push(character);
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// Whether the character at `index` opens a new word inside a run of
/// alphanumerics.
fn starts_word(characters: &[char], index: usize) -> bool {
    let character = characters[index];
    if !character.is_uppercase() {
        return false;
    }
    let Some(previous) = index
        .checked_sub(1)
        .and_then(|before| characters.get(before))
    else {
        return false;
    };
    if !previous.is_alphanumeric() {
        return false;
    }
    if previous.is_lowercase() || previous.is_numeric() {
        return true;
    }
    characters
        .get(index + 1)
        .is_some_and(|next| next.is_lowercase())
}

/// Derives the `identifier_terms` field from the names a document carries.
///
/// Each source splits into words, every word lowercases, duplicates drop, and
/// a word equal to its own whole source drops as well: that spelling already
/// sits in `name` or `qualified_name`, and repeating it there and here would
/// count one term twice in the same document's length.
///
/// The result is bounded by `bytes_max`, cut at a word boundary so a partial
/// word never enters the corpus.
#[must_use]
pub fn identifier_terms<'a>(
    sources: impl IntoIterator<Item = &'a str>,
    bytes_max: usize,
) -> String {
    let mut terms: Vec<String> = Vec::new();
    for source in sources {
        let whole = source.to_lowercase();
        for word in split_identifier_words(source) {
            let word = word.to_lowercase();
            if word == whole || terms.contains(&word) {
                continue;
            }
            terms.push(word);
        }
    }
    let mut rendered = String::new();
    for term in terms {
        let added = if rendered.is_empty() {
            term.len()
        } else {
            term.len() + 1
        };
        if rendered.len() + added > bytes_max {
            break;
        }
        if !rendered.is_empty() {
            rendered.push(' ');
        }
        rendered.push_str(&term);
    }
    rendered
}

/// Accumulates one identifier ranking: the best class each identity reached,
/// the earliest candidate that reached it, and the field that proved it.
///
/// An identity that matched several candidates appears once. Ordering is by
/// class, then by the earliest candidate position, then by identity, so two
/// runs over the same corpus produce the same order.
#[derive(Clone, Debug, Default)]
pub struct IdentifierRanking {
    best: BTreeMap<DocumentIdentity, Placement>,
}

/// The best evidence one identity accumulated.
#[derive(Clone, Copy, Debug)]
struct Placement {
    class: IdentifierMatchClass,
    position: usize,
    fields: FieldSet,
}

impl IdentifierRanking {
    /// An empty ranking.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that `identity` matched `candidate` at `class`.
    ///
    /// A better class replaces what is held; an equal class keeps the earlier
    /// candidate. Either way the field that proved the match joins the
    /// identity's field set, because the answer reports every field that
    /// placed a hit.
    pub fn observe(
        &mut self,
        identity: DocumentIdentity,
        class: IdentifierMatchClass,
        candidate: &IdentifierCandidate,
    ) {
        let placement = Placement {
            class,
            position: candidate.position(),
            fields: FieldSet::of(class.field()),
        };
        self.best
            .entry(identity)
            .and_modify(|held| held.absorb(placement))
            .or_insert(placement);
    }

    /// Whether no identity matched.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.best.is_empty()
    }

    /// The ordered ranking, cut to `bound`.
    #[must_use]
    pub fn finish(self, bound: usize) -> Vec<RankedIdentity> {
        let mut placed: Vec<(DocumentIdentity, Placement)> = self.best.into_iter().collect();
        placed.sort_by(|left, right| {
            left.1
                .class
                .cmp(&right.1.class)
                .then(left.1.position.cmp(&right.1.position))
                .then(left.0.cmp(&right.0))
        });
        placed.truncate(bound);
        placed
            .into_iter()
            .map(|(identity, placement)| RankedIdentity::new(identity, placement.fields))
            .collect()
    }

    /// The ordered ranking as one fusion input.
    #[must_use]
    pub fn into_input(self, bound: usize) -> RankingInput {
        RankingInput::new(RankingInputKind::Identifier, self.finish(bound))
    }
}

impl Placement {
    /// Keeps the better of two placements: the stronger class, and at an equal
    /// class the earlier candidate. The field sets always union, since both
    /// fields genuinely proved a match.
    fn absorb(&mut self, other: Self) {
        self.fields = self.fields.union(other.fields);
        if other.class < self.class || (other.class == self.class && other.position < self.position)
        {
            self.class = other.class;
            self.position = other.position;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        IDENTIFIER_CANDIDATES_MAX, IdentifierMatchClass, IdentifierRanking, identifier_candidates,
        identifier_terms, match_class, split_identifier_words,
    };
    use crate::document::{DocumentIdentity, SearchableField};

    fn identity(value: &str) -> DocumentIdentity {
        DocumentIdentity::new(value).expect("identity must be accepted")
    }

    fn candidate_texts(query: &str) -> Vec<String> {
        identifier_candidates(query)
            .into_iter()
            .map(|candidate| candidate.text().to_owned())
            .collect()
    }

    #[test]
    fn test_camel_case_splits_at_the_case_boundary() {
        assert_eq!(
            split_identifier_words("getUserName"),
            ["get", "User", "Name"]
        );
    }

    #[test]
    fn test_pascal_case_splits_at_the_case_boundary() {
        assert_eq!(split_identifier_words("SearchHit"), ["Search", "Hit"]);
    }

    #[test]
    fn test_an_acronym_run_splits_before_the_word_it_opens() {
        assert_eq!(split_identifier_words("HTTPServer"), ["HTTP", "Server"]);
        assert_eq!(
            split_identifier_words("parseJSONValue"),
            ["parse", "JSON", "Value"]
        );
    }

    #[test]
    fn test_a_whole_acronym_stays_one_word() {
        assert_eq!(split_identifier_words("HTTP"), ["HTTP"]);
    }

    #[test]
    fn test_snake_case_splits_on_its_separators() {
        assert_eq!(
            split_identifier_words("parse_config_file"),
            ["parse", "config", "file"]
        );
    }

    #[test]
    fn test_screaming_snake_case_splits_on_its_separators() {
        assert_eq!(split_identifier_words("MAX_VALUE"), ["MAX", "VALUE"]);
    }

    #[test]
    fn test_a_digit_stays_attached_to_the_letters_before_it() {
        assert_eq!(split_identifier_words("utf8_decode"), ["utf8", "decode"]);
    }

    #[test]
    fn test_a_digit_before_an_uppercase_letter_opens_a_word() {
        assert_eq!(split_identifier_words("utf8Decode"), ["utf8", "Decode"]);
    }

    #[test]
    fn test_a_unicode_case_boundary_splits() {
        assert_eq!(
            split_identifier_words("caf\u{e9}Menu"),
            ["caf\u{e9}", "Menu"]
        );
    }

    #[test]
    fn test_a_single_word_stays_one_word() {
        assert_eq!(split_identifier_words("identifier"), ["identifier"]);
    }

    #[test]
    fn test_an_empty_name_splits_into_nothing() {
        assert!(split_identifier_words("").is_empty());
    }

    #[test]
    fn test_derived_terms_drop_the_whole_spelling_they_came_from() {
        assert_eq!(identifier_terms(["SearchHit"], 128), "search hit");
    }

    #[test]
    fn test_derived_terms_drop_a_single_word_name_entirely() {
        assert_eq!(identifier_terms(["beacon"], 128), "");
    }

    #[test]
    fn test_derived_terms_deduplicate_across_sources() {
        assert_eq!(
            identifier_terms(["SearchHit", "search::SearchHit"], 128),
            "search hit"
        );
    }

    #[test]
    fn test_derived_terms_stop_at_the_byte_bound_on_a_word_boundary() {
        assert_eq!(identifier_terms(["getUserName"], 8), "get user");
    }

    #[test]
    fn test_a_bare_identifier_query_is_its_own_candidate() {
        assert_eq!(candidate_texts("SearchHit"), ["searchhit"]);
    }

    #[test]
    fn test_a_dotted_name_in_prose_contributes_the_name_and_its_final_segment() {
        let candidates = candidate_texts("what sets index.lexical.weight here");
        assert!(candidates.contains(&"index.lexical.weight".to_owned()));
        assert!(candidates.contains(&"weight".to_owned()));
    }

    #[test]
    fn test_a_camel_case_token_in_prose_is_a_candidate() {
        assert_eq!(
            candidate_texts("where is getUserName called"),
            ["getusername"]
        );
    }

    #[test]
    fn test_a_snake_case_token_in_prose_is_a_candidate() {
        assert_eq!(candidate_texts("who calls parse_config"), ["parse_config"]);
    }

    #[test]
    fn test_a_plain_prose_word_is_no_candidate() {
        assert!(candidate_texts("where is the score set").is_empty());
    }

    #[test]
    fn test_candidates_are_bounded() {
        let query = (0..IDENTIFIER_CANDIDATES_MAX * 2)
            .map(|index| format!("someName{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(candidate_texts(&query).len(), IDENTIFIER_CANDIDATES_MAX);
    }

    #[test]
    fn test_a_repeated_spelling_contributes_one_candidate() {
        assert_eq!(candidate_texts("SearchHit and searchHit"), ["searchhit"]);
    }

    #[test]
    fn test_match_class_orders_qualified_exact_before_the_rest() {
        assert_eq!(
            match_class("read::search", "search", "read::search"),
            Some(IdentifierMatchClass::QualifiedExact)
        );
        assert_eq!(
            match_class("search", "search", "read::search"),
            Some(IdentifierMatchClass::NameExact)
        );
        assert_eq!(
            match_class("sea", "search", "read::search"),
            Some(IdentifierMatchClass::NamePrefix)
        );
        assert_eq!(
            match_class("read", "search", "read::search"),
            Some(IdentifierMatchClass::Substring)
        );
        assert_eq!(match_class("absent", "search", "read::search"), None);
    }

    #[test]
    fn test_each_match_class_names_the_field_that_proved_it() {
        assert_eq!(
            IdentifierMatchClass::QualifiedExact.field(),
            SearchableField::QualifiedName
        );
        assert_eq!(
            IdentifierMatchClass::NameExact.field(),
            SearchableField::Name
        );
        assert_eq!(
            IdentifierMatchClass::NamePrefix.field(),
            SearchableField::Name
        );
        assert_eq!(
            IdentifierMatchClass::Substring.field(),
            SearchableField::QualifiedName
        );
    }

    #[test]
    fn test_a_ranking_orders_by_class_then_position_then_identity() {
        let candidates = identifier_candidates("AlphaOne BetaTwo");
        let mut ranking = IdentifierRanking::new();
        ranking.observe(
            identity("c"),
            IdentifierMatchClass::NameExact,
            &candidates[1],
        );
        ranking.observe(
            identity("b"),
            IdentifierMatchClass::NameExact,
            &candidates[0],
        );
        ranking.observe(
            identity("a"),
            IdentifierMatchClass::Substring,
            &candidates[0],
        );
        ranking.observe(
            identity("d"),
            IdentifierMatchClass::NameExact,
            &candidates[0],
        );
        let ordered: Vec<String> = ranking
            .finish(10)
            .into_iter()
            .map(|entry| entry.identity().as_str().to_owned())
            .collect();
        assert_eq!(ordered, ["b", "d", "c", "a"]);
    }

    #[test]
    fn test_an_identity_keeps_its_best_class_and_unions_its_fields() {
        let candidates = identifier_candidates("AlphaOne BetaTwo");
        let mut ranking = IdentifierRanking::new();
        ranking.observe(
            identity("a"),
            IdentifierMatchClass::Substring,
            &candidates[1],
        );
        ranking.observe(
            identity("a"),
            IdentifierMatchClass::NameExact,
            &candidates[0],
        );
        let finished = ranking.finish(10);
        assert_eq!(finished.len(), 1);
        assert!(finished[0].fields().holds(SearchableField::Name));
        assert!(finished[0].fields().holds(SearchableField::QualifiedName));
    }

    #[test]
    fn test_a_ranking_cuts_to_its_bound() {
        let candidates = identifier_candidates("AlphaOne");
        let mut ranking = IdentifierRanking::new();
        for index in 0..5 {
            ranking.observe(
                identity(&format!("identity-{index}")),
                IdentifierMatchClass::NameExact,
                &candidates[0],
            );
        }
        assert_eq!(ranking.finish(2).len(), 2);
    }

    #[test]
    fn test_an_untouched_ranking_is_empty() {
        assert!(IdentifierRanking::new().is_empty());
    }
}
