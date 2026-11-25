//! Named field labels in Rust struct expressions and patterns.

use rift_core::ReferenceRole;
use rift_protocol::read::{EdgeKind, SymbolKind};
use rift_syntax::{StaticCallee, StaticRustFieldSite};

use super::{Namespace, Namespaces, path_segments};
use crate::static_resolution::{File, Index, StaticResolution, Target, Work};

impl<'source> Index<'source> {
    pub(in crate::static_resolution) fn rust_field_sites(
        &self,
        file: &File<'source>,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        for site in file.facts.rust_field_sites() {
            if !work.charge() {
                return;
            }
            if file.conditional(site.field, work) || file.blocked(site.field.start, work) {
                continue;
            }
            let (role, kind) = if site.literal.is_some() {
                (ReferenceRole::Write, EdgeKind::Writes)
            } else {
                (ReferenceRole::Read, EdgeKind::Reads)
            };
            let occurrence = self
                .rust
                .field_site_target(file, *site, work)
                .and_then(|target| self.occurrence(file, site.field, site.field, role, target));
            result.record_kind(occurrence, kind);
        }
    }
}

impl<'source> Namespaces<'source> {
    fn field_site_target(
        &self,
        file: &File<'source>,
        site: StaticRustFieldSite,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let position = site.owner.start;
        let parts = path_segments(file.text(site.owner)?, work)?;
        let head = *parts.first()?;
        if parts.len() > 1
            && !matches!(head, "crate" | "self" | "super" | "Self")
            && Self::local_named(file, position, head, work, |target| {
                target.symbol.kinds.contains(&SymbolKind::Type)
                    || target.symbol.kinds.contains(&SymbolKind::Module)
            })
            .is_some()
        {
            return None;
        }
        let owner = if let Some(literal) = site.literal {
            let call = self
                .initializers
                .get(&(file.document.path.as_str(), literal))?;
            let call = (*call)?;
            if call.callee != StaticCallee::Qualified || call.callee_range != site.owner {
                return None;
            }
            self.call(file, call, work)
                .filter(|owner| matches!(owner.symbol.kind, "struct" | "union_item" | "variant"))?
        } else {
            self.pattern_field_owner(file, site, &parts, work)?
        };
        let name = owner.symbol.name_range?;
        // The stored local declaration is usable only in its actual textual block.
        // A same-name declaration from a sibling block must not supply its fields.
        if owner.file.facts.static_facts().is_some_and(|facts| {
            facts.rust_local_types().iter().any(|(item, scope)| {
                !work.charge()
                    || (item.encloses(name)
                        && (owner.file.path != file.document.path || !scope.contains(position)))
            })
        }) {
            return None;
        }
        let mut path = owner.admitted_path()?;
        path.push(file.text(site.field)?.to_owned());
        self.fields.get(&path).copied().flatten().filter(|field| {
            field.file.path == owner.file.path
                && field.symbol.container.as_deref() == Some(owner.symbol.qualified_name.as_str())
                && self.available(*field)
                && (owner.symbol.kind == "variant"
                    || self.declaration_visible(file, position, *field, work))
        })
    }

    fn pattern_field_owner(
        &self,
        file: &File<'source>,
        site: StaticRustFieldSite,
        parts: &[&str],
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if let Some(ty) = self.edge_type_target(file, site.owner, work)
            && matches!(ty.symbol.kind, "struct" | "union_item")
        {
            return Some(ty);
        }
        let position = site.owner.start;
        let path = self.resolved_path(file, position, parts, work)?;
        let (name, parent) = path.split_last()?;
        let enumeration = self
            .lookup(file, position, parent, Namespace::Type, work)
            .filter(|target| target.symbol.kind == "enum")?;
        let mut variant = enumeration.admitted_path()?;
        variant.push(name.clone());
        self.values
            .get(&variant)
            .copied()
            .flatten()
            .filter(|target| target.symbol.kind == "variant" && self.available(*target))
    }
}
