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
    use std::collections::BTreeMap;
    use std::error::Error;
    use std::fs;
    use std::sync::Arc;

    use rift_core::{
        Contribution, ContributionKey, ContributionOrigin, ContributionReference,
        DeclarationBinding, ExactKind, IndexRevision, Language, PortableSymbolFacts, ProviderId,
        ProviderRevision, ProviderSymbolId, ReferenceRole, SemanticReference, SourceApplicability,
        SourceKind, SourceLocation, SourcePath, SourceRange, SourceResolverId, SourceRevision,
        SourceUnitId, SourceVisibility, SymbolId, TreeRevision,
    };
    use rift_index::WorkspaceIndexLimits;
    use rift_protocol::configuration::HistoryConfiguration;
    use rift_protocol::map::{MAP_HUBS_MAX, MapModuleRelationship, WorkspaceMap};
    use rift_protocol::read::ProjectPath;
    use rift_provider::{
        NormalizedGraph, NormalizedReference, NormalizedTarget, Normalizer, ProviderPublication,
        PublicationLimits, PublicationSet,
    };
    use tempfile::TempDir;

    use crate::read::ReadService;

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    /// One hand-built declaration: its provider symbol, the wire identity it anchors, the unit
    /// path it declares in, and its declaration range.
    type Definition<'a> = (&'a str, &'a str, &'a str, (u64, u64));

    /// One hand-built reference: its provider symbol, the unit path the occurrence sits in, the
    /// occurrence range, and the provider symbol it names.
    type Occurrence<'a> = (&'a str, &'a str, (u64, u64), &'a str);

    /// The exact kind every hand-built declaration publishes, which a hub carries through.
    const DECLARED_KIND: &str = "rust.function";

    /// The identity a fact-less declaration anchors, so a reference resolves to a record the
    /// hub ranking cannot assemble.
    const GHOST_IDENTITY: &str = "rift://symbol/rust/src/lib.rs/ghost";

    /// The two units a crossing reference joins: `src/lib.rs` declares `beacon`, and
    /// `src/inner/mod.rs` declares the `helper` it names, so the two fold to different modules.
    const CROSSING_UNITS: &[&str] = &["src/lib.rs", "src/inner/mod.rs"];

    /// One occurrence in `src/lib.rs` naming the declaration `src/inner/mod.rs` makes.
    const ONE_CROSSING_CALL: &[Occurrence<'_>] =
        &[("beacon_calls_helper", "src/lib.rs", (10, 20), "helper")];

    /// One function calling another, twice over. A file-scope `main` proves the entry-point
    /// listing; a symbol nested four directories deep proves depth folding; `README.md` at the
    /// root and `docs/guide.md` prove the docs filter and that a root-level file earns no module
    /// entry. No provider publishes a resolved reference, so the calls in this source reach the
    /// map as neither hub nor module relationship.
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

    /// The provider every hand-built contribution publishes under. The hub ranking selects
    /// presentation facts by this identity, so facts published under another one assemble
    /// nothing and rank nothing.
    fn syntax_provider() -> TestResult<ProviderId> {
        Ok(ProviderId::new(rift_syntax::SYNTAX_PROVIDER_ID)?)
    }

    /// One contribution key for `symbol` under the syntax provider's first revision.
    fn contribution_key(symbol: &str) -> TestResult<ContributionKey> {
        Ok(ContributionKey::new(
            syntax_provider()?,
            ProviderRevision::new(1)?,
            ProviderSymbolId::new(symbol)?,
        ))
    }

    /// The single revision pair every hand-built contribution applies to.
    fn applicability() -> TestResult<SourceApplicability> {
        Ok(SourceApplicability::Exact {
            source_revision: SourceRevision::new(1)?,
            tree_revision: TreeRevision::new(1)?,
        })
    }

    /// Authored project origin, which is what a workspace provider publishes.
    fn origin() -> TestResult<ContributionOrigin> {
        Ok(ContributionOrigin::new(
            Some(SourceLocation::Project { package: None }),
            SourceKind::Authored,
        )?)
    }

    /// The unit a project-resolved `path` mints, the same key `unit_paths` files it under.
    fn source_unit(path: &str) -> TestResult<SourceUnitId> {
        Ok(SourceUnitId::new(
            SourceResolverId::new("project")?,
            SourcePath::new(path)?,
        )?)
    }

    /// One binding over `range` of `path`. Normalization joins two contributions that share one
    /// source binding into a single record, so every declaration takes a range of its own.
    fn binding(path: &str, range: (u64, u64)) -> TestResult<DeclarationBinding> {
        Ok(DeclarationBinding::new(
            source_unit(path)?,
            SourceRange::new(range.0, range.1)?,
            None,
        ))
    }

    /// Declarations carrying portable facts, one contribution each.
    fn declared_contributions(definitions: &[Definition<'_>]) -> TestResult<Vec<Contribution>> {
        definitions
            .iter()
            .map(|(symbol, identity, path, range)| {
                let language = Language {
                    name: "rust".to_owned(),
                    dialect: None,
                };
                let kind = ExactKind(DECLARED_KIND.to_owned());
                let facts = PortableSymbolFacts::new(language, *identity, *identity, kind);
                let key = contribution_key(symbol)?;
                Ok(
                    Contribution::builder(key, applicability()?, facts, origin()?)
                        .source(binding(path, *range)?)
                        .identity_anchor(SymbolId::new(*identity)?)
                        .build()?,
                )
            })
            .collect()
    }

    /// Declarations that anchor an identity and publish no portable facts: a reference resolving
    /// to one names a record the hub ranking cannot assemble and must skip.
    fn fact_less_contributions(definitions: &[Definition<'_>]) -> TestResult<Vec<Contribution>> {
        definitions
            .iter()
            .map(|(symbol, identity, path, range)| {
                let key = contribution_key(symbol)?;
                Ok(Contribution::fact_builder(key, applicability()?, origin()?)
                    .source(binding(path, *range)?)
                    .identity_anchor(SymbolId::new(*identity)?)
                    .build()?)
            })
            .collect()
    }

    /// Reference contributions, one occurrence each. A reference declares nothing, so it takes
    /// no source binding of its own and joins no declaration's record.
    fn reference_contributions(references: &[Occurrence<'_>]) -> TestResult<Vec<Contribution>> {
        references
            .iter()
            .map(|(symbol, path, range, target)| {
                let provider_symbol = ProviderSymbolId::new(*target)?;
                let targets = vec![ContributionReference::new(
                    syntax_provider()?,
                    provider_symbol,
                )];
                let source = binding(path, *range)?;
                let occurrence = SemanticReference::new(source, ReferenceRole::Call, targets)?;
                let key = contribution_key(symbol)?;
                Ok(Contribution::fact_builder(key, applicability()?, origin()?)
                    .references(vec![occurrence])
                    .build()?)
            })
            .collect()
    }

    /// Normalizes one hand-built publication into the graph `build_workspace_map` reads. No
    /// shipped provider publishes a resolved reference, so a served workspace cannot supply the
    /// graph the hub ranking and the module-relationship fold act on.
    fn published_graph(
        definitions: &[Definition<'_>],
        fact_less: &[Definition<'_>],
        references: &[Occurrence<'_>],
    ) -> TestResult<NormalizedGraph> {
        let mut contributions = declared_contributions(definitions)?;
        contributions.extend(fact_less_contributions(fact_less)?);
        contributions.extend(reference_contributions(references)?);
        let publication = ProviderPublication::new(
            syntax_provider()?,
            ProviderRevision::new(1)?,
            contributions,
            PublicationLimits::default(),
        )?;
        let publications =
            Arc::new(PublicationSet::empty(PublicationLimits::default()).replaced(publication)?);
        Ok(Normalizer::normalize(
            IndexRevision::new(1)?,
            SourceRevision::new(1)?,
            TreeRevision::new(1)?,
            &publications,
            None,
        )?)
    }

    /// One graph whose every declaration carries portable facts.
    fn resolved_graph(
        definitions: &[Definition<'_>],
        references: &[Occurrence<'_>],
    ) -> TestResult<NormalizedGraph> {
        published_graph(definitions, &[], references)
    }

    /// `src/lib.rs` declares `beacon`, `src/inner/mod.rs` declares `helper`, and `calls` names
    /// `helper` from `src/lib.rs`, so every occurrence crosses the two modules.
    fn crossing_graph(calls: &[Occurrence<'_>]) -> TestResult<NormalizedGraph> {
        resolved_graph(
            &[
                (
                    "beacon",
                    "rift://symbol/rust/src/lib.rs/beacon",
                    "src/lib.rs",
                    (0, 30),
                ),
                (
                    "helper",
                    "rift://symbol/rust/src/inner/mod.rs/helper",
                    "src/inner/mod.rs",
                    (0, 20),
                ),
            ],
            calls,
        )
    }

    /// `holder` declares portable facts and references `ghost`, whose contribution anchors an
    /// identity and carries none. The graph resolves `ghost` as a reference target.
    fn fact_less_target_graph() -> TestResult<NormalizedGraph> {
        published_graph(
            &[(
                "holder",
                "rift://symbol/rust/src/lib.rs/holder",
                "src/lib.rs",
                (0, 40),
            )],
            &[("ghost", GHOST_IDENTITY, "src/lib.rs", (50, 60))],
            &[("holder_ref_ghost", "src/lib.rs", (10, 15), "ghost")],
        )
    }

    /// The unit-to-path map `module_relationships` folds a reference through, for the units a
    /// hand-built graph declares in.
    fn unit_paths(paths: &[&str]) -> TestResult<BTreeMap<SourceUnitId, rift_core::ProjectPath>> {
        paths
            .iter()
            .map(|path| Ok((source_unit(path)?, rift_core::ProjectPath::new(*path)?)))
            .collect()
    }

    /// Every identity a graph's references resolve to. A test expecting no relationship checks
    /// this first: an empty answer over a graph that resolves nothing proves nothing.
    fn resolved_targets(graph: &NormalizedGraph) -> Vec<SymbolId> {
        graph
            .references()
            .iter()
            .flat_map(NormalizedReference::targets)
            .filter_map(|target| match target {
                NormalizedTarget::Symbol(identity) => Some(identity.clone()),
                NormalizedTarget::Contribution(_) => None,
            })
            .collect()
    }

    /// The module paths a workspace of `paths` lists, flattened, built by the same fold
    /// `build_workspace_map` credits its directories with.
    fn listed_modules(paths: &[&str]) -> TestResult<Vec<String>> {
        let mut counts = BTreeMap::new();
        for path in paths {
            let path = rift_core::ProjectPath::new(*path)?;
            super::credit_directories(&mut counts, &path, |credited| credited.files += 1);
        }
        let mut listed = Vec::new();
        let mut pending = super::module_tree(&counts);
        while let Some(module) = pending.pop() {
            listed.push(module.path.0);
            pending.extend(module.children);
        }
        Ok(listed)
    }

    /// The module pairs one relationship list names, with each pair's reference count.
    fn pairs(relationships: &[MapModuleRelationship]) -> Vec<(&str, &str, u64)> {
        relationships
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

    /// `src/lib.rs` calls a declaration `src/inner/mod.rs` defines, one module apart.
    const CROSSING_FILES: &[(&str, &str)] = &[
        (
            "src/lib.rs",
            "mod inner;\npub fn beacon() {\n    inner::helper();\n}\n",
        ),
        ("src/inner/mod.rs", "pub fn helper() {}\n"),
    ];

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
        let graph = resolved_graph(
            &[
                (
                    "alpha",
                    "rift://symbol/rust/src/lib.rs/alpha",
                    "src/lib.rs",
                    (0, 10),
                ),
                (
                    "beta",
                    "rift://symbol/rust/src/lib.rs/beta",
                    "src/lib.rs",
                    (10, 20),
                ),
                (
                    "use_alpha",
                    "rift://symbol/rust/src/lib.rs/use_alpha",
                    "src/lib.rs",
                    (20, 40),
                ),
            ],
            &[
                ("call_alpha", "src/lib.rs", (100, 105), "alpha"),
                ("call_beta", "src/lib.rs", (105, 110), "beta"),
                (
                    "first_call_use_alpha",
                    "src/lib.rs",
                    (110, 120),
                    "use_alpha",
                ),
                (
                    "second_call_use_alpha",
                    "src/lib.rs",
                    (120, 130),
                    "use_alpha",
                ),
            ],
        )?;
        let hubs = super::hubs(&graph, &super::records_by_identity(&graph));

        let ranked: Vec<(&str, u64)> = hubs
            .iter()
            .map(|hub| (hub.symbol.0.as_str(), hub.references))
            .collect();
        assert_eq!(
            ranked,
            [
                ("rift://symbol/rust/src/lib.rs/use_alpha", 2),
                ("rift://symbol/rust/src/lib.rs/alpha", 1),
                ("rift://symbol/rust/src/lib.rs/beta", 1),
            ],
            "twice-referenced use_alpha ranks first; alpha and beta tie at one reference each \
             and sort by identity"
        );
        assert!(hubs.iter().all(|hub| hub.kind.0 == DECLARED_KIND));
        Ok(())
    }

    #[test]
    fn hubs_stop_at_the_bound_and_skip_candidates_with_no_portable_facts() -> TestResult {
        let callee_count = u64::try_from(MAP_HUBS_MAX)? + 2;
        let callees: Vec<(String, String, String, (u64, u64))> = (0..callee_count)
            .map(|index| {
                (
                    format!("callee_{index:02}"),
                    format!("rift://symbol/rust/src/lib.rs/callee_{index:02}"),
                    format!("call_callee_{index:02}"),
                    (index * 10, index * 10 + 10),
                )
            })
            .collect();
        let definitions: Vec<Definition<'_>> = callees
            .iter()
            .map(|(symbol, identity, _, range)| {
                (symbol.as_str(), identity.as_str(), "src/lib.rs", *range)
            })
            .collect();
        let mut references: Vec<Occurrence<'_>> = callees
            .iter()
            .map(|(symbol, _, occurrence, range)| {
                let occurrence_range = (range.0 + 1_000, range.1 + 1_000);
                (
                    occurrence.as_str(),
                    "src/lib.rs",
                    occurrence_range,
                    symbol.as_str(),
                )
            })
            .collect();
        references.push(("first_call_ghost", "src/lib.rs", (10_000, 10_010), "ghost"));
        references.push(("second_call_ghost", "src/lib.rs", (10_010, 10_020), "ghost"));
        let ghost: &[Definition<'_>] = &[("ghost", GHOST_IDENTITY, "src/lib.rs", (20_000, 20_010))];
        let graph = published_graph(&definitions, ghost, &references)?;
        let hubs = super::hubs(&graph, &super::records_by_identity(&graph));

        assert_eq!(
            hubs.len(),
            MAP_HUBS_MAX,
            "{callee_count} referenced callees and a twice-referenced ghost exceed the bound"
        );
        assert!(
            hubs.iter().all(|hub| !hub.symbol.0.ends_with("/ghost")),
            "a record with no portable facts never ranks as a hub: hubs={hubs:?}"
        );
        assert_eq!(
            hubs[0].symbol.0, "rift://symbol/rust/src/lib.rs/callee_00",
            "the twice-referenced ghost would rank first were it assemblable; the ranked \
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

    #[test]
    fn hubs_skip_a_resolved_target_whose_record_carries_no_portable_facts() -> TestResult {
        let graph = fact_less_target_graph()?;
        let ghost = SymbolId::new(GHOST_IDENTITY)?;
        assert!(
            resolved_targets(&graph).contains(&ghost),
            "the ghost target must resolve to an established identity so the skip arm runs"
        );
        let hubs = super::hubs(&graph, &super::records_by_identity(&graph));
        assert!(
            hubs.iter().all(|hub| !hub.symbol.0.ends_with("/ghost")),
            "a record with no portable facts never ranks as a hub: hubs={hubs:?}"
        );
        Ok(())
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
        let graph = crossing_graph(ONE_CROSSING_CALL)?;
        let records = super::records_by_identity(&graph);
        let relationships =
            super::module_relationships(&graph, &unit_paths(CROSSING_UNITS)?, &records);
        assert_eq!(pairs(&relationships), [("src", "src/inner", 1)]);
        Ok(())
    }

    #[test]
    fn every_relationship_endpoint_names_a_listed_module() -> TestResult {
        let graph = crossing_graph(ONE_CROSSING_CALL)?;
        let records = super::records_by_identity(&graph);
        let relationships =
            super::module_relationships(&graph, &unit_paths(CROSSING_UNITS)?, &records);
        let listed = listed_modules(CROSSING_UNITS)?;
        assert!(
            !relationships.is_empty(),
            "the crossing graph answers one relationship to check"
        );
        for relationship in &relationships {
            assert!(
                listed.contains(&relationship.from.0),
                "from={} listed={listed:?}",
                relationship.from.0
            );
            assert!(
                listed.contains(&relationship.to.0),
                "to={} listed={listed:?}",
                relationship.to.0
            );
        }
        Ok(())
    }

    #[test]
    fn several_references_over_one_pair_answer_as_one_relationship() -> TestResult {
        let graph = crossing_graph(&[
            ("first_call_helper", "src/lib.rs", (10, 20), "helper"),
            ("second_call_helper", "src/lib.rs", (20, 30), "helper"),
        ])?;
        let records = super::records_by_identity(&graph);
        let relationships =
            super::module_relationships(&graph, &unit_paths(CROSSING_UNITS)?, &records);
        assert_eq!(pairs(&relationships), [("src", "src/inner", 2)]);
        Ok(())
    }

    #[test]
    fn a_reference_inside_one_module_is_no_relationship() -> TestResult {
        let graph = resolved_graph(
            &[
                (
                    "helper",
                    "rift://symbol/rust/src/lib.rs/helper",
                    "src/lib.rs",
                    (0, 20),
                ),
                (
                    "beacon",
                    "rift://symbol/rust/src/lib.rs/beacon",
                    "src/lib.rs",
                    (20, 50),
                ),
            ],
            &[("beacon_calls_helper", "src/lib.rs", (30, 40), "helper")],
        )?;
        let records = super::records_by_identity(&graph);
        let relationships =
            super::module_relationships(&graph, &unit_paths(&["src/lib.rs"])?, &records);
        assert!(
            !resolved_targets(&graph).is_empty(),
            "the reference must resolve, or an empty answer proves nothing"
        );
        assert!(
            relationships.is_empty(),
            "a module referencing itself says nothing about structure: {relationships:?}"
        );
        Ok(())
    }

    #[test]
    fn a_reference_from_a_root_file_contributes_no_endpoint() -> TestResult {
        let graph = resolved_graph(
            &[
                (
                    "beacon",
                    "rift://symbol/rust/lib.rs/beacon",
                    "lib.rs",
                    (0, 30),
                ),
                (
                    "helper",
                    "rift://symbol/rust/inner/mod.rs/helper",
                    "inner/mod.rs",
                    (0, 20),
                ),
            ],
            &[("beacon_calls_helper", "lib.rs", (10, 20), "helper")],
        )?;
        let records = super::records_by_identity(&graph);
        let paths = unit_paths(&["lib.rs", "inner/mod.rs"])?;
        let relationships = super::module_relationships(&graph, &paths, &records);
        assert!(
            !resolved_targets(&graph).is_empty(),
            "the reference must resolve, or an empty answer proves nothing"
        );
        assert!(
            relationships.is_empty(),
            "a file directly at the workspace root belongs to no listed module: {relationships:?}"
        );
        Ok(())
    }

    #[test]
    fn module_relationships_are_stable_across_two_builds_of_one_tree() -> TestResult {
        let first_graph = crossing_graph(ONE_CROSSING_CALL)?;
        let second_graph = crossing_graph(ONE_CROSSING_CALL)?;
        let first_records = super::records_by_identity(&first_graph);
        let second_records = super::records_by_identity(&second_graph);
        let first =
            super::module_relationships(&first_graph, &unit_paths(CROSSING_UNITS)?, &first_records);
        let second = super::module_relationships(
            &second_graph,
            &unit_paths(CROSSING_UNITS)?,
            &second_records,
        );
        assert!(
            !first.is_empty(),
            "the crossing graph answers one relationship to compare"
        );
        assert_eq!(first, second);
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

    /// The shipped state, not an accident: no provider publishes a resolved reference into the
    /// index, so both rankings that read the graph's references answer empty for a workspace
    /// whose source plainly calls across two modules. The ranking and the fold themselves stay
    /// proven over the hand-built graphs above.
    #[test]
    fn a_served_workspace_answers_no_hub_and_no_module_relationship() -> TestResult {
        let (_directory, map) = served_map(CROSSING_FILES)?;
        assert!(
            map.hubs.is_empty(),
            "no provider publishes a resolved reference, so no symbol ranks as a hub: {:?}",
            map.hubs
        );
        assert!(
            map.module_relationships.is_empty(),
            "no provider publishes a resolved reference, so no module pair is joined: {:?}",
            map.module_relationships
        );
        Ok(())
    }
}
