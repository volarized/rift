//! Installed language server recommendations shared by the CLI and workspace maps.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use rift_error::{RiftError, errors};
use rift_index::WorkspaceSourcePolicy;
use rift_protocol::configuration::{
    CONFIGURATION_FILE_BYTES_MAX, LspPackageManager, WorkspaceConfiguration,
};
use rift_protocol::map::{
    MAP_RECOMMENDATION_COMMAND_CHARS_MAX, MAP_RECOMMENDATION_ROOT_CHARS_MAX,
    MAP_RECOMMENDATIONS_MAX,
};
use rift_protocol::read::SearchScope;

/// One detected language and project, with the existing selection or an apply command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LspRecommendation {
    /// Exact language identity.
    pub language: String,
    /// Project folder relative to the primary project, or an external registered folder.
    pub root: String,
    /// Visible source file that selected this language.
    pub origin: String,
    /// Existing configuration, executable availability, or an apply command.
    pub status: LspRecommendationStatus,
}

/// Whether a detected language needs a configuration change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LspRecommendationStatus {
    /// A language already selects a named, inline, or embedded engine.
    Configured,
    /// The proposed folder overlaps a different registered project.
    Overlapping,
    /// No installed supported executable was found.
    Unavailable {
        /// Executable to install before requesting another recommendation.
        program: String,
    },
    /// Installed executable and a command accepted by `rift config set`.
    Available {
        /// Selected executable relative to its project or available on PATH.
        program: String,
        /// Native shell command that applies the recommendation.
        command: String,
        /// Primary project, root-defined profile, or extra workspace selected by the command.
        scope: SearchScope,
    },
}

/// Available map recommendations retained within the protocol and byte bounds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LspRecommendationBatch {
    /// Available apply commands, in project folder and language order.
    pub items: Vec<LspRecommendation>,
    /// More available commands were omitted by a count, character, or byte bound.
    pub truncated: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RecommendationSelection {
    Complete,
    Available,
}

/// Captured visible files and accepted configuration used by both recommendation callers.
pub struct LspRecommendationInputs {
    root: PathBuf,
    configuration: WorkspaceConfiguration,
    registered: BTreeMap<String, PathBuf>,
    projects: BTreeMap<(PathBuf, String), PathBuf>,
}

impl LspRecommendationInputs {
    /// Holds the primary configuration and resolves its direct project registrations once.
    ///
    /// # Errors
    ///
    /// Returns an error when the root or a registered project cannot be read.
    pub fn new(root: &Path, configuration: WorkspaceConfiguration) -> Result<Self, RiftError> {
        let root = std::fs::canonicalize(root).map_err(|error| unreadable(root, error))?;
        let registered = registration_roots(&root, &configuration)?;
        Ok(Self {
            root,
            configuration,
            registered,
            projects: BTreeMap::new(),
        })
    }

    /// Direct registered project roots. Their own registrations are not followed.
    #[must_use]
    pub const fn registered_projects(&self) -> &BTreeMap<String, PathBuf> {
        &self.registered
    }

    /// Adds captured visible paths using their accepted language policy, without walking disk.
    ///
    /// # Errors
    ///
    /// Returns an error when the language policy refuses a visible path.
    pub fn include_project(
        &mut self,
        root: &Path,
        paths: &[rift_core::ProjectPath],
        policy: &WorkspaceSourcePolicy,
    ) -> Result<(), RiftError> {
        let root = std::fs::canonicalize(root).map_err(|error| unreadable(root, error))?;
        let found = collect(&root, paths, policy)?;
        let registered = self.registered.values().any(|project| project == &root);
        for ((project, language), origin) in found {
            let owner = if registered { root.clone() } else { project };
            self.projects.entry((owner, language)).or_insert(origin);
        }
        Ok(())
    }

    /// Resolves installed executables once and produces shared configuration decisions.
    ///
    /// The accepted primary configuration is held by this input. A nested proposed project
    /// reads its own configuration once per recommendation. No language server is started.
    ///
    /// # Errors
    ///
    /// Returns an error for unreadable nested configuration or recommendations past the
    /// configuration byte limit.
    pub fn finish(self) -> Result<Vec<LspRecommendation>, RiftError> {
        let projects = grouped(&self.root, self.projects, &self.registered);
        decisions(
            &self.root,
            &self.configuration,
            projects,
            &self.registered,
            RecommendationSelection::Complete,
        )
        .map(|batch| batch.items)
    }

    /// Retains available commands for a map, without charging configured languages to its bounds.
    ///
    /// At most the map's recommendation count, root characters, and command characters are
    /// stored. Total language, root, and command bytes stay within the configuration byte bound.
    /// The batch records when a bound omits an available command.
    ///
    /// # Errors
    ///
    /// Returns an error when a nested project's configuration cannot be read.
    pub fn finish_available(self) -> Result<LspRecommendationBatch, RiftError> {
        let projects = grouped(&self.root, self.projects, &self.registered);
        decisions(
            &self.root,
            &self.configuration,
            projects,
            &self.registered,
            RecommendationSelection::Available,
        )
    }
}

/// Resolves the same home expansion and canonical folder used by named project commands.
///
/// # Errors
///
/// Returns an error when the home directory is unavailable or the folder cannot be read.
pub(crate) fn registered_project_root(root: &Path, saved: &str) -> Result<PathBuf, RiftError> {
    let path = root.join("rift.toml");
    let selected = if saved == "~" || saved.starts_with("~/") {
        let home = std::env::home_dir().ok_or_else(|| {
            invalid(
                &path,
                "home directory is unavailable; use a project path without home expansion",
            )
        })?;
        home.join(saved.strip_prefix("~/").unwrap_or(""))
    } else {
        root.join(saved)
    };
    std::fs::canonicalize(selected).map_err(|error| unreadable(&path, error))
}

fn unreadable(path: &Path, error: std::io::Error) -> RiftError {
    errors::core::configuration_unreadable()
        .file("rift.toml")
        .path(path.display().to_string())
        .io(&error)
        .source(error)
        .error()
}

fn invalid(path: &Path, detail: &str) -> RiftError {
    errors::core::configuration_invalid()
        .file(path.display().to_string())
        .field("languages")
        .detail(detail)
        .error()
}

fn collect(
    root: &Path,
    paths: &[rift_core::ProjectPath],
    policy: &WorkspaceSourcePolicy,
) -> Result<BTreeMap<(PathBuf, String), PathBuf>, RiftError> {
    let roots = manifest_roots(paths);
    let mut projects = BTreeMap::new();
    for path in paths {
        let Some(language) = policy
            .language_policy()
            .language_for_path(Path::new(path.as_str()))?
        else {
            continue;
        };
        if !language.enabled() {
            continue;
        }
        let identity = language.identity();
        let Some(candidate) = Candidate::for_language(identity) else {
            continue;
        };
        let primary_cargo = candidate.manifest == "Cargo.toml"
            && roots.contains(&("Cargo.toml".to_owned(), PathBuf::new()));
        let project = if primary_cargo {
            PathBuf::new()
        } else {
            nearest_root(Path::new(path.as_str()), candidate.manifest, &roots)
        };
        projects
            .entry((root.join(project), identity.to_owned()))
            .or_insert_with(|| root.join(path.as_str()));
    }
    Ok(projects)
}

fn registration_roots(
    root: &Path,
    configuration: &WorkspaceConfiguration,
) -> Result<BTreeMap<String, PathBuf>, RiftError> {
    let path = root.join("rift.toml");
    let mut roots = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for (name, saved) in &configuration.mcp.extra {
        let project = registered_project_root(root, saved)?;
        if project == root || !project.is_dir() || !seen.insert(project.clone()) {
            return Err(invalid(
                &path,
                "registered projects must name distinct folders different from the primary project",
            ));
        }
        roots.insert(name.clone(), project);
    }
    for (name, project) in crate::profile_roots(root, configuration)? {
        if !seen.insert(project.clone()) {
            return Err(invalid(
                &path,
                "profiles and extra projects must name distinct folders",
            ));
        }
        roots.insert(format!("#{name}"), project);
    }
    Ok(roots)
}

fn grouped(
    root: &Path,
    projects: BTreeMap<(PathBuf, String), PathBuf>,
    registered: &BTreeMap<String, PathBuf>,
) -> BTreeMap<(PathBuf, String), PathBuf> {
    let candidates: BTreeSet<_> = projects
        .keys()
        .map(|(project, _)| project.clone())
        .collect();
    let mut grouped = BTreeMap::new();
    for ((project, language), origin) in projects {
        let registered_owner = registered
            .values()
            .filter(|owner| project.starts_with(owner))
            .max_by_key(|owner| owner.components().count());
        let proposed_owner = project
            .ancestors()
            .filter(|owner| *owner != root)
            .filter_map(|owner| candidates.get(owner))
            .last();
        let selected = registered_owner.or(proposed_owner).unwrap_or(&project);
        let overlaps_registered_child = registered
            .values()
            .any(|child| child != selected && child.starts_with(selected));
        let selected = if overlaps_registered_child && registered_owner.is_none() {
            root
        } else {
            selected
        };
        grouped
            .entry((selected.to_owned(), language))
            .or_insert(origin);
    }
    grouped
}

fn manifest_roots(paths: &[rift_core::ProjectPath]) -> BTreeSet<(String, PathBuf)> {
    paths
        .iter()
        .filter_map(|path| {
            let path = Path::new(path.as_str());
            let name = path.file_name()?.to_str()?;
            if !["Cargo.toml", "package.json", "pyproject.toml"].contains(&name) {
                return None;
            }
            Some((
                name.to_owned(),
                path.parent().unwrap_or_else(|| Path::new("")).to_owned(),
            ))
        })
        .collect()
}

fn nearest_root(path: &Path, manifest: &str, roots: &BTreeSet<(String, PathBuf)>) -> PathBuf {
    path.parent()
        .into_iter()
        .flat_map(Path::ancestors)
        .find(|parent| roots.contains(&(manifest.to_owned(), (*parent).to_owned())))
        .unwrap_or_else(|| Path::new(""))
        .to_owned()
}

struct Candidate {
    language: &'static str,
    program: &'static str,
    manifest: &'static str,
}

impl Candidate {
    fn for_language(language: &str) -> Option<Self> {
        match language {
            "rust" => Some(Self {
                language: "rust",
                program: "rust-analyzer",
                manifest: "Cargo.toml",
            }),
            "python" => Some(Self {
                language: "python",
                program: "ty",
                manifest: "pyproject.toml",
            }),
            "typescript" | "typescript:tsx" | "javascript" => Some(Self {
                language: "typescript",
                program: "typescript-language-server",
                manifest: "package.json",
            }),
            _ => None,
        }
    }

    fn installed(
        &self,
        root: &Path,
        search: Option<&OsStr>,
        dependencies: &rift_protocol::dependencies::DependenciesConfiguration,
    ) -> Option<(LspPackageManager, String)> {
        let manager = crate::package_manager::detect(self.language, root)?;
        crate::package_manager::recommended_command(manager, root, search, dependencies)
            .map(|command| (manager, command.program().to_owned()))
    }
}

fn decisions(
    root: &Path,
    primary: &WorkspaceConfiguration,
    projects: BTreeMap<(PathBuf, String), PathBuf>,
    registered: &BTreeMap<String, PathBuf>,
    selection: RecommendationSelection,
) -> Result<LspRecommendationBatch, RiftError> {
    let search = std::env::var_os("PATH");
    let mut registrations = registered
        .iter()
        .filter_map(|(name, project)| {
            name.strip_prefix('#')
                .map(|name| (name.to_owned(), project_name(root, project)))
        })
        .collect();
    let mut recommendations = LspRecommendationBatch {
        items: Vec::new(),
        truncated: false,
    };
    let mut bytes = 0_u64;
    for ((project, language), origin) in projects {
        let relative = project_name(root, &project);
        let nested;
        let configuration = if project == root {
            primary
        } else {
            nested = crate::load_configuration(&project)?;
            &nested
        };
        if configuration
            .languages
            .get(&language)
            .is_some_and(|entry| !entry.enabled)
        {
            continue;
        }
        let Some(candidate) = Candidate::for_language(&language) else {
            continue;
        };
        let origin = project_name(root, &origin);
        let status = if configured(configuration, &language) {
            LspRecommendationStatus::Configured
        } else if project != root
            && registered.values().any(|other| {
                other != &project && (other.starts_with(&project) || project.starts_with(other))
            })
        {
            LspRecommendationStatus::Overlapping
        } else if let Some((manager, program)) =
            candidate.installed(&project, search.as_deref(), &configuration.dependencies)
        {
            let extra = registered
                .iter()
                .find(|(name, path)| !name.starts_with('#') && **path == project);
            let scope = if relative == "." {
                SearchScope::Local
            } else if let Some((name, _)) = extra {
                SearchScope::NamedLocal { name: name.clone() }
            } else {
                SearchScope::ProfileLocal {
                    name: profile_name(&relative, &mut registrations),
                }
            };
            let mut command = command(
                &language,
                &relative,
                match &scope {
                    SearchScope::ProfileLocal { name } => Some(name.as_str()),
                    _ => None,
                },
                manager,
            );
            if extra.is_some() {
                command = format!("cd {} && {command}", quote(&project.to_string_lossy()));
            }
            LspRecommendationStatus::Available {
                program,
                command,
                scope,
            }
        } else {
            LspRecommendationStatus::Unavailable {
                program: candidate.program.to_owned(),
            }
        };
        let recommendation = LspRecommendation {
            language,
            root: relative,
            origin,
            status,
        };
        if !append_recommendation(
            &mut recommendations,
            &mut bytes,
            recommendation,
            selection,
            root,
        )? {
            break;
        }
    }
    Ok(recommendations)
}

fn append_recommendation(
    batch: &mut LspRecommendationBatch,
    bytes: &mut u64,
    recommendation: LspRecommendation,
    selection: RecommendationSelection,
    root: &Path,
) -> Result<bool, RiftError> {
    let command_bytes = match &recommendation.status {
        LspRecommendationStatus::Available { command, .. } => {
            if selection == RecommendationSelection::Available
                && (recommendation.root.chars().count() > MAP_RECOMMENDATION_ROOT_CHARS_MAX
                    || command.chars().count() > MAP_RECOMMENDATION_COMMAND_CHARS_MAX)
            {
                batch.truncated = true;
                return Ok(true);
            }
            command.len()
        }
        _ if selection == RecommendationSelection::Available => return Ok(true),
        _ => 0,
    };
    if selection == RecommendationSelection::Available
        && batch.items.len() == MAP_RECOMMENDATIONS_MAX
    {
        batch.truncated = true;
        return Ok(false);
    }
    let origin_bytes = if selection == RecommendationSelection::Complete {
        recommendation.origin.len()
    } else {
        0
    };
    *bytes = bytes.saturating_add(
        u64::try_from(
            recommendation.language.len()
                + recommendation.root.len()
                + origin_bytes
                + command_bytes,
        )
        .unwrap_or(u64::MAX),
    );
    if *bytes > CONFIGURATION_FILE_BYTES_MAX {
        if selection == RecommendationSelection::Available {
            batch.truncated = true;
            return Ok(false);
        }
        return Err(invalid(
            &root.join("rift.toml"),
            "recommendations exceed the configuration byte limit; narrow source.include and retry",
        ));
    }
    batch.items.push(recommendation);
    Ok(true)
}

fn configured(configuration: &WorkspaceConfiguration, language: &str) -> bool {
    configuration
        .languages
        .get(language)
        .is_some_and(|entry| entry.lsp.is_some())
}

fn project_name(root: &Path, project: &Path) -> String {
    project
        .strip_prefix(root)
        .map_or_else(|_| project.to_string_lossy().into_owned(), slash)
}

fn command(
    language: &str,
    root: &str,
    profile: Option<&str>,
    manager: LspPackageManager,
) -> String {
    let mut arguments = vec![
        format!("--language={language}"),
        format!("--lsp-package-manager={}", manager.as_str()),
    ];
    if let Some(profile) = profile {
        arguments.push(format!("--profile={profile}"));
        arguments.push(format!("--profile-root={root}"));
    }
    format!(
        "rift config set {}",
        arguments
            .iter()
            .map(|argument| quote(argument))
            .collect::<Vec<_>>()
            .join(" ")
    )
}

fn profile_name(root: &str, registrations: &mut BTreeMap<String, String>) -> String {
    if let Some((name, _)) = registrations
        .iter()
        .find(|(_, registered)| registered.as_str() == root)
    {
        return name.clone();
    }
    let root_available =
        rift_protocol::identity::valid_registration_name(root) && !registrations.contains_key(root);
    let name = if root_available {
        root.to_owned()
    } else {
        (1..=registrations.len().saturating_add(1))
            .map(|index| format!("project-{index}"))
            .find(|name| !registrations.contains_key(name))
            .unwrap_or_else(|| {
                unreachable!("one more name than registrations always has a free entry")
            })
    };
    registrations.insert(name.clone(), root.to_owned());
    name
}

fn slash(path: &Path) -> String {
    let path = path
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if path.is_empty() {
        ".".to_owned()
    } else {
        path
    }
}

fn quote(value: &str) -> String {
    if cfg!(windows) {
        format!("'{}'", value.replace('\'', "''"))
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recommendation_inputs_refuse_overlapping_profile_roots()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        std::fs::create_dir_all(root.path().join("docs/api"))?;
        let configuration: WorkspaceConfiguration = serde_json::from_value(serde_json::json!({
            "profiles":{"docs":{"root":"docs"},"api":{"root":"docs/api"}}
        }))?;
        let error = LspRecommendationInputs::new(root.path(), configuration)
            .err()
            .ok_or("overlapping profile roots must be refused")?;
        assert_eq!(error.slug(), errors::core::configuration_invalid::SLUG);
        let context: BTreeMap<_, _> = error.context().collect();
        assert_eq!(context.get("field").map(String::as_str), Some("profiles"));
        assert_eq!(
            context.get("detail").map(String::as_str),
            Some("profile roots overlap; select separate project folders")
        );
        Ok(())
    }

    #[test]
    fn profile_and_extra_registration_keep_same_name_distinct_and_refuse_shared_root()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let extra = tempfile::tempdir()?;
        std::fs::create_dir(root.path().join("docs"))?;
        let profile_root = root.path().join("docs").canonicalize()?;
        let extra_root = extra.path().canonicalize()?;
        let mut configuration: WorkspaceConfiguration =
            serde_json::from_value(serde_json::json!({
                "profiles":{"docs":{"root":"docs"}},
                "mcp":{"extra":{"docs":extra_root.to_string_lossy()}}
            }))?;
        configuration
            .validate()
            .map_err(|violation| rift_core::configuration_violation_error(&violation))?;
        let inputs = LspRecommendationInputs::new(root.path(), configuration.clone())?;
        assert_eq!(
            inputs.registered_projects().get("#docs"),
            Some(&profile_root)
        );
        assert_eq!(inputs.registered_projects().get("docs"), Some(&extra_root));
        assert_eq!(inputs.registered_projects().len(), 2);
        configuration.mcp.extra.insert(
            "docs".to_owned(),
            profile_root.to_string_lossy().into_owned(),
        );
        let error = LspRecommendationInputs::new(root.path(), configuration)
            .err()
            .ok_or("profile and extra must not share one root")?;
        assert_eq!(error.slug(), errors::core::configuration_invalid::SLUG);
        let context: BTreeMap<_, _> = error.context().collect();
        assert_eq!(
            context.get("detail").map(String::as_str),
            Some("profiles and extra projects must name distinct folders")
        );
        Ok(())
    }

    fn project_entries(
        root: &Path,
        entries: &[(&str, &str, &str)],
    ) -> BTreeMap<(PathBuf, String), PathBuf> {
        entries
            .iter()
            .map(|(project, language, origin)| {
                (
                    (root.join(project), (*language).to_owned()),
                    root.join(origin),
                )
            })
            .collect()
    }

    #[test]
    fn test_grouped_keeps_outermost_projects_across_languages_and_separates_siblings() {
        let root = Path::new("workspace");
        let projects = project_entries(
            root,
            &[
                ("", "rust", "src/lib.rs"),
                ("docs", "python", "docs/main.py"),
                ("docs/examples", "typescript", "docs/examples/main.ts"),
                (
                    "docs/examples/deep",
                    "javascript",
                    "docs/examples/deep/main.js",
                ),
                ("docs-other", "typescript", "docs-other/main.ts"),
                ("tools", "typescript", "tools/main.ts"),
            ],
        );
        let expected = project_entries(
            root,
            &[
                ("", "rust", "src/lib.rs"),
                ("docs", "python", "docs/main.py"),
                ("docs", "typescript", "docs/examples/main.ts"),
                ("docs", "javascript", "docs/examples/deep/main.js"),
                ("docs-other", "typescript", "docs-other/main.ts"),
                ("tools", "typescript", "tools/main.ts"),
            ],
        );
        assert_eq!(grouped(root, projects, &BTreeMap::new()), expected);
    }

    #[test]
    fn test_grouped_preserves_registered_child_ownership_and_parent_fallback() {
        let root = Path::new("workspace");
        let projects = project_entries(
            root,
            &[
                ("", "rust", "src/lib.rs"),
                ("docs", "python", "docs/main.py"),
                ("docs/examples", "typescript", "docs/examples/main.ts"),
                (
                    "docs/examples/deep",
                    "javascript",
                    "docs/examples/deep/main.js",
                ),
            ],
        );
        let registered = BTreeMap::from([("examples".to_owned(), root.join("docs/examples"))]);
        let expected = project_entries(
            root,
            &[
                ("", "rust", "src/lib.rs"),
                ("", "python", "docs/main.py"),
                ("docs/examples", "typescript", "docs/examples/main.ts"),
                ("docs/examples", "javascript", "docs/examples/deep/main.js"),
            ],
        );
        assert_eq!(grouped(root, projects, &registered), expected);
    }

    #[cfg(unix)]
    fn available_inputs(
        root: &Path,
        count: usize,
    ) -> Result<LspRecommendationInputs, Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt as _;
        let mut paths = Vec::new();
        for index in 0..count {
            let folder = format!("project-{index:03}");
            let project = root.join(&folder);
            std::fs::create_dir_all(project.join("node_modules/.bin"))?;
            std::fs::write(project.join("package.json"), "{}")?;
            std::fs::write(project.join("main.ts"), "export const value = 1;")?;
            let executable = project.join("node_modules/.bin/typescript-language-server");
            std::fs::write(
                &executable,
                format!(
                    "#!/bin/sh\nprintf invoked > '{}'\nexit 91\n",
                    root.join("engine-executed").display()
                ),
            )?;
            std::fs::set_permissions(executable, std::fs::Permissions::from_mode(0o755))?;
            for name in ["package.json", "main.ts"] {
                paths.push(rift_core::ProjectPath::new(format!("{folder}/{name}"))?);
            }
        }
        let configuration = WorkspaceConfiguration::default();
        let policy = WorkspaceSourcePolicy::build_with_languages(
            root,
            rift_index::WorkspaceIndexLimits::default(),
            &rift_core::SourceVisibility::from(&configuration.source),
            &rift_core::TextFileInclusion::from(&configuration),
            &rift_core::LanguageFileSelections::from(&configuration),
        )?;
        let mut inputs = LspRecommendationInputs::new(root, configuration)?;
        inputs.include_project(root, &paths, &policy)?;
        Ok(inputs)
    }

    #[cfg(unix)]
    #[test]
    fn test_available_batches_retain_below_exact_and_over_count_bounds_without_launching()
    -> Result<(), Box<dyn std::error::Error>> {
        for count in [
            MAP_RECOMMENDATIONS_MAX - 1,
            MAP_RECOMMENDATIONS_MAX,
            MAP_RECOMMENDATIONS_MAX + 1,
        ] {
            let root = tempfile::tempdir()?;
            let batch = available_inputs(root.path(), count)?.finish_available()?;
            assert_eq!(batch.items.len(), count.min(MAP_RECOMMENDATIONS_MAX));
            assert_eq!(batch.truncated, count > MAP_RECOMMENDATIONS_MAX);
            assert!(
                batch
                    .items
                    .iter()
                    .all(|item| matches!(item.status, LspRecommendationStatus::Available { .. }))
            );
            assert!(!root.path().join("engine-executed").exists());
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_configured_projects_do_not_use_available_batch_capacity()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let inputs = available_inputs(root.path(), MAP_RECOMMENDATIONS_MAX + 1)?;
        for index in 0..MAP_RECOMMENDATIONS_MAX {
            std::fs::write(
                root.path().join(format!("project-{index:03}/rift.toml")),
                "[languages.typescript]\nlsp.command = 'selected-server'\n",
            )?;
        }
        let batch = inputs.finish_available()?;
        assert_eq!(batch.items.len(), 1);
        assert_eq!(batch.items[0].root, "project-100");
        assert!(!batch.truncated);
        assert!(!root.path().join("engine-executed").exists());
        Ok(())
    }

    fn available_item(root: String, command: String) -> LspRecommendation {
        LspRecommendation {
            language: "rust".to_owned(),
            root,
            origin: "captured.rs".to_owned(),
            status: LspRecommendationStatus::Available {
                program: "rust-analyzer".to_owned(),
                command,
                scope: SearchScope::Local,
            },
        }
    }

    #[test]
    fn test_available_batch_character_bounds_report_omitted_commands()
    -> Result<(), Box<dyn std::error::Error>> {
        for (root_chars, command_chars, retained) in [
            (
                MAP_RECOMMENDATION_ROOT_CHARS_MAX - 1,
                MAP_RECOMMENDATION_COMMAND_CHARS_MAX - 1,
                true,
            ),
            (
                MAP_RECOMMENDATION_ROOT_CHARS_MAX,
                MAP_RECOMMENDATION_COMMAND_CHARS_MAX,
                true,
            ),
            (MAP_RECOMMENDATION_ROOT_CHARS_MAX + 1, 1, false),
            (1, MAP_RECOMMENDATION_COMMAND_CHARS_MAX + 1, false),
        ] {
            let mut batch = LspRecommendationBatch {
                items: Vec::new(),
                truncated: false,
            };
            let mut bytes = 0;
            append_recommendation(
                &mut batch,
                &mut bytes,
                available_item("r".repeat(root_chars), "c".repeat(command_chars)),
                RecommendationSelection::Available,
                Path::new("."),
            )?;
            assert_eq!(batch.items.len(), usize::from(retained));
            assert_eq!(batch.truncated, !retained);
        }
        Ok(())
    }

    #[test]
    fn test_available_batch_byte_bound_retains_existing_commands()
    -> Result<(), Box<dyn std::error::Error>> {
        let maximum = usize::try_from(CONFIGURATION_FILE_BYTES_MAX)?;
        for total in [maximum - 1, maximum, maximum + 1] {
            let mut batch = LspRecommendationBatch {
                items: Vec::new(),
                truncated: false,
            };
            let mut bytes = 0;
            // Thirty-two rows contribute five bytes each for language and root.
            let command_bytes = total - 32 * 5;
            for index in 0..32 {
                let length = if index == 31 {
                    command_bytes - 31 * MAP_RECOMMENDATION_COMMAND_CHARS_MAX
                } else {
                    MAP_RECOMMENDATION_COMMAND_CHARS_MAX
                };
                if !append_recommendation(
                    &mut batch,
                    &mut bytes,
                    available_item(".".to_owned(), "c".repeat(length)),
                    RecommendationSelection::Available,
                    Path::new("."),
                )? {
                    break;
                }
            }
            assert_eq!(batch.items.len(), if total > maximum { 31 } else { 32 });
            assert_eq!(batch.truncated, total > maximum);
        }
        Ok(())
    }

    fn captured(
        root: &Path,
        configuration: WorkspaceConfiguration,
        path: &str,
    ) -> Result<Vec<LspRecommendation>, RiftError> {
        let policy = WorkspaceSourcePolicy::build_with_languages(
            root,
            rift_index::WorkspaceIndexLimits::default(),
            &rift_core::SourceVisibility::from(&configuration.source),
            &rift_core::TextFileInclusion::from(&configuration),
            &rift_core::LanguageFileSelections::from(&configuration),
        )?;
        let mut inputs = LspRecommendationInputs::new(root, configuration)?;
        inputs.include_project(root, &[rift_core::ProjectPath::new(path)?], &policy)?;
        inputs.finish()
    }

    #[test]
    fn test_primary_uses_accepted_configuration_and_captured_paths()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let accepted = crate::load_configuration(root.path())?;
        std::fs::write(
            root.path().join("rift.toml"),
            "[languages.typescript]\nlsp.command = 'selected-server'\n",
        )?;
        let unconfigured = captured(root.path(), accepted, "captured.ts")?;
        assert_eq!(unconfigured.len(), 1);
        assert_ne!(unconfigured[0].status, LspRecommendationStatus::Configured);
        assert!(!root.path().join("captured.ts").exists());
        let accepted = crate::load_configuration(root.path())?;
        std::fs::remove_file(root.path().join("rift.toml"))?;
        let configured = captured(root.path(), accepted, "captured.ts")?;
        assert_eq!(configured.len(), 1);
        assert_eq!(configured[0].status, LspRecommendationStatus::Configured);
        Ok(())
    }

    #[test]
    fn test_named_and_inline_embedded_selections_are_configured()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        for document in [
            "[languages.python]\nlsp.embedded = 'ty'\n",
            "[lsp.ty]\nembedded = 'ty'\n[languages.python]\nlsp = 'ty'\n",
        ] {
            std::fs::write(root.path().join("rift.toml"), document)?;
            let configured = captured(
                root.path(),
                crate::load_configuration(root.path())?,
                "captured.py",
            )?;
            assert_eq!(configured.len(), 1, "{document}");
            assert_eq!(
                configured[0].status,
                LspRecommendationStatus::Configured,
                "{document}"
            );
        }
        std::fs::write(
            root.path().join("rift.toml"),
            "[languages.python]\nenabled = false\nlsp.embedded = 'ty'\n",
        )?;
        assert!(
            captured(
                root.path(),
                crate::load_configuration(root.path())?,
                "captured.py",
            )?
            .is_empty()
        );
        Ok(())
    }

    #[test]
    fn test_shared_recommendation_bytes_are_bounded() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let origin = root
            .path()
            .join("x".repeat(usize::try_from(CONFIGURATION_FILE_BYTES_MAX)? + 1));
        let projects =
            BTreeMap::from([((root.path().to_owned(), "typescript".to_owned()), origin)]);
        let error = decisions(
            root.path(),
            &WorkspaceConfiguration::default(),
            projects,
            &BTreeMap::new(),
            RecommendationSelection::Complete,
        )
        .expect_err("shared results must fit the configuration byte bound");
        assert_eq!(error.slug().as_str(), "rift.core.configuration_invalid");
        Ok(())
    }

    #[test]
    fn test_recommendation_command_selects_manager_and_profile_root() {
        let command = command(
            "typescript:tsx",
            "docs",
            Some("docs"),
            LspPackageManager::Bun,
        );
        assert!(command.contains("'--profile=docs'"));
        assert!(command.contains("'--profile-root=docs'"));
        assert!(command.contains("'--language=typescript:tsx'"));
        assert!(command.contains("'--lsp-package-manager=bun'"));
    }

    #[cfg(unix)]
    #[test]
    fn test_local_executable_precedes_path_without_running_it()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir()?;
        let local = root.path().join("node_modules/.bin");
        std::fs::create_dir_all(&local)?;
        let executable = local.join("typescript-language-server");
        std::fs::write(&executable, "#!/bin/sh\nexit 91\n")?;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))?;
        let candidate = Candidate::for_language("typescript").expect("supported language");
        assert_eq!(
            candidate
                .installed(
                    root.path(),
                    Some(OsStr::new("/missing")),
                    &rift_protocol::dependencies::DependenciesConfiguration::default()
                )
                .map(|(_, program)| program)
                .as_deref(),
            Some("node_modules/.bin/typescript-language-server")
        );
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o644))?;
        assert_eq!(
            candidate.installed(
                root.path(),
                Some(OsStr::new("/missing")),
                &rift_protocol::dependencies::DependenciesConfiguration::default()
            ),
            None
        );
        Ok(())
    }
}
