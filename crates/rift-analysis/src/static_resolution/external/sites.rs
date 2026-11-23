//! Records the sites resolution against the selected sources left without a target.

use rift_core::{DeclarationBinding, ReferenceRole, SourceRange, SymbolId};
use rift_protocol::read::EdgeKind;
use rift_provider::NormalizedTarget;
use rift_syntax::{
    ByteRange, StaticBindingKind, StaticReference, StaticRustUse, SyntaxExportBinding,
};

use super::super::{Binding, File, Index, StaticOccurrence, StaticResolution, Work};
use super::ExternalOccurrence;
use crate::edge::role_kind;

/// Value names the ECMAScript language itself binds. None names a declaration.
const ECMASCRIPT_CONSTANTS: [&str; 4] = ["undefined", "NaN", "Infinity", "arguments"];

impl<'source> Index<'source> {
    /// Records one value access or type use: the selected declaration it names, else the
    /// dependency or standard-library declaration it spells, else a counted omission.
    ///
    /// `declared` states that a selected declaration answered for the name, so the site
    /// names no dependency.
    pub(in crate::static_resolution) fn record_reference(
        &self,
        file: &File<'source>,
        reference: &StaticReference,
        (occurrence, declared): (Option<StaticOccurrence>, bool),
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        if occurrence.is_some() {
            result.record(occurrence);
            return;
        }
        let external = (!declared)
            .then(|| self.external_reference(file, reference, work))
            .flatten()
            .and_then(|target| {
                self.external_occurrence(
                    file,
                    reference.target,
                    reference.target,
                    reference.role,
                    target,
                    work,
                )
            });
        if let Some(external) = external {
            result.external.push(external);
        } else if Self::names_declaration(file, reference, work) {
            result.record_kind(None, role_kind(reference.role));
        } else {
            result.record(None);
        }
    }

    /// Records one import of a whole module: the selected module it names, else the
    /// dependency or standard-library module its specifier spells, else a counted omission.
    pub(in crate::static_resolution) fn record_module_import(
        &self,
        file: &File<'source>,
        (range, module): (ByteRange, ByteRange),
        occurrence: Option<StaticOccurrence>,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        let external = occurrence
            .is_none()
            .then(|| self.javascript_module_target(file, file.text(module)?))
            .flatten()
            .and_then(|target| {
                self.external_occurrence(file, range, range, ReferenceRole::Import, target, work)
            });
        if external.is_some() {
            self.push_module_import(file, external, work, result);
        } else {
            result.record_kind(occurrence, EdgeKind::Imports);
        }
    }

    /// Records the module one reexport is written against and, for a named reexport of a
    /// dependency or standard-library module, the name it exports.
    pub(in crate::static_resolution) fn record_reexport(
        &self,
        file: &File<'source>,
        (export, module): (&SyntaxExportBinding, ByteRange),
        occurrence: Option<StaticOccurrence>,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        if occurrence.is_none() {
            let external = export.local.and_then(|name| {
                let import = (file.text(module)?, Some(file.text(name)?));
                let target = self.javascript_import_target(file, import, &[])?;
                self.external_occurrence(file, name, name, ReferenceRole::Import, target, work)
            });
            self.push_module_import(file, external, work, result);
        }
        self.record_module_import(file, (export.range, module), occurrence, work, result);
    }

    /// Records one Rust glob import: the selected declaration its path names, else the
    /// dependency or standard-library path it spells, else a counted omission.
    pub(in crate::static_resolution) fn record_glob_import(
        &self,
        file: &File<'source>,
        imported: &StaticRustUse,
        occurrence: Option<StaticOccurrence>,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        let external = occurrence
            .is_none()
            .then(|| self.external_use(file, imported.range, work))
            .flatten()
            .and_then(|target| {
                let range = imported.range;
                self.external_occurrence(file, range, range, ReferenceRole::Import, target, work)
            });
        if let Some(external) = external {
            result.external.push(external);
        } else {
            result.record_kind(occurrence, EdgeKind::Imports);
        }
    }

    /// Records one import the file's module itself holds, whatever declaration it is
    /// written in.
    fn push_module_import(
        &self,
        file: &File<'source>,
        external: Option<ExternalOccurrence>,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        if let Some(mut external) = external {
            external.edge_from = self.javascript_module_identity(file, work);
            result.external.push(external);
        }
    }

    fn external_reference(
        &self,
        file: &File<'source>,
        reference: &StaticReference,
        work: &mut Work,
    ) -> Option<SymbolId> {
        match (file.document.facts.language().name.as_str(), reference.role) {
            ("javascript" | "typescript", ReferenceRole::Type) => {
                self.external_javascript_type(file, reference.target, work)
            }
            // A field access names a member of a value. Its type is not established here.
            ("rust", ReferenceRole::Read | ReferenceRole::Write) => None,
            _ => self.external_type(file, reference.target, work),
        }
    }

    /// Whether an unresolved reference can name a declaration outside the selected sources.
    /// A name bound by the file's own scopes cannot: it names no declaration at all.
    fn names_declaration(
        file: &File<'source>,
        reference: &StaticReference,
        work: &mut Work,
    ) -> bool {
        let Some(text) = file.text(reference.target) else {
            return false;
        };
        if text.contains(['.', ':']) {
            return true;
        }
        let rust = file.document.facts.language().name == "rust";
        if rust && text == "Self" || ECMASCRIPT_CONSTANTS.contains(&text) {
            return false;
        }
        // Value bindings do not shadow paths in Rust's type namespace.
        let accepts = |kind| !rust || super::super::rust::is_type_binding(kind);
        match file.binding(text, reference.target.start, accepts, work) {
            Binding::Found(binding) => matches!(
                binding.kind,
                StaticBindingKind::JavaScriptImport { .. }
                    | StaticBindingKind::JavaScriptEdgeNamespace { .. }
                    | StaticBindingKind::JavaScriptRequire { .. }
                    | StaticBindingKind::JavaScriptTypeImport { .. }
                    | StaticBindingKind::PythonImport { .. }
                    | StaticBindingKind::PythonModule { .. }
                    | StaticBindingKind::RustUse { .. }
            ),
            Binding::Missing | Binding::Blocked => true,
        }
    }

    /// One external occurrence, attributed to the declaration a local target at the same
    /// site would be attributed to.
    pub(in crate::static_resolution) fn external_occurrence(
        &self,
        file: &File<'source>,
        range: ByteRange,
        callee_range: ByteRange,
        role: ReferenceRole,
        target: SymbolId,
        work: &mut Work,
    ) -> Option<ExternalOccurrence> {
        file.text(range)?;
        file.text(callee_range)?;
        let binding = DeclarationBinding::new(
            file.document.placement.unit().clone(),
            SourceRange::new(range.start, range.end).ok()?,
        );
        Some(ExternalOccurrence {
            binding,
            target,
            source_digest: *file.document.facts.source_digest()?,
            callee_range,
            role,
            edge_from: self.external_from(file, range, role, work),
            edge_kind: None,
            declared: false,
        })
    }

    /// The declaration a site belongs to when the language states one other than the
    /// innermost declaration around it.
    fn external_from(
        &self,
        file: &File<'source>,
        range: ByteRange,
        role: ReferenceRole,
        work: &mut Work,
    ) -> Option<SymbolId> {
        match (file.document.facts.language().name.as_str(), role) {
            ("javascript" | "typescript", ReferenceRole::Call) => {
                self.javascript_call_from(file, range, work)
            }
            ("javascript" | "typescript", ReferenceRole::Import) => {
                self.javascript_module_from(file, range, work)
            }
            ("javascript" | "typescript", _) => self
                .javascript_assignment_reference_from(file, range, role, work)
                .or_else(|| self.javascript_module_from(file, range, work)),
            ("python", ReferenceRole::Call) => {
                let caller = file.python_caller(range)?;
                self.python_identity(file, range, caller)
            }
            ("python", ReferenceRole::Import) => {
                self.python_identity(file, range, file.qualified_target(".")?)
            }
            ("python", ReferenceRole::Read | ReferenceRole::Write) => {
                let caller = file.enclosing_callable(range)?;
                self.python_identity(file, range, caller)
            }
            ("rust", ReferenceRole::Import) => {
                let source = self.rust.import_source(file, range, work)?;
                let source = self.occurrence(file, range, range, role, source)?;
                match source.target {
                    NormalizedTarget::Symbol(from) => Some(from),
                    NormalizedTarget::Contribution(_) => None,
                }
            }
            _ => None,
        }
    }
}
