//! Sans-I/O aggregation for the `rift://map` resource.
//!
//! [`build_workspace_map`] turns one already-loaded [`WorkspaceIndex`] into a
//! [`WorkspaceMap`], with no filesystem or network access of its own: every fact it reports
//! is already resident in the index and its normalized graph.

use std::collections::BTreeMap;

use rift_core::ProjectPath as CoreProjectPath;
use rift_core::SourceUnitId as CoreSourceUnitId;
use rift_core::SymbolId as CoreSymbolId;
use rift_core::{Contribution, ProviderId, SymbolRecord};
use rift_dependency::DependencyCatalog;
use rift_index::WorkspaceIndex;
use rift_protocol::map::{
    MAP_DOCS_MAX, MAP_ENTRY_POINTS_MAX, MAP_HUBS_MAX, MAP_MODULE_DEPTH_MAX,
    MAP_MODULE_RELATIONSHIPS_MAX, MAP_PACKAGES_MAX, MapHub, MapLanguage, MapModule,
    MapModuleRelationship, WorkspaceMap,
};
use rift_protocol::read::{Digest, Language, Pagination, ProjectPath, SymbolFacet, SymbolId};
use rift_protocol::workspace::{WORKSPACE_LANGUAGE_SUMMARIES_MAX, WORKSPACE_SOURCE_UNITS_MAX};
use rift_provider::{NormalizedGraph, NormalizedTarget, SymbolAssembler};
use rift_syntax::ShippedLanguage;

use crate::read::project_path;

/// Files and symbols credited to one directory or language while the index is walked.
#[derive(Clone, Copy, Default)]
struct FileSymbolCounts {
    files: u64,
    symbols: u64,
}

/// Builds the workspace orientation snapshot from one already-loaded index and catalog.
///
/// Runs once over `index.files()`, once over `index.text_files()`, once over
/// `graph.records()`, once over `graph.references()`, and once over the catalog's entries -
/// each already bounded by the workspace's configured index and binding limits and the
/// resolvers' package bound, so this stays proportional to what this revision already built.
pub(crate) fn build_workspace_map(
    index: &WorkspaceIndex,
    catalog: &DependencyCatalog,
    revision: Digest,
) -> WorkspaceMap {
    let graph = index.normalized_graph();
    let unit_paths = source_unit_paths(index);
    let records_by_identity = records_by_identity(graph);
    let mut language_counts: BTreeMap<String, (Language, FileSymbolCounts)> = BTreeMap::new();
    let mut directory_counts: BTreeMap<ProjectPath, FileSymbolCounts> = BTreeMap::new();
    let mut file_languages: BTreeMap<CoreProjectPath, Language> = BTreeMap::new();

    for file in index.files() {
        let language = file.syntax().language().clone();
        file_languages.insert(file.path().clone(), language.clone());
        language_counts
            .entry(language.identity_segment())
            .or_insert_with(|| (language, FileSymbolCounts::default()))
            .1
            .files += 1;
    }
    // `text_files` is the complete baseline content catalog: every syntax-parsed source
    // (`WorkspaceIndex::files`) is inserted into it too, alongside the plain-text files no
    // syntax provider claims. Crediting directories from `files` as well would double-count
    // every parsed source, so this is the one walk that owns directory file counts.
    for file in index.text_files() {
        credit_directories(&mut directory_counts, file.path(), |counts| {
            counts.files += 1;
        });
    }

    for record in graph.records() {
        let Some(path) = record_home_path(graph, record, &unit_paths) else {
            continue;
        };
        credit_directories(&mut directory_counts, path, |counts| {
            counts.symbols += 1;
        });
        if let Some(language) = file_languages.get(path) {
            language_counts
                .entry(language.identity_segment())
                .or_insert_with(|| (language.clone(), FileSymbolCounts::default()))
                .1
                .symbols += 1;
        }
    }

    let mut languages: Vec<MapLanguage> = language_counts
        .into_values()
        .map(|(language, counts)| MapLanguage {
            language,
            files: counts.files,
            symbols: counts.symbols,
        })
        .collect();
    languages.truncate(WORKSPACE_LANGUAGE_SUMMARIES_MAX);

    let packages = catalog
        .direct_packages()
        .take(MAP_PACKAGES_MAX)
        .cloned()
        .collect();
    WorkspaceMap {
        revision,
        languages,
        modules: module_tree(&directory_counts),
        hubs: hubs(graph, &records_by_identity),
        entry_points: entry_points(graph),
        docs: docs(index),
        module_relationships: module_relationships(graph, &unit_paths, &records_by_identity),
        packages,
        pagination: Pagination {
            page_index: 0,
            total_pages: 1,
        },
    }
}

/// Maps every indexed file's minted source-unit identity back to its project path, so a
/// [`SymbolRecord`]'s contribution - which names only the unit - can be attributed to a
/// directory. Only [`WorkspaceIndex::files`] mint a unit: a baseline text file carries no
/// syntax document, so it declares no symbols and needs no entry here.
fn source_unit_paths(index: &WorkspaceIndex) -> BTreeMap<CoreSourceUnitId, CoreProjectPath> {
    index
        .files()
        .filter_map(|file| {
            rift_syntax::source_unit(file.syntax())
                .ok()
                .map(|unit| (unit, file.path().clone()))
        })
        .collect()
}

/// The project path a normalized record's declaration belongs to, resolved through its first
/// contribution that carries a source binding. `None` for a record with no project-located
/// contribution: a dependency or standard-library declaration, or one this revision's syntax
/// pass never bound to a unit this workspace indexed.
fn record_home_path<'a>(
    graph: &NormalizedGraph,
    record: &SymbolRecord,
    unit_paths: &'a BTreeMap<CoreSourceUnitId, CoreProjectPath>,
) -> Option<&'a CoreProjectPath> {
    record
        .contributions()
        .iter()
        .filter_map(|key| graph.contribution(key))
        .find_map(Contribution::source)
        .and_then(|binding| unit_paths.get(binding.unit()))
}

/// Every graph record that established an identity, keyed by it. Both rankings need this
/// index, so it is built once and lent to each.
fn records_by_identity(graph: &NormalizedGraph) -> BTreeMap<&CoreSymbolId, &SymbolRecord> {
    graph
        .records()
        .iter()
        .filter_map(|record| record.identity().map(|identity| (identity, record)))
        .collect()
}

/// The module one project path belongs to: the deepest ancestor directory the module tree
/// lists, which is the last one [`credit_directories`] credits. `None` for a file directly at
/// the workspace root, which credits no directory and so belongs to no listed module.
fn owning_module(path: &CoreProjectPath) -> Option<ProjectPath> {
    let mut segments: Vec<&str> = path.as_str().split('/').collect();
    segments.pop();
    let depth = segments.len().min(MAP_MODULE_DEPTH_MAX);
    (depth > 0).then(|| ProjectPath(segments[..depth].join("/")))
}

/// The modules this revision resolved a reference between, ranked by that count descending
/// with the two paths breaking ties, capped at [`MAP_MODULE_RELATIONSHIPS_MAX`].
///
/// Every entry is a resolved reference: no heuristic and no historical correlation
/// contributes one. A reference whose source or target is not project-located contributes
/// nothing, since a dependency or standard-library declaration is already reported by
/// `packages`. A pair whose two sides fold to one module is dropped, because a module
/// referencing itself says nothing about structure. The tally rides the pass over
/// `graph.references()` the ranking already makes, so it adds no walk.
fn module_relationships(
    graph: &NormalizedGraph,
    unit_paths: &BTreeMap<CoreSourceUnitId, CoreProjectPath>,
    records_by_identity: &BTreeMap<&CoreSymbolId, &SymbolRecord>,
) -> Vec<MapModuleRelationship> {
    let mut counts: BTreeMap<(ProjectPath, ProjectPath), u64> = BTreeMap::new();
    for reference in graph.references() {
        let Some(from) = unit_paths
            .get(reference.binding().unit())
            .and_then(owning_module)
        else {
            continue;
        };
        for target in reference.targets() {
            let NormalizedTarget::Symbol(identity) = target else {
                continue;
            };
            let Some(to) = records_by_identity
                .get(identity)
                .and_then(|record| record_home_path(graph, record, unit_paths))
                .and_then(owning_module)
            else {
                continue;
            };
            if from == to {
                continue;
            }
            *counts.entry((from.clone(), to)).or_insert(0) += 1;
        }
    }
    let mut ranked: Vec<((ProjectPath, ProjectPath), u64)> = counts.into_iter().collect();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    ranked.truncate(MAP_MODULE_RELATIONSHIPS_MAX);
    ranked
        .into_iter()
        .map(|((from, to), references)| MapModuleRelationship {
            from,
            to,
            references,
        })
        .collect()
}

/// Credits `path`'s ancestor directories, up to [`MAP_MODULE_DEPTH_MAX`] levels deep, applying
/// `credit` to each ancestor's accumulated counts. A file directly at the workspace root - no
/// `/` in its path - credits no directory, matching [`WorkspaceMap::modules`]'s scope.
fn credit_directories(
    directory_counts: &mut BTreeMap<ProjectPath, FileSymbolCounts>,
    path: &CoreProjectPath,
    credit: impl Fn(&mut FileSymbolCounts),
) {
    let mut segments: Vec<&str> = path.as_str().split('/').collect();
    segments.pop();
    let depth = segments.len().min(MAP_MODULE_DEPTH_MAX);
    for level in 1..=depth {
        let key = ProjectPath(segments[..level].join("/"));
        credit(directory_counts.entry(key).or_default());
    }
}

/// Assembles the flat per-directory counts into the nested [`MapModule`] tree, in path order,
/// capped at [`WORKSPACE_SOURCE_UNITS_MAX`] entries per level - the same cap the schema
/// declares on `modules` and on [`MapModule::children`].
fn module_tree(directory_counts: &BTreeMap<ProjectPath, FileSymbolCounts>) -> Vec<MapModule> {
    let mut children_of: BTreeMap<ProjectPath, Vec<ProjectPath>> = BTreeMap::new();
    let mut roots: Vec<ProjectPath> = Vec::new();
    for path in directory_counts.keys() {
        let segments: Vec<&str> = path.0.split('/').collect();
        if segments.len() == 1 {
            roots.push(path.clone());
        } else {
            let parent = ProjectPath(segments[..segments.len() - 1].join("/"));
            children_of.entry(parent).or_default().push(path.clone());
        }
    }
    let mut modules: Vec<MapModule> = roots
        .into_iter()
        .map(|root| module_node(root, directory_counts, &children_of, 1))
        .collect();
    modules.truncate(WORKSPACE_SOURCE_UNITS_MAX);
    modules
}

/// Builds one [`MapModule`] and its children, recursing at most [`MAP_MODULE_DEPTH_MAX`]
/// levels: `credit_directories` never inserts a directory deeper than that bound, so
/// `children_of` never names a path recursion has not already reached by that depth.
fn module_node(
    path: ProjectPath,
    counts: &BTreeMap<ProjectPath, FileSymbolCounts>,
    children_of: &BTreeMap<ProjectPath, Vec<ProjectPath>>,
    depth: usize,
) -> MapModule {
    assert!(
        depth <= MAP_MODULE_DEPTH_MAX,
        "module tree exceeds its depth bound: depth={depth}, path={path:?}"
    );
    let node_counts = counts.get(&path).copied().unwrap_or_default();
    let mut children: Vec<MapModule> = children_of
        .get(&path)
        .into_iter()
        .flatten()
        .cloned()
        .map(|child| module_node(child, counts, children_of, depth + 1))
        .collect();
    children.truncate(WORKSPACE_SOURCE_UNITS_MAX);
    MapModule {
        path,
        files: node_counts.files,
        symbols: node_counts.symbols,
        children,
    }
}

/// Symbols carrying the [`SymbolFacet::Entrypoint`] facet, in identity order, capped at
/// [`MAP_ENTRY_POINTS_MAX`]. The identity string files a symbol under its language then its
/// declaring path, so sorting it orders entries by path within each language.
fn entry_points(graph: &NormalizedGraph) -> Vec<SymbolId> {
    let mut ids: Vec<SymbolId> = graph
        .records()
        .iter()
        .filter_map(|record| {
            let identity = record.identity()?;
            let is_entrypoint = record
                .contributions()
                .iter()
                .filter_map(|key| graph.contribution(key))
                .filter_map(Contribution::facts)
                .any(|facts| facts.symbol_facets().contains(&SymbolFacet::Entrypoint));
            is_entrypoint.then(|| SymbolId(identity.as_str().to_owned()))
        })
        .collect();
    ids.sort();
    ids.truncate(MAP_ENTRY_POINTS_MAX);
    ids
}

/// The most-referenced symbols, ranked by reference count descending with identity breaking
/// ties, capped at [`MAP_HUBS_MAX`]. `records_by_identity` is the caller's one index over the
/// graph's established identities, shared with [`module_relationships`]. A candidate whose
/// record cannot be assembled - no established identity, or no provider ever contributed
/// portable facts for it - is skipped
/// rather than reported with a guessed kind; ranking continues past it toward the next
/// candidate, bounded by the same reference graph the tally already walked once.
fn hubs(
    graph: &NormalizedGraph,
    records_by_identity: &BTreeMap<&CoreSymbolId, &SymbolRecord>,
) -> Vec<MapHub> {
    let mut reference_counts: BTreeMap<CoreSymbolId, u64> = BTreeMap::new();
    for reference in graph.references() {
        for target in reference.targets() {
            if let NormalizedTarget::Symbol(identity) = target {
                *reference_counts.entry(identity.clone()).or_insert(0) += 1;
            }
        }
    }
    let mut ranked: Vec<(CoreSymbolId, u64)> = reference_counts.into_iter().collect();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));

    let precedence = [syntax_provider_id()];

    let mut hubs = Vec::new();
    for (identity, references) in ranked {
        if hubs.len() >= MAP_HUBS_MAX {
            break;
        }
        let Some(kind) = records_by_identity
            .get(&identity)
            .and_then(|record| SymbolAssembler::assemble(graph, record, &precedence))
            .and_then(|assembled| assembled.facts().map(|facts| facts.kind().clone()))
        else {
            continue;
        };
        hubs.push(MapHub {
            symbol: SymbolId(identity.as_str().to_owned()),
            kind,
            references,
        });
    }
    hubs
}

/// The syntax provider's identity, the same precedence [`WorkspaceIndex::assembled_symbol`]
/// selects presentation facts with. `SYNTAX_PROVIDER_ID` is a fixed, valid provider identity,
/// so construction cannot fail.
fn syntax_provider_id() -> ProviderId {
    ProviderId::new(rift_syntax::SYNTAX_PROVIDER_ID).unwrap_or_else(|error| {
        unreachable!("SYNTAX_PROVIDER_ID is a compile-time-valid provider identity: {error}")
    })
}

/// Markdown-language files, in path order, capped at [`MAP_DOCS_MAX`].
fn docs(index: &WorkspaceIndex) -> Vec<ProjectPath> {
    let markdown = ShippedLanguage::Markdown.language();
    let mut paths: Vec<ProjectPath> = index
        .files()
        .filter(|file| file.syntax().language() == &markdown)
        .map(|file| project_path(file.path()))
        .collect();
    paths.sort();
    paths.truncate(MAP_DOCS_MAX);
    paths
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fmt::Write as _;
    use std::fs;

    use std::collections::BTreeMap;

    use rift_core::SourceVisibility;
    use rift_index::WorkspaceIndexLimits;
    use rift_protocol::configuration::HistoryConfiguration;
    use rift_protocol::map::{MapModule, WorkspaceMap};
    use rift_protocol::read::ProjectPath;
    use tempfile::TempDir;

    use crate::read::ReadService;

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    /// One function calling another, twice over, so two callees tie at one reference each -
    /// the hub tie-break needs a genuine tie to prove it breaks by identity. A file-scope
    /// `main` proves the entry-point listing; a symbol nested four directories deep proves
    /// depth folding; `README.md` at the root and `docs/guide.md` prove the docs filter and
    /// that a root-level file earns no module entry.
    fn fixture() -> TestResult<(TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        fs::create_dir_all(directory.path().join("src"))?;
        fs::write(
            directory.path().join("src/lib.rs"),
            "pub fn alpha() {}\npub fn beta() {}\n\
             pub fn use_alpha() { alpha(); }\npub fn use_beta() { beta(); }\n\
             fn main() { use_alpha(); }\n",
        )?;
        fs::create_dir_all(directory.path().join("src/nested/deep/more"))?;
        fs::write(
            directory.path().join("src/nested/deep/more/leaf.rs"),
            "pub fn leaf_fn() {}\n",
        )?;
        fs::write(directory.path().join("README.md"), "# Title\n")?;
        fs::create_dir_all(directory.path().join("docs"))?;
        fs::write(directory.path().join("docs/guide.md"), "# Guide\n")?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        Ok((directory, service))
    }

    #[test]
    fn module_tree_folds_deep_directories_and_keeps_counts_inclusive() -> TestResult {
        let (_directory, service) = fixture()?;
        let map = service.workspace_map();

        let src = map
            .modules
            .iter()
            .find(|module| module.path.0 == "src")
            .expect("src is a top-level module");
        assert_eq!(src.files, 2, "lib.rs and the folded leaf.rs");
        assert_eq!(src.symbols, 6, "5 functions in lib.rs plus leaf_fn");

        let nested = src
            .children
            .iter()
            .find(|module| module.path.0 == "src/nested")
            .expect("src/nested is src's child");
        let deep = nested
            .children
            .iter()
            .find(|module| module.path.0 == "src/nested/deep")
            .expect("src/nested/deep is nested's child, folding away `more`");
        assert_eq!(deep.files, 1);
        assert_eq!(deep.symbols, 1);
        assert!(
            deep.children.is_empty(),
            "more/leaf.rs folds into its depth-3 ancestor instead of a fourth level"
        );
        assert!(
            map.modules
                .iter()
                .all(|module| module.path.0 != "README.md"),
            "a root-level file earns no module entry"
        );
        Ok(())
    }

    #[test]
    fn hubs_rank_by_reference_count_then_break_ties_by_symbol_identity() -> TestResult {
        let (_directory, service) = fixture()?;
        let map = service.workspace_map();

        let ids: Vec<&str> = map.hubs.iter().map(|hub| hub.symbol.0.as_str()).collect();
        assert_eq!(
            ids,
            [
                "rift://symbol/rust/src/lib.rs/alpha",
                "rift://symbol/rust/src/lib.rs/beta",
                "rift://symbol/rust/src/lib.rs/use_alpha",
            ],
            "alpha and beta tie at one reference each and sort by identity; \
             use_alpha is referenced once from main"
        );
        assert!(map.hubs.iter().all(|hub| hub.references == 1));
        assert!(map.hubs.iter().all(|hub| hub.kind.0 == "function"));
        Ok(())
    }

    #[test]
    fn hubs_stop_at_the_bound_and_skip_candidates_with_no_portable_facts() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::create_dir_all(directory.path().join("src"))?;
        let mut source = String::new();
        for index in 0..25 {
            writeln!(source, "pub fn callee_{index:02}() {{}}")?;
        }
        source
            .push_str("pub fn caller() {\n    let local = 1;\n    let doubled = local + local;\n");
        for index in 0..25 {
            writeln!(source, "    callee_{index:02}();")?;
        }
        source.push_str("    assert!(doubled > 0);\n}\n");
        fs::write(directory.path().join("src/lib.rs"), source)?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let map = service.workspace_map();

        assert_eq!(
            map.hubs.len(),
            rift_protocol::map::MAP_HUBS_MAX,
            "twenty-five referenced callees and a twice-referenced local exceed the bound"
        );
        assert!(
            map.hubs
                .iter()
                .all(|hub| !hub.symbol.0.ends_with("/local") && !hub.symbol.0.ends_with("/doubled")),
            "a local binding carries no portable facts and never ranks as a hub: hubs={:?}",
            map.hubs
        );
        assert_eq!(
            map.hubs[0].symbol.0, "rift://symbol/rust/src/lib.rs/callee_00",
            "the twice-referenced local would rank first were it assemblable; the ranked \
             callees follow in identity order"
        );
        Ok(())
    }

    #[test]
    fn entry_points_lists_the_file_scope_main() -> TestResult {
        let (_directory, service) = fixture()?;
        let map = service.workspace_map();
        assert_eq!(
            map.entry_points
                .iter()
                .map(|id| id.0.as_str())
                .collect::<Vec<_>>(),
            ["rift://symbol/rust/src/lib.rs/main"]
        );
        Ok(())
    }

    #[test]
    fn docs_lists_markdown_files_in_path_order() -> TestResult {
        let (_directory, service) = fixture()?;
        let map = service.workspace_map();
        assert_eq!(
            map.docs
                .iter()
                .map(|path| path.0.as_str())
                .collect::<Vec<_>>(),
            ["README.md", "docs/guide.md"]
        );
        Ok(())
    }

    #[test]
    fn empty_workspace_produces_an_all_empty_map_that_omits_every_collection() -> TestResult {
        let directory = tempfile::tempdir()?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let map = service.workspace_map();
        assert!(map.languages.is_empty());
        assert!(map.modules.is_empty());
        assert!(map.hubs.is_empty());
        assert!(map.entry_points.is_empty());
        assert!(map.docs.is_empty());
        assert!(map.module_relationships.is_empty());

        let value = serde_json::to_value(&map)?;
        for field in [
            "languages",
            "modules",
            "hubs",
            "entry_points",
            "docs",
            "module_relationships",
        ] {
            assert!(value.get(field).is_none(), "field={field}");
        }
        Ok(())
    }

    #[test]
    fn workspace_map_is_deterministic_across_two_builds_of_the_same_tree() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::create_dir_all(directory.path().join("src"))?;
        fs::write(
            directory.path().join("src/lib.rs"),
            "pub fn alpha() {}\npub fn beta() { alpha(); }\nfn main() {}\n",
        )?;
        let limits = WorkspaceIndexLimits::default();
        let visibility = SourceVisibility::default();
        let inclusion = rift_core::TextFileInclusion::default();
        let first = ReadService::build(
            directory.path(),
            limits,
            &visibility,
            &inclusion,
            HistoryConfiguration::default(),
        )?
        .workspace_map();
        let second = ReadService::build(
            directory.path(),
            limits,
            &visibility,
            &inclusion,
            HistoryConfiguration::default(),
        )?
        .workspace_map();

        assert_eq!(
            serde_json::to_string(&first)?,
            serde_json::to_string(&second)?
        );
        Ok(())
    }

    /// Builds one normalized graph by hand: `holder` declares portable facts and references
    /// `ghost`, whose only contribution carries an identity anchor and no facts. The graph
    /// then resolves `ghost` as a reference target the hub ranking must skip.
    fn fact_less_target_graph() -> TestResult<rift_provider::NormalizedGraph> {
        use std::sync::Arc;

        use rift_core::{
            Contribution, ContributionKey, ContributionOrigin, ContributionReference,
            DeclarationBinding, ExactKind, IndexRevision, Language, PortableSymbolFacts,
            ProviderId, ProviderRevision, ProviderSymbolId, ReferenceRole, SemanticReference,
            SourceApplicability, SourceKind, SourceLocation, SourcePath, SourceRange,
            SourceResolverId, SourceRevision, SourceUnitId, SymbolId, TreeRevision,
        };
        use rift_provider::{Normalizer, ProviderPublication, PublicationLimits, PublicationSet};

        let provider = ProviderId::new("syntax")?;
        let key = |symbol: &str| -> Result<ContributionKey, Box<dyn Error>> {
            Ok(ContributionKey::new(
                provider.clone(),
                ProviderRevision::new(1)?,
                ProviderSymbolId::new(symbol)?,
            ))
        };
        let unit = SourceUnitId::new(
            SourceResolverId::new("project")?,
            SourcePath::new("src/lib.rs")?,
        )?;
        let applicability = SourceApplicability::Exact {
            source_revision: SourceRevision::new(1)?,
            tree_revision: TreeRevision::new(1)?,
        };
        let origin = ContributionOrigin::new(
            Some(SourceLocation::Project { package: None }),
            SourceKind::Authored,
        )?;
        let holder_identity = "rift://symbol/rust/src/lib.rs/holder";
        let ghost_identity = SymbolId::new("rift://symbol/rust/src/lib.rs/ghost")?;
        let holder = Contribution::builder(
            key("holder")?,
            applicability,
            PortableSymbolFacts::new(
                Language {
                    name: "rust".to_owned(),
                    dialect: None,
                },
                holder_identity,
                holder_identity,
                ExactKind("rust.function".to_owned()),
            ),
            origin.clone(),
        )
        .source(DeclarationBinding::new(
            unit.clone(),
            SourceRange::new(0, 40)?,
            None,
        ))
        .identity_anchor(SymbolId::new(holder_identity)?)
        .build()?;
        let ghost = Contribution::fact_builder(key("ghost")?, applicability, origin.clone())
            .source(DeclarationBinding::new(
                unit.clone(),
                SourceRange::new(50, 60)?,
                None,
            ))
            .identity_anchor(ghost_identity.clone())
            .build()?;
        let reference = Contribution::fact_builder(key("holder_ref_ghost")?, applicability, origin)
            .references(vec![SemanticReference::new(
                DeclarationBinding::new(unit, SourceRange::new(10, 15)?, None),
                ReferenceRole::Call,
                vec![ContributionReference::new(
                    provider.clone(),
                    ProviderSymbolId::new("ghost")?,
                )],
            )?])
            .build()?;
        let publication = ProviderPublication::new(
            provider,
            ProviderRevision::new(1)?,
            vec![holder, ghost, reference],
            PublicationLimits::default(),
        )?;
        let publications =
            Arc::new(PublicationSet::empty(PublicationLimits::default()).replaced(publication)?);
        let graph = Normalizer::normalize(
            IndexRevision::new(1)?,
            SourceRevision::new(1)?,
            TreeRevision::new(1)?,
            &publications,
            None,
        )?;
        Ok(graph)
    }

    #[test]
    fn hubs_skip_a_resolved_target_whose_record_carries_no_portable_facts() -> TestResult {
        use rift_provider::NormalizedTarget;

        let graph = fact_less_target_graph()?;
        let ghost_identity = rift_core::SymbolId::new("rift://symbol/rust/src/lib.rs/ghost")?;
        let ghost_resolved = graph.references().iter().any(|reference| {
            reference.targets().iter().any(
                |target| matches!(target, NormalizedTarget::Symbol(id) if id == &ghost_identity),
            )
        });
        assert!(
            ghost_resolved,
            "the ghost target must resolve to an established identity so the skip arm runs"
        );
        let hubs = super::hubs(&graph, &super::records_by_identity(&graph));
        assert!(
            hubs.iter().all(|hub| !hub.symbol.0.ends_with("/ghost")),
            "a record with no portable facts never ranks as a hub: hubs={hubs:?}"
        );
        Ok(())
    }

    /// One workspace of `files`, served, with its orientation snapshot.
    fn served_map(files: &[(&str, &str)]) -> TestResult<(tempfile::TempDir, WorkspaceMap)> {
        let directory = tempfile::tempdir()?;
        for (path, contents) in files {
            let path = directory.path().join(path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, contents)?;
        }
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let map = service.workspace_map();
        Ok((directory, map))
    }

    /// `src/lib.rs` calls a declaration `src/inner/mod.rs` defines, so one resolved reference
    /// crosses the two modules the tree lists.
    const CROSSING_FILES: &[(&str, &str)] = &[
        (
            "src/lib.rs",
            "mod inner;\npub fn beacon() {\n    inner::helper();\n}\n",
        ),
        ("src/inner/mod.rs", "pub fn helper() {}\n"),
    ];

    fn pairs(map: &WorkspaceMap) -> Vec<(&str, &str, u64)> {
        map.module_relationships
            .iter()
            .map(|relationship| {
                (
                    relationship.from.0.as_str(),
                    relationship.to.0.as_str(),
                    relationship.references,
                )
            })
            .collect()
    }

    #[test]
    fn owning_module_folds_a_path_to_its_deepest_listed_ancestor() {
        let path = |value: &str| rift_core::ProjectPath::new(value).expect("fixture path");
        assert_eq!(super::owning_module(&path("README.md")), None);
        assert_eq!(
            super::owning_module(&path("src/lib.rs")),
            Some(ProjectPath("src".to_owned()))
        );
        assert_eq!(
            super::owning_module(&path("a/b/c/d/e/deep.rs")),
            Some(ProjectPath("a/b/c".to_owned())),
            "a path deeper than MAP_MODULE_DEPTH_MAX folds into the deepest listed ancestor"
        );
    }

    #[test]
    fn a_resolved_reference_crossing_two_modules_becomes_one_relationship() -> TestResult {
        let (_directory, map) = served_map(CROSSING_FILES)?;
        assert_eq!(pairs(&map), [("src", "src/inner", 1)]);
        Ok(())
    }

    #[test]
    fn every_relationship_endpoint_names_a_listed_module() -> TestResult {
        let (_directory, map) = served_map(CROSSING_FILES)?;
        let mut listed: Vec<&str> = Vec::new();
        let mut pending: Vec<&MapModule> = map.modules.iter().collect();
        while let Some(module) = pending.pop() {
            listed.push(module.path.0.as_str());
            pending.extend(module.children.iter());
        }
        for relationship in &map.module_relationships {
            assert!(
                listed.contains(&relationship.from.0.as_str()),
                "from={} listed={listed:?}",
                relationship.from.0
            );
            assert!(
                listed.contains(&relationship.to.0.as_str()),
                "to={} listed={listed:?}",
                relationship.to.0
            );
        }
        Ok(())
    }

    #[test]
    fn several_references_over_one_pair_answer_as_one_relationship() -> TestResult {
        let (_directory, map) = served_map(&[
            (
                "src/lib.rs",
                "mod inner;\npub fn beacon() {\n    inner::helper();\n    inner::helper();\n}\n",
            ),
            ("src/inner/mod.rs", "pub fn helper() {}\n"),
        ])?;
        assert_eq!(pairs(&map), [("src", "src/inner", 2)]);
        Ok(())
    }

    #[test]
    fn a_reference_inside_one_module_is_no_relationship() -> TestResult {
        let (_directory, map) = served_map(&[(
            "src/lib.rs",
            "pub fn helper() {}\npub fn beacon() {\n    helper();\n}\n",
        )])?;
        assert!(
            map.module_relationships.is_empty(),
            "a module referencing itself says nothing about structure: {:?}",
            map.module_relationships
        );
        Ok(())
    }

    #[test]
    fn a_reference_from_a_root_file_contributes_no_endpoint() -> TestResult {
        let (_directory, map) = served_map(&[
            (
                "lib.rs",
                "mod inner;\npub fn beacon() {\n    inner::helper();\n}\n",
            ),
            ("inner/mod.rs", "pub fn helper() {}\n"),
        ])?;
        assert!(
            map.module_relationships.is_empty(),
            "a file directly at the workspace root belongs to no listed module: {:?}",
            map.module_relationships
        );
        Ok(())
    }

    #[test]
    fn module_relationships_are_stable_across_two_builds_of_one_tree() -> TestResult {
        let (_first_directory, first) = served_map(CROSSING_FILES)?;
        let (_second_directory, second) = served_map(CROSSING_FILES)?;
        assert_eq!(first.module_relationships, second.module_relationships);
        Ok(())
    }

    #[test]
    fn a_graph_with_no_resolved_reference_answers_with_no_relationship() -> TestResult {
        let graph = fact_less_target_graph()?;
        let unit_paths = BTreeMap::new();
        let records = super::records_by_identity(&graph);
        assert!(
            super::module_relationships(&graph, &unit_paths, &records).is_empty(),
            "a reference whose source unit maps to no project path contributes nothing"
        );
        Ok(())
    }
}
