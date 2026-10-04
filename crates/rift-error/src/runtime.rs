use crate::IntoRiftError;

use std::{borrow::Borrow, error::Error, fmt, path::Path, sync::Arc, time::Duration};

/// Sources one walk of a failure's `source` chain visits, at most.
///
/// A chain deeper than this is cut, so a source that cycles back on itself
/// cannot keep a render walking forever.
pub const CAUSE_DEPTH_MAX: usize = 8;

/// The causes below `error`, outermost first, each rendered once.
///
/// A carrier whose `source` renders the same text as itself contributes no
/// repeated line. The walk visits at most [`CAUSE_DEPTH_MAX`] sources.
#[must_use]
pub fn causes(error: &(dyn Error + 'static)) -> Vec<String> {
    let mut rendered = Vec::new();
    let mut previous = error.to_string();
    let mut source = error.source();
    for _ in 0..CAUSE_DEPTH_MAX {
        let Some(cause) = source else {
            break;
        };
        let text = cause.to_string();
        if text != previous {
            rendered.push(text.clone());
            previous = text;
        }
        source = cause.source();
    }
    rendered
}

/// Converts an unsigned primitive accepted by an `unsigned` registry field.
pub trait IntoUnsigned {
    /// Renders the admitted unsigned value for registry evidence.
    fn into_unsigned(self) -> String;
}

/// Converts a signed primitive accepted by an `integer` registry field.
pub trait IntoInteger {
    /// Renders the admitted signed value for registry evidence.
    fn into_integer(self) -> String;
}

macro_rules! scalar_conversions {
    ($trait:ident, $method:ident: $($type:ty),+ $(,)?) => {
        $(
            impl $trait for $type {
                fn $method(self) -> String { self.to_string() }
            }
            impl $trait for &$type {
                fn $method(self) -> String { self.to_string() }
            }
        )+
    };
}

scalar_conversions!(IntoUnsigned, into_unsigned: u8, u16, u32, u64, u128, usize);
scalar_conversions!(IntoInteger, into_integer: i8, i16, i32, i64, i128, isize);

/// Builder state before required evidence is set.
pub struct Unset;

/// Builder state after required evidence is set.
pub struct Set;

/// Sets required generated evidence on a builder.
pub trait FieldSet<Target> {
    /// Builder state after the field is set.
    type Output;

    /// Sets evidence and returns the resulting builder.
    fn set(target: Target, value: ErrorValue) -> Self::Output;
}

/// Sets or clears optional generated evidence on a builder.
pub trait OptionalFieldSet<Target> {
    /// Builder state after the optional field is set.
    type Output;

    /// Sets or clears evidence and returns the resulting builder.
    fn set_optional(target: Target, value: Option<ErrorValue>) -> Self::Output;
}

/// Stable registry identity for one error.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ErrorSlug(&'static str);

impl ErrorSlug {
    /// Creates a registry identity from a generated string literal.
    #[must_use]
    pub const fn new(value: &'static str) -> Self {
        Self(value)
    }

    /// Returns the stable registry identity.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for ErrorSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// Field value retained by a registered error.
#[derive(Clone)]
pub enum ErrorValue {
    /// Human-readable field value.
    Display(String),
    /// Underlying Rust error exposed through `Error::source`.
    Source(Arc<SourceView>),
    /// Another registered Rift error carried as the cause.
    Cause(Arc<RiftError>),
}

impl ErrorValue {
    /// Converts a displayable value to its human-readable form.
    #[must_use]
    pub fn display(value: impl fmt::Display) -> Self {
        Self::Display(value.to_string())
    }

    /// Stores an admitted unsigned primitive.
    #[must_use]
    pub fn unsigned(value: impl IntoUnsigned) -> Self {
        Self::Display(value.into_unsigned())
    }

    /// Stores an admitted signed primitive.
    #[must_use]
    pub fn integer(value: impl IntoInteger) -> Self {
        Self::Display(value.into_integer())
    }

    /// Stores a boolean registry field.
    #[must_use]
    pub fn bool_value(value: impl Borrow<bool>) -> Self {
        Self::Display(value.borrow().to_string())
    }

    /// Stores an admitted process identifier.
    #[must_use]
    pub fn pid(value: impl Borrow<u32>) -> Self {
        Self::Display(value.borrow().to_string())
    }

    /// Stores an admitted port.
    #[must_use]
    pub fn port(value: impl Borrow<u16>) -> Self {
        Self::Display(value.borrow().to_string())
    }

    /// Stores an already formatted value.
    #[must_use]
    pub fn formatted(value: impl Into<String>) -> Self {
        Self::Display(value.into())
    }

    /// Formats a path with its display form.
    #[must_use]
    pub fn path(value: impl AsRef<Path>) -> Self {
        Self::Display(value.as_ref().display().to_string())
    }

    /// Formats a duration with Rust's human-readable duration form.
    #[must_use]
    pub fn duration(value: impl Borrow<Duration>) -> Self {
        Self::Display(format!("{:?}", value.borrow()))
    }

    /// Stores an underlying error as source.
    #[must_use]
    pub fn source(value: impl Into<Box<dyn Error + Send + Sync + 'static>>) -> Self {
        let inner: Arc<dyn Error + Send + Sync + 'static> = Arc::from(value.into());
        Self::Source(Arc::new(SourceView::new(inner)))
    }

    /// Stores another Rift error as cause.
    #[must_use]
    pub fn cause(value: impl IntoRiftError) -> Self {
        Self::Cause(Arc::new(value.into_rift_error()))
    }

    fn rendered(&self) -> String {
        match self {
            Self::Display(value) => value.clone(),
            Self::Source(value) => value.to_string(),
            Self::Cause(value) => value.to_string(),
        }
    }

    fn redact_source_view(&mut self) {
        let value = std::mem::replace(self, Self::Display(String::new()));
        *self = match value {
            Self::Source(mut source) => {
                Arc::make_mut(&mut source).redacted = true;
                Self::Source(source)
            }
            Self::Cause(cause) => Self::Source(Arc::new(SourceView {
                inner: cause,
                redacted: true,
            })),
            Self::Display(value) => Self::Display(value),
        };
    }
}

impl fmt::Debug for ErrorValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Display(value) => f.debug_tuple("Display").field(value).finish(),
            Self::Source(_) => f.write_str("Source([hidden])"),
            Self::Cause(_) => f.write_str("Cause([registered error])"),
        }
    }
}

#[derive(Clone)]
#[doc(hidden)]
pub struct SourceView {
    inner: Arc<dyn Error + Send + Sync + 'static>,
    redacted: bool,
}

impl SourceView {
    fn new(inner: Arc<dyn Error + Send + Sync + 'static>) -> Self {
        Self {
            inner,
            redacted: false,
        }
    }
}

impl fmt::Display for SourceView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.redacted {
            f.write_str("[redacted]")
        } else {
            fmt::Display::fmt(self.inner.as_ref(), f)
        }
    }
}

impl fmt::Debug for SourceView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SourceView([hidden])")
    }
}

impl Error for SourceView {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        if self.redacted {
            None
        } else {
            Some(self.inner.as_ref())
        }
    }
}

impl From<String> for ErrorValue {
    fn from(value: String) -> Self {
        Self::Display(value)
    }
}

impl From<&str> for ErrorValue {
    fn from(value: &str) -> Self {
        Self::Display(value.to_owned())
    }
}

/// One named evidence value attached to an error.
#[derive(Clone)]
pub struct ErrorContext {
    key: &'static str,
    value: ErrorValue,
    display: bool,
    sensitive: bool,
    ambient: bool,
}

impl ErrorContext {
    /// Creates visible, non-sensitive evidence.
    #[must_use]
    pub fn new(key: &'static str, value: impl Into<ErrorValue>) -> Self {
        Self::with_flags(key, value, true, false)
    }

    /// Creates evidence with registry rendering flags.
    #[must_use]
    pub fn with_flags(
        key: &'static str,
        value: impl Into<ErrorValue>,
        display: bool,
        sensitive: bool,
    ) -> Self {
        let mut value = value.into();
        if sensitive {
            value.redact_source_view();
        }
        Self {
            key,
            value,
            display,
            sensitive,
            ambient: false,
        }
    }

    /// Marks evidence as ambient execution context.
    #[must_use]
    pub fn as_ambient(mut self) -> Self {
        self.ambient = true;
        self
    }

    /// Returns evidence key.
    #[must_use]
    pub const fn key(&self) -> &'static str {
        self.key
    }

    /// Returns evidence value.
    #[must_use]
    pub fn value(&self) -> &ErrorValue {
        &self.value
    }

    /// Returns whether normal rendering includes this value.
    #[must_use]
    pub const fn is_displayed(&self) -> bool {
        self.display
    }

    /// Returns whether normal rendering redacts this value.
    #[must_use]
    pub const fn is_sensitive(&self) -> bool {
        self.sensitive
    }
}

impl fmt::Debug for ErrorContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ErrorContext")
            .field("key", &self.key)
            .field(
                "value",
                &if self.sensitive {
                    "[redacted]"
                } else {
                    "[value]"
                },
            )
            .field("display", &self.display)
            .field("sensitive", &self.sensitive)
            .field("ambient", &self.ambient)
            .finish()
    }
}

/// Complete error created by generated registry builders.
#[derive(Clone)]
pub struct RiftError {
    slug: ErrorSlug,
    message: String,
    action: String,
    fields: Vec<ErrorContext>,
    templated_fields: Vec<&'static str>,
}

/// Storage shared by generated error builders.
pub struct BuilderCore {
    slug: ErrorSlug,
    message: &'static str,
    action: &'static str,
    fields: Vec<Option<ErrorContext>>,
    ambient: Vec<ErrorContext>,
}

impl BuilderCore {
    /// Creates builder storage for one registered error.
    #[must_use]
    pub fn new(
        slug: ErrorSlug,
        message: &'static str,
        action: &'static str,
        fields: usize,
    ) -> Self {
        Self {
            slug,
            message,
            action,
            fields: std::iter::repeat_with(|| None).take(fields).collect(),
            ambient: Vec::new(),
        }
    }

    /// Sets one generated field.
    pub fn set(
        &mut self,
        index: usize,
        key: &'static str,
        value: ErrorValue,
        display: bool,
        sensitive: bool,
    ) {
        self.fields[index] = Some(ErrorContext::with_flags(key, value, display, sensitive));
    }

    /// Sets or clears one optional generated field.
    pub fn set_optional(
        &mut self,
        index: usize,
        key: &'static str,
        value: Option<ErrorValue>,
        display: bool,
        sensitive: bool,
    ) {
        self.fields[index] =
            value.map(|value| ErrorContext::with_flags(key, value, display, sensitive));
    }

    /// Adds ambient execution context.
    pub fn with(&mut self, context: ErrorContext) {
        self.ambient.push(context.as_ambient());
    }

    /// Finishes generated evidence and creates the registered error.
    #[must_use]
    pub fn finish(self) -> RiftError {
        let mut fields = self.fields.into_iter().flatten().collect::<Vec<_>>();
        fields.extend(self.ambient);
        RiftError::new(self.slug, self.message, self.action, fields)
    }
}

impl RiftError {
    /// Creates a registered error from message and action templates.
    #[must_use]
    pub fn new(
        slug: ErrorSlug,
        message: impl AsRef<str>,
        action: impl AsRef<str>,
        fields: Vec<ErrorContext>,
    ) -> Self {
        let message = message.as_ref();
        let action = action.as_ref();
        let templated_fields = fields
            .iter()
            .filter(|field| {
                template_uses_key(message, field.key) || template_uses_key(action, field.key)
            })
            .map(|field| field.key)
            .collect();
        let message = render_template(message, &fields);
        let action = render_template(action, &fields);
        Self {
            slug,
            message,
            action,
            fields,
            templated_fields,
        }
    }

    /// Returns this error through a function's result type.
    ///
    /// # Errors
    ///
    /// Returns this error.
    pub fn fail<T>(self) -> Result<T, Self> {
        Err(self)
    }

    /// Returns stable registry identity.
    #[must_use]
    pub const fn slug(&self) -> ErrorSlug {
        self.slug
    }

    /// Returns registered message with evidence substituted.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns registered action with evidence substituted.
    #[must_use]
    pub fn action(&self) -> &str {
        &self.action
    }

    /// Returns retained evidence.
    #[must_use]
    pub fn fields(&self) -> &[ErrorContext] {
        &self.fields
    }

    /// Attaches ambient execution context.
    #[must_use]
    pub fn with(mut self, mut context: ErrorContext) -> Self {
        context.ambient = true;
        self.fields.push(context);
        self
    }

    /// Returns visible evidence as context strings.
    pub fn context(&self) -> impl Iterator<Item = (&'static str, String)> + '_ {
        self.fields
            .iter()
            .filter(|field| field.display)
            .map(|field| {
                (
                    field.key,
                    if field.sensitive {
                        "[redacted]".to_owned()
                    } else {
                        field.value.rendered()
                    },
                )
            })
    }

    /// Returns redacted error detail without suggested action.
    #[must_use]
    pub fn detail(&self) -> String {
        let mut detail = render_visible(self.message.as_str(), &self.fields);
        let context = self
            .fields
            .iter()
            .filter(|field| {
                field.display && (field.ambient || !self.templated_fields.contains(&field.key))
            })
            .map(|field| {
                let value = if field.sensitive {
                    "[redacted]".to_owned()
                } else {
                    field.value.rendered()
                };
                format!("{} {}", field.key, value)
            })
            .collect::<Vec<_>>();
        if !context.is_empty() {
            detail.push_str(": ");
            detail.push_str(context.join(", ").as_str());
        }
        detail
    }

    fn source_value(&self) -> Option<&(dyn Error + 'static)> {
        self.fields.iter().find_map(|field| match &field.value {
            ErrorValue::Source(view) => {
                if view.redacted {
                    Some(view as &(dyn Error + 'static))
                } else {
                    Some(view.inner.as_ref())
                }
            }
            ErrorValue::Cause(cause) => Some(cause.as_ref() as &(dyn Error + 'static)),
            ErrorValue::Display(_) => None,
        })
    }
}

impl fmt::Debug for RiftError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RiftError")
            .field("slug", &self.slug)
            .field("message", &self.detail())
            .field("action", &render_visible(&self.action, &self.fields))
            .field("fields", &self.fields)
            .field("templated_fields", &self.templated_fields)
            .finish()
    }
}

impl fmt::Display for RiftError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}; {}",
            self.detail(),
            render_visible(&self.action, &self.fields)
        )
    }
}

impl Error for RiftError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source_value()
    }
}

fn render_template(template: &str, fields: &[ErrorContext]) -> String {
    let mut rendered = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        rendered.push_str(&rest[..open]);
        let after_open = &rest[open + 1..];
        let Some(close) = after_open.find('}') else {
            rendered.push_str(&rest[open..]);
            return rendered;
        };
        let key = &after_open[..close];
        if let Some(field) = fields.iter().find(|field| field.key == key) {
            if field.display {
                if field.sensitive {
                    rendered.push_str("[redacted]");
                } else {
                    rendered.push_str(field.value.rendered().as_str());
                }
            }
        } else {
            rendered.push('{');
            rendered.push_str(key);
            rendered.push('}');
        }
        rest = &after_open[close + 1..];
    }
    rendered.push_str(rest);
    rendered
}

fn template_uses_key(template: &str, key: &str) -> bool {
    let placeholder = format!("{{{key}}}");
    template.contains(placeholder.as_str())
}

fn render_visible(template: &str, fields: &[ErrorContext]) -> String {
    let mut rendered = template.to_owned();
    for field in fields {
        if field.sensitive {
            let value = field.value.rendered();
            if !value.is_empty() {
                rendered = rendered.replace(value.as_str(), "[redacted]");
            }
        }
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Forwarding {
        text: String,
        source: Box<dyn Error + Send + Sync>,
    }

    impl fmt::Display for Forwarding {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(&self.text)
        }
    }

    impl Error for Forwarding {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(self.source.as_ref())
        }
    }

    #[derive(Debug)]
    struct Cyclic;

    impl fmt::Display for Cyclic {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("cyclic")
        }
    }

    impl Error for Cyclic {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(self)
        }
    }

    #[test]
    fn causes_walk_the_source_chain_once_and_skip_duplicate_carrier_text() {
        let inner = Forwarding {
            text: "carrier".to_owned(),
            source: Box::new(std::io::Error::other("disk gone")),
        };
        let outer = Forwarding {
            text: "carrier".to_owned(),
            source: Box::new(inner),
        };

        assert_eq!(causes(&outer), ["disk gone"]);
    }

    #[test]
    fn causes_stop_on_cycles_and_at_the_depth_bound() {
        assert!(causes(&Cyclic).is_empty());

        let mut error: Box<dyn Error + Send + Sync> = Box::new(std::io::Error::other("bottom"));
        for depth in 0..(CAUSE_DEPTH_MAX + 4) {
            error = Box::new(Forwarding {
                text: format!("carrier {depth}"),
                source: error,
            });
        }
        let chain = causes(error.as_ref());
        assert_eq!(chain.len(), CAUSE_DEPTH_MAX);
        assert!(!chain.iter().any(|cause| cause == "bottom"));
    }

    #[test]
    fn rendering_omits_hidden_evidence_and_redacts_sensitive_values() {
        let error = RiftError::new(
            ErrorSlug::new("rift.test.failure"),
            "request {request} failed with {token}; hidden {internal}",
            "rotate {token}",
            vec![
                ErrorContext::new("request", "search"),
                ErrorContext::with_flags("token", "secret", true, true),
                ErrorContext::with_flags("internal", "trace", false, false),
            ],
        );

        assert_eq!(
            error.message(),
            "request search failed with [redacted]; hidden "
        );
        assert_eq!(error.action(), "rotate [redacted]");
        assert_eq!(
            error.to_string(),
            "request search failed with [redacted]; hidden ; rotate [redacted]"
        );
        assert!(!format!("{error:?}").contains("secret"));
    }

    #[test]
    fn debug_and_source_views_redact_sensitive_source_and_cause() {
        let source = RiftError::new(
            ErrorSlug::new("rift.test.inner"),
            "private source text",
            "retry",
            vec![],
        );
        let error = RiftError::new(
            ErrorSlug::new("rift.test.outer"),
            "failed with {source} and {cause}",
            "retry {source} {cause}",
            vec![
                ErrorContext::with_flags(
                    "source",
                    ErrorValue::source(std::io::Error::other("private source text")),
                    true,
                    true,
                ),
                ErrorContext::with_flags("cause", ErrorValue::cause(source), true, true),
            ],
        );
        for rendered in [
            error.to_string(),
            error.detail(),
            format!("{error:?}"),
            Error::source(&error)
                .expect("sensitive source view remains present")
                .to_string(),
        ] {
            assert!(!rendered.contains("private source text"), "{rendered}");
        }
    }

    #[test]
    fn hidden_evidence_is_omitted_even_when_message_names_key() {
        let error = RiftError::new(
            ErrorSlug::new("rift.test.hidden"),
            "request {token} failed",
            "retry",
            vec![ErrorContext::with_flags("token", "private", false, false)],
        );
        assert_eq!(error.detail(), "request  failed");
        assert!(!error.to_string().contains("private"));
        assert!(!format!("{error:?}").contains("private"));
    }

    #[test]
    fn source_and_cause_are_exposed_through_error_source() {
        let source = std::io::Error::other("disk failed");
        let error = RiftError::new(
            ErrorSlug::new("rift.test.source"),
            "write failed",
            "retry",
            vec![ErrorContext::new("source", ErrorValue::source(source))],
        );
        assert_eq!(
            Error::source(&error)
                .expect("stored error source remains available")
                .to_string(),
            "disk failed"
        );

        let cause = RiftError::new(ErrorSlug::new("rift.test.cause"), "inner", "retry", vec![]);
        let error = RiftError::new(
            ErrorSlug::new("rift.test.outer"),
            "outer",
            "retry",
            vec![ErrorContext::new("cause", ErrorValue::cause(cause))],
        );
        assert_eq!(
            Error::source(&error)
                .expect("registered cause remains available")
                .to_string(),
            "inner; retry"
        );
    }

    #[test]
    fn ambient_context_renders_after_message_and_redacts_sensitive_values() {
        let error = RiftError::new(
            ErrorSlug::new("rift.test.context"),
            "write failed",
            "retry",
            vec![],
        )
        .with(ErrorContext::new("workspace", "repo"))
        .with(ErrorContext::with_flags("token", "secret", true, true));

        assert_eq!(
            error.detail(),
            "write failed: workspace repo, token [redacted]"
        );
        assert!(!error.to_string().contains("secret"));
    }

    #[test]
    fn displayed_evidence_is_added_once_and_hidden_evidence_stays_hidden() {
        let error = RiftError::new(
            ErrorSlug::new("rift.test.displayed"),
            "request {request} failed",
            "retry",
            vec![
                ErrorContext::new("request", "seven"),
                ErrorContext::new("uri", "/repo/file"),
                ErrorContext::with_flags("internal", "trace", false, false),
            ],
        );

        assert_eq!(error.detail(), "request seven failed: uri /repo/file");
    }

    #[test]
    fn fail_returns_rift_error_for_any_result_type() {
        fn operation() -> Result<u32, RiftError> {
            RiftError::new(ErrorSlug::new("rift.test.fail"), "failed", "retry", vec![]).fail()
        }

        assert!(operation().is_err());
    }

    #[test]
    fn template_keeps_unknown_placeholder_visible_for_diagnostics() {
        let error = RiftError::new(
            ErrorSlug::new("rift.test"),
            "missing {field}",
            "retry",
            vec![],
        );
        assert_eq!(error.message(), "missing {field}");
    }

    #[test]
    fn erased_optional_sources_preserve_source_chain_and_none() {
        let source: Option<Box<dyn Error + Send + Sync>> =
            Some(Box::new(std::io::Error::other("disk failed")));
        let value = source.map(ErrorValue::source);
        let error = RiftError::new(
            ErrorSlug::new("rift.test.erased_source"),
            "write failed",
            "retry",
            vec![ErrorContext::new(
                "source",
                value.expect("source is present"),
            )],
        );
        assert_eq!(
            Error::source(&error)
                .expect("source remains in chain")
                .downcast_ref::<std::io::Error>()
                .expect("source type stays intact")
                .to_string(),
            "disk failed"
        );

        let absent: Option<Box<dyn Error + Send + Sync>> = None;
        assert!(absent.map(ErrorValue::source).is_none());
    }

    #[derive(Debug)]
    struct IdentitySource(Arc<()>);

    impl fmt::Display for IdentitySource {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("identity source")
        }
    }

    impl Error for IdentitySource {}

    #[test]
    fn borrowed_conversions_retain_source_identity_and_registered_data() {
        let identity = Arc::new(());
        let stored = Arc::new(RiftError::new(
            ErrorSlug::new("rift.test.stored_source"),
            "source failed",
            "retry",
            vec![
                ErrorContext::new("operation", "read"),
                ErrorContext::new(
                    "source",
                    ErrorValue::source(IdentitySource(Arc::clone(&identity))),
                ),
            ],
        ));

        let first = (&stored).into_rift_error();
        let second = (&stored).into_rift_error();
        assert_eq!(first.slug(), stored.slug());
        assert!(
            second
                .context()
                .any(|(key, value)| key == "operation" && value == "read")
        );
        let first_source = Error::source(&first)
            .and_then(|source| source.downcast_ref::<IdentitySource>())
            .expect("converted error retains concrete source");
        let second_source = Error::source(&second)
            .and_then(|source| source.downcast_ref::<IdentitySource>())
            .expect("second conversion retains concrete source");
        assert!(std::ptr::eq(first_source, second_source));
        assert!(Arc::ptr_eq(&first_source.0, &identity));

        let unique = Arc::new(RiftError::new(
            ErrorSlug::new("rift.test.unique"),
            "unique",
            "retry",
            vec![],
        ));
        let message_address = unique.message.as_ptr();
        let unwrapped = unique.into_rift_error();
        assert_eq!(message_address, unwrapped.message.as_ptr());
    }

    #[test]
    fn borrowed_conversions_preserve_hidden_sources_and_nested_causes() {
        let inner = Arc::new(RiftError::new(
            ErrorSlug::new("rift.test.inner"),
            "inner detail",
            "retry",
            vec![ErrorContext::new("context", "kept")],
        ));
        let outer = Arc::new(RiftError::new(
            ErrorSlug::new("rift.test.outer"),
            "failed with {cause} and {source}",
            "retry",
            vec![
                ErrorContext::new("cause", ErrorValue::cause(&inner)),
                ErrorContext::with_flags(
                    "source",
                    ErrorValue::source(std::io::Error::other("private source")),
                    true,
                    true,
                ),
            ],
        ));

        let first = (&outer).into_rift_error();
        let second = Arc::clone(&outer).into_rift_error();
        for converted in [&first, &second] {
            assert_eq!(converted.slug(), ErrorSlug::new("rift.test.outer"));
            assert!(!converted.to_string().contains("private"));
            assert!(!format!("{converted:?}").contains("private"));
            let cause = Error::source(converted)
                .and_then(|source| source.downcast_ref::<RiftError>())
                .expect("registered cause remains source");
            assert_eq!(cause.slug(), ErrorSlug::new("rift.test.inner"));
            assert_eq!(cause.context().next(), Some(("context", "kept".to_owned())));
        }
        let cause = Error::source(&first).expect("cause remains source");
        assert!(Error::source(cause).is_none());
    }

    #[test]
    fn typed_registry_values_accept_primitive_and_borrowed_inputs() {
        let count = 7_usize;
        let code = -3_i32;
        let enabled = true;
        let pid = 42_u32;
        let port = 8080_u16;

        assert_eq!(ErrorValue::unsigned(count).rendered(), "7");
        let count_ref = &count;
        assert_eq!(ErrorValue::unsigned(count_ref).rendered(), "7");
        assert_eq!(ErrorValue::integer(code).rendered(), "-3");
        let code_ref = &code;
        assert_eq!(ErrorValue::integer(code_ref).rendered(), "-3");
        let enabled_ref = &enabled;
        assert_eq!(ErrorValue::bool_value(enabled_ref).rendered(), "true");
        let pid_ref = &pid;
        assert_eq!(ErrorValue::pid(pid_ref).rendered(), "42");
        let port_ref = &port;
        assert_eq!(ErrorValue::port(port_ref).rendered(), "8080");
    }
}
