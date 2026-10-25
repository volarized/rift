//! The packaged Rift executable owns bounded query children.

use rift_search::query::FactRequirement;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use rift_protocol::query::QueryAnswer;
use rift_search::sparql::{QueryFacts, QueryLimits, QueryServices};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn program() -> PathBuf {
    std::env::var_os("CARGO_BIN_EXE_rift")
        .expect("test runner must provide CARGO_BIN_EXE_rift")
        .into()
}

fn limits() -> QueryLimits {
    QueryLimits {
        query_bytes_max: 16_384,
        algebra_nodes_max: 4096,
        quads_max: 100_000,
        rdf_bytes_max: 1_048_576,
        result_items_max: 20_000,
        result_bytes_max: 1_048_576,
    }
}

#[test]
fn packaged_child_parses_and_evaluates_complete_results() -> TestResult {
    let started = Instant::now();
    let program = program();
    let limits = limits();
    let deadline = Instant::now() + Duration::from_secs(10);
    let plan = rift_server::prepare_query(
        &program,
        "SELECT ?x { VALUES ?x { 1 1 2 } }".into(),
        limits,
        deadline,
        &|| false,
    )?;
    assert!(!plan.requirements().requires(FactRequirement::Domain));
    let answer = rift_server::evaluate_query(
        &program,
        plan,
        QueryFacts::default(),
        limits,
        QueryServices::default(),
        deadline,
        &|| false,
    )?;
    let QueryAnswer::Bindings {
        variables,
        bindings,
    } = answer
    else {
        return Err("SELECT must return bindings".into());
    };
    assert_eq!(variables, ["x"]);
    assert_eq!(bindings.len(), 3);
    assert_eq!(bindings[0], bindings[1]);
    eprintln!(
        "query child prepare and evaluate elapsed_ms={}",
        started.elapsed().as_millis()
    );
    Ok(())
}

#[test]
fn packaged_child_keeps_invalid_and_result_limit_refusals_typed() -> TestResult {
    let program = program();
    let deadline = Instant::now() + Duration::from_secs(10);
    let invalid = rift_server::prepare_query(
        &program,
        "SELECT WHERE {".into(),
        limits(),
        deadline,
        &|| false,
    )
    .expect_err("invalid statement");
    assert_eq!(invalid.slug().as_str(), "rift.server.read_invalid");
    let limits = QueryLimits {
        result_items_max: 1,
        ..limits()
    };
    let plan = rift_server::prepare_query(
        &program,
        "SELECT ?x { VALUES ?x { 1 2 } }".into(),
        limits,
        deadline,
        &|| false,
    )?;
    let refusal = rift_server::evaluate_query(
        &program,
        plan,
        QueryFacts::default(),
        limits,
        QueryServices::default(),
        deadline,
        &|| false,
    )
    .expect_err("result admission refuses the complete query");
    assert_eq!(refusal.slug().as_str(), "rift.server.read_unavailable");
    assert!(refusal.to_string().contains("result_items_max"));
    Ok(())
}

#[test]
fn packaged_child_cancellation_discards_the_answer() -> TestResult {
    let checks = AtomicUsize::new(0);
    let cancelled = || checks.fetch_add(1, Ordering::Relaxed) >= 3;
    let error = rift_server::prepare_query(
        &program(),
        "ASK {}".into(),
        limits(),
        Instant::now() + Duration::from_secs(10),
        &cancelled,
    )
    .expect_err("cancellation cannot return a successful answer");
    assert_eq!(error.slug().as_str(), "rift.server.read_cancelled");
    assert!(checks.load(Ordering::Relaxed) >= 4);
    let plan = rift_server::prepare_query(
        &program(),
        "ASK {}".into(),
        limits(),
        Instant::now() + Duration::from_secs(10),
        &|| false,
    )?;
    assert_eq!(plan.statement(), "ASK {}");
    Ok(())
}

#[test]
fn expired_query_deadline_refuses_before_child_startup() {
    let error = rift_server::prepare_query(
        &program(),
        "ASK {}".into(),
        limits(),
        Instant::now(),
        &|| false,
    )
    .expect_err("process startup and work share the request deadline");
    assert_eq!(error.slug().as_str(), "rift.server.read_unavailable");
    assert!(
        error
            .to_string()
            .contains("query exceeded the request deadline")
    );
}

#[tokio::test]
async fn packaged_child_refuses_an_unavailable_startup_heap_budget() -> TestResult {
    let (status, diagnostic, response, acknowledgment) =
        run_child_request(Some(serde_json::json!({})), 0).await?;
    assert!(!status.success());
    assert_eq!(
        std::str::from_utf8(&diagnostic)?.trim(),
        "rift: query child heap limit is below its initial allocations"
    );
    assert!(response.is_empty());
    assert!(acknowledgment.is_empty());
    Ok(())
}

#[tokio::test]
async fn packaged_child_refuses_unversioned_and_incompatible_requests() -> TestResult {
    let task = serde_json::json!({"Prepare":{"statement":"ASK {}","limits":limits()}});
    let heap_bytes_max = rift_server::QueryProcessLimits::from_query(limits())?.heap_bytes_max;
    for request in [
        task.clone(),
        serde_json::json!({"version":3,"payload":task.clone()}),
        serde_json::json!({"version":0,"payload":task}),
    ] {
        let (status, diagnostic, response, acknowledgment) =
            run_child_request(Some(request), heap_bytes_max).await?;
        assert!(status.success());
        assert!(diagnostic.is_empty());
        let refusal: serde_json::Value = serde_json::from_slice(&response)?;
        assert!(refusal["payload"]["Err"]["Unsupported"]["capability"].is_string());
        assert!(refusal["payload"].get("Ok").is_none());
        let acknowledgment: serde_json::Value = serde_json::from_slice(&acknowledgment)?;
        assert!(
            acknowledgment["version"]
                .as_u64()
                .is_some_and(|version| version > 0)
        );
        assert!(acknowledgment["payload"].is_null());
    }
    Ok(())
}

#[tokio::test]
async fn packaged_compatible_child_reports_an_observed_input_failure_without_an_answer()
-> TestResult {
    let heap_bytes_max = rift_server::QueryProcessLimits::from_query(limits())?.heap_bytes_max;
    let (status, diagnostic, response, acknowledgment) =
        run_child_request(None, heap_bytes_max).await?;
    assert!(!status.success());
    assert!(!diagnostic.is_empty());
    assert!(response.is_empty());
    let acknowledgment: serde_json::Value = serde_json::from_slice(&acknowledgment)?;
    assert!(
        acknowledgment["version"]
            .as_u64()
            .is_some_and(|version| version > 0)
    );
    assert!(acknowledgment["payload"].is_null());
    Ok(())
}

async fn run_child_request(
    value: Option<serde_json::Value>,
    heap_bytes_max: usize,
) -> TestResult<(std::process::ExitStatus, Vec<u8>, Vec<u8>, Vec<u8>)> {
    use tokio::io::AsyncReadExt as _;

    let directory = tempfile::tempdir()?;
    let request = directory.path().join("request.json");
    let response = directory.path().join("response.json");
    if let Some(value) = value {
        std::fs::write(&request, serde_json::to_vec(&value)?)?;
    }
    std::fs::write(&response, b"")?;
    let mut child = tokio::process::Command::new(program())
        .arg(rift_server::QUERY_CHILD_COMMAND)
        .arg(&request)
        .arg(&response)
        .arg("8192")
        .arg(heap_bytes_max.to_string())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stderr = child.stderr.take().ok_or("child stderr")?.take(8193);
    let mut stdout = child.stdout.take().ok_or("child stdout")?.take(8193);
    let mut diagnostic = Vec::new();
    let mut acknowledgment = Vec::new();
    let completed = async {
        let (_, _, status) = tokio::try_join!(
            stderr.read_to_end(&mut diagnostic),
            stdout.read_to_end(&mut acknowledgment),
            child.wait()
        )?;
        Ok::<_, std::io::Error>(status)
    };
    let status = match tokio::time::timeout(Duration::from_secs(10), completed).await {
        Ok(status) => status?,
        Err(error) => {
            child.kill().await?;
            return Err(error.into());
        }
    };
    Ok((
        status,
        diagnostic,
        std::fs::read(&response)?,
        acknowledgment,
    ))
}
