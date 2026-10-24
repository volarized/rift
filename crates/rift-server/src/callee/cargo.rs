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
        let target = crate::QueryPackageTarget::prepare(
            crate::QuerySourceRequest::Call(callee.with_query_sources(
                Vec::new(),
                rift_protocol::read::Language::from_identity_segment("rust")?,
            )),
            &roots,
            &[workspace.path()],
            &configuration,
        )?;
        let symbol = target.symbol().ok_or("canonical Tokio channel")?;
        let identity = rift_protocol::identity::SymbolIdentity::parse(symbol.as_str())?;
        let rift_core::SourceLocation::Dependency { package } = file.origin() else {
            panic!("Cargo package origin");
        };
        assert_eq!(identity.owner(), &package.owner()?);
        assert!(
            symbol
                .as_str()
                .ends_with("/tokio/sync/mpsc/bounded/channel"),
            "{symbol:?}"
        );
        target.validate(&roots, &[workspace.path()], &configuration)?;
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
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut query = crate::EngineQuery::new(reads, engines, configuration, deadline, roots);
        let mut frontier = vec![rift_protocol::read::SymbolId(
            "rift://symbol/local/rust/beacon/beacon".to_owned(),
        )];
        for depth in 0..3 {
            query
                .resolve(&frontier, rift_protocol::read::TraversalDirection::Outgoing)
                .await?;
            for request in query.source_requests() {
                let target = crate::QueryPackageTarget::prepare(
                    request,
                    roots,
                    &[reads.index().root()],
                    configuration,
                )?;
                query.admit_package_target(target)?;
            }
            assert!(query.references().is_complete());
            let next = query
                .references()
                .query_relationships()
                .into_iter()
                .filter(|relationship| frontier.contains(&relationship.from))
                .map(|relationship| relationship.to)
                .collect::<std::collections::BTreeSet<_>>();
            assert!(!next.is_empty(), "Tokio call frontier {depth}");
            assert!(next.iter().any(|symbol| rift_protocol::identity::SymbolIdentity::parse(symbol.as_str()).is_ok_and(|identity| matches!(identity.owner(), rift_protocol::identity::SymbolOwner::Package { name, .. } if name == "tokio"))), "Tokio call frontier {depth}: {next:?}");
            frontier = next.into_iter().collect();
        }
        for target in query.validation_targets() {
            target.validate(roots, &[reads.index().root()], configuration)?;
        }
        Ok(())
    }
}
