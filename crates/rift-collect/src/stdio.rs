use std::collections::VecDeque;
use std::ffi::OsString;
use std::process::Stdio;
use std::time::Duration;

use crate::{DEPENDENCIES_MAX, OBSERVATIONS_MAX};
use rift_core::SourceUnitId;
use rift_lsp::Framing;
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::{
    Batch, CONTRACT_VERSION, Category, CollectionInput, CollectionSource, Collector, Error,
    MESSAGE_BYTES_MAX,
};

const SUPPORT_REQUESTS_MAX: usize = DEPENDENCIES_MAX;
const EXCHANGE_BYTES_MAX: usize = 32 << 20;

/// JSON-RPC over Content-Length framed stdio. Starts the child on the first cache miss.
pub struct StdioCollector {
    program: OsString,
    arguments: Vec<OsString>,
    deadline: Duration,
    process: Option<Process>,
}

impl StdioCollector {
    /// Prepares a launch. The child inherits the caller's environment and stderr.
    #[must_use]
    pub const fn new(program: OsString, arguments: Vec<OsString>, deadline: Duration) -> Self {
        Self {
            program,
            arguments,
            deadline,
            process: None,
        }
    }

    /// Closes the input of a started collector and waits for its exit.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsuccessful exit or a child that exceeds its shutdown deadline.
    pub async fn close(&mut self) -> Result<(), Error> {
        let Some(Process {
            mut child,
            exchange,
        }) = self.process.take()
        else {
            return Ok(());
        };
        drop(exchange);
        let Ok(status) = tokio::time::timeout(Duration::from_secs(2), child.wait()).await else {
            stop(&mut child).await;
            return Err(Error::Deadline);
        };
        let status = status?;
        if status.success() {
            Ok(())
        } else {
            Err(Error::Protocol(format!("collector exited with {status}")))
        }
    }
}

impl Collector for StdioCollector {
    type Error = Error;

    async fn collect(
        &mut self,
        input: &mut CollectionInput<'_>,
        category: Category,
    ) -> Result<Batch, Error> {
        // Owning the child in this future means cancellation closes and kills it as well.
        let mut process = match self.process.take() {
            Some(process) => process,
            None => Process::start(&self.program, &self.arguments)?,
        };
        match tokio::time::timeout(self.deadline, process.exchange.extract(input, category)).await {
            Ok(Ok(batch)) => {
                self.process = Some(process);
                Ok(batch)
            }
            Ok(Err(error)) => {
                stop(&mut process.child).await;
                Err(error)
            }
            Err(_) => {
                stop(&mut process.child).await;
                Err(Error::Deadline)
            }
        }
    }
}

struct Process {
    child: Child,
    exchange: Exchange<ChildStdout, ChildStdin>,
}

struct Exchange<R, W> {
    input: W,
    output: R,
    framing: Framing,
    ready: VecDeque<Vec<u8>>,
    buffer: Vec<u8>,
    id: u64,
    received_bytes: usize,
    sent_bytes: usize,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SupportRequest {
    unit: String,
}

#[derive(JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct SourceInput<'a> {
    unit: String,
    filename: &'a str,
    sha256: &'a str,
    text: &'a str,
}

impl<'a> From<&'a CollectionSource> for SourceInput<'a> {
    fn from(source: &'a CollectionSource) -> Self {
        Self {
            unit: source.unit().to_string(),
            filename: source.filename(),
            sha256: source.sha256(),
            text: source.text(),
        }
    }
}

#[derive(JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct ExtractParams<'a> {
    version: u32,
    category: Category,
    observations_max: usize,
    source: SourceInput<'a>,
}

/// Returns the current extractor request and result schemas.
#[must_use]
pub fn contract_schema() -> Value {
    json!({
        "version": CONTRACT_VERSION,
        "extract_params": schema_for!(ExtractParams),
        "extract_result": schema_for!(Batch),
        "support_params": schema_for!(SupportRequest),
        "support_source": schema_for!(SourceInput),
    })
}

impl Process {
    fn start(program: &OsString, arguments: &[OsString]) -> Result<Self, Error> {
        let mut child = Command::new(program)
            .args(arguments)
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let input = child
            .stdin
            .take()
            .ok_or_else(|| Error::Protocol("missing collector stdin".into()))?;
        let output = child
            .stdout
            .take()
            .ok_or_else(|| Error::Protocol("missing collector stdout".into()))?;
        Ok(Self {
            child,
            exchange: Exchange::new(output, input),
        })
    }
}

async fn stop(child: &mut Child) {
    let _ = tokio::time::timeout(Duration::from_secs(2), child.kill()).await;
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> Exchange<R, W> {
    fn new(output: R, input: W) -> Self {
        Self {
            input,
            output,
            framing: Framing::with_message_limit(MESSAGE_BYTES_MAX),
            ready: VecDeque::new(),
            buffer: vec![0; 8192],
            id: 0,
            received_bytes: 0,
            sent_bytes: 0,
        }
    }

    async fn extract(
        &mut self,
        input: &mut CollectionInput<'_>,
        category: Category,
    ) -> Result<Batch, Error> {
        self.id = self
            .id
            .checked_add(1)
            .ok_or_else(|| Error::Protocol("request ID exhausted".into()))?;
        self.received_bytes = 0;
        self.sent_bytes = 0;
        self.send(&json!({
            "jsonrpc": "2.0", "id": self.id, "method": "extractor/extract",
            "params": ExtractParams {
                version: CONTRACT_VERSION, category,
                observations_max: OBSERVATIONS_MAX, source: SourceInput::from(input.selected()),
            }
        }))
        .await?;
        for requests in 0..=SUPPORT_REQUESTS_MAX {
            let message = self.receive().await?;
            if message["jsonrpc"] != "2.0" {
                return Err(Error::Protocol("invalid JSON-RPC version".into()));
            }
            if let Some(method) = message.get("method") {
                if requests == SUPPORT_REQUESTS_MAX {
                    break;
                }
                if method != "facts/request"
                    || !message["id"].is_string()
                    || message.get("result").is_some()
                    || message.get("error").is_some()
                {
                    return Err(Error::Protocol("invalid supporting source request".into()));
                }
                let request: SupportRequest = serde_json::from_value(message["params"].clone())?;
                let unit = SourceUnitId::parse(&request.unit)
                    .map_err(|error| Error::Protocol(error.to_string()))?;
                let result = input.read(&unit)?.map(SourceInput::from);
                self.send(&json!({ "jsonrpc": "2.0", "id": message["id"], "result": result }))
                    .await?;
                continue;
            }
            if message["id"].as_u64() != Some(self.id) {
                return Err(Error::Protocol(
                    "response ID does not match the request".into(),
                ));
            }
            match (message.get("result"), message.get("error")) {
                (Some(result), None) => return Ok(serde_json::from_value(result.clone())?),
                (None, Some(error)) => {
                    let detail = error
                        .get("message")
                        .and_then(Value::as_str)
                        .ok_or_else(|| Error::Protocol("malformed collector error".into()))?;
                    let detail: String = detail.chars().take(512).collect();
                    return Err(Error::Protocol(format!("collector returned {detail}")));
                }
                _ => {
                    return Err(Error::Protocol(
                        "response must carry one result or error".into(),
                    ));
                }
            }
        }
        Err(Error::Protocol(
            "collector exceeded its supporting request bound".into(),
        ))
    }

    async fn send(&mut self, value: &Value) -> Result<(), Error> {
        let data = serde_json::to_vec(value)?;
        if data.len() > MESSAGE_BYTES_MAX {
            return Err(Error::Protocol("request exceeds its frame bound".into()));
        }
        let frame = Framing::frame(&data);
        self.sent_bytes += frame.len();
        if self.sent_bytes > EXCHANGE_BYTES_MAX {
            return Err(Error::Protocol(
                "request exceeds its aggregate byte bound".into(),
            ));
        }
        self.input.write_all(&frame).await?;
        self.input.flush().await?;
        Ok(())
    }

    async fn receive(&mut self) -> Result<Value, Error> {
        loop {
            if let Some(data) = self.ready.pop_front() {
                return Ok(serde_json::from_slice(&data)?);
            }
            let count = self.output.read(&mut self.buffer).await?;
            if count == 0 {
                return Err(Error::Protocol(
                    "collector closed stdout before its response".into(),
                ));
            }
            self.received_bytes += count;
            if self.received_bytes > EXCHANGE_BYTES_MAX {
                return Err(Error::Protocol(
                    "response exceeds its aggregate byte bound".into(),
                ));
            }
            let frames = self
                .framing
                .feed(&self.buffer[..count])
                .map_err(|error| Error::Protocol(error.to_string()))?;
            self.ready.extend(frames);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Coverage, Inventory};

    fn fixture_source(filename: &str, text: &str) -> CollectionSource {
        let unit = SourceUnitId::parse(&format!("rift://source/project/{filename}")).expect("unit");
        CollectionSource::new(unit, filename.into(), text.into()).expect("source")
    }

    #[tokio::test]
    async fn exchange_serves_support_from_the_admitted_inventory() {
        let mut inventory = Inventory::default();
        let selected = fixture_source("entry.py", "élan()");
        let dependency = fixture_source("support.py", "def élan(): pass");
        let dependency_unit = dependency.unit().to_string();
        inventory.insert(dependency).expect("inventory");
        let selected_unit = selected.unit().clone();
        inventory.insert(selected).expect("selected source");
        let mut input = CollectionInput::new(&inventory, &selected_unit).expect("collection input");
        let (client, peer) = tokio::io::duplex(128);
        let (read, write) = tokio::io::split(client);
        let mut client = Exchange::new(read, write);
        let (read, write) = tokio::io::split(peer);
        let mut peer = Exchange::new(read, write);
        let host = async {
            let request = peer.receive().await.expect("extract request");
            assert_eq!(request["method"], "extractor/extract");
            assert_eq!(request["params"]["source"]["text"], "élan()");
            assert_eq!(request["params"]["version"], CONTRACT_VERSION);
            peer.send(&json!({
                "jsonrpc": "2.0", "id": "support", "method": "facts/request",
                "params": { "unit": dependency_unit },
            }))
            .await
            .expect("support request");
            let response = peer.receive().await.expect("support response");
            assert_eq!(response["id"], "support");
            assert_eq!(response["result"]["text"], "def élan(): pass");
            peer.send(&json!({
                "jsonrpc": "2.0", "id": request["id"],
                "result": { "coverage": "complete", "observations": [] },
            }))
            .await
            .expect("extract response");
        };
        let (batch, ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(client.extract(&mut input, Category::Calls), host)
        })
        .await
        .expect("both directions finish");
        assert_eq!(batch.expect("batch").coverage, Coverage::Complete);
        assert_eq!(input.dependencies().len(), 1);
        assert!(input.dependencies()[&dependency_unit].is_some());
    }

    #[tokio::test]
    async fn exchange_refuses_a_mismatched_response() {
        let mut inventory = Inventory::default();
        let selected = fixture_source("entry.py", "f()");
        let selected_unit = selected.unit().clone();
        inventory.insert(selected).expect("selected source");
        let mut input = CollectionInput::new(&inventory, &selected_unit).expect("collection input");
        let (client, peer) = tokio::io::duplex(128);
        let (read, write) = tokio::io::split(client);
        let mut client = Exchange::new(read, write);
        let (read, write) = tokio::io::split(peer);
        let mut peer = Exchange::new(read, write);
        let host = async {
            peer.receive().await.expect("extract request");
            peer.send(&json!({
                "jsonrpc": "2.0", "id": 2,
                "result": { "coverage": "complete", "observations": [] },
            }))
            .await
            .expect("extract response");
        };
        let (batch, ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(client.extract(&mut input, Category::Calls), host)
        })
        .await
        .expect("exchange finishes");
        assert!(matches!(batch, Err(Error::Protocol(_))));
    }
}
