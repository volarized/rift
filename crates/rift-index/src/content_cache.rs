//! Shared content and syntax facts for workspace files.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, Weak};

use rift_core::FileDigest;
use rift_syntax::{SyntaxFacts, SyntaxLimits, SyntaxProvider, registry};

/// `cache.entry.count`: entries held by each bounded content cache.
static ENTRY_COUNT: rift_tracing::ObservableUpDownCounter<1> =
    rift_tracing::ObservableUpDownCounter::declare(
        crate::database_thread::SCOPE,
        "cache.entry.count",
        "{entry}",
        &["cache.name"],
    );

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ContentKey {
    digest: FileDigest,
    language_identity: usize,
    source_bytes_max: usize,
    syntax_nodes_max: usize,
    syntax_depth_max: usize,
    analyzer: usize,
}

#[derive(Debug)]
struct ContentEntry {
    source: Weak<String>,
    syntax: Weak<SyntaxFacts>,
}

#[derive(Debug, Default)]
struct ContentCacheState {
    entries: HashMap<ContentKey, ContentEntry>,
    insertions_since_prune: usize,
}

const CACHE_PRUNE_INTERVAL: usize = 1_024;

/// Weak references to source content and syntax facts shared by workspace builds.
///
/// Entries do not keep files alive after their workspace snapshots are dropped.
#[derive(Debug, Clone)]
pub struct WorkspaceContentCache {
    state: Arc<RwLock<ContentCacheState>>,
    entry_count: Arc<AtomicU64>,
    _entry_count_reading: Option<Arc<rift_tracing::ObservationGuard>>,
}

impl Default for WorkspaceContentCache {
    fn default() -> Self {
        let entry_count = Arc::new(AtomicU64::new(0));
        let observed_entry_count = Arc::downgrade(&entry_count);
        let entry_count_reading = ENTRY_COUNT
            .observe(move |observation| {
                if let Some(entry_count) = observed_entry_count.upgrade() {
                    observation.observe(
                        ["WorkspaceContentCache"],
                        entry_count.load(Ordering::Relaxed),
                    );
                }
            })
            .map(Arc::new);
        Self {
            state: Arc::new(RwLock::new(ContentCacheState::default())),
            entry_count,
            _entry_count_reading: entry_count_reading,
        }
    }
}

impl WorkspaceContentCache {
    pub(crate) fn get(
        &self,
        digest: FileDigest,
        limits: SyntaxLimits,
        provider: &dyn SyntaxProvider,
    ) -> (Option<Arc<String>>, Option<Arc<SyntaxFacts>>) {
        if !is_registered_provider(provider) {
            return (None, None);
        }
        let key = content_key(digest, limits, provider);
        let state = self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.entries.get(&key).map_or((None, None), |entry| {
            (entry.source.upgrade(), entry.syntax.upgrade())
        })
    }

    pub(crate) fn insert(
        &self,
        digest: FileDigest,
        limits: SyntaxLimits,
        provider: &dyn SyntaxProvider,
        source: &Arc<String>,
        syntax: &Arc<SyntaxFacts>,
        entries_max: usize,
    ) -> (Arc<String>, Arc<SyntaxFacts>) {
        if entries_max == 0 || !is_registered_provider(provider) {
            return (Arc::clone(source), Arc::clone(syntax));
        }
        let key = content_key(digest, limits, provider);
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.insertions_since_prune = state.insertions_since_prune.saturating_add(1);
        let prune_interval = entries_max
            .max(state.entries.capacity())
            .max(CACHE_PRUNE_INTERVAL);
        if state.entries.len() >= entries_max && state.insertions_since_prune >= prune_interval {
            state.entries.retain(|_, entry| {
                entry.source.strong_count() > 0 || entry.syntax.strong_count() > 0
            });
            state.insertions_since_prune = 0;
        }
        let shared_source = state
            .entries
            .get(&key)
            .and_then(|entry| entry.source.upgrade())
            .unwrap_or_else(|| Arc::clone(source));
        let shared_syntax = state
            .entries
            .get(&key)
            .and_then(|entry| entry.syntax.upgrade())
            .unwrap_or_else(|| Arc::clone(syntax));
        if state.entries.len() > entries_max {
            let mut retained_entries = 0;
            state.entries.retain(|candidate, _| {
                if candidate == &key {
                    true
                } else if retained_entries < entries_max.saturating_sub(1) {
                    retained_entries += 1;
                    true
                } else {
                    false
                }
            });
        }
        if !state.entries.contains_key(&key) {
            while state.entries.len() >= entries_max {
                if let Some(expired) = state.entries.keys().next().cloned() {
                    state.entries.remove(&expired);
                } else {
                    break;
                }
            }
        }
        state.entries.insert(
            key,
            ContentEntry {
                source: Arc::downgrade(&shared_source),
                syntax: Arc::downgrade(&shared_syntax),
            },
        );
        self.entry_count.store(
            u64::try_from(state.entries.len()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        (shared_source, shared_syntax)
    }

    #[cfg(test)]
    pub(crate) fn entry_count(&self) -> usize {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .len()
    }
}

fn content_key(
    digest: FileDigest,
    limits: SyntaxLimits,
    provider: &dyn SyntaxProvider,
) -> ContentKey {
    ContentKey {
        digest,
        language_identity: std::ptr::from_ref(provider.language()).cast::<()>() as usize,
        source_bytes_max: limits.source_bytes_max(),
        syntax_nodes_max: limits.syntax_nodes_max(),
        syntax_depth_max: limits.syntax_depth_max(),
        analyzer: std::ptr::from_ref(provider).cast::<()>() as usize,
    }
}

fn is_registered_provider(provider: &dyn SyntaxProvider) -> bool {
    registry::provider_for_language(provider.language())
        .is_some_and(|registered| std::ptr::eq(provider, registered))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_core::ProjectPath;
    use rift_syntax::{RustSyntaxProvider, SyntaxSource};

    #[test]
    fn custom_provider_with_shipped_language_does_not_reuse_registered_facts() {
        let registered = registry::provider_for_extension("rs").expect("Rust provider");
        let custom = RustSyntaxProvider::default();
        assert_eq!(registered.language(), custom.language());
        assert!(!is_registered_provider(&custom));

        let path = ProjectPath::new("src/lib.rs").expect("source path");
        let source = Arc::new("pub fn beacon() {}\n".to_owned());
        let syntax = registered
            .analyze(
                SyntaxSource {
                    path: &path,
                    text: &source,
                },
                SyntaxLimits::default(),
            )
            .expect("source parses")
            .into_facts();
        let digest = FileDigest::of(source.as_bytes());
        let cache = WorkspaceContentCache::default();
        cache.insert(
            digest,
            SyntaxLimits::default(),
            registered,
            &source,
            &syntax,
            4,
        );

        let (custom_source, custom_syntax) = cache.get(digest, SyntaxLimits::default(), &custom);
        assert!(custom_source.is_none());
        assert!(custom_syntax.is_none());
        let (registered_source, registered_syntax) =
            cache.get(digest, SyntaxLimits::default(), registered);
        assert!(registered_source.is_some());
        assert!(registered_syntax.is_some());
        assert_eq!(cache.entry_count(), 1);
    }

    #[test]
    fn lowering_entry_bound_trims_in_one_pass_and_keeps_requested_facts() {
        let provider = registry::provider_for_extension("rs").expect("Rust provider");
        let path = ProjectPath::new("src/cache.rs").expect("source path");
        let limits = SyntaxLimits::default();
        let cache = WorkspaceContentCache::default();
        let mut live = Vec::new();

        for index in 0..128 {
            let source = Arc::new(format!("pub fn beacon_{index}() {{}}\n"));
            let syntax = provider
                .analyze(
                    SyntaxSource {
                        path: &path,
                        text: &source,
                    },
                    limits,
                )
                .expect("source parses")
                .into_facts();
            let digest = FileDigest::of(source.as_bytes());
            cache.insert(digest, limits, provider, &source, &syntax, 128);
            live.push((digest, source, syntax));
        }
        assert_eq!(cache.entry_count(), 128);

        let (digest, source, syntax) = live.last().expect("last live source");
        let (shared_source, shared_syntax) =
            cache.insert(*digest, limits, provider, source, syntax, 8);

        assert_eq!(cache.entry_count(), 8);
        assert!(Arc::ptr_eq(source, &shared_source));
        assert!(Arc::ptr_eq(syntax, &shared_syntax));
        assert_eq!(shared_source.as_str(), "pub fn beacon_127() {}\n");
        assert_eq!(shared_syntax.symbols()[0].name, "beacon_127");
        let (reused_source, reused_syntax) = cache.get(*digest, limits, provider);
        assert!(Arc::ptr_eq(
            &reused_source.expect("cached source remains live"),
            source
        ));
        assert!(Arc::ptr_eq(
            &reused_syntax.expect("cached syntax remains live"),
            syntax
        ));
    }

    #[test]
    fn entry_count_observation_follows_cache_clones_and_bound()
    -> Result<(), rift_tracing::LogFilterError> {
        let (recorder, _drain) = rift_tracing::ScopedRecorder::builder().install()?;
        let provider = registry::provider_for_extension("rs").expect("Rust provider");
        let path = ProjectPath::new("src/cache.rs").expect("source path");
        let limits = SyntaxLimits::default();
        let cache = WorkspaceContentCache::default();
        let clone = cache.clone();
        let mut live = Vec::new();

        for index in 0..3 {
            let source = Arc::new(format!("pub fn beacon_{index}() {{}}\n"));
            let syntax = provider
                .analyze(
                    SyntaxSource {
                        path: &path,
                        text: &source,
                    },
                    limits,
                )
                .expect("source parses")
                .into_facts();
            cache.insert(
                FileDigest::of(source.as_bytes()),
                limits,
                provider,
                &source,
                &syntax,
                2,
            );
            live.push((source, syntax));
        }

        let labels = [("cache.name", "WorkspaceContentCache")];
        let held = recorder.metrics();
        let count = held
            .find("cache.entry.count", &labels)
            .expect("cache count is observed");
        assert_eq!(count.unit(), "{entry}");
        assert_eq!(count.scope_name(), env!("CARGO_PKG_NAME"));
        assert_eq!(count.value(), &rift_tracing::SeriesValue::Sum(2.0));

        drop(cache);
        let cloned = recorder.metrics();
        assert_eq!(
            cloned
                .find("cache.entry.count", &labels)
                .expect("clone keeps cache count observed")
                .value(),
            &rift_tracing::SeriesValue::Sum(2.0)
        );

        drop(clone);
        assert!(
            recorder
                .metrics()
                .find("cache.entry.count", &labels)
                .is_none(),
            "last cache clone removes its observation"
        );
        Ok(())
    }
}
