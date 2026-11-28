//! Submits the report and only the diagnostics selected by the caller.

use std::ffi::OsString;
use std::time::Duration;

use reqwest::{StatusCode, Url};
use rift_error::{RiftError, errors};
use rift_protocol::feedback::{FeedbackField, FeedbackParams, FeedbackResult};
use serde::Serialize;

pub(crate) const TOOL_NAME: &str = "feedback";
const ENDPOINT_ENV: &str = "RIFT_FEEDBACK_URL";
/// The public Rift feedback intake. A set `RIFT_FEEDBACK_URL` replaces it.
const DEFAULT_ENDPOINT: &str = "https://feedback.volar.sh/rift/feedback/v1/reports";
const SUBMISSION_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Serialize)]
struct Report {
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<Details>,
}

#[derive(Serialize)]
struct Details {
    #[serde(skip_serializing_if = "Option::is_none")]
    os: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rift_version: Option<&'static str>,
}

impl From<FeedbackParams> for Report {
    fn from(params: FeedbackParams) -> Self {
        let include = params.include;
        let details = (!include.is_empty()).then(|| Details {
            os: include
                .contains(&FeedbackField::Os)
                .then_some(std::env::consts::OS),
            rift_version: include
                .contains(&FeedbackField::RiftVersion)
                .then_some(env!("CARGO_PKG_VERSION")),
        });
        Self {
            text: params.text,
            details,
        }
    }
}

fn parse_endpoint(value: &str) -> Option<Url> {
    let authority = value.strip_prefix("https://")?.split('/').next()?;
    let url = Url::parse(value).ok()?;
    (!authority.is_empty()
        && !authority.contains('@')
        && url.host().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none())
    .then_some(url)
}

/// Selects the intake from the server process's `RIFT_FEEDBACK_URL` value. An unset
/// variable selects [`DEFAULT_ENDPOINT`]. A set value, empty included, replaces it and
/// must be an anonymous HTTPS URL, so an empty value turns submission off.
fn resolve_endpoint(value: Option<OsString>) -> Option<Url> {
    match value {
        None => parse_endpoint(DEFAULT_ENDPOINT),
        Some(value) => value.to_str().and_then(parse_endpoint),
    }
}

pub(crate) async fn submit(params: FeedbackParams) -> Result<FeedbackResult, RiftError> {
    let endpoint = resolve_endpoint(std::env::var_os(ENDPOINT_ENV))
        .ok_or_else(|| errors::mcp::feedback_intake_unavailable().error())?;
    post(endpoint, Report::from(params)).await
}

async fn post(endpoint: Url, report: Report) -> Result<FeedbackResult, RiftError> {
    let client = reqwest::ClientBuilder::new()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(SUBMISSION_TIMEOUT)
        .build()
        .map_err(|_| errors::mcp::feedback_intake_unavailable().error())?;
    let response = client
        .post(endpoint)
        .json(&report)
        .send()
        .await
        .map_err(|_| unconfirmed("the intake returned no response"))?;
    let status = response.status();
    if status != StatusCode::NO_CONTENT {
        return Err(unconfirmed(format!(
            "the intake returned HTTP {}",
            status.as_u16()
        )));
    }
    Ok(FeedbackResult { submitted: true })
}

pub(crate) fn unconfirmed(detail: impl Into<String>) -> RiftError {
    errors::mcp::feedback_unconfirmed()
        .detail(detail.into())
        .error()
}

#[cfg(test)]
mod tests {
    use axum::body::Bytes;
    use axum::http::{HeaderMap, header};
    use axum::{Router, routing::post as route_post};
    use rift_protocol::error::{ErrorPhase, RetryDirective};
    use serde_json::{Value, json};

    use super::*;
    use crate::failure::{McpErrorExt as _, WireFailure as _};

    type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

    #[test]
    fn endpoint_accepts_anonymous_https_only() {
        assert_eq!(
            parse_endpoint("https://intake.example/submit").map(|url| url.to_string()),
            Some("https://intake.example/submit".to_owned())
        );
        for invalid in [
            "",
            "http://intake.example/submit",
            "https:////intake.example/submit",
            "https://operator:secret@intake.example",
            "https:///operator:secret@intake.example",
            "https://intake.example/submit?secret",
            "https://intake.example/submit#secret",
        ] {
            assert!(parse_endpoint(invalid).is_none());
        }
    }

    #[test]
    fn unset_variable_selects_the_public_intake_and_a_set_value_replaces_it() {
        let resolved = |value: Option<&str>| {
            resolve_endpoint(value.map(OsString::from)).map(|url| url.to_string())
        };
        assert_eq!(resolved(None).as_deref(), Some(DEFAULT_ENDPOINT));
        assert_eq!(
            resolved(Some("https://intake.example/submit")).as_deref(),
            Some("https://intake.example/submit")
        );
        for refused in [
            "",
            "http://intake.example/submit",
            "https://intake.example/?q",
        ] {
            assert_eq!(resolved(Some(refused)), None, "{refused:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_variable_refuses_instead_of_selecting_the_public_intake() {
        use std::os::unix::ffi::OsStringExt as _;
        let value = OsString::from_vec(b"https://intake.example/\xff".to_vec());
        assert_eq!(resolve_endpoint(Some(value)), None);
    }

    #[test]
    fn report_only_carries_selected_diagnostics() -> TestResult {
        let text = "  Unexpected result: λ\n";
        for (requested, expected) in [
            (json!([]), json!({"text": text})),
            (
                json!(["os"]),
                json!({"text": text, "details": {"os": std::env::consts::OS}}),
            ),
            (
                json!(["rift_version", "os"]),
                json!({"text": text, "details": {
                    "os": std::env::consts::OS,
                    "rift_version": env!("CARGO_PKG_VERSION")
                }}),
            ),
        ] {
            let params = serde_json::from_value::<FeedbackParams>(
                json!({"text": text, "include": requested}),
            )?;
            assert_eq!(serde_json::to_value(Report::from(params))?, expected);
        }
        Ok(())
    }

    #[tokio::test]
    async fn submits_once_and_only_204_confirms_storage() -> TestResult {
        let app = Router::new()
            .route(
                "/submit",
                route_post(|headers: HeaderMap, body: Bytes| async move {
                    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
                    for name in [header::AUTHORIZATION, header::COOKIE, header::USER_AGENT] {
                        assert!(!headers.contains_key(name));
                    }
                    let report: Value = serde_json::from_slice(&body).expect("valid JSON");
                    assert_eq!(report, json!({"text": "feedback fixture"}));
                    StatusCode::NO_CONTENT
                }),
            )
            .route(
                "/redirect",
                route_post(|| async {
                    (
                        StatusCode::TEMPORARY_REDIRECT,
                        [(header::LOCATION, "/submit")],
                        "sensitive response body",
                    )
                }),
            )
            .route(
                "/unconfirmed",
                route_post(|| async { (StatusCode::OK, "sensitive response body") }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = Url::parse(&format!("http://{}", listener.local_addr()?))?;
        let server = tokio::spawn(async { axum::serve(listener, app).await });
        for (path, status) in [
            ("submit", None),
            ("redirect", Some(307)),
            ("unconfirmed", Some(200)),
        ] {
            let report = Report {
                text: "feedback fixture".to_owned(),
                details: None,
            };
            let result = post(base.join(path)?, report).await;
            if let Some(status) = status {
                let error = result.expect_err("only 204 confirms persistence");
                let message = error.to_string();
                assert!(message.contains(&format!("HTTP {status}")), "{message}");
                assert!(!message.contains("sensitive response body"), "{message}");
                assert!(!message.contains(base.as_str()), "{message}");
                let wire = error.mcp().wire_error(ErrorPhase::Submit);
                assert_eq!(wire.retry, RetryDirective::OperatorAction);
            } else {
                assert!(result?.submitted);
            }
        }
        server.abort();
        Ok(())
    }
}
