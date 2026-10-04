//! Setter named after its field.

/// Emits the inherent setter named after the field.
///
/// The setter converts its argument once through the field's `value` function and
/// returns the builder type the field's `Set` transition names.
#[doc(hidden)]
#[macro_export]
macro_rules! __rift_error_setter {
    ($field:ident bound [$($bound:tt)+] states [$($state:ident,)*]) => {
        impl<$($state),*> super::Builder<$($state),*> {
            #[doc = ::core::concat!("Sets `", ::core::stringify!($field), "` evidence.")]
            #[must_use]
            pub fn $field<Value>(self, value: Value) -> <Field as Set<Self>>::Output
            where
                Value: $($bound)+,
            {
                <Field as Set<Self>>::set(self, self::value(value))
            }
        }
    };
}
