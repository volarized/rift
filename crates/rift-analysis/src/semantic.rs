use std::sync::Arc;

use rift_core::{
    ContributionReference, IndexRevision, ProjectPath, ProviderId, ProviderRevision,
    ProviderSymbolId, SourceRevision, TreeRevision,
};
use rift_error::{ErrorContext, ErrorValue, RiftError};
use rift_provider::{
    AssembledSymbol, NormalizedGraph, Normalizer, PROVIDERS_MAX_DEFAULT, PublicationLimits,
    PublicationSet, SymbolAssembler,
};
use rift_syntax::{
    DocumentPlacement, SYNTAX_PROVIDER_ID, SyntaxDocument, SyntaxFacts, SyntaxPublicationBuilder,
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
/// Returns [`RiftError`] when `declarations_max` is zero.
fn publication_limits(declarations_max: usize) -> Result<PublicationLimits, RiftError> {
    Ok(PublicationLimits::new(
        PROVIDERS_MAX_DEFAULT,
        declarations_max,
        declarations_max,
    )?)
}

/// One syntax document and the placement its declarations are filed under.
#[derive(Debug)]
pub struct PlacedDocument<'a> {
    /// The parsed document.
    pub document: &'a SyntaxDocument,
    /// The unit, origin, and identity path its declarations carry.
    pub placement: DocumentPlacement,
}

/// Path-independent syntax facts and the placement their declarations are filed under.
#[derive(Debug)]
pub struct PlacedFacts<'a> {
    /// Compact syntax facts for one parsed source file.
    pub facts: &'a SyntaxFacts,
    /// Project-relative source path used for error and bound reporting.
    pub path: &'a ProjectPath,
    /// The unit, origin, and identity path the declarations carry.
    pub placement: DocumentPlacement,
}

/// Contribution graph captured by one workspace index publication.
#[derive(Debug)]
pub struct WorkspaceSemantics {
    graph: NormalizedGraph,
    relationships: RelationshipStore,
    syntax_provider: ProviderId,
}

/// One build's captured semantics and documents not included in publication.
#[derive(Debug)]
pub struct BuiltSemantics {
    /// The graph built over the documents that fit.
    pub semantics: WorkspaceSemantics,
    /// The documents the publication had no room for, in the order they were offered.
    pub beyond_declaration_bound: Vec<ProjectPath>,
    /// Documents whose Contribution the publication refused.
    pub refused_contributions: Vec<(ProjectPath, RiftError)>,
}

impl WorkspaceSemantics {
    /// Builds one syntax publication and one normalized graph over one file set.
    ///
    /// Every document takes the project placement, [`DocumentPlacement::project`];
    /// the build itself is [`Self::build_placed`].
    ///
    /// # Errors
    ///
    /// Returns a typed error when publication or graph validation fails.
    pub fn build<'a>(
        documents: impl IntoIterator<Item = &'a SyntaxDocument>,
        declarations_max: usize,
        revision: u64,
        previous: Option<&NormalizedGraph>,
    ) -> Result<BuiltSemantics, RiftError> {
        let placed = documents
            .into_iter()
            .map(|document| {
                Ok(PlacedDocument {
                    document,
                    placement: DocumentPlacement::project(document)?,
                })
            })
            .collect::<Result<Vec<_>, RiftError>>()?;
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
    ///
    /// # Errors
    ///
    /// Returns a typed error when publication or graph validation fails.
    pub fn build_placed(
        documents: &[PlacedDocument<'_>],
        declarations_max: usize,
        revision: u64,
        previous: Option<&NormalizedGraph>,
    ) -> Result<BuiltSemantics, RiftError> {
        let facts = documents
            .iter()
            .map(|placed| PlacedFacts {
                facts: placed.document.facts(),
                path: placed.document.path(),
                placement: placed.placement.clone(),
            })
            .collect::<Vec<_>>();
        Self::build_facts_placed(&facts, declarations_max, revision, previous)
    }

    /// Builds one project publication from path-independent syntax facts.
    /// Contribution refusals are returned by path so the workspace index can leave all
    /// refused files out in one build pass.
    ///
    /// # Errors
    ///
    /// Returns a typed error when one source path, publication, or graph is invalid.
    pub fn build_project_facts<'a>(
        documents: impl IntoIterator<Item = (&'a SyntaxFacts, &'a ProjectPath)>,
        declarations_max: usize,
        revision: u64,
        previous: Option<&NormalizedGraph>,
    ) -> Result<BuiltSemantics, RiftError> {
        let placed = documents
            .into_iter()
            .map(|(facts, path)| {
                Ok(PlacedFacts {
                    facts,
                    path,
                    placement: DocumentPlacement::project_path(path)?,
                })
            })
            .collect::<Result<Vec<_>, RiftError>>()?;
        Self::build_facts_placed_inner(&placed, declarations_max, revision, previous, true)
    }

    /// Builds one publication and graph over compact syntax facts with explicit source paths.
    ///
    /// # Errors
    ///
    /// Returns a typed error when publication or graph validation fails.
    pub fn build_facts_placed(
        documents: &[PlacedFacts<'_>],
        declarations_max: usize,
        revision: u64,
        previous: Option<&NormalizedGraph>,
    ) -> Result<BuiltSemantics, RiftError> {
        Self::build_facts_placed_inner(documents, declarations_max, revision, previous, false)
    }

    fn build_facts_placed_inner(
        documents: &[PlacedFacts<'_>],
        declarations_max: usize,
        revision: u64,
        previous: Option<&NormalizedGraph>,
        collect_refused_contributions: bool,
    ) -> Result<BuiltSemantics, RiftError> {
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
        let mut refused_contributions = Vec::new();
        for (offered, placed) in documents.iter().enumerate() {
            if placed.facts.symbols().len() > builder.declarations_remaining() {
                beyond_declaration_bound = documents[offered..]
                    .iter()
                    .map(|left_out| left_out.path.clone())
                    .collect();
                break;
            }
            match builder.add_facts_placed(placed.facts, &placed.placement) {
                Ok(()) => {}
                Err(error)
                    if collect_refused_contributions
                        && error.slug().as_str().starts_with("rift.core.contribution_") =>
                {
                    let error = error.with(ErrorContext::new(
                        "path",
                        ErrorValue::path(placed.path.as_str()),
                    ));
                    refused_contributions.push((placed.path.clone(), error));
                }
                Err(error) => {
                    return error
                        .with(ErrorContext::new(
                            "path",
                            ErrorValue::path(placed.path.as_str()),
                        ))
                        .fail();
                }
            }
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
                syntax_provider: ProviderId::new(SYNTAX_PROVIDER_ID)?,
            },
            beyond_declaration_bound,
            refused_contributions,
        })
    }

    /// Returns captured normalized graph.
    #[must_use]
    pub const fn graph(&self) -> &NormalizedGraph {
        &self.graph
    }

    /// Returns the symbol reference adjacency built from this revision's graph.
    #[must_use]
    pub const fn relationships(&self) -> &RelationshipStore {
        &self.relationships
    }

    /// Assembles readable symbol for syntax provider-local identity.
    #[must_use]
    pub fn assembled(&self, provider_symbol: &str) -> Option<AssembledSymbol> {
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

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use rift_core::ProjectPath;
    use rift_syntax::{DocumentPlacement, SyntaxLimits, SyntaxSource, registry};

    use super::{PlacedDocument, PlacedFacts, WorkspaceSemantics, publication_limits};

    fn document() -> rift_syntax::SyntaxDocument {
        let path = ProjectPath::new("src/lib.rs").expect("path");
        registry::provider_for_extension("rs")
            .expect("rust provider")
            .analyze(
                SyntaxSource {
                    path: &path,
                    text: "pub fn beacon() {}\n",
                },
                SyntaxLimits::default(),
            )
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
            .analyze(
                SyntaxSource {
                    path: &path,
                    text: &text,
                },
                SyntaxLimits::default(),
            )
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
    fn compact_facts_build_same_graph_as_complete_document() {
        let document = document();
        let placement = DocumentPlacement::project(&document).expect("project placement");
        let complete = WorkspaceSemantics::build_placed(
            &[PlacedDocument {
                document: &document,
                placement: placement.clone(),
            }],
            1,
            7,
            None,
        )
        .expect("complete document semantics");
        let facts = document.shared_facts();
        let compact = WorkspaceSemantics::build_facts_placed(
            &[PlacedFacts {
                facts: &facts,
                path: document.path(),
                placement,
            }],
            1,
            7,
            None,
        )
        .expect("compact facts semantics");

        assert_eq!(
            compact.semantics.graph().records(),
            complete.semantics.graph().records()
        );
        assert_eq!(
            compact.beyond_declaration_bound,
            complete.beyond_declaration_bound
        );
    }

    #[test]
    fn zero_revision_has_registered_identity() {
        let error =
            WorkspaceSemantics::build(std::iter::empty(), 1, 0, None).expect_err("zero revision");
        assert_eq!(error.slug().as_str(), "rift.core.revision_zero");
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
        assert_eq!(
            error.slug().as_str(),
            "rift.provider.publication_zero_limit"
        );
        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn test_source_unit_error_keeps_registered_identity() {
        let unit_error = rift_core::SourceUnitId::parse("not-a-source-unit")
            .expect_err("a malformed unit identity is refused");
        assert!(
            unit_error
                .slug()
                .as_str()
                .starts_with("rift.core.source_unit_id_")
        );
        assert!(!unit_error.to_string().is_empty());
    }
}
