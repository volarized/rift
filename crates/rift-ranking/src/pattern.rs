//! The regex query plan: which trigrams a text must hold for a pattern to match anywhere
//! in it, and the matcher that verifies a candidate.
//!
//! The plan walks the pattern's `regex-syntax` [`Hir`] into a formula of required
//! literals, and each literal requires its trigrams under the rule in [`crate::trigram`].
//! A formula only ever narrows to texts that could match: when a node's requirement
//! cannot be stated, the node requires nothing, and a pattern whose whole formula requires
//! nothing has no prefilter.
//!
//! Each node is read as one of two shapes. An exact node names every string it can
//! match, folded, up to [`EXACT_STRINGS_MAX`] strings: a literal, a small class, a `?` over
//! an exact node, a counted repetition of one, and a concatenation or alternation of exact
//! nodes. Any other node carries only a formula. Adjacent exact nodes join into one
//! string, so `(?i)websocket`, a concatenation of nine case classes, requires the trigrams
//! of `websocket`, and `export (async )?function` requires those of
//! `export async function ` or of `export function `.
//!
//! The walk recurses once per `Hir` level, and a parsed `Hir` is at most the parser's nest
//! limit deep (250 by default, `regex-syntax` `ast/parse.rs:141`). `regex-syntax`'s own
//! `Extractor` knows prefix and suffix literals alone and invites a custom extractor "for
//! n-grams or \"inner\" literals" (`hir/literal.rs:44-50`), which this walk is.

use std::collections::BTreeSet;
use std::ops::Range;

use regex_automata::Input;
use regex_automata::meta::Regex;
use regex_syntax::ParserBuilder;
use regex_syntax::hir::{
    Capture, Class, ClassBytes, ClassBytesRange, ClassUnicode, ClassUnicodeRange, Hir, HirKind,
    Repetition,
};

use crate::trigram::{fold, trigram_set};
use rift_error::{RiftError, errors};

/// Most distinct folded characters a class expands into before it requires nothing.
/// `['"]` and every case class of one letter fit; `[0-9a-f]` and `\w` do not.
pub const CLASS_EXPANSION_MAX: usize = 8;

/// Most class members read while expanding a class; a larger class requires nothing
/// without being read.
const CLASS_MEMBERS_MAX: u32 = 64;

/// Most strings one exact node names. Past it, a concatenation closes its current literal
/// and starts the next, and an alternation keeps a formula per branch.
pub const EXACT_STRINGS_MAX: usize = 16;

/// Most copies a counted repetition of an exact node expands into.
const REPETITION_EXPANSION_MAX: u32 = 16;

/// Most nested groups one row expression renders. FTS5's parser refuses an expression 33
/// groups deep ("fts5: parser stack overflow", `fts5YYSTACKDEPTH` 100), so a formula deeper
/// than this runs one `MATCH` per literal instead.
pub const ROW_EXPRESSION_DEPTH_MAX: usize = 16;

/// Bytes the lazy DFA's cache may grow to while one matcher verifies, the `regex` crate's
/// own default (`regex-1.13.1/src/builders.rs:54`).
const MATCHER_CACHE_BYTES: usize = 2 * (1 << 20);

/// A formula of required trigrams.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Prefilter {
    /// Every trigram of one newline-free literal. A literal never crosses a line, so one
    /// index row holds all of them: one FTS5 `MATCH` of the trigrams joined by `AND`.
    Literal(BTreeSet<String>),
    /// Every member's requirement.
    All(Vec<Prefilter>),
    /// At least one member's requirement.
    Any(Vec<Prefilter>),
}

impl Prefilter {
    /// Whether a text holding `trigrams` meets the formula.
    #[must_use]
    pub fn accepts(&self, trigrams: &BTreeSet<String>) -> bool {
        match self {
            Self::Literal(required) => required.is_subset(trigrams),
            Self::All(members) => members.iter().all(|member| member.accepts(trigrams)),
            Self::Any(members) => members.iter().any(|member| member.accepts(trigrams)),
        }
    }

    /// The FTS5 `MATCH` expression selecting the rows that meet the whole formula on their
    /// own: the rows that can hold a match sitting inside one row. `None` past
    /// [`ROW_EXPRESSION_DEPTH_MAX`] nested groups.
    #[must_use]
    pub fn row_expression(&self) -> Option<String> {
        (self.depth() <= ROW_EXPRESSION_DEPTH_MAX).then(|| self.rendered())
    }

    /// The FTS5 `MATCH` expression selecting the rows that hold every trigram of one
    /// literal: the trigrams, each quoted, joined by `AND`. `detail=none` refuses a phrase
    /// longer than one trigram, and it answers an `AND` of single trigrams.
    #[must_use]
    pub fn literal_expression(trigrams: &BTreeSet<String>) -> String {
        trigrams
            .iter()
            .map(|trigram| format!("\"{}\"", trigram.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" AND ")
    }

    fn rendered(&self) -> String {
        match self {
            Self::Literal(trigrams) => Self::literal_expression(trigrams),
            Self::All(members) => grouped(members, " AND "),
            Self::Any(members) => grouped(members, " OR "),
        }
    }

    fn depth(&self) -> usize {
        match self {
            Self::Literal(_) => 0,
            Self::All(members) | Self::Any(members) => {
                1 + members.iter().map(Self::depth).max().unwrap_or(0)
            }
        }
    }

    /// Every distinct literal the formula names, each one `MATCH` when the formula's
    /// members may sit in different rows of one file.
    #[must_use]
    pub fn literals(&self) -> BTreeSet<&BTreeSet<String>> {
        match self {
            Self::Literal(trigrams) => BTreeSet::from([trigrams]),
            Self::All(members) | Self::Any(members) => {
                members.iter().flat_map(Self::literals).collect()
            }
        }
    }

    /// Whether the formula holds when `holds` answers each literal: a file meets it when
    /// every `All` member and one `Any` member hold somewhere in the file.
    pub fn accepts_literals(&self, holds: &impl Fn(&BTreeSet<String>) -> bool) -> bool {
        match self {
            Self::Literal(trigrams) => holds(trigrams),
            Self::All(members) => members.iter().all(|member| member.accepts_literals(holds)),
            Self::Any(members) => members.iter().any(|member| member.accepts_literals(holds)),
        }
    }
}

fn grouped(members: &[Prefilter], operator: &str) -> String {
    members
        .iter()
        .map(|member| format!("({})", member.rendered()))
        .collect::<Vec<_>>()
        .join(operator)
}

/// The trigrams a text must hold for `hir` to match in it, or `None` when the pattern has
/// no prefilter and every text is a candidate.
///
/// `hir` comes from a translator in UTF-8 mode, as [`Pattern::parse`] builds it, so every
/// byte class in it holds ASCII alone and each of its bytes is one character of the text.
///
/// # Panics
///
/// In a debug build, when `hir` holds a byte class reaching past ASCII, which only a
/// translator with UTF-8 mode off produces.
#[must_use]
pub fn prefilter(hir: &Hir) -> Option<Prefilter> {
    requirement(shape(hir))
}

/// One regex search pattern: the matcher that verifies a candidate text, and the prefilter
/// that selects candidates.
///
/// The matcher reads the pattern as ripgrep does by default: `^` and `$` match at line
/// boundaries, `\r` is an ordinary character, and no class matches `\n`, so `\s+$` stops
/// at a line's end instead of taking the file's last newline. Only a literal `\n` in the
/// pattern crosses a line. Offsets stay those of the whole text.
#[derive(Clone, Debug)]
pub struct Pattern {
    matcher: Regex,
    prefilter: Option<Prefilter>,
    line_bound: bool,
}

impl Pattern {
    /// Parses `pattern` in the `regex` crate's syntax and compiles its matcher under
    /// `size_limit` bytes of compiled program.
    ///
    /// # Errors
    ///
    /// Returns a syntax error for a pattern that does not parse,
    /// carrying the parser's own message, and a size error when the
    /// compiled matcher passes `size_limit`.
    pub fn parse(pattern: &str, size_limit: usize) -> Result<Self, RiftError> {
        let parsed = ParserBuilder::new()
            .multi_line(true)
            .crlf(false)
            .build()
            .parse(pattern)
            .map_err(|error| {
                errors::ranking::pattern_syntax()
                    .subject(error.to_string())
                    .error()
            })?;
        let hir = without_line_feed_in_classes(&parsed);
        let matcher = Regex::builder()
            .configure(
                Regex::config()
                    .nfa_size_limit(Some(size_limit))
                    .hybrid_cache_capacity(MATCHER_CACHE_BYTES),
            )
            .build_from_hir(&hir)
            .map_err(|error| match error.size_limit() {
                Some(_) => errors::ranking::pattern_size()
                    .subject(format!("the compiled pattern exceeds {size_limit} bytes"))
                    .error(),
                None => errors::ranking::pattern_syntax()
                    .subject(error.to_string())
                    .error(),
            })?;
        Ok(Self {
            prefilter: prefilter(&hir),
            line_bound: !holds_line_feed(&hir),
            matcher,
        })
    }

    /// The trigrams a text must hold to match, or `None` when every text is a candidate.
    #[must_use]
    pub const fn prefilter(&self) -> Option<&Prefilter> {
        self.prefilter.as_ref()
    }

    /// Whether every match sits inside one line: the pattern holds no literal `\n`. A
    /// line-bound match sits inside one row of a chunked file, since a chunk packs whole
    /// lines, so the row the prefilter selected is the only span to verify.
    #[must_use]
    pub const fn is_line_bound(&self) -> bool {
        self.line_bound
    }

    /// The byte ranges of the matches starting inside `span` of `text`, in order. The
    /// ranges are offsets into `text`, and look-around reads the text outside `span`.
    pub fn matches<'m, 'h>(
        &'m self,
        text: &'h str,
        span: Range<usize>,
    ) -> impl Iterator<Item = Range<usize>> + use<'m, 'h> {
        self.matcher
            .find_iter(Input::new(text).span(span))
            .map(|found| found.range())
    }
}

/// `hir` with `\n` removed from every class, as ripgrep compiles a pattern for
/// line-oriented search. A class holding only `\n` becomes one that never matches.
fn without_line_feed_in_classes(hir: &Hir) -> Hir {
    match hir.kind() {
        HirKind::Class(Class::Unicode(class)) => {
            let mut class = class.clone();
            class.difference(&ClassUnicode::new([ClassUnicodeRange::new('\n', '\n')]));
            Hir::class(Class::Unicode(class))
        }
        HirKind::Class(Class::Bytes(class)) => {
            let mut class = class.clone();
            class.difference(&ClassBytes::new([ClassBytesRange::new(b'\n', b'\n')]));
            Hir::class(Class::Bytes(class))
        }
        HirKind::Repetition(repetition) => Hir::repetition(Repetition {
            sub: Box::new(without_line_feed_in_classes(&repetition.sub)),
            ..repetition.clone()
        }),
        HirKind::Capture(capture) => Hir::capture(Capture {
            sub: Box::new(without_line_feed_in_classes(&capture.sub)),
            ..capture.clone()
        }),
        HirKind::Concat(members) => {
            Hir::concat(members.iter().map(without_line_feed_in_classes).collect())
        }
        HirKind::Alternation(branches) => {
            Hir::alternation(branches.iter().map(without_line_feed_in_classes).collect())
        }
        HirKind::Empty | HirKind::Literal(_) | HirKind::Look(_) => hir.clone(),
    }
}

/// Whether a literal in `hir` holds `\n`; after [`without_line_feed_in_classes`] no class
/// does.
fn holds_line_feed(hir: &Hir) -> bool {
    match hir.kind() {
        HirKind::Literal(literal) => literal.0.contains(&b'\n'),
        HirKind::Repetition(repetition) => holds_line_feed(&repetition.sub),
        HirKind::Capture(capture) => holds_line_feed(&capture.sub),
        HirKind::Concat(members) | HirKind::Alternation(members) => {
            members.iter().any(holds_line_feed)
        }
        HirKind::Empty | HirKind::Class(_) | HirKind::Look(_) => false,
    }
}

/// What the walk knows about one node.
enum Shape {
    /// Every string the node can match, folded.
    Exact(BTreeSet<String>),
    /// A formula every match meets, or `None` when the node requires nothing.
    Required(Option<Prefilter>),
}

fn empty_string() -> BTreeSet<String> {
    BTreeSet::from([String::new()])
}

fn shape(hir: &Hir) -> Shape {
    match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => Shape::Exact(empty_string()),
        HirKind::Literal(literal) => match std::str::from_utf8(&literal.0) {
            Ok(text) => Shape::Exact(BTreeSet::from([text.chars().map(fold).collect()])),
            Err(_) => Shape::Required(None),
        },
        HirKind::Class(class) => class_shape(class),
        HirKind::Repetition(repetition) => repetition_shape(repetition),
        HirKind::Capture(capture) => shape(&capture.sub),
        HirKind::Concat(members) => concat_shape(members),
        HirKind::Alternation(branches) => alternation_shape(branches),
    }
}

/// A class's members, or `None` past [`CLASS_MEMBERS_MAX`] members.
///
/// A byte class holds ASCII alone: regex-syntax 0.8's translator in UTF-8 mode refuses a
/// byte class holding any other byte with `InvalidUtf8`, "pattern can match invalid
/// UTF-8", so each byte maps to the one character a text holds it as.
fn class_members(class: &Class) -> Option<Vec<char>> {
    match class {
        Class::Unicode(unicode) => {
            let count: u32 = unicode
                .ranges()
                .iter()
                .map(|range| u32::from(range.end()) - u32::from(range.start()) + 1)
                .sum();
            (count <= CLASS_MEMBERS_MAX).then(|| {
                unicode
                    .ranges()
                    .iter()
                    .flat_map(|range| range.start()..=range.end())
                    .collect()
            })
        }
        Class::Bytes(bytes) => {
            debug_assert!(
                bytes.is_ascii(),
                "a byte class must hold ASCII alone, as a translator in UTF-8 mode builds it: \
                 class={bytes:?}"
            );
            let count: u32 = bytes
                .ranges()
                .iter()
                .map(|range| u32::from(range.end()) - u32::from(range.start()) + 1)
                .sum();
            (count <= CLASS_MEMBERS_MAX).then(|| {
                bytes
                    .ranges()
                    .iter()
                    .flat_map(|range| range.start()..=range.end())
                    .map(char::from)
                    .collect()
            })
        }
    }
}

/// A class of at most [`CLASS_EXPANSION_MAX`] folded characters expands into its members;
/// a case class every member of which FTS5 folds alike is one character.
fn class_shape(class: &Class) -> Shape {
    let Some(members) = class_members(class) else {
        return Shape::Required(None);
    };
    let folded: BTreeSet<String> = members.into_iter().map(|c| fold(c).to_string()).collect();
    if folded.len() > CLASS_EXPANSION_MAX {
        return Shape::Required(None);
    }
    Shape::Exact(folded)
}

fn repetition_shape(repetition: &Repetition) -> Shape {
    let inner = shape(&repetition.sub);
    match (repetition.min, repetition.max, inner) {
        (0, Some(1), Shape::Exact(mut strings)) if strings.len() < EXACT_STRINGS_MAX => {
            strings.insert(String::new());
            Shape::Exact(strings)
        }
        (0, _, _) => Shape::Required(None),
        (min, Some(max), Shape::Exact(strings))
            if min == max && min <= REPETITION_EXPANSION_MAX =>
        {
            repeated(&strings, min)
        }
        (_, _, inner) => Shape::Required(requirement(inner)),
    }
}

/// `strings` repeated `count` times, or one copy's requirement when the product passes
/// [`EXACT_STRINGS_MAX`].
fn repeated(strings: &BTreeSet<String>, count: u32) -> Shape {
    let mut joined = empty_string();
    for _ in 0..count {
        match product(&joined, strings) {
            Some(longer) => joined = longer,
            None => return Shape::Required(requirement(Shape::Exact(strings.clone()))),
        }
    }
    Shape::Exact(joined)
}

fn concat_shape(members: &[Hir]) -> Shape {
    let mut current = empty_string();
    let mut required = Vec::new();
    let mut closed = false;
    for member in members {
        match shape(member) {
            Shape::Exact(strings) => {
                if let Some(joined) = product(&current, &strings) {
                    current = joined;
                } else {
                    required.extend(requirement(Shape::Exact(current)));
                    closed = true;
                    current = strings;
                }
            }
            Shape::Required(formula) => {
                required.extend(requirement(Shape::Exact(current)));
                required.extend(formula);
                closed = true;
                current = empty_string();
            }
        }
    }
    if !closed {
        return Shape::Exact(current);
    }
    required.extend(requirement(Shape::Exact(current)));
    Shape::Required(all(required))
}

fn alternation_shape(branches: &[Hir]) -> Shape {
    let shapes: Vec<Shape> = branches.iter().map(shape).collect();
    let mut union = BTreeSet::new();
    let mut exact = true;
    for branch in &shapes {
        match branch {
            Shape::Exact(strings) if union.len() + strings.len() <= EXACT_STRINGS_MAX => {
                union.extend(strings.iter().cloned());
            }
            Shape::Exact(_) | Shape::Required(_) => exact = false,
        }
    }
    if exact {
        return Shape::Exact(union);
    }
    let formulas: Option<Vec<Prefilter>> = shapes.into_iter().map(requirement).collect();
    Shape::Required(formulas.map(any))
}

/// Every string of `left` followed by every string of `right`, or `None` past
/// [`EXACT_STRINGS_MAX`].
fn product(left: &BTreeSet<String>, right: &BTreeSet<String>) -> Option<BTreeSet<String>> {
    if left.len().saturating_mul(right.len()) > EXACT_STRINGS_MAX {
        return None;
    }
    Some(
        left.iter()
            .flat_map(|head| right.iter().map(move |tail| format!("{head}{tail}")))
            .collect(),
    )
}

fn requirement(shape: Shape) -> Option<Prefilter> {
    match shape {
        Shape::Required(formula) => formula,
        Shape::Exact(strings) if strings.is_empty() => None,
        Shape::Exact(strings) => strings
            .iter()
            .map(|string| string_requirement(string))
            .collect::<Option<Vec<_>>>()
            .map(any),
    }
}

/// One exact string's requirement: the trigrams of each of its lines, since a chunk row
/// ends at a line and a literal holding a newline may cross two rows. A line under 3
/// characters requires nothing.
fn string_requirement(string: &str) -> Option<Prefilter> {
    let lines: Vec<Prefilter> = string
        .split('\n')
        .map(trigram_set)
        .filter(|trigrams| !trigrams.is_empty())
        .map(Prefilter::Literal)
        .collect();
    all(lines)
}

fn all(members: Vec<Prefilter>) -> Option<Prefilter> {
    let mut flat = BTreeSet::new();
    for member in members {
        match member {
            Prefilter::All(inner) => flat.extend(inner),
            other => {
                flat.insert(other);
            }
        }
    }
    let mut flat: Vec<Prefilter> = flat.into_iter().collect();
    match flat.len() {
        0 => None,
        1 => flat.pop(),
        _ => Some(Prefilter::All(flat)),
    }
}

fn any(members: Vec<Prefilter>) -> Prefilter {
    let mut flat = BTreeSet::new();
    for member in members {
        match member {
            Prefilter::Any(inner) => flat.extend(inner),
            other => {
                flat.insert(other);
            }
        }
    }
    let mut flat: Vec<Prefilter> = flat.into_iter().collect();
    if flat.len() == 1 {
        return flat.remove(0);
    }
    Prefilter::Any(flat)
}

#[cfg(test)]
#[path = "pattern_tests.rs"]
mod tests;
