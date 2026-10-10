use super::{
    BTreeMap, BTreeSet, ByteRange, LogicalDeclaration, NamespaceInput, PackageMetadata,
    PreparedNamespace, ProjectPath, SelectedFile, SymbolOwner, SyntaxExportKind, binding_name,
    map_files, prepare_file, selected_source_is_valid,
};
use crate::NamespaceModule;

pub(super) fn prepare(
    input: &NamespaceInput<'_>,
    metadata: Option<&PackageMetadata>,
    selected: &[SelectedFile<'_>],
    prepared: &mut PreparedNamespace,
) {
    let mut conflicts = BTreeSet::new();
    for observation in input.modules() {
        let Some(implementation) = selected_file(input, selected, observation.implementation())
        else {
            prepared.unresolved_exports = true;
            continue;
        };
        if implementation.syntax.language().name != "javascript" {
            prepared.unresolved_exports = true;
            continue;
        }
        for source in observation.declarations() {
            let Some(declarations) = selected_file(input, selected, *source) else {
                prepared.unresolved_exports = true;
                continue;
            };
            if declarations.syntax.language().name != "typescript" {
                prepared.unresolved_exports = true;
                continue;
            }
            let containers = if let Some(metadata) = metadata {
                if !package_observation(metadata, observation, declarations) {
                    prepared.unresolved_exports = true;
                    continue;
                }
                vec![None]
            } else if matches!(input.owner(), SymbolOwner::Runtime { runtime, .. } if runtime == "node")
                && observation.export_conditions().is_empty()
            {
                ambient_containers(declarations, observation.module())
                    .into_iter()
                    .map(Some)
                    .collect()
            } else {
                Vec::new()
            };
            if containers.is_empty() {
                prepared.unresolved_exports = true;
                continue;
            }
            if !prepare_file(input, implementation, observation.module(), prepared)
                || !prepare_file(input, declarations, observation.module(), prepared)
            {
                continue;
            }
            for container in containers {
                let Some(types) = prepared.anchors.get(declarations.path.as_str()) else {
                    continue;
                };
                let Some(targets) = prepared.anchors.get(implementation.path.as_str()) else {
                    continue;
                };
                let mapped = map_files(declarations, implementation, types, targets, container);
                merge_mappings(prepared, declarations.path.as_str(), mapped, &mut conflicts);
            }
        }
    }
    for (path, name) in conflicts {
        if let Some(mappings) = prepared.mappings.get_mut(&path) {
            mappings.remove(&name);
        }
    }
}

fn selected_file<'selected, 'source>(
    input: &NamespaceInput<'_>,
    selected: &'selected [SelectedFile<'source>],
    source: crate::PackageSource<'_>,
) -> Option<&'selected SelectedFile<'source>> {
    let mut matches = selected.iter().filter(|file| file.path == source.path());
    let file = matches.next()?;
    (matches.next().is_none()
        && file.source == source.text()
        && selected_source_is_valid(input, file))
    .then_some(file)
}

fn package_observation(
    metadata: &PackageMetadata,
    observation: &NamespaceModule<'_>,
    declarations: &SelectedFile<'_>,
) -> bool {
    let conditions = observation.export_conditions();
    if conditions.contains(&"import") && conditions.contains(&"require") {
        return false;
    }
    if conditions.iter().any(|condition| {
        !matches!(
            *condition,
            "import" | "require" | "node" | "browser" | "default"
        )
    }) || metadata.types_versions.is_some()
    {
        return false;
    }
    let targets = if let Some(exports) = &metadata.exports {
        if conditions.is_empty() {
            return false;
        }
        exports.observed_targets(&metadata.name, observation.module(), conditions)
    } else if observation.module() == metadata.name {
        let implementation = match (&metadata.main, &metadata.module) {
            (Some(main), Some(module)) if main != module => {
                if conditions == ["import"] {
                    Some(module.as_str())
                } else if conditions == ["require"] {
                    Some(main.as_str())
                } else {
                    None
                }
            }
            (Some(main), _) => Some(main.as_str()),
            (_, Some(module)) => Some(module.as_str()),
            _ => None,
        };
        metadata
            .types
            .as_deref()
            .or(metadata.typings.as_deref())
            .zip(implementation)
    } else {
        None
    };
    targets.is_some_and(|(types, implementation)| {
        target_path(types).as_ref() == Some(declarations.path)
            && target_path(implementation).as_ref() == Some(observation.implementation().path())
    })
}

fn target_path(target: &str) -> Option<ProjectPath> {
    ProjectPath::new(target.strip_prefix("./").unwrap_or(target)).ok()
}

fn ambient_containers<'source>(file: &'source SelectedFile<'_>, module: &str) -> Vec<&'source str> {
    let declarations = file
        .syntax
        .symbols()
        .iter()
        .filter(|symbol| {
            symbol.kind == "namespace"
                && symbol.container.is_none()
                && symbol
                    .name_range
                    .and_then(|range| literal_module_name(file.source, range))
                    .is_some()
        })
        .collect::<Vec<_>>();
    let mut pending = vec![module.to_owned()];
    let mut visited = BTreeSet::new();
    let mut containers = BTreeSet::new();
    while let Some(module) = pending.pop() {
        if !visited.insert(module.clone()) {
            continue;
        }
        for declaration in &declarations {
            if declaration
                .name_range
                .and_then(|range| literal_module_name(file.source, range))
                .as_deref()
                != Some(module.as_str())
            {
                continue;
            }
            containers.insert(declaration.qualified_name.as_str());
            for binding in file.syntax.export_bindings().unwrap_or_default() {
                if binding.kind == SyntaxExportKind::All
                    && binding.container.as_deref() == Some(declaration.qualified_name.as_str())
                    && let Some(target) = binding
                        .source
                        .and_then(|range| binding_name(file.source, range))
                {
                    pending.push(target);
                }
            }
        }
    }
    containers.into_iter().collect()
}

fn literal_module_name(source: &str, range: ByteRange) -> Option<String> {
    let start = usize::try_from(range.start).ok()?;
    let end = usize::try_from(range.end).ok()?;
    let token = source.get(start..end)?;
    matches!(token.as_bytes().first(), Some(b'\'' | b'"'))
        .then(|| binding_name(source, range))
        .flatten()
}

fn merge_mappings(
    prepared: &mut PreparedNamespace,
    path: &str,
    mapped: BTreeMap<String, LogicalDeclaration>,
    conflicts: &mut BTreeSet<(String, String)>,
) {
    let accepted = prepared.mappings.entry(path.to_owned()).or_default();
    for (name, declaration) in mapped {
        if accepted.get(&name).is_some_and(|previous| {
            previous.identity != declaration.identity || previous.language != declaration.language
        }) {
            prepared.unresolved_exports = true;
            conflicts.insert((path.to_owned(), name));
        } else {
            accepted.insert(name, declaration);
        }
    }
}

#[cfg(test)]
mod tests {
    use rift_syntax::{SyntaxProvider, SyntaxSource, TypeScriptDialect, TypeScriptSyntaxProvider};

    use super::{SelectedFile, ambient_containers};

    #[test]
    fn ambient_observation_requires_literal_module_and_recorded_reexport() {
        for (source, expected) in [
            (
                "declare module \"fs\" { export function readFile(): void; }",
                1,
            ),
            (
                "declare module \"fs\" { export * from \"node:fs\"; } declare module \"node:fs\" { export function readFile(): void; }",
                2,
            ),
            (
                "declare namespace fs { export function readFile(): void; }",
                0,
            ),
            (
                "namespace outer { export namespace fs { export function readFile() {} } }",
                0,
            ),
            (
                "declare module \"node:fs\" { export function readFile(): void; }",
                0,
            ),
        ] {
            let path = rift_core::ProjectPath::new("fs.d.ts").expect("declaration path");
            let facts = TypeScriptSyntaxProvider::new(TypeScriptDialect::TypeScript)
                .analyze(
                    SyntaxSource {
                        path: &path,
                        text: source,
                    },
                    rift_syntax::SyntaxLimits::default(),
                )
                .expect("ambient source parses")
                .into_facts();
            let selected = SelectedFile {
                path: &path,
                source,
                syntax: &facts,
            };
            assert_eq!(
                ambient_containers(&selected, "fs").len(),
                expected,
                "{source}"
            );
        }
    }
}
