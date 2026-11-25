//! The file a package entry names, decided over captured evidence.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::ProjectPath;
use rift_protocol::read::ProjectPath as ObservationPath;

use super::{PackageMetadata, entry_path};
use crate::static_resolution::is_declaration_file;
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
pub(super) struct PackageFiles<'input> {
    captured: BTreeSet<&'input str>,
    observed: Option<&'input BTreeMap<ObservationPath, Option<ArchiveMemberKind>>>,
}

impl<'input> PackageFiles<'input> {
    pub(super) fn captured(input: &NamespaceInput<'input>) -> Self {
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
        let entry = match runtime_candidates(main) {
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
        match default_types(metadata.main.as_deref()).map(|candidates| self.first(candidates)) {
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
        "javascript" => files.runtime(metadata.main.as_deref(), metadata.module.as_deref()),
        _ => Entry::Absent,
    }
}

/// Node's `LOAD_AS_DIRECTORY` order for a written `main`, or the package's index without
/// one. `None` for a target outside the package.
fn runtime_candidates(main: Option<&str>) -> Option<Vec<String>> {
    let mut candidates = Vec::new();
    if let Some(main) = main.filter(|main| !main.is_empty()) {
        let path = normalized(main)?;
        if !path.is_empty() {
            candidates.push(path.clone());
            candidates.extend(RUNTIME_EXTENSIONS.map(|extension| format!("{path}{extension}")));
            candidates
                .extend(RUNTIME_EXTENSIONS.map(|extension| format!("{path}/index{extension}")));
        }
    }
    candidates.extend(RUNTIME_EXTENSIONS.map(|extension| format!("index{extension}")));
    Some(candidates)
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
    if target.starts_with('/') || target.contains('\\') {
        return None;
    }
    let mut segments = Vec::new();
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
    if manifest.path().as_str() != "package.json" || manifest.text().len() > source_bytes_max {
        return None;
    }
    let metadata: PackageMetadata = serde_json::from_str(manifest.text()).ok()?;
    if metadata.exports.is_some() {
        return None;
    }
    let types = metadata.types.as_deref().or(metadata.typings.as_deref());
    let declared = match types {
        Some(types) => types_candidates(types).unwrap_or_default(),
        None => default_types(metadata.main.as_deref()).unwrap_or_default(),
    };
    let paths = runtime_candidates(metadata.main.as_deref())
        .unwrap_or_default()
        .into_iter()
        .chain(declared)
        .filter(|path| ProjectPath::new(path.as_str()).is_ok())
        .collect::<BTreeSet<_>>();
    Some(paths.into_iter().map(ObservationPath).collect())
}
