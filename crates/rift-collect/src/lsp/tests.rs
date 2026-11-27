use std::io::Result as IoResult;

use lsp_types::Range;
use rift_lsp::{Framing, TreeRoot};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream};

use super::*;
use crate::Inventory;

const CALLER: &str = "class Caller { void call(){ /*😀*/ first(); second(); changed(); } }";
const TARGET: &str = "class Target { void found() {} }";
const TIMEOUT: Duration = Duration::from_secs(5);

fn launch() -> EngineLaunch {
    EngineLaunch {
        program: "unused".into(),
        arguments: Vec::new(),
        environment: BTreeMap::new(),
        initialization_options: None,
        startup_timeout: TIMEOUT,
        request_timeout: TIMEOUT,
        settle_delay: Duration::ZERO,
        stderr_capture_bytes: 1024,
    }
}

struct Fixture {
    root: tempfile::TempDir,
    inventory: Inventory,
    paths: BTreeMap<SourceUnitId, ProjectPath>,
}

impl Fixture {
    fn new() -> Result<Self, Error> {
        let mut fixture = Self {
            root: tempfile::tempdir()?,
            inventory: Inventory::default(),
            paths: BTreeMap::new(),
        };
        fixture.source("Caller.java", CALLER)?;
        fixture.source("Target.java", TARGET)?;
        fixture.source("Changed.java", TARGET)?;
        Ok(fixture)
    }

    fn source(&mut self, name: &str, text: &str) -> Result<(), Error> {
        let unit = unit(name)?;
        std::fs::write(self.root.path().join(name), text)?;
        self.inventory.insert(CollectionSource::new(
            unit.clone(),
            name.to_owned(),
            text.into(),
        )?)?;
        self.paths.insert(unit, ProjectPath::new(name)?);
        Ok(())
    }

    fn collector(&self, deadline: Duration) -> Result<LspCollector, Error> {
        LspCollector::new(
            JvmLanguageServer::Java,
            self.root.path().to_owned(),
            self.paths.clone(),
            launch(),
            deadline,
        )
    }
}

fn unit(filename: &str) -> Result<SourceUnitId, Error> {
    Ok(SourceUnitId::parse(&format!(
        "rift://source/project/{filename}"
    ))?)
}

async fn next_message(stream: &mut BufReader<DuplexStream>) -> Value {
    let mut header = String::new();
    stream.read_line(&mut header).await.expect("reads header");
    let bytes = header
        .strip_prefix("Content-Length: ")
        .expect("framed length")
        .trim()
        .parse()
        .expect("decimal length");
    assert!(bytes < 8 << 10, "fixture message fits its bound");
    header.clear();
    stream
        .read_line(&mut header)
        .await
        .expect("reads separator");
    assert_eq!(header, "\r\n");
    let mut payload = vec![0; bytes];
    stream
        .read_exact(&mut payload)
        .await
        .expect("reads payload");
    serde_json::from_slice(&payload).expect("client message")
}

async fn send(stream: &mut BufReader<DuplexStream>, value: Value) -> IoResult<()> {
    let payload = serde_json::to_vec(&value).expect("fixture serializes");
    stream.write_all(&Framing::frame(&payload)).await
}

async fn answer(
    stream: &mut BufReader<DuplexStream>,
    request: &Value,
    result: Value,
) -> IoResult<()> {
    send(
        stream,
        json!({"jsonrpc":"2.0","id":request["id"],"result":result}),
    )
    .await
}

async fn initialize(stream: &mut BufReader<DuplexStream>, capability: bool) -> IoResult<()> {
    let request = next_message(stream).await;
    assert_eq!(request["method"], "initialize");
    answer(
        stream,
        &request,
        json!({"capabilities":{"definitionProvider":capability}}),
    )
    .await?;
    assert_eq!(next_message(stream).await["method"], "initialized");
    Ok(())
}

async fn exit(stream: &mut BufReader<DuplexStream>) -> IoResult<()> {
    let request = next_message(stream).await;
    assert_eq!(request["method"], "shutdown");
    answer(stream, &request, Value::Null).await?;
    assert_eq!(next_message(stream).await["method"], "exit");
    Ok(())
}

#[tokio::test]
async fn resolves_exact_utf16_locations_and_refuses_changed_targets() -> Result<(), Error> {
    let fixture = Fixture::new()?;
    let caller = unit("Caller.java")?;
    let mut input = CollectionInput::new(&fixture.inventory, &caller)?;
    // Same length as the admitted file. Metadata alone cannot detect this change.
    std::fs::write(
        fixture.root.path().join("Changed.java"),
        TARGET.replace("found", "other"),
    )?;
    let root = TreeRoot::new(fixture.root.path())?;
    let target = root.document_uri(&ProjectPath::new("Target.java")?)?;
    let changed = root.document_uri(&ProjectPath::new("Changed.java")?)?;
    let range = Range::new(
        lsp_types::Position::new(0, 20),
        lsp_types::Position::new(0, 25),
    );
    let (ours, theirs) = tokio::io::duplex(4096);
    let server = tokio::spawn(async move {
        let mut stream = BufReader::new(theirs);
        initialize(&mut stream, true).await?;
        let opened = next_message(&mut stream).await;
        assert_eq!(opened["method"], "textDocument/didOpen");
        assert_eq!(opened["params"]["textDocument"]["languageId"], "java");
        assert_eq!(opened["params"]["textDocument"]["text"], CALLER);
        let mut requests = Vec::with_capacity(3);
        for name in ["first", "second", "changed"] {
            let request = next_message(&mut stream).await;
            assert_eq!(request["method"], "textDocument/definition");
            let byte = CALLER.find(name).expect("position");
            let character = CALLER[..byte].encode_utf16().count();
            assert_eq!(
                request["params"]["position"],
                json!({"line":0,"character":character})
            );
            requests.push(request);
        }
        answer(
            &mut stream,
            &requests[2],
            json!({"uri": changed, "range": range}),
        )
        .await?;
        answer(
            &mut stream,
            &requests[1],
            json!([
                {"uri": "file:///unadmitted/Target.java", "range": range},
                {"uri": &target, "range": range},
            ]),
        )
        .await?;
        answer(
            &mut stream,
            &requests[0],
            json!([
                {"targetUri": target, "targetRange": range, "targetSelectionRange": range},
            ]),
        )
        .await?;
        assert_eq!(
            next_message(&mut stream).await["method"],
            "textDocument/didClose"
        );
        exit(&mut stream).await
    });

    let mut collector = fixture.collector(TIMEOUT)?;
    collector.session = Some(
        EngineSession::start_over_transport(
            launch(),
            fixture.root.path(),
            ours,
            tokio::io::empty(),
        )
        .await?,
    );
    let batch = collector.collect(&mut input, Category::Calls).await?;
    input.validate(&batch)?;
    assert_eq!(batch.coverage, Coverage::Partial);
    assert_eq!(batch.observations.len(), 3);
    let [first, second, changed] = batch.observations.as_slice() else {
        unreachable!("three calls collected");
    };
    assert_eq!(first.evidence, Evidence::Resolution);
    assert_eq!(second.evidence, Evidence::Candidate);
    assert_eq!(changed.evidence, Evidence::Unresolved);
    assert_eq!(
        first.target.as_ref().expect("first target").range,
        ByteRange { start: 20, end: 25 }
    );
    assert_eq!(first.target, second.target);
    assert!(changed.target.is_none());
    assert_eq!(input.dependencies().len(), 2);
    assert!(collector.close().await.is_some());
    server.await.expect("server task")?;
    Ok(())
}

#[tokio::test]
async fn requires_definition_capability_and_skips_other_languages() -> Result<(), Error> {
    let mut fixture = Fixture::new()?;
    fixture.source("Caller.kt", "fun caller() = target()")?;
    let mut collector = fixture.collector(TIMEOUT)?;
    let kotlin = unit("Caller.kt")?;
    let mut kotlin_input = CollectionInput::new(&fixture.inventory, &kotlin)?;
    let batch = collector
        .collect(&mut kotlin_input, Category::Calls)
        .await?;
    assert_eq!(batch, unsupported());
    assert!(collector.close().await.is_none());

    let (ours, theirs) = tokio::io::duplex(4096);
    let server = tokio::spawn(async move {
        let mut stream = BufReader::new(theirs);
        initialize(&mut stream, false).await?;
        exit(&mut stream).await
    });
    collector.session = Some(
        EngineSession::start_over_transport(
            launch(),
            fixture.root.path(),
            ours,
            tokio::io::empty(),
        )
        .await?,
    );
    let java = unit("Caller.java")?;
    let mut input = CollectionInput::new(&fixture.inventory, &java)?;
    assert_eq!(
        collector.collect(&mut input, Category::Calls).await?,
        unsupported()
    );
    assert!(collector.close().await.is_some());
    server.await.expect("server task")?;
    Ok(())
}

#[tokio::test]
async fn spawn_failure_and_missing_paths_return_errors() -> Result<(), Error> {
    let fixture = Fixture::new()?;
    let unit = unit("Caller.java")?;
    let mut input = CollectionInput::new(&fixture.inventory, &unit)?;
    let mut collector = fixture.collector(TIMEOUT)?;
    collector.launch.program = "rift-unavailable-fixture-language-server".into();
    assert!(matches!(
        collector.collect(&mut input, Category::Calls).await,
        Err(Error::Engine(_))
    ));
    collector.paths.clear();
    assert!(matches!(
        collector.collect(&mut input, Category::Calls).await,
        Err(Error::Input(_))
    ));
    Ok(())
}

#[tokio::test]
async fn a_definition_refusal_closes_the_document_and_session() -> Result<(), Error> {
    let fixture = Fixture::new()?;
    let unit = unit("Caller.java")?;
    let mut input = CollectionInput::new(&fixture.inventory, &unit)?;
    let mut collector = fixture.collector(TIMEOUT)?;
    let (ours, theirs) = tokio::io::duplex(4096);
    let server = tokio::spawn(async move {
        let mut stream = BufReader::new(theirs);
        initialize(&mut stream, true).await?;
        assert_eq!(
            next_message(&mut stream).await["method"],
            "textDocument/didOpen"
        );
        for _ in 0..3 {
            let message = next_message(&mut stream).await;
            send(
                &mut stream,
                json!({
                    "jsonrpc":"2.0","id":message["id"],
                    "error":{"code":-32603,"message":"fixture refused definition"}
                }),
            )
            .await?;
        }
        assert_eq!(
            next_message(&mut stream).await["method"],
            "textDocument/didClose"
        );
        exit(&mut stream).await
    });
    collector.session = Some(
        EngineSession::start_over_transport(
            launch(),
            fixture.root.path(),
            ours,
            tokio::io::empty(),
        )
        .await?,
    );
    assert!(matches!(
        collector.collect(&mut input, Category::Calls).await,
        Err(Error::Engine(_))
    ));
    assert!(collector.session.is_none());
    assert!(collector.close().await.is_some());
    server.await.expect("server task")?;
    Ok(())
}

#[tokio::test]
async fn deadline_leaves_the_batch_owed_and_closes_the_session() -> Result<(), Error> {
    let fixture = Fixture::new()?;
    let unit = unit("Caller.java")?;
    let mut input = CollectionInput::new(&fixture.inventory, &unit)?;
    let mut collector = fixture.collector(Duration::from_millis(100))?;
    let (ours, theirs) = tokio::io::duplex(4096);
    let server = tokio::spawn(async move {
        let mut stream = BufReader::new(theirs);
        initialize(&mut stream, true).await?;
        assert_eq!(
            next_message(&mut stream).await["method"],
            "textDocument/didOpen"
        );
        for _ in 0..8 {
            let request = next_message(&mut stream).await;
            if request["method"] == "shutdown" {
                answer(&mut stream, &request, Value::Null).await?;
                assert_eq!(next_message(&mut stream).await["method"], "exit");
                return Ok::<(), std::io::Error>(());
            }
        }
        unreachable!("bounded exchange shuts the session down");
    });
    collector.session = Some(
        EngineSession::start_over_transport(
            launch(),
            fixture.root.path(),
            ours,
            tokio::io::empty(),
        )
        .await?,
    );
    assert!(matches!(
        collector.collect(&mut input, Category::Calls).await,
        Err(Error::Deadline)
    ));
    assert!(collector.session.is_none());
    assert!(collector.close().await.is_some());
    server.await.expect("server task")?;
    Ok(())
}

#[tokio::test]
async fn unanchorable_definition_locations_leave_other_results_available() -> Result<(), Error> {
    let fixture = Fixture::new()?;
    let caller = unit("Caller.java")?;
    let mut input = CollectionInput::new(&fixture.inventory, &caller)?;
    let root = TreeRoot::new(fixture.root.path())?;
    let uri = root.document_uri(&ProjectPath::new("Target.java")?)?;
    let (ours, theirs) = tokio::io::duplex(4096);
    let server = tokio::spawn(async move {
        let mut stream = BufReader::new(theirs);
        initialize(&mut stream, true).await?;
        assert_eq!(
            next_message(&mut stream).await["method"],
            "textDocument/didOpen"
        );
        let location = |start, end| {
            json!({
                "uri": &uri,
                "range": Range::new(
                    lsp_types::Position::new(0, start),
                    lsp_types::Position::new(0, end),
                ),
            })
        };
        for result in [
            location(20, 20),
            json!([location(25, 20), location(20, 25)]),
            location(20, 100),
        ] {
            let request = next_message(&mut stream).await;
            assert_eq!(request["method"], "textDocument/definition");
            answer(&mut stream, &request, result).await?;
        }
        assert_eq!(
            next_message(&mut stream).await["method"],
            "textDocument/didClose"
        );
        exit(&mut stream).await
    });

    let mut collector = fixture.collector(TIMEOUT)?;
    collector.session = Some(
        EngineSession::start_over_transport(
            launch(),
            fixture.root.path(),
            ours,
            tokio::io::empty(),
        )
        .await?,
    );
    let batch = collector.collect(&mut input, Category::Calls).await?;
    input.validate(&batch)?;
    assert_eq!(batch.coverage, Coverage::Complete);
    let [point, multiple, outside] = batch.observations.as_slice() else {
        unreachable!("three calls collected");
    };
    for absent in [point, outside] {
        assert_eq!(absent.evidence, Evidence::Unresolved);
        assert!(absent.target.is_none());
    }
    assert_eq!(multiple.evidence, Evidence::Candidate);
    assert_eq!(
        multiple.target.as_ref().expect("valid target").range,
        ByteRange { start: 20, end: 25 }
    );
    assert!(collector.close().await.is_some());
    server.await.expect("server task")?;
    Ok(())
}
