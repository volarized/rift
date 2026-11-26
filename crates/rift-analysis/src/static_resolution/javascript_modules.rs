//! Reads of checked ECMAScript package modules.

use rift_core::ReferenceRole;
use rift_syntax::{ByteRange, StaticBindingKind};

use super::javascript::builtin_commonjs_name;
use super::{Binding, File, Index, StaticResolution, Work};

impl<'source> Index<'source> {
    pub(super) fn javascript_module_qualifiers(
        &self,
        file: &File<'source>,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        // Each lexical access is already captured at its authored identifier span.
        // A receiver of a named or computed member can also be a namespace binding.
        for reference in file.facts.references() {
            if !work.charge() {
                return;
            }
            let range = reference.target;
            if reference.role == ReferenceRole::Read
                && file.text(range) == Some("exports")
                && builtin_commonjs_name(file, &self.javascript, "exports", range.start, work)
            {
                self.javascript_module_read(file, range, file, work, result);
            }
        }
        let mut receivers = std::collections::BTreeSet::new();
        for member in file.facts.members() {
            if !work.charge() {
                return;
            }
            let range = member.receiver;
            if !receivers.insert(range) {
                continue;
            }
            if let Some(imported) = self.javascript_namespace_receiver(file, range, work) {
                self.javascript_module_read(file, range, imported, work, result);
            }
        }
    }

    fn javascript_namespace_receiver(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<&File<'source>> {
        let name = file.text(range)?;
        let Binding::Found(binding) = file.binding(name, range.start, |_| true, work) else {
            return None;
        };
        if binding.available_from > range.start
            || file.inactive(range, work)
            || file.javascript_binding_reassigned(binding)
        {
            return None;
        }
        let (module, require) = match binding.kind {
            StaticBindingKind::JavaScriptEdgeNamespace { module } => (module, false),
            StaticBindingKind::JavaScriptRequire {
                module,
                export: None,
                require,
            } if self.builtin_require(file, require.start, work) => (module, true),
            _ => return None,
        };
        self.javascript_specifier_file(file, module, require, work)
    }

    fn javascript_module_read(
        &self,
        file: &File<'source>,
        range: ByteRange,
        imported: &File<'source>,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        if let Some(mut occurrence) =
            self.javascript_module_occurrence(file, range, imported, ReferenceRole::Read, work)
        {
            occurrence.edge_from = self.javascript_call_from(file, range, work);
            if occurrence.edge_from.is_some() {
                result.record(Some(occurrence));
            }
        }
    }
}
