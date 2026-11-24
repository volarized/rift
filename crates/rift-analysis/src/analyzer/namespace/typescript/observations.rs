use super::{
    BTreeMap, BTreeSet, ByteRange, Entry, LogicalDeclaration, NamespaceInput, PackageFiles,
    PackageMetadata, PreparedNamespace, ProjectPath, SelectedFile, SymbolOwner, SyntaxExportKind,
    binding_name, commonjs, map_files, prepare_file, selected_source_is_valid,
};
use crate::NamespaceModule;

pub(super) fn prepare(
    input: &NamespaceInput<'_>,
    files: &PackageFiles<'_>,
    metadata: Option<&PackageMetadata>,
    selected: &[SelectedFile<'_>],
    imported: &BTreeSet<&str>,
    prepared: &mut PreparedNamespace,
) {
    let mut conflicts = BTreeSet::new();
    for observation in input.modules() {
        // A build observed under `import` holds the module; the decision is made, not unknown.
        if observation.declarations().is_empty()
            && !observation.export_conditions().contains(&"import")
            && imported.contains(observation.module())
        {
            continue;
        }
        let Some(implementation) = selected_file(input, selected, observation.implementation())
        else {
            prepared.unresolved_exports = true;
            continue;
        };
        if implementation.syntax.language().name != "javascript" {
            prepared.unresolved_exports = true;
            continue;
        }
        if observation.declarations().is_empty() {
            commonjs::prepare(
                input,
                files,
                metadata,
                selected,
                observation,
                implementation,
                prepared,
            );
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
                if !package_observation(files, metadata, observation, declarations) {
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

/// The files admitted resolver observations give each module, by language.
pub(super) fn observed_files<'input>(
    input: &NamespaceInput<'input>,
    files: &PackageFiles<'_>,
    metadata: &PackageMetadata,
    selected: &[SelectedFile<'_>],
    imported: &BTreeSet<&str>,
) -> BTreeMap<(&'static str, &'input str), BTreeSet<&'input ProjectPath>> {
    let mut observed = BTreeMap::<_, BTreeSet<_>>::new();
    for observation in input.modules() {
        let Some(implementation) = javascript_file(input, selected, observation) else {
            continue;
        };
        let module = observation.module();
        let joined = joined_files(input, files, metadata, selected, observation);
        let admitted = if observation.declarations().is_empty() {
            let import = observation.export_conditions().contains(&"import");
            if imported.contains(module) {
                import && describes_build(metadata, observation)
            } else {
                commonjs::admits(files, metadata, observation, implementation)
            }
        } else {
            !joined.is_empty()
        };
        if admitted {
            observed
                .entry(("javascript", module))
                .or_default()
                .insert(observation.implementation().path());
            observed
                .entry(("typescript", module))
                .or_default()
                .extend(joined);
        }
    }
    observed.retain(|_, files| !files.is_empty());
    observed
}

/// Modules a build observed under `import` holds: the manifest's conditions describe that
/// build, its file is selected and is the one the static entries give the module, and no
/// admitted observation joins declarations to the module.
pub(super) fn import_builds<'input>(
    input: &NamespaceInput<'input>,
    files: &PackageFiles<'_>,
    metadata: &PackageMetadata,
    selected: &[SelectedFile<'_>],
    entries: &BTreeMap<ProjectPath, Option<String>>,
) -> BTreeSet<&'input str> {
    let joined = input
        .modules()
        .iter()
        .filter(|observation| {
            javascript_file(input, selected, observation).is_some()
                && !joined_files(input, files, metadata, selected, observation).is_empty()
        })
        .map(|observation| observation.module())
        .collect::<BTreeSet<_>>();
    input
        .modules()
        .iter()
        .filter(|observation| {
            let module = observation.module();
            observation.declarations().is_empty()
                && observation.export_conditions().contains(&"import")
                && !joined.contains(module)
                && javascript_file(input, selected, observation).is_some()
                && entries
                    .get(observation.implementation().path())
                    .map(Option::as_deref)
                    == Some(Some(module))
                && describes_build(metadata, observation)
        })
        .map(|observation| observation.module())
        .collect()
}

fn javascript_file<'selected, 'source>(
    input: &NamespaceInput<'_>,
    selected: &'selected [SelectedFile<'source>],
    observation: &NamespaceModule<'_>,
) -> Option<&'selected SelectedFile<'source>> {
    selected_file(input, selected, observation.implementation())
        .filter(|file| file.syntax.language().name == "javascript")
}

/// The selected declaration files the observation joins to its module.
fn joined_files<'input>(
    input: &NamespaceInput<'_>,
    files: &PackageFiles<'_>,
    metadata: &PackageMetadata,
    selected: &[SelectedFile<'_>],
    observation: &NamespaceModule<'input>,
) -> BTreeSet<&'input ProjectPath> {
    observation
        .declarations()
        .iter()
        .filter(|source| {
            selected_file(input, selected, **source).is_some_and(|file| {
                file.syntax.language().name == "typescript"
                    && package_observation(files, metadata, observation, file)
            })
        })
        .map(|source| source.path())
        .collect()
}

fn describes_build(metadata: &PackageMetadata, observation: &NamespaceModule<'_>) -> bool {
    metadata
        .exports
        .as_ref()
        .and_then(|exports| {
            exports.observed_targets(
                &metadata.name,
                observation.module(),
                observation.export_conditions(),
            )
        })
        .and_then(|(_, implementation)| target_path(implementation))
        .as_ref()
        == Some(observation.implementation().path())
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
    files: &PackageFiles<'_>,
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
        exports
            .observed_targets(&metadata.name, observation.module(), conditions)
            .and_then(|(types, implementation)| target_path(types).zip(target_path(implementation)))
    } else if observation.module() == metadata.name {
        let implementation = match (&metadata.main, &metadata.module) {
            (Some(main), Some(module)) if main != module => {
                if conditions == ["import"] {
                    target_path(module)
                } else if conditions == ["require"] {
                    established(files.runtime(Some(main), None))
                } else {
                    None
                }
            }
            (main, module) => established(files.runtime(main.as_deref(), module.as_deref())),
        };
        established(files.types(metadata)).zip(implementation)
    } else {
        None
    };
    targets.is_some_and(|(types, implementation)| {
        &types == declarations.path && &implementation == observation.implementation().path()
    })
}

fn established(entry: Entry) -> Option<ProjectPath> {
    match entry {
        Entry::File(path) => Some(path),
        Entry::Unknown | Entry::Absent => None,
    }
}

pub(super) fn target_path(target: &str) -> Option<ProjectPath> {
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

pub(super) fn literal_module_name(source: &str, range: ByteRange) -> Option<String> {
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
