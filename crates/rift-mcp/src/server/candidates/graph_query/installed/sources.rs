//! Bounded original sources used to place an engine target.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rift_analysis::{
    ExactPackageInput, ExactPackageLimits, IndexedFile, PackageAnalysis, PackageAnalyzer,
    PackageImportRoot, PackageSource, PackageSyntax, PackageSyntaxIdentity, RustTarget,
};
use rift_core::{ContributionOrigin, ProjectPath, SourceLocation};
use rift_error::RiftError;
use rift_protocol::identity::SymbolOwner;
use rift_protocol::read::{Language, SourceKind};
use rift_server::{CalleePackage, CalleeRoots, PackageCallee};
use rift_syntax::{SyntaxProvider as _, SyntaxSource};

use super::{coverage_failure, python_import_root};

#[derive(Debug)]
pub(super) struct Sources {
    pub(super) files: BTreeMap<ProjectPath, Arc<String>>,
    context: BTreeMap<ProjectPath, String>,
    owner: SymbolOwner,
    origin: ContributionOrigin,
    language: Language,
    import_roots: Vec<PackageImportRoot>,
    root: Option<PathBuf>,
    limits: ExactPackageLimits,
    /// The target the installed sources are built for, which is the host's.
    target: Option<RustTarget>,
}

impl Sources {
    pub(super) fn capture(
        callee: &PackageCallee,
        roots: &CalleeRoots,
        tree: &Path,
        accepted: &rift_protocol::configuration::WorkspaceConfiguration,
    ) -> Result<Self, RiftError> {
        let limits = ExactPackageLimits::from_configuration(accepted)?;
        let bytes_max = accepted.providers.syntax.max_file.bytes();
        let (owner, origin) = target_owner(callee)?;
        let (source, import_root, root, metadata) = if let Some(source) = callee.captured_source() {
            if u64::try_from(source.len()).map_or(true, |bytes| bytes > bytes_max) {
                return Err(coverage_failure());
            }
            (source.to_owned(), None, None, None)
        } else {
            let (file, source, metadata) =
                roots.capture_package_call(callee, &[tree], bytes_max)?;
            (
                source,
                file.import_root().cloned(),
                Some(file.root().filesystem_path().to_path_buf()),
                metadata,
            )
        };
        let extension = Path::new(callee.path().as_str())
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .ok_or_else(coverage_failure)?;
        let provider = target_language(&owner, extension)?;
        let mut import_roots = Vec::new();
        if provider == "python" {
            import_roots.push(match &owner {
                SymbolOwner::Runtime { .. } => PackageImportRoot::runtime(owner.clone(), None)?,
                _ => python_import_root(import_root.as_ref().ok_or_else(coverage_failure)?)?,
            });
        }
        let mut captured = Self {
            files: BTreeMap::from([(callee.path().clone(), Arc::new(source))]),
            context: BTreeMap::new(),
            owner,
            origin,
            language: Language {
                name: provider.to_owned(),
                dialect: None,
            },
            import_roots,
            root,
            limits,
            target: RustTarget::host(),
        };
        if let Some(metadata) = metadata {
            captured
                .context
                .insert(ProjectPath::new("package.json")?, metadata);
        }
        if provider == "rust" {
            let manifest = match &captured.owner {
                SymbolOwner::Runtime { .. } => {
                    let directory = callee
                        .path()
                        .as_str()
                        .split('/')
                        .next()
                        .ok_or_else(coverage_failure)?;
                    ProjectPath::new(format!("{directory}/Cargo.toml"))?
                }
                _ => ProjectPath::new("Cargo.toml")?,
            };
            let text = roots.capture_package_source(callee, &[tree], &manifest, bytes_max)?;
            captured.context.insert(manifest, text);
        }
        Ok(captured)
    }

    fn input<'a>(
        &'a self,
        files: &'a [PackageSource<'a>],
        context: &'a [PackageSource<'a>],
    ) -> Result<ExactPackageInput<'a>, RiftError> {
        ExactPackageInput::new(
            &self.owner,
            &self.language,
            &self.origin,
            files,
            self.limits,
        )?
        .with_import_roots(&self.import_roots)?
        .with_framework_context(context, &[])
        .map(|input| input.with_rust_target(self.target))
    }

    pub(super) fn analyze(
        &mut self,
        callee: &PackageCallee,
        roots: &CalleeRoots,
        tree: &Path,
    ) -> Result<PackageAnalysis, RiftError> {
        #[cfg(test)]
        super::runs::record(tree, callee);
        let mut parsed = BTreeMap::<ProjectPath, IndexedFile>::new();
        if self.language.name == "rust" {
            self.capture_modules(callee, roots, tree, &mut parsed)?;
        }
        let files = sources(&self.files);
        let context = context_sources(&self.context);
        let input = self.input(&files, &context)?;
        PackageAnalyzer::analyze_with_syntax(input, 1, |source| {
            let file = parsed.get(source.path())?;
            Some(PackageSyntax::new(
                PackageSyntaxIdentity {
                    source_digest: file.digest(),
                    language: file.syntax().language().clone(),
                    limits: self.limits.syntax(),
                    analyzer_digest: rift_analysis::analyzer_digest(),
                },
                Arc::clone(file.syntax_facts()),
            ))
        })
    }

    fn capture_modules(
        &mut self,
        callee: &PackageCallee,
        roots: &CalleeRoots,
        tree: &Path,
        parsed: &mut BTreeMap<ProjectPath, IndexedFile>,
    ) -> Result<(), RiftError> {
        let selected = BTreeSet::from([callee.path().clone()]);
        let root = self.root.as_ref().ok_or_else(coverage_failure)?.clone();
        loop {
            self.input(&sources(&self.files), &context_sources(&self.context))?;
            for (path, source) in &self.files {
                if parsed.contains_key(path) {
                    continue;
                }
                let document = rift_syntax::RustSyntaxProvider::default().analyze(
                    SyntaxSource {
                        path,
                        text: source.as_str(),
                    },
                    self.limits.syntax(),
                )?;
                let digest = *document.source_digest().ok_or_else(coverage_failure)?;
                parsed.insert(
                    path.clone(),
                    IndexedFile::new(path.clone(), Arc::clone(source), digest, false, document),
                );
            }
            let files = sources(&self.files);
            let context = context_sources(&self.context);
            let documents = parsed.values().collect::<Vec<_>>();
            let requested = self
                .input(&files, &context)?
                .namespace_input()
                .namespace_paths(&documents, &|path| root.join(path).is_file(), &selected)?;
            let missing = requested
                .into_iter()
                .filter(|path| !self.files.contains_key(path))
                .collect::<Vec<_>>();
            if missing.is_empty() {
                return Ok(());
            }
            for path in missing {
                if self.files.len().saturating_add(self.context.len())
                    >= self.limits.files_max() as usize
                {
                    return Err(coverage_failure());
                }
                let text = roots.capture_package_source(
                    callee,
                    &[tree],
                    &path,
                    self.limits
                        .bytes_max()
                        .checked_sub(
                            u64::try_from(self.retained_bytes()).map_err(|_| coverage_failure())?,
                        )
                        .ok_or_else(coverage_failure)?
                        .min(
                            u64::try_from(self.limits.syntax().source_bytes_max())
                                .map_err(|_| coverage_failure())?,
                        ),
                )?;
                self.files.insert(path, Arc::new(text));
            }
        }
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.files
            .values()
            .map(|text| text.len())
            .chain(self.context.values().map(String::len))
            .fold(0, usize::saturating_add)
    }

    pub(super) fn same_capture(&self, other: &Self) -> bool {
        self.files == other.files
            && self.context == other.context
            && self.owner == other.owner
            && self.origin == other.origin
    }

    pub(super) fn verify(
        &self,
        callee: &PackageCallee,
        roots: &CalleeRoots,
        tree: &Path,
    ) -> Result<(), RiftError> {
        if let Some(source) = callee.captured_source() {
            return if self.files.get(callee.path()).map(|text| text.as_str()) == Some(source) {
                Ok(())
            } else {
                Err(coverage_failure())
            };
        }
        let (file, current, metadata) = roots.capture_package_call(
            callee,
            &[tree],
            u64::try_from(
                self.files
                    .get(callee.path())
                    .ok_or_else(coverage_failure)?
                    .len(),
            )
            .map_err(|_| coverage_failure())?,
        )?;
        if self.files.get(callee.path()).map(|text| text.as_str()) != Some(current.as_str())
            || self.context.get(&ProjectPath::new("package.json")?) != metadata.as_ref()
            || file.origin() != self.origin.location().ok_or_else(coverage_failure)?
        {
            return Err(coverage_failure());
        }
        for (path, source) in self
            .files
            .iter()
            .map(|(path, source)| (path, source.as_str()))
            .chain(
                self.context
                    .iter()
                    .map(|(path, source)| (path, source.as_str())),
            )
        {
            if path == callee.path() || path.as_str() == "package.json" {
                continue;
            }
            let current = roots.capture_package_source(
                callee,
                &[tree],
                path,
                u64::try_from(source.len()).map_err(|_| coverage_failure())?,
            )?;
            if current != source {
                return Err(coverage_failure());
            }
        }
        Ok(())
    }
}

fn sources(files: &BTreeMap<ProjectPath, Arc<String>>) -> Vec<PackageSource<'_>> {
    files
        .iter()
        .map(|(path, source)| PackageSource::new(path, source))
        .collect()
}

fn context_sources(files: &BTreeMap<ProjectPath, String>) -> Vec<PackageSource<'_>> {
    files
        .iter()
        .map(|(path, source)| PackageSource::new(path, source))
        .collect()
}

fn target_owner(callee: &PackageCallee) -> Result<(SymbolOwner, ContributionOrigin), RiftError> {
    let origin = match callee.package() {
        CalleePackage::Installed(package) => SourceLocation::Dependency {
            package: package.clone(),
        },
        CalleePackage::Runtime(runtime) => SourceLocation::Stdlib {
            runtime: Some(runtime.clone()),
        },
        CalleePackage::StandardLibrary(_) => return Err(coverage_failure()),
    };
    let owner = match callee.package() {
        CalleePackage::Installed(package) => package.owner()?,
        CalleePackage::Runtime(runtime) => runtime.owner()?,
        CalleePackage::StandardLibrary(_) => return Err(coverage_failure()),
    };
    Ok((
        owner,
        ContributionOrigin::new(Some(origin), SourceKind::Authored)?,
    ))
}

fn target_language(owner: &SymbolOwner, extension: &str) -> Result<&'static str, RiftError> {
    match (owner, extension) {
        (SymbolOwner::Package { manager, .. }, "py" | "pyi") if manager == "pypi" => Ok("python"),
        (SymbolOwner::Package { manager, .. }, "rs") if manager == "cargo" => Ok("rust"),
        (SymbolOwner::Runtime { runtime }, "py" | "pyi")
            if matches!(runtime.as_str(), "cpython" | "python") =>
        {
            Ok("python")
        }
        (SymbolOwner::Runtime { runtime }, "rs") if runtime == "rust" => Ok("rust"),
        (SymbolOwner::Package { manager, .. }, "ts" | "mts" | "cts" | "tsx")
            if manager == "npm" =>
        {
            Ok("typescript")
        }
        (SymbolOwner::Package { manager, .. }, "js" | "mjs" | "cjs" | "jsx")
            if manager == "npm" =>
        {
            Ok("javascript")
        }
        (SymbolOwner::Runtime { runtime }, "ts" | "mts" | "cts") if runtime == "node" => {
            Ok("typescript")
        }
        (SymbolOwner::Runtime { runtime }, "js" | "mjs" | "cjs") if runtime == "node" => {
            Ok("javascript")
        }
        _ => Err(coverage_failure()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "pub mod sys { #[cfg(unix)] pub fn native() {} #[cfg(windows)] pub fn other() {} }\n#[cfg(unix)] pub fn run() { sys::native() }\n#[cfg(windows)] pub fn call() { sys::other() }\n";

    /// Whether installed sources of a package or runtime store each of the two calls.
    fn calls(
        owner: SymbolOwner,
        location: SourceLocation,
        target: Option<RustTarget>,
    ) -> [bool; 2] {
        let manifest = if matches!(owner, SymbolOwner::Runtime { .. }) {
            "core/Cargo.toml"
        } else {
            "Cargo.toml"
        };
        let path = manifest.replace("Cargo.toml", "src/lib.rs");
        let sources = Sources {
            files: BTreeMap::from([(
                ProjectPath::new(path).expect("source path"),
                Arc::new(SOURCE.to_owned()),
            )]),
            context: BTreeMap::from([(
                ProjectPath::new(manifest).expect("manifest path"),
                "[package]\nname='core'\nversion='1.0.0'\n".to_owned(),
            )]),
            owner,
            origin: ContributionOrigin::new(Some(location), SourceKind::Authored).expect("origin"),
            language: Language {
                name: "rust".to_owned(),
                dialect: None,
            },
            import_roots: Vec::new(),
            root: None,
            limits: ExactPackageLimits::new(16, 65_536),
            target,
        };
        let files = super::sources(&sources.files);
        let context = context_sources(&sources.context);
        let input = sources.input(&files, &context).expect("installed input");
        let analysis = PackageAnalyzer::analyze(input, 1).expect("installed analysis");
        ["/native", "/other"].map(|callee| {
            analysis.publication().edges.iter().any(|edge| {
                edge.kinds.contains(&rift_protocol::read::EdgeKind::Calls)
                    && edge.to.as_str().ends_with(callee)
            })
        })
    }

    #[test]
    fn installed_sources_follow_the_target_of_their_host() {
        let package = rift_protocol::read::PackageIdentity {
            manager: "cargo".to_owned(),
            registry: "crates.io".to_owned(),
            name: "core".to_owned(),
            version: "1.0.0".to_owned(),
        };
        let runtime = rift_protocol::read::RuntimeIdentity {
            runtime: "rust".to_owned(),
            version: "1.98.1".to_owned(),
        };
        let owners = [
            (
                package.owner().expect("package owner"),
                SourceLocation::Dependency { package },
            ),
            (
                runtime.owner().expect("runtime owner"),
                SourceLocation::Stdlib {
                    runtime: Some(runtime),
                },
            ),
        ];
        for (owner, location) in owners {
            for (triple, expected) in [
                ("aarch64-apple-darwin", [true, false]),
                ("x86_64-unknown-linux-gnu", [true, false]),
                ("x86_64-pc-windows-msvc", [false, true]),
            ] {
                let target = RustTarget::from_triple(triple);
                assert!(target.is_some(), "{triple}");
                assert_eq!(
                    calls(owner.clone(), location.clone(), target),
                    expected,
                    "{triple}"
                );
            }
            assert_eq!(
                calls(owner, location, RustTarget::host()),
                [cfg!(unix), cfg!(windows)]
            );
        }
    }
}
