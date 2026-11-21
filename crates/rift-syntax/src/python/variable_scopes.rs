//! Variable declarations use captured global and nonlocal bindings.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::LoopBudget;
use rift_error::{RiftError, errors};
use rift_protocol::read::SymbolKind;

use crate::{ByteRange, StaticBindingKind, StaticFacts, SyntaxLimits, SyntaxSymbol};

use super::{FUNCTION_KIND_WORD, METHOD_KIND_WORD, VARIABLE_KIND_WORD};

pub(super) fn apply(
    symbols: &mut [SyntaxSymbol],
    facts: &StaticFacts,
    source: &str,
    limits: SyntaxLimits,
) -> Result<(), RiftError> {
    if !facts
        .bindings()
        .iter()
        .any(|binding| scope_declaration(binding.kind))
    {
        return Ok(());
    }
    let mut work = LoopBudget::new(limits.syntax_elements_max());
    let mut scopes = BTreeMap::<_, Vec<_>>::new();
    for symbol in symbols.iter() {
        charge(&mut work)?;
        if matches!(
            symbol.kind,
            FUNCTION_KIND_WORD | METHOD_KIND_WORD | super::CLASS_KIND_WORD
        ) {
            scopes
                .entry(symbol.qualified_name.as_str())
                .or_default()
                .push(symbol);
        }
    }
    let mut names = BTreeSet::new();
    let mut declarations = BTreeMap::new();
    let mut bindings = BTreeMap::new();
    for binding in facts.bindings() {
        charge(&mut work)?;
        let Some(name) = text(source, binding.name) else {
            continue;
        };
        let key = (binding.scope, name);
        if scope_declaration(binding.kind) {
            declarations.insert(key, binding.kind);
        } else {
            names.insert(key);
            bindings.insert(binding.name, binding.scope);
        }
    }
    let mut updates = Vec::new();
    for (index, symbol) in symbols.iter().enumerate() {
        charge(&mut work)?;
        if symbol.kind != VARIABLE_KIND_WORD {
            continue;
        }
        let Some(scope) = symbol.name_range.and_then(|name| bindings.get(&name)) else {
            continue;
        };
        let key = (*scope, symbol.name.as_str());
        match declarations.get(&key) {
            Some(StaticBindingKind::PythonGlobal) => updates.push((index, None)),
            Some(StaticBindingKind::PythonNonlocal) => {
                if let Some(container) =
                    nonlocal_container(symbol, *scope, &scopes, &names, &declarations, &mut work)?
                {
                    updates.push((index, Some(container)));
                }
            }
            _ => {}
        }
    }
    if updates.is_empty() {
        return Ok(());
    }
    for (index, container) in updates {
        charge(&mut work)?;
        let symbol = &mut symbols[index];
        symbol.qualified_name = container.as_ref().map_or_else(
            || symbol.name.clone(),
            |container| format!("{container}.{}", symbol.name),
        );
        symbol.container = container;
        symbol.kinds.retain(|kind| *kind != SymbolKind::Member);
    }
    let mut ordinals = BTreeMap::new();
    for symbol in symbols {
        charge(&mut work)?;
        let ordinal = ordinals.entry(symbol.qualified_name.as_str()).or_insert(0);
        symbol.declaration_ordinal = *ordinal;
        *ordinal += 1;
    }
    Ok(())
}

fn nonlocal_container(
    symbol: &SyntaxSymbol,
    scope: ByteRange,
    scopes: &BTreeMap<&str, Vec<&SyntaxSymbol>>,
    names: &BTreeSet<(ByteRange, &str)>,
    declarations: &BTreeMap<(ByteRange, &str), StaticBindingKind>,
    work: &mut LoopBudget,
) -> Result<Option<String>, RiftError> {
    let mut parent = symbol.container.as_deref();
    while let Some(name) = parent {
        charge(work)?;
        let Some(candidates) = scopes.get(name) else {
            break;
        };
        let mut enclosing = None;
        for &candidate in candidates {
            charge(work)?;
            if candidate.item_range.encloses(symbol.item_range) {
                enclosing = Some(candidate);
                break;
            }
        }
        let Some(candidate) = enclosing else {
            break;
        };
        let key = (candidate.item_range, symbol.name.as_str());
        if candidate.item_range != scope
            && matches!(candidate.kind, FUNCTION_KIND_WORD | METHOD_KIND_WORD)
            && names.contains(&key)
            && !declarations.contains_key(&key)
        {
            return Ok(Some(candidate.qualified_name.clone()));
        }
        parent = candidate.container.as_deref();
    }
    Ok(None)
}

fn scope_declaration(kind: StaticBindingKind) -> bool {
    matches!(
        kind,
        StaticBindingKind::PythonGlobal | StaticBindingKind::PythonNonlocal
    )
}

fn text(source: &str, range: ByteRange) -> Option<&str> {
    source.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
}

fn charge(work: &mut LoopBudget) -> Result<(), RiftError> {
    work.consume()
        .map_err(|_| errors::syntax::facts_count_exceeded().error())
}
