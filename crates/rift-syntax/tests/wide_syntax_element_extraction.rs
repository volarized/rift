//! Shapes that place many siblings under one syntax element analyze in time linear in their size.
//!
//! Tree-sitter's `Node::child`, `Node::prev_sibling`, and `Node::parent` search again from the
//! first child or from the root on every call, so a walk or a rule stepping through siblings
//! with them is quadratic in the sibling count. Each test analyzes one such shape at two sizes
//! [`GROWTH_FACTOR`] apart: a linear analysis takes about [`GROWTH_FACTOR`] times as long on
//! the larger file, a quadratic one about its square, and [`ratio_max`] tells the two apart.
//! Process CPU time measures the analyzer's work, including any worker threads, without
//! charging it for another process's scheduling time. The existing nextest wall-clock
//! deadline still bounds the whole test. Bounded batches reach [`CPU_SAMPLE_MIN`] before
//! averaging, so a coarse CPU clock can measure short analyses too. Their documents stay
//! alive until the clock stops, excluding result destruction from the measured work.
//! The smaller file grows until one analysis takes [`SMALL_RUN_MIN`] of CPU time or reaches
//! the same source, syntax element or calibration bound as before.
//!
//! The wide-syntax-element case and the attached-document case walk [`registry::providers`], so
//! every shipped language and dialect joins them once [`wide_line`] and [`attached_run`] name
//! it, and a failure names the provider's language. `.config/nextest.toml` runs this suite
//! alone, so a per-provider copy of the wide case would add its whole time to the run.

use std::time::Duration;

use cpu_time::ProcessTime;
use rift_core::ProjectPath;
use rift_protocol::read::Language;
use rift_syntax::{
    MarkdownSyntaxProvider, SyntaxDocument, SyntaxLimits, SyntaxProvider, SyntaxSource, registry,
};

/// How many times larger the larger file of each shape is.
const GROWTH_FACTOR: u16 = 8;

/// The time ratio between the larger and the smaller file from which the test judges an
/// analysis quadratic.
///
/// A linear analysis measures about [`GROWTH_FACTOR`], 8, and a quadratic one about its
/// square, 64. The bound is `GROWTH_FACTOR^1.5`, 22.6, the geometric mean of the two: as many
/// times above the linear ratio as it is below the quadratic one. Either kind of analysis
/// therefore has to measure 2.8 times away from its expected ratio, above it for a linear one
/// and below it for a quadratic one, before the test mistakes one for the other.
///
/// On an Apple M-series debug build every shape measured 7.9 to 8.3, while a macos-15 runner
/// measured a Rust wide syntax element at 11.73, its larger file analyzing 1.47 times slower per line.
/// A quadratic walk never runs alone: the measured ratio is 8 and 64 averaged by how the
/// smaller file's time splits between linear work, tree-sitter's own parse included, and the
/// walk. The walk crosses the bound once it takes more than a quarter of that time and reaches
/// 64 only as its share approaches all of it, so each shape starts at a size where the walk it
/// guards against measures above the bound.
fn ratio_max() -> f64 {
    let linear = f64::from(GROWTH_FACTOR);
    let quadratic = linear * linear;
    (linear * quadratic).sqrt()
}

/// Samples of the smaller file, and at most as many of the larger one. The fastest counts,
/// reducing incidental work inside the test process.
const REPEATS: usize = 3;

/// Target CPU time of one smaller-file analysis, within the existing input bounds.
const SMALL_RUN_MIN: Duration = Duration::from_millis(20);

/// Process CPU time the aggregate case may spend over every provider: the 60-second test
/// deadline it has on main, held in CPU time so another process's load cannot spend it.
const AGGREGATE_CPU_MAX: Duration = Duration::from_secs(60);

/// CPU work accumulated by one sample before normalizing by its analysis count.
const CPU_SAMPLE_MIN: Duration = SMALL_RUN_MIN.saturating_mul(2);

/// Bounds retained documents while measuring clocks with coarse accounting intervals.
const ANALYSES_PER_SAMPLE_MAX: usize = 64;

/// Doublings of a shape's starting count calibration takes at most: sixteen times the
/// start, which the fastest shape on an Apple M-series debug build, a JSON wide syntax element at
/// 1.25 ms per thousand lines, needs to reach [`SMALL_RUN_MIN`].
const CALIBRATION_DOUBLINGS_MAX: usize = 4;

/// Lines in the smallest calibrated wide-syntax element file, each one more child of the same syntax element.
///
/// Reading each child with `Node::child` measured 59.4 to 62.2 at this count for every
/// language whose grammar keeps the lines as direct children of one syntax element; [`wide_line`] names
/// the two whose grammars do not.
const WIDE_SYNTAX_ELEMENT_LINES_MIN: usize = 1_000;

/// Doc comment lines in front of the one declaration in the smallest calibrated
/// attached-run file. Reading each child with `Node::child` measured 51.2 to 59.6 at this
/// count.
const ATTACHED_DOC_LINES_MIN: usize = 1_000;

/// Headings in the smallest calibrated heading file, each opening a section directly under
/// the document.
///
/// Looking each heading's symbol up with a scan over every symbol measured 28.3 at 2,000
/// headings and 37.9 to 38.4 at this count.
const HEADINGS_MIN: usize = 4_000;

/// Link reference definitions in the smallest calibrated reference file, each its own block.
///
/// Tree-sitter's own parse carries most of this shape's time, so matching each link to its
/// block with a scan over every block measured 15.6 at 4,000 definitions and 23.6 to 23.9 at
/// this count. Its larger file of 240,002 syntax elements is the largest round count the default syntax
/// syntax element bound admits, and under the scan the test already runs 44 of the 60 seconds nextest
/// allows it.
///
/// Against [`ratio_max`], 22.63, that scan clears the bound by 4 to 6 percent, so this test
/// catches it with little margin to spare. A linear analysis keeps its full factor of 2.8 below
/// the bound, so the thin margin weakens detection and never fails a linear run.
const REFERENCE_DEFINITIONS_MIN: usize = 10_000;

/// One line of the wide-syntax element file for `language`: a construct the grammar places as one more
/// child of the same syntax element. Panics when a registered provider names a language this table has
/// no entry for, which tells the next implementer to add one.
///
/// The markdown and TOML grammars place no run of lines directly under one syntax element. TOML ends
/// every line with a newline its rules collect in a `repeat`, and markdown builds blocks, list
/// items, and table rows from `repeat` rules too. Tree-sitter nests each repetition in hidden
/// repeat syntax elements such as `document_repeat1`, and `Node::child` steps over a hidden syntax element by its
/// child count. Every spelling tried in those two languages, comments and values inside a TOML
/// array, markdown list items and table rows among them, measured 8.5 to 9.1 under that walk,
/// so for them this shape checks linear extraction only.
fn wide_line(language: &Language) -> &'static str {
    match language.name.as_str() {
        "rust" | "javascript" | "typescript" | "json" | "jsonc" | "c" | "cpp" => {
            "// wide syntax element comment line\n"
        }
        "python" | "cython" | "yaml" | "toml" => "# wide syntax element comment line\n",
        "markdown" => "***\n\n",
        "html" | "vue" | "svelte" => "<!-- wide syntax element comment line -->\n",
        "css" => "/* wide syntax element comment line */\n",
        name => panic!(
            "a registered provider has no wide syntax element line: language={name}, dialect={:?}",
            language.dialect
        ),
    }
}

/// One doc comment line and the declaration it attaches to for `language`; `None` for a
/// language whose grammar attaches no preceding comment to a declaration. Panics when a
/// registered provider names a language this table has no entry for.
fn attached_run(language: &Language) -> Option<(&'static str, &'static str)> {
    match language.name.as_str() {
        "rust" => Some(("/// wide syntax element doc line\n", "fn wide() {}\n")),
        "javascript" | "typescript" => Some((
            "/** wide syntax element doc line */\n",
            "function wide() {}\n",
        )),
        "python" | "json" | "jsonc" | "yaml" | "toml" | "markdown" | "html" | "css" | "vue"
        | "svelte" | "c" | "cpp" | "cython" => None,
        name => panic!(
            "a registered provider has no attached run entry: language={name}, dialect={:?}",
            language.dialect
        ),
    }
}

/// Measures a bounded batch and returns its last document and average analysis CPU time.
fn timed_analysis(provider: &dyn SyntaxProvider, text: &str) -> (SyntaxDocument, Duration) {
    let path = ProjectPath::new("wide").expect("valid fixture path");
    let mut documents = Vec::with_capacity(ANALYSES_PER_SAMPLE_MAX);
    let start = ProcessTime::try_now().expect("the process CPU clock must be available");
    let elapsed = loop {
        let source = SyntaxSource { path: &path, text };
        let (analysis, _) = rift_tracing::measure_elapsed!("syntax.analyze", {
            provider.analyze(source, SyntaxLimits::default())
        })
        .expect("the monotonic clock must not regress");
        documents.push(analysis.unwrap_or_else(|error| {
            panic!(
                "a wide shape must analyze under the default limits: language={:?}, error={error}",
                provider.language()
            )
        }));
        let elapsed = start
            .try_elapsed()
            .expect("the process CPU clock must be readable");
        if elapsed >= CPU_SAMPLE_MIN {
            break elapsed;
        }
        assert!(
            documents.len() < ANALYSES_PER_SAMPLE_MAX,
            "the CPU sample must reach its accounting interval within the document bound"
        );
    };
    let analyses = u32::try_from(documents.len()).expect("the sample count is bounded");
    let document = documents.pop().expect("at least one measured analysis");
    (document, elapsed / analyses)
}

/// The smaller file's line count: `lines_min`, doubled until one analysis of the smaller file
/// takes [`SMALL_RUN_MIN`], at most [`CALIBRATION_DOUBLINGS_MAX`] times.
///
/// A doubling that would carry the larger file past the default limits' source bytes or
/// syntax elements is not taken. One run at `lines_min` goes first and counts for nothing, so
/// first-use setup lands on no timed run.
fn calibrated_lines(
    provider: &dyn SyntaxProvider,
    build: &impl Fn(usize) -> String,
    lines_min: usize,
) -> usize {
    let limits = SyntaxLimits::default();
    let next_large = 2 * usize::from(GROWTH_FACTOR);
    timed_analysis(provider, &build(lines_min));
    let mut lines = lines_min;
    for _ in 0..CALIBRATION_DOUBLINGS_MAX {
        let text = build(lines);
        let (document, elapsed) = timed_analysis(provider, &text);
        let bytes_fit = text.len() * next_large <= limits.source_bytes_max();
        let syntax_elements_fit =
            document.syntax_elements().len() * next_large <= limits.syntax_elements_max();
        if elapsed >= SMALL_RUN_MIN || !(bytes_fit && syntax_elements_fit) {
            return lines;
        }
        lines *= 2;
    }
    lines
}

/// Analyzes the shape `build` spells at a calibrated count and at [`GROWTH_FACTOR`] times it
/// and refuses a time ratio at or above [`ratio_max`]; returns the larger file's document and
/// count.
///
/// The larger file runs between two runs of the smaller one, so their CPU work is compared
/// under the same process conditions. A larger run counts by its fastest sample, so another
/// sample can only lower the ratio: once the ratio is below the bound the larger file is not
/// analyzed again, and a ratio at or above it is refused after [`REPEATS`] samples of each.
fn analyze_linearly(
    provider: &dyn SyntaxProvider,
    build: impl Fn(usize) -> String,
    lines_min: usize,
) -> (SyntaxDocument, usize) {
    let small_lines = calibrated_lines(provider, &build, lines_min);
    let lines = small_lines * usize::from(GROWTH_FACTOR);
    let small_text = build(small_lines);
    let large_text = build(lines);
    let ratio_max = ratio_max();
    let mut small = timed_analysis(provider, &small_text).1;
    let (mut document, mut large) = timed_analysis(provider, &large_text);
    for _ in 1..REPEATS {
        small = small.min(timed_analysis(provider, &small_text).1);
    }
    for _ in 1..REPEATS {
        if large.as_secs_f64() / small.as_secs_f64() < ratio_max {
            break;
        }
        let (analysis, elapsed) = timed_analysis(provider, &large_text);
        large = large.min(elapsed);
        document = analysis;
    }
    let ratio = large.as_secs_f64() / small.as_secs_f64();
    assert!(
        ratio < ratio_max,
        "analysis CPU time must grow linearly with the sibling count: language={:?}, \
         lines={lines}, small={small:?}, large={large:?}, ratio={ratio:.2}, \
         ratio_max={ratio_max:.2}",
        provider.language(),
    );
    (document, lines)
}

/// The aggregate case from main: every registered provider inside one test deadline and one
/// [`AGGREGATE_CPU_MAX`] of process CPU, so the whole wide-syntax-element proof keeps the bound
/// it always had. Its assertions name the provider's language that fails.
#[test]
fn every_provider_reads_a_syntax_element_with_many_children_in_linear_time() {
    let start = ProcessTime::try_now().expect("the process CPU clock must be available");
    for provider in registry::providers() {
        let language = provider.language();
        let line = wide_line(language);
        let build = |count| line.repeat(count);
        let (document, lines) = analyze_linearly(provider, build, WIDE_SYNTAX_ELEMENT_LINES_MIN);
        assert!(
            document.syntax_elements().len() > lines,
            "every generated line must reach the walk as a syntax_element: language={language:?}, \
             syntax_elements={}, lines={lines}",
            document.syntax_elements().len(),
        );
    }
    let spent = start
        .try_elapsed()
        .expect("the process CPU clock must be readable");
    assert!(
        spent <= AGGREGATE_CPU_MAX,
        "every provider's wide syntax element proof must fit the aggregate bound in process CPU \
         time: spent={spent:?}, bound={AGGREGATE_CPU_MAX:?}"
    );
}

#[test]
fn native_function_prototypes_analyze_in_linear_time() {
    for extension in ["c", "cpp"] {
        let provider = registry::provider_for_extension(extension).expect("native provider");
        let (document, lines) = analyze_linearly(
            provider,
            |count| {
                format!(
                    "{}{}",
                    "// wide syntax element comment line\n".repeat(count),
                    "int wide(void);\n".repeat(count),
                )
            },
            WIDE_SYNTAX_ELEMENT_LINES_MIN,
        );
        assert_eq!(document.symbols().len(), lines);
        assert!(
            document
                .symbols()
                .iter()
                .all(|symbol| symbol.kind == "function")
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
fn ecmascript_assigned_methods_attach_doc_runs_in_linear_time() {
    for provider in registry::providers().filter(|provider| {
        matches!(
            provider.language().name.as_str(),
            "javascript" | "typescript"
        )
    }) {
        let prefix = "const owner = {};\n";
        let build = |count| {
            format!(
                "{prefix}{}owner.run = function() {{}};\n",
                "/** Method. */\n".repeat(count)
            )
        };
        let (document, lines) = analyze_linearly(provider, build, ATTACHED_DOC_LINES_MIN);
        let method = document
            .symbols()
            .iter()
            .find(|symbol| symbol.name == "run")
            .expect("assigned method");
        assert_eq!(method.kind, "method");
        assert_eq!(
            method.range.start,
            u64::try_from(prefix.len()).expect("prefix")
        );
        assert_eq!(method.documentation_ranges.len(), lines);
        assert_eq!(method.documentation[0].text.lines().count(), lines);
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
