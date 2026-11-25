//! Every published field addresses its own source bytes.

use rift_protocol::documentation::{
    DocumentationBlock, DocumentationConfiguration, DocumentationLink, DocumentationLinkResolution,
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
