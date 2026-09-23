//! Checked `CommonMark` and GFM examples against Markdown documentation facts.

use rift_core::ProjectPath;
use rift_syntax::{
    MarkdownBlockKind, MarkdownBlockStructure, MarkdownLinkKind, MarkdownSyntaxProvider,
    SyntaxProvider, SyntaxSource,
};

fn analyze(path: &str, source: &str) -> rift_syntax::SyntaxDocument {
    let path = ProjectPath::new(path).expect("fixture path is valid");
    MarkdownSyntaxProvider::default()
        .analyze(SyntaxSource {
            path: &path,
            text: source,
        })
        .expect("bounded fixture parses")
}

fn commonmark_examples(spec: &str) -> Vec<(usize, String)> {
    let mut state = 0_u8;
    let mut example = 0_usize;
    let mut markdown = String::new();
    let mut examples = Vec::new();
    for line in spec.lines() {
        match (state, line.trim()) {
            (_, "```````````````````````````````` example") => {
                state = 1;
                markdown.clear();
            }
            (1, ".") => state = 2,
            (2, "````````````````````````````````") => {
                state = 0;
                example += 1;
                examples.push((example, markdown.replace('→', "\t")));
            }
            (1, _) => {
                markdown.push_str(line);
                markdown.push('\n');
            }
            _ => {}
        }
    }
    examples
}

fn gfm_examples(corpus: &str) -> Vec<(usize, String)> {
    let mut lines = corpus.lines();
    let mut examples = Vec::new();
    while let Some(line) = lines.next() {
        if line
            != "================================================================================"
        {
            continue;
        }
        let title = lines.next().expect("example title follows separator");
        let number = title
            .strip_prefix("Example ")
            .and_then(|title| title.split_once(' '))
            .and_then(|(number, _)| number.parse::<usize>().ok())
            .unwrap_or_else(|| 1_000_000 + examples.len());
        assert_eq!(
            lines.next(),
            Some(
                "================================================================================"
            ),
            "upstream example header closes"
        );
        let mut markdown = String::new();
        for line in lines.by_ref() {
            if line
                == "--------------------------------------------------------------------------------"
            {
                break;
            }
            markdown.push_str(line);
            markdown.push('\n');
        }
        examples.push((number, markdown));
    }
    examples
}

fn assert_ranges_within_source(source: &str, document: &rift_syntax::SyntaxDocument) -> usize {
    assert_eq!(
        document.source_digest(),
        Some(&rift_core::FileDigest::of(source.as_bytes()))
    );
    let facts = document.markdown_facts().expect("Markdown facts");
    let source_len = source.len() as u64;
    let ranges = facts
        .blocks()
        .iter()
        .map(|fact| fact.range)
        .chain(facts.headings().iter().map(|fact| fact.range))
        .chain(facts.links().iter().flat_map(|fact| {
            [
                Some(fact.range),
                fact.destination_range,
                fact.fragment_range,
                fact.label_range,
            ]
            .into_iter()
            .flatten()
        }))
        .chain(facts.reference_candidates().iter().map(|fact| fact.range))
        .chain(facts.error_ranges().iter().copied())
        .chain(facts.omitted_ranges().iter().copied());
    let mut checked = 0_usize;
    for range in ranges {
        assert!(
            range.start <= range.end,
            "range start exceeds end: {range:?}"
        );
        assert!(range.end <= source_len, "range exceeds source: {range:?}");
        let start = usize::try_from(range.start).expect("range starts within source bound");
        let end = usize::try_from(range.end).expect("range ends within source bound");
        assert!(
            source.is_char_boundary(start) && source.is_char_boundary(end),
            "range splits UTF-8 source: {range:?}"
        );
        checked += 1;
    }
    checked
}

#[test]
fn full_commonmark_and_gfm_corpora_parse_with_bounded_source_ranges() {
    let commonmark =
        commonmark_examples(include_str!("fixtures/markdown/corpus/commonmark_spec.txt"));
    let mut gfm = gfm_examples(include_str!("fixtures/markdown/corpus/gfm_spec.txt"));
    gfm.extend(gfm_examples(include_str!(
        "fixtures/markdown/corpus/gfm_pipe_tables.txt"
    )));
    gfm.extend(gfm_examples(include_str!(
        "fixtures/markdown/corpus/gfm_task_lists.txt"
    )));
    assert_eq!(commonmark.len(), 652);
    assert_eq!(gfm.len(), 315);

    let mut failed = Vec::new();
    let mut error_examples = Vec::new();
    let mut blocks = 0_usize;
    let mut headings = 0_usize;
    let mut links = 0_usize;
    let mut link_kinds = [0_usize; 3];
    let mut block_kinds = [0_usize; 7];
    let mut reference_candidates = 0_usize;
    let mut checked_ranges = 0_usize;
    let mut omitted_ranges = 0_usize;
    for (corpus, cases) in [("CommonMark", commonmark), ("GFM", gfm)] {
        for (number, source) in cases {
            let path = format!("docs/{corpus}-{number}.md");
            match MarkdownSyntaxProvider::default().analyze(SyntaxSource {
                path: &ProjectPath::new(path).expect("generated path is valid"),
                text: &source,
            }) {
                Ok(document) => {
                    let facts = document.markdown_facts().expect("Markdown facts");
                    if !facts.error_ranges().is_empty() {
                        error_examples.push(format!("{corpus} {number}"));
                    }
                    blocks += facts.blocks().len();
                    headings += facts.headings().len();
                    for block in facts.blocks() {
                        let index = match block.structure {
                            MarkdownBlockStructure::Heading => 0,
                            MarkdownBlockStructure::Paragraph => 1,
                            MarkdownBlockStructure::List => 2,
                            MarkdownBlockStructure::Table => 3,
                            MarkdownBlockStructure::BlockQuote => 4,
                            MarkdownBlockStructure::Code => 5,
                            MarkdownBlockStructure::LinkDefinition => 6,
                        };
                        block_kinds[index] += 1;
                    }
                    links += facts.links().len();
                    for link in facts.links() {
                        let index = match link.kind {
                            MarkdownLinkKind::Authored => 0,
                            MarkdownLinkKind::ReferenceDefinition => 1,
                            MarkdownLinkKind::ReferenceUse => 2,
                        };
                        link_kinds[index] += 1;
                    }
                    reference_candidates += facts.reference_candidates().len();
                    omitted_ranges += facts.omitted_ranges().len();
                    checked_ranges += assert_ranges_within_source(&source, &document);
                }
                Err(error) => failed.push(format!("{corpus} {number}: {error}")),
            }
        }
    }
    assert!(failed.is_empty(), "corpus extraction failures: {failed:?}");
    assert!(
        error_examples.is_empty(),
        "spec examples with parser errors: {error_examples:?}"
    );
    assert_eq!(omitted_ranges, 0, "standard Markdown corpus omissions");
    assert!(blocks > 0);
    assert!(links > 0);
    assert!(
        block_kinds.iter().all(|count| *count > 0),
        "block fact counts: {block_kinds:?}"
    );
    assert!(
        link_kinds.iter().all(|count| *count > 0),
        "link fact counts: {link_kinds:?}"
    );
    assert!(headings > 0);
    assert!(reference_candidates > 0);
    println!(
        "CommonMark accepted=652; GFM accepted=315; failed=0; parser_error_examples=0; omitted_ranges=0; headings={headings}; blocks={blocks}; block_kinds={block_kinds:?}; links={links}; link_kinds={link_kinds:?}; reference_candidates={reference_candidates}; checked_ranges={checked_ranges}"
    );
}

#[test]
fn commonmark_headings_and_setext_examples_keep_structure() {
    let atx = include_str!("fixtures/markdown/corpus/commonmark_atx.md");
    let document = analyze("docs/atx.md", atx);
    let facts = document.markdown_facts().expect("Markdown facts");
    assert_eq!(facts.headings().len(), 6);
    assert_eq!(
        facts
            .headings()
            .iter()
            .map(|heading| heading.level)
            .collect::<Vec<_>>(),
        [1, 2, 3, 4, 5, 6]
    );
    assert!(facts.error_ranges().is_empty());

    let setext = include_str!("fixtures/markdown/corpus/commonmark_setext.md");
    let document = analyze("docs/setext.md", setext);
    let facts = document.markdown_facts().expect("Markdown facts");
    assert_eq!(facts.headings().len(), 2);
    assert_eq!(facts.blocks().len(), 4);
    assert_eq!(
        facts
            .blocks()
            .iter()
            .filter(|block| block.structure == MarkdownBlockStructure::Heading)
            .count(),
        2
    );
    assert_eq!(
        facts
            .blocks()
            .iter()
            .filter(|block| block.structure == MarkdownBlockStructure::Paragraph)
            .count(),
        2
    );
}

#[test]
fn gfm_table_and_task_list_examples_keep_block_kinds() {
    let table = include_str!("fixtures/markdown/corpus/gfm_table.md");
    let table_document = analyze("docs/table.md", table);
    let table_facts = table_document.markdown_facts().expect("Markdown facts");
    assert_eq!(table_facts.blocks().len(), 1);
    assert_eq!(
        table_facts.blocks()[0].structure,
        MarkdownBlockStructure::Table
    );
    assert_eq!(table_facts.blocks()[0].kind, MarkdownBlockKind::Prose);

    let tasks = include_str!("fixtures/markdown/corpus/gfm_task_list.md");
    let task_document = analyze("docs/tasks.md", tasks);
    let task_facts = task_document.markdown_facts().expect("Markdown facts");
    assert_eq!(task_facts.blocks().len(), 3);
    assert!(
        task_facts
            .blocks()
            .iter()
            .all(|block| block.kind == MarkdownBlockKind::Prose)
    );
    assert_eq!(
        task_facts
            .blocks()
            .iter()
            .filter(|block| block.structure == MarkdownBlockStructure::List)
            .count(),
        1
    );
}

#[test]
fn mdx_keeps_code_markers_and_omits_only_authored_prose_markers() {
    let source = "# Guide\n\nUse `{Name}` and `<Thing>` safely.\n\n```rust\nfn item() {}\n```\n\nText {expression}\n\nimport Widget from 'widget'\n";
    let document = analyze("docs/guide.mdx", source);
    let facts = document.markdown_facts().expect("Markdown facts");
    let filtered = facts.for_mdx(source);
    assert_eq!(filtered.omitted_ranges().len(), 2);
    assert_eq!(filtered.blocks().len(), 3);
    assert!(
        filtered
            .blocks()
            .iter()
            .any(|block| block.kind == MarkdownBlockKind::Code)
    );
    assert_eq!(filtered.reference_candidates().len(), 2);
    assert!(filtered.heading_path(Some(usize::MAX)).is_empty());
    assert_eq!(filtered.heading_path(Some(0)).len(), 1);
}

#[test]
fn gfm_reference_link_example_keeps_authored_ranges() {
    let source = include_str!("fixtures/markdown/corpus/gfm_reference_link.md");
    let document = analyze("docs/references.md", source);
    let facts = document.markdown_facts().expect("Markdown facts");
    assert_eq!(facts.links().len(), 2);
    assert!(
        facts
            .links()
            .iter()
            .any(|link| link.kind == MarkdownLinkKind::ReferenceUse)
    );
    let definition = facts
        .links()
        .iter()
        .find(|link| link.kind == MarkdownLinkKind::ReferenceDefinition)
        .expect("reference definition");
    let range = definition
        .destination_range
        .expect("authored destination range");
    let start = usize::try_from(range.start).expect("destination range starts within fixture");
    let end = usize::try_from(range.end).expect("destination range ends within fixture");
    assert_eq!(
        source.get(start..end).expect("destination range is valid"),
        "/f&ouml;&ouml;"
    );
}

#[test]
fn malformed_and_crlf_no_final_newline_cases_keep_exact_ranges() {
    let malformed = include_str!("fixtures/markdown/corpus/malformed_heading.md");
    let malformed_document = analyze("docs/malformed.md", malformed);
    assert!(malformed_document.has_errors());
    assert!(
        !malformed_document
            .markdown_facts()
            .expect("Markdown facts")
            .error_ranges()
            .is_empty()
    );

    let source = "# Guide\r\n\r\nSee [install](/install#steps)";
    let document = analyze("docs/crlf.md", source);
    let facts = document.markdown_facts().expect("Markdown facts");
    assert_eq!(facts.blocks().len(), 2);
    assert_eq!(facts.blocks()[1].line, 3);
    assert_eq!(facts.blocks()[1].range.end, source.len() as u64);
    assert!(facts.error_ranges().is_empty());
}
