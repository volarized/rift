//! The history store's fill: which commits the `[providers.history]` strategy
//! selects, and the rows each analyzed commit writes.
//!
//! A plan compares what the strategy selects with what the store holds and
//! names the commits still to analyze, newest first, so recent history
//! answers soon after a server starts. Analyzing one commit lists the paths it
//! changed against the commit it is compared with, pairs the pure renames by
//! blob id, parses both sides of every other changed path the workspace's
//! language policy gives a provider, and classifies each declaration the way a
//! symbol timeline does. A lockfile the search index leaves out writes no row.
//! Nothing here schedules, sleeps, or writes: the history task decides when a
//! plan runs and when a batch commits.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use rift_core::{LanguageFileSelections, ProjectPath, SourceVisibility, TextFileInclusion};
use rift_error::errors;
use rift_history::{
    ChangedBlob, REVISION_TREE_ENTRIES_MAX, Repository, ResolvedRevision, TreeFile,
};
use rift_history_store::{
    ChangedPath, CommitRecord, DeclarationChange, HeldCommit, MovedDeclaration,
};
use rift_index::{RevisionPaths, WorkspaceLanguagePolicy};
use rift_protocol::configuration::{HISTORY_REVISIONS_MAX, HistoryConfiguration, HistoryStrategy};
use rift_protocol::read::{CommitAuthor, SymbolVersionKind};
use rift_syntax::{SyntaxLimits, SyntaxProvider, SyntaxSource};

use crate::history::{SymbolShape, SymbolState, classify};
use crate::read::RiftError;

/// Tags one release selection reads, at most.
pub const RELEASE_TAGS_MAX: usize = 65_536;

/// Deleted files one commit compares its added files' declarations against, at
/// most, counted after the pure renames are paired: past it the commit pairs no
/// move by declaration, as git's `diff.renameLimit` bounds its exhaustive rename
/// search.
pub(crate) const MOVE_DELETIONS_MAX: usize = 1_000;

/// The characters that open a wildcard in a tag pattern: the literal text a
/// release selection strips ends before the first of them.
const WILDCARD_OPENERS: [char; 4] = ['*', '?', '[', '{'];

/// One commit a fill still owes the store: the commit, the commit it is
/// compared with, and whether history past it is out of the store's reach.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingCommit {
    revision: ResolvedRevision,
    base: Option<ResolvedRevision>,
    boundary: bool,
}

impl PendingCommit {
    /// The commit to analyze.
    #[must_use]
    pub const fn revision(&self) -> &ResolvedRevision {
        &self.revision
    }

    /// What the store holds for this commit once it is analyzed.
    fn held(&self) -> HeldCommit {
        HeldCommit {
            base: self.base.as_ref().map(ResolvedRevision::commit_id),
            boundary: self.boundary,
        }
    }
}

/// A tag a release selection matched and left out: its name holds no
/// version once the pattern's literal text is stripped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnversionedTag {
    /// The tag's short name.
    pub name: String,
    /// The rest of the name the version parse refused.
    pub remainder: String,
}

/// What one fill owes the store.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FillPlan {
    keep: BTreeSet<String>,
    pending: Vec<PendingCommit>,
    unversioned: Vec<UnversionedTag>,
    releases_past_bound: usize,
}

impl FillPlan {
    /// Every commit the strategy selects: the store keeps these and deletes
    /// the rest.
    #[must_use]
    pub const fn keep(&self) -> &BTreeSet<String> {
        &self.keep
    }

    /// The selected commits the store lacks, or holds compared with another
    /// commit, newest first.
    #[must_use]
    pub fn pending(&self) -> &[PendingCommit] {
        &self.pending
    }

    /// The tags a release pattern matched whose names hold no version.
    #[must_use]
    pub fn unversioned(&self) -> &[UnversionedTag] {
        &self.unversioned
    }

    /// Releases the patterns selected past `max_revisions`, which the store
    /// does not hold.
    #[must_use]
    pub const fn releases_past_bound(&self) -> usize {
        self.releases_past_bound
    }
}

/// One analyzed commit: the rows it writes, and the bytes its analysis
/// parsed, which bound the batch it joins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnalyzedCommit {
    record: CommitRecord,
    parsed_bytes: u64,
}

impl AnalyzedCommit {
    /// The rows the commit writes.
    #[must_use]
    pub const fn record(&self) -> &CommitRecord {
        &self.record
    }

    /// Consumes the analysis into the rows it writes.
    #[must_use]
    pub fn into_record(self) -> CommitRecord {
        self.record
    }

    /// The bytes both sides of the commit's changed paths parsed.
    #[must_use]
    pub const fn parsed_bytes(&self) -> u64 {
        self.parsed_bytes
    }
}

/// The tag patterns a `selective` strategy names, compiled once.
#[derive(Debug)]
struct ReleasePatterns {
    patterns: Vec<(String, globset::GlobMatcher)>,
}

impl ReleasePatterns {
    /// Compiles each pattern.
    fn compile(patterns: &[String]) -> Result<Self, RiftError> {
        let compiled = patterns
            .iter()
            .map(|pattern| {
                release_matcher(pattern)
                    .map(|matcher| (pattern.clone(), matcher))
                    .map_err(|error| {
                        errors::server::read_invalid()
                            .field("providers.history.releases")
                            .violation(format!("{pattern} is no tag pattern: {error}"))
                            .error()
                    })
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { patterns: compiled })
    }

    /// The version `tag` spells under the first pattern it matches; `None`
    /// when it matches none. A matched tag whose remainder holds no version
    /// answers the remainder the parse refused.
    fn version(&self, tag: &str) -> Option<Result<semver::Version, String>> {
        let (pattern, _) = self
            .patterns
            .iter()
            .find(|(_, matcher)| matcher.is_match(tag))?;
        Some(release_version(tag, pattern))
    }
}

/// Compiles one release pattern into the matcher a `selective` fill tests tag
/// names against. Acceptance of `rift.toml` compiles every pattern through it
/// too, so a fill never meets a pattern it cannot compile.
///
/// # Errors
///
/// Returns globset's refusal when `pattern` is no glob, such as the unclosed
/// class `v[1`.
pub(crate) fn release_matcher(pattern: &str) -> Result<globset::GlobMatcher, globset::Error> {
    globset::Glob::new(pattern).map(|glob| glob.compile_matcher())
}

/// The version one tag name spells under the pattern that matched it: the
/// pattern's literal text up to its first digit or wildcard is stripped from
/// the name, and the rest parses as a semantic version. `1.*` leaves
/// `1.80.0` whole, `v0.0.*` strips `v`, and `tokio-*-alpha*` strips
/// `tokio-`.
///
/// # Errors
///
/// Returns the remainder when it is no semantic version, such as the
/// two-part `0.10`.
pub fn release_version(tag: &str, pattern: &str) -> Result<semver::Version, String> {
    let literal_end = pattern.find(WILDCARD_OPENERS).unwrap_or(pattern.len());
    let literal = &pattern[..literal_end];
    let prefix_end = literal
        .find(|character: char| character.is_ascii_digit())
        .unwrap_or(literal.len());
    let prefix = &literal[..prefix_end];
    let remainder = tag.strip_prefix(prefix).unwrap_or(tag);
    semver::Version::parse(remainder).map_err(|_| remainder.to_owned())
}

/// The workspace policy one fill analyzes commits under, with the
/// repository it reads them from.
///
/// A repository handle is not `Send`, so each plan and each analysis opens
/// the repository again - about a millisecond beside the parses one commit
/// runs - and the analysis itself moves between the blocking threads a fill
/// runs on.
#[derive(Debug)]
pub struct HistoryAnalysis {
    root: PathBuf,
    common_directory: PathBuf,
    visible: RevisionPaths,
    language: WorkspaceLanguagePolicy,
    syntax: SyntaxLimits,
    strategy: HistoryStrategy,
    revisions_max: usize,
    releases: Option<ReleasePatterns>,
    move_deletions_max: usize,
}

impl HistoryAnalysis {
    /// Opens the repository that versions `root` and compiles the policy one
    /// fill analyzes under: the `[source]` visibility, the language entries,
    /// the syntax bounds, and the `[providers.history]` strategy.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when no repository versions `root`, a
    /// `[source]` or language pattern does not compile, or a release
    /// pattern is no tag pattern.
    pub fn open(
        root: &Path,
        history: &HistoryConfiguration,
        (visibility, text_inclusion, languages): (
            &SourceVisibility,
            &TextFileInclusion,
            &LanguageFileSelections,
        ),
        syntax: SyntaxLimits,
    ) -> Result<Self, RiftError> {
        let repository = Repository::open(root)?;
        let visible = RevisionPaths::build(repository.root(), visibility)?;
        let language =
            WorkspaceLanguagePolicy::build(repository.root(), languages, text_inclusion)?;
        let releases = match history.strategy {
            HistoryStrategy::Everything => None,
            HistoryStrategy::Selective => Some(ReleasePatterns::compile(&history.releases)?),
        };
        let revisions_max =
            usize::try_from(history.max_revisions.min(HISTORY_REVISIONS_MAX)).unwrap_or(usize::MAX);
        Ok(Self {
            root: repository.root().to_path_buf(),
            common_directory: repository.common_directory().to_path_buf(),
            visible,
            language,
            syntax,
            strategy: history.strategy,
            revisions_max,
            releases,
            move_deletions_max: MOVE_DELETIONS_MAX,
        })
    }

    /// The same analysis pairing moves in commits with at most
    /// `move_deletions_max` deleted files.
    #[cfg(test)]
    pub(crate) const fn with_move_deletions_max(mut self, move_deletions_max: usize) -> Self {
        self.move_deletions_max = move_deletions_max;
        self
    }

    /// The common git directory the history store of this repository lives
    /// in.
    #[must_use]
    pub fn common_directory(&self) -> PathBuf {
        self.common_directory.clone()
    }

    /// The repository, opened for one plan or one analysis.
    fn repository(&self) -> Result<Repository, RiftError> {
        Repository::open(&self.root)
    }

    /// What the store owes the strategy, given what it `held`.
    ///
    /// Under `everything` the store keeps the union of every live worktree's
    /// newest `max_revisions` first-parent commits, this workspace's window
    /// first. Under `selective` it keeps the newest `max_revisions` releases
    /// in version order, each compared with the release before it.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the repository cannot be read.
    pub fn plan(&self, held: &HashMap<String, HeldCommit>) -> Result<FillPlan, RiftError> {
        let selected = match (&self.strategy, &self.releases) {
            (HistoryStrategy::Selective, Some(releases)) => self.selected_releases(releases)?,
            _ => self.selected_windows()?,
        };
        let mut plan = selected;
        plan.pending
            .retain(|pending| held.get(&pending.revision.commit_id()) != Some(&pending.held()));
        Ok(plan)
    }

    /// Every live worktree's window, each commit once, newest first.
    fn selected_windows(&self) -> Result<FillPlan, RiftError> {
        let repository = self.repository()?;
        let heads = repository.live_heads()?;
        let mut plan = FillPlan::default();
        for head in &heads {
            let window = repository.first_parent_window(head, self.revisions_max)?;
            for commit in window {
                if !plan.keep.insert(commit.revision().commit_id()) {
                    continue;
                }
                plan.pending.push(PendingCommit {
                    revision: commit.revision().clone(),
                    base: commit.parent().cloned(),
                    boundary: commit.is_boundary(),
                });
            }
        }
        Ok(plan)
    }

    /// The newest `max_revisions` releases `releases` selects, each compared
    /// with the one before it in version order, the oldest with nothing.
    fn selected_releases(&self, releases: &ReleasePatterns) -> Result<FillPlan, RiftError> {
        let tagged = self.repository()?.tagged_commits(RELEASE_TAGS_MAX)?;
        let mut plan = FillPlan::default();
        let mut versioned: Vec<(semver::Version, String, ResolvedRevision)> = Vec::new();
        for tag in tagged {
            match releases.version(tag.name()) {
                None => {}
                Some(Ok(version)) => {
                    versioned.push((version, tag.name().to_owned(), tag.revision().clone()));
                }
                Some(Err(remainder)) => plan.unversioned.push(UnversionedTag {
                    name: tag.name().to_owned(),
                    remainder,
                }),
            }
        }
        versioned.sort_by(|left, right| (&left.0, &left.1).cmp(&(&right.0, &right.1)));
        let mut seen: HashSet<String> = HashSet::new();
        versioned.retain(|(_, _, revision)| seen.insert(revision.commit_id()));
        plan.releases_past_bound = versioned.len().saturating_sub(self.revisions_max);
        let kept = &versioned[plan.releases_past_bound..];
        let mut base: Option<&ResolvedRevision> = None;
        for (_, _, revision) in kept {
            plan.keep.insert(revision.commit_id());
            plan.pending.push(PendingCommit {
                revision: revision.clone(),
                base: base.cloned(),
                boundary: base.is_none(),
            });
            base = Some(revision);
        }
        plan.pending.reverse();
        Ok(plan)
    }

    /// Analyzes one pending commit into the rows it writes. `cancelled` is
    /// asked between changed paths, and a `true` ends the analysis with
    /// `None`, so a stopping server waits on one path's parse at most.
    ///
    /// A commit compared with nothing writes its facts and no path. A commit
    /// whose changed paths pass `REVISION_TREE_ENTRIES_MAX` writes none of
    /// them and is marked a boundary: a timeline through it cannot tell what
    /// it changed.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the repository cannot be read.
    pub fn analyze(
        &self,
        pending: &PendingCommit,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Option<AnalyzedCommit>, RiftError> {
        let repository = self.repository()?;
        let facts = repository.commit_facts(&pending.revision)?;
        let mut record = CommitRecord {
            id: pending.revision.commit_id(),
            base: pending.base.as_ref().map(ResolvedRevision::commit_id),
            boundary: pending.boundary,
            author: CommitAuthor {
                name: facts.author_name().to_owned(),
                email: facts.author_email().to_owned(),
            },
            committed_at: facts.committed_at().to_owned(),
            time: facts.committed_seconds(),
            message: facts.message().to_owned(),
            paths: Vec::new(),
            renames: Vec::new(),
            moves: Vec::new(),
            declarations: Vec::new(),
        };
        let compared_with_nothing = pending.base.is_none() && pending.boundary;
        if compared_with_nothing {
            return Ok(Some(AnalyzedCommit {
                record,
                parsed_bytes: 0,
            }));
        }
        let includes = |path: &str| self.records(path);
        let changed = repository.changed_blobs(
            pending.base.as_ref(),
            &pending.revision,
            &includes,
            REVISION_TREE_ENTRIES_MAX,
        )?;
        if changed.is_truncated() {
            record.boundary = true;
            return Ok(Some(AnalyzedCommit {
                record,
                parsed_bytes: 0,
            }));
        }
        record.paths = changed.blobs().iter().map(changed_path).collect();
        record.renames =
            rift_core::traced!(component = "history", operation = "history.renames", {
                ChangedPath::pure_renames(&record.paths)
            });
        let renamed: HashSet<&str> = record
            .renames
            .iter()
            .flat_map(|renamed| [renamed.old_path.as_str(), renamed.new_path.as_str()])
            .collect();
        let mut parsed_bytes = 0_u64;
        let mut declarations = Vec::new();
        let mut candidates = MoveCandidates::default();
        for blob in changed.blobs() {
            if cancelled() {
                return Ok(None);
            }
            if renamed.contains(blob.path()) {
                continue;
            }
            let parsed =
                self.classify_path(&repository, blob, &mut declarations, &mut candidates)?;
            parsed_bytes = parsed_bytes.saturating_add(parsed);
        }
        let deletions = changed
            .blobs()
            .iter()
            .filter(|blob| blob.new_blob().is_none() && !renamed.contains(blob.path()))
            .count();
        if deletions <= self.move_deletions_max {
            record.moves =
                rift_core::traced!(component = "history", operation = "history.moves", {
                    candidates.pair(&mut declarations)
                });
        }
        record.declarations = declarations;
        Ok(Some(AnalyzedCommit {
            record,
            parsed_bytes,
        }))
    }

    /// Whether the store records a changed `path`: a visible path the search index
    /// would read, so a lockfile `[search.text].excluded_lockfiles` names writes no row,
    /// as it stores none in the search index.
    fn records(&self, path: &str) -> bool {
        self.visible.includes(path) && !self.language.excludes_lockfile(Path::new(path))
    }

    /// Classifies every declaration one changed path holds differently on its
    /// two sides, and answers the bytes the two parses read. An added file's
    /// declarations and a deleted file's join `candidates`, which pair the
    /// ones that moved between files. A path the language policy gives no
    /// provider parses nothing.
    fn classify_path(
        &self,
        repository: &Repository,
        blob: &ChangedBlob,
        declarations: &mut Vec<DeclarationChange>,
        candidates: &mut MoveCandidates,
    ) -> Result<u64, RiftError> {
        let Ok(path) = ProjectPath::new(blob.path().to_owned()) else {
            return Ok(0);
        };
        let Some(provider) = self.language.syntax_provider_for(Path::new(blob.path()))? else {
            return Ok(0);
        };
        let older = self.side(repository, provider, &path, blob.old_blob())?;
        let newer = self.side(repository, provider, &path, blob.new_blob())?;
        match (blob.old_blob(), blob.new_blob()) {
            (None, Some(_)) => candidates.added.extend(newer.move_keys(blob.path())),
            (Some(_), None) => candidates.removed.extend(older.move_keys(blob.path())),
            _ => {}
        }
        for name in older.names().union(&newer.names()) {
            if let Some(change) = classify(&older.state(name), &newer.state(name)) {
                declarations.push(DeclarationChange {
                    path: blob.path().to_owned(),
                    qualified_name: (*name).to_owned(),
                    change,
                });
            }
        }
        Ok(older.parsed_bytes.saturating_add(newer.parsed_bytes))
    }

    /// One side of a changed path: absent without a blob, unknown when its
    /// bytes cannot be analyzed, parsed otherwise.
    fn side(
        &self,
        repository: &Repository,
        provider: &dyn SyntaxProvider,
        path: &ProjectPath,
        blob: Option<&TreeFile>,
    ) -> Result<ParsedSide, RiftError> {
        let Some(blob) = blob else {
            return Ok(ParsedSide::absent());
        };
        let bytes = match repository.blob_bytes(blob, self.syntax.source_bytes_max()) {
            Ok(bytes) => bytes,
            Err(error) if error.slug() == errors::history::blob_too_large::SLUG => {
                return Ok(ParsedSide::unknown(0));
            }
            Err(error) => return error.fail(),
        };
        let parsed_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let Ok(text) = String::from_utf8(bytes) else {
            return Ok(ParsedSide::unknown(parsed_bytes));
        };
        let Ok(document) = provider.analyze(SyntaxSource { path, text: &text }, self.syntax) else {
            return Ok(ParsedSide::unknown(parsed_bytes));
        };
        let mut shapes: BTreeMap<String, (&'static str, SymbolShape)> = BTreeMap::new();
        for symbol in document.symbols() {
            shapes
                .entry(symbol.qualified_name.clone())
                .or_insert_with(|| (symbol.kind, SymbolShape::from_source(&text, symbol)));
        }
        Ok(ParsedSide {
            shapes: Some(shapes),
            parsed_bytes,
        })
    }
}

/// One side of a changed path as the classifier sees it: the declarations
/// its parse holds by qualified name, the first one under a name as a symbol
/// timeline looks it up, or `None` when the side cannot be analyzed.
struct ParsedSide {
    shapes: Option<BTreeMap<String, (&'static str, SymbolShape)>>,
    parsed_bytes: u64,
}

impl ParsedSide {
    /// A side the commit does not hold: every declaration is absent.
    const fn absent() -> Self {
        Self {
            shapes: Some(BTreeMap::new()),
            parsed_bytes: 0,
        }
    }

    /// A side whose bytes read `parsed_bytes` and answer nothing provable.
    const fn unknown(parsed_bytes: u64) -> Self {
        Self {
            shapes: None,
            parsed_bytes,
        }
    }

    /// The qualified names this side holds.
    fn names(&self) -> BTreeSet<&str> {
        self.shapes
            .iter()
            .flat_map(|shapes| shapes.keys().map(String::as_str))
            .collect()
    }

    /// The state of the declaration `name` on this side.
    fn state(&self, name: &str) -> SymbolState {
        match &self.shapes {
            None => SymbolState::Unknown,
            Some(shapes) => shapes.get(name).map_or(SymbolState::Absent, |(_, shape)| {
                SymbolState::Present(shape.clone())
            }),
        }
    }

    /// Every declaration this side holds at `path`, keyed the way a move
    /// pairs them.
    fn move_keys(&self, path: &str) -> Vec<MoveCandidate> {
        self.shapes
            .iter()
            .flatten()
            .map(|(name, (kind, shape))| MoveCandidate {
                key: (name.clone(), *kind, shape.clone()),
                path: path.to_owned(),
            })
            .collect()
    }
}

/// The key a moved declaration keeps on both sides: its qualified name, its
/// kind word, and the exact bytes it is written in - the key a comparison's
/// move pairing uses.
type MoveKey = (String, &'static str, SymbolShape);

/// One declaration an added or a deleted file holds.
struct MoveCandidate {
    key: MoveKey,
    path: String,
}

/// The declarations one commit's added files and deleted files hold.
#[derive(Default)]
struct MoveCandidates {
    added: Vec<MoveCandidate>,
    removed: Vec<MoveCandidate>,
}

impl MoveCandidates {
    /// Pairs each key exactly one addition and exactly one removal hold, in
    /// two paths, into one move; the addition's `introduced` change becomes
    /// `moved`. Any other key moves nothing, since no evidence says which
    /// addition answers which removal.
    fn pair(self, declarations: &mut [DeclarationChange]) -> Vec<MovedDeclaration> {
        let mut groups: HashMap<&MoveKey, (Vec<&str>, Vec<&str>)> = HashMap::new();
        for added in &self.added {
            groups.entry(&added.key).or_default().0.push(&added.path);
        }
        for removed in &self.removed {
            groups
                .entry(&removed.key)
                .or_default()
                .1
                .push(&removed.path);
        }
        let mut moves: Vec<MovedDeclaration> = groups
            .into_iter()
            .filter_map(
                |(key, (added, removed))| match (added.as_slice(), removed.as_slice()) {
                    ([new_path], [old_path]) if new_path != old_path => Some(MovedDeclaration {
                        old_path: (*old_path).to_owned(),
                        new_path: (*new_path).to_owned(),
                        qualified_name: key.0.clone(),
                    }),
                    _ => None,
                },
            )
            .collect();
        moves.sort_by(|left, right| {
            (&left.new_path, &left.qualified_name).cmp(&(&right.new_path, &right.qualified_name))
        });
        for moved in &moves {
            let introduced = declarations.iter_mut().find(|change| {
                change.path == moved.new_path
                    && change.qualified_name == moved.qualified_name
                    && change.change == SymbolVersionKind::Introduced
            });
            if let Some(change) = introduced {
                change.change = SymbolVersionKind::Moved;
            }
        }
        moves
    }
}

/// One changed blob as the store records it.
fn changed_path(blob: &ChangedBlob) -> ChangedPath {
    ChangedPath {
        path: blob.path().to_owned(),
        old_blob: blob.old_blob().map(TreeFile::blob_id),
        new_blob: blob.new_blob().map(TreeFile::blob_id),
    }
}

#[cfg(test)]
mod tests;
