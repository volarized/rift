use std::error::Error as StdError;
use std::fmt;
use std::sync::Arc;

use rift_core::{
    ContributionError, ContributionReference, IndexRevision, ProjectPath, ProviderId,
    ProviderRevision, ProviderSymbolId, RevisionError, SourceRevision, SourceUnitIdError,
    TreeRevision,
};
use rift_provider::{
    AssembledSymbol, NormalizedGraph, Normalizer, PROVIDERS_MAX_DEFAULT, PublicationError,
    PublicationLimits, PublicationSet, SymbolAssembler,
};
use rift_syntax::{
    DocumentPlacement, SYNTAX_PROVIDER_ID, SyntaxDocument, SyntaxPublicationBuilder,
    SyntaxPublicationError,
};

use crate::relationship::RelationshipStore;

/// Publication bounds for a build that holds at most `declarations_max` declarations.
///
/// The workspace index publishes one provider, so the set bound is the per-provider bound.
/// The caller states the capacity it owns: a workspace build passes the `[source]` table's
/// `declarations`, a package analysis the provider crate's own default.
///
/// # Errors
///
/// Returns [`WorkspaceSemanticError`] when `declarations_max` is zero.
fn publication_limits(
    declarations_max: usize,
) -> Result<PublicationLimits, WorkspaceSemanticError> {
    Ok(PublicationLimits::new(
        PROVIDERS_MAX_DEFAULT,
        declarations_max,
        declarations_max,
    )?)
}

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

/// One build's captured semantics with the documents its declaration bound left out.
#[derive(Debug)]
pub(crate) struct BuiltSemantics {
    /// The graph built over the documents that fit.
    pub(crate) semantics: WorkspaceSemantics,
    /// The documents the publication had no room for, in the order they were offered.
    pub(crate) beyond_declaration_bound: Vec<ProjectPath>,
}

impl WorkspaceSemantics {
    /// Builds one syntax publication and one normalized graph over one file set.
    ///
    /// Every document takes the project placement, [`DocumentPlacement::project`];
    /// the build itself is [`Self::build_placed`].
    pub(crate) fn build<'a>(
        documents: impl IntoIterator<Item = &'a SyntaxDocument>,
        declarations_max: usize,
        revision: u64,
        previous: Option<&NormalizedGraph>,
    ) -> Result<BuiltSemantics, WorkspaceSemanticError> {
        let placed = documents
            .into_iter()
            .map(|document| {
                Ok(PlacedDocument {
                    document,
                    placement: DocumentPlacement::project(document)?,
                })
            })
            .collect::<Result<Vec<_>, SyntaxPublicationError>>()?;
        Self::build_placed(&placed, declarations_max, revision, previous)
    }

    /// Builds the publication and one normalized graph over documents the caller placed.
    ///
    /// The publication holds `declarations_max` declarations. A document that would cross
    /// that bound stops the pass: it and every document after it are named in
    /// [`BuiltSemantics::beyond_declaration_bound`], so the index leaves them out and
    /// serves what fits instead of refusing the whole build. Stopping at the first document
    /// that does not fit is the rule the file bound already follows, so which documents
    /// survive does not depend on how the walk happened to order them.
    ///
    /// A document whose declarations the syntax publication refuses for any other reason
    /// names itself in the error, so the index can leave that one file out instead.
    pub(crate) fn build_placed(
        documents: &[PlacedDocument<'_>],
        declarations_max: usize,
        revision: u64,
        previous: Option<&NormalizedGraph>,
    ) -> Result<BuiltSemantics, WorkspaceSemanticError> {
        let index_revision = IndexRevision::new(revision)?;
        let source_revision = SourceRevision::new(revision)?;
        let tree_revision = TreeRevision::new(revision)?;
        let provider_revision = ProviderRevision::new(revision)?;
        let limits = publication_limits(declarations_max)?;
        let mut builder = SyntaxPublicationBuilder::new(
            provider_revision,
            source_revision,
            tree_revision,
            limits,
        )?;
        let mut beyond_declaration_bound = Vec::new();
        for (offered, placed) in documents.iter().enumerate() {
            if placed.document.symbols().len() > builder.declarations_remaining() {
                beyond_declaration_bound = documents[offered..]
                    .iter()
                    .map(|left_out| left_out.document.path().clone())
                    .collect();
                break;
            }
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
        Ok(BuiltSemantics {
            semantics: Self {
                graph,
                relationships,
                syntax_provider: ProviderId::new(SYNTAX_PROVIDER_ID)
                    .map_err(SyntaxPublicationError::Identity)?,
            },
            beyond_declaration_bound,
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
    use std::fmt::Write as _;

    use rift_core::ProjectPath;
    use rift_syntax::{SyntaxSource, registry};

    use super::{WorkspaceSemanticError, WorkspaceSemantics, publication_limits};

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

    /// One document declaring `count` functions, filed at `path`.
    fn declaring(path: &str, count: usize) -> rift_syntax::SyntaxDocument {
        let path = ProjectPath::new(path.to_owned()).expect("path");
        let mut text = String::new();
        for index in 0..count {
            writeln!(text, "pub fn beacon_{index}() {{}}").expect("a string write succeeds");
        }
        registry::provider_for_extension("rs")
            .expect("rust provider")
            .analyze(SyntaxSource {
                path: &path,
                text: &text,
            })
            .expect("document")
    }

    #[test]
    fn syntax_graph_assembles_existing_symbol_identity() {
        let document = document();
        let semantics = WorkspaceSemantics::build([&document], 1, 7, None)
            .expect("semantics")
            .semantics;
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
            WorkspaceSemantics::build(std::iter::empty(), 1, 0, None).expect_err("zero revision");
        assert!(matches!(error, WorkspaceSemanticError::Revision(_)));
        assert!(std::error::Error::source(&error).is_some());
        assert!(!error.to_string().is_empty());
    }

    /// The syntax publication is the only publication a build carries, so the set holds
    /// one provider whatever the documents are.
    #[test]
    fn test_build_publishes_the_syntax_provider_alone() {
        let document = document();
        let semantics = WorkspaceSemantics::build([&document], 1, 3, None)
            .expect("semantics")
            .semantics;
        let syntax =
            rift_core::ProviderId::new(rift_syntax::SYNTAX_PROVIDER_ID).expect("provider identity");
        assert!(semantics.graph().publications().provider(&syntax).is_some());
        assert_eq!(semantics.graph().publications().provider_count(), 1);
    }

    /// The bound is the whole publication's, so the documents that fit are published and
    /// the rest are named. A build that refused the set outright would leave the caller
    /// with no index at all.
    #[test]
    fn test_documents_past_the_declaration_bound_are_named_and_the_rest_publish() {
        let first = declaring("src/first.rs", 3);
        let second = declaring("src/second.rs", 3);
        let third = declaring("src/third.rs", 3);
        let built = WorkspaceSemantics::build([&first, &second, &third], 4, 11, None)
            .expect("the publication keeps the documents that fit");
        assert_eq!(
            built
                .beyond_declaration_bound
                .iter()
                .map(ProjectPath::as_str)
                .collect::<Vec<_>>(),
            ["src/second.rs", "src/third.rs"],
            "the pass stops at the first document that does not fit"
        );
        assert_eq!(built.semantics.graph().records().len(), 3);
    }

    /// A build under the bound names nothing, so the index never leaves a file out for a
    /// bound it did not cross.
    #[test]
    fn test_a_build_within_the_declaration_bound_names_no_document() {
        let document = declaring("src/first.rs", 3);
        let built = WorkspaceSemantics::build([&document], 3, 5, None).expect("semantics");
        assert!(built.beyond_declaration_bound.is_empty());
        assert_eq!(built.semantics.graph().records().len(), 3);
    }

    #[test]
    fn test_zero_declaration_bound_is_a_typed_failure() {
        let error = publication_limits(0).expect_err("a zero bound is refused");
        assert!(matches!(error, WorkspaceSemanticError::Publication(_)));
        assert!(!error.to_string().is_empty());
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
