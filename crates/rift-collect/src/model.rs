//! Source admission and observations shared by semantic providers.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::future::Future;
use std::sync::Arc;

use rift_core::{DeclarationBinding, FileDigest, ProviderId, SourceRange, SourceUnitId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Maximum UTF-8 bytes in one admitted source.
pub const SOURCE_BYTES_MAX: usize = 1 << 20;
/// Maximum number of units in one inventory or selection.
pub const SOURCE_COUNT_MAX: usize = 20_000;
/// Maximum observations in one batch.
pub const OBSERVATIONS_MAX: usize = 4_096;
/// Maximum distinct supporting units read by one batch.
pub const DEPENDENCIES_MAX: usize = 512;

const INVENTORY_BYTES_MAX: usize = 128 << 20;

/// A source or observation failed admission.
#[derive(Debug, thiserror::Error)]
pub enum CollectionError {
    /// A source inventory limit or invariant was violated.
    #[error("collection input: {0}")]
    Input(String),
    /// A batch or its source dependencies failed validation.
    #[error("collection observation: {0}")]
    Observation(String),
}

/// Stable collector identity and exact implementation and context revisions.
#[derive(Clone, Debug)]
pub struct Producer {
    id: ProviderId,
    revision: FileDigest,
    context: FileDigest,
}

impl Producer {
    /// Identifies the producer and the environment its facts depend on.
    ///
    /// The context covers configuration, engines, dependencies and other inputs outside the
    /// inventory. The revision covers collector code and any normalization outside this crate.
    #[must_use]
    pub const fn new(id: ProviderId, revision: FileDigest, context: FileDigest) -> Self {
        Self {
            id,
            revision,
            context,
        }
    }

    /// Stable producer identity.
    #[must_use]
    pub const fn id(&self) -> &ProviderId {
        &self.id
    }

    /// Exact collector implementation revision.
    #[must_use]
    pub const fn revision(&self) -> FileDigest {
        self.revision
    }

    /// Exact revision of the producer's context outside the inventory.
    #[must_use]
    pub const fn context(&self) -> FileDigest {
        self.context
    }
}

/// One independently collectible category.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// Call sites and possible targets.
    Calls,
    /// Source references.
    References,
    /// Imports and their possible targets.
    Imports,
}

impl Category {
    /// Stable name of this category.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Calls => "calls",
            Self::References => "references",
            Self::Imports => "imports",
        }
    }
}

/// How much of the requested category the producer inspected.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    /// The producer inspected the selected source for this category.
    Complete,
    /// The producer returned useful observations and still owes work.
    Partial,
    /// The producer does not support this selection.
    Unsupported,
}

/// What the observation establishes about its target.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    /// The authored occurrence was identified by syntax.
    Syntax,
    /// No target was established.
    Unresolved,
    /// The target is a possible match under the producer's analysis.
    Candidate,
    /// The producer established a binding to this target.
    Resolution,
}

/// Half-open UTF-8 byte offsets as transferred by a collector.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ByteRange {
    /// First byte.
    pub start: u64,
    /// One past the final byte.
    pub end: u64,
}

impl ByteRange {
    fn validate(self, text: &str) -> Result<SourceRange, CollectionError> {
        let index = usize::try_from(self.start)
            .ok()
            .zip(usize::try_from(self.end).ok());
        if index.is_none_or(|(start, end)| text.get(start..end).is_none()) {
            return Err(CollectionError::Observation(
                "range falls outside admitted UTF-8 boundaries".into(),
            ));
        }
        SourceRange::new(self.start, self.end)
            .map_err(|error| CollectionError::Observation(error.to_string()))
    }
}

/// Physical target returned by a collector.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceAnchor {
    /// Canonical Rift source-unit address.
    pub unit: String,
    /// Lowercase SHA-256 digest of the bytes the producer inspected.
    pub source_sha256: String,
    /// Exact range in the source read through the collection input.
    pub range: ByteRange,
}

/// One original producer observation. A batch preserves distinct IDs at the same location.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    /// Producer's original ID within the batch.
    pub id: String,
    /// Occurrence within the selected source.
    pub occurrence: ByteRange,
    /// Enclosing declaration when the producer identified one.
    pub enclosing: Option<ByteRange>,
    /// Source-backed target when available.
    pub target: Option<SourceAnchor>,
    /// What the producer established.
    pub evidence: Evidence,
}

/// Observations and coverage for one producer, source and category.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Batch {
    /// Coverage of the requested category. Complete coverage can include unresolved sites.
    pub coverage: Coverage,
    /// Original observations in producer order.
    pub observations: Vec<Observation>,
}

/// An immutable UTF-8 source with its parser filename and content digest.
#[derive(Debug)]
pub struct CollectionSource {
    unit: SourceUnitId,
    filename: String,
    text: Arc<str>,
    digest: FileDigest,
    sha256: String,
    revision: FileDigest,
}

impl CollectionSource {
    /// Admits source bytes and a parser filename independent of the source identity.
    ///
    /// # Errors
    ///
    /// Returns an input error for an empty or oversized filename or oversized source.
    pub fn new(
        unit: SourceUnitId,
        filename: String,
        text: Arc<str>,
    ) -> Result<Self, CollectionError> {
        if text.len() > SOURCE_BYTES_MAX || filename.is_empty() || filename.len() > 1_000 {
            return Err(CollectionError::Input(
                "source exceeds its byte or filename bound".into(),
            ));
        }
        let digest = FileDigest::of(text.as_bytes());
        let mut key = digest.as_bytes().to_vec();
        key.extend_from_slice(filename.as_bytes());
        let revision = FileDigest::of(&key);
        let mut sha256 = String::with_capacity(64);
        for byte in digest.as_bytes() {
            let _ = write!(&mut sha256, "{byte:02x}");
        }
        Ok(Self {
            unit,
            filename,
            text,
            digest,
            sha256,
            revision,
        })
    }

    /// Canonical source identity.
    #[must_use]
    pub const fn unit(&self) -> &SourceUnitId {
        &self.unit
    }

    /// Admitted filename used by the producer's parser.
    #[must_use]
    pub fn filename(&self) -> &str {
        &self.filename
    }

    /// Exact source bytes used to validate positions.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Full SHA-256 digest of the source bytes.
    #[must_use]
    pub const fn digest(&self) -> FileDigest {
        self.digest
    }

    /// Lowercase hexadecimal form of the full source digest.
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// Validates a physical declaration anchor against this source.
    ///
    /// # Errors
    ///
    /// Returns a protocol error for an empty, out-of-bounds or misaligned range.
    pub fn declaration(&self, range: ByteRange) -> Result<DeclarationBinding, CollectionError> {
        Ok(DeclarationBinding::new(
            self.unit.clone(),
            range.validate(&self.text)?,
        ))
    }

    /// Digest of the bytes and parser filename that govern collection reuse.
    #[must_use]
    pub const fn revision(&self) -> FileDigest {
        self.revision
    }
}

/// A bounded inventory of immutable source units.
#[derive(Debug, Default)]
pub struct Inventory {
    sources: BTreeMap<SourceUnitId, CollectionSource>,
    bytes: usize,
}

impl Inventory {
    /// Adds a source to the inventory.
    ///
    /// # Errors
    ///
    /// Returns an input error for duplicate units or an exhausted inventory bound.
    pub fn insert(&mut self, source: CollectionSource) -> Result<(), CollectionError> {
        if self.sources.contains_key(source.unit()) {
            return Err(CollectionError::Input(format!(
                "duplicate source {}",
                source.unit()
            )));
        }
        let bytes = self.bytes + source.text.len();
        if self.sources.len() == SOURCE_COUNT_MAX || bytes > INVENTORY_BYTES_MAX {
            return Err(CollectionError::Input(
                "source inventory exceeds its bound".into(),
            ));
        }
        self.bytes = bytes;
        self.sources.insert(source.unit.clone(), source);
        Ok(())
    }

    /// Looks up one admitted source.
    #[must_use]
    pub fn get(&self, unit: &SourceUnitId) -> Option<&CollectionSource> {
        self.sources.get(unit)
    }

    /// Iterates admitted units in canonical order.
    pub fn units(&self) -> impl Iterator<Item = &SourceUnitId> {
        self.sources.keys()
    }
}

/// Supporting source addresses and their input digests. An absent digest records a missing unit.
pub type SourceDependencies = BTreeMap<String, Option<[u8; 32]>>;

/// Source access for one collection request. Reads of absent units also invalidate reuse.
pub struct CollectionInput<'a> {
    inventory: &'a Inventory,
    selected: &'a CollectionSource,
    dependencies: SourceDependencies,
}

impl<'a> CollectionInput<'a> {
    /// Selects one source from the inventory and starts tracking its dependencies.
    ///
    /// # Errors
    ///
    /// Returns an input error when the selected unit is absent.
    pub fn new(inventory: &'a Inventory, unit: &SourceUnitId) -> Result<Self, CollectionError> {
        let selected = inventory
            .get(unit)
            .ok_or_else(|| CollectionError::Input(format!("selected source is absent: {unit}")))?;
        Ok(Self {
            inventory,
            selected,
            dependencies: BTreeMap::new(),
        })
    }

    /// Dependencies observed so far, including physical targets validated by the input.
    #[must_use]
    pub const fn dependencies(&self) -> &SourceDependencies {
        &self.dependencies
    }

    /// Source being collected.
    #[must_use]
    pub const fn selected(&self) -> &'a CollectionSource {
        self.selected
    }

    /// Reads an admitted source and records the dependency for the next run.
    ///
    /// # Errors
    ///
    /// Returns an error for an exhausted dependency bound.
    pub fn read(
        &mut self,
        unit: &SourceUnitId,
    ) -> Result<Option<&'a CollectionSource>, CollectionError> {
        let source = self.inventory.get(unit);
        if unit != self.selected.unit() {
            let address = unit.to_string();
            if self.dependencies.len() == DEPENDENCIES_MAX
                && !self.dependencies.contains_key(&address)
            {
                return Err(CollectionError::Observation(
                    "collector exceeded its source dependency bound".into(),
                ));
            }
            self.dependencies
                .insert(address, source.map(|source| *source.revision().as_bytes()));
        }
        Ok(source)
    }

    /// Validates a batch against the inventory and records the physical target dependencies.
    ///
    /// # Errors
    ///
    /// Returns an observation error for invalid coordinates, evidence, IDs or coverage.
    pub fn validate(&mut self, batch: &Batch) -> Result<(), CollectionError> {
        use std::collections::BTreeSet;
        if batch.observations.len() > OBSERVATIONS_MAX {
            return Err(CollectionError::Observation(
                "collector exceeded its observation bound".into(),
            ));
        }
        if batch.coverage == Coverage::Unsupported && !batch.observations.is_empty() {
            return Err(CollectionError::Observation(
                "unsupported batch contains observations".into(),
            ));
        }
        let mut ids = BTreeSet::new();
        for observation in &batch.observations {
            if observation.id.is_empty()
                || observation.id.len() > 256
                || !ids.insert(&observation.id)
            {
                return Err(CollectionError::Observation(
                    "invalid or duplicate observation ID".into(),
                ));
            }
            let occurrence = observation.occurrence.validate(&self.selected.text)?;
            if let Some(enclosing) = observation.enclosing {
                let enclosing = enclosing.validate(&self.selected.text)?;
                if enclosing.start() > occurrence.start() || enclosing.end() < occurrence.end() {
                    return Err(CollectionError::Observation(
                        "enclosing range does not contain the occurrence".into(),
                    ));
                }
            }
            match (&observation.target, observation.evidence) {
                (Some(anchor), Evidence::Candidate | Evidence::Resolution) => {
                    let unit = SourceUnitId::parse(&anchor.unit)
                        .map_err(|error| CollectionError::Observation(error.to_string()))?;
                    let source = self.read(&unit)?.ok_or_else(|| {
                        CollectionError::Observation(
                            "target source is outside the inventory".into(),
                        )
                    })?;
                    if anchor.source_sha256 != source.sha256() {
                        return Err(CollectionError::Observation(
                            "target digest does not match admitted source".into(),
                        ));
                    }
                    source.declaration(anchor.range)?;
                }
                (None, Evidence::Syntax | Evidence::Unresolved) => {}
                _ => {
                    return Err(CollectionError::Observation(
                        "target and evidence disagree".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Direct and hosted collectors implement the same source and batch contract.
pub trait Collector {
    /// Error reported by this collector.
    type Error;

    /// Collects one category for the selected source.
    ///
    /// Implementations access other sources through the input and bind outside inputs to the
    /// producer context. A failure leaves this batch owed on the next run.
    ///
    /// # Errors
    ///
    /// Returns a collector failure or deadline error.
    fn collect(
        &mut self,
        input: &mut CollectionInput<'_>,
        category: Category,
    ) -> impl Future<Output = Result<Batch, Self::Error>>;
}
