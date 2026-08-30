//! Pure domain vocabulary and correctness primitives for Rift.

mod capture;
mod configuration;
mod error;
mod identity;
mod limits;
mod measurement;
mod name;
mod path;
mod semantic;

pub mod constants;
pub mod line;

pub use capture::{CapturedStream, STREAM_READ_BYTES, STREAM_TOTAL_BYTES_MAX};
pub use configuration::{
    LanguageFileSelection, LanguageFileSelections, SourceVisibility, TextFileInclusion,
};
pub use error::{
    CliCode, Error, ErrorCode, ErrorContext, ErrorDescriptor, ErrorName, Fault, LimitEvidence,
    RetryDirective, RiftError, fault_label, render_failure,
};
pub use identity::{
    CompositionId, CompositionRevision, IdError, IdFault, IndexRevision, ModelId, ModelRevision,
    ProviderId, ProviderRevision, ProviderSymbolId, RevisionError, RevisionFault, SourceResolverId,
    SourceResolverIdError, SourceResolverIdFault, SourceResolverIdViolation, SourceRevision,
    SourceUnitId, SourceUnitIdError, SourceUnitIdFault, SymbolId, TreeRevision, WorkspaceId,
    encode_path, symbol_identity,
};
pub use limits::{BudgetExhausted, LoopBudget};
pub use measurement::{
    ClockRegression, MonotonicClock, PerformanceMeasurement, SystemMonotonicClock,
};
pub use name::is_canonical_ascii_name;
pub use path::{PathError, PathFault, PathKind, PathViolation, ProjectPath, SourcePath};
pub use semantic::{
    CONTRIBUTION_EVIDENCE_MAX, CONTRIBUTION_FACTS_MAX, CONTRIBUTION_NAMESPACE_BYTES_MAX,
    CONTRIBUTION_NAMESPACES_MAX, Contribution, ContributionBuilder, ContributionError,
    ContributionFault, ContributionKey, ContributionOrigin, ContributionReference,
    ContributionRelationship, ContributionViolation, DeclarationBinding, Documentation,
    DocumentationFormat, EquivalenceEvidence, ExactKind, ExtensionKey, ExtensionValue, Extensions,
    Language, NodeId, PROVIDER_SYMBOL_ID_BYTES_MAX, PortableSymbolFacts, ReferenceRole,
    RelationshipKind, SemanticReference, Signature, SourceApplicability, SourceKind,
    SourceLocation, SourceRange, SymbolFacet, SymbolRecord, SymbolResolution, TypeBinding,
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

/// Evaluates a block once and returns its value with elapsed monotonic time.
#[macro_export]
macro_rules! measure_elapsed {
    ($clock:expr, $operation:expr, $body:block) => {{
        let __rift_clock = &$clock;
        let __rift_operation = $operation;
        let __rift_started = $crate::MonotonicClock::now(__rift_clock);
        let __rift_value = $body;
        let __rift_finished = $crate::MonotonicClock::now(__rift_clock);
        $crate::PerformanceMeasurement::between(__rift_operation, __rift_started, __rift_finished)
            .map(|__rift_measurement| (__rift_value, __rift_measurement))
    }};
}
