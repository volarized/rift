//! Resolves explicit JavaScript module paths and lexical bindings.

use std::collections::BTreeMap;

use rift_syntax::{StaticBinding, StaticBindingKind, StaticCall, StaticCallee, SyntaxExportKind};

use super::{Binding, File, Index, Target, Work};
use crate::NamespaceInput;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ModuleMode {
    Esm,
    CommonJs,
    Unknown,
}

/// Module-mode declarations captured from the nearest package manifest.
pub(super) struct PackageModes(BTreeMap<String, ModuleMode>);

impl PackageModes {
    pub(super) fn new(namespace: &NamespaceInput<'_>, work: &mut Work) -> Option<Self> {
        let mut modes = BTreeMap::new();
        for source in namespace.files().iter().chain(namespace.context_sources()) {
            if !work.charge() {
                return None;
            }
            let Some(directory) = manifest_directory(source.path().as_str()) else {
                continue;
            };
            let mode = if source.text().len() <= namespace.syntax().source_bytes_max() {
                package_mode(source.text())
            } else {
                ModuleMode::Unknown
            };
            modes.entry(directory.to_owned()).or_insert(mode);
        }
        Some(Self(modes))
    }

    fn mode(&self, path: &str, work: &mut Work) -> ModuleMode {
        match path.rsplit_once('.').map(|(_, extension)| extension) {
            Some("mjs") => return ModuleMode::Esm,
            Some("cjs") => return ModuleMode::CommonJs,
            Some("js") => {}
            _ => return ModuleMode::Unknown,
        }
        let mut directory = path.rsplit_once('/').map_or("", |(directory, _)| directory);
        loop {
            if !work.charge() {
                return ModuleMode::Unknown;
            }
            if let Some(&mode) = self.0.get(directory) {
                return mode;
            }
            if directory.is_empty() {
                return ModuleMode::Unknown;
            }
            directory = directory.rsplit_once('/').map_or("", |(parent, _)| parent);
        }
    }
}

fn manifest_directory(path: &str) -> Option<&str> {
    if path == "package.json" {
        Some("")
    } else {
        path.strip_suffix("/package.json")
    }
}

fn package_mode(text: &str) -> ModuleMode {
    match serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(text) {
        Ok(manifest) => match manifest.get("type") {
            Some(value) if value.as_str() == Some("module") => ModuleMode::Esm,
            Some(value) if value.as_str() == Some("commonjs") => ModuleMode::CommonJs,
            _ => ModuleMode::Unknown,
        },
        Err(_) => ModuleMode::Unknown,
    }
}

impl<'source> Index<'source> {
    pub(super) fn javascript_call(
        &self,
        file: &File<'source>,
        call: &StaticCall,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        match call.callee {
            StaticCallee::Direct => {
                let name = file.text(call.callee_range)?;
                let binding = js_binding(file, name, call.range.start, None, work)?;
                self.javascript_binding_target(file, binding, work)?
                    .callable()
            }
            StaticCallee::Member { receiver, member } => {
                let name = file.text(receiver)?;
                let member = file.text(member)?;
                let binding = js_binding(file, name, call.range.start, Some(member), work)?;
                self.javascript_member(file, binding, member, work)?
                    .callable()
            }
            _ => None,
        }
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
        self.javascript_binding_target(file, binding, work)
    }

    fn javascript_binding_target(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        match binding.kind {
            StaticBindingKind::Local => file.target_at(binding.name),
            StaticBindingKind::JavaScriptImport { module, export } => {
                let path = file.text(module)?;
                let name = match export {
                    Some(range) => file.text(range)?,
                    None => "default",
                };
                if self.javascript.mode(file.document.path.as_str(), work) == ModuleMode::CommonJs {
                    return None;
                }
                self.javascript_export(file, path, name, ModuleMode::Esm, work)
            }
            StaticBindingKind::JavaScriptRequire {
                module,
                export: Some(export),
                require,
            } if self.builtin_require(file, require.start, work) => self.javascript_export(
                file,
                file.text(module)?,
                file.text(export)?,
                ModuleMode::CommonJs,
                work,
            ),
            _ => None,
        }
    }

    fn javascript_member(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        member: &'source str,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        match binding.kind {
            StaticBindingKind::JavaScriptInstance { constructor } => {
                let constructor_name = file.text(constructor)?;
                let constructor_binding =
                    js_binding(file, constructor_name, constructor.start, None, work)?;
                let class = file.target_at(constructor_binding.name)?;
                if !matches!(constructor_binding.kind, StaticBindingKind::Local) {
                    return None;
                }
                class_member(file, class, member)
            }
            StaticBindingKind::JavaScriptRequire {
                module,
                export: None,
                require,
            } if self.builtin_require(file, require.start, work) => {
                self.javascript_export(file, file.text(module)?, member, ModuleMode::CommonJs, work)
            }
            _ => None,
        }
    }

    fn builtin_require(&self, file: &File<'source>, position: u64, work: &mut Work) -> bool {
        self.javascript.mode(file.document.path.as_str(), work) == ModuleMode::CommonJs
            && !file.blocked(position, work)
            && matches!(
                file.binding("require", position, |_| true, work),
                Binding::Missing
            )
            && !file.reassigned("require", None)
    }

    fn javascript_export<'index>(
        &'index self,
        mut file: &'index File<'source>,
        mut module: &'source str,
        mut name: &'source str,
        mode: ModuleMode,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        // The remaining pass budget also bounds cycles through explicit re-exports.
        // A visited module ends a cycle before consuming the rest of the pass.
        let mut visited = std::collections::BTreeSet::new();
        loop {
            if !work.charge() {
                return None;
            }
            let path = relative_path(file.document.path.as_str(), module, work)?;
            file = self.files.get(path.as_str())?;
            if !visited.insert((file.document.path.as_str(), name))
                || file.document.facts.language().name != "javascript"
            {
                return None;
            }
            let actual = self.javascript.mode(&path, work);
            match mode {
                ModuleMode::Esm if actual != ModuleMode::CommonJs => {}
                ModuleMode::CommonJs if actual == ModuleMode::CommonJs => {}
                _ => return None,
            }
            let export = selected_export(file, name, mode, work)?;
            match export {
                Export::Local { name, position } => {
                    let binding = js_binding(file, name, position, None, work)?;
                    return match binding.kind {
                        StaticBindingKind::Local => file.target_at(binding.name),
                        _ => None,
                    };
                }
                Export::Declaration(range) => return file.target_at(range),
                Export::Reexport {
                    module: next_module,
                    name: next_name,
                } => {
                    module = next_module;
                    name = next_name;
                }
            }
        }
    }
}

fn js_binding<'source>(
    file: &File<'source>,
    name: &str,
    position: u64,
    member: Option<&str>,
    work: &mut Work,
) -> Option<&'source StaticBinding> {
    if file.blocked(position, work) || file.reassigned(name, member) {
        return None;
    }
    match file.binding(name, position, |_| true, work) {
        Binding::Found(binding) => Some(binding),
        Binding::Missing | Binding::Blocked => None,
    }
}

enum Export<'source> {
    Local {
        name: &'source str,
        position: u64,
    },
    Declaration(rift_syntax::ByteRange),
    Reexport {
        module: &'source str,
        name: &'source str,
    },
}

fn selected_export<'source>(
    file: &File<'source>,
    name: &str,
    mode: ModuleMode,
    work: &mut Work,
) -> Option<Export<'source>> {
    let mut found = None;
    for binding in file.document.facts.export_bindings()? {
        if !work.charge() {
            return None;
        }
        let authored = file.text(binding.range)?;
        if !matches_export_form(authored, mode) || binding.type_only || binding.container.is_some()
        {
            continue;
        }
        let exported = match binding.kind {
            SyntaxExportKind::Named => binding.exported.and_then(|range| file.text(range)),
            SyntaxExportKind::Default => Some("default"),
            _ => None,
        };
        if exported != Some(name) {
            continue;
        }
        let local = binding.local?;
        let local_name = file.text(local)?;
        let target = match binding.source {
            Some(source) => Export::Reexport {
                module: literal_contents(file.text(source)?)?,
                name: local_name,
            },
            None if file.target_at(local).is_some()
                && !file.blocked(local.start, work)
                && !file.reassigned(local_name, None) =>
            {
                Export::Declaration(local)
            }
            None => Export::Local {
                name: local_name,
                position: local.start,
            },
        };
        if found.replace(target).is_some() {
            return None;
        }
    }
    found
}

fn matches_export_form(source: &str, mode: ModuleMode) -> bool {
    let source = source.trim_start();
    let esm = source.strip_prefix("export").is_some_and(|rest| {
        rest.starts_with(|character: char| {
            character.is_whitespace() || matches!(character, '{' | '*' | '/')
        })
    });
    match mode {
        ModuleMode::Esm => esm,
        ModuleMode::CommonJs => {
            source.starts_with("module.exports") || source.starts_with("exports.")
        }
        ModuleMode::Unknown => false,
    }
}

fn literal_contents(source: &str) -> Option<&str> {
    let quote = source.chars().next()?;
    if !matches!(quote, '\'' | '"') {
        return None;
    }
    source.strip_prefix(quote)?.strip_suffix(quote)
}

fn relative_path(from: &str, module: &str, work: &mut Work) -> Option<String> {
    let relative = module.starts_with("./") || module.starts_with("../");
    let explicit_extension = [".js", ".mjs", ".cjs"]
        .iter()
        .any(|suffix| module.ends_with(suffix));
    if !relative || !explicit_extension || module.contains(['\\', '?', '#', '%']) {
        return None;
    }
    let mut segments = from
        .rsplit_once('/')
        .map_or_else(Vec::new, |(directory, _)| directory.split('/').collect());
    for segment in module.split('/') {
        if !work.charge() {
            return None;
        }
        match segment {
            "." => {}
            ".." => {
                segments.pop()?;
            }
            "" => return None,
            name => segments.push(name),
        }
    }
    Some(segments.join("/"))
}

fn class_member<'source>(
    file: &File<'source>,
    class: Target<'source>,
    member: &str,
) -> Option<Target<'source>> {
    if class.symbol.kind != "class" {
        return None;
    }
    let body = class.symbol.body_range?;
    let header = file.text(rift_syntax::ByteRange {
        start: class.symbol.range.start,
        end: body.start,
    })?;
    let header = header.strip_prefix("class")?.trim();
    if header != class.symbol.name {
        return None;
    }
    let qualified = |name| format!("{}.{}", class.symbol.qualified_name, name);
    if file
        .by_qualified_name
        .contains_key(qualified("constructor").as_str())
    {
        return None;
    }
    if file.assignments.may_replace_method(member) {
        return None;
    }
    let method = file.qualified_target(&qualified(member))?;
    if method.symbol.kind != "method" || !body.encloses(method.symbol.range) {
        return None;
    }
    let name = method.symbol.name_range?;
    let start = method.symbol.range.start;
    let has_modifier = start < name.start
        && !file
            .text(rift_syntax::ByteRange {
                start,
                end: name.start,
            })?
            .trim()
            .is_empty();
    (!has_modifier).then_some(method)
}
