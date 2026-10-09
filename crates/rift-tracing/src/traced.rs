//! `traced!`: one operation timed through one `tracing` span, for a block or a future.
//!
//! A collector (OTLP, `rift server logs`) reads elapsed time from a span's own open
//! and close; a hand-written `Instant::now()` pair beside it is a second source
//! of the same fact that can drift from the first. The span is named by the
//! `operation` literal and carries `operation`, `component` when given, and any
//! extra fields the caller lists.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use rift_error::RiftError;
use tracing::instrument::{Instrument as _, Instrumented};

use crate::Span;
use crate::metrics::{Completion, InstrumentScope, future_completion};

mod residual;
pub use residual::{InlineResidual, InlineTry};

/// The work's value, borrowed for [`RegisteredError`] and [`OtherValue`] to read.
///
/// The macro calls `(&&WorkValue(&value)).registered_identity()`. Method lookup tries the
/// receiver `&&WorkValue<T>` before `&WorkValue<T>`, so [`RegisteredError`], implemented on
/// `&WorkValue<T>`, answers for the types it holds an impl for, and [`OtherValue`],
/// implemented on every `WorkValue<T>`, answers for the rest.
#[doc(hidden)]
pub struct WorkValue<'value, T>(pub &'value T);

/// Reads the registered identity of `Err(RiftError)`, including ready `Poll` values.
///
/// Two impls keep a work value of a type not yet inferred at the call an open obligation,
/// not a choice: a block that diverges has such a type, which falls back to `!`, and the
/// impl for `!` takes it.
#[doc(hidden)]
pub trait RegisteredError {
    /// The registered identity of the error the work returned, as `RiftError::slug` spells it.
    fn registered_identity(&self) -> Option<&'static str>;
}

impl<T> RegisteredError for &WorkValue<'_, Result<T, RiftError>> {
    fn registered_identity(&self) -> Option<&'static str> {
        self.0.as_ref().err().map(|error| error.slug().as_str())
    }
}

impl<T> RegisteredError for &WorkValue<'_, Poll<Result<T, RiftError>>> {
    fn registered_identity(&self) -> Option<&'static str> {
        match self.0 {
            Poll::Ready(Err(error)) => Some(error.slug().as_str()),
            _ => None,
        }
    }
}

impl<T> RegisteredError for &WorkValue<'_, Poll<Option<Result<T, RiftError>>>> {
    fn registered_identity(&self) -> Option<&'static str> {
        match self.0 {
            Poll::Ready(Some(Err(error))) => Some(error.slug().as_str()),
            _ => None,
        }
    }
}

impl RegisteredError for &WorkValue<'_, Never> {
    fn registered_identity(&self) -> Option<&'static str> {
        match *self.0 {}
    }
}

/// Reads no identity from work values outside the registered result types.
#[doc(hidden)]
pub trait OtherValue {
    /// No identity: the value is not a returned `RiftError`.
    fn registered_identity(&self) -> Option<&'static str> {
        None
    }
}

impl<T> OtherValue for WorkValue<'_, T> {}

/// The return type of a function pointer type: stable Rust spells `!` only as a return
/// type, and `<fn() -> ! as FnOutput>::Output` names it for the impl of `RegisteredError`.
#[doc(hidden)]
pub trait FnOutput {
    /// The function pointer's return type.
    type Output;
}

impl<T> FnOutput for fn() -> T {
    type Output = T;
}

/// The type `!`: the type a diverging block's value falls back to.
type Never = <fn() -> ! as FnOutput>::Output;

/// Records on `span` the registered identity `read` finds in the value of a `traced!`
/// block, as `error.type`, and returns the value.
///
/// The macro passes `read` as a closure argument: the compiler types a closure argument
/// after the other arguments, so the closure reads `value` with its type inferred.
#[doc(hidden)]
pub fn returned<T>(
    span: &tracing::Span,
    read: impl FnOnce(&T) -> Option<&'static str>,
    value: T,
) -> T {
    if let Some(identity) = read(&value) {
        span.record("error.type", identity);
    }
    value
}

/// Records an inline exit's registered identity on its operation's owned span.
#[doc(hidden)]
pub fn exiting<T>(
    span: &tracing::Span,
    read: impl FnOnce(&T) -> Option<&'static str>,
    value: T,
) -> T {
    returned(span, read, value)
}

pin_project_lite::pin_project! {
    /// Where an awaited operation is: not yet polled, running under its span, or done.
    ///
    /// `Running` holds the operation's completion guard beside its work, so dropping
    /// the future before the work returns records a cancelled operation. The guard is
    /// declared first so it drops before the work, while the work's span is still open.
    /// `Spent` holds nothing: the operation completed, or its span constructor panicked
    /// while the work moved into its span.
    #[project = StageProjection]
    #[project_replace = StageReplacement]
    enum Stage<Work, Open> {
        Waiting { scope: InstrumentScope, operation: &'static str, work: Work, open: Open },
        Running { completion: Completion, #[pin] work: Instrumented<Work> },
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
    struct TracedFuture<Work, Open, Read> {
        #[pin]
        stage: Stage<Work, Open>,
        read: Read,
    }

    impl<Work, Open, Read> PinnedDrop for TracedFuture<Work, Open, Read> {
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

impl<Work, Open, Read> Future for TracedFuture<Work, Open, Read>
where
    Work: Future,
    Open: FnOnce() -> tracing::Span,
    Read: Fn(&Work::Output) -> Option<&'static str>,
{
    type Output = Work::Output;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let mut stage = this.stage;
        if let StageProjection::Waiting { .. } = stage.as_mut().project()
            && let StageReplacement::Waiting {
                scope,
                operation,
                work,
                open,
            } = stage.as_mut().project_replace(Stage::Spent)
        {
            let work = work.instrument(open());
            let completion = future_completion(scope, operation).of_span(work.span().id());
            stage.set(Stage::Running { completion, work });
        }
        let StageProjection::Running {
            completion,
            mut work,
        } = stage.as_mut().project()
        else {
            panic!("a traced future was polled after it completed or its span constructor panicked")
        };
        let output = std::task::ready!(work.as_mut().poll(context));
        if let Some(identity) = (this.read)(&output) {
            work.span().record("error.type", identity);
        }
        completion.finished(work.span());
        stage.set(Stage::Spent);
        Poll::Ready(output)
    }
}

/// Wraps `work` so the span `open` builds, and the completion of `operation` under the
/// instrumentation scope `scope`, start on the first poll; the registered identity `read`
/// finds in the work's output is recorded on the span as `error.type` before it completes.
#[doc(hidden)]
pub fn traced_future<Work, Open, Read>(
    scope: InstrumentScope,
    operation: &'static str,
    work: Work,
    open: Open,
    read: Read,
) -> impl Future<Output = Work::Output>
where
    Work: Future,
    Open: FnOnce() -> tracing::Span,
    Read: Fn(&Work::Output) -> Option<&'static str>,
{
    TracedFuture {
        stage: Stage::Waiting {
            scope,
            operation,
            work,
            open,
        },
        read,
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
/// `traces.span.metrics.duration`, in seconds, under the instrumentation scope of the crate
/// the macro expands in, its `CARGO_PKG_NAME` and `CARGO_PKG_VERSION`, labeled with the
/// operation literal as
/// `span.name`, `span.kind` `Internal`, the kind its exported span carries, and its outcome
/// as `status.code`: `Ok` when the work finished, by any path
/// out of a block or by returning from a future, and `Error` with `error.type` `panic` or
/// `cancelled` when it panicked or an awaited future was dropped before it returned. Work
/// that records `error.type`, or an `outcome` other than `ok` or `acquired`, on the
/// operation's span ends with `Error` too: `error.type` holds the recorded value when it is
/// `panic`, `cancelled`, `timeout`, `refused`, or a registered error identity, as
/// `RiftError::slug` spells it, and `_OTHER` otherwise. Work whose value is
/// `Result<_, RiftError>` holding `Err`, the value of a block or the output of a future,
/// records that error's registered identity as `error.type` itself, after it returns; a
/// value of any other type records nothing. Inline `?` and `return` exits record a
/// registered error before it leaves the enclosing function. Nested closures, async
/// blocks own their exits and are read only when their output leaves the operation.
/// Only `?` and `return` written directly in the inline work are inspected; exits
/// produced by another macro expansion require explicit `error.type` recording.
/// A try block owns `?`; its `return` still leaves the enclosing function.
/// The span's close record states the same outcome as `status.code` and
/// `error.type`, and the span records `code.function.name`, the function the macro
/// expands in. The duration is the time from
/// the span's opening to the end of the work, read once: the histogram records it and the
/// close record states it as `elapsed_ms`, so a clone of the span held elsewhere lengthens
/// neither, and the span's filters do not select the metric recording. A process that
/// installed no meter records no metrics.
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
        $crate::__private::stream_unscoped();
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
        let __rift_completion = $crate::__private::completion(
            $crate::__rift_instrument_scope!(),
            $operation,
        )
        .of_span(__rift_entered.id());
        $($crate::__rift_traced_opened!($open);)?
        $crate::__private::returned(
            &__rift_entered,
            $crate::__rift_registered_error!(),
            // The arm passes the caller's expected type on to the work, and keeps a block's
            // braces out of the argument position.
            match () {
                () => $crate::__private::__rift_traced_work!([$crate] (__rift_entered, $work)),
            },
        )
    }};
}

/// The closure reading the registered identity of a `traced!` operation's value.
///
/// After a diverging block the closure is unreachable code, and the lint is allowed on
/// the closure alone, so the work keeps its own reports.
#[doc(hidden)]
#[macro_export]
macro_rules! __rift_registered_error {
    () => {
        #[allow(unreachable_code)]
        |__rift_value| {
            #[allow(unused_imports)]
            use $crate::__private::{OtherValue as _, RegisteredError as _};
            (&&$crate::__private::WorkValue(__rift_value)).registered_identity()
        }
    };
}

/// The instrumentation scope of the crate the macro expands in: its `CARGO_PKG_NAME` and
/// `CARGO_PKG_VERSION`, read where `traced!` expands, as `tracing`'s `span!` reads the
/// caller's `module_path!()` for its target.
#[doc(hidden)]
#[macro_export]
macro_rules! __rift_instrument_scope {
    () => {
        $crate::InstrumentScope::new(
            ::core::env!("CARGO_PKG_NAME"),
            ::core::env!("CARGO_PKG_VERSION"),
        )
    };
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
        $crate::__private::traced_future(
            $crate::__rift_instrument_scope!(),
            $operation,
            $work,
            move || {
                let __rift_span = $crate::__rift_traced_span!(
                    [$(__rift_parent $parent)?] [$($component)?] $operation [$($field = $value),*]
                );
                $(__rift_span.in_scope(|| $crate::__rift_traced_opened!($open));)?
                __rift_span
            },
            $crate::__rift_registered_error!(),
        )
    }};
}

/// Opens the span of an awaited operation, evaluating its fields at the first poll.
#[doc(hidden)]
#[macro_export]
macro_rules! __rift_traced_span {
    (
        [$($held:ident $parent:expr)?] [$($component:expr)?] $operation:literal
        [$($field:ident = $value:expr),*]
    ) => {{
        $crate::__private::stream_unscoped();
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
    }};
}

#[cfg(test)]
mod tests;
