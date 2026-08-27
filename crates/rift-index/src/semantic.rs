use std::error::Error as StdError;
use std::fmt;
use std::sync::Arc;

use rift_core::{
    ContributionError, ContributionReference, IndexRevision, ProviderId, ProviderRevision,
    ProviderSymbolId, RevisionError, SourceRevision, TreeRevision,
};
use rift_provider::{
    AssembledSymbol, NormalizedGraph, Normalizer, PublicationError, PublicationLimits,
    PublicationSet, SymbolAssembler,
};
use rift_syntax::{
    SYNTAX_PROVIDER_ID, SyntaxDocument, SyntaxPublicationBuilder, SyntaxPublicationError,
};

/// Contribution graph captured by one workspace index publication.
#[derive(Debug)]
pub(crate) struct WorkspaceSemantics {
    graph: NormalizedGraph,
    syntax_provider: ProviderId,
}

impl WorkspaceSemantics {
    /// Builds syntax publication and normalized graph over one file set.
    pub(crate) fn build<'a>(
        documents: impl IntoIterator<Item = &'a SyntaxDocument>,
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
        for document in documents {
            builder.add_document(document)?;
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
        Ok(Self {
            graph,
            syntax_provider: ProviderId::new(SYNTAX_PROVIDER_ID)
                .map_err(SyntaxPublicationError::Identity)?,
        })
    }

    /// Returns captured normalized graph.
    pub(crate) const fn graph(&self) -> &NormalizedGraph {
        &self.graph
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
    Publication(PublicationError),
    Normalization(ContributionError),
}

impl fmt::Display for WorkspaceSemanticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Revision(error) => error.fmt(formatter),
            Self::Syntax(error) => error.fmt(formatter),
            Self::Publication(error) => error.fmt(formatter),
            Self::Normalization(error) => error.fmt(formatter),
        }
    }
}

impl StdError for WorkspaceSemanticError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Revision(error) => Some(error),
            Self::Syntax(error) => Some(error),
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
}
