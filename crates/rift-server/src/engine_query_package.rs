//! Admitted dependency source preparation for request-owned engine reads.

use std::collections::BTreeMap;
use std::path::Path;

use lsp_types::{Range, Uri};
use rift_analysis::{
    ExactPackageInput, ExactPackageLimits, PackageAnalyzer, PackageImportRoot,
    PackageImportRootOrigin, PackageSource,
};
use rift_core::{ContributionOrigin, FileDigest, ProjectPath, SourceKind, SourceLocation};
use rift_lsp::capabilities::PositionEncoding;
use rift_lsp::position::LineIndex;
use rift_protocol::configuration::WorkspaceConfiguration;
use rift_protocol::identity::SymbolOwner;
use rift_protocol::read::{Language, SourceUnitId, SourceUnitSpan, SymbolId, TextRange};
use rift_protocol::symbol_read::SymbolDeclaration;
use rift_search::query::{QueryFacts, QuerySource, QuerySymbol};

use super::{CalleeRoots, RiftError, unavailable};
use crate::PackageCallee;

/// A dependency source that must be admitted before completing an engine answer.
#[derive(Clone, Debug, PartialEq)]
pub enum QuerySourceRequest {
    /// An outgoing target whose exact package declaration has not been named.
    Call(PackageCallee),
    /// A caller whose original source must be admitted before mapping its call ranges.
    IncomingCall {
        /// Established called declaration.
        target: SymbolId,
        /// Caller location selected by the engine.
        caller: PackageCallee,
        /// Physical call ranges in the caller's negotiated encoding.
        ranges: Vec<Range>,
    },
    /// A physical reference in a dependency file not yet held by the request.
    Reference {
        /// The established symbol this source occurrence references.
        target: SymbolId,
        /// Engine URI whose owner must be established before source is read.
        uri: Uri,
        /// Range in the engine's negotiated position encoding.
        range: Range,
        /// Negotiated position encoding of the returned range.
        encoding: PositionEncoding,
        /// Accepted language binding whose engine produced the location.
        provider: Language,
        /// Relationship established by this physical occurrence.
        facet: rift_protocol::read::RelationshipFacet,
        /// The engine established a file-level caller without an enclosing declaration.
        file_level: bool,
    },
}

impl QuerySourceRequest {
    /// Exact dependency owner selected by accepted roots, without reading source bytes.
    ///
    /// # Errors
    /// Returns unavailable when no exact package or runtime owner admits this location.
    pub fn origin(
        &self,
        roots: &CalleeRoots,
        trees: &[&Path],
    ) -> Result<SourceLocation, RiftError> {
        match self {
            Self::Call(callee) | Self::IncomingCall { caller: callee, .. } => callee
                .query_origin()
                .ok_or_else(|| unavailable("dependency runtime version is unavailable")),
            Self::Reference { uri, .. } => match roots.address(trees, uri)? {
                Some(rift_lsp::uri::EngineAddress::Package(file)) => Ok(file.origin().clone()),
                _ => unavailable("the reference source has no admitted dependency owner").fail(),
            },
        }
    }
}

#[path = "engine_query_package/rust.rs"]
mod rust;

#[derive(Debug)]
pub(super) struct DeclarationLocation {
    pub(super) source: SourceUnitSpan,
    pub(super) uri: Uri,
}

/// Captured dependency facts and engine positions established from the same bytes.
#[derive(Debug)]
pub struct QueryPackageTarget {
    pub(super) request: QuerySourceRequest,
    pub(super) unit: SourceUnitId,
    pub(super) provider: Language,
    pub(super) facts: QueryFacts,
    pub(super) name_ranges: BTreeMap<SymbolId, DeclarationLocation>,
    pub(super) digest: FileDigest,
    pub(super) selected: Option<SymbolId>,
    context: Vec<(Uri, FileDigest)>,
}

struct SelectedSource {
    origin: SourceLocation,
    path: ProjectPath,
    uri: Uri,
    provider: Language,
    text: String,
    offset: usize,
}

impl QueryPackageTarget {
    /// Physical source unit captured by this target.
    #[must_use]
    pub const fn source_unit(&self) -> &SourceUnitId {
        &self.unit
    }

    /// Revalidates admitted source ownership and bytes, blocking.
    ///
    /// The caller runs this check on its bounded worker before evaluating acquired facts.
    ///
    /// # Errors
    /// Returns unavailable when source bytes, ownership or accepted access no longer match.
    pub fn validate(
        &self,
        roots: &CalleeRoots,
        trees: &[&Path],
        configuration: &WorkspaceConfiguration,
    ) -> Result<(), RiftError> {
        self.validate_with_cancellation(roots, trees, configuration, &|| false)
    }

    /// Revalidates source ownership and bytes while observing worker cancellation.
    ///
    /// # Errors
    /// Returns cancelled before another source read; source changes retain their
    /// original admission and byte validation failures.
    pub fn validate_with_cancellation(
        &self,
        roots: &CalleeRoots,
        trees: &[&Path],
        configuration: &WorkspaceConfiguration,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), RiftError> {
        if cancelled() {
            return rift_error::errors::server::read_cancelled().fail();
        }
        let source = selected_source(
            &self.request,
            roots,
            trees,
            configuration.providers.syntax.max_file.bytes(),
        )?;
        let unit = rift_core::SourceUnitId::for_origin(&source.origin, &source.path)?;
        if unit.to_string() != self.unit.0 || FileDigest::of(source.text.as_bytes()) != self.digest
        {
            return unavailable("dependency source changed during query acquisition").fail();
        }
        for (uri, digest) in &self.context {
            if cancelled() {
                return rift_error::errors::server::read_cancelled().fail();
            }
            let (_, text) = roots.capture_query_file(
                uri,
                trees,
                configuration.providers.syntax.max_file.bytes(),
            )?;
            if FileDigest::of(text.as_bytes()) != *digest {
                return unavailable("dependency source context changed during query acquisition")
                    .fail();
            }
        }
        if cancelled() {
            return rift_error::errors::server::read_cancelled().fail();
        }
        Ok(())
    }

    /// Captures and analyzes one admitted dependency source, blocking.
    ///
    /// The caller runs this operation on its existing bounded worker pool.
    /// Package ownership comes from accepted roots. Source, syntax and publication
    /// bounds come from the same accepted configuration as the request.
    ///
    /// # Errors
    /// Returns unavailable for unadmitted sources and preserves parser and filesystem failures.
    pub fn prepare(
        request: QuerySourceRequest,
        roots: &CalleeRoots,
        trees: &[&Path],
        configuration: &WorkspaceConfiguration,
    ) -> Result<Self, RiftError> {
        Self::prepare_with_cancellation(request, roots, trees, configuration, &|| false)
    }

    /// Captures admitted source facts while observing the owning worker's cancellation.
    ///
    /// # Errors
    /// Returns cancelled before another source or parse pass starts; other failures
    /// retain the original admission, source and syntax validation errors.
    pub fn prepare_with_cancellation(
        request: QuerySourceRequest,
        roots: &CalleeRoots,
        trees: &[&Path],
        configuration: &WorkspaceConfiguration,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<Self, RiftError> {
        if cancelled() {
            return rift_error::errors::server::read_cancelled().fail();
        }
        let source = selected_source(
            &request,
            roots,
            trees,
            configuration.providers.syntax.max_file.bytes(),
        )?;
        let owner = source_owner(&source.origin)?;
        let origin = ContributionOrigin::new(Some(source.origin.clone()), SourceKind::Authored)?;
        let context = package_context(&source, roots, trees, configuration)?;
        let digest = FileDigest::of(source.text.as_bytes());
        let (analysis, captured) = if matches!(&source.origin, SourceLocation::Dependency { package } if package.manager == "cargo")
            && source.provider.name == "rust"
        {
            rust::analyze(&source, &context, roots, trees, configuration, cancelled)?
        } else {
            let files = [PackageSource::new(&source.path, &source.text)];
            let context_sources = context
                .iter()
                .map(|(_, path, text)| PackageSource::new(path, text))
                .collect::<Vec<_>>();
            let limits = ExactPackageLimits::from_configuration(configuration)?;
            let import_roots = admitted_import_roots(&source, roots, trees)?;
            let input = ExactPackageInput::new(&owner, &source.provider, &origin, &files, limits)?
                .with_framework_context(&context_sources, &[])?
                .with_import_roots(&import_roots)?;
            (
                PackageAnalyzer::analyze(input, captured_revision(digest))?,
                vec![(source.uri.clone(), source.path.clone(), source.text.clone())],
            )
        };
        let unit = SourceUnitId(
            rift_core::SourceUnitId::for_origin(&source.origin, &source.path)?.to_string(),
        );
        let files = analysis
            .files()
            .iter()
            .map(|file| (file.file().path().as_str(), file.file()))
            .collect::<BTreeMap<_, _>>();
        let uris = captured
            .iter()
            .map(|(uri, path, _)| (path.as_str(), uri))
            .collect::<BTreeMap<_, _>>();
        let mut name_ranges = BTreeMap::new();
        for declaration in &analysis.publication().declarations {
            let (_, path) =
                rift_protocol::identity::parse_source_unit_identity(declaration.unit.as_str())?;
            let Some(file) = files.get(path.as_str()) else {
                continue;
            };
            let Some(uri) = uris.get(path.as_str()) else {
                continue;
            };
            let syntax = file.enclosing_symbol(declaration.range.start, declaration.range.end);
            if let Some(range) = syntax.and_then(|symbol| symbol.name_range) {
                let location = DeclarationLocation {
                    source: SourceUnitSpan {
                        unit: declaration.unit.clone(),
                        range: TextRange {
                            start: range.start,
                            end: range.end,
                        },
                    },
                    uri: (*uri).clone(),
                };
                if declaration.unit == unit
                    && declaration.range.start <= source.offset as u64
                    && (source.offset as u64) < declaration.range.end
                {
                    name_ranges.insert(declaration.symbol.clone(), location);
                } else {
                    name_ranges
                        .entry(declaration.symbol.clone())
                        .or_insert(location);
                }
            }
        }
        let selected = analysis
            .publication()
            .declarations
            .iter()
            .filter(|declaration| {
                declaration.unit == unit
                    && declaration.range.start <= source.offset as u64
                    && (source.offset as u64) < declaration.range.end
            })
            .min_by_key(|declaration| declaration.range.end - declaration.range.start)
            .map(|declaration| declaration.symbol.clone());
        if matches!(
            request,
            QuerySourceRequest::Call(_) | QuerySourceRequest::IncomingCall { .. }
        ) && selected.is_none()
        {
            #[cfg(test)]
            eprintln!(
                "admitted target {} offset {} captured {} files {:?}; physical declarations {:?}",
                source.path.as_str(),
                source.offset,
                captured.len(),
                captured
                    .iter()
                    .take(32)
                    .map(|(_, path, _)| path.as_str())
                    .collect::<Vec<_>>(),
                analysis
                    .publication()
                    .declarations
                    .iter()
                    .filter(|declaration| declaration.unit == unit)
                    .take(16)
                    .map(|declaration| (&declaration.symbol, &declaration.range))
                    .collect::<Vec<_>>()
            );
            return unavailable("the admitted call target has no canonical declaration").fail();
        }
        let mut facts = publication_facts(analysis.publication());
        let mut context = context
            .into_iter()
            .map(|(uri, _, text)| (uri, FileDigest::of(text.as_bytes())))
            .collect::<Vec<_>>();
        for (uri, path, text) in captured {
            if path != source.path {
                context.push((uri, FileDigest::of(text.as_bytes())));
            }
            facts.sources.push(QuerySource {
                unit: SourceUnitId(
                    rift_core::SourceUnitId::for_origin(&source.origin, &path)?.to_string(),
                ),
                digest: Some(rift_protocol::identity::SourceDigest::parse(
                    &data_encoding::HEXLOWER.encode(FileDigest::of(text.as_bytes()).as_bytes()),
                )?),
                text: Some(text),
            });
        }
        if cancelled() {
            return rift_error::errors::server::read_cancelled().fail();
        }
        Ok(Self {
            request,
            unit,
            provider: source.provider,
            facts,
            name_ranges,
            digest,
            selected,
            context,
        })
    }

    /// Canonical declaration selected by a call target, or its enclosing reference declaration.
    #[must_use]
    pub fn symbol(&self) -> Option<&SymbolId> {
        self.selected.as_ref()
    }

    #[cfg(test)]
    pub(super) fn source<'facts>(
        &self,
        facts: &'facts QueryFacts,
    ) -> Result<&'facts str, RiftError> {
        self.source_for(facts, &self.unit)
    }

    pub(super) fn source_for<'facts>(
        &self,
        facts: &'facts QueryFacts,
        unit: &SourceUnitId,
    ) -> Result<&'facts str, RiftError> {
        facts
            .sources
            .iter()
            .find(|source| &source.unit == unit)
            .and_then(|source| source.text.as_deref())
            .ok_or_else(|| unavailable("the admitted dependency source is no longer retained"))
    }
}

fn captured_revision(digest: FileDigest) -> u64 {
    let mut revision = [0_u8; size_of::<u64>()];
    revision.copy_from_slice(&digest.as_bytes()[..size_of::<u64>()]);
    u64::from_be_bytes(revision).max(1)
}

fn admitted_import_roots(
    source: &SelectedSource,
    roots: &CalleeRoots,
    trees: &[&Path],
) -> Result<Vec<PackageImportRoot>, RiftError> {
    if source.provider.name != "python" {
        return Ok(Vec::new());
    }
    if matches!(source.origin, SourceLocation::Stdlib { runtime: Some(_) }) {
        return Ok(vec![PackageImportRoot::runtime(
            source_owner(&source.origin)?,
            None,
        )?]);
    }
    let Some(rift_lsp::uri::EngineAddress::Package(file)) = roots.address(trees, &source.uri)?
    else {
        return unavailable("the Python source has no admitted import root").fail();
    };
    let Some(root) = file.import_root() else {
        return unavailable("the installed Python source has no established import module").fail();
    };
    let root = root.as_str();
    let module = root
        .strip_suffix(".pyi")
        .or_else(|| root.strip_suffix(".py"))
        .unwrap_or(root);
    Ok(vec![PackageImportRoot::new(
        None,
        vec![module.replace('/', ".")],
        PackageImportRootOrigin::Wheel,
    )?])
}

fn package_context(
    source: &SelectedSource,
    roots: &CalleeRoots,
    trees: &[&Path],
    configuration: &WorkspaceConfiguration,
) -> Result<Vec<(Uri, ProjectPath, String)>, RiftError> {
    let metadata = match &source.origin {
        SourceLocation::Dependency { package } if package.manager == "npm" => "package.json",
        SourceLocation::Dependency { package } if package.manager == "cargo" => "Cargo.toml",
        _ => return Ok(Vec::new()),
    };
    let Some(rift_lsp::uri::EngineAddress::Package(file)) = roots.address(trees, &source.uri)?
    else {
        return unavailable("the dependency metadata has no admitted source owner").fail();
    };
    let path = ProjectPath::new(metadata)?;
    let uri = file.root().document_uri(&path)?;
    let (metadata, text) =
        roots.capture_query_file(&uri, trees, configuration.providers.syntax.max_file.bytes())?;
    if metadata.origin() != &source.origin {
        return unavailable("dependency metadata owner differs from the selected target").fail();
    }
    Ok(vec![(uri, path, text)])
}

fn selected_source(
    request: &QuerySourceRequest,
    roots: &CalleeRoots,
    trees: &[&Path],
    bytes_max: u64,
) -> Result<SelectedSource, RiftError> {
    match request {
        QuerySourceRequest::Call(callee)
        | QuerySourceRequest::IncomingCall { caller: callee, .. } => {
            let captured = roots.capture_query_source(callee, trees, bytes_max)?;
            let origin = captured
                .query_origin()
                .ok_or_else(|| unavailable("dependency runtime version is unavailable"))?;
            let uri = captured
                .query_uri()
                .cloned()
                .ok_or_else(|| unavailable("dependency URI is unavailable"))?;
            let provider = captured
                .query_provider()
                .cloned()
                .ok_or_else(|| unavailable("dependency provider is unavailable"))?;
            let text = captured
                .captured_source()
                .ok_or_else(|| unavailable("dependency source is unavailable"))?
                .to_owned();
            let offset =
                LineIndex::new(&text).byte_offset(captured.encoding(), captured.position())?;
            Ok(SelectedSource {
                origin,
                path: captured.path().clone(),
                uri,
                provider,
                text,
                offset,
            })
        }
        QuerySourceRequest::Reference {
            uri,
            range,
            encoding,
            provider,
            ..
        } => {
            let (file, text) = roots.capture_query_file(uri, trees, bytes_max)?;
            let offset = LineIndex::new(&text).byte_offset(*encoding, range.start)?;
            Ok(SelectedSource {
                origin: file.origin().clone(),
                path: file.path().clone(),
                uri: uri.clone(),
                provider: provider.clone(),
                text,
                offset,
            })
        }
    }
}

fn source_owner(origin: &SourceLocation) -> Result<SymbolOwner, RiftError> {
    match origin {
        SourceLocation::Dependency { package } => package.owner(),
        SourceLocation::Stdlib {
            runtime: Some(runtime),
        } => Ok(SymbolOwner::Runtime {
            runtime: runtime.runtime.clone(),
            version: runtime.version.clone(),
        }),
        _ => unavailable("the selected source has no exact dependency or runtime owner").fail(),
    }
}

fn publication_facts(publication: &rift_protocol::index::PackagePublication) -> QueryFacts {
    let mut declarations: BTreeMap<&SymbolId, Vec<SymbolDeclaration>> = BTreeMap::new();
    for declaration in &publication.declarations {
        declarations
            .entry(&declaration.symbol)
            .or_default()
            .push(SymbolDeclaration {
                origin: declaration.origin.clone(),
                path: None,
                unit: Some(declaration.unit.clone()),
                range: declaration.range.clone(),
                line: declaration.line,
                node: None,
                source: Some(declaration.source.clone()),
                source_complete: declaration.source_complete,
                signature_indices: declaration.signature_indices.clone(),
                type_indices: declaration.type_indices.clone(),
                documentation_indices: declaration.documentation_indices.clone(),
            });
    }
    let symbols = publication
        .objects
        .iter()
        .filter_map(|symbol| {
            let identity = symbol.id.as_ref()?;
            Some(QuerySymbol {
                symbol: symbol.clone(),
                declarations: declarations.remove(identity).unwrap_or_default(),
            })
        })
        .collect();
    QueryFacts {
        symbols,
        relationships: publication.relationships.clone(),
        ..QueryFacts::default()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use lsp_types::Position;
    use rift_core::{PackageIdentity, ProjectPath};
    use rift_lsp::capabilities::PositionEncoding;
    use rift_lsp::uri::{PackageRoot, TreeRoot};
    use rift_protocol::configuration::WorkspaceConfiguration;
    use rift_protocol::read::Language;

    use super::{QueryPackageTarget, QuerySourceRequest};
    use crate::callee::{CalleeFile, CalleeRoots, NamedCallee, callee_file};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn package() -> PackageIdentity {
        PackageIdentity {
            manager: "npm".to_owned(),
            registry: "npmjs.org".to_owned(),
            name: "helper".to_owned(),
            version: "1.0.0".to_owned(),
        }
    }

    fn roots(root: &Path, package: PackageIdentity) -> Result<CalleeRoots, rift_error::RiftError> {
        Ok(CalleeRoots::from_packages(vec![PackageRoot::new(
            TreeRoot::new(root)?,
            package,
        )]))
    }

    fn call_request(
        workspace: &Path,
        dependency: &Path,
        roots: &CalleeRoots,
    ) -> Result<QuerySourceRequest, rift_error::RiftError> {
        let uri = TreeRoot::new(dependency)?.document_uri(&ProjectPath::new("index.ts")?)?;
        let caller = rift_core::SymbolId::new("rift://symbol/local/typescript/app/caller")?;
        let named = NamedCallee {
            uri: &uri,
            name: "entry",
            position: Position {
                line: 0,
                character: 16,
            },
            encoding: PositionEncoding::Utf16,
        };
        let CalleeFile::Package(callee) = callee_file(roots, &[workspace], &caller, named)? else {
            panic!("fixture URI must name its admitted package source");
        };
        let provider = Language::from_identity_segment("typescript")
            .expect("fixture uses the accepted TypeScript language identity");
        Ok(QuerySourceRequest::Call(
            callee.with_query_sources(Vec::new(), provider),
        ))
    }

    #[test]
    fn test_package_target_keeps_exact_owner_internal_declarations_and_source_validation()
    -> TestResult {
        let workspace = tempfile::tempdir()?;
        let dependency = tempfile::tempdir()?;
        let source =
            "export function entry() { return helper(); }\nfunction helper() { return 7; }\n";
        std::fs::write(dependency.path().join("index.ts"), source)?;
        std::fs::write(
            dependency.path().join("package.json"),
            r#"{"name":"helper","version":"1.0.0","types":"./index.ts"}"#,
        )?;
        let roots = roots(dependency.path(), package())?;
        let request = call_request(workspace.path(), dependency.path(), &roots)?;
        let configuration = WorkspaceConfiguration::default();
        let trees = [workspace.path()];
        let uncached =
            QueryPackageTarget::prepare(request.clone(), &roots, &trees, &configuration)?;
        let QuerySourceRequest::Call(callee) = request else {
            panic!("call source fixture");
        };
        let request = QuerySourceRequest::Call(roots.capture_query_source(
            &callee,
            &trees,
            configuration.providers.syntax.max_file.bytes(),
        )?);
        let target = QueryPackageTarget::prepare(request.clone(), &roots, &trees, &configuration)?;
        let symbol = target.symbol().expect("canonical package entry");
        let parsed = rift_protocol::identity::SymbolIdentity::parse(symbol.as_str())?;
        assert_eq!(parsed.owner(), &package().owner()?);
        assert!(
            target
                .facts
                .symbols
                .iter()
                .any(|symbol| symbol.symbol.name == "helper")
        );
        assert_eq!(target.source(&target.facts)?, source);
        target.validate(&roots, &trees, &configuration)?;
        assert_eq!(
            target
                .validate_with_cancellation(&roots, &trees, &configuration, &|| true)
                .expect_err("cancelled validation")
                .slug(),
            rift_error::errors::server::read_cancelled::SLUG
        );
        std::fs::write(
            dependency.path().join("index.ts"),
            source.replace("return 7", "return 8"),
        )?;
        assert!(target.validate(&roots, &trees, &configuration).is_err());
        assert!(uncached.validate(&roots, &trees, &configuration).is_err());
        assert_eq!(uncached.source(&uncached.facts)?, source);
        assert_eq!(target.source(&target.facts)?, source);
        let fresh = call_request(workspace.path(), dependency.path(), &roots)?;
        let changed = QueryPackageTarget::prepare(fresh, &roots, &trees, &configuration)?;
        assert_ne!(target.digest, changed.digest);
        assert_ne!(
            super::captured_revision(target.digest),
            super::captured_revision(changed.digest)
        );
        changed.validate(&roots, &trees, &configuration)?;
        Ok(())
    }

    #[test]
    fn test_runtime_target_uses_the_admitted_python_runtime_owner() -> TestResult {
        let workspace = tempfile::tempdir()?;
        let runtime = tempfile::tempdir()?;
        std::fs::write(
            runtime.path().join("builtins.pyi"),
            "def beacon() -> int: ...\n",
        )?;
        let origin = rift_core::SourceLocation::Stdlib {
            runtime: Some(rift_protocol::read::RuntimeIdentity {
                runtime: "cpython".to_owned(),
                version: "3.12.4".to_owned(),
            }),
        };
        let root = TreeRoot::new(runtime.path())?;
        let uri = root.document_uri(&ProjectPath::new("builtins.pyi")?)?;
        let roots = CalleeRoots::from_packages(vec![PackageRoot::with_origin(root, origin)]);
        let request = QuerySourceRequest::Reference {
            target: rift_protocol::read::SymbolId(
                "rift://symbol/local/python/app/beacon".to_owned(),
            ),
            uri,
            range: lsp_types::Range::new(Position::new(0, 4), Position::new(0, 10)),
            encoding: PositionEncoding::Utf16,
            provider: Language::from_identity_segment("python")?,
            facet: rift_protocol::read::RelationshipFacet::References,
            file_level: false,
        };
        let target = QueryPackageTarget::prepare(
            request,
            &roots,
            &[workspace.path()],
            &WorkspaceConfiguration::default(),
        )?;
        let symbol = target
            .facts
            .symbols
            .iter()
            .find(|symbol| symbol.symbol.name == "beacon")
            .expect("canonical runtime declaration");
        let identity = symbol
            .symbol
            .id
            .as_ref()
            .expect("runtime declaration identity");
        let identity = rift_protocol::identity::SymbolIdentity::parse(identity.as_str())?;
        assert_eq!(
            identity.owner(),
            &rift_protocol::identity::SymbolOwner::Runtime {
                runtime: "cpython".to_owned(),
                version: "3.12.4".to_owned(),
            }
        );
        target.validate(
            &roots,
            &[workspace.path()],
            &WorkspaceConfiguration::default(),
        )?;
        Ok(())
    }

    #[test]
    fn test_package_target_refuses_owner_substitution_and_source_byte_overflow() -> TestResult {
        let workspace = tempfile::tempdir()?;
        let dependency = tempfile::tempdir()?;
        let source = format!("export function entry() {{}}\n// {}\n", "x".repeat(2_000));
        std::fs::write(dependency.path().join("index.ts"), source)?;
        let accepted = roots(dependency.path(), package())?;
        let request = call_request(workspace.path(), dependency.path(), &accepted)?;
        let mut changed = package();
        changed.version = "2.0.0".to_owned();
        let substituted = roots(dependency.path(), changed)?;
        let trees = [workspace.path()];
        let mut configuration = WorkspaceConfiguration::default();
        assert!(
            QueryPackageTarget::prepare(request.clone(), &substituted, &trees, &configuration)
                .is_err()
        );
        configuration.providers.syntax.max_file =
            rift_protocol::configuration::ByteSize::from_bytes(1_024);
        assert!(QueryPackageTarget::prepare(request, &accepted, &trees, &configuration).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_package_target_refuses_a_symlink_outside_its_admitted_root() -> TestResult {
        let workspace = tempfile::tempdir()?;
        let dependency = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        let path = outside.path().join("index.ts");
        std::fs::write(&path, "export function entry() {}\n")?;
        std::os::unix::fs::symlink(&path, dependency.path().join("index.ts"))?;
        let roots = roots(dependency.path(), package())?;
        let request = call_request(workspace.path(), dependency.path(), &roots)?;
        let configuration = WorkspaceConfiguration::default();
        let trees = [workspace.path()];
        assert!(QueryPackageTarget::prepare(request, &roots, &trees, &configuration).is_err());
        assert_eq!(
            std::fs::read_to_string(path)?,
            "export function entry() {}\n"
        );
        Ok(())
    }
    #[test]
    fn test_rust_target_captures_original_module_witnesses() -> TestResult {
        let workspace = tempfile::tempdir()?;
        let dependency = tempfile::tempdir()?;
        std::fs::create_dir(dependency.path().join("src"))?;
        let manifest = "[package]\nname = \"beacon\"\nversion = \"1.0.0\"\nedition = \"2024\"\n";
        std::fs::write(dependency.path().join("Cargo.toml"), manifest)?;
        std::fs::write(
            dependency.path().join("src/lib.rs"),
            "mod inner;\npub fn top() { inner::middle(); }\n",
        )?;
        let source = "pub fn middle() { leaf(); }\nfn leaf() {}\n";
        std::fs::write(dependency.path().join("src/inner.rs"), source)?;
        let package = PackageIdentity {
            manager: "cargo".to_owned(),
            registry: "crates.io".to_owned(),
            name: "beacon".to_owned(),
            version: "1.0.0".to_owned(),
        };
        let roots = roots(dependency.path(), package.clone())?;
        let uri =
            TreeRoot::new(dependency.path())?.document_uri(&ProjectPath::new("src/inner.rs")?)?;
        let caller = rift_core::SymbolId::new("rift://symbol/local/rust/app/entry")?;
        let named = NamedCallee {
            uri: &uri,
            name: "middle",
            position: Position::new(0, 7),
            encoding: PositionEncoding::Utf16,
        };
        let CalleeFile::Package(callee) = callee_file(&roots, &[workspace.path()], &caller, named)?
        else {
            panic!("admitted Cargo source");
        };
        let request = QuerySourceRequest::Call(
            callee.with_query_sources(Vec::new(), Language::from_identity_segment("rust")?),
        );
        let configuration = WorkspaceConfiguration::default();
        let target =
            QueryPackageTarget::prepare(request, &roots, &[workspace.path()], &configuration)?;
        let symbol = target.symbol().expect("canonical Rust declaration");
        let identity = rift_protocol::identity::SymbolIdentity::parse(symbol.as_str())?;
        assert_eq!(identity.owner(), &package.owner()?);
        assert!(
            symbol.as_str().ends_with("/beacon/inner/middle"),
            "{symbol:?}"
        );
        assert!(
            target
                .facts
                .symbols
                .iter()
                .any(|symbol| symbol.symbol.name == "leaf")
        );
        assert_eq!(target.facts.sources.len(), 2);
        target.validate(&roots, &[workspace.path()], &configuration)?;
        let checks = std::sync::atomic::AtomicUsize::new(0);
        let cancelled = QueryPackageTarget::prepare_with_cancellation(
            target.request.clone(),
            &roots,
            &[workspace.path()],
            &configuration,
            &|| checks.fetch_add(1, std::sync::atomic::Ordering::Relaxed) > 2,
        )
        .expect_err("cancelled Rust source pass");
        assert_eq!(
            cancelled.slug(),
            rift_error::errors::server::read_cancelled::SLUG
        );
        std::fs::write(dependency.path().join("src/lib.rs"), "mod other;\n")?;
        assert!(
            target
                .validate(&roots, &[workspace.path()], &configuration)
                .is_err()
        );
        Ok(())
    }
    #[test]
    fn package_source_preparation_observes_cancellation_before_reading() -> TestResult {
        let workspace = tempfile::tempdir()?;
        let dependency = tempfile::tempdir()?;
        let roots = roots(dependency.path(), package())?;
        let request = call_request(workspace.path(), dependency.path(), &roots)?;
        let error = QueryPackageTarget::prepare_with_cancellation(
            request,
            &roots,
            &[workspace.path()],
            &WorkspaceConfiguration::default(),
            &|| true,
        )
        .expect_err("cancelled before missing source read");
        assert_eq!(
            error.slug(),
            rift_error::errors::server::read_cancelled::SLUG
        );
        Ok(())
    }
}
