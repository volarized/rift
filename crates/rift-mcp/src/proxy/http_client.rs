//! Bounded JSON messages from the elected loopback server.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::Arc;

use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderName, HeaderValue, WWW_AUTHENTICATE};
use rmcp::model::{ClientJsonRpcMessage, JsonRpcMessage, ServerJsonRpcMessage};
use rmcp::transport::common::client_side_sse::BoxedSseResponse;
use rmcp::transport::common::http_header::{
    HEADER_LAST_EVENT_ID, HEADER_MCP_PROTOCOL_VERSION, HEADER_SESSION_ID, JSON_MIME_TYPE,
};
use rmcp::transport::streamable_http_client::{
    AuthRequiredError, InsufficientScopeError, StreamableHttpClient, StreamableHttpError,
    StreamableHttpPostResponse,
};
use serde::Serialize;

use crate::transport::INBOUND_FRAME_BYTES_MAX;

/// JSON-only POST responses from Rift's loopback HTTP services.
#[derive(Clone, Debug)]
pub(super) struct HttpClient {
    inner: reqwest::Client,
}

impl HttpClient {
    pub(super) const fn new(inner: reqwest::Client) -> Self {
        Self { inner }
    }
}

impl StreamableHttpClient for HttpClient {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<reqwest::Error>> {
        let mut request = self.inner.post(uri.as_ref()).header(ACCEPT, JSON_MIME_TYPE);
        for (name, value) in custom_headers {
            if [
                ACCEPT.as_str(),
                HEADER_SESSION_ID,
                HEADER_LAST_EVENT_ID,
                HEADER_MCP_PROTOCOL_VERSION,
            ]
            .iter()
            .any(|reserved| name.as_str().eq_ignore_ascii_case(reserved))
                && !name
                    .as_str()
                    .eq_ignore_ascii_case(HEADER_MCP_PROTOCOL_VERSION)
            {
                return Err(StreamableHttpError::ReservedHeaderConflict(
                    name.to_string(),
                ));
            }
            request = request.header(name, value);
        }
        if let Some(token) = auth_header {
            request = request.bearer_auth(token);
        }
        let session_attached = session_id.is_some();
        if let Some(session) = session_id {
            request = request.header(HEADER_SESSION_ID, session.as_ref());
        }
        let body = encode_message(&message, INBOUND_FRAME_BYTES_MAX)?;
        let response = request
            .header(CONTENT_TYPE, JSON_MIME_TYPE)
            .body(body)
            .send()
            .await?;
        decode_response(
            response,
            &message,
            session_attached,
            INBOUND_FRAME_BYTES_MAX,
        )
        .await
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<reqwest::Error>> {
        self.inner
            .delete_session(uri, session_id, auth_header, custom_headers)
            .await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxedSseResponse, StreamableHttpError<reqwest::Error>> {
        self.inner
            .get_stream(uri, session_id, last_event_id, auth_header, custom_headers)
            .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<BoxedSseResponse, StreamableHttpError<reqwest::Error>> {
        self.inner
            .get_stream_with_max_sse_event_size(
                uri,
                session_id,
                last_event_id,
                auth_header,
                custom_headers,
                max_sse_event_size,
            )
            .await
    }
}

async fn decode_response(
    response: reqwest::Response,
    message: &ClientJsonRpcMessage,
    session_attached: bool,
    limit: usize,
) -> Result<StreamableHttpPostResponse, StreamableHttpError<reqwest::Error>> {
    let status = response.status();
    if let Some(header) = response.headers().get(WWW_AUTHENTICATE)
        && matches!(
            status,
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        )
    {
        let header = header
            .to_str()
            .map_err(|_| refused("invalid www-authenticate header value"))?;
        return Err(if status == reqwest::StatusCode::UNAUTHORIZED {
            StreamableHttpError::AuthRequired(AuthRequiredError::new(header.to_owned()))
        } else {
            StreamableHttpError::InsufficientScope(InsufficientScopeError::new(
                header.to_owned(),
                header_scope(header),
            ))
        });
    }
    if matches!(
        status,
        reqwest::StatusCode::ACCEPTED | reqwest::StatusCode::NO_CONTENT
    ) {
        return Ok(StreamableHttpPostResponse::Accepted);
    }
    if status == reqwest::StatusCode::NOT_FOUND && session_attached {
        return Err(StreamableHttpError::SessionExpired);
    }
    let session = response
        .headers()
        .get(HEADER_SESSION_ID)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let one_way = matches!(
        message,
        ClientJsonRpcMessage::Notification(_)
            | ClientJsonRpcMessage::Response(_)
            | ClientJsonRpcMessage::Error(_)
    );
    if status.is_success() && response.content_length() == Some(0) && one_way {
        return Ok(StreamableHttpPostResponse::Accepted);
    }
    if status.is_success()
        && !content_type
            .as_deref()
            .is_some_and(|value| value.starts_with(JSON_MIME_TYPE))
    {
        return Err(StreamableHttpError::UnexpectedContentType(content_type));
    }
    let body = read_response(response, limit).await?;
    match serde_json::from_slice::<ServerJsonRpcMessage>(&body) {
        Ok(parsed) if status.is_success() || matches!(parsed, JsonRpcMessage::Error(_)) => {
            Ok(StreamableHttpPostResponse::Json(parsed, session))
        }
        Err(_) if status.is_success() && one_way => Ok(StreamableHttpPostResponse::Accepted),
        _ => Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
            format!("HTTP {status}: invalid JSON-RPC response"),
        ))),
    }
}

/// Matches rmcp's quoted and unquoted scope handling for authentication errors.
fn header_scope(header: &str) -> Option<String> {
    let start = header.to_ascii_lowercase().find("scope=")? + "scope=".len();
    let value = &header[start..];
    if let Some(quoted) = value.strip_prefix('"') {
        return Some(quoted[..quoted.find('"')?].to_owned());
    }
    let end = value
        .find(|character: char| character == ',' || character == ';' || character.is_whitespace())
        .unwrap_or(value.len());
    (end > 0).then(|| value[..end].to_owned())
}

async fn read_response(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, StreamableHttpError<reqwest::Error>> {
    let declared = response.content_length();
    if declared.is_some_and(|length| length > u64::try_from(limit).unwrap_or(u64::MAX)) {
        return Err(refused("JSON-RPC response exceeds byte limit"));
    }
    let mut buffer = MessageBuffer::new(limit);
    while let Some(chunk) = response.chunk().await? {
        buffer
            .write_all(&chunk)
            .map_err(|_| refused("JSON-RPC response exceeds byte limit"))?;
    }
    if declared
        .is_some_and(|length| length != u64::try_from(buffer.bytes.len()).unwrap_or(u64::MAX))
    {
        return Err(refused("JSON-RPC response content length differs"));
    }
    Ok(buffer.bytes)
}

fn encode_message(
    message: &impl Serialize,
    limit: usize,
) -> Result<Vec<u8>, StreamableHttpError<reqwest::Error>> {
    encode_bounded(message, limit).map_err(StreamableHttpError::Deserialize)
}

pub(crate) fn encode_bounded(
    message: &impl Serialize,
    limit: usize,
) -> Result<Vec<u8>, serde_json::Error> {
    let mut buffer = MessageBuffer::new(limit);
    serde_json::to_writer(&mut buffer, message)?;
    Ok(buffer.bytes)
}

struct MessageBuffer {
    bytes: Vec<u8>,
    limit: usize,
}

impl MessageBuffer {
    const fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }
}

impl Write for MessageBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = next_length(self.bytes.len(), bytes.len(), self.limit)
            .ok_or_else(|| io::Error::other("JSON-RPC message exceeds byte limit"))?;
        self.bytes
            .try_reserve_exact(next - self.bytes.len())
            .map_err(io::Error::other)?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn next_length(current: usize, incoming: usize, limit: usize) -> Option<usize> {
    current.checked_add(incoming).filter(|next| *next <= limit)
}

fn refused(message: &'static str) -> StreamableHttpError<reqwest::Error> {
    StreamableHttpError::UnexpectedServerResponse(Cow::Borrowed(message))
}

#[cfg(test)]
mod tests;
