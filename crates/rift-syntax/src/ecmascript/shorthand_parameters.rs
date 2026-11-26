//! Parameters whose shorthand properties already hold the shared package name.

use std::collections::BTreeMap;

use crate::{ByteRange, StaticFacts, SyntaxSymbol};

use super::{
    FUNCTION_DECLARATION_KIND, GENERATOR_FUNCTION_DECLARATION_KIND, PARAMETER_KIND,
    SHORTHAND_PARAMETER_KIND,
};

pub(super) fn capture(
    symbols: &[SyntaxSymbol],
    parameters: &[SyntaxSymbol],
    facts: &StaticFacts,
) -> Vec<SyntaxSymbol> {
    let mut by_name = BTreeMap::<&str, Vec<&SyntaxSymbol>>::new();
    for symbol in symbols {
        by_name
            .entry(&symbol.qualified_name)
            .or_default()
            .push(symbol);
    }
    let sites = facts
        .parameters()
        .iter()
        .map(|site| (site.name, site))
        .collect::<BTreeMap<ByteRange, _>>();
    let mut captured = BTreeMap::new();
    for parameter in parameters {
        let Some(container) = parameter.container.as_deref() else {
            continue;
        };
        let Some(site) = parameter.name_range.and_then(|name| sites.get(&name)) else {
            continue;
        };
        let Some([owner]) = by_name.get(container).map(Vec::as_slice) else {
            continue;
        };
        if parameter.kind != PARAMETER_KIND
            || !matches!(
                owner.syntax_element_kind,
                Some(FUNCTION_DECLARATION_KIND | GENERATOR_FUNCTION_DECLARATION_KIND)
            )
            || site.owner != owner.name_range
            || site.function != owner.item_range
        {
            continue;
        }
        let Some(properties) = by_name.get(parameter.qualified_name.as_str()) else {
            continue;
        };
        if !properties.iter().all(|property| {
            property.kind == "property"
                && property.syntax_element_kind == Some("shorthand_property_identifier")
                && property.container.as_deref() == Some(container)
        }) {
            continue;
        }
        let qualified = format!("{container}.$parameter.{}", parameter.name);
        if by_name.contains_key(qualified.as_str()) || !rift_core::is_portable_name(&qualified) {
            continue;
        }
        captured
            .entry(qualified.clone())
            .and_modify(|entry| *entry = None)
            .or_insert_with(|| {
                let mut parameter = parameter.clone();
                parameter.kind = SHORTHAND_PARAMETER_KIND;
                parameter.qualified_name = qualified;
                Some(parameter)
            });
    }
    captured.into_values().flatten().collect()
}
