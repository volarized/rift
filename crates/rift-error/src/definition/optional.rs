//! `maybe_<field>` setter of an optional field.

/// Emits `maybe_<field>` for an optional field and nothing for a required one.
///
/// `maybe_<field>` sets the field from `Some` and clears it on `None`; the builder type
/// stays unchanged, so optional evidence never gates completion.
#[doc(hidden)]
#[macro_export]
macro_rules! __rift_error_optional {
    (required $($plan:tt)*) => {};
    (
        optional $field:ident $maybe:ident bound [$($bound:tt)+] states [$($state:ident,)*]
        index [$($index:tt)*] flags [$display:tt $sensitive:tt]
    ) => {
        impl<$($state),*> super::Builder<$($state),*> {
            #[doc = ::core::concat!(
                "Sets `",
                ::core::stringify!($field),
                "` evidence from `Some` or clears it on `None`.",
            )]
            #[must_use]
            pub fn $maybe<Value>(mut self, value: ::core::option::Option<Value>) -> Self
            where
                Value: $($bound)+,
            {
                self.core.set_optional(
                    <[()]>::len(&[$($index),*]),
                    ::core::stringify!($field),
                    value.map(self::value),
                    $display,
                    $sensitive,
                );
                self
            }
        }
    };
}
