//! One field module of a registered error.

/// Emits one field module: its `Field` marker, `value` conversion, `Set` transition,
/// and setters.
///
/// The entry arm folds the field's flags, then asks `__rift_error_kind!` for the
/// kind's bound and `ErrorValue` constructor, which re-enters at `@emit`. `states` is
/// the builder's parameter list before the setter runs and `output` the list after.
#[doc(hidden)]
#[macro_export]
macro_rules! __rift_error_field {
    (
        $field:ident $presence:ident ($kind:ident $(, $flag:ident)* $(,)?)
        $($plan:tt)*
    ) => {
        $crate::__rift_error_kind! {
            @flags [true false] [$($flag)*]
            $kind $field $presence $($plan)*
        }
    };
    (
        @emit bound [$($bound:tt)+] convert $convert:ident flags [$display:tt $sensitive:tt]
        $field:ident $presence:ident
        index [$($index:tt)*]
        states [$($state:ident,)*]
        output [$($output:ty,)*]
        maybe $maybe:ident
    ) => {
        #[doc = ::core::concat!("Evidence field `", ::core::stringify!($field), "`.")]
        pub mod $field {
            pub use $crate::FieldSet as Set;

            /// Names this field in builder transitions and evidence mappings.
            pub struct Field;

            /// Converts one value to this field's registry evidence.
            pub fn value<Value>(value: Value) -> $crate::ErrorValue
            where
                Value: $($bound)+,
            {
                $crate::ErrorValue::$convert(value)
            }

            impl<$($state),*> Set<super::Builder<$($state),*>> for Field {
                type Output = super::Builder<$($output),*>;

                fn set(
                    mut target: super::Builder<$($state),*>,
                    value: $crate::ErrorValue,
                ) -> Self::Output {
                    target.core.set(
                        <[()]>::len(&[$($index),*]),
                        ::core::stringify!($field),
                        value,
                        $display,
                        $sensitive,
                    );
                    super::Builder {
                        core: target.core,
                        marker: ::core::marker::PhantomData,
                    }
                }
            }

            $crate::__rift_error_setter! {
                $field bound [$($bound)+] states [$($state,)*]
            }

            $crate::__rift_error_optional! {
                $presence $field $maybe bound [$($bound)+] states [$($state,)*]
                index [$($index)*] flags [$display $sensitive]
            }
        }
    };
}
