use super::{
    BTreeMap, BTreeSet, Entry, NamespaceInput, PackageFiles, PackageMetadata, PreparedNamespace,
    ProjectPath, SelectedFile, SyntaxExportKind, declaration_anchors_with_public, export_container,
    export_subpath_is_valid, observations, selected_source_is_valid,
};
use crate::NamespaceModule;
use std::collections::VecDeque;
use std::path::Path;

pub(super) fn prepare(
    input: &NamespaceInput<'_>,
    files: &PackageFiles<'_>,
    metadata: Option<&PackageMetadata>,
    selected: &[SelectedFile<'_>],
    observation: &NamespaceModule<'_>,
    implementation: &SelectedFile<'_>,
    prepared: &mut PreparedNamespace,
) {
    let Some(metadata) =
        metadata.filter(|metadata| admits(files, metadata, observation, implementation))
    else {
        prepared.unresolved_exports = true;
        return;
    };
    let mut pending =
        VecDeque::from([(implementation.path.clone(), observation.module().to_owned())]);
    let inventory = selected
        .iter()
        .map(|file| (file.path, file))
        .collect::<BTreeMap<_, _>>();
    let mut queued = BTreeSet::from([implementation.path.clone()]);
    while let Some((path, module)) = pending.pop_front() {
        let Some(file) = inventory.get(&path).copied().filter(|file| {
            file.syntax.language().name == "javascript"
                && !is_mjs(file.path)
                && selected_source_is_valid(input, file)
        }) else {
            prepared.unresolved_exports = true;
            continue;
        };
        if !admit_file(input, file, &module, None, prepared) {
            prepared.unresolved_exports = true;
            continue;
        }
        if file
            .syntax
            .static_facts()
            .and_then(rift_syntax::StaticFacts::commonjs_export)
            .is_some_and(|export| export.synchronized)
        {
            prepared.commonjs_modules.insert(path.as_str().to_owned());
        }
        let Some(bindings) = file.syntax.export_bindings() else {
            prepared.unresolved_exports = true;
            continue;
        };
        for binding in bindings
            .iter()
            .filter(|binding| binding.kind == SyntaxExportKind::All)
        {
            let Some(relative) = binding
                .source
                .and_then(|range| observations::literal_module_name(file.source, range))
            else {
                prepared.unresolved_exports = true;
                continue;
            };
            let Some(target) =
                relative_target(file.path, &relative, |path| inventory.contains_key(path))
            else {
                prepared.unresolved_exports = true;
                continue;
            };
            // Resolved CommonJS filenames identify distinct defining modules.
            let target_module = format!("{}/{}", metadata.name, target.as_str());
            prepared.unresolved_exports = true;
            if queued.insert(target.clone()) {
                pending.push_back((target, target_module));
            }
        }
    }
}

/// Whether the observation is the package's own `require` entry of a `CommonJS` file.
pub(super) fn admits(
    files: &PackageFiles<'_>,
    metadata: &PackageMetadata,
    observation: &NamespaceModule<'_>,
    implementation: &SelectedFile<'_>,
) -> bool {
    matches!(metadata.module_type.as_deref(), None | Some("commonjs"))
        && observation.export_conditions() == ["node", "require"]
        && !is_mjs(implementation.path)
        && entry_matches(files, metadata, observation)
}

pub(crate) fn admit_file(
    input: &NamespaceInput<'_>,
    file: &SelectedFile<'_>,
    module: &str,
    required: Option<&str>,
    prepared: &mut PreparedNamespace,
) -> bool {
    let path = file.path.as_str();
    let public = checked_names(file);
    let anchors = declaration_anchors_with_public(input, file, Some(module), &public);
    if required.is_some_and(|name| !anchors.contains_key(name))
        || prepared.anchors.get(path).is_some_and(|previous| {
            previous
                .iter()
                .any(|(name, identity)| anchors.get(name) != Some(identity))
        })
    {
        return false;
    }
    prepared.anchors.insert(path.to_owned(), anchors);
    prepared
        .public_names
        .entry(path.to_owned())
        .or_default()
        .extend(public);
    true
}

fn entry_matches(
    files: &PackageFiles<'_>,
    metadata: &PackageMetadata,
    observation: &NamespaceModule<'_>,
) -> bool {
    let target = if let Some(exports) = &metadata.exports {
        exports
            .observed_targets(
                &metadata.name,
                observation.module(),
                observation.export_conditions(),
            )
            .and_then(|(_, implementation)| observations::target_path(implementation))
    } else if observation.module() == metadata.name {
        match files.runtime(metadata.main.as_deref(), None) {
            Entry::File(path) => Some(path),
            Entry::Unknown | Entry::Absent => None,
        }
    } else {
        None
    };
    target.as_ref() == Some(observation.implementation().path())
}

fn checked_names(file: &SelectedFile<'_>) -> BTreeSet<String> {
    let bindings = file.syntax.export_bindings().unwrap_or_default();
    file.syntax
        .symbols()
        .iter()
        .filter(|symbol| {
            symbol.syntax_element_kind == Some("assignment_expression")
                && bindings.iter().any(|binding| {
                    binding.kind == SyntaxExportKind::Named
                        && binding.source.is_none()
                        && binding.range == symbol.item_range
                        && binding.local == symbol.name_range
                        && binding.exported == symbol.name_range
                        && binding.container.as_deref() == export_container(symbol)
                })
        })
        .map(|symbol| symbol.qualified_name.clone())
        .collect()
}

pub(crate) fn relative_target(
    path: &ProjectPath,
    relative: &str,
    exists: impl Fn(&ProjectPath) -> bool,
) -> Option<ProjectPath> {
    let relative = relative.strip_prefix("./")?;
    if !export_subpath_is_valid(relative) {
        return None;
    }
    let target = path.as_str().rsplit_once('/').map_or_else(
        || relative.to_owned(),
        |(directory, _)| format!("{directory}/{relative}"),
    );
    let exact = ProjectPath::new(&target).ok()?;
    if exists(&exact) {
        return Some(exact);
    }
    if Path::new(relative).extension().is_some() {
        return None;
    }
    let javascript = ProjectPath::new(format!("{target}.js")).ok()?;
    exists(&javascript).then_some(javascript)
}

fn is_mjs(path: &ProjectPath) -> bool {
    let path = Path::new(path.as_str());
    path.extension().is_some_and(|extension| extension == "mjs")
        || path.file_name().is_some_and(|name| name == ".mjs")
}
