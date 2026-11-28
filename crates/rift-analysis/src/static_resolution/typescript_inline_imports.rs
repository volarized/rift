//! Type references at checked same-package inline import qualifiers.

use rift_core::ReferenceRole;
use rift_protocol::read::SymbolKind;
use rift_provider::NormalizedTarget;
use rift_syntax::{
    ByteRange, CheckedInlineTypeImport, SyntaxSource, TypeScriptDialect, TypeScriptSyntaxProvider,
};

use super::javascript::ModuleMode;
use super::javascript_exports::{Exports, ResolvedExport, is_declaration_file};
use super::{File, Index, StaticOccurrence, StaticResolution, Work};

impl<'source> Index<'source> {
    pub(super) fn resolve_typescript_inline_import_types(
        &self,
        work: &mut Work,
        result: &mut StaticResolution,
    ) -> Vec<StaticOccurrence> {
        let start = result.occurrences.len();
        let mut modules = Vec::new();
        let provider = TypeScriptSyntaxProvider::new(TypeScriptDialect::TypeScript);
        for file in self.files.values() {
            let document = file.document;
            let facts = document.facts;
            if facts.language().name != "typescript"
                || !is_declaration_file(document.path.as_str())
                || facts.left_out_declaration_count() != 0
                || !document.source.contains("import")
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
            let Ok(imports) = provider.checked_inline_import_types(source, limits) else {
                continue;
            };
            for import in imports {
                if !work.charge() {
                    break;
                }
                if let Some((reference, module)) =
                    self.typescript_inline_import_type(file, import, work)
                {
                    result.record(Some(reference));
                    modules.extend(module);
                }
            }
        }
        result.typescript_inline_import_type_start =
            (result.occurrences.len() != start).then_some(start);
        modules
    }

    fn typescript_inline_import_type(
        &self,
        file: &File<'source>,
        import: CheckedInlineTypeImport,
        work: &mut Work,
    ) -> Option<(StaticOccurrence, Option<StaticOccurrence>)> {
        let owner = file.target_at(import.owner_name)?;
        let [checked_owner] = file
            .by_qualified_name
            .get(owner.symbol.qualified_name.as_str())?
            .as_slice()
        else {
            return None;
        };
        if !std::ptr::eq(*checked_owner, owner.symbol)
            || owner.symbol.syntax_element_kind != Some(import.owner_kind)
        {
            return None;
        }
        let from = self.javascript_identity(owner)?;
        let exports = Exports {
            files: &self.files,
            modes: &self.javascript,
            names: None,
        };
        let imported = exports.imported(file, file.text(import.module)?, work)?;
        if !is_declaration_file(imported.document.path.as_str()) {
            return None;
        }
        let ResolvedExport::Declaration {
            target,
            member: None,
        } = exports.resolve_file(
            imported,
            file.text(import.qualifier)?,
            ModuleMode::Esm,
            ReferenceRole::Type,
            work,
        )?
        else {
            return None;
        };
        if target.file.path != imported.document.path
            || !target.symbol.kinds.contains(&SymbolKind::Type)
        {
            return None;
        }
        let mut occurrence = self.occurrence(
            file,
            import.qualifier,
            import.qualifier,
            ReferenceRole::Type,
            target,
        )?;
        if !matches!(occurrence.target, NormalizedTarget::Symbol(_)) {
            return None;
        }
        occurrence.edge_from = Some(from);
        let module = self.typescript_inline_import_module(file, imported, import.module, work);
        Some((occurrence, module))
    }

    fn typescript_inline_import_module(
        &self,
        file: &File<'source>,
        imported: &File<'source>,
        module: ByteRange,
        work: &mut Work,
    ) -> Option<StaticOccurrence> {
        // The syntax check proved the quotes and the declaration resolver selected this file.
        imported.document.placement.logical_identity(".")?;
        let from = file.document.placement.logical_identity(".")?.clone();
        let quoted = ByteRange {
            start: module.start.checked_sub(1)?,
            end: module.end.checked_add(1)?,
        };
        let mut occurrence =
            self.javascript_module_occurrence(file, quoted, imported, ReferenceRole::Import, work)?;
        occurrence.edge_from = Some(from);
        Some(occurrence)
    }
}
