//! `traced!`: one operation timed through one `tracing` span, for a block or a future.
//!
//! A collector (OTLP, `rift://logs`) reads elapsed time from a span's own open
//! and close; a hand-written `Instant::now()` pair beside it is a second source
//! of the same fact that can drift from the first. The span is named by the
//! `operation` literal and carries `operation`, `component` when given, and any
//! extra fields the caller lists.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tracing::instrument::{Instrument as _, Instrumented};

use crate::Span;
use crate::metrics::{Completion, future_completion};

pin_project_lite::pin_project! {
    /// Where an awaited operation is: not yet polled, running under its span, or done.
    ///
    /// `Running` holds the operation's completion guard beside its work, so dropping
    /// the future before the work returns records a cancelled operation. `Spent` holds
    /// nothing: the operation completed, or its span constructor panicked while the work
    /// moved into its span.
    #[project = StageProjection]
    #[project_replace = StageReplacement]
    enum Stage<Work, Open> {
        Waiting { operation: &'static str, work: Work, open: Open },
        Running { #[pin] work: Instrumented<Work>, completion: Completion },
        Spent,
    }
}

pin_project_lite::pin_project! {
    /// A future whose span opens on its first poll.
    ///
    /// Before that poll the future holds the work and the span constructor;
    /// dropping it then drops both and opens no span. From the first poll on,
    /// `tracing`'s `Instrumented` enters the span for each poll and for the
    /// drop of the work, and exits it in between. The poll that completes the
    /// work drops it with its span, so the span closes when the work completes.
    struct TracedFuture<Work, Open> {
        #[pin]
        stage: Stage<Work, Open>,
    }

    impl<Work, Open> PinnedDrop for TracedFuture<Work, Open> {
        /// Records `error.type = "cancelled"` on the span of work dropped after its first
        /// poll and before it returned, ahead of the span's close, so the close record
        /// states the operation did not complete. A drop while the thread unwinds a panic
        /// records nothing here: the close reads the panic itself.
        fn drop(this: Pin<&mut Self>) {
            if let StageProjection::Running { work, .. } = this.project().stage.project()
                && !std::thread::panicking()
            {
                work.span().record("error.type", "cancelled");
            }
        }
    }
}

impl<Work, Open> Future for TracedFuture<Work, Open>
where
    Work: Future,
    Open: FnOnce() -> tracing::Span,
{
    type Output = Work::Output;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let mut stage = self.project().stage;
        if let StageProjection::Waiting { .. } = stage.as_mut().project()
            && let StageReplacement::Waiting {
                operation,
                work,
                open,
            } = stage.as_mut().project_replace(Stage::Spent)
        {
            let work = work.instrument(open());
            let completion = future_completion(operation);
            stage.set(Stage::Running { work, completion });
        }
        let StageProjection::Running {
            mut work,
            completion,
        } = stage.as_mut().project()
        else {
            panic!("a traced future was polled after it completed or its span constructor panicked")
        };
        let output = std::task::ready!(work.as_mut().poll(context));
        completion.finished(work.span());
        stage.set(Stage::Spent);
        Poll::Ready(output)
    }
}

/// Wraps `work` so the span `open` builds, and the completion of `operation`, start on
/// the first poll.
#[doc(hidden)]
pub fn traced_future<Work, Open>(
    operation: &'static str,
    work: Work,
    open: Open,
) -> impl Future<Output = Work::Output>
where
    Work: Future,
    Open: FnOnce() -> tracing::Span,
{
    TracedFuture {
        stage: Stage::Waiting {
            operation,
            work,
            open,
        },
    }
}

/// Holds the explicit parent of an awaited operation until its first poll.
///
/// The clone keeps the parent span open while the future waits, so the span
/// opened at the first poll still finds its parent.
#[doc(hidden)]
#[must_use]
pub fn parent_span(parent: &Span) -> Span {
    parent.clone()
}

/// Times one operation through a `tracing` span named by its `operation` literal.
///
/// The short form names the operation and the work:
///
/// ```
/// let sum = rift_tracing::traced!("index.parse", 1 + 1);
/// assert_eq!(sum, 2);
///
/// # async fn commit() -> u8 {
/// rift_tracing::traced!("lexical.commit", async { 1 + 1 }).await
/// # }
/// # let _ = commit();
/// ```
///
/// The detailed form adds `component` and extra `name = value` fields before a
/// block or an async block:
///
/// ```
/// let sources = 3;
/// let scanned = rift_tracing::traced!(
///     component = "documentation",
///     operation = "documentation.collect",
///     sources = sources,
///     { "scanned" }
/// );
/// assert_eq!(scanned, "scanned");
///
/// # async fn commit(documents: usize) -> usize {
/// rift_tracing::traced!(
///     component = "lexical",
///     operation = "lexical.documents",
///     documents = documents,
///     async move { documents }
/// )
/// .await
/// # }
/// # let _ = commit(2);
/// ```
///
/// Both forms take a leading `parent: <expr>`. Work that runs on a thread with
/// no ambient span, such as a `rayon` worker or a detached `std::thread`, has
/// no contextual parent, and a span opened there starts a new trace. Pass the
/// span the calling thread held before it handed off the work:
///
/// ```
/// let parent = rift_tracing::Span::current();
/// std::thread::spawn(move || {
///     rift_tracing::traced!(
///         parent: &parent,
///         component = "dependency",
///         operation = "package.analyze",
///         {}
///     );
/// })
/// .join()
/// .expect("the worker thread does not panic");
/// ```
///
/// # Evaluation and control flow
///
/// - `operation` is a string literal: a span's name is part of its static
///   metadata, so input cannot create unbounded span names.
/// - `component`, the extra fields, and `parent` evaluate once, at operation
///   entry: before a block, and at the first poll of a future.
/// - A block or expression evaluates once, inside the entered span, inlined in
///   the caller. `?`, `return`, `break`, and `continue` act on the enclosing
///   function or loop, and the span exits on every path out.
/// - An async block evaluates to a future and runs when polled; `.await`
///   returns its value unchanged. Its `return` and `?` apply to the async
///   block, as in any async block.
/// - The span of an awaited operation opens on its first poll, so time between
///   creating the future and polling it is excluded. A future dropped before
///   its first poll opens no span.
/// - The span is entered only while the future is polled or dropped and never
///   held across a suspension, because a multi-threaded runtime can resume the
///   future on another thread. The span closes when the work completes, or
///   when the future is dropped after its first poll.
/// - In the async form, `parent` is a `&Span` evaluated when the future is
///   created; the future keeps a clone of it until the first poll.
/// - The async form's field expressions run inside a `move` closure the future
///   holds. A variable they name moves into the future, or is copied when it is
///   `Copy`, and the value recorded is the one it has at the first poll. A
///   variable that is not `Copy` cannot appear both in a field and in an
///   `async move` block; bind the field value to a local first.
///
/// # Operations in flight
///
/// Every operation sits in the table of operations in flight from its span's opening to
/// its close, where [`publish_in_flight`](crate::publish_in_flight) and the stall report
/// read it. The detailed form takes `open = true` right after `operation` to also record
/// the opening: one `INFO` event with the target `rift_tracing::flight` and the message
/// `operation opened`, inside the operation's span, so a test or a reader of the store sees
/// the operation before it closes:
///
/// ```
/// # async fn commit() -> u8 {
/// rift_tracing::traced!(
///     component = "lexical",
///     operation = "lexical.commit",
///     open = true,
///     async { 1 }
/// )
/// .await
/// # }
/// # let _ = commit();
/// ```
///
/// # Completion metrics
///
/// Every operation records `traces.span.metrics.calls` and
/// `traces.span.metrics.duration`, in seconds, labeled with the operation literal as
/// `span.name` and its outcome as `status.code`: `Ok` when the work finished, by any path
/// out of a block or by returning from a future, and `Error` with `error.type` `panic` or
/// `cancelled` when it panicked or an awaited future was dropped before it returned. Work
/// that records `error.type`, or an `outcome` other than `ok` or `acquired`, on the
/// operation's span ends with `Error` too: `error.type` holds the recorded value when it is
/// `panic`, `cancelled`, `timeout`, or `refused`, and `_OTHER` otherwise. The macro never
/// reads the work's value. The span's close record states the same outcome as
/// `status.code` and `error.type`, and the span records `code.function.name`, the
/// function the macro expands in. The metric
/// recording follows the operation, not its span: a clone of the span held elsewhere
/// does not lengthen the duration, and the span's filters do not select it. A process
/// that installed no meter records nothing and reads no clock for it.
///
/// A computed operation name is refused at compile time:
///
/// ```compile_fail
/// let operation = "index.parse";
/// let _ = rift_tracing::traced!(operation, 1 + 1);
/// ```
///
/// A field naming a variable that is not `Copy`, which the `async move` block
/// also moves, is refused at compile time:
///
/// ```compile_fail
/// # async fn store(_: Vec<u8>) {}
/// # async fn run() {
/// let documents = vec![1_u8, 2];
/// rift_tracing::traced!(
///     component = "lexical",
///     operation = "lexical.documents",
///     documents = documents.len(),
///     async move { store(documents).await }
/// )
/// .await;
/// # }
/// ```
///
/// Extra fields need the detailed form, and its work is a block:
///
/// ```compile_fail
/// let _ = rift_tracing::traced!(component = "index", operation = "index.parse", 1 + 1);
/// ```
#[macro_export]
macro_rules! traced {
    ($(parent: $parent:expr,)? $operation:literal, async move $work:block $(,)?) => {
        $crate::__rift_traced_future!(
            [$($parent)?] [] $operation [] [] async move $work
        )
    };
    ($(parent: $parent:expr,)? $operation:literal, async $work:block $(,)?) => {
        $crate::__rift_traced_future!([$($parent)?] [] $operation [] [] async $work)
    };
    ($(parent: $parent:expr,)? $operation:literal, $work:expr $(,)?) => {
        $crate::__rift_traced_block!([$($parent)?] [] $operation [] [] $work)
    };
    (
        $(parent: $parent:expr,)?
        component = $component:expr,
        operation = $operation:literal,
        open = true,
        $($rest:tt)+
    ) => {
        $crate::__rift_traced_fields!([$($parent)?] [$component] $operation [open] [] $($rest)+)
    };
    (
        $(parent: $parent:expr,)?
        component = $component:expr,
        operation = $operation:literal,
        $($rest:tt)+
    ) => {
        $crate::__rift_traced_fields!([$($parent)?] [$component] $operation [] [] $($rest)+)
    };
}

/// Reads the detailed form's extra fields one at a time, then selects the block or future form.
///
/// A field and the work cannot be told apart by one repetition, because an
/// identifier can start either; each arm here checks one shape in turn.
#[doc(hidden)]
#[macro_export]
macro_rules! __rift_traced_fields {
    (
        [$($parent:expr)?] [$component:expr] $operation:literal [$($open:ident)?]
        [$($field:ident = $value:expr,)*] async move $work:block $(,)?
    ) => {
        $crate::__rift_traced_future!(
            [$($parent)?] [$component] $operation [$($open)?] [$($field = $value),*]
            async move $work
        )
    };
    (
        [$($parent:expr)?] [$component:expr] $operation:literal [$($open:ident)?]
        [$($field:ident = $value:expr,)*] async $work:block $(,)?
    ) => {
        $crate::__rift_traced_future!(
            [$($parent)?] [$component] $operation [$($open)?] [$($field = $value),*] async $work
        )
    };
    (
        [$($parent:expr)?] [$component:expr] $operation:literal [$($open:ident)?]
        [$($field:ident = $value:expr,)*] $work:block $(,)?
    ) => {
        $crate::__rift_traced_block!(
            [$($parent)?] [$component] $operation [$($open)?] [$($field = $value),*] $work
        )
    };
    (
        [$($parent:expr)?] [$component:expr] $operation:literal [$($open:ident)?]
        [$($field:ident = $value:expr,)*] $next:ident = $next_value:expr, $($rest:tt)+
    ) => {
        $crate::__rift_traced_fields!(
            [$($parent)?] [$component] $operation [$($open)?]
            [$($field = $value,)* $next = $next_value,]
            $($rest)+
        )
    };
}

/// The block form of [`traced!`]: enter the span, evaluate the work inline.
#[doc(hidden)]
#[macro_export]
macro_rules! __rift_traced_block {
    (
        [$($parent:expr)?] [$($component:expr)?] $operation:literal [$($open:ident)?]
        [$($field:ident = $value:expr),*] $work:expr
    ) => {{
        let __rift_entered = $crate::__private::tracing::span!(
            $(parent: $parent,)?
            $crate::__private::tracing::Level::INFO,
            $operation,
            $(component = $component,)?
            operation = $operation,
            $($field = $value,)*
            code.function.name = $crate::__rift_function_name!(),
            error.type = $crate::__private::tracing::field::Empty
        )
        .entered();
        // Declared after the entered span, the completion drops first, while the span is
        // still open and holds what it recorded.
        let __rift_completion =
            $crate::__private::completion($operation).of_span(__rift_entered.id());
        $($crate::__rift_traced_opened!($open);)?
        $work
    }};
}

/// The record an operation declared with `open = true` emits inside its span as it opens.
#[doc(hidden)]
#[macro_export]
macro_rules! __rift_traced_opened {
    (open) => {
        $crate::__private::tracing::info!(target: "rift_tracing::flight", "operation opened")
    };
}

/// The future form of [`traced!`]: keep the parent now, open the span on first poll.
#[doc(hidden)]
#[macro_export]
macro_rules! __rift_traced_future {
    (
        [$($parent:expr)?] [$($component:expr)?] $operation:literal [$($open:ident)?]
        [$($field:ident = $value:expr),*] $work:expr
    ) => {{
        $(let __rift_parent = $crate::__private::parent_span($parent);)?
        $crate::__private::traced_future($operation, $work, move || {
            let __rift_span = $crate::__rift_traced_span!(
                [$(__rift_parent $parent)?] [$($component)?] $operation [$($field = $value),*]
            );
            $(__rift_span.in_scope(|| $crate::__rift_traced_opened!($open));)?
            __rift_span
        })
    }};
}

/// Opens the span of an awaited operation, evaluating its fields at the first poll.
#[doc(hidden)]
#[macro_export]
macro_rules! __rift_traced_span {
    (
        [$($held:ident $parent:expr)?] [$($component:expr)?] $operation:literal
        [$($field:ident = $value:expr),*]
    ) => {
        $crate::__private::tracing::span!(
            $(parent: &$held,)?
            $crate::__private::tracing::Level::INFO,
            $operation,
            $(component = $component,)?
            operation = $operation,
            $($field = $value,)*
            code.function.name = $crate::__rift_function_name!(),
            error.type = $crate::__private::tracing::field::Empty
        )
    };
}

#[cfg(test)]
mod tests;
