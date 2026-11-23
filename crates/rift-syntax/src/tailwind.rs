//! Tailwind source facts extracted only after package or caller context establishes ownership.

use std::collections::BTreeMap;
use std::sync::Arc;

use rift_core::FileDigest;
use rift_error::{RiftError, errors};

use crate::angular::Children;
use crate::{
    ByteRange, SyntaxDocument, SyntaxFacts, SyntaxFactsParts, SyntaxLimits, SyntaxNames,
    SyntaxSource, SyntaxSymbol,
};

const UTILITY: &str = "utility";
const VARIANT: &str = "variant";
const THEME: &str = "theme";
const CONFIGURATION: &str = "configuration";

/// Authored Tailwind names and expressions whose values require runtime evaluation.
#[derive(Debug, Default)]
pub struct TailwindFacts {
    /// Static utility, variant, theme, and configuration names with original byte ranges.
    pub symbols: Vec<SyntaxSymbol>,
    /// Original dynamic class expression ranges, without inferred utility names.
    pub unresolved: Vec<ByteRange>,
}

struct Collector {
    facts: TailwindFacts,
    remaining: usize,
    exhausted: bool,
}

impl Collector {
    fn admit(&mut self) -> bool {
        if self.remaining == 0 {
            self.exhausted = true;
            return false;
        }
        self.remaining -= 1;
        true
    }
    fn symbol(&mut self, name: &str, kind: &'static str, range: ByteRange) {
        if self.admit() {
            self.facts.symbols.push(symbol(name, kind, range));
        }
    }
    fn unresolved(&mut self, range: ByteRange) {
        if self.admit() {
            self.facts.unresolved.push(range);
        }
    }
}

/// Extracts source facts for an established Tailwind major version from captured syntax.
///
/// This function records authored class names rather than checking compiler availability.
///
/// # Errors
///
/// Returns source or aggregate fact bound failures and invalid source witnesses.
pub fn tailwind_symbols(
    source: SyntaxSource<'_>,
    document: &SyntaxDocument,
    major: u8,
    limits: SyntaxLimits,
) -> Result<TailwindFacts, RiftError> {
    limits.admit_source(source)?;
    if document.source_digest() != Some(&FileDigest::of(source.text.as_bytes())) {
        return errors::syntax::facts_range_invalid().fail();
    }
    let Some(remaining) = limits
        .syntax_elements_max()
        .checked_sub(document.syntax_elements().len())
    else {
        return errors::syntax::too_many_syntax_elements()
            .path(source.path)
            .syntax_elements_max(limits.syntax_elements_max())
            .fail();
    };
    crate::embedded::admit_depth(source, limits, 0, document)?;
    let children = Children::new(document.syntax_elements());
    let header_ends = header_ends(source.text, &children);
    let mut facts = Collector {
        facts: TailwindFacts::default(),
        remaining,
        exhausted: false,
    };
    for (index, syntax_element) in document.syntax_elements().iter().enumerate() {
        match syntax_element.kind {
            "attribute" | "directive_attribute" | "jsx_attribute" => {
                classes(source.text, &children, index, &mut facts);
            }
            "at_keyword" => directive(
                source.text,
                &children,
                index,
                header_ends[&index],
                major,
                &mut facts,
            ),
            "import_statement" => import_reference(source.text, &children, index, &mut facts),
            "call_expression" => theme_reference(source.text, &children, index, &mut facts),
            _ => {}
        }
        if facts.exhausted {
            return errors::syntax::too_many_syntax_elements()
                .path(source.path)
                .syntax_elements_max(limits.syntax_elements_max())
                .fail();
        }
    }
    facts.facts.symbols.sort_by_key(|symbol| symbol.range.start);
    facts.facts.unresolved.sort();
    facts.facts.unresolved.dedup();
    Ok(facts.facts)
}

fn import_reference(text: &str, children: &Children<'_>, index: usize, facts: &mut Collector) {
    if !children
        .text(index, text)
        .is_some_and(|written| written.starts_with("@import"))
    {
        return;
    }
    if let Some(value) = descendants(children, index)
        .find(|child| children.syntax_elements[*child].kind == "string_value")
    {
        first_word(
            text,
            children.syntax_elements[value].range,
            CONFIGURATION,
            facts,
        );
    }
}

fn header_ends(text: &str, children: &Children<'_>) -> BTreeMap<usize, u64> {
    let keywords: Vec<usize> = children
        .syntax_elements
        .iter()
        .enumerate()
        .filter_map(|(index, syntax_element)| {
            (syntax_element.kind == "at_keyword").then_some(index)
        })
        .collect();
    keywords
        .iter()
        .enumerate()
        .map(|(position, index)| {
            let next = keywords
                .get(position + 1)
                .map_or(text.len() as u64, |next| {
                    children.syntax_elements[*next].range.start
                });
            let mut parent = children.syntax_elements[*index].parent;
            let mut end = next;
            while let Some(index) = parent {
                let syntax_element = &children.syntax_elements[index];
                if syntax_element.kind == "stylesheet" {
                    end = end.min(syntax_element.range.end);
                    break;
                }
                parent = syntax_element.parent;
            }
            (*index, end)
        })
        .collect()
}

fn classes(text: &str, children: &Children<'_>, index: usize, facts: &mut Collector) {
    if angular_class_binding(text, children, index, facts) {
        return;
    }
    let Some(name) = children
        .indices(index)
        .next()
        .and_then(|child| children.text(child, text))
    else {
        return;
    };
    let name = if children.syntax_elements[index].kind == "directive_attribute" && name == "v-bind"
    {
        let Some(value) = children
            .child(index, "directive_value")
            .and_then(|value| children.text(value, text))
        else {
            return;
        };
        value
    } else {
        name
    };
    if let Some(name) = name.strip_prefix("class:") {
        let attribute = children
            .indices(index)
            .next()
            .expect("captured attribute name");
        let end = children.syntax_elements[attribute].range.end;
        let range = ByteRange {
            start: end - name.len() as u64,
            end,
        };
        if !name.is_empty() {
            facts.symbol(name, UTILITY, range);
        }
        facts.unresolved(children.syntax_elements[index].range);
        return;
    }
    if !matches!(
        name,
        "class" | "className" | ":class" | "v-bind:class" | "[class]" | "[ngClass]"
    ) {
        return;
    }
    class_value(text, children, index, name, facts);
}

fn angular_class_binding(
    text: &str,
    children: &Children<'_>,
    index: usize,
    facts: &mut Collector,
) -> bool {
    let Some(binding) = children.child(index, "property_binding") else {
        return false;
    };
    let class = children.child(binding, "class_binding");
    let name = children
        .child(binding, "binding_name")
        .and_then(|name| children.text(name, text));
    if class.is_none() && !matches!(name, Some("class" | "ngClass" | "className" | "attr.class")) {
        return false;
    }
    if let Some(name) = class.and_then(|class| children.child(class, "class_name"))
        && let Some(written) = children.text(name, text)
    {
        facts.symbol(written, UTILITY, children.syntax_elements[name].range);
    }
    facts.unresolved(children.syntax_elements[binding].range);
    true
}

fn class_value(
    text: &str,
    children: &Children<'_>,
    index: usize,
    name: &str,
    facts: &mut Collector,
) {
    let descendants = descendants(children, index);
    let value = descendants.clone().find(|child| {
        matches!(
            children.syntax_elements[*child].kind,
            "attribute_value" | "string" | "template_string"
        )
    });
    let dynamic = children.syntax_elements[index].kind == "directive_attribute"
        || !matches!(name, "class" | "className")
        || descendants.clone().any(|child| {
            matches!(
                children.syntax_elements[child].kind,
                "template_substitution" | "interpolation" | "expression"
            )
        });
    if let Some(value) = value {
        if let Some((_, range)) = children
            .literal(value, text)
            .filter(|_| static_class_literal(children, value, index))
        {
            if descendants
                .clone()
                .any(|child| children.syntax_elements[child].kind == "template_substitution")
            {
                facts.unresolved(children.syntax_elements[index].range);
            } else {
                utilities(text, range, facts);
            }
        } else if children.syntax_elements[value].kind == "attribute_value" && !dynamic {
            let range = children.syntax_elements[value].range;
            if let Some(value) = children.text(value, text) {
                if value.contains(['{', '}']) {
                    facts.unresolved(range);
                } else {
                    utilities(text, range, facts);
                }
            }
        } else {
            facts.unresolved(children.syntax_elements[index].range);
        }
    } else {
        facts.unresolved(children.syntax_elements[index].range);
    }
}

fn static_class_literal(children: &Children<'_>, value: usize, attribute: usize) -> bool {
    let Some(parent) = children.syntax_elements[value].parent else {
        return false;
    };
    parent == attribute
        || (children.syntax_elements[parent].kind == "jsx_expression"
            && children.indices(parent).count() == 1)
}

fn descendants<'syntax_elements>(
    children: &'syntax_elements Children<'_>,
    parent: usize,
) -> impl Iterator<Item = usize> + Clone + 'syntax_elements {
    let end = children.syntax_elements[parent].range.end;
    ((parent + 1)..children.syntax_elements.len())
        .take_while(move |index| children.syntax_elements[*index].range.start < end)
}

fn directive(
    text: &str,
    children: &Children<'_>,
    index: usize,
    end: u64,
    major: u8,
    facts: &mut Collector,
) {
    let Some(name) = children.text(index, text) else {
        return;
    };
    if !matches!(
        name,
        "@apply"
            | "@tailwind"
            | "@screen"
            | "@layer"
            | "@utility"
            | "@custom-variant"
            | "@variant"
            | "@config"
            | "@import"
            | "@plugin"
            | "@source"
            | "@reference"
            | "@theme"
    ) {
        return;
    }
    let range = directive_header(
        text,
        ByteRange {
            start: children.syntax_elements[index].range.end,
            end,
        },
    );
    match name {
        "@apply" => utilities(text, range, facts),
        "@tailwind" | "@screen" | "@layer" if major == 3 => {
            first_word(
                text,
                range,
                if name == "@screen" {
                    VARIANT
                } else {
                    CONFIGURATION
                },
                facts,
            );
            if name == "@layer"
                && text_range(text, range).and_then(|text| text.split_ascii_whitespace().next())
                    == Some("utilities")
            {
                layer_utilities(text, children, range.end, facts);
            }
        }
        "@utility" if major == 4 => first_word(text, range, UTILITY, facts),
        "@custom-variant" | "@variant" if major == 4 => first_word(text, range, VARIANT, facts),
        "@config" if matches!(major, 3 | 4) => first_word(text, range, CONFIGURATION, facts),
        "@import" | "@plugin" | "@source" | "@reference" if major == 4 => {
            first_word(text, range, CONFIGURATION, facts);
        }
        "@theme" if major == 4 => theme_properties(text, children, range.end, facts),
        _ => {}
    }
}

fn directive_header(text: &str, range: ByteRange) -> ByteRange {
    let Some(value) = text_range(text, range) else {
        return range;
    };
    let mut quote = None;
    let mut escaped = false;
    for (position, character) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if quote.is_some() && character == '\\' {
            escaped = true;
            continue;
        }
        if Some(character) == quote {
            quote = None;
            continue;
        }
        if quote.is_some() {
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            ';' | '{' => {
                return ByteRange {
                    start: range.start,
                    end: range.start + position as u64,
                };
            }
            _ => {}
        }
    }
    range
}

fn directive_block(children: &Children<'_>, start: u64) -> Option<usize> {
    let first = children
        .syntax_elements
        .partition_point(|syntax_element| syntax_element.range.start < start);
    (first..children.syntax_elements.len())
        .take_while(|index| children.syntax_elements[*index].range.start == start)
        .find(|index| children.syntax_elements[*index].kind == "block")
}

fn layer_utilities(text: &str, children: &Children<'_>, start: u64, facts: &mut Collector) {
    let Some(block) = directive_block(children, start) else {
        return;
    };
    for child in descendants(children, block)
        .filter(|child| children.syntax_elements[*child].kind == "class_name")
    {
        if facts.exhausted {
            return;
        }
        if let Some(name) = children.text(child, text) {
            facts.symbol(name, UTILITY, children.syntax_elements[child].range);
        }
    }
}

fn theme_properties(text: &str, children: &Children<'_>, start: u64, facts: &mut Collector) {
    let Some(block) = directive_block(children, start) else {
        return;
    };
    for child in descendants(children, block)
        .filter(|child| children.syntax_elements[*child].kind == "property_name")
    {
        if facts.exhausted {
            return;
        }
        if let Some(name) = children
            .text(child, text)
            .filter(|name| name.starts_with("--"))
        {
            facts.symbol(name, THEME, children.syntax_elements[child].range);
        }
    }
}

fn theme_reference(text: &str, children: &Children<'_>, index: usize, facts: &mut Collector) {
    let Some(function) = children.child(index, "function_name") else {
        return;
    };
    if children.text(function, text) != Some("theme") {
        return;
    }
    let Some(arguments) = children.child(index, "arguments") else {
        return;
    };
    let range = children.syntax_elements[arguments].range;
    let Some(end) = range.end.checked_sub(1) else {
        return;
    };
    first_word(
        text,
        ByteRange {
            start: range.start + 1,
            end,
        },
        THEME,
        facts,
    );
}

fn first_word(text: &str, range: ByteRange, kind: &'static str, facts: &mut Collector) {
    let Some(value) = text_range(text, range) else {
        return;
    };
    let whitespace = value.len() - value.trim_start().len();
    let value = value.trim_start();
    let (leading_quote, name) = if let Some(quote) = value
        .chars()
        .next()
        .filter(|quote| matches!(quote, '\'' | '"'))
    {
        let Some(end) = value[1..].find(quote) else {
            facts.unresolved(range);
            return;
        };
        let name = &value[1..=end];
        if name.contains('\\') {
            facts.unresolved(range);
            return;
        }
        (1, name)
    } else {
        (0, value.split_ascii_whitespace().next().unwrap_or(""))
    };
    if name.is_empty() {
        return;
    }
    let start = range.start + (whitespace + leading_quote) as u64;
    facts.symbol(
        name,
        kind,
        ByteRange {
            start,
            end: start + name.len() as u64,
        },
    );
}

fn utilities(text: &str, range: ByteRange, facts: &mut Collector) {
    let Some(value) = text_range(text, range) else {
        return;
    };
    let mut offset = 0;
    for token in value.split_ascii_whitespace() {
        if facts.exhausted {
            return;
        }
        let Some(relative) = value[offset..].find(token) else {
            break;
        };
        offset += relative;
        let start = range.start + u64::try_from(offset).unwrap_or(u64::MAX);
        if token == "!important" {
            offset += token.len();
            continue;
        }
        if token.contains(['{', '}', '$']) {
            facts.unresolved(ByteRange {
                start,
                end: start + token.len() as u64,
            });
        } else {
            let mut bracket_depth = 0_u32;
            let mut utility_start = 0;
            for (position, character) in token.char_indices() {
                if facts.exhausted {
                    return;
                }
                match character {
                    '[' | '(' => bracket_depth += 1,
                    ']' | ')' => bracket_depth = bracket_depth.saturating_sub(1),
                    ':' if bracket_depth == 0 => {
                        if utility_start < position {
                            let range = ByteRange {
                                start: start + utility_start as u64,
                                end: start + position as u64,
                            };
                            facts.symbol(&token[utility_start..position], VARIANT, range);
                        }
                        utility_start = position + 1;
                    }
                    _ => {}
                }
            }
            if utility_start < token.len() {
                let range = ByteRange {
                    start: start + utility_start as u64,
                    end: start + token.len() as u64,
                };
                facts.symbol(&token[utility_start..], UTILITY, range);
            }
        }
        offset += token.len();
    }
}

fn symbol(name: &str, kind: &'static str, range: ByteRange) -> SyntaxSymbol {
    SyntaxSymbol {
        name: name.to_owned(),
        declaration_ordinal: 0,
        qualified_name: name.to_owned(),
        container: None,
        kind,
        syntax_element_kind: None,
        kinds: Vec::new(),
        visibility: None,
        range,
        item_range: range,
        name_range: Some(range),
        body_range: None,
        signatures: Arc::from([]),
        documentation: Arc::from([]),
        documentation_ranges: Vec::new(),
        module_path: None,
        python_overload: None,
    }
}

fn text_range(text: &str, range: ByteRange) -> Option<&str> {
    text.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
}

pub(crate) fn restored_symbol_kind(name: &str) -> Option<&'static str> {
    [UTILITY, VARIANT, THEME, CONFIGURATION]
        .into_iter()
        .find(|kind| *kind == name)
}

/// Adds contextual source declarations while retaining parser identity and original syntax elements.
///
/// # Errors
///
/// Returns invalid facts, source witness failures, or aggregate fact bound failures.
pub fn append_framework_symbols(
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
    document: &SyntaxDocument,
    symbols: Vec<SyntaxSymbol>,
) -> Result<SyntaxDocument, RiftError> {
    limits.admit_source(source)?;
    if document.source_digest() != Some(&FileDigest::of(source.text.as_bytes())) {
        return errors::syntax::facts_range_invalid().fail();
    }
    if document
        .syntax_elements()
        .len()
        .saturating_add(symbols.len())
        > limits.syntax_elements_max()
    {
        return errors::syntax::too_many_syntax_elements()
            .path(source.path)
            .syntax_elements_max(limits.syntax_elements_max())
            .fail();
    }
    crate::embedded::admit_depth(source, limits, 0, document)?;
    let names = SyntaxNames::new(document.language())
        .ok_or_else(|| errors::syntax::facts_structure_invalid().error())?;
    for symbol in &symbols {
        if restored_symbol_kind(symbol.kind).is_none() || names.symbol_kind(symbol.kind).is_none() {
            return errors::syntax::facts_structure_invalid().fail();
        }
    }
    let omitted = document.left_out_declaration_count();
    if document
        .symbols()
        .len()
        .saturating_add(symbols.len())
        .saturating_add(omitted)
        > limits.syntax_elements_max()
    {
        return errors::syntax::too_many_syntax_elements()
            .path(source.path)
            .syntax_elements_max(limits.syntax_elements_max())
            .fail();
    }
    let mut combined = document.symbols().to_vec();
    combined.extend_from_slice(document.facts().left_out_symbols());
    combined.extend(symbols);
    combined.sort_by_key(|symbol| symbol.range.start);
    let mut result = SyntaxDocument::new(
        document.language().clone(),
        source.path.clone(),
        document.syntax_elements().to_vec(),
        combined,
        document.has_errors(),
    )
    .with_source_witness(source.text)
    .with_syntax_limits(limits)
    .with_variance_errors_only(document.facts().variance_errors_only())
    .with_static_facts(document.facts().static_facts().cloned())
    .with_left_out_declarations(omitted - document.facts().left_out_symbols().len())
    .with_framework_context();
    if let Some(bindings) = document.facts().export_bindings() {
        result = result.with_export_bindings(bindings.to_vec());
    }
    let parts = SyntaxFactsParts {
        origin: result.facts().origin(),
        language: result.language().clone(),
        symbols: result.symbols().to_vec(),
        left_out_symbols: result.facts().left_out_symbols().to_vec(),
        has_errors: result.has_errors(),
        variance_errors_only: result.facts().variance_errors_only(),
        left_out_declarations: result.left_out_declaration_count(),
        markdown_facts: None,
        export_bindings: result.facts().export_bindings().map(<[_]>::to_vec),
        macro_invocations: result.facts().macro_invocations().map(<[_]>::to_vec),
        scope: result.facts().scope().map(<[_]>::to_vec),
        static_facts: result.facts().static_facts().cloned(),
        source_digest: FileDigest::of(source.text.as_bytes()),
    };
    SyntaxFacts::from_parts(source.text, limits, parts)?;
    Ok(result)
}
