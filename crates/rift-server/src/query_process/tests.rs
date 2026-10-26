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
        input_bytes_max: None,
        output_bytes_max: None,
        material_bytes_max: None,
        heap_bytes_max: None,
        diagnostic_bytes_max: None,
    }
}

#[test]
fn process_byte_limits_are_finite_and_refuse_arithmetic_overflow() {
    let limits = limits();
    let bound = QueryProcessLimits::from_query(limits).expect("accepted bounds");
    assert_eq!(
        bound.input_bytes_max,
        4 * (limits.query_bytes_max + limits.rdf_bytes_max + limits.result_bytes_max)
            + PROCESS_OVERHEAD_BYTES
    );
    assert_eq!(
        bound.heap_bytes_max,
        2 * (limits.query_bytes_max + limits.rdf_bytes_max + limits.result_bytes_max)
            + PROCESS_OVERHEAD_BYTES
    );
    assert_eq!(bound.output_bytes_max, bound.input_bytes_max);
    assert_eq!(bound.material_bytes_max, bound.input_bytes_max);
    assert_eq!(bound.diagnostic_bytes_max, 8192);
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

#[test]
fn explicit_process_bounds_are_independent_and_replace_only_their_derivation() {
    let automatic = QueryProcessLimits::from_query(limits()).expect("automatic bounds");
    let accepted = QueryLimits {
        input_bytes_max: Some(101),
        output_bytes_max: Some(102),
        material_bytes_max: Some(103),
        heap_bytes_max: Some(104),
        diagnostic_bytes_max: Some(1),
        ..limits()
    };
    let resolved = QueryProcessLimits::from_query(accepted).expect("explicit bounds");
    assert_eq!(resolved.input_bytes_max, 101);
    assert_eq!(resolved.output_bytes_max, 102);
    assert_eq!(resolved.material_bytes_max, 103);
    assert_eq!(resolved.heap_bytes_max, 104);
    assert_eq!(resolved.diagnostic_bytes_max, 1);
    let independent = QueryProcessLimits::from_query(QueryLimits {
        input_bytes_max: Some(1),
        ..limits()
    })
    .expect("one override");
    assert_eq!(independent.output_bytes_max, automatic.output_bytes_max);
    assert_eq!(independent.material_bytes_max, automatic.material_bytes_max);
    assert_eq!(independent.heap_bytes_max, automatic.heap_bytes_max);
    assert!(
        QueryProcessLimits::from_query(QueryLimits {
            query_bytes_max: usize::MAX,
            ..accepted
        })
        .is_ok()
    );
    assert!(
        QueryProcessLimits::from_query(QueryLimits {
            input_bytes_max: Some(0),
            ..accepted
        })
        .is_err()
    );
}

#[test]
fn material_override_bounds_shared_record_admission() {
    let mut budget = QueryFactBudget::new(
        QueryLimits {
            material_bytes_max: Some(3),
            ..limits()
        },
        &|| false,
    )
    .expect("configured material admission");
    budget.charge_bytes(b"abc").expect("exact material bound");
    assert!(budget.charge_bytes(b"d").is_err());
}

#[test]
fn acknowledgment_frame_bound_covers_every_version_and_rejects_extra_bytes() {
    let acknowledgment = QueryExchange {
        version: u32::MAX,
        payload: (),
    };
    let bytes = serde_json::to_vec(&acknowledgment).expect("fixed acknowledgment");
    assert_eq!(bytes.len(), ACKNOWLEDGMENT_BYTES_MAX);
    let mut value = serde_json::to_value(limits()).expect("current limits");
    value
        .as_object_mut()
        .expect("limits object")
        .remove("input_bytes_max");
    assert!(serde_json::from_value::<QueryLimits>(value).is_err());
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
    budget
        .state
        .lock()
        .expect("record admission")
        .writer
        .remaining = 9;
    budget
        .charge(&"abc")
        .expect("five encoded bytes and separator");
    budget.charge_bytes(b"xyz").expect("exact remaining bytes");
    let error = budget.charge(&0).expect_err("complete record cannot fit");
    assert_eq!(error.slug().as_str(), "rift.server.read_unavailable");
    let mut shared = budget.with_cancellation(&|| false);
    let error = shared
        .charge(&0)
        .expect_err("the same failure remains retained");
    assert_eq!(error.slug().as_str(), "rift.server.read_unavailable");
    let mut budget = QueryFactBudget::new(limits(), &check).expect("record admission");
    let mut shared = budget.with_cancellation(&|| false);
    cancelled.store(true, Ordering::Relaxed);
    let error = budget.charge(&0).expect_err("cancelled record");
    assert_eq!(error.slug().as_str(), "rift.server.read_cancelled");
    let error = shared
        .charge_bytes(b"")
        .expect_err("another handle retains cancellation");
    assert_eq!(error.slug().as_str(), "rift.server.read_cancelled");
}

#[test]
fn producer_pages_share_encoded_admission_with_owned_extensions() {
    let page: rift_protocol::read::SearchResult = serde_json::from_value(serde_json::json!({
        "results":[{"hit":{"target":"symbol","symbol":{
            "language":"rust","kind":"function","name":"beacon",
            "extensions":{"example.org.fact":{"version":1,"data":{"nested":{"text":"value"}}}}
        }}}],
        "pagination":{"page_index":0,"total_pages":1}
    }))
    .expect("ordinary Rust producer page");
    let encoded = serde_json::to_vec(&page).expect("complete producer page");
    let budget = QueryFactBudget::new(limits(), &|| false).expect("record admission");
    budget
        .state
        .lock()
        .expect("record admission")
        .writer
        .remaining = (encoded.len() + 1) * 2 - 1;
    let mut first = budget.with_cancellation(&|| false);
    let mut second = budget.with_cancellation(&|| false);
    first.charge(&page).expect("one complete page fits");
    let failure = second
        .charge(&page)
        .expect_err("the cumulative pages do not fit");
    assert_eq!(failure.slug().as_str(), "rift.server.read_unavailable");
    assert_eq!(
        first
            .check()
            .expect_err("the first failure stays shared")
            .to_string(),
        failure.to_string()
    );
    let cancelled = budget.with_cancellation(&|| true);
    assert_eq!(
        cancelled
            .check()
            .expect_err("later cancellation does not replace the failure")
            .to_string(),
        failure.to_string()
    );
}

#[test]
fn shared_producers_keep_remaining_material_and_the_original_deadline() {
    let budget = QueryFactBudget::new(limits(), &|| false).expect("record admission");
    budget
        .state
        .lock()
        .expect("record admission")
        .writer
        .remaining = 12;
    let mut first = budget.with_cancellation(&|| false);
    let mut second = budget.with_cancellation(&|| false);
    first.charge(&"abc").expect("first producer");
    second.charge_material(6).expect("second producer material");
    assert!(first.charge_bytes(b"x").is_err());
    assert!(second.check().is_err());

    let mut expired = QueryFactBudget::for_request(limits(), Instant::now(), &|| false)
        .expect("request admission");
    let mut other = expired.with_cancellation(&|| false);
    let error = expired.charge_material(0).expect_err("expired request");
    assert_eq!(error.slug().as_str(), "rift.server.read_unavailable");
    assert_eq!(
        other
            .charge_bytes(b"")
            .expect_err("same deadline")
            .to_string(),
        error.to_string()
    );
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
    let failure = QueryFailure::capture(&invalid);
    let bytes = serde_json::to_vec(&failure).expect("encoded refusal");
    let failure: QueryFailure = serde_json::from_slice(&bytes).expect("typed refusal");
    assert_eq!(failure.into_error().to_string(), original);
}

#[test]
fn registered_refusals_preserve_identity() {
    for original in [
        unavailable("query request exceeds its byte limit"),
        errors::server::read_cancelled().error(),
        incompatible("query exchange version is incompatible"),
    ] {
        let slug = original.slug();
        let message = original.to_string();
        let captured = QueryFailure::capture(&original).into_error();
        assert_eq!(captured.slug(), slug);
        assert_eq!(captured.to_string(), message);
    }
}

#[test]
fn parent_refuses_unversioned_incompatible_and_unknown_response_fields() {
    let answer = Ok::<_, QueryFailure>(QueryOutput::Answer(QueryAnswer::Boolean { value: true }));
    let unversioned = serde_json::to_vec(&answer).expect("old response format");
    assert!(decode_response(&unversioned).is_err());
    let mut envelope = QueryExchange::new(answer);
    for version in [0, 8] {
        envelope.version = version;
        let error = decode_response(&serde_json::to_vec(&envelope).expect("incompatible response"))
            .err()
            .expect("incompatible response cannot become an answer");
        assert_eq!(error.slug().as_str(), "rift.server.read_unsupported");
    }
    envelope.version = QUERY_EXCHANGE_VERSION;
    let mut value = serde_json::to_value(&envelope).expect("response object");
    value["additional"] = serde_json::Value::Bool(true);
    assert!(decode_response(&serde_json::to_vec(&value).expect("unknown field")).is_err());
    let bytes = serde_json::to_vec(&envelope).expect("compatible response");
    assert!(matches!(
        decode_response(&bytes),
        Ok(QueryOutput::Answer(QueryAnswer::Boolean { value: true }))
    ));
}

#[test]
fn child_refuses_old_tasks_and_incompatible_versions_before_execution() {
    let directory = tempfile::tempdir().expect("exchange directory");
    let request = directory.path().join("request.json");
    let response = directory.path().join("response.json");
    let task = QueryTask::Prepare {
        statement: "ASK {}".to_owned(),
        limits: limits(),
    };
    std::fs::write(&response, b"").expect("response file");
    std::fs::write(&request, serde_json::to_vec(&task).expect("old task")).expect("request file");
    run_query_child(&request, &response, 8192, 8192, 8192).expect("typed schema refusal");
    let error = decode_response(&std::fs::read(&response).expect("complete refusal"))
        .err()
        .expect("old task cannot produce an answer");
    assert_eq!(error.slug().as_str(), "rift.server.read_unsupported");
    let mut envelope = QueryExchange::new(task);
    for version in [0, 8] {
        envelope.version = version;
        std::fs::write(&request, serde_json::to_vec(&envelope).expect("new task"))
            .expect("request file");
        run_query_child(&request, &response, 8192, 8192, 8192).expect("typed version refusal");
        let error = decode_response(&std::fs::read(&response).expect("complete refusal"))
            .err()
            .expect("incompatible version cannot produce an answer");
        assert_eq!(error.slug().as_str(), "rift.server.read_unsupported");
    }
}

#[test]
fn acknowledgment_distinguishes_protocol_refusal_from_an_observed_child_failure() {
    use crate::process::fixture::{FixtureMode, fixture_command};

    for (acknowledged, slug) in [
        (false, "rift.server.read_unsupported"),
        (true, "rift.server.read_unavailable"),
    ] {
        let mut run = crate::process::run_bounded(
            &mut fixture_command(FixtureMode::Print),
            std::time::Duration::from_secs(10),
            DIAGNOSTIC_BYTES_DEFAULT,
            &|| false,
        )
        .expect("owned exit-status fixture");
        assert!(!run.exit.as_ref().expect("observed child exit").success());
        if acknowledged {
            run.stdout.text =
                serde_json::to_string(&QueryExchange::new(())).expect("acknowledgment");
        }
        let response = tempfile::NamedTempFile::new().expect("response file");
        let error = collect_response(
            run,
            &response,
            8192,
            Instant::now() + std::time::Duration::from_secs(10),
            &|| false,
        )
        .err()
        .expect("failed child never returns an answer");
        assert_eq!(error.slug().as_str(), slug);
    }
    let mut incompatible = QueryExchange::new(());
    for version in [0, 8] {
        incompatible.version = version;
        let error =
            decode_acknowledgment(&serde_json::to_vec(&incompatible).expect("version marker"))
                .expect_err("version mismatch");
        assert_eq!(error.slug().as_str(), "rift.server.read_unsupported");
    }
}

#[test]
fn current_exchange_requires_version_and_payload_and_emits_the_current_acknowledgment() {
    let acknowledgment =
        serde_json::to_value(QueryExchange::new(())).expect("current acknowledgment");
    assert_eq!(acknowledgment["version"], 9);
    assert!(decode_acknowledgment(&serde_json::to_vec(&acknowledgment).expect("marker")).is_ok());
    for field in ["version", "payload"] {
        let mut missing = acknowledgment.clone();
        missing
            .as_object_mut()
            .expect("exchange object")
            .remove(field);
        assert!(serde_json::from_value::<QueryExchange<()>>(missing).is_err());
    }
}

#[test]
fn current_plan_exchange_requires_present_nullable_retained_subject_selection() {
    let output = run_task(QueryTask::Prepare {
        statement: "ASK {}".to_owned(),
        limits: limits(),
    })
    .expect("current child plan");
    let mut envelope = serde_json::to_value(QueryExchange::new(Ok::<_, QueryFailure>(output)))
        .expect("current plan exchange");
    assert_eq!(envelope["version"], 9);
    assert!(
        envelope["payload"]["Ok"]["Plan"]
            .get("retained_subject_selection")
            .is_some_and(serde_json::Value::is_null)
    );
    let decoded = decode_response(&serde_json::to_vec(&envelope).expect("current plan frame"))
        .expect("explicit null proof is a valid current plan");
    let QueryOutput::Plan(plan) = decoded else {
        panic!("current preparation returns a plan");
    };
    assert_eq!(plan.statement(), "ASK {}");
    assert!(plan.retained_subject_selection().is_none());
    let mut absent = envelope.clone();
    absent["payload"]["Ok"]["Plan"]
        .as_object_mut()
        .expect("encoded plan object")
        .remove("retained_subject_selection");
    assert!(decode_response(&serde_json::to_vec(&absent).expect("old plan shape")).is_err());
    envelope["version"] = serde_json::json!(8);
    let refused = decode_response(&serde_json::to_vec(&envelope).expect("old plan exchange"))
        .err()
        .expect("previous query exchange refuses current plan payload");
    assert_eq!(refused.slug().as_str(), "rift.server.read_unsupported");
}
