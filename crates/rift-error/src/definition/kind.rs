//! Field kind table and field flags.

/// Maps one field kind to its accepted bound and `ErrorValue` constructor.
///
/// `@flags` folds the declaration flags into `[display sensitive]` first: `hidden`
/// clears display and `sensitive` sets redaction. The kind arms then re-enter
/// `__rift_error_field!` at `@emit`. Kind words are the `type` names of `errors.toml`;
/// `source` and `cause` are its two roles, whose fields carry `type = "error"` and
/// `type = "rift_error"`.
#[doc(hidden)]
#[macro_export]
macro_rules! __rift_error_kind {
    (@flags [$display:tt $sensitive:tt] [hidden $($flag:ident)*] $($rest:tt)*) => {
        $crate::__rift_error_kind! { @flags [false $sensitive] [$($flag)*] $($rest)* }
    };
    (@flags [$display:tt $sensitive:tt] [sensitive $($flag:ident)*] $($rest:tt)*) => {
        $crate::__rift_error_kind! { @flags [$display true] [$($flag)*] $($rest)* }
    };
    (@flags $flags:tt [$flag:ident $($other:ident)*] $kind:ident $field:ident $($rest:tt)*) => {
        ::core::compile_error!(::core::concat!(
            "field `",
            ::core::stringify!($field),
            "` has unknown flag `",
            ::core::stringify!($flag),
            "`; expected `hidden` or `sensitive`",
        ));
    };
    (@flags $flags:tt [] string $($rest:tt)*) => {
        $crate::__rift_error_field! {
            @emit bound [::core::fmt::Display] convert display flags $flags $($rest)*
        }
    };
    (@flags $flags:tt [] bool $($rest:tt)*) => {
        $crate::__rift_error_field! {
            @emit bound [::core::borrow::Borrow<bool>] convert bool_value flags $flags $($rest)*
        }
    };
    (@flags $flags:tt [] integer $($rest:tt)*) => {
        $crate::__rift_error_field! {
            @emit bound [$crate::IntoInteger] convert integer flags $flags $($rest)*
        }
    };
    (@flags $flags:tt [] unsigned $($rest:tt)*) => {
        $crate::__rift_error_field! {
            @emit bound [$crate::IntoUnsigned] convert unsigned flags $flags $($rest)*
        }
    };
    (@flags $flags:tt [] pid $($rest:tt)*) => {
        $crate::__rift_error_field! {
            @emit bound [::core::borrow::Borrow<u32>] convert pid flags $flags $($rest)*
        }
    };
    (@flags $flags:tt [] port $($rest:tt)*) => {
        $crate::__rift_error_field! {
            @emit bound [::core::borrow::Borrow<u16>] convert port flags $flags $($rest)*
        }
    };
    (@flags $flags:tt [] path $($rest:tt)*) => {
        $crate::__rift_error_field! {
            @emit bound [::core::convert::AsRef<::std::path::Path>] convert path
            flags $flags $($rest)*
        }
    };
    (@flags $flags:tt [] duration $($rest:tt)*) => {
        $crate::__rift_error_field! {
            @emit bound [::core::borrow::Borrow<::core::time::Duration>] convert duration
            flags $flags $($rest)*
        }
    };
    (@flags $flags:tt [] source $($rest:tt)*) => {
        $crate::__rift_error_field! {
            @emit bound [
                ::core::convert::Into<
                    ::std::boxed::Box<
                        dyn ::core::error::Error
                            + ::core::marker::Send
                            + ::core::marker::Sync
                            + 'static
                    >
                >
            ]
            convert source flags $flags $($rest)*
        }
    };
    (@flags $flags:tt [] cause $($rest:tt)*) => {
        $crate::__rift_error_field! {
            @emit bound [$crate::IntoRiftError] convert cause flags $flags $($rest)*
        }
    };
    (@flags $flags:tt [] $kind:ident $field:ident $($rest:tt)*) => {
        ::core::compile_error!(::core::concat!(
            "field `",
            ::core::stringify!($field),
            "` has unknown kind `",
            ::core::stringify!($kind),
            "`; expected one of string, bool, integer, unsigned, pid, port, path, ",
            "duration, source, cause",
        ));
    };
}
