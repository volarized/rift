//! References from written string selectors in TypeScript indexed access types.

use rift_core::ReferenceRole;
use rift_provider::NormalizedTarget;
use rift_syntax::{CheckedIndexedAccess, SyntaxSource, TypeScriptDialect};

use super::javascript::MemberMode;
use super::{File, Index, StaticOccurrence, StaticResolution, Work, is_declaration_file};

impl<'source> Index<'source> {
    pub(super) fn resolve_typescript_indexed_accesses(
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
                || !document.source.contains('[')
                || !document.source.contains(['\'', '"'])
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
            let Ok(accesses) = provider.checked_indexed_accesses(source, limits) else {
                continue;
            };
            for access in accesses {
                if !work.charge() {
                    break 'files;
                }
                result.record(self.checked_indexed_access(file, access, work));
            }
        }
        result.typescript_indexed_access_start =
            (result.occurrences.len() != start).then_some(start);
    }

    fn checked_indexed_access(
        &self,
        file: &File<'source>,
        access: CheckedIndexedAccess,
        work: &mut Work,
    ) -> Option<StaticOccurrence> {
        let name = file.text(access.selector)?;
        if name.trim() != name || name.contains('.') {
            return None;
        }
        let owner = self.javascript_declared_type(file, access.owner, work)?;
        let target = self.javascript_class_member(
            owner,
            name,
            MemberMode::Instance,
            ReferenceRole::Read,
            work,
        )?;
        let occurrence = self.occurrence(
            file,
            access.selector,
            access.selector,
            ReferenceRole::Unknown,
            target,
        )?;
        matches!(occurrence.target, NormalizedTarget::Symbol(_)).then_some(occurrence)
    }
}
