//! Checks effects in complete original Rust scope ranges.

use std::cell::Cell;
use std::collections::BTreeSet;

use rift_syntax::ByteRange;

use super::{Scope, SelectedFile, consume, group};

type Modules<'callback> = Option<&'callback mut dyn FnMut(ByteRange, &Scope) -> Option<()>>;

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
        self.charge(body.len().checked_mul(2)?)?;
        items(body, self.depth, |item| self.names_tokens(item))
    }

    fn names_tokens(&mut self, tokens: &[&str]) -> Option<()> {
        let mut input = tokens;
        let mut exported = false;
        while consume(&mut input, &["#"]) {
            let _ = consume(&mut input, &["!"]);
            let value = group(&mut input, "[", "]")?;
            if matches!(
                value,
                ["macro_export"] | ["macro_export", "(", "local_inner_macros", ")"]
            ) && !exported
            {
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
        self.item_tokens(&tokens, scope, library, self.depth, true, &mut None)
    }

    pub(super) fn empty_invocation(&self, range: ByteRange, name: &str) -> bool {
        let Some(tokens) = self.range(range) else {
            return false;
        };
        let mut input = tokens.as_slice();
        if !consume(&mut input, &[name, "!"]) {
            return false;
        }
        let Some(close) = input.first().and_then(|open| match *open {
            "(" => Some(")"),
            "[" => Some("]"),
            "{" => Some("}"),
            _ => None,
        }) else {
            return false;
        };
        let open = input[0];
        let Some(body) = group(&mut input, open, close) else {
            return false;
        };
        let _ = consume(&mut input, &[";"]);
        body.is_empty() && input.is_empty()
    }

    #[cfg(test)]
    pub(super) fn definition_modules(
        &self,
        range: ByteRange,
        name: &str,
        mut callback: impl FnMut(ByteRange, &str, bool, &[rift_core::ProjectPath]) -> Option<()>,
    ) -> Option<()> {
        self.definition_items(range, name, &mut callback, &mut |_, _, _| Some(()))
    }

    pub(super) fn definition_items(
        &self,
        range: ByteRange,
        name: &str,
        callback: &mut impl FnMut(ByteRange, &str, bool, &[rift_core::ProjectPath]) -> Option<()>,
        export: &mut dyn FnMut(ByteRange, String, Vec<String>) -> Option<()>,
    ) -> Option<()> {
        let tokens = self.range(range)?;
        let mut input = tokens.as_slice();
        while consume(&mut input, &["#"]) {
            if consume(&mut input, &["!"]) {
                return None;
            }
            let value = group(&mut input, "[", "]")?;
            if !matches!(value.first(), Some(&"cfg" | &"doc"))
                || !self.attribute(value, &Scope::default(), false, self.depth, false)
            {
                return None;
            }
        }
        if !consume(&mut input, &["macro_rules", "!", name, "{"]) {
            return None;
        }
        if !group(&mut input, "(", ")")?.is_empty() || !consume(&mut input, &["=", ">"]) {
            return None;
        }
        let body = group(&mut input, "{", "}")?;
        let _ = consume(&mut input, &[";"]);
        if !consume(&mut input, &["}"]) || !input.is_empty() {
            return None;
        }
        self.charge(body.len().checked_mul(2)?)?;
        let mut modules = Vec::new();
        let mut exports = Vec::new();
        let mut names = BTreeSet::new();
        items(body, self.depth, |item| {
            self.public_imports(item, &mut |range, name, path| {
                exports.push((range, name, path));
                Some(())
            })?;
            if let Some((range, name, default_path, paths)) = self.definition_module(item) {
                if !names.insert(name.clone()) {
                    return None;
                }
                modules.push((range, name, default_path, paths));
            }
            Some(())
        })?;
        for (binding, name, path) in exports {
            if binding.start < range.start || binding.end > range.end {
                return None;
            }
            export(binding, name, path)?;
        }
        for (module, name, default_path, paths) in modules {
            if module.start < range.start || module.end > range.end {
                return None;
            }
            callback(module, &name, default_path, &paths)?;
        }
        Some(())
    }

    pub(super) fn definition_prefix(&self, range: ByteRange) -> Option<()> {
        if range.start == range.end {
            return Some(());
        }
        let tokens = self.range(range)?;
        let mut input = tokens.as_slice();
        while consume(&mut input, &["#"]) {
            if consume(&mut input, &["!"]) {
                return None;
            }
            let value = group(&mut input, "[", "]")?;
            if !matches!(value.first(), Some(&"cfg" | &"doc"))
                || !self.attribute(value, &Scope::default(), false, self.depth, false)
            {
                return None;
            }
        }
        input.is_empty().then_some(())
    }

    pub(super) fn public_imports(
        &self,
        item: &[&str],
        callback: &mut dyn FnMut(ByteRange, String, Vec<String>) -> Option<()>,
    ) -> Option<()> {
        let mut input = item;
        while consume(&mut input, &["#"]) {
            if consume(&mut input, &["!"]) {
                return Some(());
            }
            let value = group(&mut input, "[", "]")?;
            if !self.attribute(value, &Scope::default(), false, self.depth, false) {
                return Some(());
            }
        }
        if !consume(&mut input, &["pub", "use"]) {
            return Some(());
        }
        let range = self.module_range(item)?;
        let mut names = BTreeSet::new();
        let mut exports = Vec::new();
        imports_path(
            &mut input,
            &[],
            &mut names,
            self.depth,
            &mut |name, path| {
                let bytes = path
                    .iter()
                    .try_fold(name.len(), |bytes, value| bytes.checked_add(value.len()))?;
                self.charge(bytes)?;
                exports.push((name, path));
                Some(())
            },
        )?;
        if !consume(&mut input, &[";"]) || !input.is_empty() {
            return None;
        }
        for (name, path) in exports {
            callback(range, name, path)?;
        }
        Some(())
    }

    fn definition_module(
        &self,
        mut input: &[&str],
    ) -> Option<(ByteRange, String, bool, Vec<rift_core::ProjectPath>)> {
        let mut default_path = true;
        let mut paths = Vec::new();
        while consume(&mut input, &["#"]) {
            if consume(&mut input, &["!"]) {
                return None;
            }
            let value = group(&mut input, "[", "]")?;
            if !self.definition_attribute(value, &mut default_path, &mut paths) {
                return None;
            }
        }
        visibility(&mut input)?;
        let module = input;
        if !consume(&mut input, &["mod"]) {
            return None;
        }
        let name = identifier(&mut input)?;
        if !consume(&mut input, &[";"]) || !input.is_empty() {
            return None;
        }
        Some((self.module_range(module)?, name, default_path, paths))
    }

    fn definition_attribute(
        &self,
        input: &[&str],
        default_path: &mut bool,
        paths: &mut Vec<rift_core::ProjectPath>,
    ) -> bool {
        let (conditional, value) = match input {
            ["path", "=", value] => (false, *value),
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
                if super::cfg(&body[..split], false, self.depth).is_none() {
                    return false;
                }
                let ["path", "=", value] = &body[split + 1..] else {
                    return body.get(split + 1) == Some(&"doc")
                        && self.attribute(input, &Scope::default(), false, self.depth, false);
                };
                (true, *value)
            }
            [
                "cfg" | "doc" | "allow" | "warn" | "deny" | "forbid" | "deprecated",
                ..,
            ] => {
                return self.attribute(input, &Scope::default(), false, self.depth, false);
            }
            _ => return false,
        };
        if !paths.is_empty() {
            return false;
        }
        let Some(path) = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
        else {
            return false;
        };
        if path.is_empty()
            || path.contains('\\')
            || path.len() > rift_core::constants::PROJECT_PATH_BYTES_MAX
        {
            return false;
        }
        let Ok(path) = rift_core::ProjectPath::new(path) else {
            return false;
        };
        paths.push(path);
        *default_path = conditional;
        true
    }

    pub(super) fn item_modules(
        &self,
        range: ByteRange,
        scope: &mut Scope,
        library: bool,
        modules: &[ByteRange],
        mut callback: impl FnMut(ByteRange, &Scope) -> Option<()>,
    ) -> Option<()> {
        let tokens = self.range(range)?;
        let mut admitted = |range, scope: &Scope| {
            self.charge(modules.len())?;
            if modules.contains(&range) {
                callback(range, scope)?;
            }
            Some(())
        };
        let mut callbacks: Modules<'_> = Some(&mut admitted);
        self.item_tokens(&tokens, scope, library, self.depth, true, &mut callbacks)
    }

    fn module_range(&self, input: &[&str]) -> Option<ByteRange> {
        self.charge(self.rows.len().checked_add(input.len())?)?;
        let first = input.first()?;
        let start = self
            .rows
            .iter()
            .position(|(_, token)| std::ptr::eq(*token, *first))?;
        let end = start.checked_add(input.len())?;
        let rows = self.rows.get(start..end)?;
        if !rows
            .iter()
            .zip(input)
            .all(|((_, row), token)| std::ptr::eq(*row, *token))
        {
            return None;
        }
        Some(ByteRange {
            start: rows.first()?.0.start,
            end: rows.last()?.0.end,
        })
    }

    pub(super) fn effects(&self, range: ByteRange, scope: &mut Scope, library: bool) -> Option<()> {
        let tokens = self.range(range)?;
        self.item_tokens(&tokens, scope, library, self.depth, false, &mut None)
    }

    pub(super) fn compile_error(&self, range: ByteRange, scope: &Scope, library: bool) -> bool {
        let Some(tokens) = self.range(range) else {
            return false;
        };
        compile_error_tokens(&tokens) && self.implicit_prelude("compile_error", scope, library)
    }

    fn implicit_prelude(&self, name: &str, scope: &Scope, library: bool) -> bool {
        if !library
            || !self.complete
            || !scope.complete
            || self.bindings.contains(name)
            || scope.contains_key(name)
            || self.charge(self.rows.len()).is_none()
        {
            return false;
        }
        !self
            .rows
            .iter()
            .any(|(_, token)| ["no_std", "no_implicit_prelude"].contains(token))
    }

    fn cfg(
        &self,
        input: &mut &[&str],
        scope: &Scope,
        library: bool,
        depth: usize,
        validate: bool,
    ) -> Option<()> {
        if !consume(input, &["cfg", "!"]) {
            return None;
        }
        let predicate = group(input, "(", ")")?;
        self.charge(predicate.len().checked_mul(depth)?)?;
        super::cfg(predicate, library, depth)?;
        if self.bindings.contains("cfg") || scope.contains_key("cfg") {
            return None;
        }
        (!validate || self.implicit_prelude("cfg", scope, library)).then_some(())
    }

    fn item_tokens(
        &self,
        tokens: &[&str],
        scope: &mut Scope,
        library: bool,
        depth: usize,
        validate: bool,
        modules: &mut Modules<'_>,
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
        let module = input;
        if consume(&mut input, &["mod"]) {
            identifier(&mut input)?;
            if !consume(&mut input, &[";"]) || !input.is_empty() {
                return None;
            }
            if let Some(callback) = modules {
                callback(self.module_range(module)?, scope)?;
            }
            return Some(());
        }
        if compile_error_tokens(input) {
            if self.bindings.contains("compile_error") || scope.contains_key("compile_error") {
                return None;
            }
            if !validate {
                return Some(());
            }
            return self
                .implicit_prelude("compile_error", scope, library)
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
            return self.body(body, scope, library, depth - 1, validate, modules);
        }
        if ![
            "mod", "fn", "impl", "struct", "enum", "union", "trait", "type", "const", "static",
            "unsafe", "async",
        ]
        .contains(input.first()?)
        {
            return None;
        }
        if input.first() == Some(&"fn")
            && self
                .function(input, scope, library, depth, validate)
                .is_some()
        {
            return Some(());
        }
        self.ordinary(input, scope, library, depth, validate, false)
    }

    fn function(
        &self,
        input: &[&str],
        scope: &Scope,
        library: bool,
        depth: usize,
        validate: bool,
    ) -> Option<()> {
        self.charge(input.len())?;
        let range = self.module_range(input)?;
        let start = input.iter().position(|token| *token == "{")?;
        let width = usize::try_from(range.end.checked_sub(range.start)?).ok()?;
        if !rift_syntax::SyntaxDocument::function_header(&input[..start], (self.rows.len(), width))
        {
            return None;
        }
        let mut tail = &input[start..];
        let body = group(&mut tail, "{", "}")?;
        if !tail.is_empty() {
            return None;
        }
        let mut local = scope.clone();
        self.local_imports(body, &mut local, depth)?;
        self.ordinary(body, &local, library, depth.checked_sub(1)?, validate, true)
    }

    fn local_imports(&self, mut input: &[&str], scope: &mut Scope, depth: usize) -> Option<()> {
        self.charge(input.len())?;
        while !input.is_empty() {
            if consume(&mut input, &["use"]) {
                let mut bindings = BTreeSet::new();
                imports(&mut input, None, &mut bindings, depth)?;
                if !consume(&mut input, &[";"]) {
                    return None;
                }
                for name in bindings {
                    scope.insert(name, false);
                }
            } else {
                input = &input[1..];
            }
        }
        Some(())
    }

    fn panic(
        &self,
        input: &mut &[&str],
        scope: &Scope,
        library: bool,
        depth: usize,
        validate: bool,
    ) -> Option<()> {
        if !consume(input, &["panic", "!"])
            || self.bindings.contains("panic")
            || scope.contains_key("panic")
            || (validate && !self.implicit_prelude("panic", scope, library))
        {
            return None;
        }
        let (open, close) = match input.first().copied()? {
            "(" => ("(", ")"),
            "[" => ("[", "]"),
            "{" => ("{", "}"),
            _ => return None,
        };
        let arguments = group(input, open, close)?;
        self.ordinary(
            arguments,
            scope,
            library,
            depth.checked_sub(1)?,
            validate,
            true,
        )
    }

    fn ordinary(
        &self,
        mut input: &[&str],
        scope: &Scope,
        library: bool,
        depth: usize,
        validate: bool,
        local: bool,
    ) -> Option<()> {
        self.charge(input.len())?;
        let mut previous = None;
        while !input.is_empty() {
            if consume(&mut input, &["#"]) {
                let _ = consume(&mut input, &["!"]);
                let value = group(&mut input, "[", "]")?;
                if !self.attribute(value, scope, library, depth, validate) {
                    return None;
                }
            } else {
                if local && consume(&mut input, &["use"]) {
                    let mut bindings = BTreeSet::new();
                    imports(&mut input, None, &mut bindings, depth)?;
                    if !consume(&mut input, &[";"]) {
                        return None;
                    }
                    previous = None;
                    continue;
                }
                if local && input.starts_with(&["panic", "!"]) {
                    if previous == Some(":") {
                        return None;
                    }
                    self.panic(&mut input, scope, library, depth, validate)?;
                    previous = Some(")");
                    continue;
                }
                if input.starts_with(&["cfg", "!", "("]) {
                    if previous == Some(":") {
                        return None;
                    }
                    self.cfg(&mut input, scope, library, depth, validate)?;
                    previous = Some(")");
                    continue;
                }
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
                previous = Some(input[0]);
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
        modules: &mut Modules<'_>,
    ) -> Option<()> {
        if depth == 0 {
            return None;
        }
        self.charge(input.len().checked_mul(2)?)?;
        items(input, depth, |item| {
            self.item_tokens(item, scope, library, depth - 1, validate, modules)
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
        let semicolon = constant(input)?;
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
                    if !semicolon
                        && groups.is_empty()
                        && *token == "}"
                        && input.get(index + 1) != Some(&";")
                    {
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

fn constant(mut input: &[&str]) -> Option<bool> {
    while consume(&mut input, &["#"]) {
        let _ = consume(&mut input, &["!"]);
        group(&mut input, "[", "]")?;
    }
    visibility(&mut input)?;
    if !consume(&mut input, &["const"]) || input.first() == Some(&"fn") {
        return Some(false);
    }
    if !consume(&mut input, &["_"]) {
        identifier(&mut input)?;
    }
    Some(consume(&mut input, &[":"]))
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
    let path = parent.map(|name| vec![name.to_owned()]).unwrap_or_default();
    imports_path(input, &path, names, depth, &mut |_, _| Some(()))
}

fn imports_path(
    input: &mut &[&str],
    parent: &[String],
    names: &mut BTreeSet<String>,
    depth: usize,
    callback: &mut dyn FnMut(String, Vec<String>) -> Option<()>,
) -> Option<()> {
    if depth == 0 {
        return None;
    }
    let absolute = consume(input, &[":", ":"]);
    let mut path = if absolute {
        Vec::new()
    } else {
        parent.to_vec()
    };
    loop {
        if consume(input, &["*"]) {
            return None;
        }
        if input.first() == Some(&"{") {
            let mut body = group(input, "{", "}")?;
            while !body.is_empty() {
                imports_path(&mut body, &path, names, depth - 1, callback)?;
                if !body.is_empty() && !consume(&mut body, &[","]) {
                    return None;
                }
            }
            return Some(());
        }
        let name = identifier(input)?;
        if name == "self" && !input.starts_with(&[":", ":"]) {
            path.last()?;
        } else {
            path.push(name);
        }
        if !consume(input, &[":", ":"]) {
            break;
        }
    }
    let mut name = path.last()?.clone();
    if consume(input, &["as"]) {
        if consume(input, &["_"]) {
            return Some(());
        }
        name = identifier(input)?;
    }
    names.insert(name.clone());
    callback(name, path)
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
        let source = "#[macro_export(local_inner_macros)] macro_rules! api { () => {}; }";
        with_tokens(source, |tokens| {
            assert!(tokens.complete);
            assert_eq!(tokens.bindings, BTreeSet::from(["api".to_owned()]));
            assert!(
                tokens
                    .item(whole(source), &mut Scope::default(), true)
                    .is_none()
            );
        });
        for source in [
            "#[macro_export] struct Item;",
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
    fn literal_definition_callbacks_refuse_work_overflow() {
        let source = "macro_rules! crate_root { () => { pub mod de; }; }";
        with_tokens(source, |tokens| {
            tokens.work.set(tokens.work_max);
            assert!(
                tokens
                    .definition_modules(whole(source), "crate_root", |_, _, _, _| panic!(
                        "work overflow cannot call back"
                    ))
                    .is_none()
            );
        });
    }

    #[test]
    fn constant_items_keep_full_initializer_and_following_scope_effects() {
        let original = "cfg_rt! { #[cfg(feature=\"rt\")] pub(crate) const BOX_FUTURE_THRESHOLD: usize = if true { 2048 } else { 16384 }; use other::Debug; pub mod runtime; }";
        for source in [original.to_owned(), original.replace(' ', "\r\n")] {
            with_tokens(&source, |tokens| {
                assert!(tokens.complete);
                assert!(tokens.bindings.contains("Debug"));
                let mut scope = Scope::default();
                scope.insert("cfg_rt".to_owned(), true);
                assert!(tokens.item(whole(&source), &mut scope, true).is_some());
                assert_eq!(scope.get("Debug"), Some(&false));
                assert!(scope.complete);
            });
        }
    }

    #[test]
    fn constant_items_preserve_function_boundaries_and_unnamed_constants() {
        let source = "cfg_rt! { pub const fn threshold() -> usize { 2048 } const _: usize = { 16384 }; pub mod runtime; }";
        with_tokens(source, |tokens| {
            let mut scope = Scope::default();
            scope.insert("cfg_rt".to_owned(), true);
            assert!(tokens.item(whole(source), &mut scope, true).is_some());
            assert!(scope.complete);
        });
    }

    #[test]
    fn constant_items_refuse_missing_semicolon_unknown_macros_and_work_overflow() {
        let malformed = [
            "const", "VALUE", ":", "usize", "=", "if", "true", "{", "1", "}", "else", "{", "2", "}",
        ];
        assert!(super::items(&malformed, 20, |_| Some(())).is_none());
        let source =
            "cfg_rt! { const VALUE: usize = if true { unknown!{} } else { 2 }; pub mod runtime; }";
        with_tokens(source, |tokens| {
            let mut scope = Scope::default();
            scope.insert("cfg_rt".to_owned(), true);
            assert!(tokens.item(whole(source), &mut scope, true).is_none());
            tokens.work.set(tokens.work_max);
            assert!(tokens.item(whole(source), &mut scope, true).is_none());
        });
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

    #[test]
    fn cfg_expression_keeps_configuration_without_observed_values_unknown() {
        assert!(matches!(
            super::super::cfg(&["debug_assertions"], true, 20),
            Some(super::super::Condition::Unknown),
        ));
        assert!(matches!(
            super::super::cfg(&["feature", "=", "\"rt\""], true, 20),
            Some(super::super::Condition::Unknown),
        ));
    }

    #[test]
    fn original_module_callbacks_require_captured_range_and_work_budget() {
        let source = "cfg_rt! { mod runtime; unknown! { mod runtime; } }";
        let start = u64::try_from(source.find("mod runtime;").expect("original direct module"))
            .expect("module range");
        let nested = u64::try_from(
            source
                .rfind("mod runtime;")
                .expect("original nested module"),
        )
        .expect("module range");
        let direct = ByteRange {
            start,
            end: start + 12,
        };
        for (range, expected) in [
            (direct, vec![direct]),
            (
                ByteRange {
                    start: nested,
                    end: nested + 12,
                },
                Vec::new(),
            ),
            (
                ByteRange {
                    start,
                    end: start + 11,
                },
                Vec::new(),
            ),
        ] {
            with_tokens(source, |tokens| {
                let mut scope = Scope::default();
                scope.insert("cfg_rt".to_owned(), true);
                let mut found = Vec::new();
                let result =
                    tokens.item_modules(whole(source), &mut scope, false, &[range], |range, _| {
                        found.push(range);
                        Some(())
                    });
                assert!(result.is_none());
                assert_eq!(found, expected);
                tokens.work.set(tokens.work_max);
                found.clear();
                assert!(
                    tokens
                        .item_modules(whole(source), &mut scope, false, &[range], |range, _| {
                            found.push(range);
                            Some(())
                        })
                        .is_none()
                );
                assert!(found.is_empty());
            });
        }
    }
}
