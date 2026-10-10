use crate::ArchiveMemberKind;
use std::collections::{BTreeMap, BTreeSet};

use rift_core::ProjectPath;
use rift_protocol::read::ProjectPath as ObservationPath;
use rift_syntax::SyntaxLimits;
use serde::Deserialize;

use crate::{NamespaceInput, PackageImportRoot, PackageImportRootOrigin, PackageSource};

#[derive(Default, Deserialize)]
struct Metadata {
    #[serde(rename = "build-system")]
    build_system: Option<BuildSystem>,
    tool: Option<Tool>,
    project: Option<Project>,
}

#[derive(Deserialize)]
struct Project {
    dynamic: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct BuildSystem {
    build_backend: String,
    requires: Vec<String>,
    backend_path: Option<Vec<String>>,
}

#[derive(Default, Deserialize)]
struct Tool {
    pdm: Option<Pdm>,
    setuptools: Option<Setuptools>,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct Setuptools {
    py_modules: Vec<String>,
    packages: Option<Vec<String>>,
    package_dir: Option<BTreeMap<String, String>>,
}

#[derive(Default, Deserialize)]
struct Pdm {
    build: Option<Build>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct Build {
    package_dir: Option<String>,
    includes: Option<Vec<String>>,
    excludes: Option<Vec<String>>,
    source_includes: Option<Vec<String>>,
    custom_hook: Option<String>,
    run_setuptools: Option<bool>,
}

struct Config {
    parent: ProjectPath,
    build: Build,
    py_modules: Option<Vec<String>>,
}

fn config(source: PackageSource<'_>, syntax: SyntaxLimits) -> Option<Config> {
    let path = source.path().as_str();
    let parent = if path == "pyproject.toml" {
        ""
    } else {
        path.strip_suffix("/pyproject.toml")?
    };
    if source.text().len() > syntax.source_bytes_max() {
        return None;
    }
    let metadata: Metadata = toml::from_str(source.text()).ok()?;
    let system = metadata.build_system?;
    if system.backend_path.is_some() {
        return None;
    }
    let tool = metadata.tool.unwrap_or_default();
    let (build, py_modules) = match system.build_backend.as_str() {
        "pdm.backend" if system.requires == ["pdm-backend"] => {
            (tool.pdm.unwrap_or_default().build.unwrap_or_default(), None)
        }
        "setuptools.build_meta" if system.requires == ["setuptools==80.9.0"] => {
            if metadata
                .project
                .is_some_and(|project| project.dynamic.is_some_and(|fields| !fields.is_empty()))
            {
                return None;
            }
            let settings = tool.setuptools?;
            if settings
                .packages
                .is_some_and(|packages| !packages.is_empty())
            {
                return None;
            }
            let package_dir = match settings.package_dir {
                None => None,
                Some(mut paths) if paths.len() == 1 => Some(paths.remove("")?),
                Some(_) => return None,
            };
            if settings.py_modules.len() > crate::PACKAGE_IMPORT_ENTRIES_MAX
                || settings
                    .py_modules
                    .iter()
                    .fold(0_usize, |bytes, module| bytes.saturating_add(module.len()))
                    > crate::PACKAGE_IMPORT_BYTES_MAX
            {
                return None;
            }
            PackageImportRoot::new(
                None,
                settings.py_modules.clone(),
                PackageImportRootOrigin::PyModules,
            )
            .ok()?;
            if settings
                .py_modules
                .iter()
                .any(|module| module.contains('.'))
            {
                return None;
            }
            (
                Build {
                    package_dir,
                    ..Build::default()
                },
                Some(settings.py_modules),
            )
        }
        _ => return None,
    };
    Some(Config {
        parent: ProjectPath::new(parent).ok()?,
        build,
        py_modules,
    })
}

fn joined(parent: &ProjectPath, relative: &str) -> Option<ProjectPath> {
    let relative = if relative == "." { "" } else { relative };
    let relative = ProjectPath::new(relative).ok()?;
    let path = match (parent.as_str(), relative.as_str()) {
        ("", relative) => relative.to_owned(),
        (parent, "") => parent.to_owned(),
        (parent, relative) => format!("{parent}/{relative}"),
    };
    ProjectPath::new(path).ok()
}

fn package_paths<'source>(
    root: &'source ProjectPath,
    files: &'source [PackageSource<'_>],
) -> impl Iterator<Item = (&'source str, &'source str)> {
    files.iter().filter_map(move |file| {
        let path = file.path().as_str();
        let relative = if root.as_str().is_empty() {
            path
        } else {
            path.strip_prefix(root.as_str())?.strip_prefix('/')?
        };
        let (module, rest) = relative.split_once('/')?;
        (rest == "__init__.py").then_some((module, path))
    })
}

/// Requests path observations without using selected-file absence as filesystem evidence.
#[must_use]
pub fn build_paths(
    metadata: PackageSource<'_>,
    files: &[PackageSource<'_>],
    syntax: SyntaxLimits,
) -> Option<Vec<ObservationPath>> {
    let config = config(metadata, syntax)?;
    let root = joined(
        &config.parent,
        config.build.package_dir.as_deref().unwrap_or(""),
    )?;
    if let Some(modules) = &config.py_modules {
        let mut paths = BTreeSet::from([
            config.parent.clone(),
            metadata.path().clone(),
            root.clone(),
            joined(&config.parent, "setup.py")?,
            joined(&config.parent, "setup.cfg")?,
        ]);
        for module in modules {
            paths.insert(joined(&root, &format!("{module}.py"))?);
        }
        return Some(
            paths
                .into_iter()
                .map(|path| ObservationPath(path.as_str().to_owned()))
                .collect(),
        );
    }
    let src = joined(&config.parent, "src")?;
    let hook = joined(
        &config.parent,
        config
            .build
            .custom_hook
            .as_deref()
            .unwrap_or("pdm_build.py"),
    )?;
    let mut paths = BTreeSet::from([
        config.parent,
        metadata.path().clone(),
        root.clone(),
        src.clone(),
        hook,
    ]);
    for candidate in [&root, &src] {
        for (module, file) in package_paths(candidate, files) {
            paths.insert(joined(candidate, module)?);
            paths.insert(ProjectPath::new(file).ok()?);
        }
    }
    Some(
        paths
            .into_iter()
            .map(|path| ObservationPath(path.as_str().to_owned()))
            .collect(),
    )
}

fn observation<'paths>(
    paths: &'paths BTreeMap<ObservationPath, Option<ArchiveMemberKind>>,
    path: &ProjectPath,
) -> Option<&'paths Option<ArchiveMemberKind>> {
    paths.get(&ObservationPath(path.as_str().to_owned()))
}

fn directory(
    paths: &BTreeMap<ObservationPath, Option<ArchiveMemberKind>>,
    path: &ProjectPath,
) -> bool {
    matches!(
        observation(paths, path),
        Some(Some(ArchiveMemberKind::Directory))
    )
}

fn regular(
    paths: &BTreeMap<ObservationPath, Option<ArchiveMemberKind>>,
    path: &ProjectPath,
) -> bool {
    matches!(
        observation(paths, path),
        Some(Some(ArchiveMemberKind::File))
    )
}

fn root(
    config: &Config,
    paths: &BTreeMap<ObservationPath, Option<ArchiveMemberKind>>,
) -> Option<ProjectPath> {
    if !directory(paths, &config.parent)
        || config.build.custom_hook.is_some()
        || config.build.run_setuptools == Some(true)
        || config
            .build
            .includes
            .as_ref()
            .is_some_and(|paths| !paths.is_empty())
        || config
            .build
            .excludes
            .as_ref()
            .is_some_and(|paths| !paths.is_empty())
        || !matches!(
            observation(paths, &joined(&config.parent, "pdm_build.py")?),
            Some(None)
        )
    {
        return None;
    }
    if let Some(explicit) = config.build.package_dir.as_deref() {
        let explicit = joined(&config.parent, explicit)?;
        return directory(paths, &explicit).then_some(explicit);
    }
    let src = joined(&config.parent, "src")?;
    match observation(paths, &src)? {
        None | Some(ArchiveMemberKind::File) => Some(config.parent.clone()),
        Some(ArchiveMemberKind::Directory) => Some(src),
        Some(ArchiveMemberKind::Link) => None,
    }
}

fn source_includes(config: &Config) -> Option<Vec<ProjectPath>> {
    let Some(paths) = config.build.source_includes.as_ref() else {
        return Some(vec![joined(&config.parent, "tests")?]);
    };
    paths
        .iter()
        .map(|path| {
            if path
                .chars()
                .any(|character| matches!(character, '*' | '?' | '[' | ']'))
            {
                return None;
            }
            joined(&config.parent, path.trim_end_matches('/'))
        })
        .collect()
}

fn overlaps(first: &ProjectPath, second: &ProjectPath) -> bool {
    first.as_str().is_empty()
        || second.as_str().is_empty()
        || first == second
        || [(first, second), (second, first)]
            .iter()
            .any(|(parent, child)| {
                child
                    .as_str()
                    .strip_prefix(parent.as_str())
                    .is_some_and(|tail| tail.starts_with('/'))
            })
}

fn captured_root(
    metadata: PackageSource<'_>,
    files: &[PackageSource<'_>],
    syntax: SyntaxLimits,
    paths: &BTreeMap<ObservationPath, Option<ArchiveMemberKind>>,
) -> Option<PackageImportRoot> {
    if !regular(paths, metadata.path()) {
        return None;
    }
    let config = config(metadata, syntax)?;
    if let Some(modules) = &config.py_modules {
        return captured_modules(&config, modules, files, paths);
    }
    let root = root(&config, paths)?;
    let excluded = source_includes(&config)?;
    let mut modules = BTreeSet::new();
    for (module, file) in package_paths(&root, files) {
        if matches!(module, "__pycache__" | "__pypackages__" | "build") {
            continue;
        }
        let package = joined(&root, module)?;
        let file = ProjectPath::new(file).ok()?;
        if !directory(paths, &package)
            || !regular(paths, &file)
            || excluded.iter().any(|path| overlaps(path, &package))
        {
            continue;
        }
        modules.insert(module.to_owned());
        if modules.len() > crate::PACKAGE_IMPORT_ENTRIES_MAX {
            return None;
        }
    }
    PackageImportRoot::new(
        (!root.as_str().is_empty()).then_some(root),
        modules.into_iter().collect(),
        PackageImportRootOrigin::Pdm,
    )
    .ok()
}

fn captured_modules(
    config: &Config,
    modules: &[String],
    files: &[PackageSource<'_>],
    paths: &BTreeMap<ObservationPath, Option<ArchiveMemberKind>>,
) -> Option<PackageImportRoot> {
    let root = joined(
        &config.parent,
        config.build.package_dir.as_deref().unwrap_or(""),
    )?;
    if !directory(paths, &config.parent)
        || !directory(paths, &root)
        || !matches!(
            observation(paths, &joined(&config.parent, "setup.py")?),
            Some(None)
        )
        || !matches!(
            observation(paths, &joined(&config.parent, "setup.cfg")?),
            Some(None)
        )
    {
        return None;
    }
    for module in modules {
        let path = joined(&root, &format!("{module}.py"))?;
        if !regular(paths, &path) || !files.iter().any(|file| file.path() == &path) {
            return None;
        }
    }
    PackageImportRoot::new(
        (!root.as_str().is_empty()).then_some(root),
        modules.to_vec(),
        PackageImportRootOrigin::PyModules,
    )
    .ok()
}

/// Establishes import roots from captured build metadata and paths without running a build backend.
/// Incomplete layout stays unresolved.
pub(super) fn import_roots(input: &NamespaceInput<'_>) -> Vec<PackageImportRoot> {
    let Some(paths) = input.build_paths() else {
        return Vec::new();
    };
    let mut roots = Vec::new();
    let mut modules = 0_usize;
    let mut bytes = 0_usize;
    for metadata in input.files().iter().chain(input.context_sources()) {
        let Some(root) = captured_root(*metadata, input.files(), input.syntax(), paths) else {
            continue;
        };
        modules = modules.saturating_add(root.modules().len());
        bytes = bytes.saturating_add(root.prefix().map_or(0, |path| path.as_str().len()));
        for module in root.modules() {
            bytes = bytes.saturating_add(module.len());
        }
        if roots.len() >= crate::PACKAGE_IMPORT_ENTRIES_MAX
            || modules > crate::PACKAGE_IMPORT_ENTRIES_MAX
            || bytes > crate::PACKAGE_IMPORT_BYTES_MAX
        {
            return Vec::new();
        }
        roots.push(root);
    }
    roots
}

#[cfg(test)]
mod tests;
