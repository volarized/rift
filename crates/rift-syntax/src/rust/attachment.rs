//! Which preceding outer attributes and doc comments belong to a
//! declaration's span.
//!
//! In tree-sitter-rust, `#[...]` attributes and `///`/`/** */` doc comments
//! precede a declaration as separate siblings, not children. Rift treats a
//! symbol's span as the whole declaration, the way rustdoc and an editor's
//! go-to-definition do: replacing or inserting beside a symbol must not
//! splice between its documentation and its item.

use tree_sitter::Node;

/// Grammar field naming the marker that classifies a `line_comment` or
/// `block_comment` as an outer (declaration-attached) doc comment, from
/// tree-sitter-rust's node types. It is absent on inner docs (`//!`,
/// `/*! */`) and on plain comments, so reading it replaces re-deriving
/// rustdoc's own `///` / `////` / `/**` / `/***` / `/**/` lexing rules from
/// comment text.
const OUTER_DOC_MARKER_FIELD: &str = "outer";

/// Returns the byte offset where `node`'s whole declaration starts.
///
/// Walks previous siblings, extending the start over each that is an
/// `attribute_item` or an outer doc comment. The first sibling that fails
/// either test stops the walk, and `node`'s own start is the fallback.
///
/// A blank line between an attachable sibling and the declaration detaches
/// it, matching rustdoc's own attachment rule.
pub(super) fn declaration_start(node: Node<'_>, text: &str) -> usize {
    let mut front = node;
    while let Some(previous) = front.prev_sibling() {
        if !is_attached(previous) {
            break;
        }
        let Some(previous_text) = text.get(previous.byte_range()) else {
            break;
        };
        let Some(gap) = text.get(previous.end_byte()..front.start_byte()) else {
            break;
        };
        if !gap_permits_attachment(previous_text, gap) {
            break;
        }
        front = previous;
    }
    front.start_byte()
}

/// Reports whether `node` is an attribute or an outer doc comment, either
/// of which can attach to a following declaration.
fn is_attached(node: Node<'_>) -> bool {
    match node.kind() {
        "attribute_item" => true,
        "line_comment" | "block_comment" => {
            node.child_by_field_name(OUTER_DOC_MARKER_FIELD).is_some()
        }
        _ => false,
    }
}

/// Reports whether the whitespace between an attachable sibling and the
/// declaration holds at most one line break, so a blank line detaches it.
///
/// A `line_comment` node's own span already ends with the newline that
/// terminates its source line (tree-sitter-rust folds it in so consecutive
/// `///` lines read as adjacent); that swallowed break counts toward the
/// budget the same as one found in the gap, or a comment immediately
/// followed by a blank line would misread as attached.
fn gap_permits_attachment(previous_text: &str, gap: &str) -> bool {
    if !gap.chars().all(char::is_whitespace) {
        return false;
    }
    let swallowed_break = usize::from(previous_text.ends_with('\n'));
    gap.matches('\n').count() + swallowed_break <= 1
}

#[cfg(test)]
mod tests {
    use rift_core::ProjectPath;

    use super::super::{RustSource, RustSymbol, RustSyntaxProvider};

    fn path() -> ProjectPath {
        ProjectPath::new("src/lib.rs").expect("valid fixture path")
    }

    fn symbols(text: &str) -> Vec<RustSymbol> {
        RustSyntaxProvider::default()
            .analyze(RustSource {
                path: &path(),
                text,
            })
            .expect("fixture must parse")
            .symbols()
            .to_vec()
    }

    fn span_text<'a>(text: &'a str, symbol: &RustSymbol) -> &'a str {
        let start = usize::try_from(symbol.range.start).expect("fixture span fits usize");
        let end = usize::try_from(symbol.range.end).expect("fixture span fits usize");
        &text[start..end]
    }

    #[test]
    fn struct_span_includes_leading_doc_and_derive() {
        let text = "/// A beacon.\n#[derive(Debug)]\npub struct Beacon;\n";
        let symbols = symbols(text);
        assert_eq!(span_text(text, &symbols[0]), text.trim_end());
    }

    #[test]
    fn fn_span_includes_attribute_only_with_no_doc() {
        let text = "#[inline]\npub fn beacon() {}\n";
        let symbols = symbols(text);
        assert_eq!(span_text(text, &symbols[0]), text.trim_end());
    }

    #[test]
    fn enum_span_includes_multi_line_doc() {
        let text = "/// Level.\n/// Second line.\npub enum Level { Low, High }\n";
        let symbols = symbols(text);
        assert_eq!(span_text(text, &symbols[0]), text.trim_end());
    }

    #[test]
    fn block_comment_doc_attaches() {
        let text = "/** Beacon docs. */\npub struct Beacon;\n";
        let symbols = symbols(text);
        assert_eq!(span_text(text, &symbols[0]), text.trim_end());
    }

    #[test]
    fn plain_line_comment_does_not_attach() {
        let text = "// just a note\npub struct Beacon;\n";
        let symbols = symbols(text);
        assert_eq!(span_text(text, &symbols[0]), "pub struct Beacon;");
    }

    #[test]
    fn blank_line_detaches_leading_doc() {
        let text = "/// Orphaned by a blank line.\n\npub struct Beacon;\n";
        let symbols = symbols(text);
        assert_eq!(span_text(text, &symbols[0]), "pub struct Beacon;");
    }

    #[test]
    fn inner_doc_at_file_top_is_excluded_from_first_item() {
        let text = "//! Crate docs.\n\npub struct Beacon;\n";
        let symbols = symbols(text);
        assert_eq!(span_text(text, &symbols[0]), "pub struct Beacon;");
    }

    #[test]
    fn first_item_doc_with_no_preceding_sibling_is_included() {
        let text = "/// Beacon.\npub struct Beacon;\n";
        let symbols = symbols(text);
        assert_eq!(symbols[0].range.start, 0);
        assert_eq!(span_text(text, &symbols[0]), text.trim_end());
    }

    #[test]
    fn first_item_own_doc_after_file_level_inner_doc_starts_after_it() {
        let text = "//! Crate docs.\n\n/// Beacon.\npub struct Beacon;\n";
        let symbols = symbols(text);
        let expected_start = text
            .find("/// Beacon.")
            .expect("fixture contains its own doc");
        assert_eq!(
            symbols[0].range.start,
            u64::try_from(expected_start).expect("fits u64")
        );
    }

    #[test]
    fn adjacent_documented_items_do_not_overlap() {
        let text = "/// Doc A.\nstruct A;\n/// Doc B.\nstruct B;\n";
        let symbols = symbols(text);
        assert_eq!(symbols.len(), 2);
        assert!(symbols[0].range.end <= symbols[1].range.start);
        assert_eq!(span_text(text, &symbols[1]), "/// Doc B.\nstruct B;");
    }
}
