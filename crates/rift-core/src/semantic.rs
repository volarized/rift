//! Provider Contributions and normalized symbol records.

use std::collections::BTreeSet;

use rift_protocol::read::ExtensionKey;
pub use rift_protocol::read::{
    Documentation, DocumentationFormat, ExactKind, Extensions, Language, NodeId, Signature,
    SourceKind, SourceLocation, SymbolFacet, TypeBinding,
};
use serde::Serialize;

use crate::{
    Error, ErrorCode, ErrorContext, ErrorName, Fault, IndexRevision, ProviderId, ProviderRevision,
    ProviderSymbolId, SourceRevision, SourceUnitId, SymbolId, TreeRevision,
    is_canonical_ascii_name,
};

/// Maximum bytes in one provider-local symbol identity.
pub const PROVIDER_SYMBOL_ID_BYTES_MAX: usize = 8_192;
/// Maximum portable facts of one kind carried by one Contribution.
pub const CONTRIBUTION_FACTS_MAX: usize = 256;
/// Maximum equivalence evidence entries carried by one Contribution.
pub const CONTRIBUTION_EVIDENCE_MAX: usize = 64;
/// Maximum namespaced facts carried by one Contribution.
pub const CONTRIBUTION_NAMESPACES_MAX: usize = 64;
/// Maximum encoded bytes in one namespaced fact.
pub const CONTRIBUTION_NAMESPACE_BYTES_MAX: usize = 65_536;

/// Source revisions to which one Contribution applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceApplicability {
    /// Fact applies to one captured source catalog and tree.
    Exact {
        /// Source catalog revision the provider read.
        source_revision: SourceRevision,
        /// Project tree revision the provider read.
        tree_revision: TreeRevision,
    },
    /// Fact does not depend on source bytes.
    Independent,
}

impl SourceApplicability {
    /// Returns whether this applicability participates in captured revisions.
    #[must_use]
    pub const fn applies_to(
        self,
        source_revision: SourceRevision,
        tree_revision: TreeRevision,
    ) -> bool {
        match self {
            Self::Exact {
                source_revision: expected_source,
                tree_revision: expected_tree,
            } => {
                expected_source.get() == source_revision.get()
                    && expected_tree.get() == tree_revision.get()
            }
            Self::Independent => true,
        }
    }
}

/// Provider-local reference to one Contribution across publications.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContributionReference {
    provider: ProviderId,
    symbol: ProviderSymbolId,
}

impl ContributionReference {
    /// Constructs one provider-local Contribution reference.
    #[must_use]
    pub const fn new(provider: ProviderId, symbol: ProviderSymbolId) -> Self {
        Self { provider, symbol }
    }

    /// Returns provider identity.
    #[must_use]
    pub const fn provider(&self) -> &ProviderId {
        &self.provider
    }

    /// Returns provider-local symbol identity.
    #[must_use]
    pub const fn symbol(&self) -> &ProviderSymbolId {
        &self.symbol
    }
}

/// Identity of one immutable Contribution.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContributionKey {
    reference: ContributionReference,
    publication: ProviderRevision,
}

impl ContributionKey {
    /// Constructs one Contribution key.
    #[must_use]
    pub const fn new(
        provider: ProviderId,
        publication: ProviderRevision,
        symbol: ProviderSymbolId,
    ) -> Self {
        Self {
            reference: ContributionReference::new(provider, symbol),
            publication,
        }
    }

    /// Returns provider-local reference.
    #[must_use]
    pub const fn reference(&self) -> &ContributionReference {
        &self.reference
    }

    /// Returns provider publication revision.
    #[must_use]
    pub const fn publication(&self) -> ProviderRevision {
        self.publication
    }
}

/// Half-open byte range in one source unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceRange {
    start: u64,
    end: u64,
}

impl SourceRange {
    /// Constructs a non-empty half-open range.
    ///
    /// # Errors
    ///
    /// Returns [`ContributionError`] when end does not follow start.
    pub fn new(start: u64, end: u64) -> Result<Self, ContributionError> {
        if start >= end {
            return Err(contribution_error(
                ContributionViolation::InvalidSourceRange,
                "source.range",
            ));
        }
        Ok(Self { start, end })
    }

    /// Returns first byte.
    #[must_use]
    pub const fn start(self) -> u64 {
        self.start
    }

    /// Returns one past last byte.
    #[must_use]
    pub const fn end(self) -> u64 {
        self.end
    }
}

/// Exact declaration coordinates supplied by one provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclarationBinding {
    unit: SourceUnitId,
    range: SourceRange,
    node: Option<NodeId>,
}

impl DeclarationBinding {
    /// Constructs one source declaration binding.
    #[must_use]
    pub const fn new(unit: SourceUnitId, range: SourceRange, node: Option<NodeId>) -> Self {
        Self { unit, range, node }
    }

    /// Returns source unit.
    #[must_use]
    pub const fn unit(&self) -> &SourceUnitId {
        &self.unit
    }

    /// Returns declaration byte range.
    #[must_use]
    pub const fn range(&self) -> SourceRange {
        self.range
    }

    /// Returns witnessed syntax node when supplied.
    #[must_use]
    pub const fn node(&self) -> Option<&NodeId> {
        self.node.as_ref()
    }
}

/// Where one provider fact came from.
#[derive(Debug, Clone, PartialEq)]
pub struct ContributionOrigin {
    location: Option<SourceLocation>,
    source_kind: SourceKind,
}

impl ContributionOrigin {
    /// Constructs authored or generated origin.
    ///
    /// # Errors
    ///
    /// Returns [`ContributionError`] when synthetic source has a location or other source does not.
    pub fn new(
        location: Option<SourceLocation>,
        source_kind: SourceKind,
    ) -> Result<Self, ContributionError> {
        let synthetic = source_kind == SourceKind::Synthetic;
        if synthetic == location.is_some() {
            return Err(contribution_error(
                ContributionViolation::InvalidOrigin,
                "origin",
            ));
        }
        Ok(Self {
            location,
            source_kind,
        })
    }

    /// Returns source location.
    #[must_use]
    pub const fn location(&self) -> Option<&SourceLocation> {
        self.location.as_ref()
    }

    /// Returns source kind.
    #[must_use]
    pub const fn source_kind(&self) -> SourceKind {
        self.source_kind
    }
}

/// Portable facts one provider supplies for one symbol.
#[derive(Debug, Clone, PartialEq)]
pub struct PortableSymbolFacts {
    language: Language,
    name: String,
    qualified_name: String,
    kind: ExactKind,
    facets: Vec<SymbolFacet>,
    container: Option<ContributionReference>,
    modifiers: Vec<String>,
    visibility: Option<String>,
    types: Vec<TypeBinding>,
    signatures: Vec<Signature>,
    documentation: Vec<Documentation>,
    document_local: bool,
}

impl PortableSymbolFacts {
    /// Constructs required portable symbol facts.
    #[must_use]
    pub fn new(
        language: Language,
        name: impl Into<String>,
        qualified_name: impl Into<String>,
        kind: ExactKind,
    ) -> Self {
        Self {
            language,
            name: name.into(),
            qualified_name: qualified_name.into(),
            kind,
            facets: Vec::new(),
            container: None,
            modifiers: Vec::new(),
            visibility: None,
            types: Vec::new(),
            signatures: Vec::new(),
            documentation: Vec::new(),
            document_local: false,
        }
    }

    /// Sets portable facets.
    #[must_use]
    pub fn facets(mut self, facets: Vec<SymbolFacet>) -> Self {
        self.facets = facets;
        self
    }

    /// Sets containing symbol reference.
    #[must_use]
    pub fn container(mut self, container: ContributionReference) -> Self {
        self.container = Some(container);
        self
    }

    /// Sets language modifiers.
    #[must_use]
    pub fn modifiers(mut self, modifiers: Vec<String>) -> Self {
        self.modifiers = modifiers;
        self
    }

    /// Sets authored visibility spelling.
    #[must_use]
    pub fn visibility(mut self, visibility: impl Into<String>) -> Self {
        self.visibility = Some(visibility.into());
        self
    }

    /// Sets callable signatures.
    #[must_use]
    pub fn signatures(mut self, signatures: Vec<Signature>) -> Self {
        self.signatures = signatures;
        self
    }

    /// Sets type facts.
    #[must_use]
    pub fn types(mut self, types: Vec<TypeBinding>) -> Self {
        self.types = types;
        self
    }

    /// Sets documentation blocks.
    #[must_use]
    pub fn documentation(mut self, documentation: Vec<Documentation>) -> Self {
        self.documentation = documentation;
        self
    }

    /// Sets document-local classification.
    #[must_use]
    pub const fn document_local(mut self, document_local: bool) -> Self {
        self.document_local = document_local;
        self
    }

    /// Returns language.
    #[must_use]
    pub const fn language(&self) -> &Language {
        &self.language
    }

    /// Returns short name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns provider-qualified name.
    #[must_use]
    pub fn qualified_name(&self) -> &str {
        &self.qualified_name
    }

    /// Returns exact provider kind.
    #[must_use]
    pub const fn kind(&self) -> &ExactKind {
        &self.kind
    }

    /// Returns portable facets.
    #[must_use]
    pub fn symbol_facets(&self) -> &[SymbolFacet] {
        &self.facets
    }

    /// Returns containing symbol reference.
    #[must_use]
    pub const fn container_reference(&self) -> Option<&ContributionReference> {
        self.container.as_ref()
    }

    /// Returns language modifiers.
    #[must_use]
    pub fn modifier_words(&self) -> &[String] {
        &self.modifiers
    }

    /// Returns authored visibility spelling.
    #[must_use]
    pub fn visibility_spelling(&self) -> Option<&str> {
        self.visibility.as_deref()
    }

    /// Returns type facts.
    #[must_use]
    pub fn type_bindings(&self) -> &[TypeBinding] {
        &self.types
    }

    /// Returns callable signatures.
    #[must_use]
    pub fn signatures_slice(&self) -> &[Signature] {
        &self.signatures
    }

    /// Returns documentation blocks.
    #[must_use]
    pub fn documentation_blocks(&self) -> &[Documentation] {
        &self.documentation
    }

    /// Returns document-local classification.
    #[must_use]
    pub const fn is_document_local(&self) -> bool {
        self.document_local
    }
}

/// Evidence a provider supplies for Contribution association.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EquivalenceEvidence {
    /// Exact declaration binding shared with another Contribution.
    Declaration(DeclarationBinding),
    /// Registered provider rule states target describes same declaration.
    Explicit(ContributionReference),
    /// Provider offers target only as a candidate.
    Candidate(ContributionReference),
}

/// One immutable provider record.
#[derive(Debug, Clone, PartialEq)]
pub struct Contribution {
    key: ContributionKey,
    applicability: SourceApplicability,
    facts: PortableSymbolFacts,
    origin: ContributionOrigin,
    source: Option<DeclarationBinding>,
    identity_anchor: Option<SymbolId>,
    equivalence: Vec<EquivalenceEvidence>,
    namespaced: Extensions,
}

impl Contribution {
    /// Starts one Contribution builder.
    #[must_use]
    pub fn builder(
        key: ContributionKey,
        applicability: SourceApplicability,
        facts: PortableSymbolFacts,
        origin: ContributionOrigin,
    ) -> ContributionBuilder {
        ContributionBuilder {
            contribution: Self {
                key,
                applicability,
                facts,
                origin,
                source: None,
                identity_anchor: None,
                equivalence: Vec::new(),
                namespaced: Extensions(std::collections::BTreeMap::new()),
            },
        }
    }

    /// Returns immutable Contribution key.
    #[must_use]
    pub const fn key(&self) -> &ContributionKey {
        &self.key
    }

    /// Returns source applicability.
    #[must_use]
    pub const fn applicability(&self) -> SourceApplicability {
        self.applicability
    }

    /// Returns portable facts.
    #[must_use]
    pub const fn facts(&self) -> &PortableSymbolFacts {
        &self.facts
    }

    /// Returns fact origin.
    #[must_use]
    pub const fn origin(&self) -> &ContributionOrigin {
        &self.origin
    }

    /// Returns declaration binding.
    #[must_use]
    pub const fn source(&self) -> Option<&DeclarationBinding> {
        self.source.as_ref()
    }

    /// Returns established identity anchor.
    #[must_use]
    pub const fn identity_anchor(&self) -> Option<&SymbolId> {
        self.identity_anchor.as_ref()
    }

    /// Returns association evidence.
    #[must_use]
    pub fn equivalence(&self) -> &[EquivalenceEvidence] {
        &self.equivalence
    }

    /// Returns provider-specific facts.
    #[must_use]
    pub const fn namespaced(&self) -> &Extensions {
        &self.namespaced
    }
}

/// Builder for one validated immutable Contribution.
#[derive(Debug)]
pub struct ContributionBuilder {
    contribution: Contribution,
}

impl ContributionBuilder {
    /// Sets exact declaration binding.
    #[must_use]
    pub fn source(mut self, source: DeclarationBinding) -> Self {
        self.contribution.source = Some(source);
        self
    }

    /// Sets identity anchored by exact declaration binding.
    #[must_use]
    pub fn identity_anchor(mut self, identity: SymbolId) -> Self {
        self.contribution.identity_anchor = Some(identity);
        self
    }

    /// Sets association evidence.
    #[must_use]
    pub fn equivalence(mut self, evidence: Vec<EquivalenceEvidence>) -> Self {
        self.contribution.equivalence = evidence;
        self
    }

    /// Sets provider-specific facts.
    #[must_use]
    pub fn namespaced(mut self, namespaced: Extensions) -> Self {
        self.contribution.namespaced = namespaced;
        self
    }

    /// Validates and builds Contribution.
    ///
    /// # Errors
    ///
    /// Returns [`ContributionError`] for invalid facts, bounds, origin, or identity evidence.
    pub fn build(self) -> Result<Contribution, ContributionError> {
        validate_contribution(&self.contribution)?;
        Ok(self.contribution)
    }
}

/// Resolution state of one normalized symbol record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolResolution {
    /// One identity anchor establishes record identity.
    Established,
    /// No accepted evidence establishes identity.
    Unresolved,
    /// More than one identity anchor applies.
    Conflicting,
}

/// One normalized record at one index revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRecord {
    index_revision: IndexRevision,
    identity: Option<SymbolId>,
    resolution: SymbolResolution,
    contributions: Vec<ContributionKey>,
}

impl SymbolRecord {
    /// Constructs one normalized symbol record.
    ///
    /// # Errors
    ///
    /// Returns [`ContributionError`] when state, identity, and members disagree.
    pub fn new(
        index_revision: IndexRevision,
        identity: Option<SymbolId>,
        resolution: SymbolResolution,
        contributions: Vec<ContributionKey>,
    ) -> Result<Self, ContributionError> {
        let identity_matches = matches!(
            (resolution, identity.is_some()),
            (SymbolResolution::Established, true)
                | (
                    SymbolResolution::Unresolved | SymbolResolution::Conflicting,
                    false
                )
        );
        if !identity_matches || contributions.is_empty() {
            return Err(contribution_error(
                ContributionViolation::InvalidRecord,
                "symbol_record",
            ));
        }
        Ok(Self {
            index_revision,
            identity,
            resolution,
            contributions,
        })
    }

    /// Returns index revision.
    #[must_use]
    pub const fn index_revision(&self) -> IndexRevision {
        self.index_revision
    }

    /// Returns stable identity when established.
    #[must_use]
    pub const fn identity(&self) -> Option<&SymbolId> {
        self.identity.as_ref()
    }

    /// Returns resolution state.
    #[must_use]
    pub const fn resolution(&self) -> SymbolResolution {
        self.resolution
    }

    /// Returns associated Contribution keys.
    #[must_use]
    pub fn contributions(&self) -> &[ContributionKey] {
        &self.contributions
    }
}

/// Stable Contribution validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContributionViolation {
    /// Portable name is empty, oversized, or contains control text.
    InvalidName,
    /// Language identity does not use canonical lowercase syntax.
    InvalidLanguage,
    /// Exact kind does not use provider-kind syntax.
    InvalidKind,
    /// Source range is empty or reversed.
    InvalidSourceRange,
    /// Source location and source kind disagree.
    InvalidOrigin,
    /// Identity anchor has no exact source binding.
    UnboundIdentity,
    /// Portable facts exceed their count bound.
    TooManyFacts,
    /// Equivalence evidence exceeds its count bound.
    TooMuchEvidence,
    /// Provider-specific facts exceed count or byte bound.
    TooManyNamespacedFacts,
    /// Provider-specific fact key is not reverse-domain syntax.
    InvalidNamespace,
    /// Provider-specific fact version is zero.
    InvalidNamespaceVersion,
    /// Portable facets contain duplicates.
    DuplicateFact,
    /// Normalized record state, identity, or members disagree.
    InvalidRecord,
}

/// Contribution validation failure and field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContributionFault {
    violation: ContributionViolation,
    field: &'static str,
}

impl ContributionFault {
    /// Returns violated rule.
    #[must_use]
    pub const fn violation(&self) -> ContributionViolation {
        self.violation
    }

    /// Returns offending field.
    #[must_use]
    pub const fn field(&self) -> &'static str {
        self.field
    }
}

impl Fault for ContributionFault {
    fn name(&self) -> ErrorName {
        ErrorName::Wire(ErrorCode::InvalidRequest)
    }

    fn context(&self) -> Vec<ErrorContext> {
        vec![
            ErrorContext::new("field", self.field),
            ErrorContext::new("violation", crate::fault_label(&self.violation)),
        ]
    }
}

/// Invalid Contribution or normalized record.
pub type ContributionError = Error<ContributionFault>;

fn contribution_error(violation: ContributionViolation, field: &'static str) -> ContributionError {
    Error::new(ContributionFault { violation, field })
}

fn validate_contribution(contribution: &Contribution) -> Result<(), ContributionError> {
    validate_provider_symbol(contribution)?;
    validate_portable_facts(&contribution.facts)?;
    validate_source_and_origin(contribution)?;
    validate_evidence(contribution)?;
    validate_namespaced(&contribution.namespaced)
}

fn validate_provider_symbol(contribution: &Contribution) -> Result<(), ContributionError> {
    let value = contribution.key.reference.symbol.as_str();
    if value.len() > PROVIDER_SYMBOL_ID_BYTES_MAX {
        return Err(contribution_error(
            ContributionViolation::InvalidName,
            "provider_symbol",
        ));
    }
    Ok(())
}

fn validate_portable_facts(facts: &PortableSymbolFacts) -> Result<(), ContributionError> {
    if invalid_text(&facts.name) || invalid_text(&facts.qualified_name) {
        return Err(contribution_error(
            ContributionViolation::InvalidName,
            "facts.name",
        ));
    }
    if !valid_language(&facts.language) {
        return Err(contribution_error(
            ContributionViolation::InvalidLanguage,
            "facts.language",
        ));
    }
    if !valid_kind(&facts.kind.0) {
        return Err(contribution_error(
            ContributionViolation::InvalidKind,
            "facts.kind",
        ));
    }
    let counts = [
        facts.facets.len(),
        facts.modifiers.len(),
        facts.types.len(),
        facts.signatures.len(),
        facts.documentation.len(),
    ];
    if counts
        .into_iter()
        .any(|count| count > CONTRIBUTION_FACTS_MAX)
    {
        return Err(contribution_error(
            ContributionViolation::TooManyFacts,
            "facts",
        ));
    }
    let unique: BTreeSet<_> = facts.facets.iter().copied().collect();
    if unique.len() != facts.facets.len() {
        return Err(contribution_error(
            ContributionViolation::DuplicateFact,
            "facts.facets",
        ));
    }
    Ok(())
}

fn validate_source_and_origin(contribution: &Contribution) -> Result<(), ContributionError> {
    if contribution.identity_anchor.is_some() && contribution.source.is_none() {
        return Err(contribution_error(
            ContributionViolation::UnboundIdentity,
            "identity_anchor",
        ));
    }
    let synthetic = contribution.origin.source_kind == SourceKind::Synthetic;
    if contribution.source.is_some() && synthetic {
        return Err(contribution_error(
            ContributionViolation::InvalidOrigin,
            "source",
        ));
    }
    Ok(())
}

fn validate_evidence(contribution: &Contribution) -> Result<(), ContributionError> {
    if contribution.equivalence.len() > CONTRIBUTION_EVIDENCE_MAX {
        return Err(contribution_error(
            ContributionViolation::TooMuchEvidence,
            "equivalence",
        ));
    }
    Ok(())
}

fn validate_namespaced(namespaced: &Extensions) -> Result<(), ContributionError> {
    if namespaced.0.len() > CONTRIBUTION_NAMESPACES_MAX {
        return Err(contribution_error(
            ContributionViolation::TooManyNamespacedFacts,
            "namespaced",
        ));
    }
    for (key, value) in &namespaced.0 {
        validate_namespace(key)?;
        if value.version == 0 {
            return Err(contribution_error(
                ContributionViolation::InvalidNamespaceVersion,
                "namespaced.version",
            ));
        }
        let bytes = serde_json::to_vec(value).map_err(|_| {
            contribution_error(
                ContributionViolation::TooManyNamespacedFacts,
                "namespaced.data",
            )
        })?;
        if bytes.len() > CONTRIBUTION_NAMESPACE_BYTES_MAX {
            return Err(contribution_error(
                ContributionViolation::TooManyNamespacedFacts,
                "namespaced.data",
            ));
        }
    }
    Ok(())
}

fn validate_namespace(key: &ExtensionKey) -> Result<(), ContributionError> {
    if valid_namespace(&key.0) {
        return Ok(());
    }
    Err(contribution_error(
        ContributionViolation::InvalidNamespace,
        "namespaced.key",
    ))
}

fn invalid_text(value: &str) -> bool {
    value.is_empty()
        || value.len() > PROVIDER_SYMBOL_ID_BYTES_MAX
        || value.chars().any(char::is_control)
}

fn valid_language(language: &Language) -> bool {
    is_canonical_ascii_name(&language.name, 64, b"._-")
        && language
            .dialect
            .as_deref()
            .is_none_or(|dialect| is_canonical_ascii_name(dialect, 64, b"._-"))
}

fn valid_kind(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

fn valid_namespace(value: &str) -> bool {
    let Some((prefix, field)) = value.rsplit_once('.') else {
        return false;
    };
    let prefix_valid = !prefix.is_empty()
        && prefix.contains(['.', '-'])
        && prefix.split(['.', '-']).all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        });
    let mut field_bytes = field.bytes();
    let field_valid = field_bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic())
        && field_bytes.all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte));
    prefix_valid && field_valid
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rift_protocol::read::{
        ExactKind, ExtensionKey, ExtensionValue, Extensions, Language, SourceKind, SourceLocation,
        SymbolFacet,
    };
    use serde_json::json;

    use super::{
        CONTRIBUTION_EVIDENCE_MAX, CONTRIBUTION_FACTS_MAX, CONTRIBUTION_NAMESPACE_BYTES_MAX,
        CONTRIBUTION_NAMESPACES_MAX, Contribution, ContributionKey, ContributionOrigin,
        ContributionReference, ContributionViolation, EquivalenceEvidence, PortableSymbolFacts,
        SourceApplicability, SourceRange, SymbolRecord, SymbolResolution,
    };
    use crate::{
        IndexRevision, ProviderId, ProviderRevision, ProviderSymbolId, SourcePath,
        SourceResolverId, SourceRevision, SourceUnitId, SymbolId, TreeRevision,
    };

    fn provider(value: &str) -> ProviderId {
        ProviderId::new(value).expect("provider")
    }

    fn provider_symbol(value: &str) -> ProviderSymbolId {
        ProviderSymbolId::new(value).expect("provider symbol")
    }

    fn publication(value: u64) -> ProviderRevision {
        ProviderRevision::new(value).expect("provider revision")
    }

    fn source_revision(value: u64) -> SourceRevision {
        SourceRevision::new(value).expect("source revision")
    }

    fn tree_revision(value: u64) -> TreeRevision {
        TreeRevision::new(value).expect("tree revision")
    }

    fn source_unit() -> SourceUnitId {
        SourceUnitId::new(
            SourceResolverId::new("project").expect("resolver"),
            SourcePath::new("src/lib.rs").expect("source path"),
        )
        .expect("source unit")
    }

    fn origin() -> ContributionOrigin {
        ContributionOrigin::new(
            Some(SourceLocation::Project { package: None }),
            SourceKind::Authored,
        )
        .expect("origin")
    }

    fn facts() -> PortableSymbolFacts {
        PortableSymbolFacts::new(
            Language {
                name: "rust".to_owned(),
                dialect: None,
            },
            "Beacon",
            "Beacon",
            ExactKind("rust.struct".to_owned()),
        )
        .facets(vec![SymbolFacet::Type, SymbolFacet::Public])
        .visibility("pub")
        .signatures(Vec::new())
        .documentation(Vec::new())
    }

    fn contribution() -> Contribution {
        let binding = super::DeclarationBinding::new(
            source_unit(),
            SourceRange::new(0, 12).expect("range"),
            None,
        );
        Contribution::builder(
            ContributionKey::new(
                provider("syntax"),
                publication(1),
                provider_symbol("Beacon"),
            ),
            SourceApplicability::Exact {
                source_revision: source_revision(1),
                tree_revision: tree_revision(1),
            },
            facts(),
            origin(),
        )
        .source(binding)
        .identity_anchor(SymbolId::new("rust:src/lib.rs:Beacon").expect("symbol"))
        .build()
        .expect("contribution")
    }

    #[test]
    fn contribution_builds_with_exact_source_anchor() {
        let contribution = contribution();
        let key = contribution.key();
        assert_eq!(key.reference().provider().as_str(), "syntax");
        assert_eq!(key.reference().symbol().as_str(), "Beacon");
        assert_eq!(key.publication(), publication(1));
        assert_eq!(
            contribution.applicability(),
            SourceApplicability::Exact {
                source_revision: source_revision(1),
                tree_revision: tree_revision(1),
            }
        );

        let facts = contribution.facts();
        assert_eq!(facts.language().name, "rust");
        assert_eq!(facts.name(), "Beacon");
        assert_eq!(facts.qualified_name(), "Beacon");
        assert_eq!(facts.kind().0, "rust.struct");
        assert_eq!(
            facts.symbol_facets(),
            &[SymbolFacet::Type, SymbolFacet::Public]
        );
        assert_eq!(facts.visibility_spelling(), Some("pub"));
        assert!(facts.signatures_slice().is_empty());
        assert!(facts.documentation_blocks().is_empty());

        let origin = contribution.origin();
        assert!(origin.location().is_some());
        assert_eq!(origin.source_kind(), SourceKind::Authored);

        let source = contribution.source().expect("source");
        assert_eq!(source.unit(), &source_unit());
        assert_eq!(source.range().start(), 0);
        assert_eq!(source.range().end(), 12);
        assert!(source.node().is_none());
        assert!(contribution.identity_anchor().is_some());
        assert!(contribution.equivalence().is_empty());
        assert!(contribution.namespaced().0.is_empty());
    }

    #[test]
    fn applicability_matches_exact_revisions_and_accepts_independent_facts() {
        let exact = SourceApplicability::Exact {
            source_revision: source_revision(2),
            tree_revision: tree_revision(3),
        };
        assert!(exact.applies_to(source_revision(2), tree_revision(3)));
        assert!(!exact.applies_to(source_revision(2), tree_revision(4)));
        assert!(SourceApplicability::Independent.applies_to(source_revision(9), tree_revision(9)));
    }

    #[test]
    fn invalid_ranges_and_origins_name_their_fields() {
        let range_error = SourceRange::new(4, 4).expect_err("empty range");
        assert_eq!(
            range_error.fault().violation(),
            ContributionViolation::InvalidSourceRange
        );
        assert_eq!(range_error.fault().field(), "source.range");
        assert_eq!(range_error.context()[0].key(), "field");
        assert!(range_error.to_string().contains("source.range"));

        let origin_error =
            ContributionOrigin::new(None, SourceKind::Authored).expect_err("missing location");
        assert_eq!(
            origin_error.fault().violation(),
            ContributionViolation::InvalidOrigin
        );
        assert_eq!(origin_error.fault().field(), "origin");
    }

    #[test]
    fn source_binding_and_identity_require_matching_origin() {
        let synthetic =
            ContributionOrigin::new(None, SourceKind::Synthetic).expect("synthetic origin");
        let error = Contribution::builder(
            ContributionKey::new(
                provider("syntax"),
                publication(1),
                provider_symbol("Beacon"),
            ),
            SourceApplicability::Independent,
            facts(),
            synthetic.clone(),
        )
        .identity_anchor(SymbolId::new("rust:Beacon").expect("symbol"))
        .build()
        .expect_err("unbound identity");
        assert_eq!(
            error.fault().violation(),
            ContributionViolation::UnboundIdentity
        );

        let source = super::DeclarationBinding::new(
            source_unit(),
            SourceRange::new(0, 12).expect("range"),
            None,
        );
        let error = Contribution::builder(
            ContributionKey::new(
                provider("syntax"),
                publication(1),
                provider_symbol("Beacon"),
            ),
            SourceApplicability::Independent,
            facts(),
            synthetic,
        )
        .source(source)
        .build()
        .expect_err("synthetic source binding");
        assert_eq!(
            error.fault().violation(),
            ContributionViolation::InvalidOrigin
        );
    }

    #[test]
    fn revision_independent_authored_fact_can_remain_unbound() {
        let contribution = Contribution::builder(
            ContributionKey::new(provider("docs"), publication(1), provider_symbol("Beacon")),
            SourceApplicability::Independent,
            facts(),
            origin(),
        )
        .build()
        .expect("unbound authored contribution");
        assert!(contribution.source().is_none());
        assert_eq!(
            contribution.applicability(),
            SourceApplicability::Independent
        );
    }

    #[test]
    fn duplicate_facets_are_refused() {
        let duplicate = facts().facets(vec![SymbolFacet::Type, SymbolFacet::Type]);
        let error = Contribution::builder(
            ContributionKey::new(
                provider("syntax"),
                publication(1),
                provider_symbol("Beacon"),
            ),
            SourceApplicability::Independent,
            duplicate,
            ContributionOrigin::new(None, SourceKind::Synthetic).expect("synthetic origin"),
        )
        .build()
        .expect_err("duplicate facets");
        assert_eq!(
            error.fault().violation(),
            ContributionViolation::DuplicateFact
        );
    }

    #[test]
    fn portable_fact_validation_covers_names_language_kind_and_bounds() {
        let cases = [
            (
                PortableSymbolFacts::new(
                    Language {
                        name: "rust".to_owned(),
                        dialect: None,
                    },
                    "",
                    "Beacon",
                    ExactKind("rust.struct".to_owned()),
                ),
                ContributionViolation::InvalidName,
            ),
            (
                PortableSymbolFacts::new(
                    Language {
                        name: "Rust".to_owned(),
                        dialect: None,
                    },
                    "Beacon",
                    "Beacon",
                    ExactKind("rust.struct".to_owned()),
                ),
                ContributionViolation::InvalidLanguage,
            ),
            (
                PortableSymbolFacts::new(
                    Language {
                        name: "rust".to_owned(),
                        dialect: Some("Rust".to_owned()),
                    },
                    "Beacon",
                    "Beacon",
                    ExactKind("rust.struct".to_owned()),
                ),
                ContributionViolation::InvalidLanguage,
            ),
            (
                PortableSymbolFacts::new(
                    Language {
                        name: "rust".to_owned(),
                        dialect: None,
                    },
                    "Beacon",
                    "Beacon",
                    ExactKind("9struct".to_owned()),
                ),
                ContributionViolation::InvalidKind,
            ),
        ];
        for (facts, violation) in cases {
            let error = Contribution::builder(
                ContributionKey::new(provider("docs"), publication(1), provider_symbol("Beacon")),
                SourceApplicability::Independent,
                facts,
                ContributionOrigin::new(None, SourceKind::Synthetic).expect("synthetic origin"),
            )
            .build()
            .expect_err("invalid portable facts");
            assert_eq!(error.fault().violation(), violation);
        }

        let error = Contribution::builder(
            ContributionKey::new(provider("docs"), publication(1), provider_symbol("Beacon")),
            SourceApplicability::Independent,
            facts().facets(vec![SymbolFacet::Type; CONTRIBUTION_FACTS_MAX + 1]),
            ContributionOrigin::new(None, SourceKind::Synthetic).expect("synthetic origin"),
        )
        .build()
        .expect_err("portable fact bound");
        assert_eq!(
            error.fault().violation(),
            ContributionViolation::TooManyFacts
        );
    }

    #[test]
    fn namespace_keys_versions_counts_and_bytes_are_validated() {
        let build = |map| {
            Contribution::builder(
                ContributionKey::new(provider("docs"), publication(1), provider_symbol("Beacon")),
                SourceApplicability::Independent,
                facts(),
                ContributionOrigin::new(None, SourceKind::Synthetic).expect("synthetic origin"),
            )
            .namespaced(Extensions(map))
            .build()
        };

        let mut invalid_key = BTreeMap::new();
        invalid_key.insert(
            ExtensionKey("Bad".to_owned()),
            ExtensionValue {
                version: 1,
                data: json!({"value": true}),
            },
        );
        let error = build(invalid_key).expect_err("invalid namespace");
        assert_eq!(
            error.fault().violation(),
            ContributionViolation::InvalidNamespace
        );

        let mut zero_version = BTreeMap::new();
        zero_version.insert(
            ExtensionKey("com.example.field".to_owned()),
            ExtensionValue {
                version: 0,
                data: json!(null),
            },
        );
        let error = build(zero_version).expect_err("zero namespace version");
        assert_eq!(
            error.fault().violation(),
            ContributionViolation::InvalidNamespaceVersion
        );

        let mut oversized = BTreeMap::new();
        oversized.insert(
            ExtensionKey("com.example.field".to_owned()),
            ExtensionValue {
                version: 1,
                data: json!("x".repeat(CONTRIBUTION_NAMESPACE_BYTES_MAX)),
            },
        );
        let error = build(oversized).expect_err("oversized namespace value");
        assert_eq!(
            error.fault().violation(),
            ContributionViolation::TooManyNamespacedFacts
        );

        let too_many = (0..=CONTRIBUTION_NAMESPACES_MAX)
            .map(|index| {
                (
                    ExtensionKey(format!("com.example.field{index}")),
                    ExtensionValue {
                        version: 1,
                        data: json!(null),
                    },
                )
            })
            .collect();
        let error = build(too_many).expect_err("namespace count bound");
        assert_eq!(
            error.fault().violation(),
            ContributionViolation::TooManyNamespacedFacts
        );
    }

    #[test]
    fn evidence_count_is_bounded() {
        let target = ContributionReference::new(provider("other"), provider_symbol("Beacon"));
        let evidence = vec![EquivalenceEvidence::Candidate(target); CONTRIBUTION_EVIDENCE_MAX + 1];
        let error = Contribution::builder(
            ContributionKey::new(provider("docs"), publication(1), provider_symbol("Beacon")),
            SourceApplicability::Independent,
            facts(),
            ContributionOrigin::new(None, SourceKind::Synthetic).expect("synthetic origin"),
        )
        .equivalence(evidence)
        .build()
        .expect_err("evidence bound");
        assert_eq!(
            error.fault().violation(),
            ContributionViolation::TooMuchEvidence
        );
    }

    #[test]
    fn symbol_record_state_requires_matching_identity() {
        let key = contribution().key().clone();
        let index_revision = IndexRevision::new(1).expect("index revision");
        let identity = SymbolId::new("rust:src/lib.rs:Beacon").expect("symbol");
        let record = SymbolRecord::new(
            index_revision,
            Some(identity.clone()),
            SymbolResolution::Established,
            vec![key.clone()],
        )
        .expect("established record");
        assert_eq!(record.index_revision(), index_revision);
        assert_eq!(record.identity(), Some(&identity));
        assert_eq!(record.resolution(), SymbolResolution::Established);
        assert_eq!(record.contributions(), std::slice::from_ref(&key));

        let unresolved = SymbolRecord::new(
            index_revision,
            None,
            SymbolResolution::Unresolved,
            vec![key.clone()],
        )
        .expect("unresolved record");
        assert_eq!(unresolved.resolution(), SymbolResolution::Unresolved);

        let invalid = SymbolRecord::new(
            index_revision,
            Some(identity),
            SymbolResolution::Conflicting,
            vec![key.clone()],
        )
        .expect_err("conflicting record with identity");
        assert_eq!(
            invalid.fault().violation(),
            ContributionViolation::InvalidRecord
        );
        assert_eq!(invalid.fault().field(), "symbol_record");

        let empty = SymbolRecord::new(
            index_revision,
            None,
            SymbolResolution::Unresolved,
            Vec::new(),
        )
        .expect_err("record without contributions");
        assert_eq!(
            empty.fault().violation(),
            ContributionViolation::InvalidRecord
        );
    }
}
