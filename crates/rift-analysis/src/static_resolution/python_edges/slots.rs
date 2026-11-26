//! Runtime member writes invalidate a selected slot through its resolved owner.

use rift_protocol::read::SymbolKind;

use super::super::{File, Index, Target, Work};
use super::Value;

impl<'source> Index<'source> {
    fn python_written_receiver(
        &self,
        path: &str,
        range: rift_syntax::ByteRange,
        work: &mut Work,
    ) -> Option<Value<'source>> {
        let file = self.files.get(path)?;
        work.python_lookup(|work| {
            let value = self.python_name(file, range, work)?;
            self.python_receiver(value, work)
        })
    }

    pub(super) fn python_module_slot_available(
        &self,
        file: &File<'source>,
        name: &str,
        work: &mut Work,
    ) -> bool {
        self.python_writes
            .get(name)
            .into_iter()
            .flatten()
            .all(|&(path, range)| {
                if !work.charge() {
                    return false;
                }
                // A writer static lookup cannot resolve is not attributed to this module.
                !matches!(
                    self.python_written_receiver(path, range, work),
                    Some(Value::Symbol(target))
                        if target.symbol.kinds.contains(&SymbolKind::Module)
                            && self.edge_python.same_module(
                                target.file.path.as_str(), file.document.path.as_str(),
                            )
                )
            })
    }

    pub(super) fn python_member_slot_available(
        &self,
        class: Target<'source>,
        instance: bool,
        name: &str,
        declaration: Target<'source>,
        work: &mut Work,
    ) -> bool {
        if !declaration.symbol.kinds.contains(&SymbolKind::Callable)
            && !declaration
                .symbol
                .kinds
                .contains(&SymbolKind::Constructible)
        {
            return true;
        }
        // An instance write goes through a property's setter and leaves its slot in place.
        let property = declaration.symbol.kinds.contains(&SymbolKind::Property);
        self.python_writes
            .get(name)
            .into_iter()
            .flatten()
            .all(|&(path, range)| {
                if !work.charge() {
                    return false;
                }
                let (written, instance_write) = match self
                    .python_written_receiver(path, range, work)
                {
                    Some(Value::Symbol(target))
                        if target.symbol.kinds.contains(&SymbolKind::Module) =>
                    {
                        return true;
                    }
                    Some(Value::Symbol(target)) => (target, false),
                    Some(Value::Instance(target)) if instance && !property => (target, true),
                    // A writer static lookup cannot resolve is not attributed to this class.
                    Some(Value::Instance(_) | Value::Member { .. } | Value::Module(_)) | None => {
                        return true;
                    }
                };
                self.python_slot_relation(class, written, declaration, instance_write, work)
                    == Some(false)
            })
    }
}
