//! References from written TypeScript declaration property selectors.

use rift_core::ReferenceRole;
use rift_provider::NormalizedTarget;
use rift_syntax::{CheckedPropertySelector, SyntaxSource, TypeScriptDialect};

use super::javascript::MemberMode;
use super::{File, Index, StaticOccurrence, StaticResolution, Work, is_declaration_file};

impl<'source> Index<'source> {
    pub(super) fn resolve_typescript_property_selectors(
        &self,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        let start = result.occurrences.len();
        let provider = TypeScriptDialect::TypeScript.provider();
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
                if !work.charge() {
                    break 'files;
                }
                result.record(self.checked_property_selector(file, selector, work));
            }
        }
        result.typescript_property_selector_start =
            (result.occurrences.len() != start).then_some(start);
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
        };
        let occurrence = self.occurrence(file, written, written, ReferenceRole::Unknown, target)?;
        matches!(occurrence.target, NormalizedTarget::Symbol(_)).then_some(occurrence)
    }
}
