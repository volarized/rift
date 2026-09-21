use std::error::Error as StdError;
use std::fmt;
use std::sync::Arc;

use rift_core::{
    ContributionError, ContributionReference, IndexRevision, ProjectPath, ProviderId,
    ProviderRevision, ProviderSymbolId, RevisionError, SourceRevision, SourceUnitIdError,
    TreeRevision,
};
use rift_provider::{
    AssembledSymbol, NormalizedGraph, Normalizer, PublicationError, PublicationLimits,
    PublicationSet, SymbolAssembler,
};
use rift_syntax::{
    DocumentPlacement, SYNTAX_PROVIDER_ID, SyntaxDocument, SyntaxPublicationBuilder,
    SyntaxPublicationError,
};

use crate::relationship::RelationshipStore;

/// One syntax document and the placement its declarations are filed under.
#[derive(Debug)]
pub(crate) struct PlacedDocument<'a> {
    /// The parsed document.
    pub(crate) document: &'a SyntaxDocument,
    /// The unit, origin, and identity path its declarations carry.
    pub(crate) placement: DocumentPlacement,
}

/// Contribution graph captured by one workspace index publication.
#[derive(Debug)]
pub(crate) struct WorkspaceSemantics {
    graph: NormalizedGraph,
    relationships: RelationshipStore,
    syntax_provider: ProviderId,
}

impl WorkspaceSemantics {
    /// Builds one syntax publication and one normalized graph over one file set.
    ///
    /// Every document takes the project placement, [`DocumentPlacement::project`];
    /// the build itself is [`Self::build_placed`].
    pub(crate) fn build<'a>(
        documents: impl IntoIterator<Item = &'a SyntaxDocument>,
        revision: u64,
        previous: Option<&NormalizedGraph>,
    ) -> Result<Self, WorkspaceSemanticError> {
        let placed = documents
            .into_iter()
            .map(|document| {
                Ok(PlacedDocument {
                    document,
                    placement: DocumentPlacement::project(document)?,
                })
            })
            .collect::<Result<Vec<_>, SyntaxPublicationError>>()?;
        Self::build_placed(&placed, revision, previous)
    }

    /// Builds the publication and one normalized graph over documents the caller placed.
    ///
    /// A document whose declarations the syntax publication refuses names itself in
    /// the error, so the index can leave that one file out instead of failing the build.
    pub(crate) fn build_placed(
        documents: &[PlacedDocument<'_>],
        revision: u64,
        previous: Option<&NormalizedGraph>,
    ) -> Result<Self, WorkspaceSemanticError> {
        let index_revision = IndexRevision::new(revision)?;
        let source_revision = SourceRevision::new(revision)?;
        let tree_revision = TreeRevision::new(revision)?;
        let provider_revision = ProviderRevision::new(revision)?;
        let limits = PublicationLimits::default();
        let mut builder = SyntaxPublicationBuilder::new(
            provider_revision,
            source_revision,
            tree_revision,
            limits,
        )?;
        for placed in documents {
            builder
                .add_document_placed(placed.document, &placed.placement)
                .map_err(|error| WorkspaceSemanticError::Document {
                    path: placed.document.path().clone(),
                    error,
                })?;
        }
        let publication = builder.build()?;
        let publications = Arc::new(PublicationSet::empty(limits).replaced(publication)?);
        let graph = Normalizer::normalize(
            index_revision,
            source_revision,
            tree_revision,
            &publications,
            previous,
        )?;
        let relationships = RelationshipStore::build(&graph);
        Ok(Self {
            graph,
            relationships,
            syntax_provider: ProviderId::new(SYNTAX_PROVIDER_ID)
                .map_err(SyntaxPublicationError::Identity)?,
        })
    }

    /// Returns captured normalized graph.
    pub(crate) const fn graph(&self) -> &NormalizedGraph {
        &self.graph
    }

    /// Returns the symbol reference adjacency built from this revision's graph.
    pub(crate) const fn relationships(&self) -> &RelationshipStore {
        &self.relationships
    }

    /// Assembles readable symbol for syntax provider-local identity.
    pub(crate) fn assembled(&self, provider_symbol: &str) -> Option<AssembledSymbol> {
        let reference = ContributionReference::new(
            self.syntax_provider.clone(),
            ProviderSymbolId::new(provider_symbol).ok()?,
        );
        let record = self.graph.record_for(&reference)?;
        SymbolAssembler::assemble(
            &self.graph,
            record,
            std::slice::from_ref(&self.syntax_provider),
        )
    }
}

/// Semantic publication failure inside workspace index build.
#[derive(Debug)]
pub(crate) enum WorkspaceSemanticError {
    Revision(RevisionError),
    Syntax(SyntaxPublicationError),
    /// One document's declarations refused publication; `path` names the document,
    /// so the index can leave that one file out instead of failing the build.
    Document {
        path: ProjectPath,
        error: SyntaxPublicationError,
    },
    Publication(PublicationError),
    Normalization(ContributionError),
}

impl WorkspaceSemanticError {
    /// The document whose declarations were refused, when the failure names one.
    pub(crate) const fn document_path(&self) -> Option<&ProjectPath> {
        match self {
            Self::Document { path, .. } => Some(path),
            _ => None,
        }
    }

    /// The Contribution the syntax publication refused for one document, when that
    /// is the failure.
    pub(crate) const fn refused_contribution(&self) -> Option<&ContributionError> {
        match self {
            Self::Document {
                error: SyntaxPublicationError::Contribution(error),
                ..
            } => Some(error),
            _ => None,
        }
    }
}

impl fmt::Display for WorkspaceSemanticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Revision(error) => error.fmt(formatter),
            Self::Syntax(error) => error.fmt(formatter),
            Self::Document { path, error } => write!(formatter, "{}: {error}", path.as_str()),
            Self::Publication(error) => error.fmt(formatter),
            Self::Normalization(error) => error.fmt(formatter),
        }
    }
}

impl StdError for WorkspaceSemanticError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Revision(error) => Some(error),
            Self::Syntax(error) | Self::Document { error, .. } => Some(error),
            Self::Publication(error) => Some(error),
            Self::Normalization(error) => Some(error),
        }
    }
}

impl From<RevisionError> for WorkspaceSemanticError {
    fn from(error: RevisionError) -> Self {
        Self::Revision(error)
    }
}

impl From<SyntaxPublicationError> for WorkspaceSemanticError {
    fn from(error: SyntaxPublicationError) -> Self {
        Self::Syntax(error)
    }
}

impl From<PublicationError> for WorkspaceSemanticError {
    fn from(error: PublicationError) -> Self {
        Self::Publication(error)
    }
}

impl From<ContributionError> for WorkspaceSemanticError {
    fn from(error: ContributionError) -> Self {
        Self::Normalization(error)
    }
}

impl From<SourceUnitIdError> for WorkspaceSemanticError {
    fn from(error: SourceUnitIdError) -> Self {
        Self::Syntax(SyntaxPublicationError::from(error))
    }
}

#[cfg(test)]
mod tests {
    use rift_core::ProjectPath;
    use rift_syntax::{SyntaxSource, registry};

    use super::{WorkspaceSemanticError, WorkspaceSemantics};

    fn document() -> rift_syntax::SyntaxDocument {
        let path = ProjectPath::new("src/lib.rs").expect("path");
        registry::provider_for_extension("rs")
            .expect("rust provider")
            .analyze(SyntaxSource {
                path: &path,
                text: "pub fn beacon() {}\n",
            })
            .expect("document")
    }

    #[test]
    fn syntax_graph_assembles_existing_symbol_identity() {
        let document = document();
        let semantics = WorkspaceSemantics::build([&document], 7, None).expect("semantics");
        let identity = "rift://symbol/rust/src/lib.rs/beacon";
        let assembled = semantics.assembled(identity).expect("assembled symbol");
        assert_eq!(
            assembled.identity().map(rift_core::SymbolId::as_str),
            Some(identity)
        );
        assert_eq!(assembled.index_revision().get(), 7);
        assert_eq!(semantics.graph().records().len(), 1);
    }

    #[test]
    fn zero_revision_is_typed_failure() {
        let error =
            WorkspaceSemantics::build(std::iter::empty(), 0, None).expect_err("zero revision");
        assert!(matches!(error, WorkspaceSemanticError::Revision(_)));
        assert!(std::error::Error::source(&error).is_some());
        assert!(!error.to_string().is_empty());
    }

    /// The syntax publication is the only publication a build carries, so the set holds
    /// one provider whatever the documents are.
    #[test]
    fn test_build_publishes_the_syntax_provider_alone() {
        let document = document();
        let semantics = WorkspaceSemantics::build([&document], 3, None).expect("semantics");
        let syntax =
            rift_core::ProviderId::new(rift_syntax::SYNTAX_PROVIDER_ID).expect("provider identity");
        assert!(semantics.graph().publications().provider(&syntax).is_some());
        assert_eq!(semantics.graph().publications().provider_count(), 1);
    }

    #[test]
    fn test_source_unit_error_converts_to_the_syntax_variant() {
        let unit_error = rift_core::SourceUnitId::parse("not-a-source-unit")
            .expect_err("a malformed unit identity is refused");
        let error = WorkspaceSemanticError::from(unit_error);
        assert!(matches!(error, WorkspaceSemanticError::Syntax(_)));
        assert!(!error.to_string().is_empty());
        assert!(std::error::Error::source(&error).is_some());
    }
}
