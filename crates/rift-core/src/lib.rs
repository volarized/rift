//! Pure domain vocabulary and correctness primitives for Rift.

pub mod acceptance;
mod capture;
mod configuration;
mod digest;
mod identity;
mod limits;
mod name;
mod path;
mod semantic;

pub mod constants;
pub mod line;

pub use capture::{CapturedStream, STREAM_READ_BYTES, STREAM_TOTAL_BYTES_MAX};
pub use configuration::{
    LanguageFileSelection, LanguageFileSelections, SourceVisibility, TextFileInclusion,
    configuration_violation_error, is_absolute_program,
};
pub use digest::FileDigest;
pub use identity::{
    CompositionId, CompositionRevision, IndexRevision, ModelId, ModelRevision,
    ParsedSymbolIdentity, ProviderId, ProviderRevision, ProviderSymbolId, SourceResolverId,
    SourceRevision, SourceUnitId, SymbolId, SymbolIdentityError, TreeRevision, WorkspaceId,
    encode_path, parse_symbol_identity, symbol_identity,
};
pub use limits::{BudgetExhausted, LoopBudget};
pub use name::is_canonical_ascii_name;
pub use path::{PathKind, PathViolation, ProjectPath, SourcePath};
pub use semantic::{
    CONTRIBUTION_EVIDENCE_MAX, CONTRIBUTION_FACTS_MAX, CONTRIBUTION_NAMESPACE_BYTES_MAX,
    CONTRIBUTION_NAMESPACES_MAX, Contribution, ContributionBuilder, ContributionKey,
    ContributionOrigin, ContributionReference, ContributionRelationship, ContributionViolation,
    DeclarationBinding, Documentation, DocumentationFormat, EquivalenceEvidence, ExactKind,
    ExtensionKey, ExtensionValue, Extensions, Language, NodeId, PROVIDER_SYMBOL_ID_BYTES_MAX,
    PackageIdentity, PortableSymbolFacts, ReferenceRole, RelationshipKind, SemanticReference,
    Signature, SourceApplicability, SourceKind, SourceLocation, SourceRange, SymbolFacet,
    SymbolRecord, SymbolResolution, TypeBinding, is_portable_name,
};

/// Iterates while charging one unit to a loop budget before each body execution.
///
/// Budget and iterator expressions are each evaluated once. `break`, `continue`,
/// and `return` retain normal `for`-loop behavior inside body.
#[macro_export]
macro_rules! bounded_for {
    ($pattern:pat_param in $iterator:expr, budget = $budget:expr, $body:block) => {{
        let mut __rift_budget = $budget;
        let __rift_iterator = $iterator;
        'rift_bounded: {
            for $pattern in __rift_iterator {
                if let Err(__rift_exhausted) = __rift_budget.consume() {
                    break 'rift_bounded ::core::result::Result::Err(__rift_exhausted);
                }
                $body
            }
            ::core::result::Result::Ok(())
        }
    }};
}
