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

/// Where one document's declarations are filed: origin, source unit, and identity path.
///
/// `unit` is the [`SourceUnitId`] every declaration's binding names, and
/// `identity_path` is the path segment each [`SymbolId`] embeds after the
/// language. The project placement files a document under
/// `rift://source/project/<path>` with the path itself as the identity path;
/// a dependency placement names the package's own unit and embeds
/// `<manager>/<name>@<version>/<path>` instead.
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentPlacement {
    origin: ContributionOrigin,
    unit: SourceUnitId,
    identity_path: String,
}

impl DocumentPlacement {
    /// Files declarations under `unit`, embedding `identity_path` in every symbol identity.
    #[must_use]
    pub fn new(
        origin: ContributionOrigin,
        unit: SourceUnitId,
        identity_path: impl Into<String>,
    ) -> Self {
        Self {
            origin,
            unit,
            identity_path: identity_path.into(),
        }
    }

    /// The project placement: `rift://source/project/<path>` with the path itself.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxPublicationError`] when the document's path breaks
    /// source-unit rules.
    pub fn project(document: &SyntaxDocument) -> Result<Self, SyntaxPublicationError> {
        let location = SourceLocation::Project { package: None };
        let origin = ContributionOrigin::new(Some(location), SourceKind::Authored)?;
        Ok(Self::new(
            origin,
            source_unit(document)?,
            document.path().as_str(),
        ))
    }

    /// Where the declarations came from.
    #[must_use]
    pub const fn origin(&self) -> &ContributionOrigin {
        &self.origin
    }

    /// The source unit every declaration's binding names.
    #[must_use]
    pub const fn unit(&self) -> &SourceUnitId {
        &self.unit
    }

    /// The path segment every symbol identity embeds after the language.
    #[must_use]
    pub fn identity_path(&self) -> &str {
        &self.identity_path
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

    /// Adds every declaration from one project-tree syntax document.
    ///
    /// The project placement is [`DocumentPlacement::project`]; the document
    /// either contributes every declaration or changes nothing.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxPublicationError`] when source identity or one
    /// Contribution is invalid.
    pub fn add_document(
        &mut self,
        document: &SyntaxDocument,
    ) -> Result<(), SyntaxPublicationError> {
        let placement = DocumentPlacement::project(document)?;
        self.add_document_placed(document, &placement)
    }

    /// Adds every declaration from one syntax document under `placement`.
    ///
    /// Each declaration's binding names the placement's unit, its identity
    /// embeds the placement's identity path, and its origin is the placement's.
    /// Document either contributes every declaration or changes nothing.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxPublicationError`] when one symbol identity or one
    /// Contribution is invalid.
    pub fn add_document_placed(
        &mut self,
        document: &SyntaxDocument,
        placement: &DocumentPlacement,
    ) -> Result<(), SyntaxPublicationError> {
        let language_segment = document.language().identity_segment();
        let mut additions = Vec::with_capacity(document.symbols().len());
        for symbol in document.symbols() {
            let identity = symbol_identity(
                &language_segment,
                placement.identity_path(),
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
                let container_identity =
                    symbol_identity(&language_segment, placement.identity_path(), container);
                facts = facts.container(ContributionReference::new(
                    self.provider.clone(),
                    ProviderSymbolId::new(container_identity)?,
                ));
            }
            let source = rift_core::DeclarationBinding::new(
                placement.unit().clone(),
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
                placement.origin().clone(),
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

    /// A dependency placement files the unit and identity under the package,
    /// and the container reference embeds the same identity path.
    #[test]
    fn test_add_document_placed_files_declarations_under_the_supplied_unit_and_path() {
        use rift_core::{
            ContributionOrigin, SourceKind, SourceLocation, SourcePath, SourceResolverId,
            SourceUnitId,
        };
        use rift_protocol::read::PackageIdentity;

        use super::DocumentPlacement;

        let provider = RustSyntaxProvider::default();
        let path = ProjectPath::new("src/lib.rs").expect("path");
        let document = provider
            .analyze(SyntaxSource {
                path: &path,
                text: "pub fn spawn() {}\npub struct Runtime; impl Runtime { pub fn new() {} }",
            })
            .expect("syntax document");
        let package = PackageIdentity {
            manager: "cargo".to_owned(),
            name: "tokio".to_owned(),
            version: "1.53.1".to_owned(),
        };
        let origin = ContributionOrigin::new(
            Some(SourceLocation::Dependency {
                package: package.clone(),
            }),
            SourceKind::Authored,
        )
        .expect("origin");
        let unit = SourceUnitId::new(
            SourceResolverId::new("cargo").expect("resolver"),
            SourcePath::new("tokio@1.53.1/src/lib.rs").expect("unit key"),
        )
        .expect("unit");
        let placement = DocumentPlacement::new(origin, unit, "cargo/tokio@1.53.1/src/lib.rs");
        let mut builder = SyntaxPublicationBuilder::new(
            publication(1),
            source_revision(1),
            tree_revision(1),
            rift_provider::PublicationLimits::default(),
        )
        .expect("builder");
        builder
            .add_document_placed(&document, &placement)
            .expect("document");
        let publication = builder.build().expect("publication");
        let named = |name: &str| {
            publication
                .contributions()
                .iter()
                .find(|contribution| {
                    contribution
                        .facts()
                        .is_some_and(|facts| facts.name() == name)
                })
                .unwrap_or_else(|| panic!("publication holds {name}"))
        };
        let spawn = named("spawn");
        assert_eq!(
            spawn.identity_anchor().map(SymbolId::as_str),
            Some("rift://symbol/rust/cargo/tokio@1.53.1/src/lib.rs/spawn")
        );
        assert_eq!(
            spawn.source().map(|binding| binding.unit().to_string()),
            Some("rift://source/cargo/tokio@1.53.1/src/lib.rs".to_owned())
        );
        assert_eq!(
            spawn.origin().location(),
            Some(&SourceLocation::Dependency { package })
        );
        assert_eq!(
            named("new")
                .facts()
                .expect("portable facts")
                .container_reference()
                .map(|reference| reference.symbol().as_str()),
            Some("rift://symbol/rust/cargo/tokio@1.53.1/src/lib.rs/Runtime")
        );
    }
}
