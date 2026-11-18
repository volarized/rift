use std::collections::{BTreeMap, BTreeSet};

use rift_core::SymbolId;
use rift_syntax::{ByteRange, SyntaxLimits, SyntaxSource, registry};

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
        .filter(|symbol| !invalid.contains(&symbol.item_range))
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
    let ranges = file
        .syntax
        .symbols()
        .iter()
        .map(|symbol| symbol.item_range)
        .collect::<Vec<_>>();
    registry::provider_for_language(file.syntax.language()).and_then(|provider| {
        provider
            .invalid_declarations(
                SyntaxSource {
                    path: file.path,
                    text: file.source,
                },
                limits,
                &ranges,
            )
            .ok()
    })
}
