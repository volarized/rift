//! `traced!`/`traced_async!`: one way to time a piece of work through `tracing` spans.
//!
//! A collector (OTLP, `rift://logs`) already reads elapsed time from a span's own open
//! and close; a hand-written `Instant::now()` pair beside it is a second, driftable
//! source of the same fact. Wrapping the work in these macros instead opens one `tracing`
//! span named by an `operation` literal, carrying `component` and `operation` fields plus
//! whatever extra fields the caller passes, and times it the way every collector already
//! measures a span.

/// Opens a `tracing` span named by `operation`, enters it, and evaluates the block once.
///
/// The span carries `component` and `operation` - both recorded as ordinary fields, in
/// addition to `operation` naming the span itself - plus any extra `name = value` fields
/// listed before the block:
///
/// ```
/// use rift_core::traced;
///
/// let sum = traced!(component = "index", operation = "index.parse", { 1 + 1 });
/// assert_eq!(sum, 2);
///
/// let sources = 3;
/// let scanned = traced!(
///     component = "documentation",
///     operation = "documentation.collect",
///     sources = sources,
///     { "scanned" }
/// );
/// assert_eq!(scanned, "scanned");
/// ```
///
/// `operation` is a string literal: a `tracing` span's name is part of its static
/// [`Metadata`](tracing::Metadata), fixed at compile time by the underlying `span!`
/// macro, so a computed name cannot be threaded through it.
///
/// The block runs exactly once, entered under a
/// [`tracing::span::EnteredSpan`] guard rather than a closure, so `?`, `return`, and
/// `break` inside it behave exactly as they would without the macro: the guard exits the
/// span on drop, on every path out, including an early return from the enclosing
/// function.
///
/// An explicit `parent: <expr>` names the span's parent directly:
///
/// ```
/// use rift_core::traced;
///
/// let parent = tracing::info_span!("package.index");
/// std::thread::spawn(move || {
///     traced!(parent: &parent, component = "dependency", operation = "package.analyze", {
///         // work that runs on a thread the parent span never entered
///     });
/// })
/// .join()
/// .expect("the worker thread does not panic");
/// ```
///
/// Work that runs on a thread with no ambient span - a `rayon` worker, a detached
/// `std::thread` - has no contextual parent to inherit, and a span opened there without
/// one starts a new, disconnected trace. Pass the span the calling thread held before it
/// handed off the work.
#[macro_export]
macro_rules! traced {
    (component = $component:expr, operation = $operation:literal $(, $field:ident = $value:expr)* , $body:block) => {{
        let __rift_guard = $crate::tracing::span!(
            $crate::tracing::Level::INFO,
            $operation,
            component = $component,
            operation = $operation,
            $($field = $value),*
        )
        .entered();
        $body
    }};
    (parent: $parent:expr, component = $component:expr, operation = $operation:literal $(, $field:ident = $value:expr)* , $body:block) => {{
        let __rift_guard = $crate::tracing::span!(
            parent: $parent,
            $crate::tracing::Level::INFO,
            $operation,
            component = $component,
            operation = $operation,
            $($field = $value),*
        )
        .entered();
        $body
    }};
}

/// Instruments a future with a `tracing` span named by `operation`, the async
/// counterpart to [`traced!`].
///
/// The block is wrapped in `async move { .. }` and attached to the span through
/// [`tracing::Instrument`], which enters the span only while the future is polled and
/// exits it between polls - never held across an `.await` as a guard, because a
/// multi-threaded runtime can resume a suspended future on a different thread than the
/// one that entered it. The macro evaluates to that instrumented future; the caller
/// awaits it, so the block still runs exactly once and its value comes back unchanged:
///
/// ```
/// use rift_core::traced_async;
///
/// # async fn commit() -> u8 {
/// traced_async!(component = "lexical", operation = "lexical.commit", { 1 + 1 }).await
/// # }
/// # let _ = commit();
/// ```
///
/// `parent: <expr>` names the span's parent directly, the same way [`traced!`] accepts
/// it, for a future spawned onto a task with no ambient span of its own.
#[macro_export]
macro_rules! traced_async {
    (component = $component:expr, operation = $operation:literal $(, $field:ident = $value:expr)* , $body:block) => {{
        #[allow(unused_imports)]
        use $crate::tracing::Instrument as _;
        let __rift_span = $crate::tracing::span!(
            $crate::tracing::Level::INFO,
            $operation,
            component = $component,
            operation = $operation,
            $($field = $value),*
        );
        async move { $body }.instrument(__rift_span)
    }};
    (parent: $parent:expr, component = $component:expr, operation = $operation:literal $(, $field:ident = $value:expr)* , $body:block) => {{
        #[allow(unused_imports)]
        use $crate::tracing::Instrument as _;
        let __rift_span = $crate::tracing::span!(
            parent: $parent,
            $crate::tracing::Level::INFO,
            $operation,
            component = $component,
            operation = $operation,
            $($field = $value),*
        );
        async move { $body }.instrument(__rift_span)
    }};
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::{Arc, Mutex};

    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id};
    use tracing_subscriber::layer::{Context, SubscriberExt as _};
    use tracing_subscriber::util::SubscriberInitExt as _;
    use tracing_subscriber::{Layer, Registry};

    /// One opened span's name, explicit parent, and rendered fields.
    #[derive(Clone, Debug)]
    struct RecordedSpan {
        name: &'static str,
        parent: Option<Id>,
        fields: String,
    }

    /// Collects every span a subscriber under test opens, in order.
    #[derive(Clone, Default)]
    struct SpanRecorder {
        spans: Arc<Mutex<Vec<RecordedSpan>>>,
    }

    impl SpanRecorder {
        fn spans(&self) -> Vec<RecordedSpan> {
            self.spans
                .lock()
                .expect("the recorder is not poisoned")
                .clone()
        }

        fn named(&self, name: &str) -> Option<RecordedSpan> {
            self.spans().into_iter().find(|span| span.name == name)
        }
    }

    struct FieldRenderer(String);

    impl Visit for FieldRenderer {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write as _;
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }

    impl<S> Layer<S> for SpanRecorder
    where
        S: tracing::Subscriber + for<'lookup> tracing_subscriber::registry::LookupSpan<'lookup>,
    {
        fn on_new_span(&self, attributes: &Attributes<'_>, _id: &Id, _context: Context<'_, S>) {
            let mut renderer = FieldRenderer(String::new());
            attributes.record(&mut renderer);
            self.spans
                .lock()
                .expect("the recorder is not poisoned")
                .push(RecordedSpan {
                    name: attributes.metadata().name(),
                    parent: attributes.parent().cloned(),
                    fields: renderer.0,
                });
        }
    }

    #[test]
    fn sync_macro_returns_value_and_runs_body_once() {
        let calls = Cell::new(0_u32);
        let value = traced!(component = "index", operation = "index.parse", {
            calls.set(calls.get() + 1);
            42
        });
        assert_eq!(value, 42);
        assert_eq!(calls.get(), 1);
    }

    /// `?` inside the block exits the enclosing function, not just the macro's own block -
    /// the guard drops on that path exactly as it would drop leaving an ordinary block.
    fn fallible(fail: bool) -> Result<u32, &'static str> {
        let value = traced!(component = "index", operation = "index.parse", {
            if fail {
                return Err("refused");
            }
            7
        });
        Ok(value)
    }

    #[test]
    fn sync_macro_question_mark_propagates_from_enclosing_function() {
        assert_eq!(fallible(false), Ok(7));
        assert_eq!(fallible(true), Err("refused"));
    }

    #[test]
    fn sync_macro_span_carries_component_operation_and_extra_fields() {
        let recorder = SpanRecorder::default();
        let _guard = tracing_subscriber::registry()
            .with(recorder.clone())
            .set_default();

        let sources = 3_u32;
        traced!(
            component = "documentation",
            operation = "documentation.collect",
            sources = sources,
            {}
        );

        let span = recorder
            .named("documentation.collect")
            .expect("the macro opens a span named after the operation literal");
        assert!(
            span.fields.contains("component=\"documentation\""),
            "{span:?}"
        );
        assert!(
            span.fields.contains("operation=\"documentation.collect\""),
            "{span:?}"
        );
        assert!(span.fields.contains("sources=3"), "{span:?}");
    }

    #[test]
    fn explicit_parent_is_honored_from_another_thread() {
        let recorder = SpanRecorder::default();
        let subscriber: Registry = tracing_subscriber::registry();
        let dispatch = tracing::Dispatch::new(subscriber.with(recorder.clone()));
        let _guard = tracing::dispatcher::set_default(&dispatch);

        let parent = tracing::info_span!("package.index");
        let parent_id = parent.id().expect("the entered span has an id");

        std::thread::spawn(move || {
            let _dispatch_guard = tracing::dispatcher::set_default(&dispatch.clone());
            traced!(
                parent: &parent,
                component = "dependency",
                operation = "package.analyze",
                {}
            );
        })
        .join()
        .expect("the worker thread does not panic");

        let child = recorder
            .named("package.analyze")
            .expect("the worker thread's span was recorded");
        assert_eq!(
            child.parent,
            Some(parent_id),
            "a span opened with an explicit parent on another thread carries that parent, \
             not none: {child:?}"
        );
    }

    #[tokio::test]
    async fn async_variant_attaches_span_to_the_future() {
        let recorder = SpanRecorder::default();
        let _guard = tracing_subscriber::registry()
            .with(recorder.clone())
            .set_default();

        let calls = Cell::new(0_u32);
        let calls_ref = &calls;
        let value = traced_async!(component = "lexical", operation = "lexical.commit", {
            calls_ref.set(calls_ref.get() + 1);
            tokio::task::yield_now().await;
            9
        })
        .await;

        assert_eq!(value, 9);
        assert_eq!(calls.get(), 1);
        let span = recorder
            .named("lexical.commit")
            .expect("the async macro opens a span named after the operation literal");
        assert!(span.fields.contains("component=\"lexical\""), "{span:?}");
        assert!(
            span.fields.contains("operation=\"lexical.commit\""),
            "{span:?}"
        );
    }
}
