//! Class objects held by unchanged, bare-name Python module aliases.

use std::collections::BTreeMap;

use rift_core::LoopBudget;
use rift_protocol::read::SymbolKind;
use rift_syntax::{ByteRange, StaticBinding, StaticBindingKind};

use super::super::{FILE_WORK_FLOOR, File, Index, Target, Work};
use super::Value;

pub(in crate::static_resolution) type ModuleAliases<'source> =
    Vec<(&'source str, &'source StaticBinding)>;

type Alias<'source> = (&'source str, ByteRange);
type Links<'source> = BTreeMap<Alias<'source>, Target<'source>>;

#[derive(Default)]
pub(in crate::static_resolution) struct ClassAliases<'source>(Links<'source>);

impl<'source> ClassAliases<'source> {
    pub(super) fn target(&self, alias: Target<'source>) -> Option<Target<'source>> {
        self.0.get(&key(alias)?).copied()
    }

    /// Follow each link once. An already removed link with no known class is unresolved,
    /// including links already visited on the current path.
    fn compress(mut links: Links<'source>, work: &mut Work) -> Self {
        let mut classes = Self::default();
        while let Some((alias, mut target)) = links.pop_first() {
            let mut path = vec![alias];
            let class = loop {
                if !work.charge() {
                    return classes;
                }
                if target.symbol.kinds.contains(&SymbolKind::Constructible) {
                    break Some(target);
                }
                if target.symbol.kinds.contains(&SymbolKind::Callable)
                    || target.symbol.kinds.contains(&SymbolKind::Module)
                {
                    break None;
                }
                if let Some(class) = classes.target(target) {
                    break Some(class);
                }
                let Some(alias) = key(target) else { break None };
                let Some(next) = links.remove(&alias) else {
                    break None;
                };
                path.push(alias);
                target = next;
            };
            if let Some(class) = class {
                classes
                    .0
                    .extend(path.into_iter().map(|alias| (alias, class)));
            }
        }
        classes
    }
}

impl<'source> Index<'source> {
    pub(in crate::static_resolution) fn python_class_alias_targets(
        &self,
        candidates: ModuleAliases<'source>,
        work: &mut Work,
    ) -> ClassAliases<'source> {
        let remaining = work.budget.remaining();
        // Preparation spends at most one file's minimum share and leaves at least one
        // minimum share available for resolution.
        let limit = remaining
            .saturating_sub(FILE_WORK_FLOOR)
            .min(FILE_WORK_FLOOR);
        if candidates.len() < 2 || limit == 0 {
            return ClassAliases::default();
        }
        let mut preparation = Work::new(limit);
        let mut links = Links::new();
        for (path, binding) in candidates {
            if !preparation.charge() {
                break;
            }
            let Some(file) = self.files.get(path) else {
                continue;
            };
            if let Some(target) = self.python_one_alias_link(file, binding, &mut preparation) {
                links.insert((path, binding.name), target);
            }
        }
        let classes = ClassAliases::compress(links, &mut preparation);
        let used = limit - preparation.budget.remaining();
        work.budget = LoopBudget::new(remaining - used);
        classes
    }

    fn python_one_alias_link(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let StaticBindingKind::PythonAlias { value } = binding.kind else {
            return None;
        };
        if self.python_unchanged_module_binding(file, file.text(binding.name)?)? != binding {
            return None;
        }
        let name = file.text(value)?;
        let source = self.python_unchanged_module_binding(file, name)?;
        match source.kind {
            StaticBindingKind::Local | StaticBindingKind::PythonAlias { .. } => {}
            StaticBindingKind::PythonImport {
                export: Some(_), ..
            } => {
                let (path, name) = self
                    .python_imports
                    .0
                    .get(&(file.document.path.as_str(), name))?
                    .as_ref()?;
                self.python_unchanged_module_binding(self.files.get(path)?, name)?;
            }
            _ => return None,
        }
        match work.python_lookup(|work| self.python_name(file, value, work))? {
            Value::Symbol(target) => Some(target),
            _ => None,
        }
    }

    pub(super) fn python_unchanged_module_binding(
        &self,
        file: &File<'source>,
        name: &str,
    ) -> Option<&'source StaticBinding> {
        let [binding] = file.by_name.get(name)?.as_slice() else {
            return None;
        };
        (file.python_module_binding(binding)
            && !file.python_globals.contains_key(name)
            && !self.python_writes.contains_key(name))
        .then_some(*binding)
    }
}

fn key(target: Target<'_>) -> Option<Alias<'_>> {
    Some((target.file.path.as_str(), target.symbol.name_range?))
}
