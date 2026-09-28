//! Shapes that place many siblings under one node analyze in time linear in their size.
//!
//! Tree-sitter's `Node::child`, `Node::prev_sibling`, and `Node::parent` search again from the
//! first child or from the root on every call, so a walk or a rule stepping through siblings
//! with them is quadratic in the sibling count. Each test builds one such shape and requires
//! its analysis to finish within a budget sized from the linear walk: a regression spends the
//! budget on the first file it reaches.
//!
//! The loops walk [`registry::providers`], so a provider registered later joins these proofs
//! once [`wide_line`] and [`attached_run`] name its language.

use std::time::Duration;

use rift_core::{ProjectPath, SystemMonotonicClock, measure_elapsed};
use rift_protocol::read::Language;
use rift_syntax::{
    MarkdownSyntaxProvider, SyntaxDocument, SyntaxLimits, SyntaxProvider, SyntaxSource, registry,
};

/// Lines in each generated file, each one more child of the same node.
const WIDE_NODE_LINES: usize = 40_000;

/// The longest every provider together may spend analyzing its generated file.
///
/// A debug build on an Apple M-series machine analyzes all of them in 1.1 s.
/// The walk that indexed each child took 38.6 s for the Rust file alone and 82 s for the
/// JavaScript one. Twenty-five seconds leaves slower CI runners and coverage
/// instrumentation a tenfold margin and stays below nextest's 60 s test deadline, so the
/// budget reports a regression on the first provider it reaches, before the deadline does.
const ANALYSIS_BUDGET: Duration = Duration::from_secs(25);

/// Doc comment lines in front of the one declaration in each attached-run file.
const ATTACHED_DOC_LINES: usize = 15_000;

/// The longest every attaching provider together may spend on its attached-run file.
///
/// A debug build on an Apple M-series machine analyzes the four files in 0.22 s, while
/// stepping through the run with `Node::prev_sibling` took 20.5 s for the Rust file alone
/// and 22 s for each ECMAScript one. Five seconds leaves a twentyfold margin.
const ATTACHED_RUN_BUDGET: Duration = Duration::from_secs(5);

/// Headings in the heading file, each opening a section directly under the document.
const HEADINGS: usize = 60_000;

/// The longest the markdown provider may spend on the heading file.
///
/// A debug build on an Apple M-series machine analyzes it in 2.3 s, while looking each
/// heading's symbol up with a scan over every symbol took 24.8 s. Twelve seconds leaves a
/// fivefold margin and stays half the scanning time.
const HEADING_BUDGET: Duration = Duration::from_secs(12);

/// Link reference definitions in the reference file, each its own block.
const REFERENCE_DEFINITIONS: usize = 160_000;

/// The longest the markdown provider may spend on the reference file.
///
/// A debug build on an Apple M-series machine analyzes it in 7.9 s, nearly all of it
/// tree-sitter's own parse, while matching each link to its block with a scan over every
/// block took 48.7 s. Twenty-four seconds leaves a threefold margin and stays half the
/// scanning time.
const REFERENCE_BUDGET: Duration = Duration::from_secs(24);

/// Node bound for the markdown files, whose every line is three or four nodes.
const MARKDOWN_NODES_MAX: usize = 1_000_000;

/// One line of the generated file for `language`: a construct the grammar places as one
/// more child of the same node. Panics when a registered provider names a language this
/// table has no entry for, which tells the next implementer to add one.
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

/// Analyzes `text` under `limits`, adds the time it took to `spent`, and requires the
/// running total to stay within `budget`.
fn analyze_within(
    provider: &dyn SyntaxProvider,
    text: &str,
    limits: SyntaxLimits,
    spent: &mut Duration,
    budget: Duration,
) -> SyntaxDocument {
    let path = ProjectPath::new("wide").expect("valid fixture path");
    let language = provider.language();
    let source = SyntaxSource { path: &path, text };
    let (analysis, measurement) = measure_elapsed!(SystemMonotonicClock, "syntax.analyze", {
        provider.analyze(source, limits)
    })
    .expect("the monotonic clock must not regress");
    let document = analysis.unwrap_or_else(|error| {
        panic!("a wide shape must analyze: language={language:?}, error={error}")
    });
    *spent += measurement.elapsed();
    assert!(
        *spent < budget,
        "siblings must be read in time linear in their count: language={language:?}, \
         elapsed={:?}, spent={spent:?}, budget={budget:?}",
        measurement.elapsed(),
    );
    document
}

/// Limits admitting the markdown files: the default bounds with a wider node bound.
fn markdown_limits() -> SyntaxLimits {
    let defaults = SyntaxLimits::default();
    SyntaxLimits::new(
        defaults.source_bytes_max(),
        MARKDOWN_NODES_MAX,
        defaults.syntax_depth_max(),
    )
    .expect("positive markdown limits")
}

#[test]
fn every_provider_analyzes_a_node_with_many_children_within_the_budget() {
    let mut spent = Duration::ZERO;
    for provider in registry::providers() {
        let language = provider.language();
        let text = wide_line(language).repeat(WIDE_NODE_LINES);
        let document = analyze_within(
            provider,
            &text,
            SyntaxLimits::default(),
            &mut spent,
            ANALYSIS_BUDGET,
        );
        assert!(
            document.nodes().len() > WIDE_NODE_LINES,
            "every generated line must reach the walk as a node: language={language:?}, \
             nodes={}, lines={WIDE_NODE_LINES}",
            document.nodes().len(),
        );
    }
}

#[test]
fn a_declaration_after_many_attached_doc_lines_analyzes_within_the_budget() {
    let mut spent = Duration::ZERO;
    for provider in registry::providers() {
        let language = provider.language();
        let Some((doc_line, declaration)) = attached_run(language) else {
            continue;
        };
        let mut text = doc_line.repeat(ATTACHED_DOC_LINES);
        text.push_str(declaration);
        let document = analyze_within(
            provider,
            &text,
            SyntaxLimits::default(),
            &mut spent,
            ATTACHED_RUN_BUDGET,
        );
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
fn many_headings_analyze_within_the_budget() {
    let mut spent = Duration::ZERO;
    let text = "# Wide node heading\n\n".repeat(HEADINGS);
    let document = analyze_within(
        &MarkdownSyntaxProvider::default(),
        &text,
        markdown_limits(),
        &mut spent,
        HEADING_BUDGET,
    );
    let facts = document.markdown_facts().expect("markdown facts");
    assert_eq!(
        facts.headings().len(),
        HEADINGS,
        "every heading must reach the facts"
    );
}

#[test]
fn many_link_reference_definitions_analyze_within_the_budget() {
    let mut spent = Duration::ZERO;
    let text = "[wide]: /wide-node\n".repeat(REFERENCE_DEFINITIONS);
    let document = analyze_within(
        &MarkdownSyntaxProvider::default(),
        &text,
        markdown_limits(),
        &mut spent,
        REFERENCE_BUDGET,
    );
    let facts = document.markdown_facts().expect("markdown facts");
    assert_eq!(
        facts.links().len(),
        REFERENCE_DEFINITIONS,
        "every link reference definition must keep its link"
    );
}
