//! A node with many children analyzes in time linear in its child count.
//!
//! A file of comment lines alone places every line directly under one node, and the walk
//! reads each node's children once, stepping from one sibling to the next. `Node::child`
//! restarts from the first child on every call, so a walk that indexes each child in turn
//! is quadratic in that count and spends [`ANALYSIS_BUDGET`] on the first of these files.
//!
//! The loop walks [`registry::providers`], so a provider registered later joins this proof
//! once [`wide_line`] names a line for its language.

use std::time::Duration;

use rift_core::{ProjectPath, SystemMonotonicClock, measure_elapsed};
use rift_protocol::read::Language;
use rift_syntax::{SyntaxLimits, SyntaxSource, registry};

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

#[test]
fn every_provider_analyzes_a_node_with_many_children_within_the_budget() {
    let path = ProjectPath::new("wide").expect("valid fixture path");
    let mut spent = Duration::ZERO;
    for provider in registry::providers() {
        let language = provider.language();
        let text = wide_line(language).repeat(WIDE_NODE_LINES);
        let source = SyntaxSource {
            path: &path,
            text: &text,
        };
        let (analysis, measurement) = measure_elapsed!(SystemMonotonicClock, "syntax.analyze", {
            provider.analyze(source, SyntaxLimits::default())
        })
        .expect("the monotonic clock must not regress");
        let document = analysis.unwrap_or_else(|error| {
            panic!(
                "a wide node must analyze under the default limits: language={language:?}, \
                 error={error}"
            )
        });
        assert!(
            document.nodes().len() > WIDE_NODE_LINES,
            "every generated line must reach the walk as a node: language={language:?}, \
             nodes={}, lines={WIDE_NODE_LINES}",
            document.nodes().len(),
        );
        spent += measurement.elapsed();
        assert!(
            spent < ANALYSIS_BUDGET,
            "a node's children must be read in time linear in their count: \
             language={language:?}, elapsed={:?}, spent={spent:?}, budget={ANALYSIS_BUDGET:?}",
            measurement.elapsed(),
        );
    }
}
