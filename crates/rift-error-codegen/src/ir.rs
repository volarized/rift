#[derive(Clone, Debug)]
pub(crate) struct Registry {
    pub namespace: String,
    pub errors: Vec<Error>,
}

#[derive(Clone, Debug)]
pub(crate) struct Error {
    pub path: Vec<String>,
    pub slug: String,
    pub message: String,
    pub action: String,
    pub fields: Vec<Field>,
}

#[derive(Clone, Debug)]
pub(crate) struct Field {
    pub name: String,
    pub kind: FieldKind,
    pub optional: bool,
    pub display: bool,
    pub sensitive: bool,
}

/// Validated field kind: a plain `type`, or the `source` and `cause` roles.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FieldKind {
    String,
    Bool,
    Integer,
    Unsigned,
    Pid,
    Port,
    Path,
    Duration,
    Source,
    Cause,
}

impl FieldKind {
    /// Kind word `__rift_error_definition!` reads in a field declaration.
    pub(crate) const fn word(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Bool => "bool",
            Self::Integer => "integer",
            Self::Unsigned => "unsigned",
            Self::Pid => "pid",
            Self::Port => "port",
            Self::Path => "path",
            Self::Duration => "duration",
            Self::Source => "source",
            Self::Cause => "cause",
        }
    }
}
