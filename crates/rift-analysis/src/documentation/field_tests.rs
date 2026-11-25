//! One field of one source never refuses a publication, and every published field
//! addresses its own source bytes.

use rift_protocol::documentation::{
    DocumentationBlock, DocumentationConfiguration, DocumentationLink, DocumentationLinkResolution,
    DocumentationWarningKind,
};
use rift_protocol::read::TextRange;

use super::collect_tests::input;
use super::{
    DocumentationCollection, DocumentationLimits, DocumentationSourceSet, collect_documentation,
};

fn collect(
    sources: &[(&str, &str)],
    configuration: &DocumentationConfiguration,
) -> DocumentationCollection {
    let limits = DocumentationLimits::from_configuration(configuration).expect("accepted bounds");
    let inputs = sources
        .iter()
        .map(|(path, text)| input(path, text))
        .collect();
    let set = DocumentationSourceSet::with_limits(inputs, &limits).expect("source set");
    collect_documentation(&set, &[])
        .unwrap_or_else(|error| panic!("one field refused the publication of {sources:?}: {error}"))
}

fn collect_one(path: &str, text: &str) -> DocumentationCollection {
    collect(&[(path, text)], &DocumentationConfiguration::default())
}

fn lowered_bounds() -> DocumentationConfiguration {
    DocumentationConfiguration {
        max_text: rift_protocol::configuration::ByteSize::from_bytes(6),
        max_heading_depth: 2,
        ..DocumentationConfiguration::default()
    }
}

fn exact<'text>(text: &'text str, range: &TextRange) -> &'text str {
    let start = usize::try_from(range.start).expect("range start");
    let end = usize::try_from(range.end).expect("range end");
    text.get(start..end)
        .unwrap_or_else(|| panic!("range {range:?} does not address {text:?}"))
}

fn block_of<'index>(
    collection: &'index DocumentationCollection,
    link: &DocumentationLink,
) -> &'index DocumentationBlock {
    collection
        .index()
        .blocks
        .iter()
        .find(|block| block.identity == link.block)
        .expect("link block")
}

fn block_texts<'text>(collection: &DocumentationCollection, text: &'text str) -> Vec<&'text str> {
    collection
        .index()
        .blocks
        .iter()
        .map(|block| exact(text, &block.range))
        .collect()
}

fn warning_count(collection: &DocumentationCollection, kind: DocumentationWarningKind) -> u64 {
    collection
        .index()
        .warnings
        .iter()
        .filter(|warning| warning.kind == kind)
        .map(|warning| warning.count)
        .sum()
}

/// Every link lies in its block and spells its syntax; every candidate spells its range.
fn assert_fields_address_source(collection: &DocumentationCollection, path: &str, text: &str) {
    for link in &collection.index().links {
        let block = block_of(collection, link);
        assert!(
            block.range.start <= link.range.start && link.range.end <= block.range.end,
            "link {link:?} outside block {:?} of {text:?}",
            block.range
        );
        let spelled = exact(text, &link.range);
        assert!(!spelled.is_empty(), "empty link {link:?} in {text:?}");
        if path == "guide.rst" {
            assert!(
                !spelled.ends_with(['\r', '\n']),
                "{spelled:?} holds a line ending of {text:?}"
            );
        } else {
            assert!(
                spelled.contains('[') && spelled.contains(']'),
                "{spelled:?} spells no Markdown link in {text:?}"
            );
        }
    }
    for candidate in &collection.index().unresolved_references {
        assert_eq!(exact(text, &candidate.range), candidate.authored);
    }
}

#[test]
fn rst_prose_after_a_literal_block_is_its_own_block_and_holds_its_links() {
    let text = "Example::\n\n  literal\nafter link_ here::\n\n  second\nlast http://a.test/b\n";
    let collection = collect_one("guide.rst", text);
    assert_eq!(
        block_texts(&collection, text),
        [
            "Example::\n\n  ",
            "literal",
            "after link_ here::\n\n  ",
            "second",
            "last http://a.test/b",
        ]
    );
    let links = &collection.index().links;
    assert_eq!(links.len(), 2);
    for (spelled, prose) in [
        ("link", "after link_ here::\n\n  "),
        ("http://a.test/b", "last http://a.test/b"),
    ] {
        let link = links
            .iter()
            .find(|link| exact(text, &link.range) == spelled)
            .unwrap_or_else(|| panic!("{spelled} missing from {links:#?}"));
        let block = block_of(&collection, link);
        assert_eq!(exact(text, &block.range), prose);
    }
    let first_lines: Vec<_> = collection
        .index()
        .blocks
        .iter()
        .map(|block| block.line)
        .collect();
    assert_eq!(first_lines, [1, 3, 4, 6, 7]);
    assert!(collection.index().warnings.is_empty());
}

#[test]
fn rst_links_after_a_literal_block_publish_in_every_nested_and_byte_shape() {
    for (text, spelled) in [
        ("* item::\n\n    literal\n  after link_\n", "link"),
        ("term\n  text::\n\n     literal\n  after link_\n", "link"),
        ("a::\n\n> quoted\nlink_\n", "link"),
        ("a::\n\n  literal\n| link_", "link"),
        ("a::\n\n  literal\n.. _b: http://c.test", "http://c.test"),
        ("a::\r\n\r\n  literal\r\nafter link_\r\n", "link"),
        ("\u{feff}é漢::\n\n\tlittéral\né漢 link_", "link"),
        ("a::\n\n  literal\n*emphasis* `two words`_", "two words"),
    ] {
        let collection = collect_one("guide.rst", text);
        assert_fields_address_source(&collection, "guide.rst", text);
        let links = &collection.index().links;
        let link = links
            .iter()
            .find(|link| exact(text, &link.range) == spelled)
            .unwrap_or_else(|| panic!("{spelled} missing from {links:#?} of {text:?}"));
        assert_eq!(exact(text, &link.range), spelled);
        assert_eq!(collection.index().coverage.omitted, 0);
    }
}

#[test]
fn rst_links_and_targets_at_a_crlf_line_end_resolve_as_at_a_lf_line_end() {
    let text = "Use name_\n\nUse `two words`_\n\nSee http://a.test/b\n\nAn _`inline`\n\nUse inline_\n\n.. _name:\n\n.. _two words: other.rst\n";
    let facts = |text: &str| {
        let collection = collect_one("guide.rst", text);
        assert_fields_address_source(&collection, "guide.rst", text);
        collection
            .index()
            .links
            .iter()
            .map(|link| {
                let resolved = matches!(
                    link.resolution,
                    DocumentationLinkResolution::Resolved { .. }
                );
                let spelled = exact(text, &link.range).to_owned();
                (spelled, link.authored.clone(), resolved)
            })
            .collect::<std::collections::BTreeSet<_>>()
    };
    let expected = [
        ("http://a.test/b", "http://a.test/b", false),
        ("inline", "#inline", true),
        ("name", "#name", true),
        ("two words", "other.rst", false),
    ]
    .map(|(spelled, authored, resolved)| (spelled.to_owned(), authored.to_owned(), resolved));
    assert_eq!(facts(text), expected.clone().into());
    assert_eq!(facts(&text.replace('\n', "\r\n")), expected.into());
}

#[test]
fn a_destination_past_the_text_bound_leaves_out_one_counted_link() {
    let image = format!("data:image/png;base64,{}", "A".repeat(4_096));
    let badge = format!("# Guide\n\n![badge]({image}) and [kept](other.md).\n");
    let target = format!("See long_ and kept_.\n\n.. _long: {image}\n.. _kept: other.rst\n");
    let code = format!("`{}` and `kept`\n", "x".repeat(4_097));
    let collection = collect(
        &[
            ("badge.md", badge.as_str()),
            ("code.md", code.as_str()),
            ("other.md", "Other.\n"),
            ("target.rst", target.as_str()),
        ],
        &DocumentationConfiguration::default(),
    );
    let index = collection.index();
    assert_eq!(index.coverage.selected, 4);
    assert_eq!(index.coverage.omitted, 0);
    let authored: Vec<_> = index
        .links
        .iter()
        .map(|link| link.authored.as_str())
        .collect();
    assert!(authored.contains(&"other.md"), "{authored:?}");
    assert!(authored.contains(&"other.rst"), "{authored:?}");
    assert!(!authored.contains(&image.as_str()));
    let candidates: Vec<_> = index
        .unresolved_references
        .iter()
        .map(|candidate| candidate.authored.as_str())
        .collect();
    assert_eq!(candidates, ["kept"]);
    let warnings: Vec<_> = index
        .warnings
        .iter()
        .map(|warning| (warning.kind, warning.count))
        .collect();
    assert_eq!(warnings, [(DocumentationWarningKind::LimitExceeded, 1); 3]);
}

#[test]
fn a_blank_rst_title_is_left_out_of_the_heading_path() {
    let text = "\u{2028}\n-\n\nBody link_.\n";
    let collection = collect_one("guide.rst", text);
    assert_fields_address_source(&collection, "guide.rst", text);
    assert!(
        collection
            .index()
            .blocks
            .iter()
            .all(|block| block.heading_path.is_empty())
    );
    assert_eq!(
        warning_count(&collection, DocumentationWarningKind::OmittedRange),
        1
    );
}

#[test]
fn a_heading_past_the_depth_bound_is_left_out_once_per_source() {
    let text = "# A\n\n## B\n\n### C\n\nOne.\n\nTwo.\n";
    let collection = collect(&[("guide.md", text)], &lowered_bounds());
    let deepest = collection
        .index()
        .blocks
        .iter()
        .flat_map(|block| &block.heading_path)
        .map(|heading| heading.level)
        .max();
    assert_eq!(deepest, Some(2));
    assert!(block_texts(&collection, text).contains(&"Two.\n"));
    assert_eq!(
        warning_count(&collection, DocumentationWarningKind::LimitExceeded),
        1
    );
}

/// Pieces that open, close, continue, and interrupt reStructuredText and Markdown blocks.
const PIECES: &[&str] = &[
    "\n",
    "\n\n",
    "\r\n",
    "\r",
    "  ",
    "    ",
    "\t",
    "\u{feff}",
    "\u{a0}",
    "\u{2028}",
    "\u{c}",
    "é漢 ",
    "text ",
    "`link <http://a.test/b>`_ ",
    "http://a.test/b",
    "name_ ",
    "`two words`_",
    "anon__ ",
    "`x`__ ",
    ".. _name: http://d.test\n",
    ".. _two words:\n",
    ".. |sub| image:: a.png\n",
    "|sub|_ ",
    ".. include:: x.rst\n",
    ".. note::\n\n   a_ http://x.test\n",
    ".. code-block:: python\n\n   x = a_\n",
    "::\n\n   lit_ http://x.test\n",
    "Title_\n======\n",
    "Head\n----\n",
    "Deep\n~~~~\n",
    "* ",
    "- ",
    "1. ",
    "term\n  def_ \n",
    ":field: val_\n",
    "| line_\n",
    "> ",
    ">>> a_\n",
    "[1]_ ",
    ".. [1] foot_\n",
    "__ http://an.test\n",
    "*emph* ",
    "``lit_``",
    ":role:`x`",
    "<http://q.test>",
    "a@b.test",
    "_`inline`",
    "\\",
    "_",
    "`",
    "::",
    "..",
    "| a | b |\n|---|---|\n| [x](y) | `z` |\n",
    "# H [x](y)\n",
    "## H\n",
    "### H\n",
    "[t](u \"v\")",
    "[t][l]",
    "[l]: /u#f \"t\"\n",
    "[l]",
    "[l][]",
    "![i](j)",
    "[![i](j)](k)",
    "[a\nb](c\n)",
    "`code`",
    "``a\nb``",
    "<a href=\"x\">y</a>",
    "```python\nx\n```\n",
    "~~~",
    "[",
    "]",
    "(",
    ")",
    "!",
    "<",
    "<!-- c -->\n",
    "[x](<y z>)",
    "[x](",
    "---\n",
    "===\n",
    "+---+\n| a_|\n+---+\n",
    "=== ===\na_  b_\n=== ===\n",
];
const GENERATED_SOURCES: usize = 3_000;
const GENERATED_PIECES_MAX: usize = 14;

/// A fixed xorshift sequence, so a failure names the same text on every run.
struct Sequence(u64);

impl Sequence {
    fn below(&mut self, bound: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        usize::try_from(self.0 % bound as u64).expect("bounded value")
    }

    fn text(&mut self) -> String {
        let count = 1 + self.below(GENERATED_PIECES_MAX);
        (0..count)
            .map(|_| PIECES[self.below(PIECES.len())])
            .collect()
    }
}

#[test]
fn generated_sources_publish_fields_that_address_their_bytes() {
    let default = DocumentationConfiguration::default();
    let lowered = lowered_bounds();
    let mut sequence = Sequence(88_172_645_463_325_252);
    for ordinal in 0..GENERATED_SOURCES {
        let path = ["guide.rst", "guide.md", "guide.mdx"][ordinal % 3];
        let text = sequence.text();
        let collection = collect(&[(path, &text)], &default);
        assert_fields_address_source(&collection, path, &text);
        let bounded = collect(&[(path, &text)], &lowered);
        assert_fields_address_source(&bounded, path, &text);
        assert_eq!(
            block_texts(&bounded, &text),
            block_texts(&collection, &text),
            "a lowered field bound lost a block of {text:?}"
        );
    }
}
