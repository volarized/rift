//! Retains literal module ranges witnessed by the original Rust token-tree rows.

use std::collections::BTreeMap;

use super::{ByteRange, SyntaxNode};

pub(super) fn capture(
    source: &str,
    nodes: &[SyntaxNode],
    invocations: &mut [super::MacroInvocation],
) {
    let indices = invocations
        .iter()
        .enumerate()
        .map(|(index, (range, _, _))| (*range, index))
        .collect::<BTreeMap<_, _>>();
    let mut previous = BTreeMap::<usize, usize>::new();
    let mut direct = BTreeMap::<usize, Vec<ByteRange>>::new();
    let mut unknown = std::collections::BTreeSet::new();
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
        direct.entry(index).or_default().push(node.range);
        if node.has_error || parent.has_error || invocation.has_error {
            unknown.insert(index);
        }
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
    for (index, (invocation, modules, children)) in invocations.iter_mut().enumerate() {
        let ranges = direct.remove(&index).unwrap_or_default();
        if !unknown.contains(&index)
            && direct_children_valid(source, *invocation, &ranges, nodes.len())
        {
            *children = Some(ranges);
        }
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
        self.scan_to(prefix, None)
    }

    fn scan_to(&mut self, prefix: &str, depth_max: Option<usize>) -> bool {
        let bytes = prefix.as_bytes();
        while self.offset < bytes.len() {
            match raw_string_end(prefix, self.offset) {
                Ok(Some(end)) => {
                    self.offset = end;
                    continue;
                }
                Err(()) => return false,
                Ok(None) => {}
            }
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
                b'\'' => {
                    let Some(end) = apostrophe_end(prefix, self.offset) else {
                        return false;
                    };
                    self.offset = end;
                }
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
                    if depth_max.is_some_and(|limit| self.stack.len() > limit) {
                        return false;
                    }
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
                    if depth_max.is_some() && self.stack.is_empty() {
                        return true;
                    }
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

/// Checks complete direct named token ranges against the original invocation text.
pub(crate) fn direct_children_valid(
    source: &str,
    invocation: ByteRange,
    children: &[ByteRange],
    depth_max: usize,
) -> bool {
    let Some(invoked) = text(source, invocation) else {
        return false;
    };
    let Some((name, body)) = invoked.split_once('!') else {
        return false;
    };
    if !identifier(name.trim()) {
        return false;
    }
    let body = body.trim_start();
    if group_end(body, depth_max) != Some(body.len()) {
        return false;
    }
    let Some(start) = invoked.len().checked_sub(body.len()).and_then(|offset| {
        invocation
            .start
            .checked_add(u64::try_from(offset + 1).ok()?)
    }) else {
        return false;
    };
    let Some(end) = invocation.end.checked_sub(1) else {
        return false;
    };
    let mut previous = start;
    for child in children {
        if child.start < previous || child.start >= child.end || child.end > end {
            return false;
        }
        let Some(gap) = text(
            source,
            ByteRange {
                start: previous,
                end: child.start,
            },
        ) else {
            return false;
        };
        let Some(token) = text(source, *child) else {
            return false;
        };
        if !anonymous_gap(gap) || !named_token(token, depth_max) {
            return false;
        }
        if source
            .get(
                usize::try_from(child.start.saturating_sub(1)).unwrap_or(usize::MAX)
                    ..usize::try_from(child.start).unwrap_or(usize::MAX),
            )
            .is_some_and(|before| before.bytes().all(identifier_byte))
            && token.bytes().next().is_some_and(identifier_byte)
        {
            return false;
        }
        previous = child.end;
    }
    text(
        source,
        ByteRange {
            start: previous,
            end,
        },
    )
    .is_some_and(anonymous_gap)
}

fn identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn identifier(value: &str) -> bool {
    let value = value.strip_prefix("r#").unwrap_or(value);
    value
        .as_bytes()
        .first()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        && value.bytes().all(identifier_byte)
}

fn anonymous_gap(value: &str) -> bool {
    let mut rest = value;
    while !rest.is_empty() {
        rest = rest.trim_start_matches(|character: char| character.is_ascii_whitespace());
        if rest.is_empty() {
            break;
        }
        let length = rest
            .bytes()
            .take_while(|byte| identifier_byte(*byte))
            .count();
        if length != 0 {
            // Exact unnamed tokens in tree-sitter-rust's token_tree grammar.
            if !matches!(
                &rest[..length],
                "as" | "async"
                    | "await"
                    | "break"
                    | "const"
                    | "continue"
                    | "default"
                    | "enum"
                    | "fn"
                    | "for"
                    | "gen"
                    | "if"
                    | "impl"
                    | "let"
                    | "loop"
                    | "match"
                    | "mod"
                    | "pub"
                    | "return"
                    | "static"
                    | "struct"
                    | "trait"
                    | "type"
                    | "union"
                    | "unsafe"
                    | "use"
                    | "where"
                    | "while"
            ) {
                return false;
            }
            rest = &rest[length..];
        } else {
            let byte = rest.as_bytes()[0];
            if !b"!#$%&*+,-./:;<=>?@^|~".contains(&byte)
                || rest.starts_with("//")
                || rest.starts_with("/*")
            {
                return false;
            }
            rest = &rest[1..];
        }
    }
    true
}

fn named_token(value: &str, depth_max: usize) -> bool {
    if identifier(value) {
        return true;
    }
    if value
        .as_bytes()
        .first()
        .is_some_and(|byte| b"([{\"".contains(byte))
    {
        if value.starts_with('"') {
            return quoted_end(value, 0) == Some(value.len());
        }
        return group_end(value, depth_max) == Some(value.len());
    }
    if value.starts_with("//") {
        return value
            .find('\n')
            .is_none_or(|end| end == value.len().saturating_sub(1));
    }
    if value.starts_with("/*") {
        return value.ends_with("*/") && !value[2..value.len().saturating_sub(2)].contains("/*");
    }
    if let Ok(Some(end)) = raw_string_end(value, 0) {
        return end == value.len();
    }
    if let Some(character) = value
        .strip_prefix('b')
        .filter(|rest| rest.starts_with('\''))
    {
        return apostrophe_end(character, 0) == Some(character.len());
    }
    if let Some(string) = value
        .strip_prefix('b')
        .or_else(|| value.strip_prefix('c'))
        .filter(|rest| rest.starts_with('"'))
    {
        return quoted_end(string, 0) == Some(string.len());
    }
    if value.starts_with('\'') {
        return apostrophe_end(value, 0) == Some(value.len());
    }
    value.as_bytes().first().is_some_and(u8::is_ascii_digit)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.+-".contains(&byte))
}

fn quoted_end(source: &str, start: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut offset = start.checked_add(1)?;
    while offset < bytes.len() {
        match bytes[offset] {
            b'\\' => offset = offset.checked_add(2)?,
            b'"' => return offset.checked_add(1),
            _ => offset += 1,
        }
    }
    None
}

fn apostrophe_end(source: &str, start: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let content = start.checked_add(1)?;
    let mut end = content;
    if bytes.get(content) == Some(&b'\\') {
        end = content.checked_add(2)?;
        match bytes.get(content + 1)? {
            b'x' => {
                let tail = source.get(end..end.checked_add(2)?)?;
                if !tail.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return None;
                }
                end += 2;
            }
            b'u' if bytes.get(end) == Some(&b'{') => {
                let tail = source.get(end + 1..)?;
                let close = tail.find('}')?;
                if close == 0 || !tail[..close].bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return None;
                }
                end += close + 2;
            }
            b'u' => {
                let tail = source.get(end..end.checked_add(4)?)?;
                if !tail.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return None;
                }
                end += 4;
            }
            _ => {}
        }
    } else {
        let character = source.get(content..)?.chars().next()?;
        if character == '\n' || character == '\r' {
            return None;
        }
        end += character.len_utf8();
    }
    if bytes.get(end) == Some(&b'\'') {
        return end.checked_add(1);
    }
    if bytes.get(content) == Some(&b'\\') {
        return None;
    }
    end = content;
    while bytes.get(end).is_some_and(|byte| identifier_byte(*byte)) {
        end += 1;
    }
    identifier(source.get(content..end)?).then_some(end)
}

fn raw_string_end(source: &str, start: usize) -> Result<Option<usize>, ()> {
    let bytes = source.as_bytes();
    let mut offset = start;
    if matches!(bytes.get(offset), Some(b'b' | b'c')) {
        offset += 1;
    }
    if bytes.get(offset) != Some(&b'r') {
        return Ok(None);
    }
    offset += 1;
    let hashes = offset;
    while bytes.get(offset) == Some(&b'#') {
        offset += 1;
    }
    if bytes.get(offset) != Some(&b'"') {
        return Ok(None);
    }
    let count = offset - hashes;
    if count > usize::from(u8::MAX) {
        return Err(());
    }
    offset += 1;
    while offset < bytes.len() {
        if bytes[offset] == b'"'
            && bytes
                .get(offset + 1..offset + 1 + count)
                .is_some_and(|tail| tail.iter().all(|byte| *byte == b'#'))
        {
            return Ok(Some(offset + 1 + count));
        }
        offset += 1;
    }
    Err(())
}

fn group_end(source: &str, depth_max: usize) -> Option<usize> {
    if !source
        .as_bytes()
        .first()
        .is_some_and(|byte| b"([{".contains(byte))
    {
        return None;
    }
    let mut context = Context::new(
        source,
        ByteRange {
            start: 0,
            end: u64::try_from(source.len()).ok()?,
        },
    )?;
    context
        .scan_to(source, Some(depth_max))
        .then_some(context.offset)
        .filter(|_| context.stack.is_empty())
}
