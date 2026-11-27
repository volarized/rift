//! Conditional item chains: `if #[cfg(..)] { items } else if #[cfg(..)] { items } else { items }`.
//!
//! A local `macro_rules!` definition whose rules are the conditional item chain (the
//! definition the cfg-if crate publishes, up to the macro and metavariable names) emits
//! each branch's items under the branch's own condition and the negation of every earlier
//! one. The `mod` items of an invocation of such a macro declare modules like ordinary
//! `#[cfg]` items do.

use std::collections::{BTreeMap, BTreeSet};

use rift_syntax::{ByteRange, SyntaxLimits};

use super::{consume, group};

pub(super) type ModuleBranches<'a> = Vec<(&'a str, Vec<&'a str>)>;

pub(super) enum ScopeAfter {
    CheckedImports(BTreeSet<String>),
    Unknown,
}

pub(super) type CheckedBranches<'a> = (ModuleBranches<'a>, ScopeAfter);

/// The conditional item chain definition without comments and attributes.
const DEFINITION: &str = "macro_rules! cfg_if {
    ($(if #[cfg($($meta:meta),*)] { $($it:item)* }) else * else { $($it2:item)* }) => {
        cfg_if! { @__items () ; $( ( ($($meta),*) ($($it)*) ), )* ( () ($($it2)*) ), }
    };
    (if #[cfg($($i_met:meta),*)] { $($i_it:item)* } $(else if #[cfg($($e_met:meta),*)] { $($e_it:item)* })*) => {
        cfg_if! { @__items () ; ( ($($i_met),*) ($($i_it)*) ), $( ( ($($e_met),*) ($($e_it)*) ), )* ( () () ), }
    };
    (@__items ($($not:meta,)*) ; ) => {};
    (@__items ($($not:meta,)*) ; ( ($($m:meta),*) ($($it:item)*) ), $($rest:tt)*) => {
        cfg_if! { @__apply cfg(all($($m,)* not(any($($not),*)))), $($it)* }
        cfg_if! { @__items ($($not,)* $($m,)*) ; $($rest)* }
    };
    (@__apply $m:meta, $($it:item)*) => { $(#[$m] $it)* };
}";

/// Original token texts of `range`, comments omitted, under the syntax bounds.
pub(crate) fn token_texts(
    source: &str,
    range: ByteRange,
    limits: SyntaxLimits,
) -> Option<Vec<&str>> {
    let mut texts = Vec::new();
    rift_syntax::tokens(source, range, limits, |token| {
        let Some(text) = super::text_range(source, token) else {
            return false;
        };
        texts.push(text);
        true
    })
    .then_some(texts)
}

/// Whether `tokens`, the item range of the definition of `name`, is the conditional item
/// chain definition. Macro, metavariable and internal rule names may differ; each must be
/// renamed consistently.
pub(super) fn is_definition(tokens: &[&str], name: &str, limits: SyntaxLimits) -> bool {
    let Ok(end) = u64::try_from(DEFINITION.len()) else {
        return false;
    };
    let Some(expected) = token_texts(DEFINITION, ByteRange { start: 0, end }, limits) else {
        return false;
    };
    if tokens.len() != expected.len() {
        return false;
    }
    let mut renamed = BTreeMap::new();
    let mut reverse = BTreeMap::new();
    for (index, (written, canonical)) in tokens.iter().zip(&expected).enumerate() {
        let previous = index.checked_sub(1).and_then(|index| expected.get(index));
        let renameable = *canonical == "cfg_if" || matches!(previous, Some(&("$" | "@")));
        if renameable && *canonical != "(" {
            if !super::super::crate_name_is_valid(written)
                || (*canonical == "cfg_if" && *written != name)
                || !rename(&mut renamed, &mut reverse, written, canonical)
            {
                return false;
            }
        } else if written != canonical {
            return false;
        }
    }
    true
}

/// Records `written` as the name of `canonical`; each must keep one counterpart.
fn rename<'a>(
    renamed: &mut BTreeMap<&'a str, &'a str>,
    reverse: &mut BTreeMap<&'a str, &'a str>,
    written: &'a str,
    canonical: &'a str,
) -> bool {
    let forward = *renamed.entry(canonical).or_insert(written);
    let backward = *reverse.entry(written).or_insert(canonical);
    forward == written && backward == canonical
}

/// The `mod` names of one conditional item chain invocation, each with the condition under
/// which its branch's items exist. `None` when the invocation is not a complete chain.
pub(crate) fn modules<'a>(tokens: &[&'a str], depth: usize) -> Option<ModuleBranches<'a>> {
    let mut body = invocation_body(tokens)?;
    let mut previous: Vec<&[&'a str]> = Vec::new();
    let mut modules = Vec::new();
    loop {
        if !consume(&mut body, &["if", "#"]) {
            return None;
        }
        let attribute = group(&mut body, "[", "]")?;
        let ["cfg", "(", predicate @ .., ")"] = attribute else {
            return None;
        };
        let metas = metas(predicate)?;
        let items = group(&mut body, "{", "}")?;
        let branch = condition(&metas, &previous);
        branch_modules(items, depth, |name| modules.push((name, branch.clone())))?;
        previous.extend(metas);
        if body.is_empty() {
            return Some(modules);
        }
        if !consume(&mut body, &["else"]) {
            return None;
        }
        if body.first() != Some(&"if") {
            let items = group(&mut body, "{", "}")?;
            let branch = condition(&[], &previous);
            branch_modules(items, depth, |name| modules.push((name, branch.clone())))?;
            return body.is_empty().then_some(modules);
        }
    }
}

/// Checks one local arm that applies its written metadata capture to each unchanged item.
pub(super) fn is_captured_predicate_definition(tokens: &[&str], name: &str) -> bool {
    let Some(input) =
        tokens.strip_prefix(&["macro_rules", "!", name, "{", "(", "#", "!", "[", "$"])
    else {
        return false;
    };
    let [
        predicate,
        ":",
        "meta",
        "]",
        "$",
        "(",
        "$",
        item,
        ":",
        "item",
        ")",
        "*",
        ")",
        "=",
        ">",
        "{",
        body @ ..,
    ] = input
    else {
        return false;
    };
    let Some(body) = body.strip_prefix(&["$", "(", "#", "[", "cfg", "(", "$", predicate, ")", "]"])
    else {
        return false;
    };
    // This exact documentation attribute leaves the emitted item and its guard unchanged.
    let body = body
        .strip_prefix(&[
            "#", "[", "cfg_attr", "(", "docsrs", ",", "doc", "(", "cfg", "(", "$", predicate, ")",
            ")", ")", "]",
        ])
        .unwrap_or(body);
    predicate != item
        && super::super::crate_name_is_valid(predicate)
        && super::super::crate_name_is_valid(item)
        && body
            .strip_prefix(&["$", item, ")", "*", "}"])
            .is_some_and(|rest| matches!(rest, ["}"] | [";", "}"]))
}

/// Places only the original outline items of a matched captured-predicate invocation.
pub(super) fn captured_predicate_modules<'a>(
    tokens: &[&'a str],
    depth: usize,
    selected: impl FnOnce(&[&str]) -> Option<bool>,
) -> Option<CheckedBranches<'a>> {
    let mut body = invocation_body(tokens)?;
    if !consume(&mut body, &["#", "!"]) {
        return None;
    }
    let predicate = group(&mut body, "[", "]")?;
    if predicate.last() == Some(&",") || metas(predicate)?.len() != 1 {
        return None;
    }
    let selection = selected(predicate);
    let mut modules = Vec::new();
    let active_scope = branch_modules(body, depth, |name| {
        if selection == Some(true) {
            modules.push((name, predicate.to_vec()));
        }
    })?;
    let scope = match selection {
        Some(true) => active_scope,
        Some(false) => ScopeAfter::CheckedImports(BTreeSet::new()),
        None => ScopeAfter::Unknown,
    };
    Some((modules, scope))
}

fn invocation_body<'tokens, 'source>(
    mut input: &'tokens [&'source str],
) -> Option<&'tokens [&'source str]> {
    super::super::source_identifier(input.first()?)?;
    input = &input[1..];
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
    let _ = consume(&mut input, &[";"]);
    input.is_empty().then_some(body)
}

/// The comma-separated predicates of `cfg(..)`; an empty predicate refuses the chain.
fn metas<'tokens, 'a>(input: &'tokens [&'a str]) -> Option<Vec<&'tokens [&'a str]>> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut nested = 0_usize;
    for (index, token) in input.iter().enumerate() {
        match *token {
            "(" => nested = nested.checked_add(1)?,
            ")" => nested = nested.checked_sub(1)?,
            "," if nested == 0 => {
                if start == index {
                    return None;
                }
                parts.push(&input[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if nested != 0 {
        return None;
    }
    if start < input.len() {
        parts.push(&input[start..]);
    }
    (!parts.is_empty()).then_some(parts)
}

/// `all(m.., not(any(earlier..)))`, the attribute the chain gives one branch's items.
fn condition<'a>(metas: &[&[&'a str]], previous: &[&[&'a str]]) -> Vec<&'a str> {
    let mut condition = vec!["all", "("];
    for meta in metas {
        condition.extend_from_slice(meta);
        condition.push(",");
    }
    condition.extend(["not", "(", "any", "("]);
    for (index, meta) in previous.iter().enumerate() {
        if index > 0 {
            condition.push(",");
        }
        condition.extend_from_slice(meta);
    }
    condition.extend([")", ")", ")"]);
    condition
}

/// Names of plain outline modules and the known bindings added by other checked items.
fn branch_modules<'a>(
    items: &[&'a str],
    depth: usize,
    mut module: impl FnMut(&'a str),
) -> Option<ScopeAfter> {
    let mut distinct = BTreeSet::new();
    let mut bindings = BTreeSet::new();
    let mut complete = true;
    super::scope::items(items, depth, |item| {
        let mut input = item;
        if super::scope::visibility(&mut input).is_none() {
            complete = false;
        } else if consume(&mut input, &["mod"])
            && let [name, ";"] = input
            && super::super::source_identifier(name).as_deref() == Some(*name)
        {
            complete &= distinct.insert(*name);
            module(name);
        } else if let Some(named) = super::scope::named_imports(input, depth) {
            for name in named {
                complete &= name != "cfg" && name != "cfg_attr" && bindings.insert(name);
            }
        } else {
            complete = false;
        }
        Some(())
    })?;
    Some(if complete {
        ScopeAfter::CheckedImports(bindings)
    } else {
        ScopeAfter::Unknown
    })
}

#[cfg(test)]
mod tests {
    use rift_syntax::{ByteRange, SyntaxLimits};

    fn texts(source: &str) -> Vec<&str> {
        super::token_texts(
            source,
            ByteRange {
                start: 0,
                end: u64::try_from(source.len()).expect("bounded source"),
            },
            SyntaxLimits::default(),
        )
        .expect("tokens")
    }

    #[test]
    fn conditional_chain_definition_allows_consistent_renaming_only() {
        let limits = SyntaxLimits::default();
        let canonical = texts(super::DEFINITION);
        assert!(super::is_definition(&canonical, "cfg_if", limits));
        let renamed = super::DEFINITION
            .replace("cfg_if", "select")
            .replace("$it2", "$other")
            .replace("@__apply", "@__emit");
        assert!(super::is_definition(&texts(&renamed), "select", limits));
        // A name that does not match the definition's own, a metavariable renamed in one
        // place only, or a changed transcription is not the chain.
        assert!(!super::is_definition(&canonical, "other", limits));
        let partial = super::DEFINITION.replacen("$m:meta", "$x:meta", 1);
        assert!(!super::is_definition(&texts(&partial), "cfg_if", limits));
        let changed = super::DEFINITION.replace("not(any(", "any(any(");
        assert!(!super::is_definition(&texts(&changed), "cfg_if", limits));
    }

    #[test]
    fn conditional_chain_branches_negate_every_earlier_condition() {
        let source = "pick! { if #[cfg(a)] { mod first; use first as imp; } else if #[cfg(b, c)] { pub(crate) mod second; } else { mod third; fn helper() {} } }";
        let modules = super::modules(&texts(source), 32).expect("chain");
        let rendered = modules
            .iter()
            .map(|(name, condition)| format!("{name}: {}", condition.concat()))
            .collect::<Vec<_>>();
        assert_eq!(
            rendered,
            [
                "first: all(a,not(any()))",
                "second: all(b,c,not(any(a)))",
                "third: all(not(any(a,b,c)))",
            ]
        );
        for refused in [
            "pick! { else { mod first; } }",
            "pick! { if #[cfg(a)] { mod first; } else }",
            "pick! { if #[cfg(a)] { mod first; } else { mod second; } else { mod third; } }",
            "pick! { if #[cfg()] { mod first; } }",
            "pick! { if #[other(a)] { mod first; } }",
        ] {
            assert!(super::modules(&texts(refused), 32).is_none(), "{refused}");
        }
    }
}
