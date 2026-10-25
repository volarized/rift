//! Captured package sources and optional namespace witnesses under shared admission.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::Path;

use lsp_types::Uri;
use rift_analysis::{
    ExactPackageInput, ExactPackageLimits, PackageAnalysis, PackageAnalyzer, PackageSource,
    PackageSyntax,
};
use rift_core::{ContributionOrigin, FileDigest, ProjectPath, SourceKind};
use rift_protocol::configuration::WorkspaceConfiguration;
use sha2::{Digest as _, Sha256};

use super::{
    CalleeRoots, RiftError, SelectedSource, admitted_import_roots, captured_revision, source_owner,
    unavailable,
};

type CapturedSource = (Uri, ProjectPath, String);
type SelectedSources = BTreeMap<ProjectPath, (Uri, String)>;

pub(super) fn analyze(
    source: &SelectedSource,
    context: &[CapturedSource],
    roots: &CalleeRoots,
    trees: &[&Path],
    configuration: &WorkspaceConfiguration,
    cancelled: &(dyn Fn() -> bool + Sync),
    namespace: bool,
) -> Result<(PackageAnalysis, Vec<CapturedSource>), RiftError> {
    Capture {
        source,
        context,
        roots,
        trees,
        configuration,
        cancelled,
        limits: ExactPackageLimits::from_configuration(configuration)?,
        observed: RefCell::new(BTreeMap::new()),
        overflow: Cell::new(false),
        root_failure: RefCell::new(None),
    }
    .analyze(namespace)
}

struct Capture<'input> {
    source: &'input SelectedSource,
    context: &'input [CapturedSource],
    roots: &'input CalleeRoots,
    trees: &'input [&'input Path],
    configuration: &'input WorkspaceConfiguration,
    cancelled: &'input (dyn Fn() -> bool + Sync),
    limits: ExactPackageLimits,
    observed: RefCell<BTreeMap<String, bool>>,
    overflow: Cell<bool>,
    root_failure: RefCell<Option<RiftError>>,
}

impl Capture<'_> {
    fn analyze(
        &self,
        namespace: bool,
    ) -> Result<(PackageAnalysis, Vec<CapturedSource>), RiftError> {
        let owner = source_owner(&self.source.origin)?;
        let origin =
            ContributionOrigin::new(Some(self.source.origin.clone()), SourceKind::Authored)?;
        let import_roots = admitted_import_roots(self.source, self.roots, self.trees)?;
        let mut sources = BTreeMap::from([(
            self.source.path.clone(),
            (self.source.uri.clone(), self.source.text.clone()),
        )]);
        let mut syntax = BTreeMap::<ProjectPath, PackageSyntax>::new();
        for _ in 0..self.limits.files_max() {
            self.check_cancelled()?;
            let files = sources
                .iter()
                .map(|(path, (_, text))| PackageSource::new(path, text))
                .collect::<Vec<_>>();
            let metadata = self
                .context
                .iter()
                .map(|(_, path, text)| PackageSource::new(path, text))
                .collect::<Vec<_>>();
            let mut input = ExactPackageInput::new(
                &owner,
                &self.source.provider,
                &origin,
                &files,
                self.limits,
            )?
            .with_framework_context(&metadata, &[])?
            .with_import_roots(&import_roots)?;
            if namespace
                && let Some((target, _)) = input.namespace_input().library_target()
                && let Some(library) = files.iter().find(|file| file.path() == &target)
                && let Some(manifest) = metadata
                    .iter()
                    .find(|file| file.path().as_str() == "Cargo.toml")
            {
                input = input.with_library(&owner, *manifest, *library)?;
            }
            let analysis = self.analyze_input(&input, &files, &metadata, &mut syntax)?;
            if !namespace {
                return Ok(Self::finish(analysis, sources));
            }
            let requested = self.namespace_requests(&input, &analysis, &sources)?;
            if requested.is_empty() {
                return Ok(Self::finish(analysis, sources));
            }
            self.capture_sources(requested, &mut sources)?;
        }
        unavailable("namespace acquisition exceeded the accepted file bound").fail()
    }

    fn analyze_input(
        &self,
        input: &ExactPackageInput<'_>,
        files: &[PackageSource<'_>],
        metadata: &[PackageSource<'_>],
        syntax: &mut BTreeMap<ProjectPath, PackageSyntax>,
    ) -> Result<PackageAnalysis, RiftError> {
        let mut digest = Sha256::new();
        for file in files.iter().chain(metadata) {
            digest.update(file.path().as_str());
            digest.update(FileDigest::of(file.text().as_bytes()).as_bytes());
        }
        let revision = captured_revision(FileDigest::from_bytes(digest.finalize().into()));
        let analysis = PackageAnalyzer::analyze_with_syntax(*input, revision, |offered| {
            if (self.cancelled)() {
                return None;
            }
            if let Some(held) = syntax
                .get(offered.path())
                .filter(|held| held.identity() == offered.identity())
            {
                return Some(held.clone());
            }
            let parsed = offered.parse().ok()?;
            syntax.insert(offered.path().clone(), parsed.clone());
            Some(parsed)
        });
        self.check_cancelled()?;
        analysis
    }

    fn namespace_requests(
        &self,
        input: &ExactPackageInput<'_>,
        analysis: &PackageAnalysis,
        sources: &SelectedSources,
    ) -> Result<Vec<ProjectPath>, RiftError> {
        let documents = analysis
            .files()
            .iter()
            .map(rift_analysis::AnalyzedFile::file)
            .collect::<Vec<_>>();
        self.check_cancelled()?;
        let paths = input
            .namespace_input()
            .namespace_closure_paths(&documents, &|path| self.contains(path));
        self.check_cancelled()?;
        if let Some(error) = self.root_failure.borrow_mut().take() {
            return Err(error);
        }
        let paths = paths?;
        if self.overflow.get() {
            return unavailable("namespace observations exceed the accepted file bound").fail();
        }
        Ok(paths
            .into_iter()
            .filter(|path| !sources.contains_key(path))
            .collect())
    }

    fn contains(&self, path: &str) -> bool {
        if (self.cancelled)() {
            return false;
        }
        if let Some(found) = self.observed.borrow().get(path) {
            return *found;
        }
        if self.observed.borrow().len() >= self.limits.files_max() as usize {
            self.overflow.set(true);
            return false;
        }
        let root = match self.file_root() {
            Ok(root) => root,
            Err(error) => {
                *self.root_failure.borrow_mut() = Some(error);
                return false;
            }
        };
        let found = ProjectPath::new(path)
            .and_then(|path| root.document_uri(&path))
            .and_then(|uri| crate::callee::query_file_path(&uri))
            .is_ok_and(|path| std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file()));
        self.observed.borrow_mut().insert(path.to_owned(), found);
        found
    }

    fn file_root(&self) -> Result<rift_lsp::uri::TreeRoot, RiftError> {
        let Some(rift_lsp::uri::EngineAddress::Package(file)) =
            self.roots.address(self.trees, &self.source.uri)?
        else {
            return unavailable("the source has no admitted package root").fail();
        };
        Ok(file.root().clone())
    }

    fn capture_sources(
        &self,
        requested: Vec<ProjectPath>,
        sources: &mut SelectedSources,
    ) -> Result<(), RiftError> {
        if sources
            .len()
            .saturating_add(self.context.len())
            .saturating_add(requested.len())
            > self.limits.files_max() as usize
        {
            return unavailable("namespace sources exceed the accepted file bound").fail();
        }
        let mut bytes = sources
            .values()
            .map(|(_, text)| text.len() as u64)
            .chain(self.context.iter().map(|(_, _, text)| text.len() as u64))
            .fold(0_u64, u64::saturating_add);
        let root = self.file_root()?;
        for path in requested {
            self.check_cancelled()?;
            let uri = root.document_uri(&path)?;
            let (file, text) = self.roots.capture_query_file(
                &uri,
                self.trees,
                self.configuration.providers.syntax.max_file.bytes(),
            )?;
            if file.origin() != &self.source.origin {
                return unavailable("namespace source owner differs from the selected package")
                    .fail();
            }
            bytes = bytes.saturating_add(text.len() as u64);
            if bytes > self.limits.bytes_max() {
                return unavailable("namespace sources exceed the accepted byte bound").fail();
            }
            sources.insert(path, (uri, text));
        }
        Ok(())
    }

    fn check_cancelled(&self) -> Result<(), RiftError> {
        if (self.cancelled)() {
            rift_error::errors::server::read_cancelled().fail()
        } else {
            Ok(())
        }
    }

    fn finish(
        analysis: PackageAnalysis,
        sources: SelectedSources,
    ) -> (PackageAnalysis, Vec<CapturedSource>) {
        (
            analysis,
            sources
                .into_iter()
                .map(|(path, (uri, text))| (uri, path, text))
                .collect(),
        )
    }
}
