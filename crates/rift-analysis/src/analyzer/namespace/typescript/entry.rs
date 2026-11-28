//! The file a package entry names, decided over captured evidence.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::ProjectPath;
use rift_protocol::read::ProjectPath as ObservationPath;

use super::{Loader, PackageMetadata, entry_path, export_subpath_is_valid};
use crate::static_resolution::{declared_extensions, is_declaration_file};
use crate::{ArchiveMemberKind, NamespaceInput, PackageSource};

/// Extensions Node tries after an exact file name, in its order.
const RUNTIME_EXTENSIONS: [&str; 3] = [".js", ".json", ".node"];
/// Extensions TypeScript tries for a path without one, in its order.
const TYPES_EXTENSIONS: [&str; 3] = [".ts", ".tsx", ".d.ts"];
/// Extensions of a file TypeScript reads as it is spelled.
const TYPES_SOURCES: [&str; 4] = [".ts", ".tsx", ".mts", ".cts"];

/// What an entry of a package resolves to.
pub(super) enum Entry {
    File(ProjectPath),
    /// A path that decides the entry has no captured state.
    Unknown,
    /// Nothing written or established names an entry.
    Absent,
}

/// What is established about one path of the package.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Presence {
    File,
    /// A folder, or a path below a file.
    NoFile,
    Missing,
    Unknown,
}

/// The state of each package path: the captured files, and the path observations whose
/// absent keys stay unknown. A selection without an observation never proves absence.
pub(crate) struct PackageFiles<'input> {
    captured: BTreeSet<&'input str>,
    observed: Option<&'input BTreeMap<ObservationPath, Option<ArchiveMemberKind>>>,
}

impl<'input> PackageFiles<'input> {
    pub(crate) fn captured(input: &NamespaceInput<'input>) -> Self {
        Self {
            captured: input
                .files()
                .iter()
                .chain(input.context_sources())
                .map(|source| source.path().as_str())
                .collect(),
            observed: input.build_paths(),
        }
    }

    fn presence(&self, path: &str) -> Presence {
        if self.captured.contains(path) {
            return Presence::File;
        }
        let folder = format!("{path}/");
        if self
            .captured
            .range(folder.as_str()..)
            .next()
            .is_some_and(|captured| captured.starts_with(&folder))
        {
            return Presence::NoFile;
        }
        let Some(observed) = self.observed else {
            return Presence::Unknown;
        };
        // A path below a missing folder is missing, and one below a file names no file.
        for (end, _) in path.match_indices('/') {
            match observed.get(&ObservationPath(path[..end].to_owned())) {
                Some(None) => return Presence::Missing,
                Some(Some(ArchiveMemberKind::File)) => return Presence::NoFile,
                _ => {}
            }
        }
        match observed.get(&ObservationPath(path.to_owned())) {
            Some(None) => Presence::Missing,
            Some(Some(ArchiveMemberKind::File)) => Presence::File,
            Some(Some(ArchiveMemberKind::Directory)) => Presence::NoFile,
            // A link's target is not captured.
            Some(Some(_)) | None => Presence::Unknown,
        }
    }

    /// The first candidate that is a file, when every earlier one is established not to be.
    fn first(&self, candidates: impl IntoIterator<Item = String>) -> Entry {
        for candidate in candidates {
            match self.presence(&candidate) {
                Presence::File => {
                    return ProjectPath::new(candidate).map_or(Entry::Absent, Entry::File);
                }
                Presence::NoFile | Presence::Missing => {}
                Presence::Unknown => return Entry::Unknown,
            }
        }
        Entry::Absent
    }

    /// The entry `main` names for Node. `module` is no Node field: without a written `main` it
    /// names the entry when Node's index is established missing, or when its state is not
    /// captured. The captured `module` file is then the one entry the manifest spells.
    pub(super) fn runtime(&self, main: Option<&str>, module: Option<&str>) -> Entry {
        let entry = match runtime_candidates(main, "") {
            Some(candidates) => self.first(candidates),
            None => return Entry::Absent,
        };
        let written = main.is_some_and(|main| !main.is_empty());
        match (entry, module) {
            (Entry::Absent | Entry::Unknown, Some(module)) if !written => {
                match normalized(module) {
                    Some(module) if self.presence(&module) == Presence::File => {
                        ProjectPath::new(module).map_or(Entry::Absent, Entry::File)
                    }
                    _ => Entry::Unknown,
                }
            }
            (entry, _) => entry,
        }
    }

    /// The first file a written nested main reaches, including files outside the source selection.
    pub(crate) fn observed_folder_entry(&self, candidates: Vec<String>) -> Option<ProjectPath> {
        match self.first(candidates) {
            Entry::File(path) => Some(path),
            Entry::Unknown | Entry::Absent => None,
        }
    }

    /// Whether a file wins before the directory for a specifier without a trailing slash.
    /// Returns `None` when no file was observed and at least one candidate remains unknown.
    pub(crate) fn file_before_folder(&self, directory: &str) -> Option<bool> {
        let mut unknown = false;
        for candidate in runtime_file_candidates(directory) {
            match self.presence(&candidate) {
                Presence::File => return Some(true),
                Presence::Unknown => unknown = true,
                Presence::NoFile | Presence::Missing => {}
            }
        }
        (!unknown).then_some(false)
    }

    /// Whether captured paths establish a TypeScript file for a runtime spelling.
    pub(super) fn has_type_substitution(&self, runtime: &str) -> bool {
        type_substitutions(runtime).is_some_and(|mut candidates| {
            candidates.any(|path| self.presence(&path) == Presence::File)
        })
    }

    /// The checked declaration beside a runtime file named by a direct package export.
    pub(super) fn runtime_declaration(&self, runtime: &str) -> Option<ProjectPath> {
        if !export_subpath_is_valid(runtime) || self.presence(runtime) != Presence::File {
            return None;
        }
        self.unique_declaration(type_substitutions(runtime)?)
    }

    /// A written relative module confirms only one captured declaration path. A typed
    /// source or another possible declaration with captured file evidence takes precedence
    /// over this proof.
    pub(super) fn relative_declaration(
        &self,
        from: &ProjectPath,
        specifier: &str,
    ) -> Option<ProjectPath> {
        if !(specifier.starts_with("./") || specifier.starts_with("../"))
            || specifier.contains(['%', '?', '#'])
            || specifier.contains("//")
            || specifier.ends_with('/')
        {
            return None;
        }
        let folder = from
            .as_str()
            .rsplit_once('/')
            .map_or("", |(folder, _)| folder);
        let path = folder_path(folder, specifier)?;
        if !export_subpath_is_valid(&path) {
            return None;
        }
        let candidates = if let Some(siblings) = type_substitutions(&path) {
            siblings.collect::<Vec<_>>()
        } else {
            if self.presence(&format!("{path}/package.json")) == Presence::File {
                return None;
            }
            types_candidates(&path)?
        };
        self.unique_declaration(candidates)
    }

    /// Resolves a captured runtime file at a written relative specifier. A `CommonJS`
    /// extensionless request can name a single captured JavaScript file or its directory index.
    pub(super) fn relative_runtime(
        &self,
        from: &ProjectPath,
        specifier: &str,
        loader: Loader,
    ) -> Option<ProjectPath> {
        if !(specifier.starts_with("./") || specifier.starts_with("../"))
            || specifier.contains(['%', '?', '#'])
            || specifier.contains("//")
            || specifier.ends_with('/')
        {
            return None;
        }
        let directory = from
            .as_str()
            .rsplit_once('/')
            .map_or("", |(directory, _)| directory);
        let path = folder_path(directory, specifier)?;
        if !export_subpath_is_valid(&path) {
            return None;
        }
        if self.presence(&path) == Presence::File {
            return ProjectPath::new(path).ok();
        }
        if loader != Loader::CommonJs || std::path::Path::new(&path).extension().is_some() {
            return None;
        }
        if self.observed.is_some_and(|observed| {
            matches!(
                observed.get(&ObservationPath(path.clone())),
                Some(Some(ArchiveMemberKind::Link))
            )
        }) || self.presence(&format!("{path}/package.json")) == Presence::File
        {
            return None;
        }
        let mut candidates = [format!("{path}.js"), format!("{path}/index.js")]
            .into_iter()
            .filter(|candidate| self.presence(candidate) == Presence::File);
        let target = candidates.next()?;
        candidates
            .next()
            .is_none()
            .then(|| ProjectPath::new(target).ok())
            .flatten()
    }

    fn unique_declaration(
        &self,
        candidates: impl IntoIterator<Item = String>,
    ) -> Option<ProjectPath> {
        let mut existing = candidates
            .into_iter()
            .filter(|path| self.presence(path) == Presence::File);
        let first = existing.next()?;
        (is_declaration_file(&first) && existing.next().is_none())
            .then(|| ProjectPath::new(first).ok())
            .flatten()
    }

    pub(super) fn types(&self, metadata: &PackageMetadata) -> Entry {
        if metadata.types_versions.is_some() {
            return Entry::Absent;
        }
        self.unversioned_types(metadata)
    }

    /// The TypeScript entry before compiler-version redirects.
    pub(super) fn unversioned_types(&self, metadata: &PackageMetadata) -> Entry {
        if let Some(types) = metadata.types.as_deref().or(metadata.typings.as_deref()) {
            return types_candidates(types)
                .map_or(Entry::Absent, |candidates| self.first(candidates));
        }
        match default_types(metadata.main()).map(|candidates| self.first(candidates)) {
            // A source file is no declared types entry.
            Some(Entry::File(path)) if !is_declaration_file(path.as_str()) => Entry::Absent,
            Some(entry) => entry,
            None => Entry::Absent,
        }
    }
}

/// The types entry without `types` and `typings`: a declaration file `main` spells, or the
/// package's index. `None` for a `main` that names something else.
fn default_types(main: Option<&str>) -> Option<Vec<String>> {
    let main = main.map_or_else(|| Some(String::new()), normalized)?;
    if main.is_empty() {
        return Some(
            TYPES_EXTENSIONS
                .map(|extension| format!("index{extension}"))
                .to_vec(),
        );
    }
    is_declaration_file(&main).then(|| vec![main])
}

/// The entry of a package for one language. `exports` names its file exactly.
pub(super) fn resolve(
    files: &PackageFiles<'_>,
    metadata: &PackageMetadata,
    language: &str,
) -> Entry {
    if metadata.exports.is_some() {
        return entry_path(metadata, language).map_or(Entry::Absent, Entry::File);
    }
    match language {
        "typescript" => files.types(metadata),
        "javascript" => files.runtime(metadata.main(), metadata.module.as_deref()),
        _ => Entry::Absent,
    }
}

fn type_substitutions(path: &str) -> Option<impl Iterator<Item = String> + '_> {
    let (stem, sources, declarations) = declared_extensions(path)?;
    Some(
        sources
            .iter()
            .chain(declarations)
            .map(move |extension| format!("{stem}{extension}")),
    )
}

/// Node's `LOAD_AS_DIRECTORY` order for a written `main`, or the package's index without
/// one. `None` for a target outside the package.
fn runtime_candidates(main: Option<&str>, directory: &str) -> Option<Vec<String>> {
    let mut candidates = Vec::new();
    if let Some(main) = main.filter(|main| !main.is_empty()) {
        let path = normalized(main)?;
        if !path.is_empty() {
            candidates.extend(runtime_file_candidates(&path));
            candidates
                .extend(RUNTIME_EXTENSIONS.map(|extension| format!("{path}/index{extension}")));
        }
    }
    let index = if directory.is_empty() {
        "index".to_owned()
    } else {
        format!("{directory}/index")
    };
    candidates.extend(RUNTIME_EXTENSIONS.map(|extension| format!("{index}{extension}")));
    Some(candidates)
}

fn runtime_file_candidates(path: &str) -> impl Iterator<Item = String> + '_ {
    std::iter::once(path.to_owned()).chain(
        RUNTIME_EXTENSIONS
            .into_iter()
            .map(move |extension| format!("{path}{extension}")),
    )
}

/// Runtime candidates for a written nested main.
pub(crate) struct NestedRuntimeCandidates {
    pub(crate) paths: Vec<String>,
    pub(crate) explicit_script: bool,
}

/// The nested entries whose runtime choice can use captured path observations.
pub(crate) fn nested_runtime_candidates(
    main: &str,
    directory: &str,
) -> Option<NestedRuntimeCandidates> {
    if main.is_empty() {
        return None;
    }
    let path = folder_path(directory, main)?;
    let explicit_script = match std::path::Path::new(&path)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
    {
        Some("js" | "cjs" | "mjs") => true,
        None => false,
        Some(_) => return None,
    };
    Some(NestedRuntimeCandidates {
        paths: runtime_candidates(Some(&path), directory)?,
        explicit_script,
    })
}

/// The order TypeScript tries for a written `types`. `None` for a target outside the package.
fn types_candidates(types: &str) -> Option<Vec<String>> {
    let path = normalized(types)?;
    if TYPES_SOURCES
        .iter()
        .any(|extension| path.ends_with(extension))
    {
        return Some(vec![path]);
    }
    let mut candidates = Vec::new();
    if !path.is_empty() {
        candidates.extend(TYPES_EXTENSIONS.map(|extension| format!("{path}{extension}")));
    }
    let folder = if path.is_empty() {
        String::new()
    } else {
        format!("{path}/")
    };
    candidates.extend(TYPES_EXTENSIONS.map(|extension| format!("{folder}index{extension}")));
    Some(candidates)
}

/// A written path as Node joins it to the package folder: empty and `.` segments vanish and
/// `..` removes the segment before it. `None` when it leaves the package or is absolute.
fn normalized(target: &str) -> Option<String> {
    folder_path("", target)
}

/// A written path joined to its folder, bounded by the selected package or workspace.
pub(crate) fn folder_path(directory: &str, target: &str) -> Option<String> {
    if target.starts_with('/') || target.contains('\\') {
        return None;
    }
    let mut segments = directory
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    for segment in target.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            segment => segments.push(segment),
        }
    }
    Some(segments.join("/"))
}

/// The paths whose state decides the entry a package manifest names, for the caller to
/// observe. `exports` names its files exactly and requests nothing.
pub(in crate::analyzer) fn build_paths(
    manifest: PackageSource<'_>,
    source_bytes_max: usize,
) -> Option<Vec<ObservationPath>> {
    if manifest.text().len() > source_bytes_max {
        return None;
    }
    let path = manifest.path().as_str();
    if path != "package.json" {
        let directory = path.strip_suffix("/package.json")?;
        let metadata: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(manifest.text()).ok()?;
        let main = metadata.get("main")?.as_str()?;
        let candidates = nested_runtime_candidates(main, directory)?;
        return Some(requested_paths(
            candidates
                .paths
                .into_iter()
                .chain(runtime_file_candidates(directory)),
        ));
    }
    let metadata = PackageMetadata::parse(manifest.text())?;
    if metadata.exports.is_some() {
        return None;
    }
    let types = metadata.types.as_deref().or(metadata.typings.as_deref());
    let declared = match types {
        Some(types) => types_candidates(types).unwrap_or_default(),
        None => default_types(metadata.main()).unwrap_or_default(),
    };
    Some(requested_paths(
        runtime_candidates(metadata.main(), "")
            .unwrap_or_default()
            .into_iter()
            .chain(declared),
    ))
}

fn requested_paths(paths: impl Iterator<Item = String>) -> Vec<ObservationPath> {
    paths
        .filter(|path| ProjectPath::new(path.as_str()).is_ok())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(ObservationPath)
        .collect()
}
