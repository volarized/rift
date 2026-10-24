//! The packaged Rift executable owns bounded query children.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use rift_protocol::query::QueryAnswer;
use rift_search::sparql::{QueryFacts, QueryLimits, QueryServices};

type TestResult = Result<(), Box<dyn std::error::Error>>;

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
    assert!(!plan.requirements().domain);
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
    use tokio::io::AsyncReadExt as _;

    let directory = tempfile::tempdir()?;
    let request = directory.path().join("request.json");
    let response = directory.path().join("response.json");
    std::fs::write(&request, b"{}")?;
    std::fs::write(&response, b"")?;
    let mut child = tokio::process::Command::new(program())
        .arg(rift_server::QUERY_CHILD_COMMAND)
        .arg(&request)
        .arg(&response)
        .args(["16", "0"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stderr = child.stderr.take().ok_or("child stderr")?.take(8193);
    let mut diagnostic = Vec::new();
    let completed = async {
        let (_, status) = tokio::try_join!(stderr.read_to_end(&mut diagnostic), child.wait())?;
        Ok::<_, std::io::Error>(status)
    };
    let status = match tokio::time::timeout(Duration::from_secs(10), completed).await {
        Ok(status) => status?,
        Err(error) => {
            child.kill().await?;
            return Err(error.into());
        }
    };
    assert!(!status.success());
    assert_eq!(
        std::str::from_utf8(&diagnostic)?.trim(),
        "rift: query child heap limit is below its initial allocations"
    );
    assert!(std::fs::read(&response)?.is_empty());
    Ok(())
}
