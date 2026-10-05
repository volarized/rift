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
/// `parent: &span` names an explicit parent.
///
/// ```
/// let span = rift_tracing::info_span!("cloud.request", status = 0_u16);
/// span.record("status", 200_u16);
/// ```
#[macro_export]
macro_rules! info_span {
    ($($arguments:tt)+) => {
        $crate::__private::span_from($crate::__private::tracing::info_span!($($arguments)+))
    };
}

/// Opens a [`Span`] at the debug level, with the caller's module path as its target.
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
        $crate::__private::span_from($crate::__private::tracing::debug_span!($($arguments)+))
    };
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
