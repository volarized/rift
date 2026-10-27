use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{HeaderMap, Response, StatusCode};
use serde_json::json;

use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;
const REPLY: &str = r#"{"jsonrpc":"2.0","id":1,"result":{}}"#;

struct Fixture {
    uri: Arc<str>,
    received: Arc<Mutex<Vec<HeaderMap>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn fixture(
    status: StatusCode,
    body: &str,
    headers: &[(&str, &str)],
) -> Result<Fixture, Box<dyn std::error::Error>> {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let uri = Arc::from(format!(
        "http://127.0.0.1:{}/",
        listener.local_addr()?.port()
    ));
    let received = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&received);
    let body = body.to_owned();
    let headers = headers
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect::<Vec<_>>();
    let router = axum::Router::new().route(
        "/",
        axum::routing::post(move |request: HeaderMap| {
            captured.lock().expect("fixture header lock").push(request);
            let mut response = Response::builder().status(status);
            for (name, value) in &headers {
                response = response.header(name, value);
            }
            let chunked = headers.iter().any(|(name, value)| {
                name.eq_ignore_ascii_case("transfer-encoding") && value == "chunked"
            });
            let body = if chunked {
                Body::from_stream(Body::from(body.clone()).into_data_stream())
            } else {
                Body::from(body.clone())
            };
            let response = response.body(body).expect("fixture response");
            async move { response }
        }),
    );
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Ok(Fixture {
        uri,
        received,
        task,
    })
}

fn client() -> HttpClient {
    HttpClient::new(
        reqwest::Client::builder()
            .pool_max_idle_per_host(0)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("fixture HTTP client"),
    )
}

fn request() -> ClientJsonRpcMessage {
    serde_json::from_value(json!({"jsonrpc":"2.0","id":1,"method":"ping"})).expect("MCP ping")
}

async fn response(fixture: &Fixture) -> Result<reqwest::Response, reqwest::Error> {
    reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("fixture HTTP client")
        .post(fixture.uri.as_ref())
        .body("{}")
        .send()
        .await
}

#[test]
fn full_json_rpc_envelope_counts_escaping_before_allocation() {
    let message = json!({"jsonrpc":"2.0","id":"root\"\\\n", "result":{"structuredContent":{"name":"\u{1}".repeat(200),"path":"目录/\"\\"}}});
    let expected = serde_json::to_vec(&message).expect("reference envelope");
    assert!(expected.len() > 1200, "escaped fields and envelope count");
    assert_eq!(
        encode_message(&message, expected.len()).expect("exact boundary"),
        expected
    );
    assert!(encode_message(&message, expected.len() - 1).is_err());
    let mut buffer = MessageBuffer::new(4);
    buffer.write_all(b"abc").expect("within limit");
    assert!(buffer.write_all(b"de").is_err());
    assert_eq!(buffer.as_bytes(), b"abc", "crossing chunk never appended");
    assert_eq!(
        encode_message(&message, expected.len()).expect("transport wrapper"),
        rift_protocol::message::encode_bounded(&message, expected.len()).expect("shared owner")
    );
}

#[tokio::test]
async fn declared_and_chunked_body_exact_boundary_and_overflow() -> TestResult {
    for chunked in [false, true] {
        let mut headers = vec![("content-type", "application/json")];
        if chunked {
            headers.push(("transfer-encoding", "chunked"));
        }
        let served = fixture(StatusCode::OK, REPLY, &headers).await?;
        let received = response(&served).await?;
        assert_eq!(received.content_length().is_none(), chunked);
        assert_eq!(
            read_response(received, REPLY.len()).await?,
            REPLY.as_bytes()
        );
        assert!(
            read_response(response(&served).await?, REPLY.len() - 1)
                .await
                .is_err()
        );
    }
    Ok(())
}

#[tokio::test]
async fn full_message_budget_includes_json_rpc_envelope_and_escaped_fields() -> TestResult {
    let prefix = r#"{"jsonrpc":"2.0","id":1,"result":{"structuredContent":{"name":""#;
    let suffix = r#""}}}"#;
    let available = INBOUND_FRAME_BYTES_MAX - prefix.len() - suffix.len();
    let escaped = "\\u0001".repeat(available / 6);
    let padding = "x".repeat(available % 6);
    let body = format!("{prefix}{escaped}{padding}{suffix}");
    assert_eq!(body.len(), INBOUND_FRAME_BYTES_MAX);
    let exact = fixture(
        StatusCode::OK,
        &body,
        &[("content-type", "application/json")],
    )
    .await?;
    let received = client()
        .post_message(
            Arc::clone(&exact.uri),
            request(),
            None,
            None,
            HashMap::new(),
        )
        .await?;
    assert!(matches!(received, StreamableHttpPostResponse::Json(_, _)));
    let excessive = format!("{body} ");
    let over = fixture(
        StatusCode::OK,
        &excessive,
        &[("content-type", "application/json")],
    )
    .await?;
    assert!(
        client()
            .post_message(Arc::clone(&over.uri), request(), None, None, HashMap::new())
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn truncated_content_length_never_becomes_a_decoded_response() -> TestResult {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let uri: Arc<str> = Arc::from(format!(
        "http://127.0.0.1:{}/",
        listener.local_addr()?.port()
    ));
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("fixture connection");
        let mut request = [0_u8; 4096];
        let received = stream.read(&mut request).await.expect("fixture request");
        assert!(received > 0, "fixture receives request bytes");
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{REPLY}",
            REPLY.len() + 1
        );
        stream
            .write_all(reply.as_bytes())
            .await
            .expect("fixture response");
        stream.shutdown().await.expect("fixture close");
    });
    let received = client()
        .post_message(uri, request(), None, None, HashMap::new())
        .await;
    assert!(matches!(received, Err(StreamableHttpError::Client(_))));
    task.await?;
    Ok(())
}

#[tokio::test]
async fn authorized_post_preserves_root_protocol_and_session_headers() -> TestResult {
    let served = fixture(
        StatusCode::OK,
        REPLY,
        &[
            ("content-type", "application/json"),
            (HEADER_SESSION_ID, "returned"),
        ],
    )
    .await?;
    let headers = HashMap::from([
        (
            HeaderName::from_static(crate::http::WORKSPACE_ROOT_HEADER),
            HeaderValue::from_static("/captured/root"),
        ),
        (
            HeaderName::from_static("mcp-protocol-version"),
            HeaderValue::from_static("2026-07-28"),
        ),
    ]);
    let answer = client()
        .post_message(
            Arc::clone(&served.uri),
            request(),
            Some(Arc::from("retained")),
            Some("token".to_owned()),
            headers,
        )
        .await?;
    assert!(
        matches!(answer, StreamableHttpPostResponse::Json(_, Some(session)) if session == "returned")
    );
    let received = served.received.lock().expect("captured headers");
    assert_eq!(received.len(), 1);
    assert_eq!(received[0]["authorization"], "Bearer token");
    assert_eq!(received[0]["accept"], "application/json, text/event-stream");
    assert_eq!(
        received[0][crate::http::WORKSPACE_ROOT_HEADER],
        "/captured/root"
    );
    assert_eq!(received[0]["mcp-protocol-version"], "2026-07-28");
    assert_eq!(received[0]["mcp-session-id"], "retained");
    Ok(())
}

#[tokio::test]
async fn auth_scope_status_and_json_rpc_error_survive_transport() -> TestResult {
    for (status, challenge) in [
        (StatusCode::UNAUTHORIZED, "Bearer realm=rift"),
        (StatusCode::FORBIDDEN, "Bearer scope=\"read write\""),
    ] {
        let served = fixture(status, "", &[("www-authenticate", challenge)]).await?;
        let error = client()
            .post_message(
                Arc::clone(&served.uri),
                request(),
                None,
                None,
                HashMap::new(),
            )
            .await
            .err()
            .ok_or("auth refused")?;
        match error {
            StreamableHttpError::AuthRequired(value) => {
                assert_eq!(value.www_authenticate_header, challenge);
            }
            StreamableHttpError::InsufficientScope(value) => {
                assert_eq!(value.www_authenticate_header, challenge);
                assert_eq!(value.get_required_scope(), Some("read write"));
            }
            _ => return Err("authentication error was lost".into()),
        }
    }
    let body = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"unavailable"}}"#;
    let served = fixture(
        StatusCode::SERVICE_UNAVAILABLE,
        body,
        &[("content-type", "application/json")],
    )
    .await?;
    assert!(matches!(
        client()
            .post_message(
                Arc::clone(&served.uri),
                request(),
                None,
                None,
                HashMap::new()
            )
            .await?,
        StreamableHttpPostResponse::Json(JsonRpcMessage::Error(_), _)
    ));
    Ok(())
}

#[tokio::test]
async fn accepted_session_expiry_and_sse_do_not_replay() -> TestResult {
    for status in [StatusCode::ACCEPTED, StatusCode::NO_CONTENT] {
        let served = fixture(status, "", &[]).await?;
        assert!(matches!(
            client()
                .post_message(
                    Arc::clone(&served.uri),
                    request(),
                    None,
                    None,
                    HashMap::new()
                )
                .await?,
            StreamableHttpPostResponse::Accepted
        ));
    }
    let missing = fixture(StatusCode::NOT_FOUND, "", &[]).await?;
    assert!(matches!(
        client()
            .post_message(
                Arc::clone(&missing.uri),
                request(),
                Some(Arc::from("expired")),
                None,
                HashMap::new()
            )
            .await,
        Err(StreamableHttpError::SessionExpired)
    ));
    let sse = fixture(
        StatusCode::OK,
        "data: {}\n\n",
        &[("content-type", "text/event-stream")],
    )
    .await?;
    let StreamableHttpPostResponse::Sse(mut events, _) = client()
        .post_message(Arc::clone(&sse.uri), request(), None, None, HashMap::new())
        .await?
    else {
        return Err("expected SSE response".into());
    };
    assert_eq!(
        events.next().await.ok_or("SSE event")??.data.as_deref(),
        Some("{}")
    );
    assert!(events.next().await.is_none());
    assert_eq!(
        sse.received.lock().expect("requests").len(),
        1,
        "POST not replayed"
    );
    Ok(())
}

#[tokio::test]
async fn redirects_are_not_followed_and_reserved_headers_are_refused() -> TestResult {
    let served = fixture(StatusCode::TEMPORARY_REDIRECT, "", &[("location", "/")]).await?;
    let answer = client()
        .post_message(
            Arc::clone(&served.uri),
            request(),
            None,
            None,
            HashMap::new(),
        )
        .await;
    assert!(matches!(
        answer,
        Err(StreamableHttpError::UnexpectedServerResponse(_))
    ));
    assert_eq!(served.received.lock().expect("requests").len(), 1);
    let headers = HashMap::from([(ACCEPT, HeaderValue::from_static("text/event-stream"))]);
    assert!(matches!(
        client()
            .post_message(Arc::clone(&served.uri), request(), None, None, headers)
            .await,
        Err(StreamableHttpError::ReservedHeaderConflict(_))
    ));
    assert_eq!(
        served.received.lock().expect("requests").len(),
        1,
        "refused before forwarding"
    );
    Ok(())
}

#[tokio::test]
async fn sse_declared_and_chunked_response_bounds_precede_parsing() -> TestResult {
    let body = format!("event: message\ndata: {REPLY}\n\n");
    for chunked in [false, true] {
        let mut headers = vec![
            ("content-type", "text/event-stream"),
            (HEADER_SESSION_ID, "returned"),
        ];
        if chunked {
            headers.push(("transfer-encoding", "chunked"));
        }
        let served = fixture(StatusCode::OK, &body, &headers).await?;
        let exact = client()
            .post_message_with_max_sse_event_size(
                Arc::clone(&served.uri),
                request(),
                None,
                None,
                HashMap::new(),
                body.len(),
            )
            .await?;
        let StreamableHttpPostResponse::Sse(mut events, session) = exact else {
            return Err("expected SSE response".into());
        };
        assert_eq!(session.as_deref(), Some("returned"));
        let event = events.next().await.ok_or("SSE event")??;
        assert_eq!(event.event.as_deref(), Some("message"));
        assert_eq!(event.data.as_deref(), Some(REPLY));
        assert!(events.next().await.is_none());
        let over = client()
            .post_message_with_max_sse_event_size(
                Arc::clone(&served.uri),
                request(),
                None,
                None,
                HashMap::new(),
                body.len() - 1,
            )
            .await;
        if chunked {
            let StreamableHttpPostResponse::Sse(mut events, _) = over? else {
                return Err("expected bounded SSE stream".into());
            };
            assert!(events.next().await.ok_or("stream refusal")?.is_err());
            assert!(events.next().await.is_none());
        } else {
            assert!(matches!(
                over,
                Err(StreamableHttpError::UnexpectedServerResponse(_))
            ));
        }
    }
    Ok(())
}

#[tokio::test]
async fn unfinished_sse_and_complete_post_response_share_the_raw_byte_bound() -> TestResult {
    for body in [
        "data: unfinished".to_owned(),
        "data: one\n\ndata: two\n\n".to_owned(),
    ] {
        let served = fixture(
            StatusCode::OK,
            &body,
            &[
                ("content-type", "text/event-stream"),
                ("transfer-encoding", "chunked"),
            ],
        )
        .await?;
        let StreamableHttpPostResponse::Sse(mut events, _) = client()
            .post_message_with_max_sse_event_size(
                Arc::clone(&served.uri),
                request(),
                None,
                None,
                HashMap::new(),
                body.len() - 1,
            )
            .await?
        else {
            return Err("expected bounded SSE stream".into());
        };
        let mut refused = false;
        while let Some(event) = events.next().await {
            if event.is_err() {
                refused = true;
                break;
            }
        }
        assert!(
            refused,
            "the raw response limit includes unfinished and multiple events"
        );
    }
    let over = fixture(
        StatusCode::OK,
        &"x".repeat(INBOUND_FRAME_BYTES_MAX + 1),
        &[("content-type", "text/event-stream")],
    )
    .await?;
    assert!(
        client()
            .post_message(Arc::clone(&over.uri), request(), None, None, HashMap::new(),)
            .await
            .is_err()
    );
    Ok(())
}

#[derive(Clone)]
struct HttpHandler;
impl rmcp::ServerHandler for HttpHandler {}

async fn response_value(
    answer: StreamableHttpPostResponse,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    match answer {
        StreamableHttpPostResponse::Json(message, _) => Ok(serde_json::to_value(message)?),
        StreamableHttpPostResponse::Sse(mut events, _) => {
            while let Some(event) = events.next().await {
                let event = event?;
                if let Some(data) = event.data {
                    return Ok(serde_json::from_str(&data)?);
                }
            }
            Err("SSE reply is missing".into())
        }
        _ => Err("JSON-RPC reply is missing".into()),
    }
}

#[tokio::test]
async fn initialize_and_list_tools_accept_json_and_sse_from_pinned_http_service() -> TestResult {
    use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
    };

    for json_response in [true, false] {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let uri: Arc<str> = Arc::from(format!(
            "http://127.0.0.1:{}/",
            listener.local_addr()?.port()
        ));
        let service = StreamableHttpService::new(
            || Ok(HttpHandler),
            Arc::new(NeverSessionManager::default()),
            StreamableHttpServerConfig::default()
                .with_legacy_session_mode(false)
                .with_json_response(json_response),
        );
        let router = axum::Router::new().fallback_service(service);
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        let served = Fixture {
            uri,
            received: Arc::new(Mutex::new(Vec::new())),
            task,
        };
        let initialize = serde_json::from_value(json!({
            "jsonrpc":"2.0", "id":1, "method":"initialize", "params":{
                "protocolVersion":"2025-11-25", "capabilities":{},
                "clientInfo":{"name":"transport-test-client", "version":"0.0.0"}
            }
        }))?;
        let answer = client()
            .post_message(
                Arc::clone(&served.uri),
                initialize,
                None,
                None,
                HashMap::new(),
            )
            .await?;
        let answer = response_value(answer).await?;
        assert_eq!(answer["id"], 1);
        assert!(answer.get("error").is_none(), "initialize: {answer}");
        assert_eq!(answer["result"]["protocolVersion"], "2025-11-25");
        let tools =
            serde_json::from_value(json!({"jsonrpc":"2.0", "id":2, "method":"tools/list"}))?;
        let headers = HashMap::from([(
            HeaderName::from_static("mcp-protocol-version"),
            HeaderValue::from_static("2025-11-25"),
        )]);
        let answer = response_value(
            client()
                .post_message(Arc::clone(&served.uri), tools, None, None, headers)
                .await?,
        )
        .await?;
        assert_eq!(answer["id"], 2);
        assert!(answer.get("error").is_none(), "list tools: {answer}");
        assert_eq!(answer["result"]["tools"], json!([]));
    }
    Ok(())
}
