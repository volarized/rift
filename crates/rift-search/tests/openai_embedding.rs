//! The OpenAI-compatible embedding arm, against a mock endpoint.
//!
//! Every case here serves the endpoint from this process on a loopback port,
//! so the suite makes no external request. What it proves is the contract
//! Rift states for such a service: the configured credential, base URL,
//! dimensions, and attempt count are used; the declared response `index`
//! decides which input each vector belongs to; and a response that cannot
//! state that association is refused rather than mis-associated.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use rift_search::{
    BatchSchedule, EmbeddingModels, EmbeddingSpace, REMOTE_INPUTS_MAX, RemoteEmbeddingSettings,
    RetrievalModels, RiftOpenAiEmbeddingModel,
};
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// How the mock endpoint answers one request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Answer {
    /// One vector per input, in declared index order.
    InOrder,
    /// One vector per input, the array reversed while the declared indexes
    /// stay correct.
    Reversed,
    /// Two vectors declaring the same index.
    DuplicateIndex,
    /// One vector declaring an index past the request's own length.
    IndexPastEnd,
    /// Fewer vectors than inputs.
    FewerThanAsked,
    /// A coordinate past what the stored `f32` format can hold.
    UnholdableCoordinate,
    /// The service refuses with a status another attempt could answer.
    Refuses,
    /// The service refuses the request as sent, with a status every repeat
    /// would meet again.
    RefusesTheRequest,
}

/// What the mock endpoint recorded about the requests it served.
#[derive(Debug, Default)]
struct Recorded {
    requests: AtomicUsize,
    inputs_max: AtomicUsize,
    in_flight: AtomicUsize,
    in_flight_max: AtomicUsize,
}

#[derive(Clone)]
struct MockState {
    answer: Answer,
    recorded: Arc<Recorded>,
    authorization: Arc<std::sync::Mutex<Option<String>>>,
    delay: Duration,
}

/// The vector one input position is answered with: a one-hot vector whose set
/// coordinate is that position, so a mis-association is visible in the value.
fn vector_for(position: usize, dimensions: usize) -> Vec<f64> {
    (0..dimensions)
        .map(|axis| if axis == position { 1.0 } else { 0.0 })
        .collect()
}

async fn embeddings(
    State(state): State<MockState>,
    headers: HeaderMap,
    body: String,
) -> (StatusCode, String) {
    if let Some(value) = headers.get("authorization")
        && let Ok(value) = value.to_str()
    {
        *state
            .authorization
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(value.to_owned());
    }
    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let inputs = parsed["input"].as_array().map_or(0, Vec::len);
    let dimensions = usize::try_from(parsed["dimensions"].as_u64().unwrap_or(0)).unwrap_or(0);
    state.recorded.requests.fetch_add(1, Ordering::SeqCst);
    state
        .recorded
        .inputs_max
        .fetch_max(inputs, Ordering::SeqCst);
    let open = state.recorded.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    state
        .recorded
        .in_flight_max
        .fetch_max(open, Ordering::SeqCst);
    if !state.delay.is_zero() {
        tokio::time::sleep(state.delay).await;
    }
    state.recorded.in_flight.fetch_sub(1, Ordering::SeqCst);

    if state.answer == Answer::Refuses {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": {"message": "the model is loading"}}).to_string(),
        );
    }
    if state.answer == Answer::RefusesTheRequest {
        return (
            StatusCode::BAD_REQUEST,
            json!({"error": {"message": "this model does not serve that width"}}).to_string(),
        );
    }
    let mut data: Vec<Value> = (0..inputs)
        .map(|position| {
            json!({
                "object": "embedding",
                "index": position,
                "embedding": vector_for(position, dimensions),
            })
        })
        .collect();
    match state.answer {
        Answer::Reversed => data.reverse(),
        Answer::DuplicateIndex => {
            if let Some(last) = data.last_mut() {
                last["index"] = json!(0);
            }
        }
        Answer::IndexPastEnd => {
            if let Some(last) = data.last_mut() {
                last["index"] = json!(inputs);
            }
        }
        Answer::FewerThanAsked => {
            data.pop();
        }
        Answer::UnholdableCoordinate => {
            if let Some(first) = data.first_mut() {
                first["embedding"] = json!(vec![f64::MAX; dimensions]);
            }
        }
        Answer::InOrder | Answer::Refuses | Answer::RefusesTheRequest => {}
    }
    (
        StatusCode::OK,
        json!({
            "object": "list",
            "model": "mock",
            "data": data,
            "usage": {"prompt_tokens": 1, "total_tokens": 1},
        })
        .to_string(),
    )
}

/// Serves the mock endpoint and answers its base URL and its record.
async fn serve(answer: Answer, delay: Duration) -> TestResult<(String, Arc<Recorded>, MockState)> {
    let recorded = Arc::new(Recorded::default());
    let state = MockState {
        answer,
        recorded: Arc::clone(&recorded),
        authorization: Arc::new(std::sync::Mutex::new(None)),
        delay,
    };
    let router = Router::new()
        .route("/v1/embeddings", post(embeddings))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await?;
    let base = format!("http://{}/v1", listener.local_addr()?);
    tokio::spawn(async move {
        let _served = axum::serve(listener, router).await;
    });
    Ok((base, recorded, state))
}

fn settings(endpoint: String, dimensions: usize) -> RemoteEmbeddingSettings {
    RemoteEmbeddingSettings {
        endpoint,
        model: "text-embedding-3-small".to_owned(),
        revision: "2024-01-25".to_owned(),
        dimensions,
        api_key: "test-key".to_owned(),
        request_timeout: Duration::from_secs(5),
        attempts: 1,
    }
}

fn models(settings: &RemoteEmbeddingSettings) -> TestResult<EmbeddingModels> {
    let space = EmbeddingSpace::remote(
        settings.endpoint.clone(),
        settings.model.clone(),
        settings.revision.clone(),
        settings.dimensions,
    );
    let model = RiftOpenAiEmbeddingModel::new(settings)?;
    Ok(EmbeddingModels::OpenAi(RetrievalModels::new(
        model.clone(),
        model,
        space,
    )))
}

fn texts(count: usize) -> Vec<String> {
    (0..count)
        .map(|index| format!("document {index}"))
        .collect()
}

#[tokio::test]
async fn the_configured_endpoint_key_and_dimensions_reach_the_service() -> TestResult {
    let (endpoint, recorded, state) = serve(Answer::InOrder, Duration::ZERO).await?;
    let settings = settings(endpoint, 4);
    let embedded = models(&settings)?
        .embed_documents(texts(2), BatchSchedule::new(8, 1))
        .await?;
    assert_eq!(embedded.len(), 2);
    assert!(embedded.iter().all(|vector| vector.len() == 4));
    assert_eq!(recorded.requests.load(Ordering::SeqCst), 1);
    assert_eq!(
        state
            .authorization
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_deref(),
        Some("Bearer test-key")
    );
    Ok(())
}

#[tokio::test]
async fn a_reversed_response_still_lands_each_vector_on_its_own_input() -> TestResult {
    let (endpoint, _recorded, _state) = serve(Answer::Reversed, Duration::ZERO).await?;
    let settings = settings(endpoint, 3);
    let embedded = models(&settings)?
        .embed_documents(texts(3), BatchSchedule::new(8, 1))
        .await?;
    for (position, vector) in embedded.iter().enumerate() {
        assert!(
            (vector[position] - 1.0).abs() < 1e-6,
            "input {position} must carry the vector the service declared for it: {vector:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_duplicate_response_index_is_refused() -> TestResult {
    let (endpoint, _recorded, _state) = serve(Answer::DuplicateIndex, Duration::ZERO).await?;
    let settings = settings(endpoint, 3);
    let refused = models(&settings)?
        .embed_documents(texts(3), BatchSchedule::new(8, 1))
        .await;
    assert!(refused.is_err(), "a repeated index must be refused");
    Ok(())
}

#[tokio::test]
async fn a_response_index_past_the_request_is_refused() -> TestResult {
    let (endpoint, _recorded, _state) = serve(Answer::IndexPastEnd, Duration::ZERO).await?;
    let settings = settings(endpoint, 3);
    let refused = models(&settings)?
        .embed_documents(texts(3), BatchSchedule::new(8, 1))
        .await;
    assert!(refused.is_err(), "an index past the inputs must be refused");
    Ok(())
}

#[tokio::test]
async fn a_response_covering_fewer_inputs_than_it_was_given_is_refused() -> TestResult {
    let (endpoint, _recorded, _state) = serve(Answer::FewerThanAsked, Duration::ZERO).await?;
    let settings = settings(endpoint, 3);
    let refused = models(&settings)?
        .embed_documents(texts(3), BatchSchedule::new(8, 1))
        .await;
    assert!(refused.is_err(), "a short answer must be refused");
    Ok(())
}

#[tokio::test]
async fn a_coordinate_the_stored_format_cannot_hold_is_refused() -> TestResult {
    let (endpoint, _recorded, _state) = serve(Answer::UnholdableCoordinate, Duration::ZERO).await?;
    let settings = settings(endpoint, 3);
    let refused = models(&settings)?
        .embed_documents(texts(2), BatchSchedule::new(8, 1))
        .await;
    assert!(
        refused.is_err(),
        "a coordinate outside the stored format must be refused"
    );
    Ok(())
}

#[tokio::test]
async fn a_request_the_service_refuses_is_sent_once() -> TestResult {
    // A status the service answers the same way every time ends the request
    // at once: retrying a rejected width, credential, or model spends the
    // pass's budget on the same refusal and delays the answer the caller is
    // waiting for.
    let (endpoint, recorded, _state) = serve(Answer::RefusesTheRequest, Duration::ZERO).await?;
    let mut settings = settings(endpoint, 3);
    settings.attempts = 3;
    let refused = models(&settings)?
        .embed_documents(texts(1), BatchSchedule::new(8, 1))
        .await;
    assert!(refused.is_err(), "a refused request must refuse the pass");
    assert_eq!(
        recorded.requests.load(Ordering::SeqCst),
        1,
        "the status decides, not the fact that an error arrived"
    );
    Ok(())
}

#[tokio::test]
async fn a_service_refusal_is_retried_up_to_the_attempt_bound() -> TestResult {
    let (endpoint, recorded, _state) = serve(Answer::Refuses, Duration::ZERO).await?;
    let mut settings = settings(endpoint, 3);
    settings.attempts = 3;
    let refused = models(&settings)?
        .embed_documents(texts(1), BatchSchedule::new(8, 1))
        .await;
    assert!(refused.is_err(), "a refusing service must refuse the pass");
    assert_eq!(
        recorded.requests.load(Ordering::SeqCst),
        3,
        "the attempt bound is what stops the retry"
    );
    Ok(())
}

#[tokio::test]
async fn the_batch_bound_cuts_one_pass_into_requests() -> TestResult {
    let (endpoint, recorded, _state) = serve(Answer::InOrder, Duration::ZERO).await?;
    let settings = settings(endpoint, 2);
    let embedded = models(&settings)?
        .embed_documents(texts(7), BatchSchedule::new(3, 1))
        .await?;
    assert_eq!(embedded.len(), 7);
    assert_eq!(recorded.requests.load(Ordering::SeqCst), 3);
    assert_eq!(recorded.inputs_max.load(Ordering::SeqCst), 3);
    Ok(())
}

#[tokio::test]
async fn the_concurrency_bound_limits_the_requests_open_at_once() -> TestResult {
    let (endpoint, recorded, _state) = serve(Answer::InOrder, Duration::from_millis(40)).await?;
    let settings = settings(endpoint, 2);
    let embedded = models(&settings)?
        .embed_documents(texts(8), BatchSchedule::new(1, 2))
        .await?;
    assert_eq!(embedded.len(), 8);
    assert_eq!(recorded.requests.load(Ordering::SeqCst), 8);
    assert!(
        recorded.in_flight_max.load(Ordering::SeqCst) <= 2,
        "at most two requests may be open at once, observed {}",
        recorded.in_flight_max.load(Ordering::SeqCst)
    );
    Ok(())
}

#[tokio::test]
async fn a_request_past_its_timeout_is_refused() -> TestResult {
    let (endpoint, _recorded, _state) = serve(Answer::InOrder, Duration::from_millis(400)).await?;
    let mut settings = settings(endpoint, 2);
    settings.request_timeout = Duration::from_millis(50);
    let refused = models(&settings)?
        .embed_documents(texts(1), BatchSchedule::new(8, 1))
        .await;
    assert!(refused.is_err(), "a request past its timeout must refuse");
    Ok(())
}

#[tokio::test]
async fn a_returned_width_the_space_does_not_declare_is_refused() -> TestResult {
    let (endpoint, _recorded, _state) = serve(Answer::InOrder, Duration::ZERO).await?;
    let mut settings = settings(endpoint, 4);
    let space = EmbeddingSpace::remote(
        settings.endpoint.clone(),
        settings.model.clone(),
        settings.revision.clone(),
        8,
    );
    settings.dimensions = 4;
    let model = RiftOpenAiEmbeddingModel::new(&settings)?;
    let held = EmbeddingModels::OpenAi(RetrievalModels::new(model.clone(), model, space));
    let refused = held
        .embed_documents(texts(1), BatchSchedule::new(8, 1))
        .await;
    assert!(
        refused.is_err(),
        "a vector of another width than the space declares must be refused"
    );
    Ok(())
}

#[tokio::test]
async fn one_query_embeds_through_the_query_handle() -> TestResult {
    let (endpoint, recorded, _state) = serve(Answer::InOrder, Duration::ZERO).await?;
    let settings = settings(endpoint, 3);
    let embedded = models(&settings)?
        .embed_query("where is the beacon")
        .await?;
    assert_eq!(embedded.len(), 3);
    assert_eq!(recorded.requests.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn a_pass_past_the_remote_input_bound_is_cut_into_requests() -> TestResult {
    let (endpoint, recorded, _state) = serve(Answer::InOrder, Duration::ZERO).await?;
    let settings = settings(endpoint, 1);
    let asked = REMOTE_INPUTS_MAX + 1;
    let embedded = models(&settings)?
        .embed_documents(texts(asked), BatchSchedule::new(asked, 1))
        .await?;
    assert_eq!(embedded.len(), asked);
    assert_eq!(
        recorded.inputs_max.load(Ordering::SeqCst),
        REMOTE_INPUTS_MAX,
        "no request may carry more inputs than the service accepts"
    );
    assert_eq!(
        recorded.requests.load(Ordering::SeqCst),
        2,
        "a pass one input past the bound is two requests"
    );
    Ok(())
}

#[test]
fn two_spaces_differing_in_one_field_carry_two_identities() {
    let one = EmbeddingSpace::remote("https://one.example/v1", "model", "2024-01-25", 1_536);
    let other = EmbeddingSpace::remote("https://two.example/v1", "model", "2024-01-25", 1_536);
    assert_ne!(one.identity(), other.identity());
    let widened = EmbeddingSpace::remote("https://one.example/v1", "model", "2024-01-25", 768);
    assert_ne!(one.identity(), widened.identity());
    assert_eq!(one.identity(), one.identity());
}
