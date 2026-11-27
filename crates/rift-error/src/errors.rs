macro_rules! registry_namespace {
    () => {
        "rift"
    };
}

#[doc(hidden)]
pub const REGISTRY_NAMESPACE: &str = registry_namespace!();

/// An error and its message, action, and evidence fields.
#[cfg(any(feature = "registry", test))]
#[cfg_attr(feature = "registry", derive(serde::Serialize))]
#[derive(Debug)]
pub struct ErrorDefinition {
    /// Stable identity of the error.
    pub slug: &'static str,
    /// Message template before evidence is substituted.
    pub message: &'static str,
    /// Action template before evidence is substituted.
    pub action: &'static str,
    /// Evidence fields accepted by the error builder.
    pub fields: &'static [FieldDefinition],
}

/// An evidence field as declared in the registry.
#[cfg(any(feature = "registry", test))]
#[cfg_attr(feature = "registry", derive(serde::Serialize))]
#[derive(Debug)]
pub struct FieldDefinition {
    /// Field name used in evidence and in templates.
    pub name: &'static str,
    /// Field kind accepted by the builder.
    pub kind: &'static str,
    /// Whether the builder can complete without this field.
    pub optional: bool,
    /// Whether the field appears in rendered text.
    pub display: bool,
    /// Whether the renderer redacts the field value.
    pub sensitive: bool,
}

macro_rules! error_namespace {
    (
        $namespace:ident;
        $(
            $name:ident {
                message = $message:literal,
                action = $action:literal,
                fields = {
                    $($field:ident: $presence:ident($kind:ident $(, $flag:ident)* $(,)?)),*
                    $(,)?
                },
            }
        )+
    ) => {
        $(
            crate::__rift_error_definition! {
                $name,
                slug = ::core::concat!(
                    registry_namespace!(),
                    ".",
                    ::core::stringify!($namespace),
                    ".",
                    ::core::stringify!($name),
                ),
                message = $message,
                action = $action,
                fields = { $($field: $presence($kind $(, $flag)*)),* },
            }
        )+

        pub(super) const SLUGS: &[&str] = &[$($name::SLUG.as_str()),+];

        #[cfg(any(feature = "registry", test))]
        pub(super) const DEFINITIONS: &[super::ErrorDefinition] = &[
            $(
                super::ErrorDefinition {
                    slug: $name::SLUG.as_str(),
                    message: $message,
                    action: $action,
                    fields: &[
                        $(
                            crate::__rift_error_kind! {
                                @flags [true false] [$($flag)*]
                                @registry $kind $field $presence
                            }
                        ),*
                    ],
                },
            )+
        ];
    };
}

macro_rules! error_modules {
    ($($namespace:ident),+ $(,)?) => {
        $(
            #[doc = ::core::concat!("Registered errors under `", registry_namespace!(), ".", ::core::stringify!($namespace), "`.")]
            pub mod $namespace;
        )+

        #[doc(hidden)]
        pub const REGISTERED_SLUGS: &[&str] = &registry_slugs::<{ 0 $(+ $namespace::SLUGS.len())+ }>(
            &[$($namespace::SLUGS),+],
        );

        /// Returns the definitions from every namespace in slug order.
        #[cfg(any(feature = "registry", test))]
        pub fn registry() -> impl Iterator<Item = &'static ErrorDefinition> {
            [$($namespace::DEFINITIONS),+].into_iter().flatten()
        }
    };
}

const fn registry_slugs<const COUNT: usize>(
    namespaces: &[&[&'static str]],
) -> [&'static str; COUNT] {
    let mut slugs = [""; COUNT];
    let mut namespace = 0;
    let mut written = 0;
    while namespace < namespaces.len() {
        let mut remaining = namespaces[namespace];
        while let Some((&slug, rest)) = remaining.split_first() {
            slugs[written] = slug;
            written += 1;
            remaining = rest;
        }
        namespace += 1;
    }
    slugs
}

error_modules!(
    analysis,
    cli,
    client,
    core,
    history,
    history_store,
    index,
    lsp,
    mcp,
    provider,
    ranking,
    search,
    server,
    syntax,
    tracing,
);

#[cfg(test)]
mod tests;
