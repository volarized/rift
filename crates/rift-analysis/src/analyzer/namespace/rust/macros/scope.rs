//! Checks effects in complete original Rust scope ranges.

use std::cell::Cell;
use std::collections::BTreeSet;

use rift_syntax::ByteRange;

use super::{Scope, SelectedFile, consume, group};

pub(super) struct Tokens<'a> {
    rows: Vec<(ByteRange, &'a str)>,
    bindings: BTreeSet<String>,
    pub(super) complete: bool,
    depth: usize,
    work: Cell<usize>,
    work_max: usize,
}

impl<'a> Tokens<'a> {
    pub(super) fn new(file: &'a SelectedFile<'a>) -> Option<Self> {
        let limits = file.syntax.syntax_limits()?;
        let mut rows = Vec::new();
        let range = ByteRange {
            start: 0,
            end: u64::try_from(file.source.len()).ok()?,
        };
        if !rift_syntax::tokens(file.source, range, limits, |range| {
            let Some(text) = super::text_range(file.source, range) else {
                return false;
            };
            rows.push((range, text));
            true
        }) {
            return None;
        }
        let mut result = Self {
            rows,
            bindings: BTreeSet::new(),
            complete: true,
            depth: limits.syntax_depth_max(),
            work: Cell::new(0),
            work_max: limits
                .syntax_nodes_max()
                .checked_mul(limits.syntax_depth_max())?,
        };
        let scopes = file.syntax.scope()?;
        if scopes.first().map(|(range, _)| *range) != Some(range) {
            return None;
        }
        for (_, children) in scopes {
            let Some(children) = children else {
                result.complete = false;
                continue;
            };
            for child in children {
                if super::macro_export_attribute(file, *child) {
                    result.charge(file.syntax.symbols().len())?;
                    continue;
                }
                result.names(*child)?;
            }
        }
        for (range, _, children) in file.syntax.macro_invocations()? {
            if children.is_none() {
                result.complete = false;
                continue;
            }
            result.invocation_names(*range)?;
        }
        Some(result)
    }

    pub(super) fn range(&self, range: ByteRange) -> Option<Vec<&'a str>> {
        let start = self
            .rows
            .partition_point(|(row, _)| row.start < range.start);
        let end = self.rows.partition_point(|(row, _)| row.end <= range.end);
        let rows = self.rows.get(start..end)?;
        if rows
            .iter()
            .any(|(row, _)| row.start < range.start || row.end > range.end)
        {
            return None;
        }
        self.charge(rows.len())?;
        Some(rows.iter().map(|(_, token)| *token).collect())
    }

    fn charge(&self, count: usize) -> Option<()> {
        let work = self.work.get().checked_add(count)?;
        if work > self.work_max {
            return None;
        }
        self.work.set(work);
        Some(())
    }

    fn names(&mut self, range: ByteRange) -> Option<()> {
        let tokens = self.range(range)?;
        self.names_tokens(&tokens)
    }

    fn invocation_names(&mut self, range: ByteRange) -> Option<()> {
        let tokens = self.range(range)?;
        if compile_error_tokens(&tokens) {
            return Some(());
        }
        let mut input = tokens.as_slice();
        identifier(&mut input)?;
        if !consume(&mut input, &["!"]) {
            return None;
        }
        let close = match *input.first()? {
            "(" => ")",
            "[" => "]",
            "{" => "}",
            _ => return None,
        };
        let open = *input.first()?;
        let body = group(&mut input, open, close)?;
        if !input.is_empty() {
            return None;
        }
        self.charge(body.len())?;
        items(body, self.depth, |item| self.names_tokens(item))
    }

    fn names_tokens(&mut self, tokens: &[&str]) -> Option<()> {
        let mut input = tokens;
        let mut exported = false;
        while consume(&mut input, &["#"]) {
            let _ = consume(&mut input, &["!"]);
            let value = group(&mut input, "[", "]")?;
            if value == ["macro_export"] && !exported {
                exported = true;
            } else if !self.attribute(value, &Scope::default(), true, self.depth, false) {
                self.complete = false;
            }
            if value.iter().any(|token| {
                [
                    "no_implicit_prelude",
                    "proc_macro",
                    "proc_macro_derive",
                    "proc_macro_attribute",
                ]
                .contains(token)
            }) {
                self.complete = false;
            }
        }
        visibility(&mut input)?;
        if exported && !input.starts_with(&["macro_rules", "!"]) {
            self.complete = false;
        }
        if consume(&mut input, &["use"]) {
            if imports(&mut input, None, &mut self.bindings, self.depth).is_none()
                || !consume(&mut input, &[";"])
                || !input.is_empty()
            {
                self.complete = false;
            }
        } else if input.starts_with(&["extern", "crate"]) {
            self.complete = false;
        } else if consume(&mut input, &["macro_rules", "!"]) {
            self.bindings.insert(identifier(&mut input)?);
        }
        Some(())
    }

    fn attribute(
        &self,
        input: &[&str],
        scope: &Scope,
        library: bool,
        depth: usize,
        validate: bool,
    ) -> bool {
        if depth == 0 {
            return false;
        }
        match input {
            ["cfg", "(", predicate @ .., ")"] => super::cfg(predicate, library, depth).is_some(),
            ["cfg_attr", "(", body @ .., ")"] => {
                let mut nested = 0_usize;
                let Some(split) = body.iter().position(|token| {
                    match *token {
                        "(" => nested += 1,
                        ")" => nested = nested.saturating_sub(1),
                        _ => {}
                    }
                    *token == "," && nested == 0
                }) else {
                    return false;
                };
                super::cfg(&body[..split], library, depth).is_some()
                    && self.attribute(&body[split + 1..], scope, library, depth - 1, validate)
            }
            ["derive", "(", names @ .., ")"] => {
                if validate && (!library || !self.complete) {
                    return false;
                }
                let mut input = names;
                while !input.is_empty() {
                    let Some(name) = identifier(&mut input) else {
                        return false;
                    };
                    if ![
                        "Clone",
                        "Copy",
                        "Debug",
                        "Default",
                        "Eq",
                        "Hash",
                        "Ord",
                        "PartialEq",
                        "PartialOrd",
                    ]
                    .contains(&name.as_str())
                        || (validate
                            && (self.bindings.contains(&name)
                                || !scope.complete
                                || scope.contains_key(&name)))
                    {
                        return false;
                    }
                    if !input.is_empty() && !consume(&mut input, &[","]) {
                        return false;
                    }
                }
                true
            }
            ["doc", rest @ ..] => !rest.contains(&"!"),
            [
                "allow" | "warn" | "deny" | "forbid" | "expect" | "inline" | "deprecated"
                | "must_use" | "repr",
                "(",
                rest @ ..,
                ")",
            ] => !rest
                .iter()
                .any(|token| ["!", "$", "{", "}"].contains(token)),
            [
                "inline" | "cold" | "track_caller" | "must_use" | "non_exhaustive" | "macro_use"
                | "no_std",
            ] => true,
            ["feature", "(", rest @ .., ")"] => !rest
                .iter()
                .any(|token| ["!", "$", "{", "}"].contains(token)),
            _ => false,
        }
    }

    pub(super) fn item(&self, range: ByteRange, scope: &mut Scope, library: bool) -> Option<()> {
        let tokens = self.range(range)?;
        self.item_tokens(&tokens, scope, library, self.depth, true)
    }

    pub(super) fn effects(&self, range: ByteRange, scope: &mut Scope, library: bool) -> Option<()> {
        let tokens = self.range(range)?;
        self.item_tokens(&tokens, scope, library, self.depth, false)
    }

    pub(super) fn compile_error(&self, range: ByteRange, scope: &Scope, library: bool) -> bool {
        let Some(tokens) = self.range(range) else {
            return false;
        };
        if !compile_error_tokens(&tokens)
            || !library
            || !self.complete
            || !scope.complete
            || self.bindings.contains("compile_error")
            || scope.contains_key("compile_error")
        {
            return false;
        }
        if self.charge(self.rows.len()).is_none() {
            return false;
        }
        !self
            .rows
            .iter()
            .any(|(_, token)| ["no_std", "no_implicit_prelude"].contains(token))
    }

    fn item_tokens(
        &self,
        tokens: &[&str],
        scope: &mut Scope,
        library: bool,
        depth: usize,
        validate: bool,
    ) -> Option<()> {
        if depth == 0 {
            return None;
        }
        let mut input = tokens;
        let mut exported = false;
        while consume(&mut input, &["#"]) {
            let _ = consume(&mut input, &["!"]);
            let value = group(&mut input, "[", "]")?;
            if value == ["macro_export"] && !exported {
                exported = true;
            } else if !self.attribute(value, scope, library, depth, validate) {
                return None;
            }
        }
        if input.is_empty() {
            return Some(());
        }
        visibility(&mut input)?;
        if exported && !input.starts_with(&["macro_rules", "!"]) {
            return None;
        }
        if consume(&mut input, &["use"]) {
            let mut bindings = BTreeSet::new();
            imports(&mut input, None, &mut bindings, depth)?;
            if !consume(&mut input, &[";"]) || !input.is_empty() {
                return None;
            }
            for name in bindings {
                scope.insert(name, false);
            }
            return Some(());
        }
        if consume(&mut input, &["macro_rules", "!"]) {
            let name = identifier(&mut input)?;
            group(&mut input, "{", "}")?;
            let _ = consume(&mut input, &[";"]);
            if !input.is_empty() {
                return None;
            }
            scope.insert(name, false);
            return Some(());
        }
        if consume(&mut input, &["mod"]) {
            identifier(&mut input)?;
            return (consume(&mut input, &[";"]) && input.is_empty()).then_some(());
        }
        if compile_error_tokens(input) {
            if self.bindings.contains("compile_error") || scope.contains_key("compile_error") {
                return None;
            }
            if !validate {
                return Some(());
            }
            self.charge(self.rows.len())?;
            return (library
                && self.complete
                && scope.complete
                && !self
                    .rows
                    .iter()
                    .any(|(_, token)| ["no_std", "no_implicit_prelude"].contains(token)))
            .then_some(());
        }
        let mut probe = input;
        if let Some(name) = identifier(&mut probe)
            && consume(&mut probe, &["!"])
        {
            if scope.get(&name) != Some(&true) {
                return None;
            }
            let body = group(&mut probe, "{", "}")?;
            let _ = consume(&mut probe, &[";"]);
            if !probe.is_empty() {
                return None;
            }
            return self.body(body, scope, library, depth - 1, validate);
        }
        if ![
            "mod", "fn", "impl", "struct", "enum", "union", "trait", "type", "const", "static",
            "unsafe", "async",
        ]
        .contains(input.first()?)
        {
            return None;
        }
        self.ordinary(input, scope, library, depth, validate)
    }

    fn ordinary(
        &self,
        mut input: &[&str],
        scope: &Scope,
        library: bool,
        depth: usize,
        validate: bool,
    ) -> Option<()> {
        self.charge(input.len())?;
        while !input.is_empty() {
            if consume(&mut input, &["#"]) {
                let _ = consume(&mut input, &["!"]);
                let value = group(&mut input, "[", "]")?;
                if !self.attribute(value, scope, library, depth, validate) {
                    return None;
                }
            } else {
                if input[0] == "!"
                    && input
                        .get(1)
                        .is_some_and(|token| ["(", "[", "{"].contains(token))
                {
                    return None;
                }
                if ["macro_rules", "macro_export", "use", "extern", "mod"].contains(&input[0])
                    && input.len() != 1
                {
                    return None;
                }
                input = &input[1..];
            }
        }
        Some(())
    }

    fn body(
        &self,
        input: &[&str],
        scope: &mut Scope,
        library: bool,
        depth: usize,
        validate: bool,
    ) -> Option<()> {
        if depth == 0 {
            return None;
        }
        self.charge(input.len())?;
        items(input, depth, |item| {
            self.item_tokens(item, scope, library, depth - 1, validate)
        })
    }
}

fn compile_error_tokens(mut input: &[&str]) -> bool {
    if identifier(&mut input).as_deref() != Some("compile_error") || !consume(&mut input, &["!"]) {
        return false;
    }
    let Some((open, close)) = input.first().and_then(|token| match *token {
        "(" => Some(("(", ")")),
        "{" => Some(("{", "}")),
        _ => None,
    }) else {
        return false;
    };
    let Some([message]) = group(&mut input, open, close) else {
        return false;
    };
    let _ = consume(&mut input, &[";"]);
    input.is_empty() && message.starts_with('"') && message.ends_with('"')
}

fn items(
    mut input: &[&str],
    depth: usize,
    mut callback: impl FnMut(&[&str]) -> Option<()>,
) -> Option<()> {
    if depth == 0 {
        return None;
    }
    while !input.is_empty() {
        let mut groups = Vec::new();
        let mut end = None;
        for (index, token) in input.iter().enumerate() {
            match *token {
                "(" => groups.push(")"),
                "[" => groups.push("]"),
                "{" => groups.push("}"),
                ")" | "]" | "}" => {
                    if groups.pop() != Some(*token) {
                        return None;
                    }
                    if groups.is_empty() && *token == "}" && input.get(index + 1) != Some(&";") {
                        end = Some(index + 1);
                        break;
                    }
                }
                ";" if groups.is_empty() => {
                    end = Some(index + 1);
                    break;
                }
                _ => {}
            }
            if groups.len() > depth {
                return None;
            }
        }
        let end = end?;
        callback(&input[..end])?;
        input = &input[end..];
    }
    Some(())
}

fn visibility(input: &mut &[&str]) -> Option<()> {
    if consume(input, &["pub"]) && input.first() == Some(&"(") {
        let value = group(input, "(", ")")?;
        if value.is_empty()
            || value.iter().any(|token| {
                !["crate", "self", "super", "in", ":"].contains(token)
                    && !super::super::crate_name_is_valid(token)
            })
        {
            return None;
        }
    }
    Some(())
}

fn identifier(input: &mut &[&str]) -> Option<String> {
    let raw = consume(input, &["r", "#"]);
    let token = input.first()?;
    let name = if raw {
        super::super::source_identifier(&format!("r#{token}"))?
    } else {
        super::super::source_identifier(token)?
    };
    *input = &input[1..];
    Some(name)
}

fn imports(
    input: &mut &[&str],
    parent: Option<&str>,
    names: &mut BTreeSet<String>,
    depth: usize,
) -> Option<()> {
    if depth == 0 {
        return None;
    }
    let _ = consume(input, &[":", ":"]);
    let mut last = parent.map(str::to_owned);
    loop {
        if consume(input, &["*"]) {
            return None;
        }
        if input.first() == Some(&"{") {
            let mut body = group(input, "{", "}")?;
            while !body.is_empty() {
                imports(&mut body, last.as_deref(), names, depth - 1)?;
                if !body.is_empty() && !consume(&mut body, &[","]) {
                    return None;
                }
            }
            return Some(());
        }
        let name = identifier(input)?;
        last = Some(if name == "self" && !input.starts_with(&[":", ":"]) {
            last?
        } else {
            name
        });
        if !consume(input, &[":", ":"]) {
            break;
        }
    }
    if consume(input, &["as"]) {
        if consume(input, &["_"]) {
            return Some(());
        }
        last = Some(identifier(input)?);
    }
    names.insert(last?);
    Some(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use rift_core::ProjectPath;
    use rift_syntax::{ByteRange, RustSyntaxProvider, SyntaxLimits, SyntaxProvider, SyntaxSource};

    use super::{Scope, SelectedFile, Tokens};

    fn with_tokens(source: &str, check: impl FnOnce(&Tokens<'_>)) {
        let path = ProjectPath::new("src/lib.rs").expect("original file path");
        let facts = RustSyntaxProvider::default()
            .analyze(
                SyntaxSource {
                    path: &path,
                    text: source,
                },
                SyntaxLimits::default(),
            )
            .expect("original Rust syntax")
            .into_facts();
        let file = SelectedFile {
            path: &path,
            source,
            syntax: &facts,
        };
        let tokens = Tokens::new(&file).expect("original complete source ranges");
        check(&tokens);
    }

    fn whole(source: &str) -> ByteRange {
        ByteRange {
            start: 0,
            end: u64::try_from(source.len()).expect("bounded source"),
        }
    }

    #[test]
    fn complete_import_bindings_keep_alias_raw_self_and_underscore_names() {
        with_tokens(
            "use other::{Debug as Compass, r#Copy as Pointer, nested::{self as Namespace, Entry}}; use other::Clone as _; use crate::root::{self, Value}; use self::doc::os;",
            |tokens| {
                assert!(tokens.complete);
                assert_eq!(
                    tokens.bindings,
                    BTreeSet::from(
                        [
                            "Compass",
                            "Pointer",
                            "Namespace",
                            "Entry",
                            "root",
                            "Value",
                            "os"
                        ]
                        .map(str::to_owned)
                    )
                );
                assert!(tokens.attribute(
                    &["derive", "(", "Debug", ",", "Copy", ",", "Clone", ")"],
                    &Scope::default(),
                    true,
                    20,
                    true
                ));
            },
        );
        for source in [
            "use other::Debug;",
            "use other::r#Debug;",
            "use other::Pointer as Debug;",
            "use other::{nested::{Debug}};",
        ] {
            with_tokens(source, |tokens| {
                assert!(tokens.bindings.contains("Debug"), "{source}");
                assert!(!tokens.attribute(
                    &["derive", "(", "Debug", ")"],
                    &Scope::default(),
                    true,
                    20,
                    true
                ));
            });
        }
    }

    #[test]
    fn macro_named_token_ranges_use_whole_body_for_literal_import_bindings() {
        with_tokens(
            "cfg_rt! { use self::doc::os; use other::Debug as Compass; }",
            |tokens| {
                assert!(tokens.complete);
                assert_eq!(
                    tokens.bindings,
                    BTreeSet::from(["os".to_owned(), "Compass".to_owned()])
                );
            },
        );
        with_tokens("cfg_rt! { use other::Debug; }", |tokens| {
            assert!(tokens.bindings.contains("Debug"));
            assert!(!tokens.attribute(
                &["derive", "(", "Debug", ")"],
                &Scope::default(),
                true,
                20,
                true
            ));
        });
    }

    #[test]
    fn complete_derive_proof_refuses_glob_unknown_later_and_qualified_names() {
        for source in [
            "use other::*;",
            "extern crate other;",
            "#[custom] struct Neighbor;",
            "#[derive(Debug)] struct Neighbor; use other::Debug;",
        ] {
            with_tokens(source, |tokens| {
                assert!(
                    !tokens.attribute(
                        &["derive", "(", "Debug", ")"],
                        &Scope::default(),
                        true,
                        20,
                        true
                    ),
                    "{source}"
                );
            });
        }
        with_tokens("struct Neighbor;", |tokens| {
            assert!(!tokens.attribute(
                &["derive", "(", "Debug", ")"],
                &Scope::default(),
                false,
                20,
                true
            ));
            assert!(!tokens.attribute(
                &["derive", "(", "other", ":", ":", "Debug", ")"],
                &Scope::default(),
                true,
                20,
                true
            ));
            assert!(!tokens.attribute(
                &["derive", "(", "Synthetic", ")"],
                &Scope::default(),
                true,
                20,
                true
            ));
        });
    }

    #[test]
    fn unknown_scope_stays_unknown_after_clone_and_later_known_definition() {
        with_tokens("struct Neighbor;", |tokens| {
            let mut scope = Scope::default();
            scope.insert("cfg_rt".to_owned(), true);
            scope.clear();
            let mut cloned = scope.clone();
            cloned.insert("cfg_rt".to_owned(), true);
            assert_eq!(cloned.get("cfg_rt"), Some(&true));
            assert!(!cloned.complete);
            assert!(!tokens.attribute(&["derive", "(", "Debug", ")"], &cloned, true, 20, true));
        });
    }

    #[test]
    fn original_mixed_items_keep_literal_lifetime_character_and_comment_effects() {
        let source = "impl Timer { fn poll<'a>(&self) { let _ = '\''; let _ = r#\"unknown! { use other::Debug; }\"#; /* unknown! {} */ 'label: loop { break 'label; } } }";
        with_tokens(source, |tokens| {
            let mut scope = Scope::default();
            scope.insert("cfg_rt".to_owned(), true);
            assert!(tokens.item(whole(source), &mut scope, true).is_some());
            assert_eq!(scope.get("cfg_rt"), Some(&true));
            assert!(scope.complete);
        });
        for source in [
            "impl Timer { fn poll(&self) { unknown! {}; } }",
            "impl Timer { fn poll(&self) { macro_rules! cfg_rt { () => {}; } } }",
            "impl Timer { fn poll(&self) { use other::Debug; } }",
        ] {
            with_tokens(source, |tokens| {
                assert!(
                    tokens
                        .item(whole(source), &mut Scope::default(), true)
                        .is_none(),
                    "{source}"
                );
            });
        }
    }

    #[test]
    fn literal_definition_and_import_shadow_existing_macro_without_restoring_unknown_scope() {
        for source in [
            "macro_rules! cfg_rt { () => {}; }",
            "use other::Macro as cfg_rt;",
        ] {
            with_tokens(source, |tokens| {
                let mut scope = Scope::default();
                scope.insert("cfg_rt".to_owned(), true);
                assert!(tokens.item(whole(source), &mut scope, true).is_some());
                assert_eq!(scope.get("cfg_rt"), Some(&false));
            });
        }
        with_tokens(
            "#[macro_export] macro_rules! Debug { () => {}; }",
            |tokens| {
                assert!(tokens.complete);
                assert!(tokens.bindings.contains("Debug"));
                let mut scope = Scope::default();
                assert!(
                    tokens
                        .item(
                            whole("#[macro_export] macro_rules! Debug { () => {}; }"),
                            &mut scope,
                            true
                        )
                        .is_some()
                );
                assert_eq!(scope.get("Debug"), Some(&false));
                assert!(!tokens.attribute(&["derive", "(", "Debug", ")"], &scope, true, 20, true));
            },
        );
    }
    #[test]
    fn exported_literal_macro_binding_keeps_body_opaque_without_invocation() {
        let source =
            "#[macro_export] macro_rules! api { () => { use synthetic::Debug; unknown! {}; } }";
        with_tokens(source, |tokens| {
            assert!(tokens.complete);
            assert_eq!(tokens.bindings, BTreeSet::from(["api".to_owned()]));
            let mut scope = Scope::default();
            assert!(tokens.item(whole(source), &mut scope, true).is_some());
            assert_eq!(scope.get("api"), Some(&false));
            assert!(tokens.attribute(&["derive", "(", "Debug", ")"], &scope, true, 20, true));
        });
    }

    #[test]
    fn exported_macro_attribute_refuses_unknown_misattached_and_inner_macro_forms() {
        for source in [
            "#[macro_export] struct Item;",
            "#[macro_export(local_inner_macros)] macro_rules! api { () => {}; }",
            "#[proc_macro] macro_rules! api { () => {}; }",
            "#[custom] #[macro_export] macro_rules! api { () => {}; }",
        ] {
            with_tokens(source, |tokens| {
                assert!(!tokens.complete, "{source}");
                assert!(
                    tokens
                        .item(whole(source), &mut Scope::default(), true)
                        .is_none(),
                    "{source}"
                );
            });
        }
    }
    #[test]
    fn literal_diagnostic_requires_complete_library_scope_and_implicit_prelude() {
        for source in [
            "compile_error!(\"message\");",
            "compile_error! { \"message\" }",
        ] {
            with_tokens(source, |tokens| {
                assert!(
                    tokens
                        .effects(whole(source), &mut Scope::default(), true)
                        .is_some()
                );
                assert!(tokens.compile_error(whole(source), &Scope::default(), true));
                assert!(!tokens.compile_error(whole(source), &Scope::default(), false));
                let mut unknown = Scope::default();
                unknown.clear();
                assert!(!tokens.compile_error(whole(source), &unknown, true));
            });
        }
    }

    #[test]
    fn literal_diagnostic_refuses_shadow_prelude_changes_and_computed_messages() {
        for source in [
            "#![no_std] compile_error!(\"message\");",
            "#![no_implicit_prelude] compile_error!(\"message\");",
            "#![cfg_attr(feature=\"bare\", no_std)] compile_error!(\"message\");",
            "use other::Macro as compile_error; compile_error!(\"message\");",
            "use other::*; compile_error!(\"message\");",
            "macro_rules! compile_error { () => {}; } compile_error!(\"message\");",
        ] {
            let path = ProjectPath::new("src/lib.rs").expect("original source path");
            let facts = RustSyntaxProvider::default()
                .analyze(
                    SyntaxSource {
                        path: &path,
                        text: source,
                    },
                    SyntaxLimits::default(),
                )
                .expect("original source")
                .into_facts();
            let file = SelectedFile {
                path: &path,
                source,
                syntax: &facts,
            };
            let allowed = Tokens::new(&file).is_some_and(|tokens| {
                let (range, _, _) = facts
                    .macro_invocations()
                    .expect("captured invocations")
                    .last()
                    .expect("literal diagnostic");
                tokens.compile_error(*range, &Scope::default(), true)
            });
            assert!(!allowed, "{source}");
        }
        for tokens in [
            &[
                "other",
                ":",
                ":",
                "compile_error",
                "!",
                "(",
                "\"message\"",
                ")",
            ][..],
            &[
                "compile_error",
                "!",
                "(",
                "concat",
                "!",
                "(",
                "\"message\"",
                ")",
                ")",
            ][..],
            &["compile_error", "!", "(", "message", ")"][..],
        ] {
            assert!(!super::compile_error_tokens(tokens));
        }
    }

    #[test]
    fn original_macro_delimiters_keep_constructor_ranges_and_unknown_effects() {
        for source in ["crate_root!();", "crate_root![];", "crate_root!{}"] {
            with_tokens(source, |tokens| {
                assert!(tokens.complete, "{source}");
                assert!(tokens.bindings.is_empty(), "{source}");
                let range = ByteRange {
                    start: 0,
                    end: u64::try_from(source.trim_end_matches(';').len())
                        .expect("bounded original invocation"),
                };
                assert_eq!(
                    tokens.range(range).expect("original invocation").first(),
                    Some(&"crate_root")
                );
                assert!(
                    tokens.effects(range, &mut Scope::default(), true).is_none(),
                    "{source}"
                );
                assert!(
                    tokens.item(range, &mut Scope::default(), true).is_none(),
                    "{source}"
                );
            });
        }
    }

    #[test]
    fn original_macro_delimiters_keep_literal_import_names_and_refuse_shadowed_derive() {
        for source in [
            "cfg_rt!(use other::Debug;);",
            "cfg_rt![use other::Debug;];",
            "cfg_rt!{use other::Debug;}",
        ] {
            with_tokens(source, |tokens| {
                assert!(tokens.complete, "{source}");
                assert!(tokens.bindings.contains("Debug"), "{source}");
                assert!(
                    !tokens.attribute(
                        &["derive", "(", "Debug", ")"],
                        &Scope::default(),
                        true,
                        20,
                        true,
                    ),
                    "{source}"
                );
            });
        }
    }
}
