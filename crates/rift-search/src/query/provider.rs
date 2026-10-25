//! Captured provider topology associated with the existing query fact arrays.

mod retained_size;
mod wire;

use rift_core::{
    ContributionKey, DeclarationBinding, IndexRevision, ReferenceRole, SourceRevision, SymbolId,
    TreeRevision, WorkspaceId,
};
use rift_protocol::identity::{SourceDigest, SymbolOwner};
use rift_provider::NormalizedTarget;
use serde::{Deserialize, Serialize};

/// The original source owner and revisions of one normalized provider graph.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryProviderContext {
    /// Original serving publication and accepted configuration.
    pub selection: QueryProviderSelection,
    /// Original root URI supplied by the captured source owner.
    #[serde(with = "wire::workspace")]
    pub root: WorkspaceId,
    /// Physical source owner selected before analysis.
    pub owner: SymbolOwner,
    /// Original normalized index revision.
    #[serde(with = "wire::index_revision")]
    pub index_revision: IndexRevision,
    /// Original normalized source revision.
    #[serde(with = "wire::source_revision")]
    pub source_revision: SourceRevision,
    /// Original normalized tree revision.
    #[serde(with = "wire::tree_revision")]
    pub tree_revision: TreeRevision,
    /// Full digest of the source material admitted by this owner.
    pub source_digest: SourceDigest,
    /// Exact metadata inputs captured by the same source owner before analysis.
    pub inputs: Vec<QueryProviderInput>,
}

/// Original immutable selection serving one request-owned provider graph.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryProviderSelection {
    /// Original serving root URI.
    #[serde(with = "wire::workspace")]
    pub root: WorkspaceId,
    /// Fixed source or profile owner.
    pub owner: SymbolOwner,
    /// Original normalized index revision.
    #[serde(with = "wire::index_revision")]
    pub index_revision: IndexRevision,
    /// Original normalized source revision.
    #[serde(with = "wire::source_revision")]
    pub source_revision: SourceRevision,
    /// Original normalized tree revision.
    #[serde(with = "wire::tree_revision")]
    pub tree_revision: TreeRevision,
    /// Full captured source fingerprint.
    pub fingerprint: SourceDigest,
    /// Full accepted configuration fingerprint.
    pub configuration: SourceDigest,
}

impl QueryProviderSelection {
    /// Computes accepted configuration identity without reading its file again.
    ///
    /// # Errors
    /// Returns the registered configuration encoding failure.
    pub fn configuration_digest(
        configuration: &rift_protocol::configuration::WorkspaceConfiguration,
    ) -> Result<SourceDigest, rift_error::RiftError> {
        let bytes = rift_protocol::canonical::canonical_json(configuration).map_err(|error| {
            rift_error::errors::server::read_unavailable()
                .operation("query provider configuration")
                .detail(error.to_string())
                .error()
        })?;
        SourceDigest::parse(
            &data_encoding::HEXLOWER.encode(rift_core::FileDigest::of(bytes.as_bytes()).as_bytes()),
        )
    }
}

impl get_size2::GetSize for QueryProviderSelection {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (root, tracker) = get_size2::GetSize::get_heap_size_with_tracker(&self.root, tracker);
        let (owner, tracker) = get_size2::GetSize::get_heap_size_with_tracker(&self.owner, tracker);
        let (fingerprint, tracker) =
            get_size2::GetSize::get_heap_size_with_tracker(&self.fingerprint, tracker);
        let (configuration, tracker) =
            get_size2::GetSize::get_heap_size_with_tracker(&self.configuration, tracker);
        (
            root.saturating_add(owner)
                .saturating_add(fingerprint)
                .saturating_add(configuration),
            tracker,
        )
    }
}

/// An immutable source input contributing to the selected provider context.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryProviderInput {
    /// Original admitted source URI.
    pub uri: String,
    /// Full digest of its captured bytes.
    pub digest: SourceDigest,
}

/// Checked topology from one captured normalized owner, without another symbol payload copy.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryProviderSnapshot {
    /// The original source and provider context.
    pub context: QueryProviderContext,
    /// Original normalized record associations into the shared symbol array.
    pub records: Vec<QueryProviderRecord>,
    /// Source array entries owned by this captured graph.
    pub source_indices: Vec<usize>,
    /// Original references projected from this graph.
    pub references: Vec<QueryProviderReference>,
    /// Original relationships projected from this graph.
    pub relationships: Vec<QueryProviderRelationship>,
}

/// An original normalized record's keys and physical declaration associations.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryProviderRecord {
    /// Selected payload in `QueryFacts.symbols`, without duplicating its facts.
    pub symbol_index: usize,
    /// Original normalized identity, retaining graph-local identities as internal references.
    #[serde(with = "wire::optional_symbol")]
    pub identity: Option<SymbolId>,
    /// The original immutable provider Contribution keys.
    #[serde(with = "wire::keys")]
    pub contributions: Vec<ContributionKey>,
    /// Physical declarations associated with the selected symbol payload.
    pub declarations: Vec<QueryProviderDeclaration>,
    /// Original containing record, including a provider-local target when no canonical ID exists.
    #[serde(with = "wire::optional_target")]
    pub container: Option<NormalizedTarget>,
}

/// A supplied physical binding and its existing declaration association.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryProviderDeclaration {
    /// Entry in the selected symbol payload's declaration array.
    pub declaration_index: usize,
    /// Original provider Contribution supplying the declaration.
    #[serde(with = "wire::key")]
    pub contribution: ContributionKey,
    /// Original supplied unit, full byte range and optional node witness.
    #[serde(with = "wire::binding")]
    pub binding: DeclarationBinding,
}

/// An original semantic reference, retaining its role and supplied physical binding.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryProviderReference {
    /// Original provider Contribution supplying this reference.
    #[serde(with = "wire::key")]
    pub source: ContributionKey,
    /// Original physical occurrence.
    #[serde(with = "wire::binding")]
    pub binding: DeclarationBinding,
    /// Original reference classification.
    #[serde(with = "wire::role")]
    pub role: ReferenceRole,
    /// Portable facet projected by the existing reference owner.
    pub facet: rift_protocol::read::RelationshipFacet,
    /// Targets normalized by the original graph owner.
    #[serde(with = "wire::targets")]
    pub targets: Vec<NormalizedTarget>,
}

/// An original normalized relationship, retaining its evidence and occurrence.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryProviderRelationship {
    /// Original provider Contribution supplying the relationship.
    #[serde(with = "wire::key")]
    pub source: ContributionKey,
    /// Original relationship classification.
    #[serde(with = "wire::kind")]
    pub kind: rift_core::RelationshipKind,
    /// Portable facet projected by the existing normalized relationship owner.
    pub facet: rift_protocol::read::RelationshipFacet,
    /// Target normalized by the original graph owner.
    #[serde(with = "wire::target")]
    pub target: NormalizedTarget,
    /// How the provider established this relationship.
    pub derivation: Option<rift_protocol::read::RelationshipDerivation>,
    /// Exact physical occurrence when supplied by the provider.
    #[serde(with = "wire::optional_binding")]
    pub occurrence: Option<DeclarationBinding>,
}

/// A normalized target selected within its original captured provider context.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryProviderTarget {
    /// Captured normalized owner; absent only for an established canonical query subject.
    #[serde(with = "wire::optional_context")]
    pub context: Option<std::sync::Arc<QueryProviderContext>>,
    /// Existing normalized target, never interpreted as a canonical identity without its codec.
    #[serde(with = "wire::target")]
    pub target: NormalizedTarget,
}

/// An engine-established relationship between captured normalized targets.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryProviderEdge {
    /// Original caller or referencing declaration.
    pub from: Option<QueryProviderEndpoint>,
    /// Original called or referenced declaration.
    pub to: QueryProviderEndpoint,
    /// Relationship established by the selected provider.
    pub facet: rift_protocol::read::RelationshipFacet,
    /// Original physical occurrence, when supplied.
    pub occurrence: Option<rift_protocol::read::SourceUnitSpan>,
    /// Evidence supplied by the provider.
    pub derivation: Option<rift_protocol::read::RelationshipDerivation>,
}

/// A normalized target referring to its existing captured provider array entry.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryProviderEndpoint {
    /// Original provider context in `QueryFacts.providers`, absent for canonical subjects.
    pub provider: Option<usize>,
    /// Original normalized target.
    #[serde(with = "wire::target")]
    pub target: NormalizedTarget,
}

impl QueryProviderEndpoint {
    /// Returns a canonical subject only when this selection has no provider-local scope.
    #[must_use]
    pub fn canonical_symbol(&self) -> Option<rift_protocol::read::SymbolId> {
        if self.provider.is_some() {
            return None;
        }
        let NormalizedTarget::Symbol(identity) = &self.target else {
            return None;
        };
        rift_protocol::identity::SymbolIdentity::parse(identity.as_str()).ok()?;
        Some(rift_protocol::read::SymbolId(identity.as_str().to_owned()))
    }
}

impl QueryProviderContext {
    /// Returns the original target key without cloning captured context metadata.
    ///
    /// # Errors
    /// Returns the existing encoding failure for invalid captured target metadata.
    pub fn target_key(&self, target: &NormalizedTarget) -> Result<[u8; 32], rift_error::RiftError> {
        provider_target_key(Some(self), target)
    }
}

fn provider_target_key(
    context: Option<&QueryProviderContext>,
    target: &NormalizedTarget,
) -> Result<[u8; 32], rift_error::RiftError> {
    use sha2::{Digest, Sha256};
    #[derive(Serialize)]
    struct Identity<'target> {
        context: Option<&'target QueryProviderContext>,
        #[serde(serialize_with = "wire::target::serialize")]
        target: &'target NormalizedTarget,
    }
    let bytes = rift_protocol::canonical::canonical_json(&Identity { context, target }).map_err(
        |error| {
            rift_error::errors::server::read_invalid()
                .field("query")
                .violation(error.to_string())
                .error()
        },
    )?;
    Ok(Sha256::digest(bytes).into())
}

impl QueryProviderTarget {
    /// Returns a fixed-size key covering the complete original target and source context.
    ///
    /// # Errors
    /// Returns the existing encoding error if this captured target cannot be serialized.
    pub fn key(&self) -> Result<[u8; 32], rift_error::RiftError> {
        provider_target_key(self.context.as_deref(), &self.target)
    }
    /// Selects an already checked public canonical subject without a provider-local graph.
    ///
    /// # Errors
    /// Returns the existing identity error when the supplied subject is not canonical.
    pub fn canonical(
        identity: &rift_protocol::read::SymbolId,
    ) -> Result<Self, rift_error::RiftError> {
        rift_protocol::identity::SymbolIdentity::parse(identity.as_str())?;
        Ok(Self {
            context: None,
            target: NormalizedTarget::Symbol(SymbolId::new(identity.as_str())?),
        })
    }

    /// Returns a canonical subject only when the original selection has no provider-local scope.
    #[must_use]
    pub fn canonical_symbol(&self) -> Option<rift_protocol::read::SymbolId> {
        if self.context.is_some() {
            return None;
        }
        let NormalizedTarget::Symbol(identity) = &self.target else {
            return None;
        };
        rift_protocol::identity::SymbolIdentity::parse(identity.as_str()).ok()?;
        Some(rift_protocol::read::SymbolId(identity.as_str().to_owned()))
    }
}

impl QueryProviderRecord {
    /// Encodes original captured associations without payload array positions.
    ///
    /// # Errors
    /// Returns the serialization error when a supplied association cannot be encoded.
    pub fn key(&self, context: &QueryProviderContext) -> Result<Vec<u8>, serde_json::Error> {
        #[derive(Serialize)]
        struct Identity<'record> {
            context: &'record QueryProviderContext,
            #[serde(serialize_with = "wire::keys::serialize")]
            contributions: &'record [ContributionKey],
            declarations: Declarations<'record>,
        }
        struct Declarations<'record>(&'record [QueryProviderDeclaration]);
        impl Serialize for Declarations<'_> {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                #[derive(Serialize)]
                struct Binding<'record> {
                    #[serde(serialize_with = "wire::key::serialize")]
                    contribution: &'record ContributionKey,
                    #[serde(serialize_with = "wire::binding::serialize")]
                    binding: &'record DeclarationBinding,
                }
                serializer.collect_seq(self.0.iter().map(|declaration| Binding {
                    contribution: &declaration.contribution,
                    binding: &declaration.binding,
                }))
            }
        }
        serde_json::to_vec(&Identity {
            context,
            contributions: &self.contributions,
            declarations: Declarations(&self.declarations),
        })
    }
    /// Returns the existing normalized target of this captured record.
    #[must_use]
    pub fn target(&self) -> Option<NormalizedTarget> {
        self.identity
            .as_ref()
            .map(|identity| NormalizedTarget::Symbol(identity.clone()))
            .or_else(|| {
                self.contributions
                    .first()
                    .map(|key| NormalizedTarget::Contribution(key.reference().clone()))
            })
    }

    /// Returns whether the original normalized record owns this target.
    #[must_use]
    pub fn owns(&self, target: &NormalizedTarget) -> bool {
        match target {
            NormalizedTarget::Symbol(identity) => self.identity.as_ref() == Some(identity),
            NormalizedTarget::Contribution(reference) => self
                .contributions
                .iter()
                .any(|key| key.reference() == reference),
        }
    }
}

impl QueryProviderSnapshot {
    /// Finds the original normalized record without assigning another identity.
    #[must_use]
    pub fn record(&self, target: &NormalizedTarget) -> Option<&QueryProviderRecord> {
        self.records.iter().find(|record| record.owns(target))
    }
}

impl super::QueryFacts {
    /// Selects a worker endpoint using the existing captured provider array.
    #[must_use]
    pub fn provider_endpoint(&self, target: &QueryProviderTarget) -> Option<QueryProviderEndpoint> {
        let provider = if let Some(context) = &target.context {
            Some(
                self.providers
                    .iter()
                    .position(|snapshot| &snapshot.context == context.as_ref())?,
            )
        } else {
            target.canonical_symbol()?;
            None
        };
        Some(QueryProviderEndpoint {
            provider,
            target: target.target.clone(),
        })
    }

    /// Restores the original captured target for a request-owned frontier.
    #[must_use]
    pub fn provider_target(&self, endpoint: &QueryProviderEndpoint) -> Option<QueryProviderTarget> {
        let context = if let Some(index) = endpoint.provider {
            Some(std::sync::Arc::new(
                self.providers.get(index)?.context.clone(),
            ))
        } else {
            endpoint.canonical_symbol()?;
            None
        };
        Some(QueryProviderTarget {
            context,
            target: endpoint.target.clone(),
        })
    }
    /// Finds a supplied normalized target in its original captured graph.
    #[must_use]
    pub fn provider_record(
        &self,
        target: &QueryProviderTarget,
    ) -> Option<(&QueryProviderSnapshot, &QueryProviderRecord)> {
        let context = target.context.as_ref()?;
        let snapshot = self
            .providers
            .iter()
            .find(|snapshot| &snapshot.context == context.as_ref())?;
        Some((snapshot, snapshot.record(&target.target)?))
    }

    /// Returns the existing symbol payload selected by this normalized target.
    #[must_use]
    pub fn provider_symbol_index(&self, target: &QueryProviderTarget) -> Option<usize> {
        if let Some(identity) = target.canonical_symbol() {
            self.symbols
                .iter()
                .position(|symbol| symbol.symbol.id.as_ref() == Some(&identity))
        } else {
            self.provider_record(target)
                .map(|(_, record)| record.symbol_index)
        }
    }

    /// Validates captured provider associations against their existing source and symbol arrays.
    ///
    /// # Errors
    /// Returns the registered invalid-query error for absent or conflicting captured associations.
    pub fn validate_providers(&self) -> Result<(), rift_error::RiftError> {
        let invalid = || {
            rift_error::errors::server::read_invalid()
                .field("query")
                .violation(
                    "captured provider associations differ from their source or symbol records",
                )
                .error()
        };
        for snapshot in &self.providers {
            if snapshot.source_indices.is_empty()
                || !snapshot.source_indices.iter().any(|index| {
                    self.sources
                        .get(*index)
                        .and_then(|source| source.digest.as_ref())
                        == Some(&snapshot.context.source_digest)
                })
            {
                return Err(invalid());
            }
            let mut keys = std::collections::BTreeSet::new();
            for record in &snapshot.records {
                let symbol = self.symbols.get(record.symbol_index).ok_or_else(invalid)?;
                if record.contributions.is_empty() || record.declarations.is_empty() {
                    return Err(invalid());
                }
                for key in &record.contributions {
                    if !keys.insert(key) {
                        return Err(invalid());
                    }
                }
                for association in &record.declarations {
                    if !record.contributions.contains(&association.contribution) {
                        return Err(invalid());
                    }
                    let declaration = symbol
                        .declarations
                        .get(association.declaration_index)
                        .ok_or_else(invalid)?;
                    let unit = association.binding.unit().to_string();
                    let (_, path) = rift_protocol::identity::parse_source_unit_identity(&unit)?;
                    let matches_unit = match (&declaration.unit, &declaration.path) {
                        (Some(selected), None) => selected.as_str() == unit,
                        (None, Some(selected)) => selected.0 == path,
                        _ => false,
                    };
                    if !matches_unit
                        || declaration.range.start != association.binding.range().start()
                        || declaration.range.end != association.binding.range().end()
                        || declaration.node.as_ref() != association.binding.node()
                    {
                        return Err(invalid());
                    }
                    let source = snapshot
                        .source_indices
                        .iter()
                        .filter_map(|index| self.sources.get(*index))
                        .find(|source| source.unit.as_str() == unit)
                        .ok_or_else(invalid)?;
                    let (owner, _) =
                        rift_protocol::identity::parse_source_unit_identity(source.unit.as_str())?;
                    if owner
                        .as_ref()
                        .is_some_and(|owner| owner != &snapshot.context.owner)
                    {
                        return Err(invalid());
                    }
                    if let Some(text) = &source.text {
                        let start =
                            usize::try_from(declaration.range.start).map_err(|_| invalid())?;
                        let end = usize::try_from(declaration.range.end).map_err(|_| invalid())?;
                        if text.get(start..end).is_none() {
                            return Err(invalid());
                        }
                    }
                }
            }
        }
        for edge in &self.provider_edges {
            for target in edge.from.iter().chain(std::iter::once(&edge.to)) {
                if target.provider.is_none() && target.canonical_symbol().is_none() {
                    return Err(invalid());
                }
                if target.provider.is_some_and(|index| {
                    self.providers
                        .get(index)
                        .and_then(|snapshot| snapshot.record(&target.target))
                        .is_none()
                }) {
                    return Err(invalid());
                }
            }
            if edge.from.is_none() && edge.occurrence.is_none() {
                return Err(invalid());
            }
        }
        Ok(())
    }
}
