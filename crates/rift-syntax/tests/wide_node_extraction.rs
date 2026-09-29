//! Shapes that place many siblings under one node analyze in time linear in their size.
//!
//! Tree-sitter's `Node::child`, `Node::prev_sibling`, and `Node::parent` search again from the
//! first child or from the root on every call, so a walk or a rule stepping through siblings
//! with them is quadratic in the sibling count. Each test analyzes one such shape at two sizes
//! [`GROWTH_FACTOR`] apart: a linear analysis takes about [`GROWTH_FACTOR`] times as long on
//! the larger file, a quadratic one about its square. Both sizes run on the same machine, so
//! the ratio between them does not depend on how fast that machine is, and
//! `.config/nextest.toml` runs this suite alone, so no other test's load lands between them.
//! The smaller file grows until one analysis of it takes [`SMALL_RUN_MIN`], so a slowdown the
//! machine imposes for a moment stays small beside the times it divides.
//!
//! The loops walk [`registry::providers`], so a provider registered later joins these proofs
//! once [`wide_line`] and [`attached_run`] name its language.

use std::time::Duration;

use rift_core::{ProjectPath, SystemMonotonicClock, measure_elapsed};
use rift_protocol::read::Language;
use rift_syntax::{
    MarkdownSyntaxProvider, SyntaxDocument, SyntaxLimits, SyntaxProvider, SyntaxSource, registry,
};

/// How many times larger the larger file of each shape is.
const GROWTH_FACTOR: usize = 8;

/// The largest time ratio between the larger and the smaller file a linear analysis may show.
///
/// On an Apple M-series debug build every shape measured 7.97 to 8.18 after the walks stopped
/// restarting from the first sibling, each size the fastest of [`REPEATS`] runs and the
/// smaller one calibrated to [`SMALL_RUN_MIN`]. The quadratic walks measured 58.8 for wide
/// nodes, 60.6 for attached runs, 28.7 for headings, and 15.5 for link reference definitions,
/// where tree-sitter's own parse carries most of the time. Eleven leaves linear growth a third
/// of headroom for timing noise and stays more than a quarter below the smallest quadratic
/// ratio.
const RATIO_MAX: f64 = 11.0;

/// Runs per size. The fastest counts, so a run slowed by other work on the machine drops out.
const REPEATS: usize = 3;

/// The time one analysis of the smaller file takes at least, once calibrated.
///
/// A stall that lands on every larger run adds its length divided by the smaller run's time
/// to the ratio. A TOML wide node whose smaller file analyzed in 3.0 ms measured 11.8 on a
/// busy machine, each 23.6 ms larger run stalled to 35.5 ms; the quiet machine measured 8.1.
/// At twenty milliseconds the same stall adds 0.6, and only a stall past about 60 ms on
/// every larger run reaches [`RATIO_MAX`].
const SMALL_RUN_MIN: Duration = Duration::from_millis(20);

/// Doublings of a shape's starting count calibration takes at most: sixteen times the
/// start, which the fastest shape on an Apple M-series debug build, a JSON wide node at
/// 1.25 ms per thousand lines, needs to reach [`SMALL_RUN_MIN`].
const CALIBRATION_DOUBLINGS_MAX: usize = 4;

/// Lines in the smallest calibrated wide-node file, each one more child of the same node.
const WIDE_NODE_LINES_MIN: usize = 1_000;

/// Doc comment lines in front of the one declaration in the smallest calibrated
/// attached-run file.
const ATTACHED_DOC_LINES_MIN: usize = 1_000;

/// Headings in the smallest calibrated heading file, each opening a section directly under
/// the document.
const HEADINGS_MIN: usize = 2_000;

/// Link reference definitions in the smallest calibrated reference file, each its own
/// block. Tree-sitter's own parse carries most of this shape's time, so it starts large
/// enough that a quadratic walk still shows past [`RATIO_MAX`].
const REFERENCE_DEFINITIONS_MIN: usize = 4_000;

/// One line of the wide-node file for `language`: a construct the grammar places as one more
/// child of the same node. Panics when a registered provider names a language this table has
/// no entry for, which tells the next implementer to add one.
fn wide_line(language: &Language) -> &'static str {
    match language.name.as_str() {
        "rust" | "javascript" | "typescript" | "json" => "// wide node comment line\n",
        "python" | "yaml" | "toml" => "# wide node comment line\n",
        "markdown" => "***\n\n",
        name => panic!(
            "a registered provider has no wide node line: language={name}, dialect={:?}",
            language.dialect
        ),
    }
}

/// One doc comment line and the declaration it attaches to for `language`; `None` for a
/// language whose grammar attaches no preceding comment to a declaration. Panics when a
/// registered provider names a language this table has no entry for.
fn attached_run(language: &Language) -> Option<(&'static str, &'static str)> {
    match language.name.as_str() {
        "rust" => Some(("/// wide node doc line\n", "fn wide() {}\n")),
        "javascript" | "typescript" => {
            Some(("/** wide node doc line */\n", "function wide() {}\n"))
        }
        "python" | "json" | "yaml" | "toml" | "markdown" => None,
        name => panic!(
            "a registered provider has no attached run entry: language={name}, dialect={:?}",
            language.dialect
        ),
    }
}

/// Analyzes `text` once and returns the document with the time it took.
fn timed_analysis(provider: &dyn SyntaxProvider, text: &str) -> (SyntaxDocument, Duration) {
    let path = ProjectPath::new("wide").expect("valid fixture path");
    let source = SyntaxSource { path: &path, text };
    let (analysis, measurement) = measure_elapsed!(SystemMonotonicClock, "syntax.analyze", {
        provider.analyze(source, SyntaxLimits::default())
    })
    .expect("the monotonic clock must not regress");
    let document = analysis.unwrap_or_else(|error| {
        panic!(
            "a wide shape must analyze under the default limits: language={:?}, error={error}",
            provider.language()
        )
    });
    (document, measurement.elapsed())
}

/// The smaller file's line count: `lines_min`, doubled until one analysis of the smaller file
/// takes [`SMALL_RUN_MIN`], at most [`CALIBRATION_DOUBLINGS_MAX`] times.
///
/// A doubling that would carry the larger file past the default limits' source bytes or
/// syntax nodes is not taken. One run at `lines_min` goes first and counts for nothing, so
/// first-use setup lands on no timed run.
fn calibrated_lines(
    provider: &dyn SyntaxProvider,
    build: &impl Fn(usize) -> String,
    lines_min: usize,
) -> usize {
    let limits = SyntaxLimits::default();
    let next_large = 2 * GROWTH_FACTOR;
    timed_analysis(provider, &build(lines_min));
    let mut lines = lines_min;
    for _ in 0..CALIBRATION_DOUBLINGS_MAX {
        let text = build(lines);
        let (document, elapsed) = timed_analysis(provider, &text);
        let bytes_fit = text.len() * next_large <= limits.source_bytes_max();
        let nodes_fit = document.nodes().len() * next_large <= limits.syntax_nodes_max();
        if elapsed >= SMALL_RUN_MIN || !(bytes_fit && nodes_fit) {
            return lines;
        }
        lines *= 2;
    }
    lines
}

/// Analyzes the shape `build` spells at a calibrated count and at [`GROWTH_FACTOR`] times it
/// and refuses a time ratio above [`RATIO_MAX`]; returns the larger file's document and count.
///
/// The two sizes alternate so both meet the same machine load.
fn analyze_linearly(
    provider: &dyn SyntaxProvider,
    build: impl Fn(usize) -> String,
    lines_min: usize,
) -> (SyntaxDocument, usize) {
    let small_lines = calibrated_lines(provider, &build, lines_min);
    let lines = small_lines * GROWTH_FACTOR;
    let small_text = build(small_lines);
    let large_text = build(lines);
    let mut small = Duration::MAX;
    let mut large = Duration::MAX;
    let mut document = None;
    for _ in 0..REPEATS {
        small = small.min(timed_analysis(provider, &small_text).1);
        let (analysis, elapsed) = timed_analysis(provider, &large_text);
        large = large.min(elapsed);
        document = Some(analysis);
    }
    let ratio = large.as_secs_f64() / small.as_secs_f64();
    assert!(
        ratio < RATIO_MAX,
        "analysis time must grow linearly with the sibling count: language={:?}, \
         lines={lines}, small={small:?}, large={large:?}, ratio={ratio:.2}, \
         ratio_max={RATIO_MAX}",
        provider.language(),
    );
    (document.expect("at least one measured run"), lines)
}

#[test]
fn every_provider_reads_a_node_with_many_children_in_linear_time() {
    for provider in registry::providers() {
        let language = provider.language();
        let line = wide_line(language);
        let build = |count| line.repeat(count);
        let (document, lines) = analyze_linearly(provider, build, WIDE_NODE_LINES_MIN);
        assert!(
            document.nodes().len() > lines,
            "every generated line must reach the walk as a node: language={language:?}, \
             nodes={}, lines={lines}",
            document.nodes().len(),
        );
    }
}

#[test]
fn a_declaration_after_many_attached_doc_lines_attaches_in_linear_time() {
    for provider in registry::providers() {
        let language = provider.language();
        let Some((doc_line, declaration)) = attached_run(language) else {
            continue;
        };
        let build = |count| format!("{}{declaration}", doc_line.repeat(count));
        let (document, _) = analyze_linearly(provider, build, ATTACHED_DOC_LINES_MIN);
        let symbol = document
            .symbols()
            .first()
            .expect("the attached-run file declares one symbol");
        assert_eq!(
            symbol.range.start, 0,
            "the whole doc run must attach to the declaration: language={language:?}"
        );
    }
}

#[test]
fn many_headings_analyze_in_linear_time() {
    let provider = MarkdownSyntaxProvider::default();
    let build = |count| "# h\n".repeat(count);
    let (document, headings) = analyze_linearly(&provider, build, HEADINGS_MIN);
    let facts = document.markdown_facts().expect("markdown facts");
    assert_eq!(
        facts.headings().len(),
        headings,
        "every heading must reach the facts"
    );
}

#[test]
fn many_link_reference_definitions_analyze_in_linear_time() {
    let provider = MarkdownSyntaxProvider::default();
    let build = |count| "[w]: /w\n".repeat(count);
    let (document, definitions) = analyze_linearly(&provider, build, REFERENCE_DEFINITIONS_MIN);
    let facts = document.markdown_facts().expect("markdown facts");
    assert_eq!(
        facts.links().len(),
        definitions,
        "every link reference definition must keep its link"
    );
}
