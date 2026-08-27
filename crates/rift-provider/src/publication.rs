use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use rift_core::{
    Contribution, Error, ErrorCode, ErrorContext, ErrorName, Fault, ProviderId, ProviderRevision,
};
use serde::Serialize;

/// Default provider count bound for one publication set.
pub const PROVIDERS_MAX_DEFAULT: usize = 64;
/// Default Contribution count bound for one provider publication.
pub const CONTRIBUTIONS_PER_PROVIDER_MAX_DEFAULT: usize = 100_000;
/// Default Contribution count bound across one publication set.
pub const CONTRIBUTIONS_TOTAL_MAX_DEFAULT: usize = 500_000;

/// Bounds applied while provider publications are accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicationLimits {
    providers: usize,
    contributions_per_provider: usize,
    contributions_total: usize,
}

impl PublicationLimits {
    /// Constructs non-zero publication bounds.
    ///
    /// # Errors
    ///
    /// Returns [`PublicationError`] when any bound is zero.
    pub fn new(
        providers_max: usize,
        contributions_per_provider_max: usize,
        contributions_total_max: usize,
    ) -> Result<Self, PublicationError> {
        if [
            providers_max,
            contributions_per_provider_max,
            contributions_total_max,
        ]
        .contains(&0)
        {
            return Err(publication_error(PublicationViolation::ZeroLimit, "limits"));
        }
        Ok(Self {
            providers: providers_max,
            contributions_per_provider: contributions_per_provider_max,
            contributions_total: contributions_total_max,
        })
    }

    /// Returns provider count bound.
    #[must_use]
    pub const fn providers_max(self) -> usize {
        self.providers
    }

    /// Returns per-provider Contribution count bound.
    #[must_use]
    pub const fn contributions_per_provider_max(self) -> usize {
        self.contributions_per_provider
    }

    /// Returns total Contribution count bound.
    #[must_use]
    pub const fn contributions_total_max(self) -> usize {
        self.contributions_total
    }
}

impl Default for PublicationLimits {
    fn default() -> Self {
        Self {
            providers: PROVIDERS_MAX_DEFAULT,
            contributions_per_provider: CONTRIBUTIONS_PER_PROVIDER_MAX_DEFAULT,
            contributions_total: CONTRIBUTIONS_TOTAL_MAX_DEFAULT,
        }
    }
}

/// One provider's immutable Contribution publication.
#[derive(Debug)]
pub struct ProviderPublication {
    provider: ProviderId,
    revision: ProviderRevision,
    contributions: Box<[Contribution]>,
}

impl ProviderPublication {
    /// Validates and constructs one provider publication.
    ///
    /// # Errors
    ///
    /// Returns [`PublicationError`] for a bound, mismatched key, or duplicate provider symbol.
    pub fn new(
        provider: ProviderId,
        revision: ProviderRevision,
        contributions: Vec<Contribution>,
        limits: PublicationLimits,
    ) -> Result<Self, PublicationError> {
        if contributions.len() > limits.contributions_per_provider {
            return Err(publication_error(
                PublicationViolation::ProviderContributionLimit,
                "contributions",
            ));
        }
        validate_keys(&provider, revision, &contributions)?;
        Ok(Self {
            provider,
            revision,
            contributions: contributions.into_boxed_slice(),
        })
    }

    /// Returns provider identity.
    #[must_use]
    pub const fn provider(&self) -> &ProviderId {
        &self.provider
    }

    /// Returns publication revision.
    #[must_use]
    pub const fn revision(&self) -> ProviderRevision {
        self.revision
    }

    /// Returns immutable Contributions in provider order.
    #[must_use]
    pub const fn contributions(&self) -> &[Contribution] {
        &self.contributions
    }
}

fn validate_keys(
    provider: &ProviderId,
    revision: ProviderRevision,
    contributions: &[Contribution],
) -> Result<(), PublicationError> {
    let mut symbols = BTreeSet::new();
    for contribution in contributions {
        let key = contribution.key();
        if key.reference().provider() != provider {
            return Err(publication_error(
                PublicationViolation::ProviderMismatch,
                "contribution.provider",
            ));
        }
        if key.publication() != revision {
            return Err(publication_error(
                PublicationViolation::RevisionMismatch,
                "contribution.publication",
            ));
        }
        if !symbols.insert(key.reference().symbol()) {
            return Err(publication_error(
                PublicationViolation::DuplicateProviderSymbol,
                "contribution.provider_symbol",
            ));
        }
    }
    Ok(())
}

/// One immutable set of captured provider publications.
#[derive(Debug)]
pub struct PublicationSet {
    limits: PublicationLimits,
    publications: BTreeMap<ProviderId, Arc<ProviderPublication>>,
    contribution_count: usize,
}

impl PublicationSet {
    /// Constructs an empty publication set.
    #[must_use]
    pub const fn empty(limits: PublicationLimits) -> Self {
        Self {
            limits,
            publications: BTreeMap::new(),
            contribution_count: 0,
        }
    }

    /// Returns another set with one provider publication replaced.
    ///
    /// Existing provider publications remain shared with the old set.
    ///
    /// # Errors
    ///
    /// Returns [`PublicationError`] when provider or total Contribution bound is crossed.
    pub fn replaced(&self, publication: ProviderPublication) -> Result<Self, PublicationError> {
        let replacing = self.publications.contains_key(publication.provider());
        if !replacing && self.publications.len() >= self.limits.providers {
            return Err(publication_error(
                PublicationViolation::ProviderLimit,
                "providers",
            ));
        }
        let old_count = self
            .publications
            .get(publication.provider())
            .map_or(0, |current| current.contributions.len());
        let without_old = self
            .contribution_count
            .checked_sub(old_count)
            .ok_or_else(|| {
                publication_error(PublicationViolation::ContributionLimit, "contributions")
            })?;
        let next_count = without_old
            .checked_add(publication.contributions.len())
            .ok_or_else(|| {
                publication_error(PublicationViolation::ContributionLimit, "contributions")
            })?;
        if next_count > self.limits.contributions_total {
            return Err(publication_error(
                PublicationViolation::ContributionLimit,
                "contributions",
            ));
        }
        let mut publications = self.publications.clone();
        publications.insert(publication.provider.clone(), Arc::new(publication));
        Ok(Self {
            limits: self.limits,
            publications,
            contribution_count: next_count,
        })
    }

    /// Returns provider publication.
    #[must_use]
    pub fn provider(&self, provider: &ProviderId) -> Option<&ProviderPublication> {
        self.publications.get(provider).map(Arc::as_ref)
    }

    /// Returns publications in provider identity order.
    pub fn publications(&self) -> impl ExactSizeIterator<Item = &ProviderPublication> {
        self.publications.values().map(Arc::as_ref)
    }

    /// Returns provider count.
    #[must_use]
    pub fn provider_count(&self) -> usize {
        self.publications.len()
    }

    /// Returns total Contribution count.
    #[must_use]
    pub const fn contribution_count(&self) -> usize {
        self.contribution_count
    }
}

/// Atomically published provider Contribution sets.
#[derive(Debug)]
pub struct PublicationStore {
    current: RwLock<Arc<PublicationSet>>,
}

impl PublicationStore {
    /// Constructs store over an empty publication set.
    #[must_use]
    pub fn new(limits: PublicationLimits) -> Self {
        Self {
            current: RwLock::new(Arc::new(PublicationSet::empty(limits))),
        }
    }

    /// Captures one immutable publication set.
    #[must_use]
    pub fn snapshot(&self) -> Arc<PublicationSet> {
        Arc::clone(&read_current(&self.current))
    }

    /// Atomically replaces one provider publication and returns new set.
    ///
    /// A reader retaining an earlier snapshot continues to read that complete set.
    ///
    /// # Errors
    ///
    /// Returns [`PublicationError`] when new set crosses a configured bound.
    pub fn replace(
        &self,
        publication: ProviderPublication,
    ) -> Result<Arc<PublicationSet>, PublicationError> {
        let mut current = write_current(&self.current);
        let next = Arc::new(current.replaced(publication)?);
        *current = Arc::clone(&next);
        Ok(next)
    }
}

fn read_current(lock: &RwLock<Arc<PublicationSet>>) -> RwLockReadGuard<'_, Arc<PublicationSet>> {
    match lock.read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn write_current(lock: &RwLock<Arc<PublicationSet>>) -> RwLockWriteGuard<'_, Arc<PublicationSet>> {
    match lock.write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Stable provider-publication refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationViolation {
    /// One publication bound is zero.
    ZeroLimit,
    /// Publication provider and Contribution provider differ.
    ProviderMismatch,
    /// Publication revision and Contribution revision differ.
    RevisionMismatch,
    /// One provider publication repeats a provider-local symbol.
    DuplicateProviderSymbol,
    /// Publication set exceeds provider count bound.
    ProviderLimit,
    /// One provider exceeds its Contribution count bound.
    ProviderContributionLimit,
    /// Publication set exceeds total Contribution count bound.
    ContributionLimit,
}

/// Provider publication refusal and field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationFault {
    violation: PublicationViolation,
    field: &'static str,
}

impl PublicationFault {
    /// Returns violated rule.
    #[must_use]
    pub const fn violation(&self) -> PublicationViolation {
        self.violation
    }
}

impl Fault for PublicationFault {
    fn name(&self) -> ErrorName {
        ErrorName::Wire(ErrorCode::InvalidRequest)
    }

    fn context(&self) -> Vec<ErrorContext> {
        vec![
            ErrorContext::new("field", self.field),
            ErrorContext::new("violation", rift_core::fault_label(&self.violation)),
        ]
    }
}

/// Invalid provider publication.
pub type PublicationError = Error<PublicationFault>;

fn publication_error(violation: PublicationViolation, field: &'static str) -> PublicationError {
    Error::new(PublicationFault { violation, field })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use rift_core::{
        Contribution, ContributionKey, ContributionOrigin, ExactKind, Language,
        PortableSymbolFacts, ProviderId, ProviderRevision, ProviderSymbolId, SourceApplicability,
        SourceKind,
    };

    use super::{ProviderPublication, PublicationLimits, PublicationStore, PublicationViolation};

    fn provider(value: &str) -> ProviderId {
        ProviderId::new(value).expect("provider")
    }

    fn revision(value: u64) -> ProviderRevision {
        ProviderRevision::new(value).expect("revision")
    }

    fn contribution(provider_id: &str, revision_id: u64, symbol: &str) -> Contribution {
        Contribution::builder(
            ContributionKey::new(
                provider(provider_id),
                revision(revision_id),
                ProviderSymbolId::new(symbol).expect("provider symbol"),
            ),
            SourceApplicability::Independent,
            PortableSymbolFacts::new(
                Language {
                    name: "rust".to_owned(),
                    dialect: None,
                },
                symbol,
                symbol,
                ExactKind("rust.struct".to_owned()),
            ),
            ContributionOrigin::new(None, SourceKind::Synthetic).expect("origin"),
        )
        .build()
        .expect("contribution")
    }

    #[test]
    fn publication_refuses_duplicate_provider_symbols() {
        let error = ProviderPublication::new(
            provider("syntax"),
            revision(1),
            vec![
                contribution("syntax", 1, "Beacon"),
                contribution("syntax", 1, "Beacon"),
            ],
            PublicationLimits::default(),
        )
        .expect_err("duplicate provider symbol");
        assert_eq!(
            error.fault().violation(),
            PublicationViolation::DuplicateProviderSymbol
        );
    }

    #[test]
    fn publication_limits_keys_and_accessors_are_enforced() {
        let error = PublicationLimits::new(0, 1, 1).expect_err("zero limit");
        assert_eq!(error.fault().violation(), PublicationViolation::ZeroLimit);
        assert_eq!(error.context()[0].key(), "field");
        assert!(error.to_string().contains("limits"));

        let limits = PublicationLimits::new(1, 1, 2).expect("limits");
        assert_eq!(limits.providers_max(), 1);
        assert_eq!(limits.contributions_per_provider_max(), 1);
        assert_eq!(limits.contributions_total_max(), 2);

        let publication = ProviderPublication::new(
            provider("syntax"),
            revision(1),
            vec![contribution("syntax", 1, "Beacon")],
            limits,
        )
        .expect("publication");
        assert_eq!(publication.provider(), &provider("syntax"));
        assert_eq!(publication.revision(), revision(1));
        assert_eq!(publication.contributions().len(), 1);

        let error = ProviderPublication::new(
            provider("syntax"),
            revision(1),
            vec![
                contribution("syntax", 1, "Beacon"),
                contribution("syntax", 1, "Other"),
            ],
            limits,
        )
        .expect_err("provider contribution bound");
        assert_eq!(
            error.fault().violation(),
            PublicationViolation::ProviderContributionLimit
        );

        let error = ProviderPublication::new(
            provider("syntax"),
            revision(1),
            vec![contribution("docs", 1, "Beacon")],
            limits,
        )
        .expect_err("provider mismatch");
        assert_eq!(
            error.fault().violation(),
            PublicationViolation::ProviderMismatch
        );

        let error = ProviderPublication::new(
            provider("syntax"),
            revision(1),
            vec![contribution("syntax", 2, "Beacon")],
            limits,
        )
        .expect_err("revision mismatch");
        assert_eq!(
            error.fault().violation(),
            PublicationViolation::RevisionMismatch
        );
    }

    #[test]
    fn publication_set_exposes_order_and_enforces_provider_bound() {
        let limits = PublicationLimits::new(1, 1, 2).expect("limits");
        let store = PublicationStore::new(limits);
        let empty = store.snapshot();
        assert_eq!(empty.provider_count(), 0);
        assert_eq!(empty.publications().count(), 0);
        assert!(empty.provider(&provider("syntax")).is_none());

        let publication = ProviderPublication::new(
            provider("syntax"),
            revision(1),
            vec![contribution("syntax", 1, "Beacon")],
            limits,
        )
        .expect("publication");
        let current = store.replace(publication).expect("replacement");
        assert_eq!(current.provider_count(), 1);
        assert_eq!(current.publications().count(), 1);

        let second = ProviderPublication::new(
            provider("docs"),
            revision(1),
            vec![contribution("docs", 1, "Other")],
            limits,
        )
        .expect("second publication");
        let error = store.replace(second).expect_err("provider bound");
        assert_eq!(
            error.fault().violation(),
            PublicationViolation::ProviderLimit
        );
    }

    #[test]
    fn poisoned_publication_lock_recovers_current_value() {
        let limits = PublicationLimits::default();
        let initial = Arc::new(super::PublicationSet::empty(limits));
        let lock = Arc::new(RwLock::new(initial));
        let poisoned = Arc::clone(&lock);
        std::thread::spawn(move || {
            let _guard = poisoned.write().expect("write");
            panic!("poison publication lock");
        })
        .join()
        .expect_err("thread panic");
        assert_eq!(super::read_current(&lock).provider_count(), 0);
        *super::write_current(&lock) = Arc::new(super::PublicationSet::empty(limits));
        assert_eq!(super::read_current(&lock).contribution_count(), 0);
    }

    #[test]
    fn store_replaces_one_provider_without_changing_retained_snapshot() {
        let store = PublicationStore::new(PublicationLimits::default());
        let first = ProviderPublication::new(
            provider("syntax"),
            revision(1),
            vec![contribution("syntax", 1, "Beacon")],
            PublicationLimits::default(),
        )
        .expect("first publication");
        let first_snapshot = store.replace(first).expect("first replacement");
        let second = ProviderPublication::new(
            provider("syntax"),
            revision(2),
            vec![contribution("syntax", 2, "Other")],
            PublicationLimits::default(),
        )
        .expect("second publication");
        let second_snapshot = store.replace(second).expect("second replacement");

        assert_eq!(
            first_snapshot
                .provider(&provider("syntax"))
                .map(ProviderPublication::revision),
            Some(revision(1))
        );
        assert_eq!(
            second_snapshot
                .provider(&provider("syntax"))
                .map(ProviderPublication::revision),
            Some(revision(2))
        );
        assert_eq!(store.snapshot().contribution_count(), 1);
    }

    #[test]
    fn total_contribution_bound_counts_replacement_delta() {
        let limits = PublicationLimits::new(2, 2, 2).expect("limits");
        let store = PublicationStore::new(limits);
        let syntax = ProviderPublication::new(
            provider("syntax"),
            revision(1),
            vec![contribution("syntax", 1, "A")],
            limits,
        )
        .expect("syntax");
        store.replace(syntax).expect("syntax replacement");
        let docs = ProviderPublication::new(
            provider("docs"),
            revision(1),
            vec![contribution("docs", 1, "B")],
            limits,
        )
        .expect("docs");
        store.replace(docs).expect("docs replacement");
        let replacement = ProviderPublication::new(
            provider("syntax"),
            revision(2),
            vec![
                contribution("syntax", 2, "A"),
                contribution("syntax", 2, "C"),
            ],
            limits,
        )
        .expect("replacement");
        let error = store
            .replace(replacement)
            .expect_err("replacement must cross total bound");
        assert_eq!(
            error.fault().violation(),
            PublicationViolation::ContributionLimit
        );
        assert_eq!(store.snapshot().contribution_count(), 2);
    }
}
