use std::error::Error as StdError;
use std::fmt::{Display, Formatter};

use rift_core::{
    Contribution, ContributionError, ContributionKey, ContributionOrigin, ContributionReference,
    ExactKind, IdError, PortableSymbolFacts, ProviderId, ProviderRevision, ProviderSymbolId,
    SourceApplicability, SourceKind, SourceLocation, SourceRange, SourceRevision, SourceUnitId,
    SourceUnitIdError, SymbolId, TreeRevision, encode_path, symbol_identity,
};
use rift_provider::{ProviderPublication, PublicationError, PublicationLimits};

use crate::SyntaxDocument;

/// Stable identity of built-in syntax Contribution provider.
pub const SYNTAX_PROVIDER_ID: &str = "syntax";

/// Failure while syntax facts become one provider publication.
#[derive(Debug)]
pub enum SyntaxPublicationError {
    /// Provider or symbol identity is invalid.
    Identity(IdError),
    /// Source-unit identity is invalid.
    SourceUnit(SourceUnitIdError),
    /// One syntax Contribution is invalid.
    Contribution(ContributionError),
    /// Completed provider publication is invalid.
    Publication(PublicationError),
}

impl Display for SyntaxPublicationError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Identity(error) => Display::fmt(error, formatter),
            Self::SourceUnit(error) => Display::fmt(error, formatter),
            Self::Contribution(error) => Display::fmt(error, formatter),
            Self::Publication(error) => Display::fmt(error, formatter),
        }
    }
}

impl StdError for SyntaxPublicationError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Identity(error) => Some(error),
            Self::SourceUnit(error) => Some(error),
            Self::Contribution(error) => Some(error),
            Self::Publication(error) => Some(error),
        }
    }
}

impl From<IdError> for SyntaxPublicationError {
    fn from(error: IdError) -> Self {
        Self::Identity(error)
    }
}

impl From<SourceUnitIdError> for SyntaxPublicationError {
    fn from(error: SourceUnitIdError) -> Self {
        Self::SourceUnit(error)
    }
}

impl From<ContributionError> for SyntaxPublicationError {
    fn from(error: ContributionError) -> Self {
        Self::Contribution(error)
    }
}

impl From<PublicationError> for SyntaxPublicationError {
    fn from(error: PublicationError) -> Self {
        Self::Publication(error)
    }
}

/// Collects syntax documents into one atomic provider publication.
#[derive(Debug)]
pub struct SyntaxPublicationBuilder {
    provider: ProviderId,
    publication: ProviderRevision,
    source_revision: SourceRevision,
    tree_revision: TreeRevision,
    limits: PublicationLimits,
    contributions: Vec<Contribution>,
}

impl SyntaxPublicationBuilder {
    /// Starts one syntax provider publication.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxPublicationError`] when built-in provider identity is
    /// invalid.
    pub fn new(
        publication: ProviderRevision,
        source_revision: SourceRevision,
        tree_revision: TreeRevision,
        limits: PublicationLimits,
    ) -> Result<Self, SyntaxPublicationError> {
        Ok(Self {
            provider: ProviderId::new(SYNTAX_PROVIDER_ID)?,
            publication,
            source_revision,
            tree_revision,
            limits,
            contributions: Vec::new(),
        })
    }

    /// Adds every declaration from one syntax document.
    ///
    /// Document either contributes every declaration or changes nothing.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxPublicationError`] when source identity or one
    /// Contribution is invalid.
    pub fn add_document(
        &mut self,
        document: &SyntaxDocument,
    ) -> Result<(), SyntaxPublicationError> {
        let source_unit = source_unit(document)?;
        let mut additions = Vec::with_capacity(document.symbols().len());
        for symbol in document.symbols() {
            let identity = symbol_identity(
                &document.language().identity_segment(),
                document.path().as_str(),
                &symbol.qualified_name,
            );
            let identity = SymbolId::new(identity)?;
            let provider_symbol = ProviderSymbolId::new(identity.as_str())?;
            let mut facts = PortableSymbolFacts::new(
                document.language().clone(),
                symbol.name.clone(),
                symbol.qualified_name.clone(),
                ExactKind(symbol.kind.to_owned()),
            )
            .facets(symbol.facets.clone())
            .signatures(symbol.signatures.clone())
            .documentation(symbol.documentation.clone());
            if let Some(visibility) = &symbol.visibility {
                facts = facts.visibility(visibility.clone());
            }
            if let Some(container) = &symbol.container {
                let container_identity = symbol_identity(
                    &document.language().identity_segment(),
                    document.path().as_str(),
                    container,
                );
                facts = facts.container(ContributionReference::new(
                    self.provider.clone(),
                    ProviderSymbolId::new(container_identity)?,
                ));
            }
            let source = rift_core::DeclarationBinding::new(
                source_unit.clone(),
                SourceRange::new(symbol.item_range.start, symbol.item_range.end)?,
                None,
            );
            let contribution = Contribution::builder(
                ContributionKey::new(self.provider.clone(), self.publication, provider_symbol),
                SourceApplicability::Exact {
                    source_revision: self.source_revision,
                    tree_revision: self.tree_revision,
                },
                facts,
                ContributionOrigin::new(
                    Some(SourceLocation::Project { package: None }),
                    SourceKind::Authored,
                )?,
            )
            .source(source)
            .identity_anchor(identity)
            .build()?;
            additions.push(contribution);
        }
        self.contributions.extend(additions);
        Ok(())
    }

    /// Validates and returns complete syntax provider publication.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxPublicationError`] when publication bounds or keys are
    /// invalid.
    pub fn build(self) -> Result<ProviderPublication, SyntaxPublicationError> {
        Ok(ProviderPublication::new(
            self.provider,
            self.publication,
            self.contributions,
            self.limits,
        )?)
    }
}

/// Mints one syntax document's project source-unit identity.
///
/// The spelling is `rift://source/project/<escaped path>`, the identity every
/// project-tree Contribution carries, so a consumer joining another provider's
/// facts to this document addresses the same unit.
///
/// # Errors
///
/// Returns [`SourceUnitIdError`] when the document's path breaks source-unit rules.
pub fn source_unit(document: &SyntaxDocument) -> Result<SourceUnitId, SourceUnitIdError> {
    SourceUnitId::parse(&format!(
        "rift://source/project/{}",
        encode_path(document.path().as_str())
    ))
}

#[cfg(test)]
mod tests {
    use rift_core::{ProjectPath, ProviderRevision, SourceRevision, SymbolId, TreeRevision};

    use super::SyntaxPublicationBuilder;
    use crate::{RustSyntaxProvider, SyntaxProvider, SyntaxSource};

    fn publication(value: u64) -> ProviderRevision {
        ProviderRevision::new(value).expect("publication")
    }

    fn source_revision(value: u64) -> SourceRevision {
        SourceRevision::new(value).expect("source revision")
    }

    fn tree_revision(value: u64) -> TreeRevision {
        TreeRevision::new(value).expect("tree revision")
    }

    #[test]
    fn syntax_documents_publish_portable_facts_and_container_reference() {
        let provider = RustSyntaxProvider::default();
        let path = ProjectPath::new("src/lib.rs").expect("path");
        let document = provider
            .analyze(SyntaxSource {
                path: &path,
                text: "pub struct Beacon; impl Beacon { pub fn run() {} }",
            })
            .expect("syntax document");
        let mut builder = SyntaxPublicationBuilder::new(
            publication(1),
            source_revision(1),
            tree_revision(1),
            rift_provider::PublicationLimits::default(),
        )
        .expect("builder");
        builder.add_document(&document).expect("document");
        let publication = builder.build().expect("publication");
        let beacon = publication
            .contributions()
            .iter()
            .find(|contribution| {
                contribution
                    .facts()
                    .is_some_and(|facts| facts.name() == "Beacon")
            })
            .expect("Beacon");
        let run = publication
            .contributions()
            .iter()
            .find(|contribution| {
                contribution
                    .facts()
                    .is_some_and(|facts| facts.name() == "run")
            })
            .expect("run");
        assert_eq!(
            beacon.identity_anchor().map(SymbolId::as_str),
            Some("rift://symbol/rust/src/lib.rs/Beacon")
        );
        assert_eq!(
            run.facts()
                .expect("portable facts")
                .container_reference()
                .map(|reference| reference.symbol().as_str()),
            Some("rift://symbol/rust/src/lib.rs/Beacon")
        );
        assert_eq!(
            run.facts().expect("portable facts").visibility_spelling(),
            Some("pub")
        );
    }

    #[test]
    fn publication_bound_accepts_exact_contribution_count() {
        let provider = RustSyntaxProvider::default();
        let path = ProjectPath::new("src/lib.rs").expect("path");
        let document = provider
            .analyze(SyntaxSource {
                path: &path,
                text: "pub struct Beacon;",
            })
            .expect("syntax document");
        let mut builder = SyntaxPublicationBuilder::new(
            publication(1),
            source_revision(1),
            tree_revision(1),
            rift_provider::PublicationLimits::new(1, 1, 1).expect("limits"),
        )
        .expect("builder");
        builder.add_document(&document).expect("document");
        let publication = builder.build().expect("publication");
        assert_eq!(publication.contributions().len(), 1);
    }
}
