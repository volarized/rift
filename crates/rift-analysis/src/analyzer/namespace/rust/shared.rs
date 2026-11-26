//! Multiple logical module owners backed by the same authored Rust source file.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::SymbolId;
use rift_protocol::identity::SymbolOwner;
use rift_syntax::{ByteRange, RustModulePath};

use super::{ChildModulePath, file_anchors, module_identity, source_is_valid};
use crate::analyzer::namespace::SelectedFile;
use crate::{EnclosingDefinitions, NamespaceInput};

type RouteKey = (String, Vec<String>);
type Route = (String, ByteRange);
type Inventory<'a> = BTreeMap<&'a str, &'a SelectedFile<'a>>;

/// One logical module projection of a single checked physical source.
#[derive(Debug)]
pub(crate) struct SharedRustModule {
    module: SymbolId,
    provider_suffix: String,
    anchors: BTreeMap<String, SymbolId>,
}

impl SharedRustModule {
    pub(crate) fn placement(
        self,
        original: &rift_syntax::DocumentPlacement,
    ) -> rift_syntax::DocumentPlacement {
        let provider_path = format!("{}/{}", original.identity_path(), self.provider_suffix);
        rift_syntax::DocumentPlacement::new(
            original.origin().clone(),
            original.unit().clone(),
            provider_path,
        )
        .with_identity_anchors(self.anchors)
        .with_module(Some(self.module))
    }
}

/// Direct literal declarations are the only supported authority for projecting a unit twice.
#[derive(Default)]
pub(super) struct Routes {
    literals: BTreeMap<RouteKey, Option<Route>>,
    alternatives: BTreeMap<String, BTreeSet<Vec<String>>>,
}

impl Routes {
    pub(super) fn record(
        &mut self,
        file: &SelectedFile<'_>,
        module: &[String],
        ordinary: &[ChildModulePath],
    ) {
        let literal_items = file
            .syntax
            .symbols()
            .iter()
            .filter(|symbol| matches!(symbol.module_path, Some(RustModulePath::Literal { .. })))
            .map(|symbol| symbol.item_range)
            .collect::<BTreeSet<_>>();
        for (path, name, item) in ordinary {
            if name.len() != 1 || !literal_items.contains(item) {
                continue;
            }
            let logical = module.iter().chain(name).cloned().collect::<Vec<_>>();
            self.literals
                .entry((path.clone(), logical))
                .and_modify(|previous| *previous = None)
                .or_insert_with(|| Some((file.path.as_str().to_owned(), *item)));
        }
    }

    pub(super) fn record_shared(
        &mut self,
        path: &str,
        previous: &[String],
        next: &[String],
    ) -> bool {
        let former = (path.to_owned(), previous.to_vec());
        let latter = (path.to_owned(), next.to_vec());
        if self
            .literals
            .get(&former)
            .and_then(Option::as_ref)
            .is_none()
            || self
                .literals
                .get(&latter)
                .and_then(Option::as_ref)
                .is_none()
        {
            return false;
        }
        let alternatives = self.alternatives.entry(path.to_owned()).or_default();
        alternatives.insert(former.1);
        alternatives.insert(latter.1);
        true
    }

    pub(super) fn projections(
        self,
        input: &NamespaceInput<'_>,
        context: (
            &Inventory<'_>,
            &BTreeMap<&str, &str>,
            &BTreeMap<String, Vec<String>>,
        ),
        configurations: &super::configuration::Configurations<'_>,
        admitted: &BTreeMap<String, Option<bool>>,
    ) -> Option<BTreeMap<String, Vec<SharedRustModule>>> {
        if !self.alternatives.is_empty()
            && !matches!(input.owner(), SymbolOwner::Package { manager, .. } if manager == "cargo")
        {
            return None;
        }
        if self.alternatives.is_empty() {
            return Some(BTreeMap::new());
        }
        let (inventory, originals, assigned) = context;
        let mut scopes = BTreeMap::new();
        for (path, scope) in assigned {
            scopes
                .entry(scope.clone())
                .and_modify(|previous| *previous = None)
                .or_insert_with(|| Some(path.clone()));
        }
        let mut projections = BTreeMap::new();
        for (path, alternatives) in self.alternatives {
            if alternatives.len() != 2 {
                return None;
            }
            let file = inventory.get(path.as_str())?;
            if originals.get(path.as_str()).copied() != Some(file.source)
                || !leaf(file, &configurations.file(&path))
            {
                return None;
            }
            let mut placed = Vec::new();
            for scope in alternatives {
                let (parent, item) = self
                    .literals
                    .get(&(path.clone(), scope.clone()))?
                    .as_ref()?;
                if scopes
                    .entry(scope.clone())
                    .or_insert_with(|| Some(path.clone()))
                    .as_deref()
                    != Some(path.as_str())
                {
                    return None;
                }
                let parent_file = inventory.get(parent.as_str())?;
                if admitted.get(parent).copied().unwrap_or(Some(true)) != Some(true)
                    || configurations.file(parent).admitted(
                        parent_file.syntax,
                        parent_file.source,
                        *item,
                    ) != Some(true)
                {
                    return None;
                }
                let anchors = file_anchors(input, file.syntax, &scope, &BTreeSet::new());
                placed.push(SharedRustModule {
                    module: module_identity(input, &scope)?,
                    provider_suffix: scope.join("::"),
                    anchors: crate::analyzer::namespace::retain_valid_declarations(
                        input, file, anchors,
                    ),
                });
            }
            projections.insert(path, placed);
        }
        Some(projections)
    }
}

fn leaf(file: &SelectedFile<'_>, configuration: &super::configuration::Configuration<'_>) -> bool {
    if !source_is_valid(file.syntax, file.source)
        || file.syntax.has_errors()
        || file.syntax.left_out_declaration_count() != 0
        || file.syntax.static_facts().is_none()
        || file.syntax.symbols().iter().any(|symbol| {
            symbol.kind == "module"
                && configuration.admitted(file.syntax, file.source, symbol.item_range)
                    != Some(false)
        })
    {
        return false;
    }
    let bodies = EnclosingDefinitions::new(
        file.syntax
            .symbols()
            .iter()
            .filter(|symbol| symbol.kind == "function")
            .filter_map(|symbol| symbol.body_range)
            .map(|range| (range.start, range.end, ())),
    );
    file.syntax.macro_invocations().is_some_and(|invocations| {
        invocations
            .iter()
            .all(|(range, _, _)| bodies.resolve(range.start, range.end).is_some())
    })
}
