//! Physical source facets for out-of-line modules with an authored declaration.

use std::collections::BTreeMap;

use rift_core::SymbolId;
use rift_protocol::identity::SymbolOwner;
use rift_syntax::ByteRange;

use super::{Declaring, source_is_valid};
use crate::NamespaceInput;
use crate::analyzer::namespace::{PreparedNamespace, SelectedFile};

type ItemOwners<'a> = BTreeMap<ByteRange, &'a SymbolId>;

pub(super) fn retain(
    input: &NamespaceInput<'_>,
    inventory: &BTreeMap<&str, &SelectedFile<'_>>,
    declaring: &BTreeMap<String, Vec<(String, Declaring)>>,
    admitted: &BTreeMap<String, Option<bool>>,
    prepared: &mut PreparedNamespace,
) {
    if !matches!(input.owner(), SymbolOwner::Package { manager, .. } if manager == "cargo") {
        return;
    }
    let mut per_parent = BTreeMap::<&str, ItemOwners<'_>>::new();
    for (path, module) in &prepared.rust_modules {
        if admitted.get(path) != Some(&Some(true)) {
            continue;
        }
        let Some([(parent, Declaring::Item(item))]) = declaring.get(path).map(Vec::as_slice) else {
            continue;
        };
        let Some(file) = inventory.get(path.as_str()) else {
            continue;
        };
        if file.syntax.has_errors()
            || file.syntax.left_out_declaration_count() != 0
            || !source_is_valid(file.syntax, file.source)
        {
            continue;
        }
        let written = per_parent.entry(parent).or_insert_with(|| {
            inventory
                .get(parent.as_str())
                .zip(prepared.anchors.get(parent))
                .into_iter()
                .flat_map(|(file, anchors)| {
                    file.syntax
                        .symbols()
                        .iter()
                        .filter(|symbol| {
                            symbol.kind == "module"
                                && symbol.syntax_element_kind == Some("mod_item")
                                && symbol.body_range.is_none()
                        })
                        .filter_map(|symbol| {
                            Some((symbol.item_range, anchors.get(&symbol.qualified_name)?))
                        })
                })
                .collect()
        });
        if written.get(item).copied() == Some(module) {
            prepared.rust_module_sources.insert(path.clone());
        }
    }
}
