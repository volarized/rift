//! Runtime support for registered Rift errors.

extern crate self as rift_error;

mod evidence;
mod generated_support;
mod representation;
mod runtime;

/// Ambient context constructors.
pub mod ctx;

/// Generated error builders, grouped by registry namespace.
pub mod errors {
    include!("generated.rs");
}

pub use evidence::EvidenceFor;
pub use representation::IntoRiftError;
pub use runtime::{
    BuilderCore, CAUSE_DEPTH_MAX, ErrorContext, ErrorSlug, ErrorValue, FieldSet, IntoInteger,
    IntoUnsigned, OptionalFieldSet, RiftError, Set, SourceView, Unset, causes,
};

#[doc(hidden)]
#[macro_export]
macro_rules! __rift_evidence_impl {
    (@run $source:ty, $($target_path:ident)::+, $tag:path,
        $target:ident, $target_value:ident, $source_value:ident, $state:ty,
        [$($bounds:tt)*], [$($steps:tt)*];) => {
        impl<$target> $crate::EvidenceFor<$target, $state, $tag> for $source
        where
            $($bounds)*
        {
            fn apply_evidence(&self, $target_value: $target) -> $state {
                let $source_value: &$source = self;
                $($steps)*
                $target_value
            }
        }
    };
    (@run $source:ty, $($target_path:ident)::+, $tag:path,
        $target:ident, $target_value:ident, $source_value:ident, $state:ty,
        [$($bounds:tt)*], [$($steps:tt)*]; $field:ident = @field $source_field:ident; $($rest:tt)*) => {
        $crate::__rift_evidence_impl!(@run
            $source, $($target_path)::+, $tag,
            $target, $target_value, $source_value,
            <$($target_path)::+::$field::Field as
                $($target_path)::+::$field::Set<$state>>::Output,
            [$($bounds)* $($target_path)::+::$field::Field: $($target_path)::+::$field::Set<$state>,],
            [$($steps)*
                let $target_value = <$($target_path)::+::$field::Field as
                    $($target_path)::+::$field::Set<$state>>::set(
                        $target_value,
                        $($target_path)::+::$field::value(&$source_value.$source_field),
                    );];
            $($rest)*
        );
    };
    (@run $source:ty, $($target_path:ident)::+, $tag:path,
        $target:ident, $target_value:ident, $source_value:ident, $state:ty,
        [$($bounds:tt)*], [$($steps:tt)*]; $field:ident = $extract:expr; $($rest:tt)*) => {
        $crate::__rift_evidence_impl!(@run
            $source, $($target_path)::+, $tag,
            $target, $target_value, $source_value,
            <$($target_path)::+::$field::Field as
                $($target_path)::+::$field::Set<$state>>::Output,
            [$($bounds)* $($target_path)::+::$field::Field: $($target_path)::+::$field::Set<$state>,],
            [$($steps)*
                let $target_value = <$($target_path)::+::$field::Field as
                    $($target_path)::+::$field::Set<$state>>::set(
                        $target_value,
                        {
                            let extract = |source: &$source| ($extract)(source);
                            $($target_path)::+::$field::value(extract($source_value))
                        },
                    );];
            $($rest)*
        );
    };
}

/// Declares evidence extraction for one domain type and registered error.
#[macro_export]
macro_rules! evidence {
    ($source:ty => $($target_path:ident)::+ { $($field:ident = $source_field:ident),+ $(,)? }) => {
        $crate::__rift_evidence_impl!(@run
            $source, $($target_path)::+, $($target_path)::+::EvidenceTag,
            __RiftEvidenceTarget, __rift_evidence_target, __rift_evidence_source,
            __RiftEvidenceTarget, [], [];
            $($field = @field $source_field;)*
        );
    };
    ($source:ty => $($target_path:ident)::+ { $($field:ident => |$value:ident| $extract:expr),+ $(,)? }) => {
        $crate::__rift_evidence_impl!(@run
            $source, $($target_path)::+, $($target_path)::+::EvidenceTag,
            __RiftEvidenceTarget, __rift_evidence_target, __rift_evidence_source,
            __RiftEvidenceTarget, [], [];
            $($field = |$value: &$source| $extract;)*
        );
    };
}

/// Registers an extension that converts registered errors to one representation.
#[macro_export]
macro_rules! format {
    ($method:ident -> $representation:ident using $convert:path) => {
        /// Converts complete registered errors to configured representations.
        pub trait RiftErrorFormatExt {
            /// Converts this complete error to the configured representation.
            fn $method(self) -> $representation;
        }

        impl<T> RiftErrorFormatExt for T
        where
            T: $crate::IntoRiftError,
        {
            fn $method(self) -> $representation {
                $convert(self.into_rift_error())
            }
        }

        /// Returns configured representations through function result types.
        pub trait RiftErrorFailExt: Sized {
            /// Returns this representation through a function result type.
            fn fail<T>(self) -> Result<T, Self> {
                Err(self)
            }
        }

        impl RiftErrorFailExt for $representation {}
    };
    ($method:ident -> $representation:ident as $extension:ident,
        $failure_extension:ident using $convert:path) => {
        /// Converts complete registered errors to configured representations.
        pub trait $extension {
            /// Converts this complete error to the configured representation.
            fn $method(self) -> $representation;
        }

        impl<T> $extension for T
        where
            T: $crate::IntoRiftError,
        {
            fn $method(self) -> $representation {
                $convert(self.into_rift_error())
            }
        }

        /// Returns configured representations through function result types.
        pub trait $failure_extension: Sized {
            /// Returns this representation through a function result type.
            fn fail<T>(self) -> Result<T, Self> {
                Err(self)
            }
        }

        impl $failure_extension for $representation {}
    };
}

#[cfg(test)]
mod generated_api_tests {
    #![allow(private_interfaces, unreachable_pub)]

    use super::{ErrorSlug, RiftError, errors};

    struct QueryEvidence {
        subject: String,
        field: String,
    }

    crate::evidence! {
        QueryEvidence => errors::ranking::query_length {
            subject = subject,
            field = field,
        }
    }

    crate::evidence! {
        QueryEvidence => errors::ranking::query_empty {
            subject => |evidence| evidence.subject.clone(),
        }
    }

    #[test]
    fn evidence_macro_supports_partial_required_fields_and_option_values() {
        let evidence = QueryEvidence {
            subject: "query".to_owned(),
            field: "text".to_owned(),
        };
        let error = errors::ranking::query_length()
            .evidence(&evidence)
            .subject("explicit query")
            .limit(4_usize)
            .required(5_usize)
            .error();
        assert_eq!(error.slug(), ErrorSlug::new("rift.ranking.query_length"));
        assert!(
            error
                .context()
                .any(|(key, value)| { key == "subject" && value == "explicit query" })
        );

        let error = errors::ranking::query_empty()
            .evidence(Some(evidence))
            .error();
        assert_eq!(error.slug(), ErrorSlug::new("rift.ranking.query_empty"));

        let absent: Option<QueryEvidence> = None;
        let error = errors::ranking::query_empty().evidence(absent).error();
        assert_eq!(error.slug(), ErrorSlug::new("rift.ranking.query_empty"));
    }

    #[derive(Debug)]
    pub struct TestRepresentation;

    impl std::fmt::Display for TestRepresentation {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("test representation")
        }
    }

    impl std::error::Error for TestRepresentation {}

    #[derive(Debug)]
    pub struct CliRepresentation;

    impl std::fmt::Display for CliRepresentation {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("cli representation")
        }
    }

    impl std::error::Error for CliRepresentation {}

    fn convert(_: RiftError) -> TestRepresentation {
        TestRepresentation
    }

    fn convert_cli(_: RiftError) -> CliRepresentation {
        CliRepresentation
    }

    crate::format! {
        mcp -> TestRepresentation using convert
    }

    crate::format! {
        cli -> CliRepresentation as CliErrorFormatExt, CliErrorFailExt using convert_cli
    }

    #[test]
    fn format_macro_registers_conversion_and_failure_methods() {
        use self::{RiftErrorFailExt as _, RiftErrorFormatExt as _};
        let result: Result<(), TestRepresentation> = errors::ranking::query_empty().mcp().fail();
        assert!(result.is_err());
    }

    #[test]
    fn format_registrations_coexist_for_multiple_representations() {
        use self::{
            CliErrorFailExt as _, CliErrorFormatExt as _, RiftErrorFailExt as _,
            RiftErrorFormatExt as _,
        };

        let mcp: TestRepresentation = errors::ranking::query_empty().mcp();
        let cli: CliRepresentation = errors::ranking::query_empty().cli();
        assert_eq!(mcp.to_string(), "test representation");
        let mcp_result: Result<(), TestRepresentation> = mcp.fail();
        let cli_result: Result<(), CliRepresentation> = cli.fail();
        assert!(mcp_result.is_err());
        assert!(cli_result.is_err());
    }
}
