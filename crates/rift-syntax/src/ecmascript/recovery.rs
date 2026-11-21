//! Reparses a source whose document root is one error, leaving out the bodies that hold it.
//!
//! A malformed member list can keep the parser from resynchronizing. The root becomes an
//! error and every declaration in the file is inside it. The source before the first error
//! is read correctly, so its tokens name the braced body that holds the error. Parsing
//! again without that body's interior keeps every byte offset and loses only what the body
//! declares.

use tree_sitter::{Node, Parser, Tree};

use crate::ByteRange;

/// Most bodies left out of one source before recovery gives up.
const BODIES_MAX: usize = 64;

/// Parses `text`, recovering a document whose root is an error.
///
/// Returns the tree and whether bodies were left out. Recovery is accepted only when the
/// remaining source parses without any error; otherwise the first tree is returned as the
/// parser produced it. Each round parses the whole text once, at most `BODIES_MAX` times.
pub(crate) fn parse(parser: &mut Parser, text: &str) -> Option<(Tree, bool)> {
    let first = parser.parse(text, None)?;
    if !first.root_node().is_error() {
        return Some((first, false));
    }
    let lines = crate::embedded::line_starts(text);
    let mut omitted: Vec<ByteRange> = Vec::new();
    let mut recovered = None;
    let mut root_error_start = open_before_first_error(first.root_node());
    for _ in 0..BODIES_MAX {
        let Some(interior) = root_error_start.and_then(|open| interior(text, open)) else {
            break;
        };
        if omitted.contains(&interior) {
            break;
        }
        // A body that encloses earlier ones replaces them.
        omitted.retain(|range| !interior.encloses(*range));
        omitted.push(interior);
        omitted.sort_unstable_by_key(|range| range.start);
        let mut included = Vec::with_capacity(omitted.len() + 1);
        let mut start = 0_u64;
        for range in &omitted {
            included.push(crate::embedded::included_range(
                text,
                ByteRange {
                    start,
                    end: range.start,
                },
                &lines,
            ));
            start = range.end;
        }
        included.push(crate::embedded::included_range(
            text,
            ByteRange {
                start,
                end: text.len() as u64,
            },
            &lines,
        ));
        if parser.set_included_ranges(&included).is_err() {
            break;
        }
        let Some(tree) = parser.parse(text, None) else {
            break;
        };
        if !tree.root_node().has_error() {
            recovered = Some(tree);
            break;
        }
        root_error_start = open_before_first_error(tree.root_node());
    }
    // Later parses with this parser read the whole text again.
    let reset = parser.set_included_ranges(&[]);
    debug_assert!(reset.is_ok(), "an empty range list is always accepted");
    Some(match recovered {
        Some(tree) => (tree, true),
        None => (first, false),
    })
}

/// The opening brace of the innermost body still open at the first error below `root`.
///
/// Tokens before the first error were read correctly, so their braces nest as written.
/// `None` when the tree holds no error below its root, or when the first error is outside
/// every braced body or inside a template substitution.
fn open_before_first_error(root: Node<'_>) -> Option<u64> {
    let mut open: Vec<Option<u64>> = Vec::new();
    let mut cursor = root.walk();
    let mut descend = true;
    loop {
        let node = cursor.node();
        if descend {
            if node != root && (node.is_error() || node.is_missing()) {
                return open.last().copied().flatten();
            }
            if node.child_count() == 0 {
                match node.kind() {
                    "{" => open.push(Some(node.start_byte() as u64)),
                    "${" => open.push(None),
                    "}" => {
                        open.pop();
                    }
                    _ => {}
                }
            }
            if cursor.goto_first_child() {
                continue;
            }
        }
        if cursor.goto_next_sibling() {
            descend = true;
        } else if cursor.goto_parent() {
            descend = false;
        } else {
            return None;
        }
    }
}

/// The bytes between the brace at `open` and the brace that closes it.
///
/// Quoted strings, template literals and comments are skipped. A body this reading closes
/// at the wrong brace leaves a source that still fails to parse, which `parse` refuses.
fn interior(text: &str, open: u64) -> Option<ByteRange> {
    let bytes = text.as_bytes();
    let open = usize::try_from(open).ok()?;
    let mut depth = 0_usize;
    let mut index = open;
    while index < bytes.len() {
        match bytes[index] {
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    let start = open as u64 + 1;
                    let end = index as u64;
                    return (start < end).then_some(ByteRange { start, end });
                }
            }
            quote @ (b'\'' | b'"' | b'`') => {
                index += 1;
                while index < bytes.len() && bytes[index] != quote {
                    if bytes[index] == b'\\' {
                        index += 1;
                    }
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/')
                {
                    index += 1;
                }
                index += 1;
            }
            _ => {}
        }
        index += 1;
    }
    None
}
