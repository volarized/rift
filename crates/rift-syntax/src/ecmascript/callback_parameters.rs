//! Authored bindings of ECMAScript function values.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rift_protocol::read::SymbolKind;

use crate::{ByteRange, StaticCallee, StaticFacts, StaticFunctionContext, SyntaxSymbol};

use super::{
    CALLBACK_PARAMETER_KIND, CALLBACK_PATTERN_PARAMETER_KIND,
    TYPED_ARGUMENT_CALLBACK_PARAMETER_KIND,
};

pub(super) fn append(
    symbols: &mut Vec<SyntaxSymbol>,
    facts: &StaticFacts,
    source: &str,
    javascript: bool,
) {
    if facts.parameters().is_empty()
        && (!javascript || facts.callback_pattern_parameters().is_empty())
    {
        return;
    }
    let typed = (!javascript).then(|| typed_arrow_arguments(facts));
    if typed.as_ref().is_some_and(BTreeSet::is_empty) {
        return;
    }
    let callbacks = facts
        .function_values()
        .iter()
        .filter(|(range, value)| range == value)
        .map(|(range, _)| *range)
        .collect::<BTreeSet<_>>();
    if callbacks.is_empty() {
        return;
    }
    let ordinals = callbacks
        .into_iter()
        .enumerate()
        .map(|(ordinal, range)| (range, ordinal))
        .collect::<BTreeMap<_, _>>();
    let owners = symbols
        .iter()
        .filter_map(|symbol| Some((symbol.name_range?, symbol)))
        .collect::<BTreeMap<_, _>>();
    let declared = symbols
        .iter()
        .map(|symbol| symbol.qualified_name.as_str())
        .collect::<BTreeSet<_>>();
    let mut captured = BTreeMap::new();
    let ordinary_kind = if javascript {
        CALLBACK_PARAMETER_KIND
    } else {
        TYPED_ARGUMENT_CALLBACK_PARAMETER_KIND
    };
    let patterns = if javascript {
        facts.callback_pattern_parameters()
    } else {
        &[]
    };
    let sites = facts
        .parameters()
        .iter()
        .map(|site| (site, ordinary_kind))
        .chain(
            patterns
                .iter()
                .map(|site| (site, CALLBACK_PATTERN_PARAMETER_KIND)),
        );
    for (site, kind) in sites {
        if typed
            .as_ref()
            .is_some_and(|checked| !checked.contains(&site.function))
        {
            continue;
        }
        let Some(ordinal) = ordinals.get(&site.function) else {
            continue;
        };
        let Some(name) = text(source, site.name) else {
            continue;
        };
        if name == "this" || !name.is_ascii() || name.contains('\\') {
            continue;
        }
        let owner = site
            .owner
            .and_then(|name| owners.get(&name))
            .filter(|owner| {
                owner.kinds.contains(&SymbolKind::Callable)
                    && owner
                        .body_range
                        .is_some_and(|body| body.encloses(site.function))
            });
        let container = owner.map(|owner| owner.qualified_name.clone());
        let qualified = container.as_ref().map_or_else(
            || format!("$callback#{ordinal}.{name}"),
            |owner| format!("{owner}.{name}"),
        );
        if declared.contains(qualified.as_str()) || !rift_core::is_portable_name(&qualified) {
            continue;
        }
        captured
            .entry(qualified.clone())
            .and_modify(|row| *row = None)
            .or_insert_with(|| Some(parameter(name, qualified, container, site.name, kind)));
    }
    symbols.extend(captured.into_values().flatten());
}

/// Finds physically written arrow arguments without resolving their call targets.
fn typed_arrow_arguments(facts: &StaticFacts) -> BTreeSet<ByteRange> {
    let non_arrows = facts
        .function_scopes()
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let calls = facts
        .calls()
        .iter()
        .filter(|call| {
            matches!(
                call.callee,
                StaticCallee::Direct | StaticCallee::Member { .. }
            )
        })
        .map(|call| (call.range, call.callee_range))
        .collect::<BTreeMap<_, _>>();
    facts
        .function_contexts()
        .iter()
        .filter_map(|&(function, context)| {
            let StaticFunctionContext::Argument { call, .. } = context else {
                return None;
            };
            let callee = calls.get(&call)?;
            (callee.end <= function.start
                && call.encloses(function)
                && !non_arrows.contains(&function))
            .then_some(function)
        })
        .collect()
}

fn text(source: &str, range: ByteRange) -> Option<&str> {
    source.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
}

fn parameter(
    name: &str,
    qualified_name: String,
    container: Option<String>,
    name_range: ByteRange,
    kind: &'static str,
) -> SyntaxSymbol {
    SyntaxSymbol {
        name: name.to_owned(),
        qualified_name,
        declaration_ordinal: 0,
        container,
        kind,
        syntax_element_kind: Some("identifier"),
        kinds: vec![SymbolKind::Value, SymbolKind::Parameter],
        visibility: None,
        range: name_range,
        item_range: name_range,
        name_range: Some(name_range),
        body_range: None,
        signatures: Arc::default(),
        documentation: Arc::default(),
        documentation_ranges: Vec::new(),
        module_path: None,
        python_overload: None,
    }
}
