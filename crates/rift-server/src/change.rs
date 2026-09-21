//! Declaration-level comparison of two committed revisions.
//!
//! One `search` request carrying `change` lists the paths the two revisions
//! hold differently, builds a read index over those paths on each side, and
//! classifies every declaration the two sides disagree on through the same
//! classifier a symbol timeline runs. Only the changed paths are read and
//! parsed: the comparison never builds a whole revision index.
//!

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use rift_core::constants::SEARCH_RESULTS_DEFAULT;
use rift_core::{LanguageFileSelections, ProjectPath, SourceVisibility, TextFileInclusion};
use rift_history::Repository;
use rift_index::{
    IndexedFile, RevisionPaths, SymbolMatch, WorkspaceIndex, WorkspaceIndexLimits,
    WorkspaceIndexWarning,
};
use rift_protocol::read::{
    MatchedField, PathSelector, ReadWarning, ResultOrder, SEARCH_CHANGE_PATHS_MAX, SearchChange,
    SearchHit, SearchParams, SearchParamsTarget, SearchResult, SymbolChange, SymbolVersionKind,
};
use rift_ranking::IdentifierMatchClass;
use rift_syntax::SyntaxSymbol;

use crate::history::{SymbolShape, SymbolState, classify};
use crate::read::{
    ReadError, ReadFault, accepted_limit, page, project_path, results_truncation_warning,
    source_warnings,
};
use crate::search::{
    HitPayloads, bound_hits, build_symbol_hit, order_hits, path_matcher, validate_search,
};

/// Answers one `search` request that carries `change`: the declarations the two
/// named revisions hold differently, ordered and paged like any other search
/// answer.
///
/// # Errors
///
/// Returns [`ReadError`] when the request combines `change` with a field that
/// selects another result set, when a revision spelling breaks its contract or
/// resolves to no commit, when the workspace has no version-control repository,
/// or when one side's changed paths cannot be indexed within bounds.
pub fn search_change(
    root: &Path,
    params: &SearchParams,
    change: &SearchChange,
    limits: WorkspaceIndexLimits,
    visibility: &SourceVisibility,
    (text_inclusion, languages): (&TextFileInclusion, &LanguageFileSelections),
) -> Result<SearchResult, ReadError> {
    validate_search(params)?;
    let limit = accepted_limit(params.limit.unwrap_or(SEARCH_RESULTS_DEFAULT as u64))?;
    let compared = ComparedRevisions::open(
        root,
        change,
        params.paths.as_ref(),
        limits,
        visibility,
        (text_inclusion, languages),
    )?;
    let mut results = compared.hits(params)?;
    order_change_hits(&mut results, params.order);
    let results_max_reached = bound_hits(&mut results, compared.head.results_max());
    let (results, pagination) = page(results, params.page_index, limit);
    Ok(SearchResult {
        results,
        pagination,
        warnings: compared.warnings(results_max_reached),
    })
}

/// One comparison's two sides: the read index each revision holds over the paths
/// the two hold differently.
struct ComparedRevisions {
    base: WorkspaceIndex,
    head: WorkspaceIndex,
    paths: Vec<ProjectPath>,
    truncated: bool,
}

impl ComparedRevisions {
    /// Resolves both revisions, lists the paths they hold differently under the
    /// workspace's visible-path policy and the request's own `paths` selector, and
    /// indexes those paths on each side.
    fn open(
        root: &Path,
        change: &SearchChange,
        selector: Option<&PathSelector>,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
        (text_inclusion, languages): (&TextFileInclusion, &LanguageFileSelections),
    ) -> Result<Self, ReadError> {
        let repository = Repository::open(root).map_err(ReadFault::history)?;
        let base = repository
            .resolve(&change.base.0)
            .map_err(ReadFault::history)?;
        let head = repository
            .resolve(&change.head.0)
            .map_err(ReadFault::history)?;
        let visible = RevisionPaths::build(root, visibility).map_err(ReadFault::index)?;
        let requested = path_matcher(root, selector)?;
        let compared = |path: &str| {
            visible.includes(path)
                && requested
                    .as_ref()
                    .is_none_or(|matcher| matcher.includes(&root.join(path)))
        };
        let paths_max = usize::try_from(SEARCH_CHANGE_PATHS_MAX).unwrap_or(usize::MAX);
        let changed = repository
            .changed_files(&base, &head, &compared, paths_max)
            .map_err(ReadFault::history)?;
        let selected: HashSet<&str> = changed.paths().iter().map(String::as_str).collect();
        let holds = |path: &str| selected.contains(path);
        let index_side = |revision| {
            WorkspaceIndex::at_revision_with_selection(
                &repository,
                revision,
                limits,
                visibility,
                text_inclusion,
                languages,
                &holds,
            )
            .map_err(ReadFault::index)
        };
        let base_index = index_side(&base)?;
        let head_index = index_side(&head)?;
        // Every selected path already passed the project-path contract: the index
        // build over the same selection refuses a spelling that contract forbids
        // before this list is taken.
        let paths = changed
            .paths()
            .iter()
            .filter_map(|path| ProjectPath::new(path.clone()).ok())
            .collect();
        Ok(Self {
            base: base_index,
            head: head_index,
            paths,
            truncated: changed.is_truncated(),
        })
    }

    /// The hits one comparison answers: every declaration the two sides hold
    /// differently, with an addition and a removal paired into one moved
    /// declaration wherever that pairing is unambiguous.
    ///
    /// `target: "file"` answers none of them, the way a traversal does: a
    /// comparison reaches declarations alone.
    fn hits(&self, params: &SearchParams) -> Result<Vec<SearchHit>, ReadError> {
        if params.target == SearchParamsTarget::File {
            return Ok(Vec::new());
        }
        let payloads = HitPayloads::for_change(params);
        let mut results = Vec::new();
        let mut unpaired = UnpairedDeclarations::default();
        for path in &self.paths {
            self.compare_path(path, payloads, &mut results, &mut unpaired)?;
        }
        unpaired.resolve(payloads, &mut results)?;
        Ok(results)
    }

    /// Compares one changed path. A path only one side indexes contributes every
    /// declaration it holds to that side's unpaired list; a path neither side
    /// indexes - a plain text file, or one whose bytes no provider could read -
    /// contributes none.
    fn compare_path<'sides>(
        &'sides self,
        path: &ProjectPath,
        payloads: HitPayloads,
        results: &mut Vec<SearchHit>,
        unpaired: &mut UnpairedDeclarations<'sides>,
    ) -> Result<(), ReadError> {
        match (self.base.file(path), self.head.file(path)) {
            (Some(base_file), Some(head_file)) => {
                self.compare_files(path, (base_file, head_file), payloads, results, unpaired)
            }
            (None, Some(head_file)) => {
                unpaired.added.extend(held(&self.head, head_file));
                Ok(())
            }
            (Some(base_file), None) => {
                unpaired.removed.extend(held(&self.base, base_file));
                Ok(())
            }
            (None, None) => Ok(()),
        }
    }

    /// Compares the declarations two indexed files hold. A declaration both files
    /// hold is classified against its base-side bytes; one only a file holds waits
    /// for the cross-path pairing that decides whether it moved.
    fn compare_files<'sides>(
        &'sides self,
        path: &ProjectPath,
        (base_file, head_file): (&'sides IndexedFile, &'sides IndexedFile),
        payloads: HitPayloads,
        results: &mut Vec<SearchHit>,
        unpaired: &mut UnpairedDeclarations<'sides>,
    ) -> Result<(), ReadError> {
        let base = declarations(base_file);
        let head = declarations(head_file);
        for (key, head_symbol) in &head {
            let added = Declaration::new(&self.head, head_file, head_symbol);
            let Some(base_symbol) = base.get(key).copied() else {
                // The base side holds no such declaration, in this path or any
                // other yet: whether it moved here is decided once every path is
                // compared.
                unpaired.added.push(added);
                continue;
            };
            let older =
                SymbolState::Present(SymbolShape::from_source(base_file.source(), base_symbol));
            let newer =
                SymbolState::Present(SymbolShape::from_source(head_file.source(), head_symbol));
            let Some(kind) = classify(&older, &newer) else {
                continue;
            };
            let wire_path = project_path(path);
            let change = SymbolChange {
                kind,
                base_path: Some(wire_path.clone()),
                head_path: Some(wire_path),
            };
            results.push(changed_hit(&added, change, payloads)?);
        }
        for (key, base_symbol) in &base {
            if head.contains_key(key) {
                continue;
            }
            unpaired
                .removed
                .push(Declaration::new(&self.base, base_file, base_symbol));
        }
        Ok(())
    }

    /// The warnings one comparison answer carries: the changed-path bound when the
    /// comparison reached it, the files either side left out of its index, what a
    /// walk riding beside the comparison reported, then the result bound when the
    /// hit set reached it.
    fn warnings(&self, results_max_reached: Option<usize>) -> Vec<ReadWarning> {
        let mut warnings = Vec::new();
        if self.truncated {
            warnings.push(change_truncation_warning());
        }
        warnings.extend(source_warnings(&self.left_out()));
        if let Some(results_max) = results_max_reached {
            warnings.push(results_truncation_warning(results_max));
        }
        warnings
    }

    /// The files either side left out of its index: a blob past the per-file byte
    /// bound, one whose bytes are not UTF-8, or one a provider refused. Such a file
    /// contributes no declaration on the side that left it out, and every other
    /// changed path still answers. A file both sides left out is named once.
    fn left_out(&self) -> Vec<WorkspaceIndexWarning> {
        let mut left_out: Vec<WorkspaceIndexWarning> = Vec::new();
        for warning in self.head.warnings().iter().chain(self.base.warnings()) {
            if !left_out.contains(warning) {
                left_out.push(warning.clone());
            }
        }
        left_out
    }
}

/// One declaration's identity inside one path: the provider's qualified name and
/// its kind word.
type DeclarationKey<'side> = (&'side str, &'static str);

/// The pairing key an addition and a removal have to share to be one declaration
/// that moved: the qualified name, the kind word, and the exact bytes it is
/// written in.
type MoveKey = (String, &'static str, SymbolShape);

/// The additions and the removals one move key groups, by their position in the
/// unpaired lists.
type MoveGroup = (Vec<usize>, Vec<usize>);

/// The declarations one side holds in one path, keyed by the provider's qualified
/// name and its kind word.
///
/// A provider names one declaration once per document, so the first entry under a
/// key is the declaration - the same rule a symbol timeline's revision state
/// applies when it looks one up by qualified name.
fn declarations(file: &IndexedFile) -> BTreeMap<DeclarationKey<'_>, &SyntaxSymbol> {
    let mut declarations = BTreeMap::new();
    for symbol in file.syntax().symbols() {
        declarations
            .entry((symbol.qualified_name.as_str(), symbol.kind))
            .or_insert(symbol);
    }
    declarations
}

/// Every declaration one indexed file holds, as the side holding it sees them:
/// one entry per declaration key, the way `declarations` keys them.
fn held<'side>(
    index: &'side WorkspaceIndex,
    file: &'side IndexedFile,
) -> impl Iterator<Item = Declaration<'side>> {
    declarations(file)
        .into_values()
        .map(move |symbol| Declaration::new(index, file, symbol))
}

/// One declaration as one side of the comparison holds it.
#[derive(Clone, Copy)]
struct Declaration<'side> {
    index: &'side WorkspaceIndex,
    file: &'side IndexedFile,
    symbol: &'side SyntaxSymbol,
}

impl<'side> Declaration<'side> {
    const fn new(
        index: &'side WorkspaceIndex,
        file: &'side IndexedFile,
        symbol: &'side SyntaxSymbol,
    ) -> Self {
        Self {
            index,
            file,
            symbol,
        }
    }

    /// This declaration's pairing key.
    fn move_key(&self) -> MoveKey {
        (
            self.symbol.qualified_name.clone(),
            self.symbol.kind,
            SymbolShape::from_source(self.file.source(), self.symbol),
        )
    }
}

/// The declarations one side holds and the other does not, held until the
/// comparison knows which of them pair up across paths.
#[derive(Default)]
struct UnpairedDeclarations<'sides> {
    added: Vec<Declaration<'sides>>,
    removed: Vec<Declaration<'sides>>,
}

impl UnpairedDeclarations<'_> {
    /// Pairs additions with removals and turns both into hits.
    ///
    /// A pairing key holding exactly one addition and exactly one removal, in two
    /// different paths, is one declaration that moved. Any other key - two
    /// additions, two removals, or one of each in the same path - leaves both
    /// sides standing on their own, because no evidence says which addition
    /// answers which removal.
    fn resolve(self, payloads: HitPayloads, results: &mut Vec<SearchHit>) -> Result<(), ReadError> {
        let mut groups: HashMap<MoveKey, MoveGroup> = HashMap::new();
        for (position, added) in self.added.iter().enumerate() {
            groups.entry(added.move_key()).or_default().0.push(position);
        }
        for (position, removed) in self.removed.iter().enumerate() {
            groups
                .entry(removed.move_key())
                .or_default()
                .1
                .push(position);
        }
        let mut moved_added = vec![false; self.added.len()];
        let mut moved_removed = vec![false; self.removed.len()];
        for (additions, removals) in groups.into_values() {
            let ([addition], [removal]) = (additions.as_slice(), removals.as_slice()) else {
                continue;
            };
            let added = self.added[*addition];
            let removed = self.removed[*removal];
            let added_path = added.file.path();
            let removed_path = removed.file.path();
            assert_ne!(
                added_path, removed_path,
                "one pairing key cannot hold an addition and a removal in one path, since a \
                 declaration both sides of that path hold is classified instead: \
                 path={added_path:?}"
            );
            moved_added[*addition] = true;
            moved_removed[*removal] = true;
            let change = SymbolChange {
                kind: SymbolVersionKind::Moved,
                base_path: Some(project_path(removed_path)),
                head_path: Some(project_path(added_path)),
            };
            results.push(changed_hit(&added, change, payloads)?);
        }
        for (position, added) in self.added.iter().enumerate() {
            if moved_added[position] {
                continue;
            }
            let change = SymbolChange {
                kind: SymbolVersionKind::Introduced,
                base_path: None,
                head_path: Some(project_path(added.file.path())),
            };
            results.push(changed_hit(added, change, payloads)?);
        }
        for (position, removed) in self.removed.iter().enumerate() {
            if moved_removed[position] {
                continue;
            }
            let change = SymbolChange {
                kind: SymbolVersionKind::Removed,
                base_path: Some(project_path(removed.file.path())),
                head_path: None,
            };
            results.push(changed_hit(removed, change, payloads)?);
        }
        Ok(())
    }
}

/// One change hit over `declaration`'s bytes as its own side holds them.
///
/// A comparison ranks nothing, so the hit carries no score whatever `include`
/// asks for; `change` is what a caller reads in its place.
fn changed_hit(
    declaration: &Declaration<'_>,
    change: SymbolChange,
    payloads: HitPayloads,
) -> Result<SearchHit, ReadError> {
    let matched = SymbolMatch {
        file: declaration.file,
        symbol: declaration.symbol,
        // This lane reads no identifier rank: nothing here was matched by name.
        rank: IdentifierMatchClass::Substring,
    };
    let matched_by = vec![MatchedField::Change];
    let mut hit = build_symbol_hit(declaration.index, matched, None, matched_by, payloads)?;
    hit.change = Some(change);
    Ok(hit)
}

/// Orders one comparison's hits.
///
/// A comparison ranks nothing, so `relevance` has no score to order by and takes
/// the path order instead, which already ends in the hit's own identity - within
/// one path, that identity's last segment is the declaration's qualified name.
fn order_change_hits(results: &mut [SearchHit], order: ResultOrder) {
    let order = match order {
        ResultOrder::Relevance => ResultOrder::Path,
        ResultOrder::Path | ResultOrder::Identity => order,
    };
    order_hits(results, order);
}

/// The warning a comparison that reached its changed-path bound carries.
fn change_truncation_warning() -> ReadWarning {
    ReadWarning::ChangeTruncated {
        paths_max: SEARCH_CHANGE_PATHS_MAX,
        detail: format!(
            "the two revisions differ in more than {SEARCH_CHANGE_PATHS_MAX} paths, so the \
             comparison stopped there; declarations in the changed paths past it are absent, \
             and `paths` narrows a further comparison onto them"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;

    use rift_core::{ErrorCode, ErrorName, Fault as _};
    use rift_history::fixture::{commit_all, git, init};
    use rift_protocol::read::ProjectPath as WireProjectPath;
    use serde_json::{Value, json};

    use super::*;

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    /// One comparison over a fixture whose base revision carries the tag
    /// `baseline`: `files` is written and committed on top of it, and whatever
    /// `removed` names is deleted first.
    struct Fixture {
        directory: tempfile::TempDir,
        limits: WorkspaceIndexLimits,
    }

    impl Fixture {
        /// Commits `base` as the tagged baseline, then writes `head` over it,
        /// deleting whatever `removed` names first, and commits that.
        fn revisions(
            base: &[(&str, &str)],
            head: &[(&str, &str)],
            removed: &[&str],
        ) -> TestResult<Self> {
            Self::baseline(base)?.head(head, removed)
        }

        /// Commits `base` as the tagged baseline.
        fn baseline(base: &[(&str, &str)]) -> TestResult<Self> {
            let directory = tempfile::tempdir()?;
            write_all(directory.path(), base)?;
            init(directory.path());
            commit_all(directory.path(), "baseline");
            git(directory.path(), &["tag", "baseline"]);
            Ok(Self {
                directory,
                limits: WorkspaceIndexLimits::default(),
            })
        }

        /// The same fixture under tighter index bounds.
        const fn under(mut self, limits: WorkspaceIndexLimits) -> Self {
            self.limits = limits;
            self
        }

        /// Writes `head`, deletes `removed`, and commits the result.
        fn head(self, head: &[(&str, &str)], removed: &[&str]) -> TestResult<Self> {
            for path in removed {
                fs::remove_file(self.directory.path().join(path))?;
            }
            write_all(self.directory.path(), head)?;
            commit_all(self.directory.path(), "head");
            Ok(self)
        }

        /// Answers the comparison `params` asks for over this fixture.
        fn search(&self, params: &serde_json::Value) -> Result<SearchResult, ReadError> {
            let params: SearchParams =
                serde_json::from_value(params.clone()).expect("test parameters must deserialize");
            let change = params.change.clone().expect("the test names a change");
            search_change(
                self.directory.path(),
                &params,
                &change,
                self.limits,
                &SourceVisibility::default(),
                (
                    &TextFileInclusion::default(),
                    &LanguageFileSelections::default(),
                ),
            )
        }

        /// The comparison of `baseline` against `HEAD`, with no other criteria.
        fn baseline_to_head(&self) -> Result<SearchResult, ReadError> {
            self.search(&json!({"change": {"base": "baseline"}}))
        }
    }

    fn write_all(root: &std::path::Path, files: &[(&str, &str)]) -> TestResult {
        for (name, source) in files {
            let path = root.join(name);
            fs::create_dir_all(path.parent().unwrap_or(root))?;
            fs::write(path, source)?;
        }
        Ok(())
    }

    /// One hit's declaration name, its change kind, and the two paths it names.
    type ChangeRow = (String, String, Option<String>, Option<String>);

    /// Every hit's declaration name, its change kind, and the two paths it names,
    /// in the order the answer carries them.
    fn changes(answer: &SearchResult) -> Vec<ChangeRow> {
        let wire = serde_json::to_value(answer).expect("a result serializes");
        wire["results"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(|hit| {
                (
                    text(&hit["hit"]["symbol"]["name"]),
                    text(&hit["change"]["kind"]),
                    hit["change"]["base_path"].as_str().map(str::to_owned),
                    hit["change"]["head_path"].as_str().map(str::to_owned),
                )
            })
            .collect()
    }

    /// One wire string, read as the payload spells it.
    fn text(value: &Value) -> String {
        value.as_str().unwrap_or_default().to_owned()
    }

    fn wire_code(error: &ReadError) -> ErrorName {
        error.fault().name()
    }

    #[test]
    fn change_reports_an_added_declaration_as_introduced() -> TestResult {
        let base = [("src/lib.rs", "pub fn kept() {}\n")];
        let head = [("src/added.rs", "pub fn added() {}\n")];
        let fixture = Fixture::revisions(&base, &head, &[])?;

        let answer = fixture.baseline_to_head()?;

        assert_eq!(
            changes(&answer),
            [(
                "added".to_owned(),
                "introduced".to_owned(),
                None,
                Some("src/added.rs".to_owned())
            )]
        );
        Ok(())
    }

    #[test]
    fn change_reports_a_removed_declaration_from_its_base_side() -> TestResult {
        let base = [
            ("src/lib.rs", "pub fn kept() {}\n"),
            ("src/gone.rs", "pub fn gone() {}\n"),
        ];
        let fixture = Fixture::revisions(&base, &[], &["src/gone.rs"])?;

        let answer = fixture.search(&json!({
            "change": {"base": "baseline"},
            "include": ["source"]
        }))?;

        assert_eq!(
            changes(&answer),
            [(
                "gone".to_owned(),
                "removed".to_owned(),
                Some("src/gone.rs".to_owned()),
                None
            )]
        );
        let hit = &answer.results[0];
        assert_eq!(hit.path, Some(WireProjectPath("src/gone.rs".to_owned())));
        assert_eq!(hit.source.as_deref(), Some("pub fn gone() {}"));
        assert_eq!(hit.line, Some(1));
        Ok(())
    }

    #[test]
    fn change_separates_signature_body_and_decorator_changes() -> TestResult {
        let base = [
            ("src/signature.rs", "pub fn shifted() {}\n"),
            ("src/body.rs", "pub fn worked() {\n    let x = 1;\n}\n"),
            (
                "src/decorators.rs",
                "/// One.\npub fn documented() {\n    let x = 1;\n}\n",
            ),
        ];
        let head = [
            ("src/signature.rs", "pub fn shifted(flag: bool) {}\n"),
            ("src/body.rs", "pub fn worked() {\n    let x = 2;\n}\n"),
            (
                "src/decorators.rs",
                "/// Two.\npub fn documented() {\n    let x = 1;\n}\n",
            ),
        ];
        let fixture = Fixture::revisions(&base, &head, &[])?;

        let answer = fixture.baseline_to_head()?;

        assert_eq!(
            changes(&answer),
            [
                (
                    "worked".to_owned(),
                    "body_changed".to_owned(),
                    Some("src/body.rs".to_owned()),
                    Some("src/body.rs".to_owned())
                ),
                (
                    "documented".to_owned(),
                    "decorators_changed".to_owned(),
                    Some("src/decorators.rs".to_owned()),
                    Some("src/decorators.rs".to_owned())
                ),
                (
                    "shifted".to_owned(),
                    "signature_changed".to_owned(),
                    Some("src/signature.rs".to_owned()),
                    Some("src/signature.rs".to_owned())
                ),
            ]
        );
        Ok(())
    }

    /// A path both revisions index, where the head revision adds one declaration
    /// and drops another, answers one `introduced` hit beside one `removed` hit.
    #[test]
    fn change_reports_a_declaration_added_and_one_dropped_inside_one_path() -> TestResult {
        let base = [("src/lib.rs", "pub fn kept() {}\npub fn dropped() {}\n")];
        let head = [("src/lib.rs", "pub fn kept() {}\npub fn arrived() {}\n")];
        let fixture = Fixture::revisions(&base, &head, &[])?;

        let answer = fixture.baseline_to_head()?;

        assert_eq!(
            changes(&answer),
            [
                (
                    "arrived".to_owned(),
                    "introduced".to_owned(),
                    None,
                    Some("src/lib.rs".to_owned())
                ),
                (
                    "dropped".to_owned(),
                    "removed".to_owned(),
                    Some("src/lib.rs".to_owned()),
                    None
                ),
            ]
        );
        Ok(())
    }

    /// A file both revisions changed outside every declaration answers no hit,
    /// even though the comparison read and parsed it.
    #[test]
    fn change_reports_no_hit_for_a_file_whose_declarations_stand() -> TestResult {
        let base = [("src/lib.rs", "pub fn kept() {}\n")];
        let head = [("src/lib.rs", "// a leading comment\npub fn kept() {}\n")];
        let fixture = Fixture::revisions(&base, &head, &[])?;

        let answer = fixture.baseline_to_head()?;

        assert!(answer.results.is_empty(), "{answer:?}");
        assert!(answer.warnings.is_empty(), "{answer:?}");
        Ok(())
    }

    #[test]
    fn change_pairs_a_declaration_moved_between_paths() -> TestResult {
        let base = [("src/from.rs", "pub fn travelled() {}\n")];
        let head = [("src/to.rs", "pub fn travelled() {}\n")];
        let fixture = Fixture::revisions(&base, &head, &["src/from.rs"])?;

        let answer = fixture.baseline_to_head()?;

        assert_eq!(
            changes(&answer),
            [(
                "travelled".to_owned(),
                "moved".to_owned(),
                Some("src/from.rs".to_owned()),
                Some("src/to.rs".to_owned())
            )]
        );
        assert_eq!(
            answer.results[0].path,
            Some(WireProjectPath("src/to.rs".to_owned())),
            "a moved hit reads its head side"
        );
        Ok(())
    }

    /// Two removals sharing one addition's name, kind, and bytes pair with
    /// nothing: the evidence cannot say which removal the addition answers.
    #[test]
    fn change_leaves_an_ambiguous_move_as_an_addition_beside_its_removals() -> TestResult {
        let base = [
            ("src/first.rs", "pub fn travelled() {}\n"),
            ("src/second.rs", "pub fn travelled() {}\n"),
        ];
        let head = [("src/third.rs", "pub fn travelled() {}\n")];
        let removed = ["src/first.rs", "src/second.rs"];
        let fixture = Fixture::revisions(&base, &head, &removed)?;

        let answer = fixture.baseline_to_head()?;

        assert_eq!(
            changes(&answer),
            [
                (
                    "travelled".to_owned(),
                    "removed".to_owned(),
                    Some("src/first.rs".to_owned()),
                    None
                ),
                (
                    "travelled".to_owned(),
                    "removed".to_owned(),
                    Some("src/second.rs".to_owned()),
                    None
                ),
                (
                    "travelled".to_owned(),
                    "introduced".to_owned(),
                    None,
                    Some("src/third.rs".to_owned())
                ),
            ]
        );
        Ok(())
    }

    /// An addition sharing a removed declaration's name but written differently
    /// is its own declaration, not the removed one relocated.
    #[test]
    fn change_never_pairs_a_same_named_declaration_written_differently() -> TestResult {
        let base = [("src/from.rs", "pub fn travelled() {}\n")];
        let head = [("src/to.rs", "pub fn travelled(flag: bool) {}\n")];
        let fixture = Fixture::revisions(&base, &head, &["src/from.rs"])?;

        let answer = fixture.baseline_to_head()?;

        assert_eq!(
            changes(&answer),
            [
                (
                    "travelled".to_owned(),
                    "removed".to_owned(),
                    Some("src/from.rs".to_owned()),
                    None
                ),
                (
                    "travelled".to_owned(),
                    "introduced".to_owned(),
                    None,
                    Some("src/to.rs".to_owned())
                ),
            ]
        );
        Ok(())
    }

    /// A changed path whose bytes are not UTF-8 joins neither side's index, so it
    /// contributes no declaration while every other changed path answers.
    #[test]
    fn change_answers_the_rest_when_one_changed_blob_is_not_utf8() -> TestResult {
        let base = [("src/lib.rs", "pub fn kept() {}\n")];
        let fixture = Fixture::baseline(&base)?;
        let root = fixture.directory.path();
        fs::write(root.join("src/binary.rs"), [0xff, 0xfe])?;
        fs::write(root.join("src/added.rs"), "pub fn added() {}\n")?;
        commit_all(root, "head");

        let answer = fixture.baseline_to_head()?;

        assert_eq!(
            changes(&answer),
            [(
                "added".to_owned(),
                "introduced".to_owned(),
                None,
                Some("src/added.rs".to_owned())
            )]
        );
        Ok(())
    }

    /// `paths` narrows which changed paths are compared, through the same glob
    /// engine every other selector uses.
    #[test]
    fn change_compares_only_the_paths_the_selector_keeps() -> TestResult {
        let base = [("src/lib.rs", "pub fn kept() {}\n")];
        let head = [
            ("src/added.rs", "pub fn added() {}\n"),
            ("other/added.rs", "pub fn elsewhere() {}\n"),
        ];
        let fixture = Fixture::revisions(&base, &head, &[])?;

        let answer = fixture.search(&json!({
            "change": {"base": "baseline"},
            "paths": {"include": ["src/**"]}
        }))?;

        assert_eq!(
            changes(&answer),
            [(
                "added".to_owned(),
                "introduced".to_owned(),
                None,
                Some("src/added.rs".to_owned())
            )]
        );
        Ok(())
    }

    /// A comparison reaches declarations alone, so a `file` target answers none.
    #[test]
    fn change_with_a_file_target_answers_no_hit() -> TestResult {
        let fixture = Fixture::baseline(&[("src/lib.rs", "pub fn kept() {}\n")])?
            .head(&[("src/added.rs", "pub fn added() {}\n")], &[])?;

        let answer = fixture.search(&json!({
            "change": {"base": "baseline"},
            "target": "file"
        }))?;

        assert!(answer.results.is_empty(), "{answer:?}");
        Ok(())
    }

    /// `head` defaults to `HEAD`, and a comparison of one revision against itself
    /// answers nothing.
    #[test]
    fn change_of_one_revision_against_itself_answers_nothing() -> TestResult {
        let fixture = Fixture::baseline(&[("src/lib.rs", "pub fn kept() {}\n")])?;

        let answer = fixture.search(&json!({"change": {"base": "HEAD", "head": "HEAD"}}))?;

        assert!(answer.results.is_empty(), "{answer:?}");
        Ok(())
    }

    /// A changed blob past the per-file byte bound joins neither side's index,
    /// so it contributes no declaration while the changed path beside it answers.
    #[test]
    fn change_answers_the_rest_when_one_changed_blob_is_oversized() -> TestResult {
        let oversized = format!("pub fn oversized() {{\n    // {}\n}}\n", "x".repeat(2_048));
        let base = [("src/lib.rs", "pub fn kept() {}\n")];
        let head = [
            ("src/added.rs", "pub fn added() {}\n"),
            ("src/oversized.rs", oversized.as_str()),
        ];
        let limits = WorkspaceIndexLimits::new(64, 512, 262_144, 8, 1_000)?;
        let fixture = Fixture::revisions(&base, &head, &[])?.under(limits);

        let answer = fixture.baseline_to_head()?;

        assert_eq!(
            changes(&answer),
            [(
                "added".to_owned(),
                "introduced".to_owned(),
                None,
                Some("src/added.rs".to_owned())
            )]
        );
        Ok(())
    }

    /// The comparison stops at its changed-path bound, answers the declarations
    /// in the paths that fit, and names the bound it reached.
    #[test]
    fn change_stops_at_the_changed_path_bound_and_warns() -> TestResult {
        let fixture = Fixture::baseline(&[("src/lib.rs", "pub fn kept() {}\n")])?;
        // The tree entries sort `added.rs` ahead of the `noise` directory, so the
        // one declaration this comparison can answer is compared before the
        // budget is spent on the rest.
        let root = fixture.directory.path();
        fs::write(root.join("added.rs"), "pub fn added() {}\n")?;
        fs::create_dir_all(root.join("noise"))?;
        let noise_files = usize::try_from(SEARCH_CHANGE_PATHS_MAX).unwrap_or(usize::MAX);
        for index in 0..noise_files {
            fs::write(root.join(format!("noise/{index:04}.txt")), "prose\n")?;
        }
        commit_all(root, "head");

        let answer = fixture.baseline_to_head()?;

        assert_eq!(
            changes(&answer),
            [(
                "added".to_owned(),
                "introduced".to_owned(),
                None,
                Some("added.rs".to_owned())
            )]
        );
        assert_eq!(
            answer.warnings,
            vec![super::change_truncation_warning()],
            "{answer:?}"
        );
        Ok(())
    }

    /// The result bound cuts a comparison's hit set the way it cuts every other
    /// search answer, and the answer names the bound it reached.
    #[test]
    fn change_stops_at_the_result_bound_and_warns() -> TestResult {
        let base = [("src/lib.rs", "pub fn kept() {}\n")];
        let head = [("src/added.rs", "pub fn first() {}\npub fn second() {}\n")];
        let limits = WorkspaceIndexLimits::new(64, 1_000_000, 262_144, 8, 1)?;
        let fixture = Fixture::revisions(&base, &head, &[])?.under(limits);

        let answer = fixture.baseline_to_head()?;

        assert_eq!(answer.results.len(), 1, "{answer:?}");
        assert_eq!(
            answer.warnings,
            vec![results_truncation_warning(1)],
            "{answer:?}"
        );
        Ok(())
    }

    /// A changed path the syntax provider refuses joins neither side's index, so
    /// it contributes no declaration, the answer names it, and every other
    /// changed path still answers.
    #[test]
    fn change_names_a_changed_path_the_provider_refused() -> TestResult {
        let refused = format!(
            "pub fn deep() -> i32 {{ {open}1{close} }}\n",
            open = "(".repeat(600),
            close = ")".repeat(600),
        );
        let base = [("src/lib.rs", "pub fn kept() {}\n")];
        let head = [
            ("src/added.rs", "pub fn added() {}\n"),
            ("src/deep.rs", refused.as_str()),
        ];
        let fixture = Fixture::revisions(&base, &head, &[])?;

        let answer = fixture.baseline_to_head()?;

        assert_eq!(
            changes(&answer),
            [(
                "added".to_owned(),
                "introduced".to_owned(),
                None,
                Some("src/added.rs".to_owned())
            )]
        );
        assert!(
            matches!(
                answer.warnings.as_slice(),
                [ReadWarning::SourceUnavailable { unit: Some(unit), .. }]
                    if unit.0.contains("deep.rs")
            ),
            "{answer:?}"
        );
        Ok(())
    }

    /// `change` names both sides itself, so `rev` has nothing left to address.
    #[test]
    fn change_beside_rev_refuses_as_an_invalid_request() -> TestResult {
        let fixture = Fixture::baseline(&[("src/lib.rs", "pub fn kept() {}\n")])?;

        let error = fixture
            .search(&json!({"change": {"base": "baseline"}, "rev": "main"}))
            .expect_err("change beside rev must refuse");

        assert_eq!(
            wire_code(&error),
            ErrorName::Wire(ErrorCode::InvalidRequest)
        );
        assert!(
            error.to_string().contains("change names its own revisions"),
            "{error}"
        );
        Ok(())
    }

    /// A lexical query and a comparison select different result sets.
    #[test]
    fn change_beside_query_refuses_as_an_invalid_request() -> TestResult {
        let fixture = Fixture::baseline(&[("src/lib.rs", "pub fn kept() {}\n")])?;

        let error = fixture
            .search(&json!({"change": {"base": "baseline"}, "query": "kept"}))
            .expect_err("change beside query must refuse");

        assert_eq!(
            wire_code(&error),
            ErrorName::Wire(ErrorCode::InvalidRequest)
        );
        assert!(
            error
                .to_string()
                .contains("query and change select different result sets"),
            "{error}"
        );
        Ok(())
    }

    /// The dependency index serves the current tree alone, so a scope past
    /// `project` refuses beside a comparison of two committed revisions.
    #[test]
    fn change_beside_a_scope_past_local_refuses_as_an_invalid_request() -> TestResult {
        let fixture = Fixture::baseline(&[("src/lib.rs", "pub fn kept() {}\n")])?;

        for scope in ["global", "all"] {
            let error = fixture
                .search(&json!({"change": {"base": "baseline"}, "scope": scope}))
                .expect_err("change beside a wider scope must refuse");
            assert_eq!(
                wire_code(&error),
                ErrorName::Wire(ErrorCode::InvalidRequest)
            );
            assert!(
                error
                    .to_string()
                    .contains("package facts are served for the current tree alone"),
                "{error}"
            );
        }
        Ok(())
    }

    /// `path` and `identity` take the orders every search answer uses, and
    /// `relevance` falls back to the path order a comparison can state.
    #[test]
    fn change_orders_every_requested_order_deterministically() -> TestResult {
        let base = [("src/b.rs", "pub fn second() {}\n")];
        let head = [
            ("src/a.rs", "pub fn first() {}\n"),
            ("src/b.rs", "pub fn second(flag: bool) {}\n"),
        ];
        let fixture = Fixture::revisions(&base, &head, &[])?;

        for order in ["relevance", "path", "identity"] {
            let answer =
                fixture.search(&json!({"change": {"base": "baseline"}, "order": order}))?;
            let names: Vec<String> = changes(&answer).into_iter().map(|hit| hit.0).collect();
            assert_eq!(names, ["first", "second"], "order={order}");
        }
        Ok(())
    }

    /// A comparison ranks nothing, so no hit carries a score even when
    /// `include` names one.
    #[test]
    fn change_hits_carry_no_score() -> TestResult {
        let fixture = Fixture::baseline(&[("src/lib.rs", "pub fn kept() {}\n")])?
            .head(&[("src/added.rs", "pub fn added() {}\n")], &[])?;

        let answer = fixture.search(&json!({
            "change": {"base": "baseline"},
            "include": ["score"]
        }))?;

        assert!(answer.results[0].score.is_none(), "{answer:?}");
        assert_eq!(answer.results[0].matched_by, [MatchedField::Change]);
        Ok(())
    }

    #[test]
    fn change_refuses_a_revision_that_resolves_to_nothing() -> TestResult {
        let fixture = Fixture::baseline(&[("src/lib.rs", "pub fn kept() {}\n")])?;

        let error = fixture
            .search(&json!({"change": {"base": "no-such-branch"}}))
            .expect_err("an unknown revision must refuse");

        assert_eq!(
            wire_code(&error),
            ErrorName::Wire(ErrorCode::ResourceNotFound)
        );
        Ok(())
    }

    #[test]
    fn change_refuses_a_workspace_with_no_repository() -> TestResult {
        let fixture = Fixture {
            directory: tempfile::tempdir()?,
            limits: WorkspaceIndexLimits::default(),
        };

        let error = fixture
            .baseline_to_head()
            .expect_err("a workspace with no repository must refuse");

        assert_eq!(
            wire_code(&error),
            ErrorName::Wire(ErrorCode::CapabilityUnavailable)
        );
        Ok(())
    }

    #[test]
    fn change_refuses_a_revision_spelling_outside_the_advertised_charset() -> TestResult {
        let fixture = Fixture::baseline(&[("src/lib.rs", "pub fn kept() {}\n")])?;

        let error = fixture
            .search(&json!({"change": {"base": "HEAD~1"}}))
            .expect_err("a spelling outside the charset must refuse");

        assert_eq!(
            wire_code(&error),
            ErrorName::Wire(ErrorCode::InvalidRequest)
        );
        assert!(error.to_string().contains("change.base"), "{error}");
        Ok(())
    }

    /// The base revision of the impact fixture: `watched`, the declaration a later
    /// revision widens, with `calls_watched` calling it; and, in a file of its own, an
    /// unrelated declaration spelled the same, with a caller of its own.
    const IMPACT_BASE: &[(&str, &str)] = &[
        (
            "lib.rs",
            "pub fn watched() {}\npub fn calls_watched() {\n    watched();\n}\n",
        ),
        (
            "other.rs",
            "pub fn watched() {}\npub fn calls_legacy() {\n    watched();\n}\n",
        ),
    ];

    /// The head revision of the impact fixture: `watched` takes a parameter, and its
    /// caller's bytes are the base revision's.
    const IMPACT_HEAD: &[(&str, &str)] = &[(
        "lib.rs",
        "pub fn watched(flag: bool) {}\npub fn calls_watched() {\n    watched();\n}\n",
    )];

    /// A walk needs references a language engine resolves, and an engine session serves
    /// the current tree; a comparison names two committed revisions, so the server refuses
    /// the pairing instead of answering a walk that could follow no edge.
    #[test]
    fn change_refuses_a_traversal_riding_beside_it() -> TestResult {
        let fixture = Fixture::revisions(IMPACT_BASE, IMPACT_HEAD, &[])?;

        let error = fixture
            .search(&json!({
                "change": {"base": "baseline"},
                "traversal": {"direction": "incoming", "facets": ["calls"]}
            }))
            .expect_err("a walk beside a comparison must refuse");

        assert_eq!(
            wire_code(&error),
            ErrorName::Wire(ErrorCode::CapabilityUnavailable)
        );
        assert!(
            error
                .to_string()
                .contains("relationship traversal beside a comparison"),
            "{error}"
        );
        Ok(())
    }
}
