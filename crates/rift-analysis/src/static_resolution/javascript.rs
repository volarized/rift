//! Resolves explicit JavaScript module paths and lexical bindings.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::ReferenceRole;
use rift_protocol::read::EdgeKind;
use rift_provider::NormalizedTarget;
use rift_syntax::{
    ByteRange, HeritageEdge, StaticBinding, StaticBindingKind, StaticCall, StaticCallee,
};

use super::{Binding, File, Index, StaticOccurrence, StaticResolution, Target, Work};
use crate::NamespaceInput;
use crate::analyzer::namespace::typescript::{
    PackageFiles, PackageImports, nested_runtime_candidates,
};

/// A member site awaiting proof of its receiver's call-result assignment.
#[derive(Debug)]
pub(crate) enum AssignedMemberSite {
    /// A written member invocation.
    Call(StaticCall),
    /// A written member access.
    Read(ByteRange),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ModuleMode {
    Esm,
    CommonJs,
    NodeDefault,
    Unknown,
}

/// Module-mode declarations captured from the nearest package manifest.
pub(super) struct PackageModes<'source> {
    modes: BTreeMap<String, ModuleMode>,
    /// What the manifest of each folder names for a specifier of the folder.
    folders: BTreeMap<String, Folder>,
    module_syntax: std::collections::BTreeSet<&'source str>,
    imports: Option<PackageImports>,
    pub(super) owner: rift_protocol::identity::SymbolOwner,
    pub(super) scopes: BTreeMap<&'source str, Scopes<'source>>,
}

/// What the `package.json` of a folder names for a specifier of the folder.
#[derive(Default)]
pub(super) struct Folder {
    /// Whether the manifest could be read.
    pub(super) known: bool,
    /// The file a runtime loads, as the manifest writes it.
    pub(super) main: Option<String>,
    /// The explicit script entry, or the extensionless entry's outer JavaScript index.
    pub(super) observed_entry: Option<rift_core::ProjectPath>,
    /// Captured path observations decide the full runtime order for this explicit script entry.
    pub(super) observes_explicit_main: bool,
    /// Whether an observed file wins before the folder for a bare specifier.
    pub(super) earlier_file: Option<bool>,
    /// The declaration file, as the manifest writes it.
    pub(super) types: Option<String>,
}

pub(super) struct Scopes<'source> {
    pub(super) values: BTreeMap<rift_syntax::ByteRange, Option<&'source rift_syntax::SyntaxSymbol>>,
    pub(super) classes: crate::EnclosingDefinitions<&'source rift_syntax::SyntaxSymbol>,
    pub(super) definitions: crate::EnclosingDefinitions<&'source rift_syntax::SyntaxSymbol>,
    pub(super) callables: crate::EnclosingDefinitions<&'source rift_syntax::SyntaxSymbol>,
    /// Every arrow function and function expression, named or not.
    pub(super) function_values: crate::EnclosingDefinitions<rift_syntax::ByteRange>,
    pub(super) functions: crate::EnclosingDefinitions<rift_syntax::ByteRange>,
    pub(super) blocks: crate::EnclosingDefinitions<rift_syntax::ByteRange>,
    pub(super) deferred: crate::EnclosingDefinitions<rift_syntax::ByteRange>,
}

impl<'source> PackageModes<'source> {
    pub(super) fn new(
        namespace: &NamespaceInput<'_>,
        files: &BTreeMap<&'source str, File<'source>>,
        work: &mut Work,
    ) -> Option<Self> {
        let mut modes = BTreeMap::new();
        let mut folders = BTreeMap::new();
        let mut observed_files = None;
        for source in namespace.files().iter().chain(namespace.context_sources()) {
            if !work.charge() {
                return None;
            }
            let Some(directory) = manifest_directory(source.path().as_str()) else {
                continue;
            };
            let manifest = (source.text().len() <= namespace.syntax().source_bytes_max())
                .then(|| serde_json::from_str(source.text()).ok())
                .flatten();
            modes
                .entry(directory.to_owned())
                .or_insert_with(|| package_mode(manifest.as_ref()));
            folders.entry(directory.to_owned()).or_insert_with(|| {
                let mut folder = folder(manifest.as_ref());
                if namespace.build_paths().is_some()
                    && !directory.is_empty()
                    && let Some(main) = folder.main.as_deref()
                    && let Some(candidates) = nested_runtime_candidates(main, directory)
                {
                    let files =
                        observed_files.get_or_insert_with(|| PackageFiles::captured(namespace));
                    folder.observes_explicit_main = candidates.explicit_script;
                    folder.observed_entry =
                        files
                            .observed_folder_entry(candidates.paths)
                            .filter(|path| {
                                folder.observes_explicit_main
                                    || path.as_str() == format!("{directory}/index.js")
                            });
                    if folder.observed_entry.is_some() {
                        folder.earlier_file = files.file_before_folder(directory);
                    }
                }
                folder
            });
        }
        let mut scopes = BTreeMap::new();
        let mut module_syntax = std::collections::BTreeSet::new();
        for file in files.values().filter(|file| {
            matches!(
                file.document.facts.language().name.as_str(),
                "javascript" | "typescript"
            )
        }) {
            if file.facts.has_javascript_module_syntax() {
                module_syntax.insert(file.document.path.as_str());
            }
            scopes.insert(file.document.path.as_str(), Scopes::new(file, work)?);
        }
        Some(Self {
            modes,
            folders,
            module_syntax,
            imports: PackageImports::captured(namespace),
            owner: namespace.owner().clone(),
            scopes,
        })
    }

    /// What the manifest of a folder names, or `None` for a folder without one.
    pub(super) fn folder(&self, directory: &str) -> Option<&Folder> {
        self.folders.get(directory)
    }

    /// The root imports map applies only to sources within the root manifest's own scope.
    pub(super) fn import_alias(
        &self,
        from: &str,
        specifier: &str,
        typescript: bool,
        require: bool,
        work: &mut Work,
    ) -> Option<rift_core::ProjectPath> {
        let imports = self.imports.as_ref()?;
        for (end, _) in from.rmatch_indices('/') {
            if !work.charge() || self.folders.contains_key(&from[..end]) {
                return None;
            }
        }
        imports.destination(specifier, typescript, require, || work.charge())
    }

    pub(super) fn mode(&self, path: &str, work: &mut Work) -> ModuleMode {
        match path.rsplit_once('.').map(|(_, extension)| extension) {
            Some("mjs" | "ts" | "tsx" | "mts" | "cts") => return ModuleMode::Esm,
            Some("cjs") => return ModuleMode::CommonJs,
            Some("js") => {}
            _ => return ModuleMode::Unknown,
        }
        let mut directory = path.rsplit_once('/').map_or("", |(directory, _)| directory);
        loop {
            if !work.charge() {
                return ModuleMode::Unknown;
            }
            if let Some(&mode) = self.modes.get(directory) {
                if mode == ModuleMode::NodeDefault {
                    return if self.module_syntax.contains(path) {
                        ModuleMode::Esm
                    } else {
                        ModuleMode::CommonJs
                    };
                }
                return mode;
            }
            if directory.is_empty() {
                return ModuleMode::Unknown;
            }
            directory = directory.rsplit_once('/').map_or("", |(parent, _)| parent);
        }
    }
}

/// Whether the file is JavaScript source, whose types documentation comments write.
pub(super) fn javascript_source(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .is_some_and(|extension| {
            ["js", "cjs", "mjs", "jsx"]
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known))
        })
}

pub(super) fn typescript_commonjs(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("cts"))
}

fn manifest_directory(path: &str) -> Option<&str> {
    if path == "package.json" {
        Some("")
    } else {
        path.strip_suffix("/package.json")
    }
}

/// Whether `new` can construct the target: in JavaScript every function written with the
/// `function` keyword is a constructor; an arrow function, a method and a generator are not.
fn javascript_constructor_function(target: Target<'_>) -> bool {
    javascript_source(target.file.path.as_str())
        && target.symbol.kind == "function"
        && matches!(
            target.symbol.syntax_element_kind,
            Some("function_declaration" | "function_expression" | "assignment_expression")
        )
}

type Manifest = serde_json::Map<String, serde_json::Value>;

fn package_mode(manifest: Option<&Manifest>) -> ModuleMode {
    match manifest.map(|manifest| manifest.get("type")) {
        Some(Some(value)) if value.as_str() == Some("module") => ModuleMode::Esm,
        Some(Some(value)) if value.as_str() == Some("commonjs") => ModuleMode::CommonJs,
        Some(None) => ModuleMode::NodeDefault,
        Some(Some(_)) | None => ModuleMode::Unknown,
    }
}

fn folder(manifest: Option<&Manifest>) -> Folder {
    let Some(manifest) = manifest else {
        return Folder::default();
    };
    let written = |key| {
        manifest
            .get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    Folder {
        known: true,
        main: written("main"),
        observed_entry: None,
        observes_explicit_main: false,
        earlier_file: None,
        types: written("types").or_else(|| written("typings")),
    }
}

impl<'source> Index<'source> {
    pub(super) fn edge_javascript_target(
        &self,
        file: &File<'source>,
        range: rift_syntax::ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let name = file.text(range)?.trim();
        if name
            .split_once('.')
            .is_some_and(|(receiver, _)| matches!(receiver.trim(), "this" | "super"))
        {
            return None;
        }
        self.javascript_target(file, range, rift_core::ReferenceRole::Type, work)
    }

    /// A class extends a constructor value. Keep the normal type path for
    /// interfaces and implements, and admit an authored class value only for extends.
    pub(super) fn javascript_heritage_target(
        &self,
        file: &File<'source>,
        edge: &HeritageEdge,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        self.edge_javascript_target(file, edge.target, work)
            .or_else(|| {
                if edge.kind != EdgeKind::Extends
                    || file.target_at(edge.from)?.symbol.kind != "class"
                {
                    return None;
                }
                let base = self.javascript_target(
                    file,
                    edge.target,
                    rift_core::ReferenceRole::Read,
                    work,
                )?;
                self.javascript_stable_class_value(base).then_some(base)
            })
    }

    pub(super) fn javascript_target(
        &self,
        file: &File<'source>,
        range: rift_syntax::ByteRange,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if let Some(member) = file.members.get(&range) {
            let member = (*member)?;
            let name = file.text(member.member?)?;
            if role != rift_core::ReferenceRole::Type && file.callbacks.is_call(member.receiver) {
                let owner = self.javascript_call_result(file, member.receiver, work)?;
                return self.javascript_class_member(owner, name, MemberMode::Instance, role, work);
            }
            if !file.members.contains_key(&member.receiver) {
                return self.javascript_named_member(
                    file,
                    file.text(member.receiver)?,
                    name,
                    range.start,
                    role,
                    work,
                );
            }
        }
        let name = file.text(range)?.trim();
        if let Some((receiver, member)) = name.split_once('.') {
            return self.javascript_named_member(
                file,
                receiver.trim(),
                member.trim(),
                range.start,
                role,
                work,
            );
        }
        let binding = js_binding(file, &self.javascript, name, range.start, role, work)?;
        self.javascript_binding_target(file, binding, role, work)
    }

    fn javascript_named_member(
        &self,
        file: &File<'source>,
        receiver: &str,
        member: &'source str,
        position: u64,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if matches!(receiver, "this" | "super") {
            return self.javascript_receiver_member(file, position, receiver, member, role, work);
        }
        // A receiver with a declared type keeps it when the binding is assigned again: the
        // call is refused only for a callee named by the binding itself. So does a
        // TypeScript binding typed by the spread its initializer ends in.
        let binding =
            js_binding(file, &self.javascript, receiver, position, role, work).or_else(|| {
                (role == rift_core::ReferenceRole::Call)
                    .then(|| {
                        js_binding(
                            file,
                            &self.javascript,
                            receiver,
                            position,
                            rift_core::ReferenceRole::Read,
                            work,
                        )
                    })
                    .flatten()
                    .filter(|binding| {
                        file.declared_types
                            .get(&binding.name)
                            .copied()
                            .flatten()
                            .is_some()
                            || (file.spread_values.contains_key(&binding.name)
                                && !javascript_source(file.document.path.as_str()))
                    })
            });
        match binding {
            Some(binding) => self.javascript_member(file, binding, member, role, work),
            None => self.javascript_commonjs_member(file, receiver, member, position, role, work),
        }
    }

    pub(super) fn javascript_call(
        &self,
        file: &File<'source>,
        call: &StaticCall,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if call.callee == StaticCallee::Unsupported {
            return None;
        }
        if call.callee == StaticCallee::Constructor && file.text(call.callee_range)? == "super" {
            return self.javascript_super_constructor(file, call.range.start, work);
        }
        let target = match call.callee {
            StaticCallee::Member { receiver, member } if file.callbacks.is_call(receiver) => {
                let owner = self.javascript_call_result(file, receiver, work)?;
                self.javascript_class_member(
                    owner,
                    file.text(member)?,
                    MemberMode::Instance,
                    rift_core::ReferenceRole::Call,
                    work,
                )?
            }
            StaticCallee::Member { receiver, member } if !file.text(receiver)?.contains('.') => {
                self.javascript_named_member(
                    file,
                    file.text(receiver)?,
                    file.text(member)?,
                    call.range.start,
                    rift_core::ReferenceRole::Call,
                    work,
                )?
            }
            _ => self.javascript_target(
                file,
                call.callee_range,
                rift_core::ReferenceRole::Call,
                work,
            )?,
        };
        if call.callee == StaticCallee::Constructor {
            return (target.symbol.kind == "class"
                || target
                    .symbol
                    .kinds
                    .contains(&rift_protocol::read::SymbolKind::Constructible)
                || javascript_constructor_function(target)
                || self.javascript_declared_signature(target, true, work)
                || self.javascript_stable_class_value(target))
            .then_some(target);
        }
        target
            .callable()
            .or_else(|| {
                self.javascript_declared_signature(target, false, work)
                    .then_some(target)
            })
            .or_else(|| {
                self.javascript_callable_result(target, work)
                    .then_some(target)
            })
            .or_else(|| {
                self.javascript_shorthand_callable(target, work)
                    .then_some(target)
            })
    }

    /// Follows an unchanged local alias for an authored direct invocation after earlier facts.
    pub(super) fn resolve_javascript_lexical_alias_calls(
        &self,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        let start = result.occurrences.len();
        let candidates = std::mem::take(&mut result.javascript_lexical_alias_candidates);
        if candidates.is_empty() {
            return;
        }
        for file in self.files.values().filter(|file| {
            let facts = file.document.facts;
            matches!(facts.language().name.as_str(), "javascript" | "typescript")
                && !facts.has_errors()
                && facts.left_out_declaration_count() == 0
        }) {
            if work.exhausted {
                break;
            }
            let unit = file.document.placement.unit();
            let Some((calls, coverage)) = candidates.get(unit).zip(result.edge_calls.get_mut(unit))
            else {
                continue;
            };
            if coverage.unavailable {
                continue;
            }
            let mut resolved = BTreeSet::new();
            for call in calls {
                if !work.charge() {
                    break;
                }
                if let Some(found) = self.javascript_lexical_alias_call(file, call, work) {
                    result.occurrences.push(found);
                    resolved.insert(call.range);
                }
            }
            coverage
                .unresolved
                .retain(|range| !resolved.contains(range));
        }
        let resolved = result.occurrences.len() - start;
        result.javascript_lexical_alias_call_start = (resolved > 0).then_some(start);
        result.unresolved_sites = result.unresolved_sites.saturating_sub(resolved);
        if let Some(count) = result.omitted.get_mut(&EdgeKind::Calls) {
            *count = count.saturating_sub(u64::try_from(resolved).unwrap_or(u64::MAX));
            if *count == 0 {
                result.omitted.remove(&EdgeKind::Calls);
            }
        }
    }

    fn javascript_lexical_alias_call(
        &self,
        file: &File<'source>,
        call: &StaticCall,
        work: &mut Work,
    ) -> Option<StaticOccurrence> {
        let alias = self.javascript_unchanged_local(file, call.callee_range, work)?;
        let value = Self::javascript_alias_initializer(alias)?;
        let target = self.javascript_lexical_callable(file, value, work)?;
        let mut found = self.occurrence(
            file,
            call.range,
            call.callee_range,
            ReferenceRole::Call,
            target,
        )?;
        if !matches!(found.target, NormalizedTarget::Symbol(_)) {
            return None;
        }
        found.edge_from = Some(self.javascript_call_from(file, call.range, work)?);
        Some(found)
    }

    /// Follows direct identifiers only, using the existing nesting and work limits.
    fn javascript_lexical_callable(
        &self,
        file: &File<'source>,
        value: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        work.javascript_lookup(|work| {
            let target = self.javascript_unchanged_local(file, value, work)?;
            if target.symbol.syntax_element_kind == Some("function_declaration") {
                target.symbol.body_range?;
                return target.callable();
            }
            self.javascript_lexical_callable(
                file,
                Self::javascript_alias_initializer(target)?,
                work,
            )
        })
    }

    fn javascript_unchanged_local(
        &self,
        file: &File<'source>,
        name: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let binding = js_binding(
            file,
            &self.javascript,
            file.text(name)?,
            name.start,
            ReferenceRole::Call,
            work,
        )?;
        if binding.kind != StaticBindingKind::Local || file.javascript_binding_reassigned(binding) {
            return None;
        }
        file.target_at(binding.name)
    }

    fn javascript_alias_initializer(target: Target<'source>) -> Option<ByteRange> {
        (target.symbol.kind == "variable"
            && target.symbol.syntax_element_kind == Some("variable_declarator"))
        .then_some(target.symbol.body_range)
        .flatten()
    }

    /// Adds the checked member facts after the previously resolved package edges.
    pub(super) fn resolve_javascript_assigned_members(
        &self,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        let candidates = std::mem::take(&mut result.javascript_assigned_members);
        if candidates.is_empty() {
            return;
        }
        let start = result.occurrences.len();
        let mut resolved_calls = 0_usize;
        for file in self.files.values() {
            if work.exhausted {
                break;
            }
            let unit = file.document.placement.unit();
            let Some((sites, coverage)) = candidates.get(unit).zip(result.edge_calls.get_mut(unit))
            else {
                continue;
            };
            if coverage.unavailable
                || file.document.facts.has_errors()
                || file.document.facts.left_out_declaration_count() != 0
            {
                continue;
            }
            let mut resolved = BTreeSet::new();
            let mut assignments = BTreeMap::new();
            for site in sites {
                if !work.charge() {
                    break;
                }
                if let Some(found) =
                    self.javascript_assigned_member(file, site, &mut assignments, work)
                {
                    if let AssignedMemberSite::Call(call) = site {
                        resolved.insert(call.range);
                    }
                    result.occurrences.push(found);
                }
            }
            resolved_calls += resolved.len();
            coverage
                .unresolved
                .retain(|range| !resolved.contains(range));
        }
        let resolved = result.occurrences.len() - start;
        result.javascript_assigned_member_start = (resolved > 0).then_some(start);
        result.unresolved_sites = result.unresolved_sites.saturating_sub(resolved);
        if let Some(count) = result.omitted.get_mut(&EdgeKind::Calls) {
            *count = count.saturating_sub(u64::try_from(resolved_calls).unwrap_or(u64::MAX));
            if *count == 0 {
                result.omitted.remove(&EdgeKind::Calls);
            }
        }
    }

    fn javascript_assigned_member(
        &self,
        file: &File<'source>,
        site: &AssignedMemberSite,
        assignments: &mut BTreeMap<ByteRange, Option<ByteRange>>,
        work: &mut Work,
    ) -> Option<StaticOccurrence> {
        let (range, callee, receiver, member, role) = match site {
            AssignedMemberSite::Call(call) => {
                let StaticCallee::Member { receiver, member } = call.callee else {
                    return None;
                };
                (
                    call.range,
                    call.callee_range,
                    receiver,
                    member,
                    ReferenceRole::Call,
                )
            }
            AssignedMemberSite::Read(range) => {
                let member = (*file.members.get(range)?)?;
                (
                    *range,
                    *range,
                    member.receiver,
                    member.member?,
                    ReferenceRole::Read,
                )
            }
        };
        let member = file.text(member)?;
        let assignment = (*assignments
            .entry(receiver)
            .or_insert_with(|| self.javascript_settled_call_assignment(file, receiver, work)))?;
        let target =
            self.javascript_member_of_returned_call(file, assignment, member, role, true, work)?;
        if role == ReferenceRole::Call
            && target.callable().is_none()
            && !self.javascript_shorthand_callable(target, work)
        {
            return None;
        }
        let mut occurrence = self.occurrence(file, range, callee, role, target)?;
        if !matches!(occurrence.target, NormalizedTarget::Symbol(_)) {
            return None;
        }
        occurrence.edge_from = Some(self.javascript_call_from(file, range, work)?);
        Some(occurrence)
    }

    /// Proves that the first use follows one direct assignment in its lexical block.
    fn javascript_settled_call_assignment(
        &self,
        file: &File<'source>,
        receiver: ByteRange,
        work: &mut Work,
    ) -> Option<ByteRange> {
        let name = file.text(receiver)?;
        let binding = js_binding(
            file,
            &self.javascript,
            name,
            receiver.start,
            ReferenceRole::Read,
            work,
        )?;
        let local = file.target_at(binding.name)?;
        if binding.kind != StaticBindingKind::Local
            || local.symbol.syntax_element_kind != Some("variable_declarator")
            || local.symbol.body_range.is_some()
        {
            return None;
        }
        let mut writes = file
            .assignments
            .name_writes
            .get(name)?
            .iter()
            .copied()
            .filter(|write| binding.scope.contains(*write));
        let write = writes.next()?;
        if writes.next().is_some() {
            return None;
        }
        let assignments = file.facts.assigned_calls();
        let index = assignments
            .binary_search_by_key(&write, |assignment| assignment.target.start)
            .ok()?;
        let site = assignments[index];
        if site.block != binding.scope
            || binding.name.end > site.target.start
            || site.call.end > receiver.start
            || !site.block.encloses(receiver)
        {
            return None;
        }
        let scopes = self.javascript.scopes.get(file.document.path.as_str())?;
        if scopes.deferred.resolve(site.target.start, site.target.end)
            != scopes.deferred.resolve(receiver.start, receiver.end)
        {
            return None;
        }
        for reference in file.facts.references() {
            if !work.charge() {
                return None;
            }
            if site.call.end <= reference.target.start
                && reference.target.start < receiver.start
                && reference.role == ReferenceRole::Read
                && file.text(reference.target) == Some(name)
            {
                return None;
            }
        }
        Some(site.call)
    }

    /// Whether a shorthand object member (`{ verify }`) holds a callable value: the name it
    /// is written with resolves, in the member's own file, to a callable declaration. The
    /// call targets the member itself.
    fn javascript_shorthand_callable(&self, target: Target<'source>, work: &mut Work) -> bool {
        if target.symbol.syntax_element_kind != Some("shorthand_property_identifier") {
            return false;
        }
        let Some((file, name)) = self
            .files
            .get(target.file.path.as_str())
            .zip(target.symbol.name_range)
        else {
            return false;
        };
        work.javascript_lookup(|work| {
            let binding = js_binding(
                file,
                &self.javascript,
                file.text(name)?,
                name.start,
                rift_core::ReferenceRole::Call,
                work,
            )?;
            let value = self.javascript_binding_target(
                file,
                binding,
                rift_core::ReferenceRole::Call,
                work,
            )?;
            value.callable().map(|_| ())
        })
        .is_some()
    }

    /// The immutable local function named by a direct property of an object literal that no
    /// later property can replace.
    pub(super) fn javascript_final_pair_callable(
        &self,
        file: &File<'source>,
        member: Target<'source>,
        object: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if member.symbol.syntax_element_kind != Some("pair")
            || member.callable().is_some()
            || !(javascript_final_pair(file, member, object, work)
                || file.exported_pair_is_settled(member, work))
        {
            return None;
        }
        let value = member.symbol.body_range?;
        let binding = js_binding(
            file,
            &self.javascript,
            file.text(value)?,
            value.start,
            rift_core::ReferenceRole::Call,
            work,
        )?;
        if binding.kind != StaticBindingKind::Local || file.javascript_binding_reassigned(binding) {
            return None;
        }
        let function = file.target_at(binding.name)?.callable()?;
        (function.symbol.kind == "function" && function.symbol.body_range.is_some())
            .then_some(function)
    }

    /// Whether the binding's declared type states that its value can be called or constructed.
    ///
    /// The annotation must name, through at most `DECLARED_TYPE_DEPTH_MAX` aliases, a type
    /// declaration whose own shape is the signature: a function or constructor type alias,
    /// or an interface with a call or construct signature. The call then targets the
    /// binding itself; no function body is chosen for it.
    pub(super) fn javascript_declared_signature(
        &self,
        target: Target<'source>,
        constructs: bool,
        work: &mut Work,
    ) -> bool {
        let Some(file) = self.files.get(target.file.path.as_str()) else {
            return false;
        };
        // A declaration written with a function type, `run: () => number`, is callable.
        if !constructs
            && target
                .symbol
                .name_range
                .is_some_and(|name| file.callbacks.has_function_type(name))
        {
            return true;
        }
        let Some(annotation) = target
            .symbol
            .name_range
            .and_then(|name| file.declared_types.get(&name).copied().flatten())
        else {
            return false;
        };
        self.javascript_written_signature(file, annotation, constructs, work)
    }

    /// Whether the written type `annotation` names, through at most
    /// `DECLARED_TYPE_DEPTH_MAX` aliases, a type whose own shape is a call or construct
    /// signature, or the global `Function` type.
    pub(super) fn javascript_written_signature<'index>(
        &'index self,
        mut file: &'index File<'source>,
        mut annotation: rift_syntax::ByteRange,
        constructs: bool,
        work: &mut Work,
    ) -> bool {
        let declared = |file: &File<'source>, name: rift_syntax::ByteRange| {
            file.declared_types.get(&name).copied().flatten()
        };
        for _ in 0..super::javascript_members::DECLARED_TYPE_DEPTH_MAX {
            if !work.charge() {
                return false;
            }
            let Some(named) =
                self.javascript_target(file, annotation, rift_core::ReferenceRole::Type, work)
            else {
                // The global `Function` type, which no declaration of the workspace takes,
                // is callable and constructs nothing.
                return !constructs && file.text(annotation) == Some("Function");
            };
            let Some((owner, name)) = self
                .files
                .get(named.file.path.as_str())
                .zip(named.symbol.name_range)
            else {
                return false;
            };
            if owner.type_signatures.contains(&(name, constructs)) {
                return true;
            }
            if named.symbol.kind != "type_alias" {
                return false;
            }
            let Some(next) = declared(owner, name) else {
                return false;
            };
            file = owner;
            annotation = next;
        }
        false
    }

    pub(super) fn javascript_import(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if file.blocked(binding.name.start, work) {
            return None;
        }
        if let StaticBindingKind::JavaScriptRequire { export, .. } = binding.kind {
            export?;
            return self
                .javascript_require_value(file, binding, rift_core::ReferenceRole::Type, work)
                .or_else(|| {
                    self.javascript_require_value(
                        file,
                        binding,
                        rift_core::ReferenceRole::Read,
                        work,
                    )
                });
        }
        self.javascript_binding_target(file, binding, rift_core::ReferenceRole::Type, work)
            .or_else(|| {
                self.javascript_binding_target(file, binding, rift_core::ReferenceRole::Read, work)
            })
    }

    /// Whether the selected physical variable still holds its written class.
    pub(super) fn javascript_stable_class_value(&self, target: Target<'source>) -> bool {
        self.files
            .get(target.file.path.as_str())
            .and_then(|file| file.javascript_class_value_body(target.symbol))
            .is_some()
    }

    fn javascript_binding_target(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        match binding.kind {
            StaticBindingKind::Shadow
                if matches!(
                    role,
                    rift_core::ReferenceRole::Read | rift_core::ReferenceRole::Write
                ) =>
            {
                file.target_at(binding.name)
                    .filter(|target| target.is_scoped_javascript_lexical_binding())
            }
            StaticBindingKind::Local => file.target_at(binding.name).or_else(|| {
                let scopes = self.javascript.scopes.get(file.document.path.as_str())?;
                let symbol = scopes.values.get(&binding.scope).copied().flatten()?;
                Some(Target {
                    file: file.document,
                    symbol,
                })
            }),
            StaticBindingKind::JavaScriptClassValue { .. }
            | StaticBindingKind::JavaScriptInstance { .. }
            | StaticBindingKind::JavaScriptConstInstance { .. } => file.target_at(binding.name),
            StaticBindingKind::JavaScriptImport { module, export } => {
                let name = match export {
                    Some(range) => file.text(range)?,
                    None => "default",
                };
                if self.javascript.mode(file.document.path.as_str(), work) == ModuleMode::CommonJs {
                    return None;
                }
                self.javascript_export(file, file.text(module)?, name, ModuleMode::Esm, role, work)
            }
            StaticBindingKind::JavaScriptRequire { .. }
                if matches!(
                    role,
                    rift_core::ReferenceRole::Read | rift_core::ReferenceRole::Write
                ) =>
            {
                file.target_at(binding.name)
            }
            StaticBindingKind::JavaScriptRequire { .. } => {
                self.javascript_require_value(file, binding, role, work)
            }
            StaticBindingKind::JavaScriptTypeImport { module, export }
                if role == rift_core::ReferenceRole::Type =>
            {
                let mode = match self.javascript.mode(file.document.path.as_str(), work) {
                    ModuleMode::CommonJs => ModuleMode::CommonJs,
                    _ => ModuleMode::Esm,
                };
                self.javascript_export(
                    file,
                    file.text(module)?,
                    file.text(export)?,
                    mode,
                    role,
                    work,
                )
            }
            _ => None,
        }
    }

    fn javascript_require_value(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let StaticBindingKind::JavaScriptRequire {
            module,
            export,
            require,
        } = binding.kind
        else {
            return None;
        };
        if !self.builtin_require(file, require.start, work) {
            return None;
        }
        if let Some(export) = export {
            let module = file.text(module)?;
            let export = file.text(export)?;
            // `const { name } = require(module)` reads what `require(module).name` reads.
            return self
                .javascript_export(file, module, export, ModuleMode::CommonJs, role, work)
                .or_else(|| {
                    self.javascript_module_value_member(file, binding, module, export, role, work)
                });
        }
        let value = super::javascript_exports::Exports {
            files: &self.files,
            modes: &self.javascript,
            names: None,
        }
        .resolve_module_value(file, file.text(module)?, role, work)?;
        self.javascript_export_value(value, role, work)
    }

    fn javascript_member(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        member: &'source str,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if role != rift_core::ReferenceRole::Type
            && let Some(&Some(annotation)) = file.declared_types.get(&binding.name)
        {
            let owner = self.javascript_declared_type(file, annotation, work)?;
            return self.javascript_class_member(owner, member, MemberMode::Instance, role, work);
        }
        if role != rift_core::ReferenceRole::Type
            && binding.kind == StaticBindingKind::Shadow
            && let Some(owner) = self
                .javascript_callback_parameter(file, binding, work)
                .or_else(|| self.javascript_destructured_name(file, binding, work))
        {
            return self.javascript_class_member(owner, member, MemberMode::Instance, role, work);
        }
        // A member the last spread value of an object literal declares is that value's
        // member (D35); members it does not declare stay the literal's own.
        if role != rift_core::ReferenceRole::Type
            && binding.kind == StaticBindingKind::Local
            && let Some(target) =
                self.javascript_spread_owner(file, binding, work)
                    .and_then(|owner| {
                        self.javascript_class_member(
                            owner,
                            member,
                            MemberMode::Instance,
                            role,
                            work,
                        )
                    })
        {
            return Some(target);
        }
        match binding.kind {
            StaticBindingKind::JavaScriptEdgeNamespace { module } => self.javascript_export(
                file,
                file.text(module)?,
                member,
                ModuleMode::Esm,
                role,
                work,
            ),
            StaticBindingKind::JavaScriptInstance { constructor }
            | StaticBindingKind::JavaScriptConstInstance { constructor } => {
                let class = self.javascript_target(
                    file,
                    constructor,
                    rift_core::ReferenceRole::Call,
                    work,
                )?;
                self.javascript_class_member(class, member, MemberMode::Instance, role, work)
            }
            StaticBindingKind::JavaScriptImport { .. } => {
                self.javascript_imported_member(file, binding, member, role, work)
            }
            StaticBindingKind::JavaScriptClassValue { .. } => self
                .javascript_binding_target(file, binding, role, work)
                .and_then(|target| self.javascript_object_member(target, member, role, work)),
            StaticBindingKind::Local => self
                .javascript_binding_target(file, binding, role, work)
                .and_then(|target| self.javascript_object_member(target, member, role, work))
                .or_else(|| {
                    if role == rift_core::ReferenceRole::Type {
                        return None;
                    }
                    self.javascript_destructured_name(file, binding, work)
                        .and_then(|owner| {
                            self.javascript_class_member(
                                owner,
                                member,
                                MemberMode::Instance,
                                role,
                                work,
                            )
                        })
                        .or_else(|| {
                            self.javascript_returned_member(file, binding, member, role, work)
                        })
                }),
            StaticBindingKind::JavaScriptRequire {
                module,
                export: None,
                require,
            } if self.builtin_require(file, require.start, work) => {
                let module = file.text(module)?;
                self.javascript_export(file, module, member, ModuleMode::CommonJs, role, work)
                    .or_else(|| {
                        self.javascript_module_value_member(
                            file, binding, module, member, role, work,
                        )
                    })
            }
            _ => None,
        }
    }

    /// The declared type of the name an object literal initializer spreads last, when that
    /// literal initializes `binding`: the spread name's written type or the type its place
    /// as a callback parameter gives it. In TypeScript a `let` keeps the type of its
    /// initializer through later writes; in JavaScript a binding written again has none.
    fn javascript_spread_owner(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let spread = *file.spread_values.get(&binding.name)?;
        if javascript_source(file.document.path.as_str())
            && file.javascript_binding_written(binding, work)
        {
            return None;
        }
        let spread_binding = js_binding(
            file,
            &self.javascript,
            file.text(spread)?,
            spread.start,
            rift_core::ReferenceRole::Read,
            work,
        )?;
        match file
            .declared_types
            .get(&spread_binding.name)
            .copied()
            .flatten()
        {
            Some(annotation) => self.javascript_declared_type(file, annotation, work),
            None => self.javascript_callback_parameter(file, spread_binding, work),
        }
    }

    /// A member of a required module's value: `module.exports = name` with `name` an object
    /// declared in that module, or `module.exports = { ... }`.
    fn javascript_module_value_member(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        module: &'source str,
        member: &'source str,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let exports = super::javascript_exports::Exports {
            files: &self.files,
            modes: &self.javascript,
            names: None,
        };
        if let Some(value) = exports.resolve_module_value(file, module, role, work) {
            let owner = self.javascript_export_value(value, role, work)?;
            return self.javascript_object_member(owner, member, role, work);
        }
        if member.contains('.') {
            return None;
        }
        let supplied = exports.imported(file, module, work)?;
        self.javascript_commonjs_literal_member(file, binding, supplied, member, role, work)
    }

    /// Reads a checked direct export's property or calls the local function its settled pair
    /// names.
    fn javascript_commonjs_literal_member(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        supplied: &File<'source>,
        member: &str,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if !matches!(
            role,
            rift_core::ReferenceRole::Read | rift_core::ReferenceRole::Call
        ) || file.document.facts.language().name != "javascript"
            || self.javascript.mode(supplied.document.path.as_str(), work) != ModuleMode::CommonJs
            || supplied.facts.has_javascript_module_syntax()
        {
            return None;
        }
        let literal = supplied.facts.commonjs_export()?.literal?;
        let owner = supplied.qualified_target("default")?;
        if owner.symbol.kind != "object"
            || owner.symbol.syntax_element_kind != Some("object")
            || owner.symbol.body_range != Some(literal)
        {
            return None;
        }
        // A new literal member can be used only while the selected receiver stays unchanged.
        let target =
            self.javascript_object_member(owner, member, rift_core::ReferenceRole::Call, work)?;
        if target.symbol.syntax_element_kind != Some("pair") {
            return None;
        }
        let selected = if role == rift_core::ReferenceRole::Read {
            (javascript_final_pair(supplied, target, literal, work)
                || supplied.exported_pair_is_settled(target, work))
            .then_some(target)
        } else if file.javascript_binding_reassigned(binding) {
            None
        } else {
            self.javascript_final_pair_callable(supplied, target, literal, work)
        }?;
        self.javascript_direct_require_calls(file, supplied, member, work)
            .then_some(selected)
    }

    /// A whole-module binding can mutate the selected literal. Every such alias in the
    /// caller must stay inside direct named calls of the property being resolved.
    fn javascript_direct_require_calls(
        &self,
        file: &File<'source>,
        supplied: &File<'source>,
        property: &str,
        work: &mut Work,
    ) -> bool {
        let exports = super::javascript_exports::Exports {
            files: &self.files,
            modes: &self.javascript,
            names: None,
        };
        let mut receiver_names = BTreeSet::new();
        let mut receiver_ranges = BTreeSet::new();
        let mut requires = BTreeSet::new();
        for binding in file.facts.bindings() {
            if !work.charge() {
                return false;
            }
            let StaticBindingKind::JavaScriptRequire {
                module,
                export,
                require,
            } = binding.kind
            else {
                continue;
            };
            requires.insert(require);
            if export.is_some() {
                continue;
            }
            let Some(module) = file.text(module) else {
                return false;
            };
            if exports
                .imported(file, module, work)
                .is_none_or(|other| other.document.path != supplied.document.path)
            {
                continue;
            }
            let Some(name) = file.text(binding.name) else {
                return false;
            };
            if !self.builtin_require(file, require.start, work)
                || file.javascript_binding_reassigned(binding)
                || file.javascript_reassigned(name, Some(property))
            {
                return false;
            }
            receiver_names.insert(name);
            receiver_ranges.insert(binding.name);
        }
        file.javascript_closed_require_uses(
            &receiver_names,
            &receiver_ranges,
            &requires,
            property,
            work,
        )
    }

    fn javascript_imported_member(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        member: &'source str,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let StaticBindingKind::JavaScriptImport { module, export } = binding.kind else {
            return None;
        };
        let name = export
            .and_then(|range| file.text(range))
            .unwrap_or("default");
        let resolver = super::javascript_exports::Exports {
            files: &self.files,
            modes: &self.javascript,
            names: None,
        };
        match resolver.resolve(file, file.text(module)?, name, ModuleMode::Esm, role, work)? {
            super::javascript_exports::ResolvedExport::Module(imported) => {
                let imported = self.files.get(imported.path.as_str())?;
                self.javascript_export_value(
                    resolver.resolve_file(imported, member, ModuleMode::Esm, role, work)?,
                    role,
                    work,
                )
            }
            value @ super::javascript_exports::ResolvedExport::Declaration { .. } => {
                let target = self.javascript_export_value(value, role, work)?;
                self.javascript_object_member(target, member, role, work)
            }
        }
    }

    pub(super) fn builtin_require(
        &self,
        file: &File<'source>,
        position: u64,
        work: &mut Work,
    ) -> bool {
        self.builtin_commonjs_name(file, "require", position, work)
    }

    fn builtin_commonjs_name(
        &self,
        file: &File<'source>,
        name: &str,
        position: u64,
        work: &mut Work,
    ) -> bool {
        builtin_commonjs_name(file, &self.javascript, name, position, work)
    }

    fn javascript_commonjs_member(
        &self,
        file: &File<'source>,
        receiver: &str,
        member: &'source str,
        position: u64,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let name = match receiver {
            "exports" => member,
            "module" => member.strip_prefix("exports.")?,
            _ => return None,
        };
        if !self.builtin_commonjs_name(file, receiver, position, work) {
            return None;
        }
        let export = super::javascript_exports::Exports {
            files: &self.files,
            modes: &self.javascript,
            names: None,
        }
        .resolve_file(file, name, ModuleMode::CommonJs, role, work)?;
        self.javascript_export_value(export, role, work)
    }

    pub(super) fn edge_javascript_module(&self, path: &str) -> Option<&File<'source>> {
        super::javascript_exports::module_file(&self.files, &self.javascript, path, false)
    }

    fn javascript_export(
        &self,
        from: &File<'source>,
        module: &'source str,
        name: &'source str,
        mode: ModuleMode,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let resolved = super::javascript_exports::Exports {
            files: &self.files,
            modes: &self.javascript,
            names: None,
        }
        .resolve(from, module, name, mode, role, work)?;
        self.javascript_export_value(resolved, role, work)
    }

    fn javascript_export_value(
        &self,
        resolved: super::javascript_exports::ResolvedExport<'source>,
        role: rift_core::ReferenceRole,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        match resolved {
            super::javascript_exports::ResolvedExport::Declaration {
                target,
                member: Some(member),
            } if target.symbol.kind == "class" => {
                self.javascript_class_member(target, member, MemberMode::Static, role, work)
            }
            super::javascript_exports::ResolvedExport::Declaration {
                target,
                member: Some(member),
            } => self.javascript_object_member(target, member, role, work),
            super::javascript_exports::ResolvedExport::Declaration {
                target,
                member: None,
            } => Some(target),
            super::javascript_exports::ResolvedExport::Module(_) => None,
        }
    }
}

/// Proves this written key is direct and no later property can replace it.
fn javascript_final_pair(
    file: &File<'_>,
    member: Target<'_>,
    object: ByteRange,
    work: &mut Work,
) -> bool {
    let Some(name) = member.symbol.name_range.and_then(|range| file.text(range)) else {
        return false;
    };
    if name != member.symbol.name {
        return false;
    }
    let Some(tail) = file
        .text(ByteRange {
            start: member.symbol.item_range.end,
            end: object.end,
        })
        .and_then(|tail| tail.strip_suffix('}'))
    else {
        return false;
    };
    let mut comma = false;
    tail.chars().all(|character| {
        if !work.charge() {
            return false;
        }
        if character == ',' && !comma {
            comma = true;
            return true;
        }
        character.is_whitespace()
    })
}

impl File<'_> {
    pub(super) fn javascript_reassigned(&self, name: &str, member: Option<&str>) -> bool {
        self.assignments.names.contains(name)
            || member.is_some_and(|member| {
                self.assignments.unknown || self.assignments.members.contains(&(name, member))
            })
    }

    /// Every reference to a checked receiver must be a direct named call.
    fn javascript_closed_require_uses(
        &self,
        names: &BTreeSet<&str>,
        receivers: &BTreeSet<ByteRange>,
        requires: &BTreeSet<ByteRange>,
        property: &str,
        work: &mut Work,
    ) -> bool {
        for reference in self.facts.references() {
            if !work.charge() {
                return false;
            }
            let range = reference.target;
            let Some(name) = self.text(range) else {
                return false;
            };
            // An unsupported use can acquire and mutate the same module value.
            if name == "require" && !requires.contains(&range) {
                return false;
            }
            if !names.contains(name) {
                continue;
            }
            match self.binding(name, range.start, |_| true, work) {
                Binding::Found(other) if !receivers.contains(&other.name) => continue,
                Binding::Found(_) => {}
                _ => return false,
            }
            if !self
                .callbacks
                .has_named_member_call(range, work, |member| self.text(member) == Some(property))
            {
                return false;
            }
        }
        !work.exhausted
    }
}

impl File<'_> {
    /// Whether an assignment writes `binding` with a value that may not be callable: a write
    /// of its name inside its scope that no inner binding of the same name takes, whose
    /// value is not a function written in place. A write whose binding is ambiguous counts.
    pub(super) fn javascript_binding_written(
        &self,
        binding: &StaticBinding,
        work: &mut Work,
    ) -> bool {
        let Some(name) = self.text(binding.name) else {
            return true;
        };
        let Some(writes) = self.assignments.name_writes.get(name) else {
            return false;
        };
        let bindings = self.by_name.get(name).map_or(&[][..], Vec::as_slice);
        writes.iter().any(|&start| {
            // A function written in place keeps the binding callable (D35).
            if !binding.scope.contains(start) || self.function_writes.contains(&start) {
                return false;
            }
            // The innermost scope holding the write decides which binding it writes.
            let mut innermost: Option<rift_syntax::ByteRange> = None;
            let mut takes = false;
            for candidate in bindings {
                if !work.charge() {
                    return true;
                }
                if !candidate.scope.contains(start) {
                    continue;
                }
                match innermost {
                    Some(scope) if scope == candidate.scope => {
                        takes |= candidate.name == binding.name;
                    }
                    Some(scope) if !scope.encloses(candidate.scope) => {}
                    _ => {
                        innermost = Some(candidate.scope);
                        takes = candidate.name == binding.name;
                    }
                }
            }
            takes
        })
    }
}

impl File<'_> {
    /// Whether an assignment inside the scope of `binding` writes its name again. A
    /// write to an inner binding of the same name counts too: the check only refuses.
    pub(super) fn javascript_binding_reassigned(&self, binding: &StaticBinding) -> bool {
        self.text(binding.name)
            .and_then(|name| self.assignments.name_writes.get(name))
            .is_some_and(|writes| writes.iter().any(|&start| binding.scope.contains(start)))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum MemberMode {
    Instance,
    Static,
}

/// Whether `name` at `position` is the `CommonJS` builtin: the file runs as `CommonJS` and
/// no binding in scope takes the name.
pub(super) fn builtin_commonjs_name<'source>(
    file: &File<'source>,
    modes: &PackageModes<'source>,
    name: &str,
    position: u64,
    work: &mut Work,
) -> bool {
    (modes.mode(file.document.path.as_str(), work) == ModuleMode::CommonJs
        || typescript_commonjs(file.document.path.as_str()))
        && !file.blocked(position, work)
        && matches!(
            file.binding(name, position, |_| true, work),
            Binding::Missing
        )
        && !file.javascript_reassigned(name, None)
}

pub(super) fn js_binding<'source>(
    file: &File<'source>,
    modes: &PackageModes<'source>,
    name: &str,
    position: u64,
    role: rift_core::ReferenceRole,
    work: &mut Work,
) -> Option<&'source StaticBinding> {
    if file.blocked(position, work) {
        return None;
    }
    let mut best: Option<&StaticBinding> = None;
    let mut ambiguous = false;
    let bound = file.innermost_bindings(
        name,
        position,
        work,
        |binding| {
            // A type a documentation comment imports names no value.
            if role != rift_core::ReferenceRole::Type
                && matches!(binding.kind, StaticBindingKind::JavaScriptTypeImport { .. })
            {
                return false;
            }
            let symbol = file.target_at(binding.name).map(|target| target.symbol);
            if role == rift_core::ReferenceRole::Type {
                let javascript = javascript_source(file.document.path.as_str());
                // A JavaScript type names a class value; a name bound by `require` can hold one.
                let required_value =
                    matches!(binding.kind, StaticBindingKind::JavaScriptRequire { .. })
                        && javascript;
                // A TypeScript function names no type: an interface of the same name does.
                required_value
                    || symbol.is_none_or(|symbol| {
                        symbol.kind != "variable" && (javascript || symbol.kind != "function")
                    })
            } else {
                symbol.is_none_or(|symbol| !matches!(symbol.kind, "interface" | "type_alias"))
            }
        },
        |binding, first| {
            ambiguous = !first;
            if first {
                best = Some(binding);
            }
        },
    )?;
    if !bound {
        return None;
    }
    let binding = best?;
    let deferred = modes
        .scopes
        .get(file.document.path.as_str())
        .and_then(|scopes| scopes.deferred.resolve(position, position.checked_add(1)?))
        .is_some_and(|function| !function.encloses(binding.name));
    let available =
        binding.available_from <= position || role == rift_core::ReferenceRole::Type || deferred;
    let written =
        role == rift_core::ReferenceRole::Call && file.javascript_binding_written(binding, work);
    (!ambiguous && available && !written).then_some(binding)
}
