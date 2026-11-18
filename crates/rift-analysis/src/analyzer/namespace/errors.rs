use std::collections::{BTreeMap, BTreeSet};

use rift_core::SymbolId;
use rift_syntax::SyntaxSource;

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
    // The facts keep the answer, so a held file is parsed again once, not on every build.
    let Some(invalid) = file.syntax.invalid_declarations(
        SyntaxSource {
            path: file.path,
            text: file.source,
        },
        input.syntax(),
    ) else {
        return BTreeMap::new();
    };
    let valid = file
        .syntax
        .symbols()
        .iter()
        .filter(|symbol| !invalid.contains(&symbol.item_range))
        .map(|symbol| symbol.qualified_name.as_str())
        .collect::<BTreeSet<_>>();
    anchors.retain(|name, _| valid.contains(name.as_str()));
    anchors
}
