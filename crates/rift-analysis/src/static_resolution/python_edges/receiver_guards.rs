//! Python receiver types established by guards over conditionally assigned values.

use rift_syntax::{ByteRange, StaticBinding, StaticBindingKind};

use super::super::{File, Index, Work};
use super::{ReceiverJoin, Scope};

impl<'source> Index<'source> {
    pub(super) fn python_guarded_receiver(
        &self,
        file: &File<'source>,
        name: &str,
        scope: Scope,
        position: u64,
        candidates: impl Iterator<Item = &'source StaticBinding>,
        work: &mut Work,
    ) -> Option<&'source StaticBinding> {
        let guarded = file.python_guarded_use(name, position, work)?;
        if (!scope.module && file.class_scopes.contains(&scope.range))
            || (scope.module && !self.python_module_slot_available(file, name, work))
        {
            return None;
        }
        for &binding in file.by_name.get(name)? {
            if !work.charge()
                || (scope.module && binding.kind == StaticBindingKind::PythonGlobal)
                || (Scope::of(file, binding) == scope
                    && binding.name.start > guarded
                    && binding.name.start < position)
            {
                return None;
            }
        }
        self.python_agreed_receiver(file, candidates, ReceiverJoin::NonNone, work)
    }
}

impl File<'_> {
    fn python_guarded_use(&self, name: &str, position: u64, work: &mut Work) -> Option<u64> {
        let callable = self
            .enclosing_callable(ByteRange {
                start: position,
                end: position.checked_add(1)?,
            })
            .map(|owner| owner.symbol.item_range);
        for &(checked, consequence) in self.python_non_none.get(name)?.iter().rev() {
            if !work.charge() {
                return None;
            }
            if !consequence.contains(position) {
                continue;
            }
            let owner = self
                .enclosing_callable(checked)
                .map(|owner| owner.symbol.item_range);
            if owner != callable {
                continue;
            }
            for expression in self
                .facts
                .expression_scopes()
                .iter()
                .chain(self.facts.loops())
            {
                if !work.charge()
                    || (expression.contains(position) && !expression.encloses(checked))
                {
                    return None;
                }
            }
            return Some(checked.start);
        }
        None
    }
}
