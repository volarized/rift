use std::collections::{BTreeMap, BTreeSet};

use rift_core::SymbolId;
use rift_protocol::read::SymbolKind;
use rift_syntax::{SyntaxSource, registry};

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
    // Retained facts release syntax elements. Validate only malformed captured files,
    // under the same bounds, including declarations with zero-width missing tokens.
    let ranges = file
        .syntax
        .symbols()
        .iter()
        .map(|symbol| symbol.item_range)
        .collect::<Vec<_>>();
    let invalid = registry::provider_for_language(file.syntax.language()).and_then(|provider| {
        provider
            .invalid_declarations(
                SyntaxSource {
                    path: file.path,
                    text: file.source,
                },
                input.syntax(),
                &ranges,
            )
            .ok()
    });
    let Some(invalid) = invalid else {
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
