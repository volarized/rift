//! Physical ownership of ordinary ECMAScript value parameters.

use std::collections::BTreeMap;

use rift_core::ReferenceRole;
use rift_syntax::{
    ByteRange, StaticBindingKind, StaticFacts, StaticParameter, SyntaxFacts, SyntaxSymbol,
};

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
    TypeScriptType,
    JavaScriptShorthand,
    JavaScriptWrapped,
    JavaScriptDestructured,
    TypeScriptMethodType,
    TypeScriptFunctionType,
    TypeScriptDestructured,
    JavaScriptCallbackPattern,
    TypeScriptSignatureType,
    TypeScriptDeclaredSignatureType,
    TypeScriptInference,
}

impl ScopedParameterReads {
    pub(super) const COUNT: usize = Self::TypeScriptInference as usize + 1;
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
        .chain(facts.callback_pattern_parameters())
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
            ScopedParameterReads::JavaScriptCallbackPattern => (
                &file.javascript_callback_patterns,
                &mut result.deferred_javascript_callback_patterns,
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
            ScopedParameterReads::JavaScriptShorthand => (
                &file.javascript_shorthand_parameters,
                &mut result.deferred_javascript_shorthand_parameters,
            ),
            ScopedParameterReads::TypeScriptType
            | ScopedParameterReads::JavaScriptWrapped
            | ScopedParameterReads::JavaScriptDestructured
            | ScopedParameterReads::TypeScriptMethodType
            | ScopedParameterReads::TypeScriptFunctionType
            | ScopedParameterReads::TypeScriptDestructured
            | ScopedParameterReads::TypeScriptSignatureType
            | ScopedParameterReads::TypeScriptDeclaredSignatureType
            | ScopedParameterReads::TypeScriptInference => {
                self.checked_parameter_uses(file, class, work, result);
                return;
            }
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

    fn checked_parameter_uses(
        &self,
        file: &File<'source>,
        class: ScopedParameterReads,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        let (role, parameters, pending) = match class {
            ScopedParameterReads::TypeScriptType => (
                ReferenceRole::Type,
                &file.typescript_type_parameters,
                &mut result.deferred_typescript_type_parameters,
            ),
            ScopedParameterReads::JavaScriptWrapped => (
                ReferenceRole::Read,
                &file.javascript_wrapped_parameters,
                &mut result.deferred_javascript_wrapped_parameters,
            ),
            ScopedParameterReads::JavaScriptDestructured => (
                ReferenceRole::Read,
                &file.javascript_destructured_parameters,
                &mut result.deferred_javascript_destructured_parameters,
            ),
            ScopedParameterReads::TypeScriptMethodType => (
                ReferenceRole::Type,
                &file.typescript_method_type_parameters,
                &mut result.deferred_typescript_method_type_parameters,
            ),
            ScopedParameterReads::TypeScriptFunctionType => (
                ReferenceRole::Type,
                &file.typescript_function_type_parameters,
                &mut result.deferred_typescript_function_type_parameters,
            ),
            ScopedParameterReads::TypeScriptDestructured => (
                ReferenceRole::Read,
                &file.typescript_destructured_parameters,
                &mut result.deferred_typescript_destructured_parameters,
            ),
            ScopedParameterReads::TypeScriptSignatureType => (
                ReferenceRole::Type,
                &file.typescript_signature_type_parameters,
                &mut result.deferred_typescript_signature_type_parameters,
            ),
            ScopedParameterReads::TypeScriptDeclaredSignatureType => (
                ReferenceRole::Type,
                &file.typescript_declared_signature_type_parameters,
                &mut result.deferred_typescript_declared_signature_type_parameters,
            ),
            ScopedParameterReads::TypeScriptInference => (
                ReferenceRole::Type,
                &file.typescript_inference_parameters,
                &mut result.deferred_typescript_inference_parameters,
            ),
            _ => return,
        };
        for reference in file.facts.references() {
            if !work.charge() {
                return;
            }
            if reference.role != role || file.members.contains_key(&reference.target) {
                continue;
            }
            if let Some(occurrence) =
                self.checked_parameter_use(file, reference.target, role, parameters, work)
            {
                result.unresolved_sites = result.unresolved_sites.saturating_sub(1);
                pending.push(occurrence);
            }
        }
    }

    fn checked_parameter_use(
        &self,
        file: &File<'source>,
        range: ByteRange,
        role: ReferenceRole,
        parameters: &BTreeMap<ByteRange, &'source SyntaxSymbol>,
        work: &mut Work,
    ) -> Option<StaticOccurrence> {
        let name = file.text(range)?;
        let binding = js_binding(file, &self.javascript, name, range.start, role, work)?;
        let symbol = *parameters.get(&binding.name)?;
        let container = symbol.container.as_deref()?;
        let owner = if symbol.is_typescript_declared_signature_type_parameter() {
            let mut matches = file
                .by_qualified_name
                .get(container)?
                .iter()
                .copied()
                .filter(|owner| {
                    owner.item_range == binding.scope
                        && owner.syntax_element_kind == Some("function_signature")
                });
            let owner = matches.next()?;
            if matches.next().is_some() {
                return None;
            }
            Target {
                file: file.document,
                symbol: owner,
            }
        } else {
            file.qualified_target(container)?
        };
        let inferred = symbol.is_typescript_inference_parameter();
        let scope_matches = if inferred || symbol.is_typescript_signature_type_parameter() {
            owner.symbol.item_range.encloses(binding.scope)
                && binding.scope.encloses(symbol.item_range)
        } else {
            binding.scope == owner.symbol.item_range
        };
        let expected = if inferred {
            StaticBindingKind::TypeScriptInference
        } else {
            StaticBindingKind::Shadow
        };
        if binding.kind != expected
            || (inferred && binding.available_from > range.start)
            || !scope_matches
            || !binding.scope.encloses(range)
            || (role == ReferenceRole::Read && !owner.symbol.body_range?.encloses(range))
        {
            return None;
        }
        let target = Target {
            file: file.document,
            symbol,
        };
        let mut occurrence = self.occurrence(file, range, range, role, target)?;
        occurrence.edge_from = Some(self.javascript_identity(owner)?);
        Some(occurrence)
    }

    pub(super) fn resolve_javascript_wrapped_arrow_reads(
        &self,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        let start = result.occurrences.len();
        'files: for file in self
            .files
            .values()
            .filter(|file| !file.javascript_arrow_wrapped_parameters.is_empty())
        {
            let mut bodies = BTreeMap::new();
            for &(scope, body) in file.facts.javascript_wrapped_arrow_bodies() {
                if !work.charge() {
                    break 'files;
                }
                bodies.insert(scope, body);
            }
            for reference in file.facts.references() {
                if !work.charge() {
                    break 'files;
                }
                if reference.role == ReferenceRole::Read
                    && !file.members.contains_key(&reference.target)
                    && let Some(occurrence) =
                        self.javascript_wrapped_arrow_read(file, reference.target, &bodies, work)
                {
                    result.occurrences.push(occurrence);
                    result.unresolved_sites = result.unresolved_sites.saturating_sub(1);
                }
            }
        }
        result.javascript_wrapped_arrow_read_start =
            (result.occurrences.len() > start).then_some(start);
    }

    fn javascript_wrapped_arrow_read(
        &self,
        file: &File<'source>,
        range: ByteRange,
        bodies: &BTreeMap<ByteRange, ByteRange>,
        work: &mut Work,
    ) -> Option<StaticOccurrence> {
        let role = ReferenceRole::Read;
        let binding = js_binding(
            file,
            &self.javascript,
            file.text(range)?,
            range.start,
            role,
            work,
        )?;
        let symbol = *file
            .javascript_arrow_wrapped_parameters
            .get(&binding.name)?;
        let owner = file.qualified_target(symbol.container.as_deref()?)?;
        if binding.kind != StaticBindingKind::Shadow
            || owner.symbol.body_range != Some(binding.scope)
            || !bodies.get(&binding.scope)?.encloses(range)
        {
            return None;
        }
        let target = Target {
            file: file.document,
            symbol,
        };
        let mut occurrence = self.occurrence(file, range, range, role, target)?;
        occurrence.edge_from = Some(self.javascript_call_from(file, range, work)?);
        Some(occurrence)
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
            || parameter.symbol.is_javascript_shorthand_parameter()
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
