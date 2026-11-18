//! Python containment, member accesses, overrides, and annotation uses.

use rift_core::ReferenceRole;
use rift_protocol::read::{EdgeKind, SymbolKind};
use rift_syntax::ByteRange;

use super::super::{File, Index, StaticOccurrence, StaticResolution, Target, Work};

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
            let occurrence =
                self.edge_python_name(file, reference.range, work)
                    .and_then(|target| {
                        let mut occurrence = self.occurrence(
                            file,
                            reference.range,
                            reference.range,
                            reference.role,
                            target,
                        )?;
                        // An annotation belongs to its declaration. Accesses belong to the enclosing callable.
                        if reference.role != ReferenceRole::Type
                            && let Some(caller) = file.enclosing_callable(reference.range)
                        {
                            occurrence.edge_from =
                                self.python_identity(file, reference.range, caller);
                        }
                        Some(occurrence)
                    });
            result.record(occurrence);
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

    fn python_identity(
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
