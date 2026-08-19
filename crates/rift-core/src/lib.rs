//! Pure domain vocabulary and correctness primitives for Rift.

mod error;
mod identity;
mod limits;
mod measurement;
mod path;

pub mod constants;

pub use error::{ErrorContext, ErrorDescriptor, ErrorName, ErrorRegistry, RetryPolicy, RiftError};
pub use identity::{
    CompositionId, CompositionRevision, CursorId, IdError, IndexRevision, ModelId, ModelRevision,
    ProviderId, ProviderRevision, RevisionError, SourceResolverId, SourceResolverIdError,
    SourceResolverIdViolation, SourceRevision, SourceUnitId, SourceUnitIdError,
    SourceUnitIdErrorKind, SymbolId, TreeRevision, WorkspaceId,
};
pub use limits::{BudgetExhausted, LoopBudget};
pub use measurement::{
    ClockRegression, MonotonicClock, PerformanceMeasurement, SystemMonotonicClock,
};
/// Defines a property-like method as one visible field projection.
///
/// Receiver and projection remain explicit at the call site. Use this only for
/// cheap, parameterless reads that cannot fail or perform I/O.
#[macro_export]
macro_rules! property {
    ($(#[$attribute:meta])* $visibility:vis const fn $name:ident($($receiver:tt)*) -> $return_type:ty = $value:expr;) => {
        $(#[$attribute])*
        #[must_use]
        $visibility const fn $name($($receiver)*) -> $return_type {
            $value
        }
    };
    ($(#[$attribute:meta])* $visibility:vis fn $name:ident($($receiver:tt)*) -> $return_type:ty = $value:expr;) => {
        $(#[$attribute])*
        #[must_use]
        $visibility fn $name($($receiver)*) -> $return_type {
            $value
        }
    };
}
pub use path::{PathError, PathKind, PathViolation, ProjectPath, SourcePath};

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

#[cfg(test)]
mod tests {
    #[derive(Debug)]
    struct PropertyFixture {
        count: usize,
        name: String,
    }

    impl PropertyFixture {
        crate::property! {
            /// Returns copied count.
            pub(crate) const fn count(&self) -> usize = self.count;
        }

        crate::property! {
            /// Returns borrowed name.
            pub(crate) fn name(&self) -> &str = self.name.as_str();
        }
    }

    #[test]
    fn test_property_projects_copy_and_borrowed_fields() {
        let fixture = PropertyFixture {
            count: 2,
            name: "rift".into(),
        };

        assert_eq!(fixture.count(), 2);
        assert_eq!(fixture.name(), "rift");
    }
}
