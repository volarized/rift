//! Offline Cargo source association through the existing bounded process owner.

use std::ffi::OsString;
use std::sync::Arc;
use std::time::Duration;

use rift_dependency::{CargoResolver, InstallLocation};
use rift_error::{RiftError, errors};
use rift_protocol::configuration::WorkspaceConfiguration;
use tokio::time::Instant;

use super::{CalleeRoots, RootSpellings, folder_roots, query_source_error, root_order};
use crate::process::{Command, RunEnding, run_bounded};
use crate::{EnginePool, ReadService};

impl CalleeRoots {
    /// Captures exact Cargo source roots for a semantic query, blocking.
    ///
    /// The caller uses its existing cancellation-aware worker. Cargo runs in frozen
    /// mode, inherits the server environment and receives the selected engine overlay.
    /// The accepted command timeout and remaining request deadline bound its lifetime;
    /// dependency collection bounds limit captured metadata and admitted packages.
    ///
    /// # Errors
    /// Refuses cancelled or expired requests. Other failed observations leave source
    /// roots unresolved; requiring a foreign source returns the original typed failure.
    pub fn read_query(
        reads: &ReadService,
        engines: &EnginePool,
        configuration: &WorkspaceConfiguration,
        deadline: Instant,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<Self, RiftError> {
        let roots = Self::read(reads, engines);
        if !roots.registry_unresolved {
            return Ok(roots);
        }
        let Some((_, slot)) = engines
            .served_slots()
            .find(|(language, _)| *language == "rust")
        else {
            return Ok(roots);
        };
        if cancelled() {
            return errors::server::read_cancelled().fail();
        }
        let mut unresolved = roots.clone();
        match roots.associate_query(reads, slot, configuration, deadline, cancelled) {
            Ok(roots) => Ok(roots),
            Err(_) if cancelled() => errors::server::read_cancelled().fail(),
            Err(error)
                if Instant::now() >= deadline
                    || error.slug() == errors::server::read_cancelled::SLUG =>
            {
                Err(error)
            }
            Err(error) => {
                unresolved.registry_unavailable = Some(Arc::new(error));
                Ok(unresolved)
            }
        }
    }

    fn associate_query(
        mut self,
        reads: &ReadService,
        slot: &crate::EngineSlot,
        configuration: &WorkspaceConfiguration,
        deadline: Instant,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<Self, RiftError> {
        let timeout =
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(
                    configuration.dependencies.command_timeout.milliseconds(),
                ));
        if timeout.is_zero() {
            return Err(query_source_error(
                "Cargo source acquisition exceeded the request deadline",
            ));
        }
        let collection = configuration.dependencies.collection;
        let bytes = usize::try_from(collection.lockfile_size.bytes())
            .map_err(|error| query_source_error(error.to_string()))?;
        let path = slot
            .configuration()
            .environment
            .get("PATH")
            .map(OsString::from)
            .or_else(|| std::env::var_os("PATH"));
        let program = which::which_in("cargo", path, slot.workspace_root())
            .map_err(|error| query_source_error(error.to_string()))?;
        let mut command = Command::new(program);
        command
            .current_dir(slot.workspace_root())
            .args(["metadata", "--format-version=1", "--frozen"])
            .envs(&slot.configuration().environment)
            .env("RUSTUP_AUTO_INSTALL", "0")
            .env("CARGO_NET_OFFLINE", "true");
        let metadata = capture(&mut command, timeout, bytes, cancelled)?;
        let workspace = std::fs::canonicalize(reads.index().root())
            .map_err(|error| query_source_error(error.to_string()))?;
        let count = usize::try_from(collection.packages)
            .ok()
            .and_then(|packages| packages.checked_add(collection.manifests as usize))
            .ok_or_else(|| {
                query_source_error("Cargo metadata package bound cannot be represented")
            })?;
        let folders = CargoResolver::installed_folders(
            metadata.as_bytes(),
            &workspace,
            reads.dependency_context(),
            bytes,
            count,
        )
        .map_err(query_source_error)?;
        let spellings = RootSpellings {
            index_root: reads.index().root(),
            engine_roots: &[],
        };
        let mut packages = self.packages.to_vec();
        packages.extend(
            folders
                .iter()
                .flat_map(|folder| folder_roots(folder, spellings)),
        );
        packages.sort_by(|left, right| root_order(left).cmp(&root_order(right)));
        packages.dedup();
        self.packages = Arc::from(packages);
        self.registry_unresolved = reads.dependency_context().install_folders().any(|folder| {
            matches!(folder.location, InstallLocation::CargoRegistry { .. })
                && !folders
                    .iter()
                    .any(|verified| verified.origin == folder.origin)
        });
        Ok(self)
    }
}

fn capture(
    command: &mut Command,
    timeout: Duration,
    bytes: usize,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<String, RiftError> {
    let run = run_bounded(command, timeout, bytes, cancelled)
        .map_err(|error| query_source_error(error.to_string()))?;
    match run.ending {
        RunEnding::Cancelled => return errors::server::read_cancelled().fail(),
        RunEnding::TimedOut => {
            return Err(query_source_error(
                "Cargo metadata exceeded the request deadline or command timeout",
            ));
        }
        RunEnding::Exited => {}
    }
    if run.stdout.truncated
        || run.stderr.truncated
        || run
            .stdout
            .total_bytes
            .saturating_add(run.stderr.total_bytes)
            > bytes as u64
    {
        return Err(query_source_error(
            "Cargo metadata exceeds dependencies.collection.lockfile_size",
        ));
    }
    if !run
        .exit
        .map_err(|error| query_source_error(error.to_string()))?
        .success()
    {
        return Err(query_source_error(
            "Cargo metadata is unavailable in frozen mode; install the locked Cargo dependencies and retry",
        ));
    }
    Ok(run.stdout.text)
}

#[cfg(all(test, unix))]
#[path = "../../../rift-lsp/tests/live_engine_gate.rs"]
mod live_engine_gate;

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn shell(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn cargo_metadata_capture_bounds_both_streams_and_exit() {
        assert_eq!(
            capture(
                &mut shell("printf metadata"),
                Duration::from_secs(1),
                8,
                &|| false
            )
            .expect("bounded output"),
            "metadata"
        );
        assert!(
            capture(
                &mut shell("printf metadata; printf failure >&2"),
                Duration::from_secs(1),
                8,
                &|| false
            )
            .is_err()
        );
        assert!(capture(&mut shell("exit 1"), Duration::from_secs(1), 8, &|| false).is_err());
    }

    #[test]
    fn cargo_metadata_capture_cancels_and_reaps_before_returning() {
        let start = std::time::Instant::now();
        let error = capture(
            &mut shell("exec sleep 30"),
            Duration::from_secs(30),
            8,
            &|| true,
        )
        .expect_err("cancelled child");
        assert!(error.to_string().contains("cancel"), "{error}");
        assert!(start.elapsed() < Duration::from_secs(2));
        let start = std::time::Instant::now();
        assert!(
            capture(
                &mut shell("exec sleep 30"),
                Duration::from_millis(30),
                8,
                &|| false
            )
            .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(2));
    }
    struct CargoFixture {
        _workspace: tempfile::TempDir,
        reads: ReadService,
        engines: EnginePool,
        roots: CalleeRoots,
        configuration: WorkspaceConfiguration,
    }

    fn installed_tokio_fixture() -> Result<CargoFixture, Box<dyn std::error::Error>> {
        let phase = std::time::Instant::now();
        eprintln!("Tokio fixture: begin");
        let workspace = tempfile::tempdir()?;
        std::fs::write(
            workspace.path().join("Cargo.toml"),
            "[package]\nname='beacon'\nversion='1.0.0'\nedition='2024'\n[lib]\npath='lib.rs'\n[dependencies]\ntokio={version='1',default-features=false,features=['sync']}\n",
        )?;
        std::fs::write(
            workspace.path().join("lib.rs"),
            "pub fn beacon() { let _ = tokio::sync::mpsc::channel::<u8>(1); }\n",
        )?;
        let cargo = which::which("cargo")?;
        let mut setup = Command::new(cargo);
        setup
            .current_dir(workspace.path())
            .args(["generate-lockfile", "--offline"])
            .env("RUSTUP_AUTO_INSTALL", "0")
            .env("CARGO_NET_OFFLINE", "true");
        capture(&mut setup, Duration::from_secs(10), 64 << 10, &|| false)?;
        let lock = std::fs::read(workspace.path().join("Cargo.lock"))?;
        let reads = ReadService::build(
            workspace.path(),
            rift_index::WorkspaceIndexLimits::default(),
            &rift_core::SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            rift_protocol::configuration::HistoryConfiguration::default(),
        )?;
        let configuration = WorkspaceConfiguration::default();
        let binding = crate::LspProcessKey::named("cargo-test");
        let engine = serde_json::from_value(
            serde_json::json!({"command":["rustup","run","1.98","rust-analyzer"],"environment":{"RUSTUP_AUTO_INSTALL":"0"},"retry":{"attempts":16}}),
        )?;
        let engines = EnginePool::new(
            workspace.path(),
            std::collections::BTreeMap::from([(binding.clone(), engine)]),
            std::collections::BTreeMap::from([("rust".to_owned(), binding)]),
        );
        let roots = CalleeRoots::read_query(
            &reads,
            &engines,
            &configuration,
            Instant::now() + Duration::from_secs(10),
            &|| false,
        )?;
        assert_eq!(std::fs::read(workspace.path().join("Cargo.lock"))?, lock);
        let root = roots.packages.iter().find(|root| matches!(root.origin(), rift_core::SourceLocation::Dependency { package } if package.manager == "cargo" && package.registry == "crates.io" && package.name == "tokio")).ok_or("Cargo-reported Tokio source root")?;
        let uri = root
            .root()
            .document_uri(&rift_core::ProjectPath::new("src/sync/mpsc/bounded.rs")?)?;
        let (file, text) = roots.capture_query_file(
            &uri,
            &[workspace.path()],
            configuration.providers.syntax.max_file.bytes(),
        )?;
        let offset = text
            .find("pub fn channel")
            .ok_or("Tokio channel declaration")?
            + "pub fn ".len();
        let position = rift_lsp::position::LineIndex::new(&text)
            .position(rift_lsp::capabilities::PositionEncoding::Utf16, offset)?;
        let caller = rift_core::SymbolId::new("rift://symbol/local/rust/beacon/beacon")?;
        let named = crate::callee::NamedCallee {
            uri: &uri,
            name: "channel",
            position,
            encoding: rift_lsp::capabilities::PositionEncoding::Utf16,
        };
        let crate::callee::CalleeFile::Package(callee) =
            crate::callee::callee_file(&roots, &[workspace.path()], &caller, named)?
        else {
            panic!("exact Cargo source owner");
        };
        let inventory_root = crate::callee::query_file_path(&root.root().root_uri()?)?;
        let inventory = rift_index::WorkspaceIndex::build(
            &inventory_root,
            rift_index::WorkspaceIndexLimits::default(),
            &rift_core::SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
        )?;
        let sources = inventory
            .files()
            .filter(|file| file.syntax().language().name == "rust")
            .map(|file| rift_analysis::PackageSource::new(file.path(), file.source()))
            .collect::<Vec<_>>();
        let manifest_path = rift_core::ProjectPath::new("Cargo.toml")?;
        let manifest_uri = root.root().document_uri(&manifest_path)?;
        let (_, manifest_text) = roots.capture_query_file(
            &manifest_uri,
            &[workspace.path()],
            configuration.providers.syntax.max_file.bytes(),
        )?;
        let metadata = [rift_analysis::PackageSource::new(
            &manifest_path,
            &manifest_text,
        )];
        let owner = match file.origin() {
            rift_core::SourceLocation::Dependency { package } => package.owner()?,
            _ => unreachable!("admitted Cargo package"),
        };
        let origin = rift_core::ContributionOrigin::new(
            Some(file.origin().clone()),
            rift_core::SourceKind::Authored,
        )?;
        let language = rift_protocol::read::Language::from_identity_segment("rust")?;
        let selected_sources = [rift_analysis::PackageSource::new(file.path(), &text)];
        let selected_input = rift_analysis::ExactPackageInput::new(
            &owner,
            &language,
            &origin,
            &selected_sources,
            rift_analysis::ExactPackageLimits::from_configuration(&configuration)?,
        )?
        .with_framework_context(&metadata, &[])?;
        let selected = rift_analysis::PackageAnalyzer::analyze(selected_input, 1)?;
        assert_eq!(selected.files().len(), 1);
        let parsed = selected.files()[0].file();
        assert_eq!(parsed.path(), file.path());
        assert_eq!(parsed.source(), text);
        assert_eq!(
            rift_core::FileDigest::of(parsed.source().as_bytes()),
            rift_core::FileDigest::of(text.as_bytes()),
        );
        let selected_offset = u64::try_from(offset)?;
        let declaration = parsed
            .enclosing_symbol(selected_offset, selected_offset + 1)
            .ok_or("selected Tokio syntax declaration")?;
        let name_range = declaration
            .name_range
            .as_ref()
            .ok_or("selected Tokio declaration name range")?;
        assert_eq!(name_range.start, selected_offset);
        assert_eq!(
            text.get(usize::try_from(name_range.start)?..usize::try_from(name_range.end)?),
            Some("channel"),
        );
        let declaration_range = (declaration.item_range.start, declaration.item_range.end);
        let (_, _, selected_semantics, _) = selected.into_parts();
        let scope = selected_semantics
            .graph()
            .scope_at(file.unit().clone(), selected_offset);
        let physical = scope
            .contributions()
            .iter()
            .filter_map(|key| {
                let contribution = selected_semantics.graph().contribution(key)?;
                let binding = contribution.source()?;
                let facts = contribution.facts()?;
                if binding.unit() != file.unit()
                    || (binding.range().start(), binding.range().end()) != declaration_range
                    || facts.name() != "channel"
                {
                    return None;
                }
                let record = selected_semantics.graph().record_for(key.reference())?;
                Some((key, record))
            })
            .collect::<Vec<_>>();
        let graph = selected_semantics.graph();
        let candidates = graph
            .records()
            .iter()
            .flat_map(|record| record.contributions().iter().map(move |key| (record, key)))
            .filter_map(|(record, key)| {
                let contribution = graph.contribution(key)?;
                Some((
                    key,
                    contribution.facts().map(|facts| facts.name()),
                    contribution.source(),
                    record.identity(),
                ))
            })
            .take(16)
            .collect::<Vec<_>>();
        eprintln!(
            "selected-file Tokio unit {:?} syntax range {:?} scope {:?} first records {:?}",
            file.unit(),
            declaration_range,
            scope.contributions().iter().take(16).collect::<Vec<_>>(),
            candidates,
        );
        assert_eq!(physical.len(), 1);
        assert!(selected_semantics.assembled_record(physical[0].1).is_some());
        let selected_identity = physical[0].1.identity().cloned();
        eprintln!(
            "selected-file Tokio exact physical declaration {:?} range {:?} identity {:?}",
            physical[0].0,
            declaration_range,
            physical[0].1.identity(),
        );
        let input = rift_analysis::ExactPackageInput::new(
            &owner,
            &language,
            &origin,
            &sources,
            rift_analysis::ExactPackageLimits::from_configuration(&configuration)?,
        )?
        .with_framework_context(&metadata, &[])?;
        let (library_path, _) = input
            .namespace_input()
            .library_target()
            .ok_or("original Cargo library target")?;
        let library = sources
            .iter()
            .find(|source| source.path() == &library_path)
            .copied()
            .ok_or("captured library root")?;
        let input = input.with_library(&owner, metadata[0], library)?;
        let oracle = rift_analysis::PackageAnalyzer::analyze(input, 1)?;
        let original = oracle
            .files()
            .iter()
            .find(|original| original.file().path() == file.path())
            .ok_or("original selected Tokio source")?;
        assert_eq!(original.file().source(), text);
        let selected_unit = file.unit().to_string();
        eprintln!(
            "full Tokio oracle Rust files {} declarations {} selected {:?}",
            sources.len(),
            oracle.publication().declarations.len(),
            oracle
                .publication()
                .declarations
                .iter()
                .filter(|declaration| declaration.unit.as_str() == selected_unit.as_str())
                .take(16)
                .map(|declaration| (&declaration.symbol, &declaration.range))
                .collect::<Vec<_>>()
        );
        let (_, _, semantics, _) = oracle.into_parts();
        let scope = semantics
            .graph()
            .scope_at(file.unit().clone(), u64::try_from(offset)?);
        let selected = scope
            .contributions()
            .iter()
            .take(16)
            .filter_map(|key| {
                let contribution = semantics.graph().contribution(key)?;
                let binding = contribution.source()?;
                let record = semantics.graph().record_for(key.reference())?;
                Some((
                    key,
                    contribution.facts().map(|facts| facts.name()),
                    binding.range(),
                    record.identity(),
                    semantics.assembled_record(record).is_some(),
                ))
            })
            .collect::<Vec<_>>();
        eprintln!("full Tokio oracle selected provider records {selected:?}");
        let original = selected
            .iter()
            .find(|(_, name, range, _, _)| {
                *name == Some("channel") && (range.start(), range.end()) == declaration_range
            })
            .ok_or("authoritative Tokio physical declaration")?;
        assert_eq!(selected_identity.as_ref(), original.3);
        assert!(original.4);
        eprintln!(
            "Tokio fixture: exact namespace preparation starts at {:?}",
            phase.elapsed()
        );
        let target = crate::QueryPackageTarget::prepare(
            crate::QuerySourceRequest::Call(callee.with_query_sources(
                Vec::new(),
                rift_protocol::read::Language::from_identity_segment("rust")?,
            )),
            &roots,
            &[workspace.path()],
            &configuration,
        )?;
        assert!(
            target.symbol().is_none(),
            "Tokio channel has no established canonical identity"
        );
        target.validate(&roots, &[workspace.path()], &configuration)?;
        eprintln!("Tokio fixture: complete at {:?}", phase.elapsed());
        Ok(CargoFixture {
            _workspace: workspace,
            reads,
            engines,
            roots,
            configuration,
        })
    }

    #[test]
    fn cargo_metadata_admits_installed_tokio_from_its_frozen_workspace()
    -> Result<(), Box<dyn std::error::Error>> {
        installed_tokio_fixture().map(|_| ())
    }

    #[tokio::test]
    async fn live_cargo_tokio_physical_target_keeps_source_view()
    -> Result<(), Box<dyn std::error::Error>> {
        if !live_engine_gate::engine_live() {
            return Ok(());
        }
        let fixture = installed_tokio_fixture()?;
        let result = physical_tokio_call(&fixture).await;
        fixture.engines.shutdown().await;
        result
    }

    async fn physical_tokio_call(fixture: &CargoFixture) -> Result<(), Box<dyn std::error::Error>> {
        use rift_protocol::query::{QueryAnswer, RdfTerm};
        use rift_protocol::source_read::{GetSourceParams, GetSourceResult};
        use rift_search::query::QueryLimits;
        use rift_search::sparql::{PreparedQuery, QueryCancellation};

        let phase = std::time::Instant::now();
        eprintln!("Tokio physical: engine resolve starts");
        let material = fixture.reads.local_search_view_input(true)?;
        let selection = material.provider_selection(&fixture.configuration)?;
        let mut query = crate::EngineQuery::new(
            &fixture.reads,
            &fixture.engines,
            &fixture.configuration,
            Instant::now() + Duration::from_secs(60),
            &fixture.roots,
        )
        .with_provider_selection(selection.clone());
        let caller =
            rift_protocol::read::SymbolId("rift://symbol/local/rust/beacon/beacon".to_owned());
        query
            .resolve(
                &[caller.clone()],
                rift_protocol::read::TraversalDirection::Outgoing,
            )
            .await?;
        eprintln!(
            "Tokio physical: engine resolve complete at {:?}",
            phase.elapsed()
        );
        assert!(
            !query.source_requests().is_empty(),
            "actual Rust engine must select installed dependency source"
        );
        for request in query.source_requests() {
            let target = crate::QueryPackageTarget::prepare_selected_with_selection(
                request,
                &fixture.roots,
                &[fixture.reads.index().root()],
                &fixture.configuration,
                &|| false,
                None,
                Some(&selection),
            )?;
            query.admit_package_target(target)?;
        }
        assert!(
            query.references().is_complete(),
            "every actual engine target must be admitted"
        );
        eprintln!(
            "Tokio physical: source admission complete at {:?}",
            phase.elapsed()
        );
        let packages = query.references().query_package_facts().clone();
        assert!(
            !packages.provider_edges.is_empty(),
            "physical dependency edge retained"
        );
        let channel = packages
            .symbols
            .iter()
            .find(|symbol| symbol.symbol.name == "channel")
            .ok_or("actual Tokio channel physical target")?;
        assert!(channel.symbol.id.is_none());
        let declaration = channel
            .declarations
            .first()
            .ok_or("original physical declaration")?;
        let unit = declaration.unit.clone().ok_or("original source unit")?;
        let range = declaration.range.clone();
        let expected = packages
            .sources
            .iter()
            .find(|source| source.unit == unit)
            .and_then(|source| source.text.as_deref())
            .ok_or("captured installed source")?
            .to_owned();
        assert!(
            expected
                .get(usize::try_from(range.start)?..usize::try_from(range.end)?)
                .is_some_and(|text| text.contains("pub fn channel"))
        );
        let limits = QueryLimits::try_from(&fixture.configuration.search.query)?;
        let mut facts = fixture.reads.query_facts(true, false, limits)?;
        crate::merge_query_facts(&mut facts, packages.clone())?;
        let statement = format!(
            "SELECT ?target ?name WHERE {{ <{}> :calls ?target . ?target :name ?name FILTER(?name = \"channel\") }}",
            caller.as_str()
        );
        let answer = PreparedQuery::parse(&statement, limits)?.evaluate(
            facts,
            limits,
            QueryCancellation::default(),
        )?;
        let QueryAnswer::Bindings { bindings, .. } = answer else {
            panic!("SELECT bindings");
        };
        assert_eq!(bindings.len(), 1, "one physical channel resource");
        assert!(matches!(
            bindings[0].get("target"),
            Some(RdfTerm::BlankNode { .. })
        ));
        let owner = material.owner().clone();
        let root = material.root().to_owned();
        let material = material
            .with_provider_selection(selection)?
            .with_package_facts(packages)?;
        let views = crate::LocalSymbolViews::new(64 << 20, [41; 32]);
        let view = views
            .capture(material)?
            .ok_or("captured physical target view")?;
        std::fs::write(root.join("lib.rs"), "pub fn changed() {}\n")?;
        let source = views.get_source(
            &GetSourceParams {
                unit: unit.clone(),
                range: None,
                view: Some(view.id.clone()),
                rev: None,
            },
            &root,
            &owner,
        )?;
        let GetSourceResult::Found {
            source,
            source_complete,
            ..
        } = source
        else {
            panic!("held physical source must remain readable");
        };
        assert!(source_complete);
        assert_eq!(source.text, expected);
        assert_eq!(source.span.unit, unit);
        Ok(())
    }

    #[tokio::test]
    async fn live_cargo_tokio_calls_continue_through_three_levels()
    -> Result<(), Box<dyn std::error::Error>> {
        if !live_engine_gate::engine_live() {
            return Ok(());
        }
        let fixture = installed_tokio_fixture()?;
        let result = live_tokio_calls(
            &fixture.reads,
            &fixture.engines,
            &fixture.roots,
            &fixture.configuration,
        )
        .await;
        fixture.engines.shutdown().await;
        result
    }

    async fn live_tokio_calls(
        reads: &ReadService,
        engines: &EnginePool,
        roots: &CalleeRoots,
        configuration: &WorkspaceConfiguration,
    ) -> Result<(), Box<dyn std::error::Error>> {
        use rift_protocol::read::{RelationshipFacet, TraversalDirection};
        use rift_search::query::QueryProviderTarget;
        let phase = std::time::Instant::now();
        let selection = reads
            .local_search_view_input(true)?
            .provider_selection(configuration)?;
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut query = crate::EngineQuery::new(reads, engines, configuration, deadline, roots)
            .with_provider_selection(selection.clone());
        let caller =
            rift_protocol::read::SymbolId("rift://symbol/local/rust/beacon/beacon".to_owned());
        let caller = QueryProviderTarget::canonical(&caller)?;
        let mut frontier = std::collections::BTreeSet::from([query
            .references()
            .query_package_facts()
            .provider_endpoint(&caller)
            .ok_or("canonical seed endpoint")?]);
        for depth in 0..3 {
            eprintln!(
                "Tokio frontier {depth}: resolve starts at {:?}, subjects {}",
                phase.elapsed(),
                frontier.len()
            );
            let subjects = frontier
                .iter()
                .map(|endpoint| {
                    query
                        .references()
                        .query_package_facts()
                        .provider_target(endpoint)
                        .ok_or("scoped frontier subject")
                })
                .collect::<Result<Vec<_>, _>>()?;
            query
                .resolve_targets(
                    &subjects,
                    TraversalDirection::Outgoing,
                    &[RelationshipFacet::Calls],
                )
                .await?;
            eprintln!(
                "Tokio frontier {depth}: resolve complete at {:?}, source requests {}",
                phase.elapsed(),
                query.source_requests().len()
            );
            for request in query.source_requests() {
                let target = crate::QueryPackageTarget::prepare_selected_with_selection(
                    request,
                    roots,
                    &[reads.index().root()],
                    configuration,
                    &|| false,
                    None,
                    Some(&selection),
                )?;
                query.admit_package_target(target)?;
            }
            eprintln!(
                "Tokio frontier {depth}: source admission complete at {:?}",
                phase.elapsed()
            );
            assert!(
                query.references().is_complete(),
                "complete physical frontier {depth}"
            );
            let mut next = std::collections::BTreeSet::new();
            for subject in &frontier {
                next.extend(query.provider_neighbors(
                    subject,
                    TraversalDirection::Outgoing,
                    &[RelationshipFacet::Calls],
                )?);
            }
            assert!(!next.is_empty(), "Tokio call frontier {depth}");
            assert!(next.iter().any(|endpoint| {
                endpoint.provider.and_then(|provider| query.references().query_package_facts().providers.get(provider))
                    .is_some_and(|snapshot| matches!(&snapshot.context.owner,
                        rift_protocol::identity::SymbolOwner::Package { name, .. } if name == "tokio"))
                    || endpoint.canonical_symbol().is_some_and(|symbol| {
                        rift_protocol::identity::SymbolIdentity::parse(symbol.as_str()).is_ok_and(|identity|
                            matches!(identity.owner(), rift_protocol::identity::SymbolOwner::Package { name, .. } if name == "tokio"))
                    })
            }), "original Tokio ownership at frontier {depth}: {next:?}");
            frontier = next;
        }
        for target in query.validation_targets() {
            target.validate(roots, &[reads.index().root()], configuration)?;
        }
        let limits = rift_search::query::QueryLimits::try_from(&configuration.search.query)?;
        let mut facts = reads.query_facts(true, false, limits)?;
        crate::merge_query_facts(&mut facts, query.references().query_package_facts().clone())?;
        facts
            .relationships
            .extend(query.references().query_relationships());
        let answer = rift_search::sparql::PreparedQuery::parse(
            "SELECT ?target WHERE { <rift://symbol/local/rust/beacon/beacon> :calls/:calls/:calls ?target }", limits,
        )?.evaluate(facts, limits, rift_search::sparql::QueryCancellation::default())?;
        let rift_protocol::query::QueryAnswer::Bindings { bindings, .. } = answer else {
            panic!("SELECT bindings");
        };
        assert!(
            !bindings.is_empty(),
            "library evaluates actual three-step engine path"
        );
        Ok(())
    }
}
