//! `Span`: the context an operation runs in, held by Rift code without the backend type.

use std::future::Future;

use tracing::instrument::Instrument as _;
use tracing::span::Id;

/// The context an operation runs in: a parent for later work, and fields set after opening.
///
/// `info_span!` and `debug_span!` open one where an operation spans several
/// stages or sets a field after it opens; [`traced!`](crate::traced!) opens one
/// for a block or a future. A clone refers to the same span, and the span closes
/// when its last clone drops.
#[derive(Clone, Debug)]
pub struct Span(tracing::Span);

impl Span {
    /// Returns the span the calling thread is in, or a disabled span outside any.
    #[must_use]
    pub fn current() -> Self {
        Self(tracing::Span::current())
    }

    /// Sets one field the span declared when it opened; an undeclared field is ignored.
    pub fn record<Value: tracing::field::Value>(&self, field: &str, value: Value) -> &Self {
        self.0.record(field, value);
        self
    }

    /// Runs `work` once inside this span and returns its value.
    ///
    /// Work on a thread with no ambient span, such as a `rayon` worker, runs
    /// under the span its caller held before handing the work off.
    pub fn in_scope<Value>(&self, work: impl FnOnce() -> Value) -> Value {
        self.0.in_scope(work)
    }

    /// Attaches this span to `work`, entering it for each poll and for the drop of `work`.
    ///
    /// The span is never held entered across a suspension, so a multi-threaded
    /// runtime can resume `work` on another thread.
    pub fn instrument<Work: Future>(self, work: Work) -> impl Future<Output = Work::Output> {
        work.instrument(self.0)
    }
}

impl From<&Span> for Option<Id> {
    fn from(span: &Span) -> Self {
        span.0.id()
    }
}

/// Wraps a span the backend macros opened.
#[doc(hidden)]
#[must_use]
pub fn span_from(span: tracing::Span) -> Span {
    Span(span)
}

/// Opens a [`Span`] at the info level, with the caller's module path as its target.
///
/// The arguments are those of a `traced!` span: a name literal, then fields;
/// `parent: &span` names an explicit parent. The span also records the field
/// `code.function.name`: the function that opened it, the column a line prints for every
/// record inside it.
///
/// ```
/// let span = rift_tracing::info_span!("cloud.request", status = 0_u16);
/// span.record("status", 200_u16);
/// ```
#[macro_export]
macro_rules! info_span {
    ($($arguments:tt)+) => {
        $crate::__private::span_from($crate::__rift_named_span!(info_span [] $($arguments)+))
    };
}

/// Opens a [`Span`] at the debug level, with the caller's module path as its target, and
/// the field `code.function.name` as [`info_span!`] records it.
///
/// ```
/// # async fn read() -> u8 { 1 }
/// # async fn run() -> u8 {
/// rift_tracing::debug_span!("database.read").instrument(read()).await
/// # }
/// # let _ = run();
/// ```
#[macro_export]
macro_rules! debug_span {
    ($($arguments:tt)+) => {
        $crate::__private::span_from($crate::__rift_named_span!(debug_span [] $($arguments)+))
    };
}

/// Opens a `tracing` span through `$macro` with `code.function.name` right after its name
/// literal, ahead of the caller's fields and any trailing comma they end with.
#[doc(hidden)]
#[macro_export]
macro_rules! __rift_named_span {
    ($macro:ident [$($parent:tt)*] parent: $value:expr, $($rest:tt)+) => {
        $crate::__rift_named_span!($macro [parent: $value,] $($rest)+)
    };
    ($macro:ident [$($parent:tt)*] $name:literal $(,)?) => {{
        $crate::__private::stream_unscoped();
        $crate::__private::tracing::$macro!(
            $($parent)* $name,
            code.function.name = $crate::__rift_function_name!()
        )
    }};
    ($macro:ident [$($parent:tt)*] $name:literal, $($fields:tt)+) => {{
        $crate::__private::stream_unscoped();
        $crate::__private::tracing::$macro!(
            $($parent)* $name,
            code.function.name = $crate::__rift_function_name!(),
            $($fields)+
        )
    }};
    ($macro:ident [$($parent:tt)*] $($arguments:tt)+) => {{
        $crate::__private::stream_unscoped();
        $crate::__private::tracing::$macro!($($parent)* $($arguments)+)
    }};
}

/// The fully-qualified name of the function the macro expands in, as the field
/// `code.function.name` records it: `rift_mcp::server::RiftMcp::search`.
///
/// `tracing`'s metadata holds a module path and no function, and `type_name` is not a
/// `const fn` on a stable compiler, so the name is read when a record is made: the
/// `type_name` of an item `f` declared here, trimmed by
/// [`function_name`](crate::__private::function_name). The standard library leaves the
/// format of that text unspecified, so the name is display text alone.
#[doc(hidden)]
#[macro_export]
macro_rules! __rift_function_name {
    () => {
        $crate::__private::function_name({
            fn f() {}
            ::core::any::type_name_of_val(&f)
        })
    };
}

/// `raw`, the `type_name` of an item `f` declared inside a function body, without its
/// trailing `::f` and `::{{closure}}` segments: the function that declares the body.
///
/// `rift_mcp::server::Server::nodes::{{closure}}::f`, the item inside an async method,
/// names `rift_mcp::server::Server::nodes`.
#[doc(hidden)]
#[must_use]
pub fn function_name(raw: &'static str) -> &'static str {
    let mut name = raw.strip_suffix("::f").unwrap_or(raw);
    while let Some(outer) = name.strip_suffix("::{{closure}}") {
        name = outer;
    }
    name
}

/// Emits an event at the `trace` level, with the caller's module path as its target.
///
/// The arguments are those of `tracing::trace!`. The event also records the field
/// `code.function.name`: the function that emitted it, the column a line prints for a
/// record outside every span.
#[macro_export]
macro_rules! trace {
    (target: $target:expr, $($rest:tt)+) => {{
        $crate::__private::stream_unscoped();
        $crate::__private::tracing::event!(
            target: $target,
            $crate::__private::tracing::Level::TRACE,
            code.function.name = $crate::__rift_function_name!(),
            $($rest)+
        )
    }};
    (parent: $parent:expr, $($rest:tt)+) => {{
        $crate::__private::stream_unscoped();
        $crate::__private::tracing::event!(
            parent: $parent,
            $crate::__private::tracing::Level::TRACE,
            code.function.name = $crate::__rift_function_name!(),
            $($rest)+
        )
    }};
    ($($rest:tt)+) => {{
        $crate::__private::stream_unscoped();
        $crate::__private::tracing::trace!(
            code.function.name = $crate::__rift_function_name!(),
            $($rest)+
        )
    }};
}

/// Emits an event at the `debug` level, with the caller's module path as its target.
///
/// The arguments are those of `tracing::debug!`. The event also records the field
/// `code.function.name`: the function that emitted it, the column a line prints for a
/// record outside every span.
#[macro_export]
macro_rules! debug {
    (target: $target:expr, $($rest:tt)+) => {{
        $crate::__private::stream_unscoped();
        $crate::__private::tracing::event!(
            target: $target,
            $crate::__private::tracing::Level::DEBUG,
            code.function.name = $crate::__rift_function_name!(),
            $($rest)+
        )
    }};
    (parent: $parent:expr, $($rest:tt)+) => {{
        $crate::__private::stream_unscoped();
        $crate::__private::tracing::event!(
            parent: $parent,
            $crate::__private::tracing::Level::DEBUG,
            code.function.name = $crate::__rift_function_name!(),
            $($rest)+
        )
    }};
    ($($rest:tt)+) => {{
        $crate::__private::stream_unscoped();
        $crate::__private::tracing::debug!(
            code.function.name = $crate::__rift_function_name!(),
            $($rest)+
        )
    }};
}

/// Emits an event at the `info` level, with the caller's module path as its target.
///
/// The arguments are those of `tracing::info!`. The event also records the field
/// `code.function.name`: the function that emitted it, the column a line prints for a
/// record outside every span.
#[macro_export]
macro_rules! info {
    (target: $target:expr, $($rest:tt)+) => {{
        $crate::__private::stream_unscoped();
        $crate::__private::tracing::event!(
            target: $target,
            $crate::__private::tracing::Level::INFO,
            code.function.name = $crate::__rift_function_name!(),
            $($rest)+
        )
    }};
    (parent: $parent:expr, $($rest:tt)+) => {{
        $crate::__private::stream_unscoped();
        $crate::__private::tracing::event!(
            parent: $parent,
            $crate::__private::tracing::Level::INFO,
            code.function.name = $crate::__rift_function_name!(),
            $($rest)+
        )
    }};
    ($($rest:tt)+) => {{
        $crate::__private::stream_unscoped();
        $crate::__private::tracing::info!(
            code.function.name = $crate::__rift_function_name!(),
            $($rest)+
        )
    }};
}

/// Emits an event at the `warn` level, with the caller's module path as its target.
///
/// The arguments are those of `tracing::warn!`. The event also records the field
/// `code.function.name`: the function that emitted it, the column a line prints for a
/// record outside every span.
#[macro_export]
macro_rules! warn {
    (target: $target:expr, $($rest:tt)+) => {{
        $crate::__private::stream_unscoped();
        $crate::__private::tracing::event!(
            target: $target,
            $crate::__private::tracing::Level::WARN,
            code.function.name = $crate::__rift_function_name!(),
            $($rest)+
        )
    }};
    (parent: $parent:expr, $($rest:tt)+) => {{
        $crate::__private::stream_unscoped();
        $crate::__private::tracing::event!(
            parent: $parent,
            $crate::__private::tracing::Level::WARN,
            code.function.name = $crate::__rift_function_name!(),
            $($rest)+
        )
    }};
    ($($rest:tt)+) => {{
        $crate::__private::stream_unscoped();
        $crate::__private::tracing::warn!(
            code.function.name = $crate::__rift_function_name!(),
            $($rest)+
        )
    }};
}

/// Emits an event at the `error` level, with the caller's module path as its target.
///
/// The arguments are those of `tracing::error!`. The event also records the field
/// `code.function.name`: the function that emitted it, the column a line prints for a
/// record outside every span.
#[macro_export]
macro_rules! error {
    (target: $target:expr, $($rest:tt)+) => {{
        $crate::__private::stream_unscoped();
        $crate::__private::tracing::event!(
            target: $target,
            $crate::__private::tracing::Level::ERROR,
            code.function.name = $crate::__rift_function_name!(),
            $($rest)+
        )
    }};
    (parent: $parent:expr, $($rest:tt)+) => {{
        $crate::__private::stream_unscoped();
        $crate::__private::tracing::event!(
            parent: $parent,
            $crate::__private::tracing::Level::ERROR,
            code.function.name = $crate::__rift_function_name!(),
            $($rest)+
        )
    }};
    ($($rest:tt)+) => {{
        $crate::__private::stream_unscoped();
        $crate::__private::tracing::error!(
            code.function.name = $crate::__rift_function_name!(),
            $($rest)+
        )
    }};
}

/// A span field declared at opening and given its value later with [`Span::record`].
///
/// A field a span did not declare when it opened is ignored by `record`, so a value
/// known only when the operation ends is declared with this placeholder, which records
/// nothing until then.
///
/// ```
/// let span = rift_tracing::info_span!(
///     "global.request",
///     component = "global",
///     status = rift_tracing::empty!(),
/// );
/// span.record("status", 200_u16);
/// ```
#[macro_export]
macro_rules! empty {
    () => {
        $crate::__private::tracing::field::Empty
    };
}
