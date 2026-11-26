//! Physical parameters of functions that share a source name.

use std::collections::BTreeMap;

use crate::{ByteRange, StaticFacts, SyntaxSymbol};

use super::SCOPED_FUNCTION_PARAMETER_KIND;

pub(super) fn capture(
    symbols: &[SyntaxSymbol],
    parameters: &[SyntaxSymbol],
    facts: &StaticFacts,
) -> Vec<SyntaxSymbol> {
    let mut declarations = BTreeMap::<&str, Vec<&SyntaxSymbol>>::new();
    for symbol in symbols {
        declarations
            .entry(&symbol.qualified_name)
            .or_default()
            .push(symbol);
    }
    let mut scopes = BTreeMap::new();
    for mut group in declarations.into_values() {
        if group.len() < 2
            || !group.iter().all(|symbol| {
                symbol.container.is_none()
                    && symbol.syntax_element_kind == Some("function_declaration")
                    && symbol.body_range.is_some()
            })
        {
            continue;
        }
        group.sort_unstable_by_key(|declaration| declaration.item_range.start);
        for (ordinal, declaration) in group.into_iter().enumerate() {
            if let Some(name) = declaration.name_range {
                scopes.insert((name, declaration.item_range), (declaration, ordinal));
            }
        }
    }
    let owners = facts
        .parameters()
        .iter()
        .filter_map(|site| {
            let (owner, ordinal) = scopes.get(&(site.owner?, site.function))?;
            Some((site.name, (*owner, *ordinal)))
        })
        .collect::<BTreeMap<ByteRange, _>>();
    let mut retained = BTreeMap::new();
    for parameter in parameters {
        let Some((owner, ordinal)) = parameter.name_range.and_then(|name| owners.get(&name)) else {
            continue;
        };
        let container = owner.qualified_name.as_str();
        if parameter.container.as_deref() != Some(container) {
            continue;
        }
        let qualified = format!("{container}.$scope#{ordinal}.{}", parameter.name);
        if !rift_core::is_portable_name(&qualified) {
            continue;
        }
        retained
            .entry(qualified.clone())
            .and_modify(|row| *row = None)
            .or_insert_with(|| {
                let mut located = parameter.clone();
                located.kind = SCOPED_FUNCTION_PARAMETER_KIND;
                located.qualified_name = qualified;
                Some(located)
            });
    }
    retained.into_values().flatten().collect()
}
