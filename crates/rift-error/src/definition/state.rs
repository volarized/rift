//! Required-field typestate planning for one declaration.

/// Plans builder type parameters and the transition each field setter performs.
///
/// `@partition` collects required fields in declaration order; each one becomes a
/// type parameter named `<Field>State`. `@walk` then visits every field once,
/// carrying the parameters already passed (`done`) and those still ahead (`pending`):
/// a required field's setter maps its own parameter to `Set` and keeps the others, and
/// an optional field's setter keeps them all. The finished plan is pasted into
/// identifiers once and handed to `__rift_error_definition!`'s `@emit` arm.
#[doc(hidden)]
#[macro_export]
macro_rules! __rift_error_state {
    (
        @partition $meta:tt required [$($required:ident)*] all $all:tt
        rest [$field:ident required $spec:tt $($rest:tt)*]
    ) => {
        $crate::__rift_error_state! {
            @partition $meta required [$($required)* $field] all $all rest [$($rest)*]
        }
    };
    (
        @partition $meta:tt required $required:tt all $all:tt
        rest [$field:ident optional $spec:tt $($rest:tt)*]
    ) => {
        $crate::__rift_error_state! {
            @partition $meta required $required all $all rest [$($rest)*]
        }
    };
    (
        @partition $meta:tt required $required:tt all $all:tt
        rest [$field:ident $presence:ident $spec:tt $($rest:tt)*]
    ) => {
        ::core::compile_error!(::core::concat!(
            "field `",
            ::core::stringify!($field),
            "` has unknown presence `",
            ::core::stringify!($presence),
            "`; expected `required` or `optional`",
        ));
    };
    (@partition $meta:tt required [$($required:ident)*] all [$($all:tt)*] rest []) => {
        $crate::__rift_error_state! {
            @walk $meta states [$($required)*]
            done [] pending [$($required)*] count [] items []
            fields [$($all)*]
        }
    };
    (
        @walk $meta:tt states $states:tt
        done [$($done:ident)*] pending [$current:ident $($pending:ident)*]
        count [$($count:tt)*] items [$($items:tt)*]
        fields [$field:ident required $spec:tt $($rest:tt)*]
    ) => {
        $crate::__rift_error_state! {
            @walk $meta states $states
            done [$($done)* $current] pending [$($pending)*]
            count [$($count)* ()]
            items [
                $($items)*
                $crate::__rift_error_field! {
                    $field required $spec
                    index [$($count)*]
                    states [
                        $([<$done:camel State>],)*
                        [<$current:camel State>],
                        $([<$pending:camel State>],)*
                    ]
                    output [
                        $([<$done:camel State>],)*
                        $crate::Set,
                        $([<$pending:camel State>],)*
                    ]
                    maybe [<maybe_ $field>]
                }
            ]
            fields [$($rest)*]
        }
    };
    (
        @walk $meta:tt states $states:tt
        done [$($done:ident)*] pending [$($pending:ident)*]
        count [$($count:tt)*] items [$($items:tt)*]
        fields [$field:ident optional $spec:tt $($rest:tt)*]
    ) => {
        $crate::__rift_error_state! {
            @walk $meta states $states
            done [$($done)*] pending [$($pending)*]
            count [$($count)* ()]
            items [
                $($items)*
                $crate::__rift_error_field! {
                    $field optional $spec
                    index [$($count)*]
                    states [$([<$done:camel State>],)* $([<$pending:camel State>],)*]
                    output [$([<$done:camel State>],)* $([<$pending:camel State>],)*]
                    maybe [<maybe_ $field>]
                }
            ]
            fields [$($rest)*]
        }
    };
    (
        @walk [$name:ident $slug:literal $message:literal $action:literal]
        states [$($state:ident)*] done $done:tt pending []
        count [$($count:tt)*] items [$($items:tt)*] fields []
    ) => {
        $crate::__rift_paste! {
            $crate::__rift_error_definition! {
                @emit $name $slug $message $action
                count [$($count)*]
                states [$([<$state:camel State>] => $crate::Set,)*]
                items { $($items)* }
            }
        }
    };
}
