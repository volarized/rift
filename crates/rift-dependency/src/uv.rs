//! The uv resolver: Python distributions as `uv.lock` pins them and the project environment holds them.

mod environment;
#[cfg(test)]
mod fixture;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rift_protocol::read::{Language, ProjectPath};
use serde::Deserialize;

use crate::catalog::{CatalogEntry, Resolution, package_identity};
use crate::manifest::{
    LockfileFailure, ResolutionBuilder, file_beside, is_ancestor_directory, manifest_directory,
    manifest_directory_path, read_lockfile,
};
use crate::resolver::{DependencyResolver, Inspector, ResolutionRequest, ResolverName};

/// The package namespace every uv dependency entry belongs to.
const PYPI_MANAGER: &str = "pypi";
/// The manifest file name this resolver claims.
const UV_MANIFEST_FILE_NAME: &str = "pyproject.toml";
/// The lockfile uv keeps beside a workspace root manifest.
const UV_LOCK_FILE_NAME: &str = "uv.lock";
/// The characters PEP 503 folds into one `-` when normalizing a distribution name.
const NAME_SEPARATORS: [char; 3] = ['-', '_', '.'];
/// The separator a normalized distribution name keeps.
const NORMALIZED_SEPARATOR: char = '-';
/// The language every cataloged package's source is parsed as.
const PYTHON_LANGUAGE_NAME: &str = "python";

/// The resolver for Python distributions, answering from `uv.lock` and the project environment.
#[derive(Debug, Default)]
pub struct UvResolver;

impl UvResolver {
    /// The uv resolver. It holds no state, so one instance serves every workspace.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl DependencyResolver for UvResolver {
    fn name(&self) -> ResolverName {
        ResolverName::Uv
    }

    fn manager(&self) -> &'static str {
        PYPI_MANAGER
    }

    fn language(&self) -> Language {
        python_language()
    }

    fn manifest_file_name(&self) -> &'static str {
        UV_MANIFEST_FILE_NAME
    }

    /// Catalogs the distributions the request's manifests resolve to.
    ///
    /// A manifest with `uv.lock` beside it is a lockfile root, resolved from that
    /// lockfile. A manifest without one is covered by a listed manifest in an ancestor
    /// directory that has one, since a uv workspace keeps one `uv.lock` at its root; a
    /// manifest with no such ancestor is reported unresolved. Every listed manifest and
    /// every root's `uv.lock` is an input. Each root's packages take their source roots
    /// from the project environment beside its manifest, and that environment's
    /// interpreter is cataloged as the standard library, once per root;
    /// `DependencyCatalog::assemble` merges equal identities across roots. Entries stop
    /// at `PACKAGES_MAX` and the drop is reported. Every manifest reads its lockfile
    /// once, and the coverage check compares every manifest pair, so that work is
    /// quadratic in the manifest count, which `MANIFESTS_MAX` bounds.
    fn resolve(
        &self,
        request: &ResolutionRequest<'_>,
        inspector: &mut dyn Inspector,
    ) -> Resolution {
        let mut answer = ResolutionBuilder::default();
        for manifest in request.manifests {
            answer.input(manifest.clone());
        }
        let mut observed = Vec::with_capacity(request.manifests.len());
        for manifest in request.manifests {
            observed.push(observe_manifest(request.root, manifest, inspector));
        }
        for manifest in &observed {
            resolve_observed(manifest, &observed, inspector, &mut answer);
        }
        answer.build()
    }
}

/// One listed manifest, its absolute directory, and what stood beside it.
struct ObservedManifest<'a> {
    manifest: &'a ProjectPath,
    directory: PathBuf,
    lockfile: LockfileBeside,
}

impl ObservedManifest<'_> {
    /// Whether a `uv.lock` stands beside the manifest, parsed or refused.
    fn is_root(&self) -> bool {
        matches!(self.lockfile, LockfileBeside::Root(_))
    }
}

/// What stood beside one listed manifest.
enum LockfileBeside {
    /// No `uv.lock`: an ancestor root covers the manifest, or nothing resolves it.
    Absent,
    /// The `uv.lock` the manifest is the root of, parsed or refused.
    Root(Result<Lockfile, LockfileFailure>),
}

/// Reads the `uv.lock` beside one listed manifest, parsing it when it stands there.
fn observe_manifest<'a>(
    root: &Path,
    manifest: &'a ProjectPath,
    inspector: &mut dyn Inspector,
) -> ObservedManifest<'a> {
    let directory = manifest_directory_path(root, manifest);
    let lockfile = match read_lockfile(&directory, UV_LOCK_FILE_NAME, inspector) {
        Err(failure) if failure.is_absent() => LockfileBeside::Absent,
        observed => LockfileBeside::Root(observed.and_then(|bytes| parse_lockfile(&bytes))),
    };
    ObservedManifest {
        manifest,
        directory,
        lockfile,
    }
}

/// Parses `uv.lock` bytes, naming the parser's message when they are not its document.
fn parse_lockfile(bytes: &[u8]) -> Result<Lockfile, LockfileFailure> {
    toml::from_slice(bytes)
        .map_err(|error| LockfileFailure::unparsable(UV_LOCK_FILE_NAME, error.message().to_owned()))
}

/// Resolves one listed manifest: a root from its lockfile, a covered manifest not at all.
fn resolve_observed(
    observed: &ObservedManifest<'_>,
    all: &[ObservedManifest<'_>],
    inspector: &mut dyn Inspector,
    answer: &mut ResolutionBuilder,
) {
    let manifest_path = observed.manifest.0.as_str();
    match &observed.lockfile {
        LockfileBeside::Absent if is_covered(observed, all) => {}
        LockfileBeside::Absent => answer.degradation(format!(
            "{manifest_path}: no {UV_LOCK_FILE_NAME} beside it or above it; not resolved"
        )),
        LockfileBeside::Root(Ok(lockfile)) => {
            answer.input(file_beside(observed.manifest, UV_LOCK_FILE_NAME));
            resolve_root(observed, lockfile, inspector, answer);
        }
        LockfileBeside::Root(Err(failure)) => {
            answer.input(file_beside(observed.manifest, UV_LOCK_FILE_NAME));
            answer.degradation(format!("{manifest_path}: {failure}; no packages cataloged"));
        }
    }
}

/// Whether a listed root stands in an ancestor directory of the manifest.
fn is_covered(observed: &ObservedManifest<'_>, all: &[ObservedManifest<'_>]) -> bool {
    let directory = manifest_directory(observed.manifest);
    all.iter()
        .filter(|candidate| candidate.is_root())
        .any(|root| is_ancestor_directory(manifest_directory(root.manifest), directory))
}

/// The `uv.lock` document, the fields this resolver reads.
///
/// The top-level `[manifest]` table and every package field past these are ignored.
#[derive(Deserialize)]
struct Lockfile {
    #[serde(default)]
    package: Vec<LockedPackage>,
}

/// One `[[package]]` table of the lockfile.
///
/// A package whose source is `editable` or `virtual` is one of the workspace's own
/// projects: it is not cataloged, and its `dependencies` and dependency groups decide
/// which packages the workspace declares directly. Every other source is a resolved
/// dependency. A project with a dynamic version carries no `version`.
#[derive(Deserialize)]
struct LockedPackage {
    name: String,
    version: Option<String>,
    source: LockedSource,
    #[serde(default)]
    dependencies: Vec<LockedDependency>,
    #[serde(default, rename = "dev-dependencies")]
    dev_dependencies: BTreeMap<String, Vec<LockedDependency>>,
}

/// Where a locked package came from; only the two member-marking keys are read.
#[derive(Deserialize)]
struct LockedSource {
    editable: Option<String>,
    #[serde(rename = "virtual")]
    virtual_directory: Option<String>,
}

impl LockedSource {
    /// Whether the package is one of the workspace's own projects.
    fn is_member(&self) -> bool {
        self.editable.is_some() || self.virtual_directory.is_some()
    }
}

/// One dependency edge of a locked package; only the depended-on name is read.
#[derive(Deserialize)]
struct LockedDependency {
    name: String,
}

/// Catalogs one root's packages, rooted in the project environment beside its manifest.
fn resolve_root(
    observed: &ObservedManifest<'_>,
    lockfile: &Lockfile,
    inspector: &mut dyn Inspector,
    answer: &mut ResolutionBuilder,
) {
    let manifest_path = observed.manifest.0.as_str();
    let directory = environment::environment_directory(&observed.directory, inspector);
    let environment = match environment::ProjectEnvironment::observe(directory, inspector) {
        Ok(environment) => environment,
        Err(failure) => {
            answer.degradation(format!(
                "{manifest_path}: {failure}; packages cataloged without source roots"
            ));
            catalog_packages(lockfile, None, manifest_path, inspector, answer);
            return;
        }
    };
    let site_packages = match environment.site_packages(inspector) {
        Ok(site_packages) => Some(site_packages),
        Err(failure) => {
            answer.degradation(format!(
                "{manifest_path}: {failure}; packages cataloged without source roots"
            ));
            None
        }
    };
    catalog_packages(
        lockfile,
        site_packages.as_ref(),
        manifest_path,
        inspector,
        answer,
    );
    match environment.stdlib_entry(inspector) {
        Ok(entry) => answer.entry(entry),
        Err(failure) => answer.degradation(format!(
            "{manifest_path}: {failure}; no standard library entry"
        )),
    }
}

/// Catalogs every package outside the workspace members, direct when a member depends on it.
///
/// Source roots come from `site_packages` when the environment has one. The work is one
/// pass over the lockfile's packages plus what each root lookup reads.
fn catalog_packages(
    lockfile: &Lockfile,
    site_packages: Option<&environment::SitePackages>,
    manifest_path: &str,
    inspector: &mut dyn Inspector,
    answer: &mut ResolutionBuilder,
) {
    let direct = member_dependencies(lockfile);
    let dependencies = lockfile
        .package
        .iter()
        .filter(|package| !package.source.is_member());
    for package in dependencies {
        let name = normalized_name(&package.name);
        let Some(version) = package.version.as_deref() else {
            answer.degradation(format!(
                "{manifest_path}: {UV_LOCK_FILE_NAME} pins {name} without a version; not cataloged"
            ));
            continue;
        };
        let source_root = site_packages
            .and_then(|site_packages| site_packages.source_root(inspector, &name, version));
        answer.entry(CatalogEntry::dependency(
            package_identity(PYPI_MANAGER, &name, version),
            python_language(),
            source_root,
            direct.contains(&name),
        ));
    }
}

/// The normalized names every member's dependencies and dependency groups name.
fn member_dependencies(lockfile: &Lockfile) -> BTreeSet<String> {
    lockfile
        .package
        .iter()
        .filter(|package| package.source.is_member())
        .flat_map(|member| {
            member
                .dependencies
                .iter()
                .chain(member.dev_dependencies.values().flatten())
        })
        .map(|dependency| normalized_name(&dependency.name))
        .collect()
}

/// The PEP 503 normalized form of a distribution name: lowercase, every separator run one `-`.
fn normalized_name(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len());
    let mut separator_pending = false;
    for character in name.to_lowercase().chars() {
        if NAME_SEPARATORS.contains(&character) {
            separator_pending = true;
            continue;
        }
        if separator_pending {
            normalized.push(NORMALIZED_SEPARATOR);
            separator_pending = false;
        }
        normalized.push(character);
    }
    if separator_pending {
        normalized.push(NORMALIZED_SEPARATOR);
    }
    normalized
}

/// The Python language, with no dialect.
fn python_language() -> Language {
    Language {
        name: PYTHON_LANGUAGE_NAME.to_owned(),
        dialect: None,
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::path::Path;

    use super::fixture::{
        ENVIRONMENT, ENVIRONMENT_FILE, REGISTRY, ROOT, SITE_PACKAGES, asked_count, entry,
        environment_inspector, lockfile_reads, names, project, resolve, single_package_lockfile,
    };
    use super::*;
    use crate::catalog::PackageLocation;
    use crate::fixture::RecordedInspector;
    use crate::resolver::{LOCKFILE_BYTES_MAX, PACKAGES_MAX};

    #[test]
    fn test_uv_resolver_identity_names_uv_pypi_and_python() {
        let resolver = UvResolver::new();
        assert_eq!(resolver.name(), ResolverName::Uv);
        assert_eq!(resolver.manager(), "pypi");
        assert_eq!(resolver.language().identity_segment(), "python");
        assert_eq!(resolver.manifest_file_name(), "pyproject.toml");
    }

    #[test]
    fn test_resolve_catalogs_registry_packages_with_member_declared_directness() {
        let mut inspector = environment_inspector();

        let resolution = resolve(&["pyproject.toml"], &mut inspector);

        assert_eq!(
            names(&resolution),
            [
                "pypi/colorama@0.4.6",
                "pypi/markdown-it-py@4.2.0",
                "pypi/mdurl@0.1.2",
                "pypi/pluggy@1.6.0",
                "pypi/py@1.11.0",
                "pypi/pytest@9.1.1",
                "pypi/typer@0.27.1",
                "stdlib/python@3.14.0",
            ]
        );
        assert!(
            entry(&resolution, "typer").is_direct(),
            "the editable member depends on typer"
        );
        assert!(
            entry(&resolution, "colorama").is_direct(),
            "the virtual member depends on colorama"
        );
        assert!(
            entry(&resolution, "pytest").is_direct(),
            "a dependency group counts as direct"
        );
        assert!(
            !entry(&resolution, "mdurl").is_direct(),
            "only markdown-it-py depends on mdurl"
        );
        assert!(
            !entry(&resolution, "pluggy").is_direct(),
            "only pytest depends on pluggy"
        );
        let typer = entry(&resolution, "typer");
        assert_eq!(typer.location(), PackageLocation::Dependency);
        assert_eq!(typer.language().identity_segment(), "python");
        assert_eq!(
            resolution.inputs,
            [project("pyproject.toml"), project("uv.lock")]
        );
        assert!(
            resolution.degradations.is_empty(),
            "{:?}",
            resolution.degradations
        );
        assert_eq!(
            asked_count(&inspector, &format!("list {SITE_PACKAGES}")),
            1,
            "site-packages is listed once per root"
        );
    }

    #[test]
    fn test_resolve_normalizes_identity_and_dependency_names_per_pep_503() {
        let lockfile = "[[package]]\nname = \"Markdown_It.py\"\nversion = \"4.2.0\"\n\
                        source = { registry = \"https://pypi.org/simple\" }\n\n\
                        [[package]]\nname = \"app\"\nversion = \"0.1.0\"\n\
                        source = { editable = \".\" }\n\
                        dependencies = [\n    { name = \"markdown.it_py\" },\n]\n";
        let mut inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/uv.lock"), lockfile)
            .with_file(format!("{ENVIRONMENT}/pyvenv.cfg"), ENVIRONMENT_FILE)
            .with_file(
                format!("{SITE_PACKAGES}/markdown_it_py-4.2.0.dist-info/top_level.txt"),
                "markdown_it\n",
            )
            .with_directory(format!("{SITE_PACKAGES}/markdown_it"));

        let resolution = resolve(&["pyproject.toml"], &mut inspector);

        assert_eq!(
            names(&resolution),
            ["pypi/markdown-it-py@4.2.0", "stdlib/python@3.14.0"]
        );
        let markdown_it = entry(&resolution, "markdown-it-py");
        assert!(
            markdown_it.is_direct(),
            "the dependency name normalizes too"
        );
        assert_eq!(
            markdown_it.source_root(),
            Some(Path::new(
                "/workspace/.venv/lib/python3.14t/site-packages/markdown_it"
            ))
        );
    }

    #[test]
    fn test_resolve_nested_manifest_without_lockfile_is_covered_by_the_root() {
        let mut inspector = environment_inspector();

        let resolution = resolve(
            &["apps/api/pyproject.toml", "pyproject.toml"],
            &mut inspector,
        );

        assert!(
            resolution.degradations.is_empty(),
            "{:?}",
            resolution.degradations
        );
        assert_eq!(resolution.entries.len(), 8);
        assert_eq!(
            resolution.inputs,
            [
                project("apps/api/pyproject.toml"),
                project("pyproject.toml"),
                project("uv.lock"),
            ]
        );
        assert_eq!(
            lockfile_reads(&inspector),
            [
                "read /workspace/apps/api/uv.lock",
                "read /workspace/uv.lock"
            ],
            "each lockfile path is read once: the nested probe answers absent"
        );
    }

    #[test]
    fn test_resolve_nested_manifest_with_lockfile_resolves_separately() {
        let mut inspector = environment_inspector().with_file(
            format!("{ROOT}/tools/uv.lock"),
            single_package_lockfile("colorama", "0.4.6"),
        );

        let resolution = resolve(&["pyproject.toml", "tools/pyproject.toml"], &mut inspector);

        assert_eq!(
            resolution.degradations,
            [
                "tools/pyproject.toml: no environment at /workspace/tools/.venv; packages \
                 cataloged without source roots"
            ]
        );
        assert_eq!(
            resolution.entries.len(),
            9,
            "one entry per root that pinned a package; assembly merges identities"
        );
        assert_eq!(
            resolution.inputs,
            [
                project("pyproject.toml"),
                project("tools/pyproject.toml"),
                project("uv.lock"),
                project("tools/uv.lock"),
            ]
        );
        assert_eq!(
            lockfile_reads(&inspector),
            ["read /workspace/uv.lock", "read /workspace/tools/uv.lock"]
        );
    }

    #[test]
    fn test_resolve_manifests_without_lockfile_in_their_chain_degrade() {
        let mut inspector = RecordedInspector::default().with_directory(ROOT);

        let resolution = resolve(&["apps/x/pyproject.toml", "pyproject.toml"], &mut inspector);

        assert_eq!(
            resolution.degradations,
            [
                "apps/x/pyproject.toml: no uv.lock beside it or above it; not resolved",
                "pyproject.toml: no uv.lock beside it or above it; not resolved",
            ]
        );
        assert!(resolution.entries.is_empty());
        assert_eq!(
            resolution.inputs,
            [project("apps/x/pyproject.toml"), project("pyproject.toml")]
        );
    }

    #[test]
    fn test_resolve_lockfile_over_bound_degrades_without_entries() {
        let oversized = vec![b'#'; usize::try_from(LOCKFILE_BYTES_MAX).expect("bound fits") + 1];
        let mut inspector =
            RecordedInspector::default().with_file(format!("{ROOT}/uv.lock"), oversized);

        let resolution = resolve(&["pyproject.toml"], &mut inspector);

        assert_eq!(
            resolution.degradations,
            [format!(
                "pyproject.toml: uv.lock holds {} bytes, past the {LOCKFILE_BYTES_MAX} byte \
                 bound; no packages cataloged",
                LOCKFILE_BYTES_MAX + 1
            )]
        );
        assert!(resolution.entries.is_empty());
        assert_eq!(
            resolution.inputs,
            [project("pyproject.toml"), project("uv.lock")]
        );
    }

    #[test]
    fn test_resolve_lockfile_unparsable_degrades_without_entries() {
        let mut inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/uv.lock"), "[[package]]\nname = ");

        let resolution = resolve(&["pyproject.toml"], &mut inspector);

        let degradation = &resolution.degradations[0];
        assert!(
            degradation.starts_with("pyproject.toml: uv.lock could not be parsed: "),
            "{degradation}"
        );
        assert!(
            degradation.ends_with("; no packages cataloged"),
            "{degradation}"
        );
        assert!(resolution.entries.is_empty());
        assert!(
            !inspector
                .asked
                .iter()
                .any(|line| line.ends_with("pyvenv.cfg")),
            "a refused lockfile never reaches the environment"
        );
    }

    #[test]
    fn test_resolve_package_without_version_degrades_and_is_not_cataloged() {
        let lockfile = "[[package]]\nname = \"app\"\nversion = \"0.1.0\"\n\
                        source = { editable = \".\" }\n\n\
                        [[package]]\nname = \"local-lib\"\n\
                        source = { directory = \"../local-lib\" }\n";
        let mut inspector =
            RecordedInspector::default().with_file(format!("{ROOT}/uv.lock"), lockfile);

        let resolution = resolve(&["pyproject.toml"], &mut inspector);

        assert_eq!(
            resolution.degradations,
            [
                "pyproject.toml: no environment at /workspace/.venv; packages cataloged without \
                 source roots",
                "pyproject.toml: uv.lock pins local-lib without a version; not cataloged",
            ]
        );
        assert!(resolution.entries.is_empty());
    }

    #[test]
    fn test_resolve_packages_over_max_drops_excess_with_degradation() {
        let mut lockfile = String::from("version = 1\n\n");
        for index in 0..=PACKAGES_MAX {
            writeln!(
                lockfile,
                "[[package]]\nname = \"pkg-{index:05}\"\nversion = \"1.0.0\"\nsource = {REGISTRY}\n"
            )
            .expect("a string takes every write");
        }
        let mut inspector =
            RecordedInspector::default().with_file(format!("{ROOT}/uv.lock"), lockfile);

        let resolution = resolve(&["pyproject.toml"], &mut inspector);

        assert_eq!(resolution.entries.len(), PACKAGES_MAX);
        assert!(resolution.entries.iter().all(|entry| !entry.is_direct()));
        assert_eq!(
            resolution.degradations,
            [
                "pyproject.toml: no environment at /workspace/.venv; packages cataloged without \
                 source roots"
                    .to_owned(),
                format!(
                    "1 of {} packages were not cataloged: at most {PACKAGES_MAX} are cataloged \
                     per workspace",
                    PACKAGES_MAX + 1
                ),
            ]
        );
    }

    #[test]
    fn test_resolve_same_inspector_twice_answers_equal_resolutions() {
        let first = resolve(&["pyproject.toml"], &mut environment_inspector());
        let second = resolve(&["pyproject.toml"], &mut environment_inspector());

        assert_eq!(first, second);
        assert_eq!(first.entries.len(), 8);
    }

    #[test]
    fn test_normalized_name_folds_separator_runs_and_case() {
        assert_eq!(normalized_name("Markdown_It.py"), "markdown-it-py");
        assert_eq!(normalized_name("typing-extensions"), "typing-extensions");
        assert_eq!(normalized_name("Zope.Interface"), "zope-interface");
        assert_eq!(normalized_name("foo__.--bar"), "foo-bar");
        assert_eq!(normalized_name("trailing_"), "trailing-");
    }
}
