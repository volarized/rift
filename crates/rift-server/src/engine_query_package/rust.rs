//! Rust namespace witnesses captured through admitted package roots.

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

use super::{CalleeRoots, RiftError, SelectedSource, captured_revision, source_owner, unavailable};

type CapturedSource = (Uri, ProjectPath, String);

pub(super) fn analyze(
    source: &SelectedSource,
    context: &[CapturedSource],
    roots: &CalleeRoots,
    trees: &[&Path],
    configuration: &WorkspaceConfiguration,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<(PackageAnalysis, Vec<CapturedSource>), RiftError> {
    let owner = source_owner(&source.origin)?;
    let origin = ContributionOrigin::new(Some(source.origin.clone()), SourceKind::Authored)?;
    let limits = ExactPackageLimits::from_configuration(configuration)?;
    let Some(rift_lsp::uri::EngineAddress::Package(file)) = roots.address(trees, &source.uri)?
    else {
        return unavailable("the Rust source has no admitted package root").fail();
    };
    let root = file.root();
    let mut sources = BTreeMap::from([(
        source.path.clone(),
        (source.uri.clone(), source.text.clone()),
    )]);
    let mut syntax = BTreeMap::<ProjectPath, PackageSyntax>::new();
    let observed = RefCell::new(BTreeMap::new());
    let overflow = Cell::new(false);
    let maximum = limits.files_max() as usize;
    for _ in 0..maximum {
        if cancelled() {
            return rift_error::errors::server::read_cancelled().fail();
        }
        let files = sources
            .iter()
            .map(|(path, (_, text))| PackageSource::new(path, text))
            .collect::<Vec<_>>();
        let metadata = context
            .iter()
            .map(|(_, path, text)| PackageSource::new(path, text))
            .collect::<Vec<_>>();
        let mut input = ExactPackageInput::new(&owner, &source.provider, &origin, &files, limits)?
            .with_framework_context(&metadata, &[])?;
        if let Some((target, _)) = input.namespace_input().library_target()
            && let Some(library) = files.iter().find(|file| file.path() == &target)
            && let Some(manifest) = metadata
                .iter()
                .find(|file| file.path().as_str() == "Cargo.toml")
        {
            input = input.with_library(&owner, *manifest, *library)?;
        }
        let mut digest = Sha256::new();
        for file in files.iter().chain(&metadata) {
            digest.update(file.path().as_str());
            digest.update(FileDigest::of(file.text().as_bytes()).as_bytes());
        }
        let revision = captured_revision(FileDigest::from_bytes(digest.finalize().into()));
        let analysis = PackageAnalyzer::analyze_with_syntax(input, revision, |offered| {
            if cancelled() {
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
        if cancelled() {
            return rift_error::errors::server::read_cancelled().fail();
        }
        let analysis = analysis?;
        let documents = analysis
            .files()
            .iter()
            .map(|file| file.file())
            .collect::<Vec<_>>();
        let contains = |path: &str| {
            if cancelled() {
                return false;
            }
            if let Some(found) = observed.borrow().get(path) {
                return *found;
            }
            if observed.borrow().len() >= maximum {
                overflow.set(true);
                return false;
            }
            let found = ProjectPath::new(path)
                .and_then(|path| root.document_uri(&path))
                .and_then(|uri| crate::callee::query_file_path(&uri))
                .is_ok_and(|path| std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file()));
            observed.borrow_mut().insert(path.to_owned(), found);
            found
        };
        if cancelled() {
            return rift_error::errors::server::read_cancelled().fail();
        }
        let paths = input
            .namespace_input()
            .namespace_closure_paths(&documents, &contains);
        if cancelled() {
            return rift_error::errors::server::read_cancelled().fail();
        }
        let paths = paths?;
        if overflow.get() {
            return unavailable("Rust namespace observations exceed the accepted file bound")
                .fail();
        }
        let requested = paths
            .into_iter()
            .filter(|path| !sources.contains_key(path))
            .collect::<Vec<_>>();
        if requested.is_empty() {
            return Ok((
                analysis,
                sources
                    .into_iter()
                    .map(|(path, (uri, text))| (uri, path, text))
                    .collect(),
            ));
        }
        if sources
            .len()
            .saturating_add(context.len())
            .saturating_add(requested.len())
            > maximum
        {
            return unavailable("Rust namespace sources exceed the accepted file bound").fail();
        }
        let mut bytes = sources
            .values()
            .map(|(_, text)| text.len() as u64)
            .chain(context.iter().map(|(_, _, text)| text.len() as u64))
            .fold(0_u64, u64::saturating_add);
        for path in requested {
            if cancelled() {
                return rift_error::errors::server::read_cancelled().fail();
            }
            let uri = root.document_uri(&path)?;
            let (file, text) = roots.capture_query_file(
                &uri,
                trees,
                configuration.providers.syntax.max_file.bytes(),
            )?;
            if file.origin() != &source.origin {
                return unavailable(
                    "Rust namespace source owner differs from the selected package",
                )
                .fail();
            }
            bytes = bytes.saturating_add(text.len() as u64);
            if bytes > limits.bytes_max() {
                return unavailable("Rust namespace sources exceed the accepted byte bound").fail();
            }
            sources.insert(path, (uri, text));
        }
    }
    unavailable("Rust namespace acquisition exceeded the accepted file bound").fail()
}
