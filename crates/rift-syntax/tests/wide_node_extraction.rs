//! Shapes that place many siblings under one node analyze in time linear in their size.
//!
//! Tree-sitter's `Node::child`, `Node::prev_sibling`, and `Node::parent` search again from the
//! first child or from the root on every call, so a walk or a rule stepping through siblings
//! with them is quadratic in the sibling count. Each test analyzes one such shape at two sizes
//! [`GROWTH_FACTOR`] apart: a linear analysis takes about [`GROWTH_FACTOR`] times as long on
//! the larger file, a quadratic one about its square. Both sizes run on the same machine, so
//! the ratio between them does not depend on how fast that machine is, and
//! `.config/nextest.toml` runs this suite alone, so no other test's load lands between them.
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
/// On an Apple M-series debug build every shape measured 7.9 to 8.2 after the walks stopped
/// restarting from the first sibling, each size the fastest of [`REPEATS`] runs. The quadratic
/// walks measured 60 to 63 for wide nodes and attached runs, 28 for headings, and 15.8 for
/// link reference definitions, where tree-sitter's own parse carries most of the time. Eleven
/// leaves linear growth a third of headroom for timing noise and stays a third below the
/// smallest quadratic ratio.
const RATIO_MAX: f64 = 11.0;

/// Runs per size. The fastest counts, so a run slowed by other work on the machine drops out.
const REPEATS: usize = 3;

/// Lines in each wide-node file, each one more child of the same node.
const WIDE_NODE_LINES: usize = 8_000;

/// Doc comment lines in front of the one declaration in each attached-run file.
const ATTACHED_DOC_LINES: usize = 8_000;

/// Headings in the heading file, each opening a section directly under the document.
const HEADINGS: usize = 16_000;

/// Link reference definitions in the reference file, each its own block.
const REFERENCE_DEFINITIONS: usize = 32_000;

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

/// Analyzes the shape `build` spells at `lines` and at `lines / GROWTH_FACTOR` and refuses a
/// time ratio above [`RATIO_MAX`]; returns the larger file's document.
///
/// The smaller file runs once before the measured runs, so neither size pays for first-use
/// setup, and the two sizes alternate so both meet the same machine load.
fn analyze_linearly(
    provider: &dyn SyntaxProvider,
    build: impl Fn(usize) -> String,
    lines: usize,
) -> SyntaxDocument {
    let small_text = build(lines / GROWTH_FACTOR);
    let large_text = build(lines);
    timed_analysis(provider, &small_text);
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
    document.expect("at least one measured run")
}

#[test]
fn every_provider_reads_a_node_with_many_children_in_linear_time() {
    for provider in registry::providers() {
        let language = provider.language();
        let line = wide_line(language);
        let document = analyze_linearly(provider, |count| line.repeat(count), WIDE_NODE_LINES);
        assert!(
            document.nodes().len() > WIDE_NODE_LINES,
            "every generated line must reach the walk as a node: language={language:?}, \
             nodes={}, lines={WIDE_NODE_LINES}",
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
        let document = analyze_linearly(provider, build, ATTACHED_DOC_LINES);
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
    let document = analyze_linearly(&provider, |count| "# h\n".repeat(count), HEADINGS);
    let facts = document.markdown_facts().expect("markdown facts");
    assert_eq!(
        facts.headings().len(),
        HEADINGS,
        "every heading must reach the facts"
    );
}

#[test]
fn many_link_reference_definitions_analyze_in_linear_time() {
    let provider = MarkdownSyntaxProvider::default();
    let build = |count| "[w]: /w\n".repeat(count);
    let document = analyze_linearly(&provider, build, REFERENCE_DEFINITIONS);
    let facts = document.markdown_facts().expect("markdown facts");
    assert_eq!(
        facts.links().len(),
        REFERENCE_DEFINITIONS,
        "every link reference definition must keep its link"
    );
}
