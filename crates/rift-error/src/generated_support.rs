//! Shared macro bodies for generated registered-error code.

/// Emits one registered error from its metadata and planned field transitions.
#[doc(hidden)]
#[macro_export]
macro_rules! __rift_error_definition {
    (
        error $name:ident;
        imports { $($imports:item)* }
        metadata [$slug:literal, $message:literal, $action:literal, $count:literal];
        builder $builder:ident;
        states [$($state:ident),*];
        complete [$($complete:ty),*];
        fields $fields:tt
    ) => {
        #[allow(non_camel_case_types, missing_docs, unused_parens)]
        pub mod $name {
            $($imports)*
            pub use super::{FieldSet, OptionalFieldSet};

            /// Stable registry identity for this error.
            pub const SLUG: $crate::ErrorSlug = $crate::ErrorSlug::new($slug);

            pub struct $builder<$($state = $crate::Unset),*> {
                pub(super) core: $crate::BuilderCore,
                pub(super) marker: ::core::marker::PhantomData<($($state,)*)>,
            }

            pub struct EvidenceTag;
            pub type EvidenceInput = $builder;
            pub type EvidenceOutput = $builder<$($complete),*>;

            $crate::__rift_error_field! {
                @fields builder $builder; states [$($state),*]; fields $fields
            }

            impl<$($state),*> $builder<$($state),*> {
                $crate::__rift_error_setter! { @fields $fields }

                #[must_use]
                pub fn with(mut self, context: $crate::ErrorContext) -> Self {
                    self.core.with(context);
                    self
                }

                pub fn evidence<E, O>(self, evidence: E) -> O
                where
                    E: $crate::EvidenceFor<Self, O, EvidenceTag>,
                {
                    evidence.apply_evidence(self)
                }
            }

            impl $builder<$($complete),*> {
                #[must_use]
                pub fn error(self) -> $crate::RiftError {
                    self.finish()
                }

                /// Return this registered error as a failed result.
                ///
                /// # Errors
                ///
                /// Always returns this registered error.
                pub fn fail<T>(self) -> Result<T, $crate::RiftError> {
                    Err(self.finish())
                }

                fn finish(self) -> $crate::RiftError {
                    self.core.finish()
                }
            }

            impl $crate::IntoRiftError for $builder<$($complete),*> {
                fn into_rift_error(self) -> $crate::RiftError {
                    self.finish()
                }
            }
        }

        #[must_use]
        pub fn $name() -> $name::$builder {
            $name::$builder {
                core: $crate::BuilderCore::new(
                    $crate::ErrorSlug::new($slug), $message, $action, $count,
                ),
                marker: ::core::marker::PhantomData,
            }
        }
    };
}

/// Emits field modules using the state transitions planned by the generator.
#[doc(hidden)]
#[macro_export]
macro_rules! __rift_error_field {
    (@fields builder $builder:ident; states $states:tt;
        fields { $($field:ident $descriptor:tt)* }
    ) => {
        $(
            $crate::__rift_error_field! {
                @field builder $builder; states $states; field $field $descriptor
            }
        )*
    };
    (
        @field builder $builder:ident; states [$($state:ident),*];
        field $field:ident {
            imports { $($imports:item)* }
            output [$($output:ty),*];
            index $index:literal;
            key $key:literal;
            flags [$display:literal, $sensitive:literal];
            bound [$($bound:tt)+];
            value $value:ident => [$conversion:expr];
            optional [$($optional:ident)?];
        }
    ) => {
        pub mod $field {
            $($imports)*
            pub use $crate::FieldSet as Set;

            pub struct Field;

            pub fn value<__RiftErrorValue>($value: __RiftErrorValue) -> $crate::ErrorValue
            where
                __RiftErrorValue: $($bound)+,
            {
                $conversion
            }

            pub fn optional_value<__RiftErrorValue>(
                value: Option<__RiftErrorValue>,
            ) -> Option<$crate::ErrorValue>
            where
                __RiftErrorValue: $($bound)+,
            {
                value.map(self::value)
            }

            impl<$($state),*> Set<$builder<$($state),*>> for Field {
                type Output = $builder<$($output),*>;

                fn set(
                    mut target: $builder<$($state),*>,
                    value: $crate::ErrorValue,
                ) -> Self::Output {
                    target.core.set($index as usize, $key, value, $display, $sensitive);
                    $builder::<$($output),*> {
                        core: target.core,
                        marker: ::core::marker::PhantomData,
                    }
                }
            }

            $crate::__rift_error_field! {
                @optional [$($optional)?]
                builder $builder; states [$($state),*];
                index $index; key $key; flags [$display, $sensitive];
            }
        }
    };
    (@optional [] $($rest:tt)*) => {};
    (
        @optional [$optional:ident]
        builder $builder:ident; states [$($state:ident),*];
        index $index:literal; key $key:literal; flags [$display:literal, $sensitive:literal];
    ) => {
        pub use $crate::OptionalFieldSet as SetOptional;

        impl<$($state),*> SetOptional<$builder<$($state),*>> for Field {
            type Output = $builder<$($state),*>;

            fn set_optional(
                mut target: $builder<$($state),*>,
                value: Option<$crate::ErrorValue>,
            ) -> Self::Output {
                target.core.set_optional($index as usize, $key, value, $display, $sensitive);
                target
            }
        }
    };
}

/// Emits inherent setters from the same field descriptors as the field modules.
#[doc(hidden)]
#[macro_export]
macro_rules! __rift_error_setter {
    (@fields { $($field:ident $descriptor:tt)* }) => {
        $( $crate::__rift_error_setter! { @field $field $descriptor } )*
    };
    (
        @field $field:ident {
            imports $imports:tt
            output $output:tt;
            index $index:literal;
            key $key:literal;
            flags $flags:tt;
            bound [$($bound:tt)+];
            value $value:ident => [$conversion:expr];
            optional [$($optional:ident)?];
        }
    ) => {
        pub fn $field<__RiftErrorValue>(
            self,
            value: __RiftErrorValue,
        ) -> <$field::Field as $field::Set<Self>>::Output
        where
            __RiftErrorValue: $($bound)+,
        {
            <$field::Field as $field::Set<Self>>::set(self, $field::value(value))
        }

        $crate::__rift_error_setter! {
            @optional [$($optional)?] $field; bound [$($bound)+];
        }
    };
    (@optional [] $field:ident; bound [$($bound:tt)+];) => {};
    (@optional [$optional:ident] $field:ident; bound [$($bound:tt)+];) => {
        pub fn $optional<__RiftErrorValue>(
            self,
            value: Option<__RiftErrorValue>,
        ) -> <$field::Field as $field::SetOptional<Self>>::Output
        where
            __RiftErrorValue: $($bound)+,
        {
            <$field::Field as $field::SetOptional<Self>>::set_optional(
                self, $field::optional_value(value),
            )
        }
    };
}
