use super::*;
use rift_core::{SourceVisibility, TextFileInclusion};
use rift_lsp::uri::{PackageRoot, TreeRoot};
use rift_protocol::configuration::HistoryConfiguration;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn selection() -> EngineInventorySelection {
    EngineInventorySelection {
        calls: false,
        references: true,
        enclosing: true,
        occurrences: None,
        types: rift_search::sparql::TypeSelection {
            definitions: true,
            extends: true,
            implements: true,
        },
        batch_inputs: 3,
        records_max: 100,
    }
}

fn fixture(root: &Path) -> Result<ReadService, RiftError> {
    ReadService::build(
        root,
        rift_index::WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
        HistoryConfiguration::default(),
    )
}

#[test]
fn authored_sites_share_reference_type_and_heritage_positions_with_a_bound() -> TestResult {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("graph.py"),
        "from greeting import Base\nclass Child(Base):\n    item: Base\n",
    )?;
    let reads = fixture(directory.path())?;
    let path = ProjectPath::new("graph.py")?;
    let file = reads.index().file(&path).ok_or("file")?;
    let seeds = reads
        .local_search_view_input(true)?
        .inventory_seeds()?
        .0
        .into_iter()
        .collect();
    let found = sites(&reads, file, &seeds, selection())?;
    let heritage = found
        .iter()
        .find(|site| site.kinds.contains(&EdgeKind::Extends))
        .ok_or("authored base")?;
    assert!(heritage.caller.as_str().ends_with("/Child"));
    assert_eq!(heritage.name, "Base");
    assert!(
        found
            .iter()
            .any(|site| site.kinds.contains(&EdgeKind::HasType))
    );
    assert!(
        found
            .iter()
            .any(|site| site.kinds.contains(&EdgeKind::References))
    );
    let bounded = EngineInventorySelection {
        records_max: 1,
        ..selection()
    };
    assert!(sites(&reads, file, &seeds, bounded).is_err());
    let calls_only = EngineInventorySelection {
        calls: true,
        references: false,
        types: rift_search::sparql::TypeSelection::default(),
        ..selection()
    };
    assert!(sites(&reads, file, &seeds, calls_only)?.is_empty());
    Ok(())
}

#[test]
fn external_types_and_heritage_never_reenter_the_local_mapper() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    std::fs::create_dir(root.join("installed"))?;
    std::fs::write(root.join("graph.py"), "class Child:\n    item: object\n")?;
    std::fs::write(root.join("installed/core.py"), "class Foreign: pass\n")?;
    let reads = fixture(&root)?;
    let child = reads
        .local_search_view_input(true)?
        .inventory_seeds()?
        .0
        .into_iter()
        .find(|seed| seed.as_str().ends_with("/Child"))
        .ok_or("Child")?;
    let package = rift_core::PackageIdentity {
        manager: "pypi".to_owned(),
        registry: "pypi.org".to_owned(),
        name: "greeting".to_owned(),
        version: "1.0.0".to_owned(),
    };
    let installed = TreeRoot::new(&root.join("installed"))?;
    let roots = CalleeRoots::from_packages(vec![PackageRoot::new(installed.clone(), package)]);
    let location = Location {
        uri: installed.document_uri(&ProjectPath::new("core.py")?)?,
        range: lsp_types::Range::new(Position::new(0, 6), Position::new(0, 13)),
    };
    for provider in [EdgeProvider::Engine, EdgeProvider::Lsp] {
        let mapping = Mapping {
            reads: &reads,
            roots: &roots,
            workspace: &root,
            encoding: PositionEncoding::Utf16,
            provider,
        };
        let found = mapping.resolve(
            vec![(child.clone(), vec![location.clone()])],
            vec![
                InventoryHeritage {
                    seed: child.clone(),
                    location: location.clone(),
                    kind: EdgeKind::Extends,
                    direction: TraversalDirection::Outgoing,
                },
                InventoryHeritage {
                    seed: child.clone(),
                    location: location.clone(),
                    kind: EdgeKind::Implements,
                    direction: TraversalDirection::Incoming,
                },
            ],
            Vec::new(),
        )?;
        assert!(
            found
                .types
                .iter()
                .all(|(_, locations)| locations.is_empty())
        );
        assert!(found.heritage.is_empty());
        assert!(found.warnings.is_empty());
        assert_eq!(found.packages.len(), 3);
        let id = SymbolId("rift://symbol/pypi/pypi.org/greeting/python/core/Foreign".to_owned());
        for target in found.packages {
            assert_eq!(target.target.path().as_str(), "core.py");
            assert_eq!(target.target.position(), location.range.start);
            let edge = target.named(id.clone());
            assert_eq!(edge.provider, provider);
            assert_eq!(edge.kinds, vec![target.kind]);
            assert_eq!(
                (&edge.from, &edge.to),
                match target.direction {
                    TraversalDirection::Outgoing => (&child, &id),
                    TraversalDirection::Incoming => (&id, &child),
                }
            );
        }
    }
    Ok(())
}

#[test]
fn unadmitted_targets_report_each_missing_kind_once() -> TestResult {
    let directory = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    std::fs::write(directory.path().join("graph.py"), "class Child: pass\n")?;
    let reads = fixture(directory.path())?;
    let child = reads
        .local_search_view_input(true)?
        .inventory_seeds()?
        .0
        .into_iter()
        .find(|seed| seed.as_str().ends_with("/Child"))
        .ok_or("Child")?;
    let location = Location {
        uri: TreeRoot::new(outside.path())?.document_uri(&ProjectPath::new("other.py")?)?,
        range: lsp_types::Range::new(Position::new(0, 6), Position::new(0, 11)),
    };
    let roots = CalleeRoots::default();
    let mapping = Mapping {
        reads: &reads,
        roots: &roots,
        workspace: directory.path(),
        encoding: PositionEncoding::Utf8,
        provider: EdgeProvider::Lsp,
    };
    let found = mapping.resolve(
        vec![(child.clone(), vec![location.clone(), location])],
        Vec::new(),
        Vec::new(),
    )?;
    assert!(found.packages.is_empty());
    assert!(
        matches!(found.warnings.as_slice(), [ReadWarning::EdgeCoverageMissing { kinds, .. }] if kinds == &[EdgeKind::HasType])
    );
    Ok(())
}

#[test]
fn a_file_only_position_names_no_first_declaration() -> TestResult {
    let directory = tempfile::tempdir()?;
    let package = tempfile::tempdir()?;
    std::fs::write(directory.path().join("graph.py"), "class Child: pass\n")?;
    std::fs::write(package.path().join("core.py"), "class First: pass\n")?;
    let reads = fixture(directory.path())?;
    let child = reads
        .local_search_view_input(true)?
        .inventory_seeds()?
        .0
        .into_iter()
        .find(|seed| seed.as_str().ends_with("/Child"))
        .ok_or("Child")?;
    let root = TreeRoot::new(package.path())?;
    let owner = rift_core::PackageIdentity {
        manager: "pypi".to_owned(),
        registry: "pypi.org".to_owned(),
        name: "greeting".to_owned(),
        version: "1.0.0".to_owned(),
    };
    let roots = CalleeRoots::from_packages(vec![PackageRoot::new(root.clone(), owner)]);
    let mapping = Mapping {
        reads: &reads,
        roots: &roots,
        workspace: directory.path(),
        encoding: PositionEncoding::Utf8,
        provider: EdgeProvider::Engine,
    };
    let location = Location {
        uri: root.document_uri(&ProjectPath::new("core.py")?)?,
        range: lsp_types::Range::new(Position::new(0, 0), Position::new(0, 0)),
    };
    let found = mapping.resolve(vec![(child, vec![location])], Vec::new(), Vec::new())?;
    assert!(found.packages.is_empty());
    assert!(
        found
            .types
            .iter()
            .all(|(_, locations)| locations.is_empty())
    );
    assert!(matches!(
        found.warnings.as_slice(),
        [ReadWarning::EdgeCoverageMissing { .. }]
    ));
    Ok(())
}

#[test]
fn local_type_sites_keep_definition_answers_in_the_type_inventory() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    std::fs::write(
        root.join("graph.py"),
        "class Foreign: pass\nclass Child:\n    item: Foreign\n",
    )?;
    let reads = fixture(&root)?;
    let path = ProjectPath::new("graph.py")?;
    let file = reads.index().file(&path).ok_or("file")?;
    let seeds = reads
        .local_search_view_input(true)?
        .inventory_seeds()?
        .0
        .into_iter()
        .collect();
    let site = sites(&reads, file, &seeds, selection())?
        .into_iter()
        .find(|site| site.kinds.contains(&EdgeKind::HasType))
        .ok_or("annotation")?;
    let caller = site.caller.clone();
    let location = Location {
        uri: TreeRoot::new(&root)?.document_uri(&path)?,
        range: lsp_types::Range::new(Position::new(0, 6), Position::new(0, 13)),
    };
    let roots = CalleeRoots::default();
    let mapping = Mapping {
        reads: &reads,
        roots: &roots,
        workspace: &root,
        encoding: PositionEncoding::Utf16,
        provider: EdgeProvider::Lsp,
    };
    let found = mapping.resolve(
        Vec::new(),
        Vec::new(),
        vec![Answer {
            site,
            locations: vec![location],
        }],
    )?;
    assert!(found.packages.is_empty());
    assert!(found.warnings.is_empty());
    let edges = reads.inventory_type_edges(
        &root,
        PositionEncoding::Utf16,
        found.types,
        EdgeProvider::Lsp,
    )?;
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].from, caller);
    assert!(edges[0].to.as_str().ends_with("/Foreign"));
    assert_eq!(edges[0].kinds, vec![EdgeKind::HasType]);
    assert_eq!(edges[0].provider, EdgeProvider::Lsp);
    Ok(())
}

/// The installed `greeting` distribution under `root/installed`.
fn installed_greeting(root: &Path) -> Result<(TreeRoot, CalleeRoots), Box<dyn std::error::Error>> {
    let installed = TreeRoot::new(&root.join("installed"))?;
    let package = rift_core::PackageIdentity {
        manager: "pypi".to_owned(),
        registry: "pypi.org".to_owned(),
        name: "greeting".to_owned(),
        version: "1.0.0".to_owned(),
    };
    let roots = CalleeRoots::from_packages(vec![PackageRoot::new(installed.clone(), package)]);
    Ok((installed, roots))
}

#[test]
fn engine_targets_at_statically_named_sites_take_the_static_name_of_their_owner() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    std::fs::create_dir(root.join("installed"))?;
    std::fs::write(root.join("installed/core.py"), "class Foreign: pass\n")?;
    std::fs::write(
        root.join("service.py"),
        "from _typeshed import SupportsRead\n\n\ndef serve(port: int) -> SupportsRead[str]:\n    raise NotImplementedError\n",
    )?;
    let reads = fixture(&root)?;
    let path = ProjectPath::new("service.py")?;
    let file = reads.index().file(&path).ok_or("file")?;
    let seeds = reads
        .local_search_view_input(true)?
        .inventory_seeds()?
        .0
        .into_iter()
        .collect();
    let found = sites(&reads, file, &seeds, selection())?;
    let site = |name: &str| {
        found
            .iter()
            .find(|site| site.name == name && site.caller.as_str().ends_with("/serve"))
            .cloned()
            .ok_or(format!("site {name}"))
    };
    let stub = |path: &str| -> Result<Location, Box<dyn std::error::Error>> {
        Ok(Location {
            uri: format!("vendored://stdlib/{path}").parse()?,
            range: lsp_types::Range::new(Position::new(3, 6), Position::new(3, 9)),
        })
    };
    let (installed, roots) = installed_greeting(&root)?;
    let foreign = Location {
        uri: installed.document_uri(&ProjectPath::new("core.py")?)?,
        range: lsp_types::Range::new(Position::new(0, 6), Position::new(0, 13)),
    };
    let mapping = Mapping {
        reads: &reads,
        roots: &roots,
        workspace: &root,
        encoding: PositionEncoding::Utf16,
        provider: EdgeProvider::Engine,
    };
    let (int_site, read_site) = (site("int")?, site("SupportsRead")?);
    let (int_name, read_name) = (int_site.source.range.start, read_site.source.range.start);
    let resolved = mapping.resolve(
        Vec::new(),
        Vec::new(),
        vec![
            Answer {
                site: int_site,
                locations: vec![stub("builtins.pyi")?, foreign],
            },
            Answer {
                site: read_site,
                locations: vec![stub("_typeshed/__init__.pyi")?],
            },
        ],
    )?;
    let int = "rift://symbol/stdlib/cpython/python/builtins/int";
    let mut named = resolved
        .packages
        .iter()
        .map(|edge| {
            (
                edge.source.as_ref().map(|source| source.range.start),
                edge.target.path().as_str(),
                edge.kind,
                edge.static_target.as_ref().map(SymbolId::as_str),
            )
        })
        .collect::<Vec<_>>();
    named.sort();
    // Another owner's declaration at the same site keeps no static name.
    let mut expected = [
        (read_name, "_typeshed/__init__.pyi", None),
        (int_name, "builtins.pyi", Some(int)),
        (int_name, "core.py", None),
    ]
    .into_iter()
    .flat_map(|(start, path, target)| {
        [EdgeKind::HasType, EdgeKind::References].map(|kind| (Some(start), path, kind, target))
    })
    .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(
        named, expected,
        "only targets inside the owner the static pass named take its name"
    );
    Ok(())
}
