//! Python containment, member accesses, overrides, and annotation uses.

use rift_core::ReferenceRole;
use rift_protocol::read::{EdgeKind, SymbolKind};
use rift_syntax::ByteRange;

use super::super::{File, Index, StaticOccurrence, StaticResolution, Target, Work};
use super::Value;

impl<'source> Index<'source> {
    pub(in crate::static_resolution) fn python_references(
        &self,
        file: &File<'source>,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        for reference in file.facts.references() {
            if !work.charge() {
                return;
            }
            let target = self
                .edge_python_name(file, reference.target, work)
                .or_else(|| {
                    (reference.role == ReferenceRole::Write)
                        .then(|| self.python_written_module_name(file, reference.target, work))
                        .flatten()
                });
            let declared = target.is_some();
            let occurrence = target.and_then(|target| {
                let mut occurrence = self.occurrence(
                    file,
                    reference.target,
                    reference.target,
                    reference.role,
                    target,
                )?;
                // An annotation belongs to its declaration. Accesses belong to the enclosing callable.
                if reference.role != ReferenceRole::Type
                    && let Some(caller) = file.enclosing_callable(reference.target)
                {
                    occurrence.edge_from = self.python_identity(file, reference.target, caller);
                }
                Some(occurrence)
            });
            self.record_reference(file, reference, (occurrence, declared), work, result);
        }
        for symbol in file.document.facts.symbols() {
            if !work.charge() {
                return;
            }
            if symbol.qualified_name == "." {
                continue;
            }
            let Some(range) = symbol
                .name_range
                .or(Some(symbol.item_range))
                .filter(|range| range.start < range.end)
            else {
                continue;
            };
            let target = Target {
                file: file.document,
                symbol,
            };
            let parent = file.qualified_target(symbol.container.as_deref().unwrap_or("."));
            if let Some(parent) = parent {
                result.record(self.python_edge(file, range, EdgeKind::Contains, parent, target));
                if symbol.kinds.contains(&SymbolKind::Callable)
                    && parent.symbol.kinds.contains(&SymbolKind::Constructible)
                    && let Some(base) = self.python_base_member(parent, &symbol.name, work)
                    && base.symbol.kinds.contains(&SymbolKind::Callable)
                {
                    result.record(self.python_edge(file, range, EdgeKind::Overrides, target, base));
                }
            }
        }
    }

    /// The declaration a write to `module.name` names: the module's own declaration of `name`,
    /// even when other writes replace its value at runtime.
    fn python_written_module_name(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let (receiver, member) = file.text(range)?.rsplit_once('.')?;
        let receiver = ByteRange {
            start: range.start,
            end: range.start + u64::try_from(receiver.len()).ok()?,
        };
        let Value::Module(name) =
            work.python_lookup(|work| self.python_name(file, receiver, work))?
        else {
            return None;
        };
        let module = self.files.get(self.edge_python.path(&name)?)?;
        let root = ByteRange {
            start: 0,
            end: module.document.source.len() as u64,
        };
        let mut declaration: Option<Target<'source>> = None;
        for binding in module.by_name.get(member.trim())? {
            if !work.charge() {
                return None;
            }
            if binding.scope != root {
                continue;
            }
            let target = module.target_at(binding.name)?;
            if declaration
                .is_some_and(|known| known.symbol.qualified_name != target.symbol.qualified_name)
            {
                return None;
            }
            declaration.get_or_insert(target);
        }
        declaration
    }

    pub(in crate::static_resolution) fn python_identity(
        &self,
        file: &File<'source>,
        range: ByteRange,
        target: Target<'source>,
    ) -> Option<rift_core::SymbolId> {
        let occurrence = self.occurrence(file, range, range, ReferenceRole::Read, target)?;
        match occurrence.target {
            rift_provider::NormalizedTarget::Symbol(symbol) => Some(symbol),
            rift_provider::NormalizedTarget::Contribution(_) => None,
        }
    }

    fn python_edge(
        &self,
        file: &File<'source>,
        range: ByteRange,
        kind: EdgeKind,
        from: Target<'source>,
        to: Target<'source>,
    ) -> Option<StaticOccurrence> {
        let mut occurrence = self.occurrence(file, range, range, ReferenceRole::Read, to)?;
        occurrence.edge_kind = Some(kind);
        occurrence.edge_from = Some(self.python_identity(file, range, from)?);
        Some(occurrence)
    }
}
