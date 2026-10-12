use super::{
    BTreeMap, BTreeSet, NamespaceInput, PackageMetadata, PreparedNamespace, ProjectPath,
    SelectedFile, SyntaxExportKind, declaration_anchors_with_public, export_subpath_is_valid,
    observations, selected_source_is_valid,
};
use crate::NamespaceModule;
use std::collections::VecDeque;
use std::path::Path;

pub(super) fn prepare(
    input: &NamespaceInput<'_>,
    metadata: Option<&PackageMetadata>,
    selected: &[SelectedFile<'_>],
    observation: &NamespaceModule<'_>,
    implementation: &SelectedFile<'_>,
    prepared: &mut PreparedNamespace,
) {
    let Some(metadata) = metadata
        .filter(|metadata| matches!(metadata.module_type.as_deref(), None | Some("commonjs")))
    else {
        prepared.unresolved_exports = true;
        return;
    };
    if observation.export_conditions() != ["node", "require"]
        || is_mjs(implementation.path)
        || !entry_matches(metadata, observation)
    {
        prepared.unresolved_exports = true;
        return;
    }
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
        let public = checked_names(file);
        let anchors = declaration_anchors_with_public(
            input,
            file.syntax,
            file.source,
            Some(&module),
            &public,
        );
        if prepared.anchors.get(path.as_str()).is_some_and(|previous| {
            previous
                .iter()
                .any(|(name, identity)| anchors.get(name) != Some(identity))
        }) {
            prepared.unresolved_exports = true;
            continue;
        }
        prepared.anchors.insert(path.as_str().to_owned(), anchors);
        prepared
            .public_names
            .entry(path.as_str().to_owned())
            .or_default()
            .extend(public);
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
            let Some(target) = relative_target(file.path, &relative) else {
                prepared.unresolved_exports = true;
                continue;
            };
            // Resolved CommonJS filenames identify distinct defining modules.
            let target_module = format!("{}/{}", metadata.name, target.as_str());
            prepared.unresolved_exports = true;
            if !inventory.contains_key(&target) {
                continue;
            }
            if queued.insert(target.clone()) {
                pending.push_back((target, target_module));
            }
        }
    }
}

fn entry_matches(metadata: &PackageMetadata, observation: &NamespaceModule<'_>) -> bool {
    let target = if let Some(exports) = &metadata.exports {
        exports
            .observed_targets(
                &metadata.name,
                observation.module(),
                observation.export_conditions(),
            )
            .map(|(_, implementation)| implementation)
    } else if observation.module() == metadata.name {
        metadata.main.as_deref()
    } else {
        None
    };
    target.and_then(observations::target_path).as_ref() == Some(observation.implementation().path())
}

fn checked_names(file: &SelectedFile<'_>) -> BTreeSet<String> {
    let bindings = file.syntax.export_bindings().unwrap_or_default();
    file.syntax
        .symbols()
        .iter()
        .filter(|symbol| {
            symbol.node_kind == Some("assignment_expression")
                && bindings.iter().any(|binding| {
                    binding.kind == SyntaxExportKind::Named
                        && binding.source.is_none()
                        && binding.range == symbol.item_range
                        && binding.local == symbol.name_range
                        && binding.exported == symbol.name_range
                        && binding.container == symbol.container
                })
        })
        .map(|symbol| symbol.qualified_name.clone())
        .collect()
}

fn relative_target(path: &ProjectPath, relative: &str) -> Option<ProjectPath> {
    let relative = relative.strip_prefix("./")?;
    if !export_subpath_is_valid(relative) {
        return None;
    }
    let target = path.as_str().rsplit_once('/').map_or_else(
        || relative.to_owned(),
        |(directory, _)| format!("{directory}/{relative}"),
    );
    ProjectPath::new(target).ok()
}

fn is_mjs(path: &ProjectPath) -> bool {
    let path = Path::new(path.as_str());
    path.extension().is_some_and(|extension| extension == "mjs")
        || path.file_name().is_some_and(|name| name == ".mjs")
}
