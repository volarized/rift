//! Retains literal module ranges witnessed by the original Rust token-tree rows.

use std::collections::BTreeMap;

use super::{ByteRange, SyntaxNode};

pub(super) fn capture(
    source: &str,
    nodes: &[SyntaxNode],
    invocations: &mut [(ByteRange, Vec<ByteRange>)],
) {
    let indices = invocations
        .iter()
        .enumerate()
        .map(|(index, (range, _))| (*range, index))
        .collect::<BTreeMap<_, _>>();
    let mut previous = BTreeMap::<usize, usize>::new();
    for (index, node) in nodes.iter().enumerate() {
        let Some(parent_index) = node.parent else {
            continue;
        };
        let previous_index = previous.insert(parent_index, index);
        let Some(parent) = nodes
            .get(parent_index)
            .filter(|node| node.kind == "token_tree")
        else {
            continue;
        };
        let Some(invocation) = parent.parent.and_then(|index| nodes.get(index)) else {
            continue;
        };
        let Some(index) = indices.get(&invocation.range).copied() else {
            continue;
        };
        if node.kind != "identifier" {
            continue;
        }
        let start = previous_index.map_or(parent.range.start.saturating_add(1), |index| {
            nodes[index].range.end
        });
        let Some(prefix) = text(
            source,
            ByteRange {
                start,
                end: node.range.start,
            },
        ) else {
            continue;
        };
        let Some(keyword) = prefix
            .strip_suffix("mod ")
            .or_else(|| prefix.trim_end().strip_suffix("mod"))
        else {
            continue;
        };
        let before = keyword.trim();
        if !["", ";", "pub", ";pub", "; pub"].contains(&before) {
            continue;
        }
        let Some(tail) = text(
            source,
            ByteRange {
                start: node.range.end,
                end: parent.range.end,
            },
        ) else {
            continue;
        };
        let spaces = tail.len() - tail.trim_start().len();
        if !tail.trim_start().starts_with(';') {
            continue;
        }
        let leading = prefix.find("mod").unwrap_or(prefix.len());
        let Some(start) = start.checked_add(u64::try_from(leading).unwrap_or(u64::MAX)) else {
            continue;
        };
        let Some(end) = node
            .range
            .end
            .checked_add(u64::try_from(spaces + 1).unwrap_or(u64::MAX))
        else {
            continue;
        };
        let range = ByteRange { start, end };
        invocations[index].1.push(range);
    }
    for (invocation, modules) in invocations {
        let Some(mut context) = Context::new(source, *invocation) else {
            modules.clear();
            continue;
        };
        modules.retain(|module| context.direct(*module));
    }
}

fn text(source: &str, range: ByteRange) -> Option<&str> {
    source.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
}

// The source digest and original node rows establish the capture. Restoration checks
// the literal bytes and rejects ranges inside strings, comments, or nested groups.
pub(crate) struct Context<'source> {
    source: &'source str,
    invocation: ByteRange,
    offset: usize,
    stack: Vec<u8>,
    boundary: Option<usize>,
    valid: bool,
}

impl<'source> Context<'source> {
    pub(crate) fn new(source: &'source str, invocation: ByteRange) -> Option<Self> {
        text(source, invocation)?;
        Some(Self {
            source,
            invocation,
            offset: 0,
            stack: Vec::new(),
            boundary: None,
            valid: true,
        })
    }

    pub(crate) fn direct(&mut self, module: ByteRange) -> bool {
        if !self.valid || !module_name(self.source, module) {
            return false;
        }
        let Some(prefix) = text(
            self.source,
            ByteRange {
                start: self.invocation.start,
                end: module.start,
            },
        ) else {
            self.valid = false;
            return false;
        };
        if self.offset > prefix.len() || !self.scan(prefix) {
            self.valid = false;
            return false;
        }
        if self.stack.as_slice() != b"{" {
            return false;
        }
        let Some(boundary) = self.boundary else {
            return false;
        };
        let visibility = prefix[boundary..].split_whitespace().collect::<String>();
        ["", "pub", "pub(crate)", "pub(self)", "pub(super)"].contains(&visibility.as_str())
    }

    fn scan(&mut self, prefix: &str) -> bool {
        let bytes = prefix.as_bytes();
        while self.offset < bytes.len() {
            match bytes[self.offset] {
                b'"' => {
                    self.offset += 1;
                    let mut ended = false;
                    while self.offset < bytes.len() {
                        match bytes[self.offset] {
                            b'\\' => self.offset += 2,
                            b'"' => {
                                self.offset += 1;
                                ended = true;
                                break;
                            }
                            _ => self.offset += 1,
                        }
                    }
                    if !ended {
                        return false;
                    }
                }
                b'\'' => return false,
                b'/' if bytes.get(self.offset + 1) == Some(&b'/') => {
                    self.offset += 2;
                    while self.offset < bytes.len() && bytes[self.offset] != b'\n' {
                        self.offset += 1;
                    }
                    if self.stack.len() == 1 {
                        self.boundary = Some(self.offset);
                    }
                }
                b'/' if bytes.get(self.offset + 1) == Some(&b'*') => {
                    let Some(end) = prefix
                        .get(self.offset + 2..)
                        .and_then(|rest| rest.find("*/"))
                    else {
                        return false;
                    };
                    // Nested comments remain unsupported by this literal context check.
                    if prefix[self.offset + 2..self.offset + 2 + end].contains("/*") {
                        return false;
                    }
                    self.offset += end + 4;
                    if self.stack.len() == 1 {
                        self.boundary = Some(self.offset);
                    }
                }
                open @ (b'(' | b'[' | b'{') => {
                    self.stack.push(open);
                    self.offset += 1;
                    if self.stack.len() == 1 && open == b'{' {
                        self.boundary = Some(self.offset);
                    }
                }
                close @ (b')' | b']' | b'}') => {
                    let expected = match close {
                        b')' => b'(',
                        b']' => b'[',
                        _ => b'{',
                    };
                    if self.stack.pop() != Some(expected) {
                        return false;
                    }
                    self.offset += 1;
                    if self.stack.len() == 1 && close == b'}' {
                        self.boundary = Some(self.offset);
                    }
                }
                b';' if self.stack.len() == 1 => {
                    self.offset += 1;
                    self.boundary = Some(self.offset);
                }
                _ => self.offset += 1,
            }
        }
        true
    }
}

fn module_name(source: &str, module: ByteRange) -> bool {
    let Some(item) = text(source, module) else {
        return false;
    };
    let Some(name) = item
        .strip_prefix("mod")
        .and_then(|rest| rest.strip_suffix(';'))
    else {
        return false;
    };
    if !name.starts_with(char::is_whitespace) {
        return false;
    }
    let name = name.trim();
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        || !name.as_bytes()[0].is_ascii_alphabetic() && name.as_bytes()[0] != b'_'
    {
        return false;
    }
    true
}
