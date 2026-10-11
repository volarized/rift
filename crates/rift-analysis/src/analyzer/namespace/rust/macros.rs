//! Checks captured item-repetition macros without expanding source.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rift_syntax::{ByteRange, SyntaxSymbol};

use super::super::SelectedFile;

pub(super) type Scope = Arc<BTreeMap<String, bool>>;

type Inventory<'a> = BTreeMap<&'a str, &'a SelectedFile<'a>>;
type Child = (String, Vec<String>, Scope);

fn text_range(source: &str, range: ByteRange) -> Option<&str> {
    source.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
}

// Every token borrows the admitted source. Unsupported literal forms refuse proof.
fn tokens(source: &str) -> Option<Vec<&str>> {
    let mut output = Vec::new();
    let mut offset = 0;
    let bytes = source.as_bytes();
    while offset < bytes.len() {
        if bytes[offset].is_ascii_whitespace() {
            offset += 1;
            continue;
        }
        let start = offset;
        match bytes[offset] {
            b'"' => {
                offset += 1;
                loop {
                    match *bytes.get(offset)? {
                        b'\\' => offset = offset.checked_add(2)?,
                        b'"' => {
                            offset += 1;
                            break;
                        }
                        _ => offset += 1,
                    }
                }
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                offset += 1;
                while bytes
                    .get(offset)
                    .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
                {
                    offset += 1;
                }
            }
            b'(' | b')' | b'{' | b'}' | b'[' | b']' | b'$' | b'*' | b':' | b';' | b'#' | b'!'
            | b',' | b'=' | b'>' => offset += 1,
            _ => return None,
        }
        output.push(source.get(start..offset)?);
    }
    Some(output)
}

fn consume(input: &mut &[&str], expected: &[&str]) -> bool {
    if input.starts_with(expected) {
        *input = &input[expected.len()..];
        true
    } else {
        false
    }
}

fn group<'tokens, 'source>(
    input: &mut &'tokens [&'source str],
    open: &str,
    close: &str,
) -> Option<&'tokens [&'source str]> {
    if !consume(input, &[open]) {
        return None;
    }
    let mut depth = 1_usize;
    for (index, token) in input.iter().enumerate() {
        if *token == open {
            depth = depth.checked_add(1)?;
        } else if *token == close {
            depth -= 1;
            if depth == 0 {
                let contents = &input[..index];
                *input = &input[index + 1..];
                return Some(contents);
            }
        }
    }
    None
}

fn attribute<'a>(input: &mut &'a [&'a str]) -> Option<&'a [&'a str]> {
    if !consume(input, &["#"]) {
        return None;
    }
    group(input, "[", "]")
}

fn cfg_attribute(input: &[&str]) -> bool {
    let Some((name, mut rest)) = input.split_first() else {
        return false;
    };
    let Some(body) = group(&mut rest, "(", ")") else {
        return false;
    };
    if !rest.is_empty()
        || body
            .iter()
            .any(|token| ["$", "!", "{", "}", "[", "]", ";"].contains(token))
    {
        return false;
    }
    *name == "cfg"
        || (*name == "cfg_attr"
            && body.starts_with(&["docsrs", ",", "doc", "("])
            && body.last() == Some(&")"))
}

fn transparent_definition(source: &str, symbol: &SyntaxSymbol, scope: &Scope) -> bool {
    let prefix = ByteRange {
        start: symbol.range.start,
        end: symbol.item_range.start,
    };
    let Some(prefix) = text_range(source, prefix).and_then(tokens) else {
        return false;
    };
    let mut prefix = prefix.as_slice();
    while !prefix.is_empty() {
        let Some(attribute) = attribute(&mut prefix) else {
            return false;
        };
        if !attribute.starts_with(&["doc", "("]) || attribute.last() != Some(&")") {
            return false;
        }
    }
    let Some(source) = text_range(source, symbol.item_range) else {
        return false;
    };
    let Some(tokens) = tokens(source) else {
        return false;
    };
    let mut input = tokens.as_slice();
    if !consume(
        &mut input,
        &[
            "macro_rules",
            "!",
            &symbol.name,
            "{",
            "(",
            "$",
            "(",
            "$",
            "item",
            ":",
            "item",
            ")",
            "*",
            ")",
            "=",
            ">",
            "{",
        ],
    ) {
        return false;
    }
    let repeated = consume(&mut input, &["$", "("]);
    let mut attributes = 0;
    while input.first() == Some(&"#") {
        let Some(value) = attribute(&mut input) else {
            return false;
        };
        if !cfg_attribute(value) {
            return false;
        }
        attributes += 1;
    }
    if attributes == 0 {
        return false;
    }
    if repeated {
        if !consume(&mut input, &["$", "item", ")", "*", "}"]) {
            return false;
        }
    } else {
        let Some(&target) = input.first() else {
            return false;
        };
        if target == symbol.name
            || scope.get(target) != Some(&true)
            || !consume(
                &mut input,
                &[target, "!", "{", "$", "(", "$", "item", ")", "*", "}", "}"],
            )
        {
            return false;
        }
    }
    let _ = consume(&mut input, &[";"]);
    consume(&mut input, &["}"]) && input.is_empty()
}

fn macro_use(source: &str, symbol: &SyntaxSymbol) -> bool {
    let prefix = ByteRange {
        start: symbol.range.start,
        end: symbol.item_range.start,
    };
    let Some(tokens) = text_range(source, prefix).and_then(tokens) else {
        return false;
    };
    let mut input = tokens.as_slice();
    let mut imported = false;
    while !input.is_empty() {
        let Some(value) = attribute(&mut input) else {
            return false;
        };
        if value == ["macro_use"] {
            if imported {
                return false;
            }
            imported = true;
        } else if !value.starts_with(&["doc", "("]) || value.last() != Some(&")") {
            return false;
        }
    }
    imported
}

fn invocation_modules(source: &str) -> Option<(&str, Vec<String>)> {
    let tokens = tokens(source)?;
    let mut input = tokens.as_slice();
    let name = *input.first()?;
    if !super::crate_name_is_valid(name) || !consume(&mut input, &[name, "!", "{"]) {
        return None;
    }
    let mut modules = Vec::new();
    while input.first() != Some(&"}") {
        if consume(&mut input, &["pub"]) && input.first() == Some(&"(") {
            let visibility = group(&mut input, "(", ")")?;
            if ![&["crate"][..], &["self"][..], &["super"][..]].contains(&visibility) {
                return None;
            }
        }
        if !consume(&mut input, &["mod"]) {
            return None;
        }
        let module = *input.first()?;
        if !super::crate_name_is_valid(module) || !consume(&mut input, &[module, ";"]) {
            return None;
        }
        modules.push(module.to_owned());
    }
    (consume(&mut input, &["}"]) && input.is_empty()).then_some((name, modules))
}

fn child_path(name: &str, directory: &str, inventory: &Inventory<'_>) -> Option<String> {
    let stem = if directory.is_empty() {
        name.to_owned()
    } else {
        format!("{directory}/{name}")
    };
    let candidates = [format!("{stem}.rs"), format!("{stem}/mod.rs")];
    let mut admitted = candidates
        .into_iter()
        .filter(|path| inventory.contains_key(path.as_str()));
    let path = admitted.next()?;
    admitted.next().is_none().then_some(path)
}

fn child_directory(path: &str) -> &str {
    path.strip_suffix("/mod.rs")
        .or_else(|| path.strip_suffix(".rs"))
        .unwrap_or(path)
}

#[derive(Clone, Copy)]
enum Event<'a> {
    Symbol(&'a SyntaxSymbol),
    Invocation(&'a rift_syntax::MacroInvocation),
}

impl Event<'_> {
    fn start(self) -> u64 {
        match self {
            Self::Symbol(symbol) => symbol.item_range.start,
            Self::Invocation((range, _, _)) => range.start,
        }
    }
}

fn events<'a>(file: &'a SelectedFile<'a>) -> Vec<Event<'a>> {
    let mut events = file
        .syntax
        .symbols()
        .iter()
        .filter(|symbol| symbol.container.is_none() && ["macro", "module"].contains(&symbol.kind))
        .map(Event::Symbol)
        .collect::<Vec<_>>();
    events.extend(
        file.syntax
            .macro_invocations()
            .into_iter()
            .flatten()
            .map(Event::Invocation),
    );
    events.sort_by_key(|event| event.start());
    events
}

struct Context<'a> {
    inventory: &'a Inventory<'a>,
    originals: &'a BTreeMap<&'a str, &'a str>,
    depth_max: usize,
}

impl Context<'_> {
    fn exports(
        &self,
        path: &str,
        directory: &str,
        mut scope: Scope,
        visiting: &mut BTreeSet<String>,
    ) -> Option<Scope> {
        if visiting.len() >= self.depth_max || !visiting.insert(path.to_owned()) {
            return None;
        }
        let result = self.file_exports(path, directory, &mut scope, visiting);
        visiting.remove(path);
        result.map(|()| scope)
    }

    fn file_exports(
        &self,
        path: &str,
        directory: &str,
        scope: &mut Scope,
        visiting: &mut BTreeSet<String>,
    ) -> Option<()> {
        let file = self.inventory.get(path)?;
        if self.originals.get(path).copied() != Some(file.source)
            || !super::source_is_valid(file.syntax, file.source)
            || file.syntax.macro_invocations().is_none()
        {
            return None;
        }
        for event in events(file) {
            match event {
                Event::Symbol(symbol) if symbol.kind == "macro" => {
                    let transparent = transparent_definition(file.source, symbol, scope);
                    Arc::make_mut(scope).insert(symbol.name.clone(), transparent);
                }
                Event::Symbol(symbol) if macro_use(file.source, symbol) => {
                    if !super::module_has_default_path(symbol) || symbol.body_range.is_some() {
                        return None;
                    }
                    let path = child_path(&symbol.name, directory, self.inventory)?;
                    *scope =
                        self.exports(&path, child_directory(&path), Arc::clone(scope), visiting)?;
                }
                Event::Invocation((range, _, _)) => {
                    let (name, _) = invocation_modules(text_range(file.source, *range)?)?;
                    if scope.get(name) != Some(&true) {
                        return None;
                    }
                }
                Event::Symbol(_) => {}
            }
        }
        Some(())
    }
}

pub(super) fn children<'a>(
    file: &'a SelectedFile<'a>,
    directory: &str,
    inventory: &'a Inventory<'a>,
    originals: &'a BTreeMap<&'a str, &'a str>,
    mut scope: Scope,
    depth_max: usize,
) -> Vec<Child> {
    let context = Context {
        inventory,
        originals,
        depth_max,
    };
    let ordinary = super::module_children(file.syntax, directory, inventory);
    let direct = ordinary
        .iter()
        .filter(|(_, local)| local.len() == 1)
        .map(|(path, local)| (local[0].as_str(), (path, local)))
        .collect::<BTreeMap<_, _>>();
    let mut children = ordinary
        .iter()
        .filter(|(_, local)| local.len() > 1)
        .map(|(path, local)| (path.clone(), local.clone(), Scope::default()))
        .collect::<Vec<_>>();
    if file.syntax.macro_invocations().is_none() {
        Arc::make_mut(&mut scope).clear();
    }
    for event in events(file) {
        match event {
            Event::Symbol(symbol) if symbol.kind == "macro" => {
                let transparent = transparent_definition(file.source, symbol, &scope);
                Arc::make_mut(&mut scope).insert(symbol.name.clone(), transparent);
            }
            Event::Symbol(symbol) => {
                let name = super::source_identifier(&symbol.name);
                let child = name.as_deref().and_then(|name| direct.get(name)).copied();
                if let Some((path, local)) = child {
                    children.push((path.clone(), local.clone(), Arc::clone(&scope)));
                }
                if macro_use(file.source, symbol) {
                    scope = child
                        .and_then(|(path, _)| {
                            context.exports(
                                path,
                                child_directory(path),
                                Arc::clone(&scope),
                                &mut BTreeSet::new(),
                            )
                        })
                        .unwrap_or_default();
                }
            }
            Event::Invocation((range, modules, _)) => {
                let source = text_range(file.source, *range);
                let name = source
                    .and_then(|source| source.split_once('!'))
                    .map(|(name, _)| name.trim())
                    .filter(|name| super::crate_name_is_valid(name));
                if name.is_none_or(|name| scope.get(name) != Some(&true)) {
                    Arc::make_mut(&mut scope).clear();
                    continue;
                }
                for range in modules {
                    let name = text_range(file.source, *range)
                        .and_then(|item| item.strip_prefix("mod"))
                        .and_then(|item| item.strip_suffix(';'))
                        .map(str::trim)
                        .filter(|name| super::crate_name_is_valid(name));
                    if let Some(name) = name
                        && let Some(path) = child_path(name, directory, inventory)
                    {
                        children.push((path, vec![name.to_owned()], Arc::clone(&scope)));
                    }
                }
                // Mixed items preserve witnessed modules. Their other effects remain unknown.
                if source.and_then(invocation_modules).is_none() {
                    Arc::make_mut(&mut scope).clear();
                }
            }
        }
    }
    children
}
