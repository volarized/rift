//! References from written TypeScript declaration property selectors.

use std::collections::BTreeMap;

use rift_core::ReferenceRole;
use rift_provider::NormalizedTarget;
use rift_syntax::{
    ByteRange, CheckedKeyofBound, CheckedPropertySelector, StaticBindingKind, SyntaxSource,
    TypeScriptDialect,
};

use super::javascript::{MemberMode, js_binding};
use super::{File, Index, StaticOccurrence, StaticResolution, Target, Work, is_declaration_file};

type KeyofMemberCache<'source> =
    BTreeMap<(&'source str, ByteRange, &'source str), Option<Vec<Target<'source>>>>;

type KeyofRequest<'index, 'source> = (&'index File<'source>, CheckedPropertySelector);

#[derive(Default)]
pub(super) struct KeyofRequests<'index, 'source> {
    parameters: Vec<KeyofRequest<'index, 'source>>,
    conditional: Vec<KeyofRequest<'index, 'source>>,
}

impl<'source> Index<'source> {
    pub(super) fn resolve_typescript_property_selectors(
        &self,
        work: &mut Work,
        result: &mut StaticResolution,
    ) -> KeyofRequests<'_, 'source> {
        let start = result.occurrences.len();
        let provider = TypeScriptDialect::TypeScript.provider();
        let mut keyof = KeyofRequests::default();
        'files: for file in self.files.values() {
            let document = file.document;
            let facts = document.facts;
            if facts.language().name != "typescript"
                || !is_declaration_file(document.path.as_str())
                || !facts.declaration_syntax_complete()
                || !document.source.contains(['[', '{'])
            {
                continue;
            }
            if !work.charge() {
                break;
            }
            let Some(limits) = facts.syntax_limits() else {
                continue;
            };
            let source = SyntaxSource {
                path: document.path,
                text: document.source,
            };
            let Ok(selectors) = provider.checked_property_selectors(source, limits) else {
                continue;
            };
            for selector in selectors {
                if let CheckedPropertySelector::KeyofIndexedAccess { proof, .. } = selector {
                    match proof {
                        CheckedKeyofBound::Parameter => &mut keyof.parameters,
                        CheckedKeyofBound::TrueBranch { .. } => &mut keyof.conditional,
                    }
                    .push((file, selector));
                    continue;
                }
                if !work.charge() {
                    break 'files;
                }
                result.record(self.checked_property_selector(file, selector, work));
            }
        }
        result.typescript_property_selector_start =
            (result.occurrences.len() != start).then_some(start);
        keyof
    }

    pub(super) fn resolve_keyof_members(
        &self,
        requests: KeyofRequests<'_, 'source>,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        let start = result.occurrences.len();
        let mut cache = KeyofMemberCache::new();
        self.record_keyof_members(requests.parameters, &mut cache, work, result);
        let conditional = result.occurrences.len();
        self.record_keyof_members(requests.conditional, &mut cache, work, result);
        result.typescript_keyof_member_start = (conditional != start).then_some(start);
        result.typescript_conditional_keyof_member_start =
            (result.occurrences.len() != conditional).then_some(conditional);
    }

    fn record_keyof_members(
        &self,
        requests: Vec<KeyofRequest<'_, 'source>>,
        cache: &mut KeyofMemberCache<'source>,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        'requests: for (using, selector) in requests {
            let CheckedPropertySelector::KeyofIndexedAccess {
                owner,
                bound,
                key,
                parameter,
                proof,
                member,
                selector,
            } = selector
            else {
                continue;
            };
            if !work.charge() {
                break;
            }
            if !self.type_parameter_matches(using, key, parameter, work)
                || matches!(proof, CheckedKeyofBound::TrueBranch { key: test }
                    if !self.type_parameter_matches(using, test, parameter, work))
            {
                continue;
            }
            let Some(owner) = self.keyof_owner(using, owner, bound, work) else {
                continue;
            };
            let Some(name) = using.text(member) else {
                continue;
            };
            let Some(anchor) = owner.symbol.name_range else {
                continue;
            };
            let cache_key = (owner.file.path.as_str(), anchor, name);
            let targets = cache
                .entry(cache_key)
                .or_insert_with(|| self.closed_keyof_targets(owner, anchor, name, work));
            let Some(targets) = targets else {
                continue;
            };
            for &target in targets.iter() {
                if !work.charge() {
                    break 'requests;
                }
                let occurrence = self
                    .occurrence(using, selector, selector, ReferenceRole::Unknown, target)
                    .filter(|occurrence| matches!(occurrence.target, NormalizedTarget::Symbol(_)));
                result.record(occurrence);
            }
        }
    }

    fn type_parameter_matches(
        &self,
        using: &File<'source>,
        key: ByteRange,
        parameter: ByteRange,
        work: &mut Work,
    ) -> bool {
        using.text(key).is_some_and(|name| {
            js_binding(
                using,
                &self.javascript,
                name,
                key.start,
                ReferenceRole::Type,
                work,
            )
            .is_some_and(|local| local.kind == StaticBindingKind::Shadow && local.name == parameter)
        })
    }

    fn keyof_owner(
        &self,
        using: &File<'source>,
        owner: ByteRange,
        bound: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let owner = self.javascript_declared_type(using, owner, work)?;
        let bound = self.javascript_declared_type(using, bound, work)?;
        (owner.file.path == bound.file.path && std::ptr::eq(owner.symbol, bound.symbol))
            .then_some(owner)
    }

    fn closed_keyof_targets(
        &self,
        owner: Target<'source>,
        anchor: ByteRange,
        selector: &str,
        work: &mut Work,
    ) -> Option<Vec<Target<'source>>> {
        let file = self.files.get(owner.file.path.as_str())?;
        if file
            .by_qualified_name
            .get(owner.symbol.qualified_name.as_str())?
            .len()
            != 1
        {
            return None;
        }
        let document = file.document;
        if !document.facts.declaration_syntax_complete()
            || !is_declaration_file(document.path.as_str())
        {
            return None;
        }
        let limits = document.facts.syntax_limits()?;
        let source = SyntaxSource {
            path: document.path,
            text: document.source,
        };
        let ranges = TypeScriptDialect::TypeScript
            .provider()
            .closed_keyof_members(source, limits, anchor, selector)
            .ok()??;
        ranges
            .into_iter()
            .map(|range| {
                if !work.charge() {
                    return None;
                }
                let target = file.target_at(range)?;
                target.admitted_path()?;
                Some(target)
            })
            .collect()
    }

    fn checked_property_selector(
        &self,
        file: &File<'source>,
        selector: CheckedPropertySelector,
        work: &mut Work,
    ) -> Option<StaticOccurrence> {
        let (written, target) = match selector {
            CheckedPropertySelector::IndexedAccess { owner, selector } => {
                let name = file.text(selector)?;
                if name.trim() != name || name.contains('.') {
                    return None;
                }
                let owner = self.javascript_declared_type(file, owner, work)?;
                let member = self.javascript_class_member(
                    owner,
                    name,
                    MemberMode::Instance,
                    ReferenceRole::Read,
                    work,
                )?;
                (selector, member)
            }
            CheckedPropertySelector::DestructuredParameter { selector, property } => {
                (selector, file.target_at(property)?)
            }
            CheckedPropertySelector::KeyofIndexedAccess { .. } => return None,
        };
        let occurrence = self.occurrence(file, written, written, ReferenceRole::Unknown, target)?;
        matches!(occurrence.target, NormalizedTarget::Symbol(_)).then_some(occurrence)
    }
}
