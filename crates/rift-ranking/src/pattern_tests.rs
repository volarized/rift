use std::collections::{BTreeMap, BTreeSet};

use regex_syntax::hir::{Class, HirKind};

use super::{Pattern, Prefilter, ROW_EXPRESSION_DEPTH_MAX, prefilter};
use crate::trigram::trigram_set;
use rift_error::ErrorSlug;

/// A compiled-size bound far above every pattern these tests write.
const SIZE_LIMIT: usize = 1 << 20;

fn plan(pattern: &str) -> Option<Prefilter> {
    Pattern::parse(pattern, SIZE_LIMIT)
        .expect("pattern compiles")
        .prefilter()
        .cloned()
}

fn literal(text: &str) -> Prefilter {
    Prefilter::Literal(trigram_set(text))
}

#[test]
fn a_literal_requires_its_trigrams() {
    assert_eq!(plan(r"useState\("), Some(literal("usestate(")));
    assert_eq!(plan(r"\.unwrap\(\)"), Some(literal(".unwrap()")));
}

#[test]
fn case_classes_fts5_folds_count_as_one_character() {
    assert_eq!(plan("(?i)websocket"), Some(literal("websocket")));
    assert_eq!(plan("(?i)KS"), None, "two characters hold no trigram");
    assert_eq!(plan("(?i)kslot"), Some(literal("kslot")));
}

#[test]
fn case_classes_fts5_does_not_fold_expand() {
    let Some(Prefilter::Any(branches)) = plan("(?i)აbc") else {
        panic!("Georgian case class expands into an alternation");
    };
    assert_eq!(branches, [literal("აbc"), literal("Აbc")]);
}

#[test]
fn an_alternation_requires_one_branch() {
    assert_eq!(
        plan("TODO|FIXME"),
        Some(Prefilter::Any(vec![literal("fixme"), literal("todo")]))
    );
    assert_eq!(
        plan("TODO|FI"),
        None,
        "a branch under 3 characters requires nothing"
    );
}

#[test]
fn an_optional_exact_node_joins_its_neighbours() {
    assert_eq!(
        plan(r"export (async )?function \w+Handler"),
        Some(Prefilter::All(vec![
            literal("handler"),
            Prefilter::Any(vec![
                literal("export async function "),
                literal("export function ")
            ]),
        ]))
    );
}

#[test]
fn small_classes_expand_and_large_ones_close_the_literal() {
    assert_eq!(
        plan(r#"from ['"]react"#),
        Some(Prefilter::Any(vec![
            literal("from \"react"),
            literal("from 'react")
        ]))
    );
    assert_eq!(
        plan(r"process\.env\.[A-Z_]+"),
        Some(literal("process.env."))
    );
    assert_eq!(plan("[0-9a-f]{40}"), None);
    assert_eq!(plan(r"\bassert\w*\("), Some(literal("assert")));
}

#[test]
fn a_translator_in_utf8_mode_refuses_a_byte_class_past_ascii() {
    let refused = Pattern::parse(r"(?-u:[\x80\x81])abc", SIZE_LIMIT)
        .expect_err("a byte class past ASCII can match invalid UTF-8");
    assert_eq!(
        refused.slug(),
        ErrorSlug::new("rift.ranking.pattern_syntax")
    );
}

/// Only a translator with UTF-8 mode off builds a byte class past ASCII, and a debug build
/// stops `prefilter` there rather than read its bytes as characters no text holds.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "a byte class must hold ASCII alone")]
fn a_byte_class_past_ascii_breaks_the_prefilter_precondition() {
    let hir = regex_syntax::ParserBuilder::new()
        .utf8(false)
        .build()
        .parse(r"(?-u:[\x80\x81])abc")
        .expect("a byte pattern parses");
    let _ = prefilter(&hir);
}

#[test]
fn a_literal_past_utf8_requires_nothing() {
    let hir = regex_syntax::ParserBuilder::new()
        .utf8(false)
        .build()
        .parse(r"(?-u:\xFF)abc")
        .expect("a byte pattern parses");
    assert_eq!(prefilter(&hir), None);
}

#[test]
fn an_ascii_byte_class_drops_the_line_feed_and_expands_into_its_members() {
    let pattern = Pattern::parse(r"(?-u:[ab\n])cd", SIZE_LIMIT).expect("pattern compiles");
    assert!(pattern.is_line_bound());
    assert_eq!(
        pattern.prefilter(),
        Some(&Prefilter::Any(vec![literal("acd"), literal("bcd")]))
    );
    let matched: Vec<(usize, usize)> = pattern
        .matches("bcd\ncd", 0..6)
        .map(|found| (found.start, found.end))
        .collect();
    assert_eq!(matched, [(0, 3)]);
}

#[test]
fn repetitions_keep_one_copy_or_none() {
    assert_eq!(plan("(abc)+"), Some(literal("abc")));
    assert_eq!(plan("x(abc)*y"), None);
    assert_eq!(plan("(ab){2}"), Some(literal("abab")));
    assert_eq!(
        plan("(abcd|efgh|ijkl|mnop|qrst){3}"),
        Some(Prefilter::Any(
            ["abcd", "efgh", "ijkl", "mnop", "qrst"]
                .map(literal)
                .to_vec()
        )),
        "a product past the string bound keeps one copy's requirement"
    );
}

#[test]
fn a_newline_splits_the_literal() {
    assert_eq!(
        plan("foo\nbar"),
        Some(Prefilter::All(vec![literal("bar"), literal("foo")]))
    );
}

#[test]
fn anchors_and_captures_are_transparent() {
    assert_eq!(plan("^(?P<name>fn) main$"), Some(literal("fn main")));
}

#[test]
fn a_row_expression_renders_the_whole_formula_in_one_match() {
    let expression = |pattern: &str| {
        plan(pattern)
            .and_then(|formula| formula.row_expression())
            .expect("a shallow formula renders")
    };
    assert_eq!(
        expression("TODO|FIXME"),
        r#"("fix" AND "ixm" AND "xme") OR ("odo" AND "tod")"#
    );
    assert_eq!(expression(r#"a"bc"#), r#""""bc" AND "a""b""#);
    assert_eq!(
        expression("foo.*bar"),
        r#"("bar") AND ("foo")"#,
        "a line-bound match holds both literals in one row"
    );
}

#[test]
fn a_formula_past_the_depth_bound_renders_no_row_expression() {
    let mut formula = literal("abc");
    for level in 0..=ROW_EXPRESSION_DEPTH_MAX {
        let sibling = literal(&format!("sibling {level}"));
        formula = if level % 2 == 0 {
            Prefilter::All(vec![formula, sibling])
        } else {
            Prefilter::Any(vec![formula, sibling])
        };
    }
    assert_eq!(formula.row_expression(), None);
    assert_eq!(
        formula.literals().len(),
        ROW_EXPRESSION_DEPTH_MAX + 2,
        "each literal still runs its own match"
    );
}

#[test]
fn literals_hold_answers_the_formula_per_file() {
    let formula = plan("foo\nbar|bazz").expect("the formula requires trigrams");
    let holding = |present: &[&str]| {
        let present: BTreeSet<BTreeSet<String>> =
            present.iter().map(|text| trigram_set(text)).collect();
        formula.accepts_literals(&|literal| present.contains(literal))
    };
    assert!(holding(&["foo", "bar"]));
    assert!(holding(&["bazz"]));
    assert!(!holding(&["foo"]));
}

/// A seeded generator, so a failure names the seed that reproduces it.
struct Generator {
    state: u64,
    variants: BTreeMap<char, Vec<char>>,
}

/// Characters the generated patterns and texts draw from: case pairs FTS5 folds (`k` and
/// the Kelvin sign, `s` and the long s), case pairs it does not (Georgian, Cyrillic `в`
/// and `ᲀ`), a carriage return, a newline, and NUL.
const ALPHABET: [char; 25] = [
    'a', 'b', 'c', 'k', 's', 'K', 'S', '\u{212A}', '\u{017F}', 'ა', 'Ა', 'в', 'В', 'ᲀ', ' ', '\n',
    '\r', '_', '0', '(', '.', '"', '\0', 'é', 'É',
];

const NAMED_CLASSES: [&str; 7] = [r"\w", r"\s", r"\d", ".", "(?s:.)", "[0-9a-f]", "[a-c]"];

const LOOKS: [&str; 6] = ["^", "$", r"\b", r"\B", "(?-m:^)", "(?-m:$)"];

enum Node {
    Literal(Vec<char>),
    Class(Vec<char>),
    Named(&'static str),
    Negated(Vec<char>),
    Look(&'static str),
    Concat(Vec<Node>),
    Alternation(Vec<Node>),
    Repeat(Box<Node>, u32, Option<u32>),
    Capture(Box<Node>),
    CaseInsensitive(Box<Node>),
}

impl Generator {
    fn new(seed: u64) -> Self {
        Self {
            state: seed,
            variants: BTreeMap::new(),
        }
    }

    /// `SplitMix64`.
    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(bound).expect("bound fits")).expect("fits")
    }

    fn character(&mut self) -> char {
        ALPHABET[self.below(ALPHABET.len())]
    }

    fn characters(&mut self, most: usize) -> Vec<char> {
        let count = 1 + self.below(most);
        (0..count).map(|_| self.character()).collect()
    }

    fn node(&mut self, depth: u32) -> Node {
        let choice = if depth == 0 {
            self.below(5)
        } else {
            self.below(11)
        };
        match choice {
            0 | 1 => Node::Literal(self.characters(6)),
            2 => Node::Class(self.characters(4)),
            3 => Node::Named(NAMED_CLASSES[self.below(NAMED_CLASSES.len())]),
            4 => Node::Look(LOOKS[self.below(LOOKS.len())]),
            5 => Node::Negated(self.characters(2)),
            6 | 7 => {
                let count = 2 + self.below(3);
                Node::Concat((0..count).map(|_| self.node(depth - 1)).collect())
            }
            8 => {
                let count = 2 + self.below(3);
                Node::Alternation((0..count).map(|_| self.node(depth - 1)).collect())
            }
            9 => self.repeat(depth),
            _ if self.below(2) == 0 => Node::Capture(Box::new(self.node(depth - 1))),
            _ => Node::CaseInsensitive(Box::new(self.node(depth - 1))),
        }
    }

    fn repeat(&mut self, depth: u32) -> Node {
        let inner = Box::new(self.node(depth - 1));
        match self.below(5) {
            0 => Node::Repeat(inner, 0, Some(1)),
            1 => Node::Repeat(inner, 0, None),
            2 => Node::Repeat(inner, 1, None),
            3 => {
                let count = u32::try_from(self.below(3)).expect("fits") + 1;
                Node::Repeat(inner, count, Some(count))
            }
            _ => Node::Repeat(inner, 1, Some(3)),
        }
    }

    fn variants(&mut self, character: char) -> Vec<char> {
        self.variants
            .entry(character)
            .or_insert_with(|| {
                let pattern = format!("(?i){}", regex_syntax::escape(&character.to_string()));
                let hir = regex_syntax::Parser::new().parse(&pattern).expect("parses");
                match hir.kind() {
                    HirKind::Class(Class::Unicode(class)) => class
                        .ranges()
                        .iter()
                        .flat_map(|range| range.start()..=range.end())
                        .collect(),
                    _ => vec![character],
                }
            })
            .clone()
    }

    fn pick(&mut self, from: &[char], insensitive: bool) -> char {
        let character = from[self.below(from.len())];
        if !insensitive {
            return character;
        }
        let variants = self.variants(character);
        variants[self.below(variants.len())]
    }

    fn sample(&mut self, node: &Node, insensitive: bool, text: &mut String) {
        match node {
            Node::Literal(characters) => {
                for &character in characters {
                    let chosen = self.pick(&[character], insensitive);
                    text.push(chosen);
                }
            }
            Node::Class(characters) => {
                let chosen = self.pick(characters, insensitive);
                text.push(chosen);
            }
            Node::Named(name) => {
                let from: &[char] = match *name {
                    r"\w" => &['a', '_', '0', 'é', 'в', 'K'],
                    r"\s" => &[' ', '\n', '\r'],
                    r"\d" => &['0'],
                    "[0-9a-f]" => &['0', 'a', 'c'],
                    "[a-c]" => &['a', 'b', 'c'],
                    _ => &ALPHABET,
                };
                let chosen = self.pick(from, insensitive);
                text.push(chosen);
            }
            Node::Negated(_) => text.push(self.character()),
            Node::Look(_) => {}
            Node::Concat(members) => {
                for member in members {
                    self.sample(member, insensitive, text);
                }
            }
            Node::Alternation(branches) => {
                let chosen = self.below(branches.len());
                self.sample(&branches[chosen], insensitive, text);
            }
            Node::Repeat(inner, min, max) => {
                let spread = max.unwrap_or(min + 3) - min;
                let count = min
                    + u32::try_from(self.below(usize::try_from(spread + 1).expect("fits")))
                        .expect("fits");
                for _ in 0..count {
                    self.sample(inner, insensitive, text);
                }
            }
            Node::Capture(inner) => self.sample(inner, insensitive, text),
            Node::CaseInsensitive(inner) => self.sample(inner, true, text),
        }
    }

    fn noise(&mut self, most: usize) -> String {
        let count = self.below(most + 1);
        (0..count).map(|_| self.character()).collect()
    }
}

fn escaped(characters: &[char]) -> String {
    characters
        .iter()
        .map(|character| regex_syntax::escape(&character.to_string()))
        .collect()
}

fn render(node: &Node) -> String {
    match node {
        Node::Literal(characters) => escaped(characters),
        Node::Class(characters) => format!("[{}]", escaped(characters)),
        Node::Named(name) | Node::Look(name) => (*name).to_owned(),
        Node::Negated(characters) => format!("[^{}]", escaped(characters)),
        Node::Concat(members) => members.iter().map(render).collect(),
        Node::Alternation(branches) => format!(
            "(?:{})",
            branches.iter().map(render).collect::<Vec<_>>().join("|")
        ),
        Node::Repeat(inner, min, max) => {
            let operator = match (min, max) {
                (0, Some(1)) => "?".to_owned(),
                (0, None) => "*".to_owned(),
                (1, None) => "+".to_owned(),
                (min, Some(max)) if min == max => format!("{{{min}}}"),
                (min, Some(max)) => format!("{{{min},{max}}}"),
                (min, None) => format!("{{{min},}}"),
            };
            format!("(?:{}){operator}", render(inner))
        }
        Node::Capture(inner) => format!("({})", render(inner)),
        Node::CaseInsensitive(inner) => format!("(?i:{})", render(inner)),
    }
}

/// What one property run saw.
#[derive(Debug, Default)]
struct Outcome {
    patterns: usize,
    with_prefilter: usize,
    texts: usize,
    matched: usize,
    crossing_lines: usize,
}

/// The rows of `text` at the finest chunking the index ever stores: one row per line, its
/// ending kept. A stored row packs whole lines, so a literal one of these rows holds sits
/// whole in every row the index could store.
fn line_rows(text: &str) -> Vec<(std::ops::Range<usize>, BTreeSet<String>)> {
    let mut start = 0;
    text.split_inclusive('\n')
        .map(|line| {
            let span = start..start + line.len();
            start = span.end;
            (span, trigram_set(line))
        })
        .collect()
}

/// Asserts the index selects `text` for `pattern` whenever the matcher finds a match in
/// it: a line-bound match's own row meets the whole formula, and any other match's file
/// meets it literal by literal across its rows.
fn assert_selected(seed: u64, source: &str, pattern: &Pattern, text: &str) -> bool {
    let found: Vec<std::ops::Range<usize>> = pattern.matches(text, 0..text.len()).collect();
    let (Some(first), Some(formula)) = (found.first(), pattern.prefilter()) else {
        return !found.is_empty();
    };
    let rows = line_rows(text);
    if pattern.is_line_bound() {
        for matched in &found {
            assert!(
                !text[matched.clone()].contains('\n'),
                "seed {seed}: line-bound pattern {source:?} matched across a line in {text:?}"
            );
            let (_, row) = rows
                .iter()
                .find(|(span, _)| span.contains(&matched.start))
                .expect("every offset sits in a row");
            assert!(
                formula.accepts(row),
                "seed {seed}: pattern {source:?} matches {text:?} at {matched:?}, its row \
                 misses formula {formula:?}"
            );
        }
    } else {
        let selected = formula.accepts_literals(&|literal| {
            rows.iter().any(|(_, trigrams)| literal.is_subset(trigrams))
        });
        assert!(
            selected,
            "seed {seed}: pattern {source:?} matches {text:?} at {first:?}, its rows miss \
             formula {formula:?}"
        );
    }
    assert!(
        formula.accepts(&trigram_set(text)),
        "seed {seed}: pattern {source:?} matches {text:?}, formula {formula:?} refuses it"
    );
    true
}

fn never_drops_a_match(seed: u64, patterns: usize, texts_per_pattern: usize) -> Outcome {
    let mut generator = Generator::new(seed);
    let mut outcome = Outcome::default();
    for _ in 0..patterns {
        let node = generator.node(3);
        let source = render(&node);
        let pattern = Pattern::parse(&source, SIZE_LIMIT).expect("generated pattern compiles");
        outcome.patterns += 1;
        outcome.with_prefilter += usize::from(pattern.prefilter().is_some());
        outcome.crossing_lines += usize::from(!pattern.is_line_bound());
        for _ in 0..texts_per_pattern {
            let mut text = generator.noise(6);
            generator.sample(&node, false, &mut text);
            text.push_str(&generator.noise(6));
            outcome.texts += 1;
            outcome.matched += usize::from(assert_selected(seed, &source, &pattern, &text));
        }
    }
    outcome
}

/// The query plan never drops a match: for every generated text the matcher matches, the
/// rows the index holds meet the formula the query plan built. The generator covers
/// alternations, small classes, case classes FTS5 folds and ones it does not, repetitions,
/// look-around, and literal newlines.
#[test]
fn the_formula_never_drops_a_matching_text() {
    let outcome = never_drops_a_match(0x5EED_0001, 4_000, 8);
    assert!(
        outcome.matched > outcome.texts / 4,
        "the generator reaches matches: {outcome:?}"
    );
    assert!(
        outcome.with_prefilter > outcome.patterns / 4,
        "the generator reaches prefilters: {outcome:?}"
    );
    assert!(
        outcome.crossing_lines > outcome.patterns / 20,
        "the generator reaches literal newlines: {outcome:?}"
    );
}

mod matcher {
    use super::super::Pattern;
    use super::SIZE_LIMIT;
    use rift_error::ErrorSlug;

    /// Each match's start and end offsets.
    fn ranges(pattern: &str, text: &str) -> Vec<(usize, usize)> {
        let pattern = Pattern::parse(pattern, SIZE_LIMIT).expect("pattern compiles");
        pattern
            .matches(text, 0..text.len())
            .map(|found| (found.start, found.end))
            .collect()
    }

    #[test]
    fn anchors_match_at_line_boundaries_and_classes_never_take_a_line_feed() {
        let text = "use a;\nfn b() {}  \nuse c;\n";
        assert_eq!(ranges("^use ", text), [(0, 4), (19, 23)]);
        assert_eq!(
            ranges(r"\s+$", text),
            [(16, 18)],
            "only the trailing spaces"
        );
        assert_eq!(ranges(r";$", text), [(5, 6), (24, 25)]);
        assert_eq!(
            ranges(r"(?s)b.+", text),
            [(10, 18)],
            "a dot stops at the line end"
        );
    }

    #[test]
    fn a_crlf_line_keeps_its_carriage_return() {
        let text = "let a = 1;\r\nlet b = 2;\n";
        assert_eq!(
            ranges(";$", text),
            [(21, 22)],
            "`$` sits before `\\n` alone"
        );
        assert_eq!(ranges(r"\r$", text), [(10, 11)]);
    }

    #[test]
    fn a_span_reads_look_around_outside_it_and_reports_whole_text_offsets() {
        let pattern = Pattern::parse(r"\bbeta\b", SIZE_LIMIT).expect("pattern compiles");
        let text = "alphabeta\nbeta\n";
        assert_eq!(
            pattern
                .matches(text, 5..text.len())
                .map(|found| (found.start, found.end))
                .collect::<Vec<_>>(),
            [(10, 14)]
        );
    }

    #[test]
    fn a_literal_line_feed_crosses_lines_and_the_pattern_says_so() {
        let pattern = Pattern::parse(r"a;\nuse", SIZE_LIMIT).expect("pattern compiles");
        assert!(!pattern.is_line_bound());
        assert!(
            Pattern::parse("use", SIZE_LIMIT)
                .expect("compiles")
                .is_line_bound()
        );
        assert_eq!(ranges(r"a;\nuse", "use a;\nuse b;\n"), [(4, 10)]);
    }

    #[test]
    fn multibyte_text_matches_at_byte_offsets() {
        let text = "grüße 日本語\n";
        assert_eq!(ranges("日本", text), [(8, 14)]);
        assert_eq!(ranges("(?i)GRÜSSE|grüße", text), [(0, 7)]);
    }

    #[test]
    fn a_pattern_past_the_size_limit_or_out_of_syntax_is_refused() {
        let oversized = Pattern::parse(r"\w{1000}", 1024).expect_err("past the size bound");
        assert_eq!(
            oversized.slug(),
            ErrorSlug::new("rift.ranking.pattern_size")
        );
        assert!(
            oversized.detail().contains("exceeds 1024 bytes"),
            "{}",
            oversized.detail()
        );
        assert_eq!(
            oversized.slug(),
            ErrorSlug::new("rift.ranking.pattern_size")
        );
        let unclosed = Pattern::parse("useState(", SIZE_LIMIT).expect_err("an unclosed group");
        assert_eq!(
            unclosed.slug(),
            ErrorSlug::new("rift.ranking.pattern_syntax")
        );
        assert!(
            unclosed.detail().contains("unclosed group"),
            "{}",
            unclosed.detail()
        );
    }

    #[test]
    fn the_prefilter_comes_from_the_rewritten_pattern() {
        let pattern = Pattern::parse("[0-9a-f]{40}", SIZE_LIMIT).expect("pattern compiles");
        assert!(pattern.prefilter().is_none());
        let pattern = Pattern::parse("(?i)todo|fixme", SIZE_LIMIT).expect("pattern compiles");
        assert!(pattern.prefilter().is_some());
    }
}

#[test]
fn the_rewrite_leaves_every_class_without_a_line_feed() {
    let pattern = Pattern::parse(r"[^a]\s(?s:.)", SIZE_LIMIT).expect("pattern compiles");
    assert!(pattern.is_line_bound());
    assert_eq!(
        pattern.matches("b \nx", 0..4).count(),
        0,
        "no class of the rewritten pattern takes the line feed"
    );
    let refused = Pattern::parse("(", SIZE_LIMIT).expect_err("an unclosed group");
    assert_eq!(
        refused.slug(),
        ErrorSlug::new("rift.ranking.pattern_syntax")
    );
}
