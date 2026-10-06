use std::cell::Cell;
use std::future::Future;
use std::pin::pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use tracing::Event;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};

use crate::Span;
use tracing_subscriber::layer::{Context as LayerContext, SubscriberExt as _};
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{Layer, Registry};

/// One opened span: its name, explicit parent, rendered fields, and whether it closed.
#[derive(Clone, Debug)]
struct RecordedSpan {
    id: Id,
    name: &'static str,
    parent: Option<Id>,
    fields: String,
    closed: bool,
}

/// Collects every span and event target a subscriber under test sees, in order.
#[derive(Clone, Default)]
struct Recorder {
    spans: Arc<Mutex<Vec<RecordedSpan>>>,
    targets: Arc<Mutex<Vec<String>>>,
}

impl Recorder {
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

impl<S> Layer<S> for Recorder
where
    S: tracing::Subscriber + for<'lookup> tracing_subscriber::registry::LookupSpan<'lookup>,
{
    fn on_new_span(&self, attributes: &Attributes<'_>, id: &Id, _context: LayerContext<'_, S>) {
        let mut renderer = FieldRenderer(String::new());
        attributes.record(&mut renderer);
        self.spans
            .lock()
            .expect("the recorder is not poisoned")
            .push(RecordedSpan {
                id: id.clone(),
                name: attributes.metadata().name(),
                parent: attributes.parent().cloned(),
                fields: renderer.0,
                closed: false,
            });
    }

    fn on_record(
        &self,
        id: &Id,
        values: &tracing::span::Record<'_>,
        _context: LayerContext<'_, S>,
    ) {
        let mut renderer = FieldRenderer(String::new());
        values.record(&mut renderer);
        for span in self
            .spans
            .lock()
            .expect("the recorder is not poisoned")
            .iter_mut()
        {
            if span.id == *id {
                span.fields.push_str(&renderer.0);
            }
        }
    }

    fn on_close(&self, id: Id, _context: LayerContext<'_, S>) {
        for span in self
            .spans
            .lock()
            .expect("the recorder is not poisoned")
            .iter_mut()
        {
            if span.id == id {
                span.closed = true;
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, _context: LayerContext<'_, S>) {
        self.targets
            .lock()
            .expect("the recorder is not poisoned")
            .push(event.metadata().target().to_owned());
    }
}

fn recording() -> (Recorder, tracing::subscriber::DefaultGuard) {
    let recorder = Recorder::default();
    let guard = tracing_subscriber::registry()
        .with(recorder.clone())
        .set_default();
    (recorder, guard)
}

fn poll_once<Work: Future>(work: std::pin::Pin<&mut Work>) -> Poll<Work::Output> {
    work.poll(&mut Context::from_waker(Waker::noop()))
}

/// Pending on its first poll, which wakes its task, and ready on its second.
#[derive(Default)]
struct YieldOnce {
    yielded: bool,
}

impl Future for YieldOnce {
    type Output = ();

    fn poll(mut self: std::pin::Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        if self.yielded {
            Poll::Ready(())
        } else {
            self.yielded = true;
            context.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

#[test]
fn block_evaluates_once_and_returns_value_unchanged() {
    let calls = Cell::new(0_u32);
    let value = traced!(component = "index", operation = "index.parse", {
        calls.set(calls.get() + 1);
        42
    });
    let short = traced!("index.fold", {
        calls.set(calls.get() + 1);
        "folded"
    });

    assert_eq!((value, short), (42, "folded"));
    assert_eq!(calls.get(), 2);
}

fn parse(fail: bool) -> Result<u32, &'static str> {
    let value = traced!(component = "index", operation = "index.parse", {
        if fail {
            return Err("refused");
        }
        7
    });
    let checked = traced!("index.check", Ok::<u32, &str>(value))?;
    Ok(checked)
}

#[test]
fn block_return_and_question_mark_leave_the_enclosing_function_and_close_the_span() {
    let (recorder, _guard) = recording();

    assert_eq!(parse(false), Ok(7));
    assert_eq!(parse(true), Err("refused"));

    let spans = recorder.spans();
    assert_eq!(spans.len(), 3, "{spans:?}");
    assert!(spans.iter().all(|span| span.closed), "{spans:?}");
}

#[test]
fn block_break_and_continue_act_on_the_enclosing_loop() {
    let mut visited = Vec::new();
    for value in 0..10_u32 {
        traced!("index.visit", {
            if value % 2 == 0 {
                continue;
            }
            if value > 5 {
                break;
            }
            visited.push(value);
        });
    }

    assert_eq!(visited, [1, 3, 5]);
}

#[test]
fn span_carries_name_component_operation_and_fields() {
    let (recorder, _guard) = recording();

    let sources = 3_u32;
    traced!(
        component = "documentation",
        operation = "documentation.collect",
        sources = sources,
        {}
    );
    traced!("fingerprint.fold", ());

    let detailed = recorder
        .named("documentation.collect")
        .expect("the span is named after the operation literal");
    assert_eq!(
        detailed.fields,
        " component=\"documentation\" operation=\"documentation.collect\" sources=3 \
         code.function.name=\"rift_tracing::traced::tests::span_carries_name_component_operation_and_fields\""
    );
    let short = recorder
        .named("fingerprint.fold")
        .expect("the short form opens a span too");
    assert_eq!(
        short.fields,
        " operation=\"fingerprint.fold\" code.function.name=\"rift_tracing::traced::tests::\
         span_carries_name_component_operation_and_fields\""
    );
}

#[test]
fn explicit_parent_is_honored_from_another_thread() {
    let recorder = Recorder::default();
    let subscriber: Registry = tracing_subscriber::registry();
    let dispatch = tracing::Dispatch::new(subscriber.with(recorder.clone()));
    let _guard = tracing::dispatcher::set_default(&dispatch);

    let parent = crate::info_span!("package.index");
    let parent_id = Option::<Id>::from(&parent).expect("the parent span has an id");

    std::thread::spawn(move || {
        let _dispatch_guard = tracing::dispatcher::set_default(&dispatch);
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
    assert_eq!(child.parent, Some(parent_id), "{child:?}");
}

#[test]
fn future_runs_once_and_returns_value_unchanged() {
    let (recorder, _guard) = recording();
    let calls = Cell::new(0_u32);
    let calls = &calls;

    let documents = 4_usize;
    let mut work = pin!(traced!(
        component = "lexical",
        operation = "lexical.documents",
        documents = documents,
        async move {
            calls.set(calls.get() + 1);
            YieldOnce::default().await;
            documents * 2
        }
    ));

    assert!(poll_once(work.as_mut()).is_pending());
    assert_eq!(poll_once(work.as_mut()), Poll::Ready(8));
    assert_eq!(calls.get(), 1);
    let span = recorder
        .named("lexical.documents")
        .expect("the future opens a span named after the operation literal");
    assert_eq!(
        span.fields,
        " component=\"lexical\" operation=\"lexical.documents\" documents=4 \
         code.function.name=\"rift_tracing::traced::tests::future_runs_once_and_returns_value_unchanged\""
    );
}

#[test]
fn future_opens_its_span_on_first_poll_and_closes_it_when_the_work_completes() {
    let (recorder, _guard) = recording();

    let mut work = pin!(traced!("lexical.commit", async { 1 }));
    assert!(
        recorder.spans().is_empty(),
        "creating the future opens no span"
    );

    assert_eq!(poll_once(work.as_mut()), Poll::Ready(1));
    let span = recorder
        .named("lexical.commit")
        .expect("the first poll opens the span");
    assert!(
        span.closed,
        "the span closes when the work completes, while the future is still held: {span:?}"
    );
}

#[test]
fn future_evaluates_fields_at_first_poll() {
    let (recorder, _guard) = recording();
    let attempt = Cell::new(1_u32);
    let attempt = &attempt;

    let mut work = pin!(traced!(
        component = "search",
        operation = "search.read_store",
        attempt = attempt.get(),
        async {}
    ));
    attempt.set(2);
    assert!(poll_once(work.as_mut()).is_ready());

    let span = recorder
        .named("search.read_store")
        .expect("the first poll opens the span");
    assert_eq!(
        span.fields,
        " component=\"search\" operation=\"search.read_store\" attempt=2 \
         code.function.name=\"rift_tracing::traced::tests::future_evaluates_fields_at_first_poll\""
    );
}

#[test]
fn future_dropped_before_first_poll_opens_no_span() {
    let (recorder, _guard) = recording();
    let ran = Cell::new(false);

    let work = traced!(component = "lexical", operation = "lexical.commit", async {
        ran.set(true);
    });
    drop(work);

    assert!(!ran.get());
    assert!(recorder.spans().is_empty(), "{:?}", recorder.spans());
}

#[test]
fn future_cancelled_after_first_poll_closes_its_span() {
    let (recorder, _guard) = recording();

    let mut work = Box::pin(traced!("search.request", async {
        YieldOnce::default().await;
    }));
    assert!(poll_once(work.as_mut()).is_pending());
    let open = recorder
        .named("search.request")
        .expect("the first poll opens the span");
    assert!(!open.closed, "{open:?}");

    drop(work);
    let closed = recorder
        .named("search.request")
        .expect("the span was recorded");
    assert!(closed.closed, "{closed:?}");
}

#[test]
fn future_enters_its_span_only_while_polled() {
    let (recorder, _guard) = recording();
    let inside = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&inside);

    let mut work = pin!(traced!("history.batch", async move {
        seen.lock()
            .expect("not poisoned")
            .push(tracing::Span::current().id());
        YieldOnce::default().await;
        seen.lock()
            .expect("not poisoned")
            .push(tracing::Span::current().id());
    }));
    assert!(poll_once(work.as_mut()).is_pending());
    let between = tracing::Span::current().id();
    assert!(poll_once(work.as_mut()).is_ready());

    let span = recorder
        .named("history.batch")
        .expect("the span was recorded");
    let inside = inside.lock().expect("not poisoned").clone();
    assert_eq!(inside, [Some(span.id.clone()), Some(span.id)]);
    assert_eq!(between, None, "the span is not entered between polls");
}

#[test]
fn future_parent_outlives_the_caller_handle() {
    let (recorder, _guard) = recording();

    let parent = crate::info_span!("search.request");
    let parent_id = Option::<Id>::from(&parent).expect("the parent span has an id");
    let mut work = pin!(traced!(
        parent: &parent,
        component = "search",
        operation = "search.read",
        async {}
    ));
    drop(parent);
    assert!(poll_once(work.as_mut()).is_ready());

    let child = recorder
        .named("search.read")
        .expect("the span was recorded");
    assert_eq!(child.parent, Some(parent_id), "{child:?}");
}

#[test]
fn async_block_question_mark_applies_to_the_block() {
    fn refuse() -> Result<u8, &'static str> {
        Err("refused")
    }

    let mut work = pin!(traced!("index.refuse", async {
        let value = refuse()?;
        Ok::<u8, &str>(value)
    }));

    assert_eq!(poll_once(work.as_mut()), Poll::Ready(Err("refused")));
}

#[test]
fn events_keep_the_caller_module_as_target() {
    let (recorder, _guard) = recording();

    crate::info!(component = "logs", operation = "logs.drain", "drained");
    crate::warn!(target: "rift_mcp::election", "election document unreadable");

    let targets = recorder.targets.lock().expect("not poisoned").clone();
    assert_eq!(targets, [module_path!(), "rift_mcp::election"]);
}

/// A body holding this many bytes across an await keeps the future at least that large.
const BODY_BYTES: usize = 4_096;

async fn hold_across_await() -> u8 {
    let bytes = [7_u8; BODY_BYTES];
    YieldOnce::default().await;
    bytes[BODY_BYTES - 1]
}

#[test]
fn future_adds_no_more_than_instrumentation_to_the_work() {
    use tracing::Instrument as _;

    let work = hold_across_await();
    let work_bytes = size_of_val(&work);
    let instrumented = hold_across_await().instrument(tracing::Span::none());
    let traced = traced!(
        component = "search",
        operation = "search.store",
        attempt = 1_u32,
        async move { hold_across_await().await }
    );

    assert!(work_bytes >= BODY_BYTES);
    assert_eq!(size_of::<Span>(), size_of::<tracing::Span>());
    // Beside the instrumentation, the future holds the operation's completion guard.
    let completion = size_of::<crate::metrics::Completion>();
    assert!(
        size_of_val(&traced) <= size_of_val(&instrumented) + 2 * size_of::<Span>() + completion,
        "work {work_bytes}, instrumented {}, traced {}",
        size_of_val(&instrumented),
        size_of_val(&traced)
    );
}

#[tokio::test]
async fn awaited_shapes_return_their_work_value() {
    let parent = Span::current();
    let plain = hold_across_await().await;
    let short = traced!("search.read", async { hold_across_await().await }).await;
    let detailed = traced!(
        component = "search",
        operation = "search.store",
        attempt = 1_u32,
        async move { hold_across_await().await }
    )
    .await;
    let parented = traced!(
        parent: &parent,
        component = "lexical",
        operation = "lexical.commit",
        async { hold_across_await().await }
    )
    .await;

    assert_eq!([short, detailed, parented], [plain; 3]);
}

#[test]
fn span_records_declared_fields_and_runs_work_in_scope() {
    let (recorder, _guard) = recording();

    let span = crate::info_span!("cloud.request", status = tracing::field::Empty);
    span.record("status", 200_u16);
    let inside = span.in_scope(|| tracing::Span::current().id());
    let child = span.in_scope(|| traced!("cloud.decode", 3));

    let opened = recorder
        .named("cloud.request")
        .expect("the span was recorded");
    assert_eq!(
        opened.fields,
        " code.function.name=\"rift_tracing::traced::tests::\
         span_records_declared_fields_and_runs_work_in_scope\" status=200"
    );
    assert_eq!(inside, Some(opened.id.clone()));
    assert_eq!(child, 3);
    let decode = recorder
        .named("cloud.decode")
        .expect("the child was recorded");
    assert_eq!(
        decode.parent, None,
        "a contextual child carries no explicit parent"
    );
}

#[test]
fn span_instruments_a_future_and_keeps_the_caller_target() {
    let (recorder, _guard) = recording();
    let targets = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&targets);

    let mut work = pin!(crate::debug_span!("database.read").instrument(async move {
        seen.lock().expect("not poisoned").push(
            tracing::Span::current()
                .metadata()
                .map(tracing::Metadata::target),
        );
        YieldOnce::default().await;
        5
    }));
    assert!(poll_once(work.as_mut()).is_pending());
    assert_eq!(tracing::Span::current().id(), None);
    assert_eq!(poll_once(work.as_mut()), Poll::Ready(5));

    assert!(recorder.named("database.read").is_some());
    assert_eq!(
        targets.lock().expect("not poisoned").clone(),
        [Some(module_path!())]
    );
}

/// The number of calls the operation `operation` recorded under `status.code` `Error` and
/// `error_type`.
fn failed_calls(
    snapshot: &crate::MetricSnapshot,
    operation: &str,
    error_type: &str,
) -> Option<f64> {
    let labels = [
        ("span.name", operation),
        ("span.kind", "Internal"),
        ("status.code", "Error"),
        ("error.type", error_type),
    ];
    match snapshot.find("traces.span.metrics.calls", &labels)?.value() {
        crate::SeriesValue::Sum(sum) => Some(*sum),
        other => panic!("a counter holds a sum, not {other:?}"),
    }
}

/// Work that returns `Err(RiftError)` ends with its registered identity as the operation
/// metrics' `error.type` label and in its close record; an identity the registry does not
/// hold, recorded by hand, is labeled `_OTHER`, and the close record keeps it.
#[test]
fn a_registered_error_identity_is_the_error_type_label() -> Result<(), Box<dyn std::error::Error>> {
    let (recorder, mut drain) = crate::ScopedRecorder::builder().install()?;
    let stored: Result<(), rift_error::RiftError> = crate::traced!("test.registered", {
        Err(crate::store::store_failure(
            "open",
            std::path::Path::new("metrics"),
            "refused",
        ))
    });
    crate::traced!("test.unregistered", {
        crate::Span::current().record("error.type", "rift.tracing.unregistered");
    });
    let snapshot = recorder.metrics();
    drop(recorder);

    let identity = rift_error::errors::tracing::log_store_failed::SLUG.as_str();
    assert_eq!(stored.map_err(|error| error.slug().as_str()), Err(identity));
    assert_eq!(
        failed_calls(&snapshot, "test.registered", identity),
        Some(1.0),
        "{snapshot:?}"
    );
    assert_eq!(
        failed_calls(&snapshot, "test.unregistered", "_OTHER"),
        Some(1.0),
        "{snapshot:?}"
    );
    let records = drain.queued_records();
    for (operation, recorded) in [
        ("test.registered", identity),
        ("test.unregistered", "rift.tracing.unregistered"),
    ] {
        let closed = records
            .iter()
            .find(|record| record.message() == operation)
            .ok_or("the operation's span wrote its close record")?;
        let fields: serde_json::Value = serde_json::from_str(closed.fields())?;
        assert_eq!(fields["status.code"], "Error", "{fields}");
        assert_eq!(fields["error.type"], recorded, "{fields}");
    }
    Ok(())
}
