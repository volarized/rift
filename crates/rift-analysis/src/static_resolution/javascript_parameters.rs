//! Physical ownership of ordinary ECMAScript value parameters.

use std::collections::BTreeMap;

use rift_core::ReferenceRole;
use rift_syntax::{ByteRange, StaticBindingKind, StaticFacts, StaticParameter, SyntaxFacts};

use super::javascript::js_binding;
use super::{
    EcmaScriptParameter, EcmaScriptParameters, File, Index, StaticOccurrence, StaticResolution,
    Target, Work,
};

#[derive(Default)]
pub(super) struct ParameterReads {
    pub(super) functions: Vec<ByteRange>,
    pub(super) methods: Vec<ByteRange>,
}

#[derive(Clone, Copy)]
pub(super) enum ScopedParameterReads {
    Callback,
    Overload,
    JavaScriptMethod,
    JavaScriptScopedFunction,
}

/// Exact parameter ranges, built only when syntax holds an ordinary candidate.
pub(super) fn parameter_sites<'source>(
    syntax: &SyntaxFacts,
    facts: &'source StaticFacts,
) -> BTreeMap<ByteRange, &'source StaticParameter> {
    if !syntax
        .symbols()
        .iter()
        .any(rift_syntax::SyntaxSymbol::is_ecmascript_parameter)
    {
        return BTreeMap::new();
    }
    facts
        .parameters()
        .iter()
        .map(|site| (site.name, site))
        .collect()
}

impl File<'_> {
    pub(super) fn record_parameter_read(&self, range: ByteRange, reads: &mut ParameterReads) {
        if (self.ecmascript_parameters.is_empty() && self.typescript_method_parameters.is_empty())
            || self.members.contains_key(&range)
        {
            return;
        }
        let Some(name) = self.text(range) else {
            return;
        };
        if self.ecmascript_parameters.contains_key(name) {
            reads.functions.push(range);
        }
        if self.typescript_method_parameters.contains_key(name) {
            reads.methods.push(range);
        }
    }
}

impl<'source> Index<'source> {
    pub(super) fn ecmascript_scoped_parameter_reads(
        &self,
        file: &File<'source>,
        class: ScopedParameterReads,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        let (parameters, pending) = match class {
            ScopedParameterReads::Callback => (
                &file.javascript_callback_parameters,
                &mut result.deferred_callback_parameters,
            ),
            ScopedParameterReads::Overload => (
                &file.typescript_overload_parameters,
                &mut result.deferred_overload_parameters,
            ),
            ScopedParameterReads::JavaScriptMethod => (
                &file.javascript_method_parameters,
                &mut result.deferred_javascript_method_parameters,
            ),
            ScopedParameterReads::JavaScriptScopedFunction => (
                &file.scoped_javascript_parameters,
                &mut result.deferred_scoped_javascript_parameters,
            ),
        };
        for reference in file.facts.references() {
            if !work.charge() {
                return;
            }
            if reference.role == ReferenceRole::Read
                && !file.members.contains_key(&reference.target)
                && let Some(occurrence) =
                    self.ecmascript_scoped_parameter_read(file, reference.target, parameters, work)
            {
                result.unresolved_sites = result.unresolved_sites.saturating_sub(1);
                pending.push(occurrence);
            }
        }
    }

    fn ecmascript_scoped_parameter_read(
        &self,
        file: &File<'source>,
        range: ByteRange,
        parameters: &BTreeMap<ByteRange, EcmaScriptParameter<'source>>,
        work: &mut Work,
    ) -> Option<StaticOccurrence> {
        let name = file.text(range)?;
        let binding = js_binding(
            file,
            &self.javascript,
            name,
            range.start,
            ReferenceRole::Read,
            work,
        )?;
        let parameter = parameters.get(&binding.name)?;
        if binding.kind != StaticBindingKind::Shadow || binding.scope != parameter.site.function {
            return None;
        }
        if parameter.symbol.is_ecmascript_overload_parameter()
            || parameter.symbol.is_javascript_method_parameter()
            || parameter.symbol.is_javascript_scoped_function_parameter()
        {
            let owner = file.target_at(parameter.site.owner?)?;
            if owner.symbol.item_range != binding.scope || !owner.symbol.body_range?.encloses(range)
            {
                return None;
            }
        }
        let target = Target {
            file: file.document,
            symbol: parameter.symbol,
        };
        let mut occurrence = self.occurrence(file, range, range, ReferenceRole::Read, target)?;
        occurrence.edge_from = Some(self.javascript_call_from(file, range, work)?);
        Some(occurrence)
    }

    /// Adds reads after previously collected facts have spent their reserved work.
    pub(super) fn ecmascript_parameter_reads(
        &self,
        file: &File<'source>,
        sites: Vec<ByteRange>,
        parameters: &EcmaScriptParameters<'source>,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        for range in sites {
            if !work.charge() {
                return;
            }
            if let Some(occurrence) = self.ecmascript_parameter_read(file, range, parameters, work)
            {
                result.unresolved_sites = result.unresolved_sites.saturating_sub(1);
                result.record(Some(occurrence));
            }
        }
    }

    fn ecmascript_parameter_read(
        &self,
        file: &File<'source>,
        range: ByteRange,
        parameters: &EcmaScriptParameters<'source>,
        work: &mut Work,
    ) -> Option<StaticOccurrence> {
        let name = file.text(range)?;
        let binding = js_binding(
            file,
            &self.javascript,
            name,
            range.start,
            ReferenceRole::Read,
            work,
        )?;
        if binding.kind != StaticBindingKind::Shadow {
            return None;
        }
        let parameter = parameters.get(name)?.get(&binding.name)?;
        let site = parameter.site;
        let owner = file.target_at(site.owner?)?;
        let scopes = self.javascript.scopes.get(file.document.path.as_str())?;
        if site.function != binding.scope
            || owner.symbol.item_range != site.function
            || parameter.symbol.container.as_deref() != Some(owner.symbol.qualified_name.as_str())
            || !owner.symbol.body_range?.encloses(range)
            || scopes.callables.resolve(range.start, range.end)?.item_range != site.function
            || scopes
                .function_values
                .resolve(range.start, range.end)
                .is_some_and(|nested| site.function.encloses(*nested))
            || scopes
                .classes
                .resolve(range.start, range.end)
                .is_some_and(|nested| site.function.encloses(nested.item_range))
        {
            return None;
        }
        let target = Target {
            file: file.document,
            symbol: parameter.symbol,
        };
        let mut occurrence = self.occurrence(file, range, range, ReferenceRole::Read, target)?;
        occurrence.edge_from = Some(self.javascript_identity(owner)?);
        Some(occurrence)
    }
}
