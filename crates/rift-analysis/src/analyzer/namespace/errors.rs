use std::collections::{BTreeMap, BTreeSet};

use rift_core::SymbolId;
use rift_protocol::read::SymbolKind;
use rift_syntax::{ByteRange, SyntaxLimits, SyntaxSource};

use super::SelectedFile;
use crate::NamespaceInput;

pub(super) fn retain_valid_declarations(
    input: &NamespaceInput<'_>,
    file: &SelectedFile<'_>,
    mut anchors: BTreeMap<String, SymbolId>,
) -> BTreeMap<String, SymbolId> {
    if !file.syntax.has_errors() {
        return anchors;
    }
    let Some(invalid) = invalid_declarations(file, input.syntax()) else {
        return BTreeMap::new();
    };
    let valid = file
        .syntax
        .symbols()
        .iter()
        .filter(|symbol| {
            // File modules have no source name or header. Their range can equal
            // a broken declaration's range when the file has no trailing newline.
            let file_module =
                symbol.name_range.is_none() && symbol.kinds.contains(&SymbolKind::Module);
            file_module || !invalid.contains(&symbol.item_range)
        })
        .map(|symbol| symbol.qualified_name.as_str())
        .collect::<BTreeSet<_>>();
    anchors.retain(|name, _| valid.contains(name.as_str()));
    anchors
}

pub(super) fn invalid_declarations(
    file: &SelectedFile<'_>,
    limits: SyntaxLimits,
) -> Option<BTreeSet<ByteRange>> {
    if !file.syntax.has_errors() {
        return Some(BTreeSet::new());
    }
    // Retained facts release syntax elements. Validate only malformed captured files,
    // under the same bounds, including declarations with zero-width missing tokens.
    // The facts keep the answer, so a held file is parsed again once, not on every build.
    file.syntax.invalid_declarations(
        SyntaxSource {
            path: file.path,
            text: file.source,
        },
        limits,
    )
}
