//! Checks captured item-repetition macros without expanding source.

pub(crate) mod chain;
mod scope;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rift_syntax::{ByteRange, RustQuery, SyntaxSymbol};

use super::super::SelectedFile;

type Binding = (bool, Option<(rift_core::ProjectPath, ByteRange)>);

#[derive(Clone)]
pub(super) struct Scope {
    symbols: Arc<BTreeMap<String, Binding>>,
    complete: bool,
    registry_cfg_if: bool,
}

impl Default for Scope {
    fn default() -> Self {
        Self {
            symbols: Arc::default(),
            complete: true,
            registry_cfg_if: false,
        }
    }
}

impl Scope {
    fn get(&self, name: &str) -> Option<&bool> {
        self.symbols.get(name).map(|(transparent, _)| transparent)
    }
    fn contains_key(&self, name: &str) -> bool {
        self.symbols.contains_key(name)
    }
    fn insert(&mut self, name: String, transparent: bool) {
        if name == "cfg_if" {
            self.registry_cfg_if = false;
        }
        Arc::make_mut(&mut self.symbols).insert(name, (transparent, None));
    }
    fn define(
        &mut self,
        file: &SelectedFile<'_>,
        symbol: &SyntaxSymbol,
        transparent: bool,
        original: Option<&str>,
    ) {
        if symbol.name == "cfg_if" {
            self.registry_cfg_if = false;
        }
        if symbol.kind != "macro"
            || symbol.syntax_element_kind != Some("macro_definition")
            || original != Some(file.source)
            || file.syntax.has_errors()
            || text_range(file.source, symbol.item_range).is_none()
        {
            self.insert(symbol.name.clone(), false);
            return;
        }
        Arc::make_mut(&mut self.symbols).insert(
            symbol.name.clone(),
            (transparent, Some((file.path.clone(), symbol.item_range))),
        );
    }

    fn definition(&self, name: &str) -> Option<&(rift_core::ProjectPath, ByteRange)> {
        self.symbols.get(name)?.1.as_ref()
    }

    fn clear(&mut self) {
        Arc::make_mut(&mut self.symbols).clear();
        self.complete = false;
        self.registry_cfg_if = false;
    }
}

type Inventory<'a> = BTreeMap<&'a str, &'a SelectedFile<'a>>;
type Child = (String, Vec<String>, Scope);
/// A module file a conditional item chain declares, with the condition of its branch.
pub(super) type Conditioned<'a> = (String, Vec<&'a str>);

pub(super) fn text_range(source: &str, range: ByteRange) -> Option<&str> {
    source.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
}

// Every token borrows the admitted source. Unsupported literal forms refuse proof.
pub(super) fn tokens(source: &str) -> Option<Vec<&str>> {
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

pub(super) fn consume(input: &mut &[&str], expected: &[&str]) -> bool {
    if input.starts_with(expected) {
        *input = &input[expected.len()..];
        true
    } else {
        false
    }
}

pub(super) fn group<'tokens, 'source>(
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
    if *name == "cfg" {
        return true;
    }
    if *name != "cfg_attr" || !body.starts_with(&["docsrs", ",", "doc", "("]) {
        return false;
    }
    let mut documentation = &body[3..];
    group(&mut documentation, "(", ")").is_some() && documentation.is_empty()
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Condition {
    Known(bool),
    Unknown,
}

fn cfg(input: &[&str], library: bool, depth: usize) -> Option<Condition> {
    configured(input, library, depth, &|_| None)
}

pub(super) fn configured(
    input: &[&str],
    library: bool,
    depth: usize,
    feature: &impl Fn(&str) -> Option<bool>,
) -> Option<Condition> {
    configured_with_flags(input, library, depth, feature, &|_| None)
}

pub(super) fn configured_with_flags(
    input: &[&str],
    library: bool,
    depth: usize,
    feature: &impl Fn(&str) -> Option<bool>,
    flag: &impl Fn(&[&str]) -> Option<bool>,
) -> Option<Condition> {
    if depth == 0 {
        return None;
    }
    match input {
        ["test"] => Some(flag(input).map_or_else(
            || {
                if library {
                    Condition::Known(false)
                } else {
                    Condition::Unknown
                }
            },
            Condition::Known,
        )),
        ["true"] => Some(Condition::Known(true)),
        ["false"] => Some(Condition::Known(false)),
        [name] if super::crate_name_is_valid(name) => {
            Some(flag(input).map_or(Condition::Unknown, Condition::Known))
        }
        ["feature", "=", value] if value.starts_with('"') && value.ends_with('"') => Some(
            flag(input)
                .or_else(|| feature(value))
                .map_or(Condition::Unknown, Condition::Known),
        ),
        [name, "=", value]
            if super::crate_name_is_valid(name)
                && value.starts_with('"')
                && value.ends_with('"') =>
        {
            Some(flag(input).map_or(Condition::Unknown, Condition::Known))
        }
        ["not", "(", rest @ .., ")"] => {
            configured_with_flags(rest, library, depth - 1, feature, flag).map(
                |value| match value {
                    Condition::Known(value) => Condition::Known(!value),
                    Condition::Unknown => Condition::Unknown,
                },
            )
        }
        [operation @ ("all" | "any"), "(", rest @ .., ")"] => {
            let mut rest = rest;
            let mut result = Condition::Known(*operation == "all");
            while !rest.is_empty() {
                let mut split = 0;
                let mut nested = 0_usize;
                while split < rest.len() {
                    match rest[split] {
                        "(" => nested = nested.checked_add(1)?,
                        ")" => nested = nested.checked_sub(1)?,
                        "," if nested == 0 => break,
                        _ => {}
                    }
                    if nested > depth {
                        return None;
                    }
                    split += 1;
                }
                if nested != 0 || split == 0 {
                    return None;
                }
                let value =
                    configured_with_flags(&rest[..split], library, depth - 1, feature, flag)?;
                result = if *operation == "all" {
                    match (result, value) {
                        (Condition::Known(false), _) | (_, Condition::Known(false)) => {
                            Condition::Known(false)
                        }
                        (Condition::Known(true), Condition::Known(true)) => Condition::Known(true),
                        _ => Condition::Unknown,
                    }
                } else {
                    match (result, value) {
                        (Condition::Known(true), _) | (_, Condition::Known(true)) => {
                            Condition::Known(true)
                        }
                        (Condition::Known(false), Condition::Known(false)) => {
                            Condition::Known(false)
                        }
                        _ => Condition::Unknown,
                    }
                };
                rest = rest.get(split.saturating_add(1)..).unwrap_or_default();
            }
            Some(result)
        }
        _ => None,
    }
}

fn macro_use(
    tokens: &scope::Tokens<'_>,
    symbol: &SyntaxSymbol,
    library: bool,
    depth_max: usize,
) -> Option<bool> {
    let prefix = ByteRange {
        start: symbol.range.start,
        end: symbol.item_range.start,
    };
    let prefix = tokens.range(prefix)?;
    let mut input = prefix.as_slice();
    let mut imported = false;
    let mut condition = Condition::Known(true);
    while !input.is_empty() {
        let value = attribute(&mut input)?;
        if value == ["macro_use"] {
            if imported {
                return None;
            }
            imported = true;
        } else if let ["cfg", "(", predicate @ .., ")"] = value {
            condition = match (condition, cfg(predicate, library, depth_max)?) {
                (Condition::Known(false), _) | (_, Condition::Known(false)) => {
                    Condition::Known(false)
                }
                (Condition::Known(true), Condition::Known(true)) => Condition::Known(true),
                _ => Condition::Unknown,
            };
        } else if !cfg_attribute(value)
            && (!value.starts_with(&["doc", "("]) || value.last() != Some(&")"))
        {
            return None;
        }
    }
    match condition {
        Condition::Known(false) => Some(false),
        Condition::Known(true) => Some(imported),
        Condition::Unknown if !imported => Some(false),
        Condition::Unknown => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{Condition, Scope, cfg};

    #[test]
    fn captured_literal_definition_callbacks_keep_original_ranges_and_refuse_changed_definitions() {
        let source = "macro_rules! crate_root { () => { unknown! {}; pub mod de; include!(\"generated.rs\"); }; }";
        let path = rift_core::ProjectPath::new("src/crate_root.rs").expect("original path");
        let facts = rift_syntax::SyntaxProvider::analyze(
            &rift_syntax::RustSyntaxProvider::default(),
            rift_syntax::SyntaxSource {
                path: &path,
                text: source,
            },
            rift_syntax::SyntaxLimits::default(),
        )
        .expect("original definition")
        .into_facts();
        let file = super::SelectedFile {
            path: &path,
            source,
            syntax: &facts,
        };
        let symbol = facts
            .symbols()
            .iter()
            .find(|symbol| symbol.kind == "macro")
            .expect("captured definition");
        let tokens = super::scope::Tokens::new(&file, None).expect("original bounded scanner");
        let mut rows = Vec::new();
        assert!(
            tokens
                .definition_modules(
                    symbol.item_range,
                    "crate_root",
                    |range, name, default_path, paths| {
                        rows.push(range);
                        assert_eq!(name, "de");
                        assert!(default_path && paths.is_empty());
                        assert_eq!(super::text_range(source, range), Some("mod de;"));
                        Some(())
                    }
                )
                .is_some()
        );
        assert_eq!(rows.len(), 1);
        assert!(
            tokens
                .definition_modules(symbol.item_range, "wrong", |_, _, _, _| panic!(
                    "wrong definition cannot call back"
                ))
                .is_none()
        );
        let changed = rift_syntax::ByteRange {
            start: symbol.item_range.start + 1,
            end: symbol.item_range.end,
        };
        assert!(
            tokens
                .definition_modules(changed, "crate_root", |_, _, _, _| panic!(
                    "changed range cannot call back"
                ))
                .is_none()
        );
    }

    fn definition_scope(source: &str, changed: Option<&str>) -> Scope {
        let path = rift_core::ProjectPath::new("src/crate_root.rs").expect("captured path");
        let document = rift_syntax::SyntaxProvider::analyze(
            &rift_syntax::RustSyntaxProvider::default(),
            rift_syntax::SyntaxSource {
                path: &path,
                text: source,
            },
            rift_syntax::SyntaxLimits::default(),
        )
        .expect("original captured definition");
        let facts = document.into_facts();
        let file = super::SelectedFile {
            path: &path,
            source: changed.unwrap_or(source),
            syntax: &facts,
        };
        let symbol = facts
            .symbols()
            .iter()
            .find(|symbol| symbol.kind == "macro")
            .expect("captured macro");
        let mut scope = Scope::default();
        let transparent = super::transparent_definition(source, symbol, &scope);
        scope.define(&file, symbol, transparent, Some(source));
        scope
    }

    #[test]
    fn captured_macro_definition_references_keep_original_range_across_clones() {
        let source = "macro_rules! crate_root { () => { pub mod de; }; }";
        let scope = definition_scope(source, None);
        let cloned = scope.clone();
        assert_eq!(cloned.get("crate_root"), Some(&false));
        let (path, range) = cloned.symbols["crate_root"]
            .1
            .as_ref()
            .expect("original definition");
        assert_eq!(path.as_str(), "src/crate_root.rs");
        assert_eq!(range.start, 0);
        assert_eq!(
            range.end,
            u64::try_from(source.len()).expect("bounded source")
        );
        assert!(scope.complete && cloned.complete);
    }

    #[test]
    fn captured_macro_definition_references_clear_on_rebinding_and_unknown_effects() {
        let source = "macro_rules! crate_root { () => { pub mod de; }; }";
        let mut scope = definition_scope(source, None);
        scope.insert("crate_root".to_owned(), false);
        assert!(scope.symbols["crate_root"].1.is_none());
        scope = definition_scope(source, None);
        scope.clear();
        assert!(scope.symbols.is_empty());
        assert!(!scope.complete);
        scope.insert("later".to_owned(), true);
        assert!(!scope.complete);
        assert_eq!(scope.get("later"), Some(&true));
    }

    #[test]
    fn captured_macro_definition_references_refuse_changed_source_and_keep_templates_opaque() {
        let source = "macro_rules! crate_root { () => { #[cfg(feature = \"std\")] mod de; include!(\"generated.rs\"); }; }";
        let scope = definition_scope(source, None);
        assert_eq!(scope.get("crate_root"), Some(&false));
        assert!(scope.symbols["crate_root"].1.is_some());
        let changed = definition_scope(source, Some("macro_rules! other { () => {}; }"));
        assert_eq!(changed.get("crate_root"), Some(&false));
        assert!(changed.symbols["crate_root"].1.is_none());
    }

    #[test]
    fn selected_library_test_predicate_keeps_other_configuration_unknown() {
        assert_eq!(cfg(&["test"], false, 20), Some(Condition::Unknown));
        assert_eq!(cfg(&["test"], true, 20), Some(Condition::Known(false)));
        assert_eq!(
            cfg(&["not", "(", "test", ")"], true, 20),
            Some(Condition::Known(true))
        );
        assert_eq!(
            cfg(&["feature", "=", "\"rt\""], true, 20),
            Some(Condition::Unknown)
        );
        assert_eq!(
            cfg(&["target_family", "=", "\"wasm\""], true, 20),
            Some(Condition::Unknown)
        );
        assert_eq!(
            cfg(
                &[
                    "all",
                    "(",
                    "test",
                    ",",
                    "target_family",
                    "=",
                    "\"wasm\"",
                    ")"
                ],
                true,
                20
            ),
            Some(Condition::Known(false))
        );
        assert_eq!(
            cfg(
                &[
                    "any",
                    "(",
                    "test",
                    ",",
                    "target_family",
                    "=",
                    "\"wasm\"",
                    ")"
                ],
                true,
                20
            ),
            Some(Condition::Unknown)
        );
        assert_eq!(
            cfg(&["all", "(", ")"], true, 20),
            Some(Condition::Known(true))
        );
        assert_eq!(
            cfg(&["any", "(", ")"], true, 20),
            Some(Condition::Known(false))
        );
        assert_eq!(cfg(&["not", "(", "test", ")"], true, 1), None);
        assert_eq!(
            cfg(&["all", "(", "test", ",", ",", "false", ")"], true, 20),
            None
        );
        assert_eq!(cfg(&["unknown", "!", "(", ")"], true, 20), None);
    }
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
    Item(ByteRange),
}

impl Event<'_> {
    fn start(self) -> u64 {
        match self {
            Self::Symbol(symbol) => symbol.item_range.start,
            Self::Invocation((range, _, _)) => range.start,
            Self::Item(range) => range.start,
        }
    }
}

fn macro_export_attribute(file: &SelectedFile<'_>, range: ByteRange) -> bool {
    let Some(source) = text_range(file.source, range) else {
        return false;
    };
    let local = source.trim() == "#[macro_export(local_inner_macros)]";
    (source.trim() == "#[macro_export]" || local)
        && file.syntax.symbols().iter().any(|symbol| {
            symbol.kind == "macro"
                && symbol.syntax_element_kind == Some("macro_definition")
                && (local || symbol.visibility.as_deref() == Some("pub"))
                && symbol.range.start <= range.start
                && range.end <= symbol.item_range.start
        })
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
    if let Some((_, Some(children))) = file.syntax.scope().and_then(|rows| rows.first()) {
        for range in children {
            if macro_export_attribute(file, *range) {
                continue;
            }
            if events.iter().any(|event| match event {
                Event::Symbol(symbol) => symbol.item_range == *range,
                Event::Invocation((invocation, _, _)) => *invocation == *range,
                Event::Item(_) => false,
            }) {
                continue;
            }
            events.push(Event::Item(*range));
        }
    }
    events.sort_by_key(|event| event.start());
    events
}

struct Context<'a, 'configuration> {
    inventory: &'a Inventory<'a>,
    originals: &'a BTreeMap<&'a str, &'a str>,
    configurations: &'configuration super::configuration::Configurations<'a>,
    depth_max: usize,
    library: bool,
}

impl Context<'_, '_> {
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
        let result = (|| {
            let file = self.inventory.get(path)?;
            if self.originals.get(path).copied() != Some(file.source)
                || !super::source_is_valid(file.syntax, file.source)
                || file.syntax.macro_invocations().is_none()
            {
                return None;
            }
            let configuration = self.configurations.file(path);
            let mut tokens = scope::Tokens::new(file, Some(&configuration))?;
            let mut proof = scope.clone();
            self.file_exports(path, directory, &mut proof, visiting, &tokens, false)?;
            tokens.complete &= proof.complete;
            self.file_exports(path, directory, &mut scope, visiting, &tokens, true)
        })();
        visiting.remove(path);
        result.map(|()| scope)
    }

    fn tokens<'a>(
        &self,
        file: &'a SelectedFile<'a>,
        directory: &str,
        scope: &Scope,
    ) -> Option<scope::Tokens<'a>> {
        let configuration = self.configurations.file(file.path.as_str());
        let mut tokens = scope::Tokens::new(file, Some(&configuration));
        if let Some(tokens) = tokens.as_mut() {
            let mut proof = scope.clone();
            let mut visiting = BTreeSet::new();
            visiting.insert(file.path.as_str().to_owned());
            let complete = self
                .file_exports(
                    file.path.as_str(),
                    directory,
                    &mut proof,
                    &mut visiting,
                    tokens,
                    false,
                )
                .is_some()
                && proof.complete;
            tokens.complete &= complete;
        }
        tokens
    }

    fn import_module(
        &self,
        symbol: &SyntaxSymbol,
        child: Option<&str>,
        tokens: Option<&scope::Tokens<'_>>,
        scope: &mut Scope,
    ) {
        let imported =
            tokens.and_then(|tokens| macro_use(tokens, symbol, self.library, self.depth_max));
        if imported == Some(true) {
            if let Some(imported) = child.and_then(|path| {
                self.exports(
                    path,
                    child_directory(path),
                    scope.clone(),
                    &mut BTreeSet::new(),
                )
            }) {
                *scope = imported;
            } else {
                scope.clear();
            }
        } else if imported.is_none() {
            scope.clear();
        }
    }

    fn definition_children(
        &self,
        file: &SelectedFile<'_>,
        invocation: ByteRange,
        name: &str,
        directory: &str,
        scope: (&Scope, &mut Vec<Export>),
        tokens: &scope::Tokens<'_>,
    ) -> Option<Vec<Child>> {
        let (scope, exports) = scope;
        if !tokens.empty_invocation(invocation, name) {
            return None;
        }
        let (path, range) = scope.definition(name)?;
        let defining = self.inventory.get(path.as_str())?;
        if self.originals.get(path.as_str()).copied() != Some(defining.source)
            || defining.syntax.has_errors()
            || !defining.syntax.symbols().iter().any(|symbol| {
                symbol.name == name
                    && symbol.kind == "macro"
                    && symbol.syntax_element_kind == Some("macro_definition")
                    && symbol.item_range == *range
            })
            || path.as_str().rsplit_once('/').map(|(parent, _)| parent)
                != file
                    .path
                    .as_str()
                    .rsplit_once('/')
                    .map(|(parent, _)| parent)
        {
            return None;
        }
        let configuration = self.configurations.file(path.as_str());
        let definition = scope::Tokens::new(defining, Some(&configuration))?;
        let mut symbols = defining
            .syntax
            .symbols()
            .iter()
            .filter(|symbol| symbol.name == name && symbol.kind == "macro");
        let symbol = symbols.next()?;
        if symbols.next().is_some() {
            return None;
        }
        definition.definition_prefix(ByteRange {
            start: symbol.range.start,
            end: symbol.item_range.start,
        })?;
        let mut requested = Vec::new();
        let mut children = Vec::new();
        let mut incomplete = scope.clone();
        incomplete.clear();
        definition.definition_items(
            *range,
            name,
            &mut |module, name, default_path, paths| {
                if module.start < range.start || module.end > range.end {
                    return None;
                }
                let mut candidates = Vec::new();
                if default_path && let Some(path) = child_path(name, directory, self.inventory) {
                    candidates.push(path);
                }
                let parent = file
                    .path
                    .as_str()
                    .rsplit_once('/')
                    .map_or("", |(parent, _)| parent);
                for path in paths {
                    let candidate = if parent.is_empty() {
                        path.as_str().to_owned()
                    } else {
                        format!("{parent}/{}", path.as_str())
                    };
                    if rift_core::ProjectPath::new(candidate.as_str()).is_ok()
                        && self.inventory.contains_key(candidate.as_str())
                    {
                        candidates.push(candidate);
                    }
                }
                candidates.sort();
                candidates.dedup();
                if let [path] = candidates.as_slice() {
                    children.push((path.clone(), vec![name.to_owned()], incomplete.clone()));
                }
                Some(())
            },
            &mut |range, name, target| {
                requested.push((path.clone(), range, name, target, None));
                Some(())
            },
        )?;
        exports.extend(requested);
        Some(children)
    }

    fn file_exports(
        &self,
        path: &str,
        directory: &str,
        scope: &mut Scope,
        visiting: &mut BTreeSet<String>,
        tokens: &scope::Tokens<'_>,
        validate: bool,
    ) -> Option<()> {
        let file = self.inventory.get(path)?;
        for event in events(file) {
            match event {
                Event::Symbol(symbol) if symbol.kind == "macro" => {
                    let transparent = transparent_definition(file.source, symbol, scope);
                    scope.define(file, symbol, transparent, self.originals.get(path).copied());
                }
                Event::Symbol(symbol)
                    if macro_use(tokens, symbol, self.library, self.depth_max)? =>
                {
                    if !super::module_has_default_path(symbol) || symbol.body_range.is_some() {
                        return None;
                    }
                    let path = child_path(&symbol.name, directory, self.inventory)?;
                    *scope =
                        self.exports(&path, child_directory(&path), scope.clone(), visiting)?;
                }
                Event::Invocation((range, _, children)) => {
                    children.as_ref()?;
                    if validate {
                        tokens.item(*range, scope, self.library)?;
                    } else {
                        tokens.effects(*range, scope, self.library)?;
                    }
                }
                Event::Item(range) => {
                    if validate {
                        tokens.item(range, scope, self.library)?;
                    } else {
                        tokens.effects(range, scope, self.library)?;
                    }
                }
                Event::Symbol(_) => {}
            }
        }
        Some(())
    }
}

fn invocation_name(source: &str, range: ByteRange) -> Option<&str> {
    text_range(source, range)
        .and_then(|source| source.split_once('!'))
        .map(|(name, _)| name.trim())
        .filter(|name| super::crate_name_is_valid(name))
}

pub(super) type Export = (
    rift_core::ProjectPath,
    ByteRange,
    String,
    Vec<String>,
    Option<ByteRange>,
);

pub(super) fn children<'a>(
    file: &'a SelectedFile<'a>,
    directory: &str,
    inventory: &'a Inventory<'a>,
    originals: &'a BTreeMap<&'a str, &'a str>,
    scope: (
        Scope,
        &mut std::collections::BTreeSet<ByteRange>,
        &mut Vec<Export>,
        &mut Vec<Conditioned<'a>>,
    ),
    limits: rift_syntax::SyntaxLimits,
    (library, ordinary, configurations): (
        bool,
        &[super::ChildModulePath],
        &super::configuration::Configurations<'_>,
    ),
) -> Vec<Child> {
    let (mut scope, admitted, exports, conditioned) = scope;
    let configuration = configurations.file(file.path.as_str());
    // An ordinary use binds this file module. Its children need their own import.
    scope.registry_cfg_if = false;
    let registry_cfg_if = configuration.captured_registry_cfg_if()
        && !file.syntax.symbols().iter().any(|symbol| {
            symbol.container.is_none() && symbol.name == "cfg_if" && symbol.kind == "module"
        });
    let context = Context {
        inventory,
        originals,
        configurations,
        depth_max: limits.syntax_depth_max(),
        library,
    };
    // Keyed by the declaring item: alternative `mod` items of one name each keep their file.
    let direct = ordinary
        .iter()
        .filter(|(_, local, _)| local.len() == 1)
        .map(|(path, local, item)| (*item, (path, local)))
        .collect::<BTreeMap<_, _>>();
    let mut children = ordinary
        .iter()
        .filter(|(_, local, _)| local.len() > 1)
        .map(|(path, local, _)| (path.clone(), local.clone(), Scope::default()))
        .collect::<Vec<_>>();
    let tokens = context.tokens(file, directory, &scope);
    if file.syntax.macro_invocations().is_none() || tokens.is_none() {
        scope.clear();
    }
    for event in events(file) {
        match event {
            Event::Symbol(symbol) if symbol.kind == "macro" => {
                let transparent = transparent_definition(file.source, symbol, &scope);
                let original = originals.get(file.path.as_str()).copied();
                scope.define(file, symbol, transparent, original);
            }
            Event::Symbol(symbol) => {
                let child = direct.get(&symbol.item_range).copied();
                if let Some((path, local)) = child {
                    children.push((path.clone(), local.clone(), scope.clone()));
                }
                let path = child.map(|(path, _)| path.as_str());
                context.import_module(symbol, path, tokens.as_ref(), &mut scope);
            }
            Event::Invocation(invocation) => context.invocation_children(
                (file, directory, &configuration),
                invocation,
                tokens.as_ref(),
                (&mut scope, admitted, exports, conditioned),
                &mut children,
            ),
            Event::Item(range) => {
                let registry_import = registry_cfg_if
                    && scope.complete
                    && !scope.contains_key("cfg_if")
                    && tokens
                        .as_ref()
                        .and_then(|tokens| tokens.range(range))
                        .is_some_and(|item| {
                            matches!(
                                item.as_slice(),
                                ["use", "cfg_if", ":", ":", "cfg_if", ";"]
                                    | ["use", ":", ":", "cfg_if", ":", ":", "cfg_if", ";"]
                            )
                        })
                    && configuration.admitted(file.syntax, file.source, range) == Some(true);
                context.item(
                    (file, &configuration),
                    range,
                    (tokens.as_ref(), ordinary),
                    &mut scope,
                    exports,
                );
                if registry_import && scope.complete {
                    scope.registry_cfg_if = true;
                }
            }
        }
    }
    children
}

fn top_level(file: &SelectedFile<'_>, range: ByteRange) -> bool {
    file.syntax
        .scope()
        .and_then(|scope| scope.first())
        .and_then(|(_, children)| children.as_ref())
        .is_some_and(|children| children.contains(&range))
}

impl Context<'_, '_> {
    fn invocation_children<'a>(
        &self,
        (file, directory, configuration): (
            &SelectedFile<'_>,
            &str,
            &super::configuration::Configuration<'_>,
        ),
        (range, modules, direct): &rift_syntax::MacroInvocation,
        tokens: Option<&scope::Tokens<'a>>,
        (scope, admitted, exports, conditioned): (
            &mut Scope,
            &mut BTreeSet<ByteRange>,
            &mut Vec<Export>,
            &mut Vec<Conditioned<'a>>,
        ),
        children: &mut Vec<Child>,
    ) {
        if tokens.is_some_and(|tokens| tokens.compile_error(*range, scope, self.library)) {
            return;
        }
        let name = invocation_name(file.source, *range);
        if name.is_none_or(|name| scope.get(name) != Some(&true)) {
            if let Some(name) = name
                && direct.is_some()
                && let Some(literal) = tokens.and_then(|tokens| {
                    self.definition_children(
                        file,
                        *range,
                        name,
                        directory,
                        (scope, exports),
                        tokens,
                    )
                })
            {
                children.extend(literal);
            }
            if let Some(name) = name
                && let Some(tokens) = tokens
            {
                let declared = (&mut *children, &mut *conditioned);
                let registry = scope.registry_cfg_if
                    && configuration.admitted(file.syntax, file.source, *range) == Some(true)
                    && top_level(file, *range);
                if self.chain_children(
                    (*range, name, directory),
                    scope,
                    tokens,
                    declared,
                    registry,
                    (file, configuration),
                ) {
                    return;
                }
            }
            scope.clear();
            return;
        }
        let complete = tokens.zip(direct.as_ref()).is_some_and(|(tokens, _)| {
            tokens
                .item_modules(*range, scope, self.library, modules, |range, scope| {
                    let name = text_range(file.source, range)
                        .and_then(|item| item.strip_prefix("mod"))
                        .and_then(|item| item.strip_suffix(';'))
                        .map(str::trim)
                        .filter(|name| super::crate_name_is_valid(name));
                    if let Some(name) = name
                        && let Some(path) = child_path(name, directory, self.inventory)
                    {
                        children.push((path, vec![name.to_owned()], scope.clone()));
                    }
                    Some(())
                })
                .is_some()
        });
        if complete {
            admitted.insert(*range);
        } else {
            scope.clear();
        }
    }

    /// Adds the module files of a conditional item chain invocation to `children`, and
    /// each with its branch condition to `conditioned`.
    fn chain_children<'a>(
        &self,
        (invocation, name, directory): (ByteRange, &str, &str),
        scope: &mut Scope,
        tokens: &scope::Tokens<'a>,
        (children, conditioned): (&mut Vec<Child>, &mut Vec<Conditioned<'a>>),
        registry: bool,
        selection: (&SelectedFile<'_>, &super::configuration::Configuration<'_>),
    ) -> bool {
        let modules = if registry && name == "cfg_if" {
            tokens
                .range(invocation)
                .and_then(|invocation| chain::modules(&invocation, self.depth_max))
                .map(|modules| (modules, chain::ScopeAfter::Unknown))
        } else {
            self.chain(invocation, name, scope, tokens, selection)
        };
        let Some((modules, certainty)) = modules else {
            return false;
        };
        let checked = self.chain_scope((name, directory), (&modules, certainty), scope);
        let preserved = checked.is_some();
        let inherited = checked.unwrap_or_else(|| {
            let mut unknown = scope.clone();
            unknown.clear();
            unknown
        });
        if preserved {
            *scope = inherited.clone();
        }
        for (module, condition) in modules {
            if let Some(path) = child_path(module, directory, self.inventory) {
                children.push((path.clone(), vec![module.to_owned()], inherited.clone()));
                conditioned.push((path, condition));
            }
        }
        preserved
    }

    fn chain_scope(
        &self,
        (name, directory): (&str, &str),
        (modules, certainty): (&chain::ModuleBranches<'_>, chain::ScopeAfter),
        scope: &Scope,
    ) -> Option<Scope> {
        let (mut bindings, globs) = match certainty {
            chain::ScopeAfter::CheckedImports(bindings) => (bindings, BTreeSet::new()),
            chain::ScopeAfter::CheckedSiblingGlobs { bindings, modules } => (bindings, modules),
            chain::ScopeAfter::Unknown => return None,
        };
        for (module, _) in modules {
            let path = child_path(module, directory, self.inventory)?;
            let child = self.inventory.get(path.as_str())?;
            if !self.outline_keeps_macro_scope(child) {
                return None;
            }
            if globs.contains(*module) {
                let configuration = self.configurations.file(child.path.as_str());
                let tokens = scope::Tokens::with_capture(child, Some(name), Some(&configuration))?;
                bindings.extend(tokens.outward_bindings(child, Some(name), scope, self.library)?);
            }
        }
        if !globs.is_empty() && bindings.contains(name) {
            return None;
        }
        let mut inherited = scope.clone();
        for binding in bindings {
            inherited.insert(binding, false);
        }
        Some(inherited)
    }

    fn ordinary_sibling_glob(
        &self,
        (file, configuration): (&SelectedFile<'_>, &super::configuration::Configuration<'_>),
        ordinary: &[super::ChildModulePath],
        (range, mut input): (ByteRange, &[&str]),
        tokens: &scope::Tokens<'_>,
        scope: &mut Scope,
    ) -> Option<()> {
        if !scope.complete {
            return None;
        }
        scope::visibility(&mut input)?;
        let ["use", "self", ":", ":", module, ":", ":", "*", ";"] = input else {
            return None;
        };
        if tokens.configured_item(range, Some(configuration)) != Some(true)
            || !self.outline_keeps_macro_scope(file)
        {
            return None;
        }
        let child =
            self.ordinary_glob_target((file, configuration), ordinary, range, module, tokens)?;
        let child_configuration = self.configurations.file(child.path.as_str());
        let invocations = child.syntax.macro_invocations()?;
        let mut selected = None;
        for (invocation, _, items) in invocations {
            items.as_ref()?;
            let name = invocation_name(child.source, *invocation)?;
            if selected.is_some_and(|prior| prior != name) {
                return None;
            }
            selected = Some(name);
        }
        let child_tokens =
            scope::Tokens::with_capture(child, selected, Some(&child_configuration))?;
        if let Some(name) = selected {
            for (invocation, _, _) in invocations {
                let original = child_tokens.range(*invocation)?;
                let body = chain::invocation_body(&original)?;
                let (predicate, _) = chain::captured_predicate_body(body)?;
                child_tokens.charge(predicate.len().checked_mul(self.depth_max)?)?;
                child_configuration.condition(predicate)?;
                // The definition and original selected predicate must be checked.
                // The complete outward pass below checks the admitted item effects.
                self.chain(
                    *invocation,
                    name,
                    scope,
                    &child_tokens,
                    (child, &child_configuration),
                )?;
            }
        }
        let bindings = child_tokens.outward_bindings(child, selected, scope, self.library)?;
        if bindings.iter().any(|name| {
            scope.definition(name).is_some()
                || scope.get(name) == Some(&true)
                || (name == "cfg_if" && scope.registry_cfg_if)
        }) {
            return None;
        }
        for name in bindings {
            scope.insert(name, false);
        }
        Some(())
    }

    fn ordinary_glob_target<'a>(
        &'a self,
        (file, configuration): (&SelectedFile<'_>, &super::configuration::Configuration<'_>),
        ordinary: &[super::ChildModulePath],
        occurrence: ByteRange,
        name: &str,
        tokens: &scope::Tokens<'_>,
    ) -> Option<&'a SelectedFile<'a>> {
        let symbols = file.syntax.symbols();
        tokens.charge(symbols.len().checked_add(ordinary.len())?)?;
        let mut selected = None;
        for symbol in symbols.iter().filter(|symbol| {
            symbol.container.is_none() && symbol.kind == "module" && symbol.name == name
        }) {
            match configuration.admitted(file.syntax, file.source, symbol.item_range)? {
                false => {}
                true if selected.is_none() && symbol.item_range.end <= occurrence.start => {
                    selected = Some(symbol);
                }
                true => return None,
            }
        }
        let symbol = selected?;
        if symbol.syntax_element_kind != Some("mod_item")
            || symbol.body_range.is_some()
            || !super::module_has_default_path(symbol)
            || macro_use(tokens, symbol, self.library, self.depth_max) != Some(false)
        {
            return None;
        }
        let mut targets = ordinary
            .iter()
            .filter(|(_, local, item)| *item == symbol.item_range && local.as_slice() == [name]);
        let (path, _, _) = targets.next()?;
        if targets.next().is_some() {
            return None;
        }
        let child = self.inventory.get(path.as_str())?;
        tokens.charge(child.source.len())?;
        self.outline_keeps_macro_scope(child).then_some(child)
    }

    fn outline_keeps_macro_scope(&self, file: &SelectedFile<'_>) -> bool {
        if self.originals.get(file.path.as_str()).copied() != Some(file.source)
            || file.syntax.has_errors()
            || file.syntax.left_out_declaration_count() != 0
        {
            return false;
        }
        let Some(limits) = file.syntax.syntax_limits() else {
            return false;
        };
        // The module's own inner attributes can introduce outer macro bindings even
        // though its written outline has neither an attribute nor a changed path.
        RustQuery::new("(source_file (inner_attribute_item) @attribute)")
            .and_then(|query| query.captures(file.source, limits.syntax_elements_max()))
            .is_ok_and(|attributes| {
                attributes.into_iter().all(|capture| {
                    chain::token_texts(file.source, capture.range, limits).is_some_and(|tokens| {
                        let ["#", "!", "[", attribute @ .., "]"] = tokens.as_slice() else {
                            return false;
                        };
                        cfg_attribute(attribute)
                            || matches!(
                                attribute,
                                ["doc" | "allow" | "warn" | "deny" | "forbid" | "expect", ..]
                            )
                    })
                })
            })
    }

    /// The modules of an invocation of `name` when its definition in scope is the
    /// conditional item chain or forwards one captured predicate to unchanged items.
    fn chain<'a>(
        &self,
        invocation: ByteRange,
        name: &str,
        scope: &Scope,
        tokens: &scope::Tokens<'a>,
        (file, configuration): (&SelectedFile<'_>, &super::configuration::Configuration<'_>),
    ) -> Option<chain::CheckedBranches<'a>> {
        let (path, range) = scope.definition(name)?;
        let defining = self.inventory.get(path.as_str())?;
        let limits = defining.syntax.syntax_limits()?;
        let mut symbols = defining
            .syntax
            .symbols()
            .iter()
            .filter(|symbol| symbol.name == name && symbol.kind == "macro");
        let symbol = symbols.next()?;
        if symbols.next().is_some()
            || symbol.item_range != *range
            || symbol.syntax_element_kind != Some("macro_definition")
            || self.originals.get(path.as_str()).copied() != Some(defining.source)
            || defining.syntax.has_errors()
        {
            return None;
        }
        let definition = chain::token_texts(defining.source, *range, limits)?;
        if chain::is_captured_predicate_definition(&definition, name) {
            let defining_configuration = self.configurations.file(path.as_str());
            if defining_configuration.admitted(defining.syntax, defining.source, *range)
                != Some(true)
                || configuration.admitted(file.syntax, file.source, invocation) != Some(true)
            {
                return None;
            }
            if symbol.range.start != range.start {
                let prefix = ByteRange {
                    start: symbol.range.start,
                    end: range.start,
                };
                if defining.path == file.path {
                    tokens.definition_prefix(prefix)?;
                } else {
                    scope::Tokens::new(defining, Some(&defining_configuration))?
                        .definition_prefix(prefix)?;
                }
            }
            chain::captured_predicate_modules(
                &tokens.range(invocation)?,
                self.depth_max,
                |predicate| configuration.condition(predicate),
            )
        } else if chain::is_definition(&definition, name, limits) {
            chain::modules(&tokens.range(invocation)?, self.depth_max)
                .map(|modules| (modules, chain::ScopeAfter::Unknown))
        } else {
            None
        }
    }

    fn item(
        &self,
        (file, configuration): (&SelectedFile<'_>, &super::configuration::Configuration<'_>),
        range: ByteRange,
        (tokens, ordinary): (Option<&scope::Tokens<'_>>, &[super::ChildModulePath]),
        scope: &mut Scope,
        exports: &mut Vec<Export>,
    ) {
        if tokens.is_some_and(|tokens| tokens.inert_item(range)) {
            return;
        }
        // Original explicit imports remain declarations when macro effects are unknown.
        // Their own attributes and paths are checked before recording an alias.
        if let Some(tokens) = tokens
            && let Some(item) = tokens.range(range)
        {
            if self
                .ordinary_sibling_glob(
                    (file, configuration),
                    ordinary,
                    (range, &item),
                    tokens,
                    scope,
                )
                .is_some()
            {
                return;
            }
            let mut requested = Vec::new();
            if tokens
                .public_imports(&item, true, &mut |range, name, target, visibility| {
                    requested.push((file.path.clone(), range, name, target, visibility));
                    Some(())
                })
                .is_some()
            {
                exports.extend(requested);
            }
        }
        if tokens
            .and_then(|tokens| tokens.item(range, scope, self.library))
            .is_none()
        {
            scope.clear();
        }
    }
}
