//! Typed query exchange and admission checks.

use super::*;

fn limits() -> QueryLimits {
    QueryLimits {
        query_bytes_max: 16_384,
        algebra_nodes_max: 4096,
        quads_max: 100_000,
        rdf_bytes_max: 16_777_216,
        result_items_max: 20_000,
        result_bytes_max: 16_777_216,
    }
}

#[test]
fn process_byte_limits_are_finite_and_refuse_arithmetic_overflow() {
    let limits = limits();
    let bound = QueryProcessLimits::from_query(limits).expect("accepted bounds");
    assert_eq!(
        bound.bytes_max,
        4 * (limits.query_bytes_max + limits.rdf_bytes_max + limits.result_bytes_max)
            + PROCESS_OVERHEAD_BYTES
    );
    assert_eq!(
        bound.heap_bytes_max,
        2 * (limits.query_bytes_max + limits.rdf_bytes_max + limits.result_bytes_max)
            + PROCESS_OVERHEAD_BYTES
    );
    assert!(
        QueryProcessLimits::from_query(QueryLimits {
            query_bytes_max: usize::MAX,
            ..limits
        })
        .is_err()
    );
    assert!(
        QueryProcessLimits::from_query(QueryLimits {
            query_bytes_max: usize::MAX / 3,
            rdf_bytes_max: 0,
            result_bytes_max: 0,
            ..limits
        })
        .is_err()
    );
}

#[cfg(unix)]
#[test]
fn exchange_directory_is_restricted_to_its_owner() {
    use std::os::unix::fs::PermissionsExt as _;
    let directory = exchange_directory().expect("exchange directory");
    let permissions = directory
        .path()
        .metadata()
        .expect("directory metadata")
        .permissions();
    assert_eq!(permissions.mode() & 0o777, 0o700);
}

#[test]
fn request_read_counts_one_byte_past_the_ceiling() {
    assert_eq!(read_request(&b"abc"[..], 3).expect("exact bound"), b"abc");
    let error = read_request(&b"abcd-more"[..], 3).expect_err("one over bound");
    assert!(
        error
            .to_string()
            .contains("query request exceeds its byte limit")
    );
}

#[test]
fn json_write_refuses_before_writing_past_the_ceiling() {
    let mut output = Vec::new();
    write_json(&mut output, &"abc", 5).expect("exact JSON size");
    assert_eq!(output, br#""abc""#);
    let mut output = Vec::new();
    assert!(write_json(&mut output, &"abcd", 5).is_err());
    assert!(output.len() <= 5);
}

#[test]
fn request_serialization_stops_when_cancelled() {
    let mut output = Vec::new();
    let active = || false;
    let writer = RequestWriter {
        writer: &mut output,
        active: &active,
    };
    assert!(write_json(writer, &"abc", 16).is_err());
    assert!(output.is_empty());
}

#[test]
fn record_admission_counts_complete_encoded_data_and_checks_cancellation() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let cancelled = AtomicBool::new(false);
    let check = || cancelled.load(Ordering::Relaxed);
    let mut budget = QueryFactBudget::new(limits(), &check).expect("record admission");
    budget.writer.remaining = 9;
    budget
        .charge(&"abc")
        .expect("five encoded bytes and separator");
    budget.charge_bytes(b"xyz").expect("exact remaining bytes");
    let error = budget.charge(&0).expect_err("complete record cannot fit");
    assert_eq!(error.slug().as_str(), "rift.server.read_unavailable");
    cancelled.store(true, Ordering::Relaxed);
    let error = budget.charge(&0).expect_err("cancelled record");
    assert_eq!(error.slug().as_str(), "rift.server.read_cancelled");
    let error = budget
        .charge_bytes(b"")
        .expect_err("cancelled captured bytes");
    assert_eq!(error.slug().as_str(), "rift.server.read_cancelled");
}

#[test]
fn cancelled_or_expired_requests_do_not_start_a_child() {
    let missing = Path::new("rift-query-fixture-that-does-not-exist");
    let cancelled = prepare_query(
        missing,
        "ASK {}".into(),
        limits(),
        Instant::now() + std::time::Duration::from_secs(1),
        &|| true,
    )
    .expect_err("cancelled before process creation");
    assert_eq!(cancelled.slug().as_str(), "rift.server.read_cancelled");
    let expired = prepare_query(missing, "ASK {}".into(), limits(), Instant::now(), &|| {
        false
    })
    .expect_err("deadline passed before process creation");
    assert!(
        expired
            .to_string()
            .contains("query exceeded the request deadline")
    );
    let missing = prepare_query(
        missing,
        "ASK {}".into(),
        limits(),
        Instant::now() + std::time::Duration::from_secs(5),
        &|| false,
    )
    .expect_err("missing worker must refuse");
    assert_eq!(missing.slug().as_str(), "rift.server.read_unavailable");
}

#[test]
fn typed_plan_round_trip_preserves_page_metadata_and_original_statement() {
    let statement = "SELECT ?x { SERVICE <urn:rift:result:captured?page=2> {} }";
    let output = run_task(QueryTask::Prepare {
        statement: statement.to_owned(),
        limits: limits(),
    })
    .expect("plan");
    let bytes = serde_json::to_vec(&output).expect("encoded plan");
    let output: QueryOutput = serde_json::from_slice(&bytes).expect("typed plan");
    let QueryOutput::Plan(plan) = output else {
        panic!("preparation must return a plan");
    };
    assert_eq!(plan.statement(), statement);
    assert_eq!(plan.page_selection().expect("exact page").page, 2);
    assert_eq!(plan.requirements().services.len(), 1);
}

#[test]
fn typed_evaluation_collects_lazy_bindings_and_preserves_invalid_refusals() {
    let limits = limits();
    let plan =
        QueryPlan::prepare("SELECT ?x { VALUES ?x { 1 1 2 } }".to_owned(), limits).expect("plan");
    let output = run_task(QueryTask::Evaluate {
        plan: Box::new(plan),
        facts: Box::default(),
        limits,
        services: Box::default(),
    })
    .expect("evaluated result");
    let QueryOutput::Answer(QueryAnswer::Bindings { bindings, .. }) = output else {
        panic!("SELECT must return bindings");
    };
    assert_eq!(bindings.len(), 3);
    assert_eq!(bindings[0], bindings[1]);
    let invalid = run_task(QueryTask::Prepare {
        statement: "SELECT WHERE {".to_owned(),
        limits,
    })
    .err()
    .expect("invalid statement");
    let original = invalid.to_string();
    let failure = QueryFailure::capture(invalid);
    let bytes = serde_json::to_vec(&failure).expect("encoded refusal");
    let failure: QueryFailure = serde_json::from_slice(&bytes).expect("typed refusal");
    assert_eq!(failure.into_error().to_string(), original);
}

#[test]
fn unavailable_and_cancelled_refusals_preserve_registered_identity() {
    for original in [
        unavailable("query request exceeds its byte limit"),
        errors::server::read_cancelled().error(),
    ] {
        let slug = original.slug();
        let message = original.to_string();
        let captured = QueryFailure::capture(original).into_error();
        assert_eq!(captured.slug(), slug);
        assert_eq!(captured.to_string(), message);
    }
}
