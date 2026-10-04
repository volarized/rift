//! Expansion of one compact registered-error declaration.
//!
//! The generator writes only what an error is: its name, slug, message, action, and
//! field declarations. These macros own how a registered error is implemented: the
//! builder typestate, field modules, setters, and completion methods.
//!
//! Responsibilities split by module:
//!
//! - `definition`: declaration grammar, the error module, and the constructor
//! - `state`: required-field typestate parameters and per-field transitions
//! - `field`: one field module with its value conversion and required setter
//! - `kind`: the field kind table and the `hidden` and `sensitive` flags
//! - `setter`: the inherent setter named after the field
//! - `optional`: the `maybe_<field>` setter of an optional field

mod field;
mod kind;
mod optional;
mod setter;
mod state;

/// Expands one registered error from its compact declaration.
///
/// ```
/// mod analysis {
///     rift_error::__rift_error_definition!(
///         context7_malformed,
///         slug = "rift.analysis.context7_malformed",
///         message = "context7.json has an invalid JSON shape",
///         action = "correct context7.json shape and retry",
///         fields = {
///             file: required(path),
///             key: optional(string),
///         },
///     );
/// }
/// let error = analysis::context7_malformed()
///     .file("context7.json")
///     .maybe_key(None::<&str>)
///     .error();
/// assert_eq!(error.slug().as_str(), "rift.analysis.context7_malformed");
/// ```
///
/// Each field reads `name: presence(kind, flags...)`. Presence is `required` or
/// `optional`. Kind is one of `string`, `bool`, `integer`, `unsigned`, `pid`, `port`,
/// `path`, `duration`, `source`, or `cause`. Flags are `hidden` (the field stays out of
/// rendered text) and `sensitive` (the value is redacted).
///
/// A required field adds one builder type parameter that starts at `Unset` and becomes
/// `Set` when the field's setter runs; `error()`, `fail()`, and `IntoRiftError` exist only
/// once every parameter is `Set`. An optional field adds a `maybe_<field>` setter and
/// leaves the builder type unchanged.
///
/// Expansion recurses once per field and once per flag, so a declaration stays far
/// below the default recursion limit. Every setter evaluates its argument once.
///
/// Completion requires every required field:
///
/// ```compile_fail,E0599
/// mod analysis {
///     rift_error::__rift_error_definition!(
///         context7_malformed,
///         slug = "rift.analysis.context7_malformed",
///         message = "context7.json has an invalid JSON shape",
///         action = "correct context7.json shape and retry",
///         fields = { file: required(path), key: optional(string) },
///     );
/// }
/// let _ = analysis::context7_malformed().maybe_key(Some("key")).error();
/// ```
///
/// A kind outside the table is rejected:
///
/// ```compile_fail
/// rift_error::__rift_error_definition!(
///     malformed,
///     slug = "rift.analysis.malformed",
///     message = "malformed",
///     action = "retry",
///     fields = { count: required(count) },
/// );
/// ```
///
/// So are an unknown presence and an unknown flag:
///
/// ```compile_fail
/// rift_error::__rift_error_definition!(
///     malformed,
///     slug = "rift.analysis.malformed",
///     message = "malformed",
///     action = "retry",
///     fields = { key: maybe(string) },
/// );
/// ```
///
/// ```compile_fail
/// rift_error::__rift_error_definition!(
///     malformed,
///     slug = "rift.analysis.malformed",
///     message = "malformed",
///     action = "retry",
///     fields = { key: optional(string, secret) },
/// );
/// ```
///
/// Each kind keeps its bound:
///
/// ```compile_fail,E0277
/// mod analysis {
///     rift_error::__rift_error_definition!(
///         port_rejected,
///         slug = "rift.analysis.port_rejected",
///         message = "port rejected",
///         action = "retry",
///         fields = { port: required(port) },
///     );
/// }
/// let _ = analysis::port_rejected().port("8080").error();
/// ```
#[doc(hidden)]
#[macro_export]
macro_rules! __rift_error_definition {
    (
        $name:ident,
        slug = $slug:literal,
        message = $message:literal,
        action = $action:literal,
        fields = { $($field:ident : $presence:ident ( $($spec:tt)* )),* $(,)? }
        $(,)?
    ) => {
        $crate::__rift_error_state! {
            @partition [$name $slug $message $action]
            required []
            all [$($field $presence ($($spec)*))*]
            rest [$($field $presence ($($spec)*))*]
        }
    };
    (
        @emit $name:ident $slug:literal $message:literal $action:literal
        count [$($count:tt)*]
        states [$($state:ident => $complete:ty,)*]
        items { $($items:tt)* }
    ) => {
        #[doc = ::core::concat!("Builder types for `", $slug, "`.")]
        pub mod $name {
            /// Stable registry identity for this error.
            pub const SLUG: $crate::ErrorSlug = $crate::ErrorSlug::new($slug);

            /// Error builder; each type parameter tracks one required field.
            pub struct Builder<$($state = $crate::Unset),*> {
                pub(super) core: $crate::BuilderCore,
                pub(super) marker: ::core::marker::PhantomData<($($state,)*)>,
            }

            /// Names this error in evidence mappings.
            pub struct EvidenceTag;

            $($items)*

            impl<$($state),*> Builder<$($state),*> {
                /// Adds ambient execution context.
                #[must_use]
                pub fn with(mut self, context: $crate::ErrorContext) -> Self {
                    self.core.with(context);
                    self
                }

                /// Applies evidence declared with `evidence!` for this error.
                pub fn evidence<E, O>(self, evidence: E) -> O
                where
                    E: $crate::EvidenceFor<Self, O, EvidenceTag>,
                {
                    evidence.apply_evidence(self)
                }
            }

            impl Builder<$($complete),*> {
                /// Creates the registered error from complete evidence.
                #[must_use]
                pub fn error(self) -> $crate::RiftError {
                    self.core.finish()
                }

                /// Returns this registered error as a failed result.
                ///
                /// # Errors
                ///
                /// Always returns this registered error.
                pub fn fail<T>(self) -> ::core::result::Result<T, $crate::RiftError> {
                    ::core::result::Result::Err(self.core.finish())
                }
            }

            impl $crate::IntoRiftError for Builder<$($complete),*> {
                fn into_rift_error(self) -> $crate::RiftError {
                    self.core.finish()
                }
            }
        }

        #[doc = ::core::concat!("Starts a `", $slug, "` builder.")]
        #[must_use]
        pub fn $name() -> $name::Builder {
            $name::Builder {
                core: $crate::BuilderCore::new(
                    $crate::ErrorSlug::new($slug),
                    $message,
                    $action,
                    <[()]>::len(&[$($count),*]),
                ),
                marker: ::core::marker::PhantomData,
            }
        }
    };
}

#[cfg(test)]
mod tests;
