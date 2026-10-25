use super::*;
use crate::output::OutputPolicy;
use crate::proxy::{Upstream, extra};
use rift_protocol::configuration::WorkspaceConfiguration;
use rift_protocol::query::{QueryAnswer, QueryReadResult, RdfTerm};
use rift_protocol::source_read::GetSourceResult;
use rmcp::ServiceExt as _;
use rmcp::service::{RoleServer, RunningService};
use serde_json::json;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

async fn attach(
    proxy: &RiftProxy,
    key: &str,
) -> TestResult<RunningService<RoleServer, crate::server::RiftMcp>> {
    let backend = if key == "local" {
        proxy
    } else {
        proxy.backends.get(key).ok_or("accepted profile")?
    };
    crate::server::hermetic_workspace(&backend.root, "")?;
    let server = crate::server::RiftMcp::build_settled(
        &backend.root,
        rift_index::WorkspaceIndexLimits::default(),
    )
    .await?;
    let (client, transport) = tokio::io::duplex(64 * 1024);
    let (upstream, serving) = tokio::join!(().serve(client), server.serve(transport));
    let mut slot = backend.upstream.lock().await;
    slot.connected = Some(Upstream {
        running: upstream?,
        generation: 0,
    });
    slot.generation_next = 1;
    Ok(serving?)
}

fn request(query: &str) -> TestResult<CallToolRequestParams> {
    Ok(CallToolRequestParams::new("search")
        .with_arguments(serde_json::from_value(json!({"query":query}))?))
}

fn decoded(response: CallToolResponse) -> TestResult<rift_protocol::query::QueryResult> {
    let CallToolResponse::Complete(result) = response else {
        return Err("complete query result".into());
    };
    assert_ne!(result.is_error, Some(true), "{result:?}");
    match serde_json::from_value(result.structured_content.ok_or("structured query result")?)? {
        QueryReadResult::Results(result) => Ok(*result),
        other @ QueryReadResult::Unavailable(_) => {
            Err(format!("complete query result: {other:?}").into())
        }
    }
}

fn assert_count(answer: &QueryAnswer) -> TestResult {
    let QueryAnswer::Bindings { bindings, .. } = answer else {
        return Err("aggregate bindings".into());
    };
    assert_eq!(
        bindings[0]["count"],
        RdfTerm::Literal {
            value: "4".to_owned(),
            datatype: "http://www.w3.org/2001/XMLSchema#integer".to_owned()
        }
    );
    Ok(())
}

fn assert_original_symbols(
    proxy: &RiftProxy,
    view: &rift_protocol::symbol_read::CapturedView,
) -> TestResult {
    let captured = proxy
        .query_views
        .get()
        .ok_or("retained material owner")?
        .query_view(
            &view.id,
            proxy
                .accepted_root
                .as_deref()
                .ok_or("accepted primary root")?,
            &proxy.owner,
        )
        .map_err(|reason| format!("retained query view: {reason:?}"))?;
    let originals: Vec<(
        String,
        crate::server::QueryFactSelection,
        rift_search::sparql::QueryIdentities,
    )> = serde_json::from_slice(captured.material().facts())?;
    let mut names = std::collections::BTreeSet::new();
    for (backend, selected, _) in &originals {
        for service in &selected.services.retrievals {
            for hit in &service.result.results {
                let rift_protocol::read::SearchHitTarget::Symbol { symbol } = &hit.hit else {
                    return Err("selected symbol result".into());
                };
                names.insert((backend.as_str(), symbol.name.as_str()));
                assert!(
                    symbol.id.is_none(),
                    "original fixture has no canonical symbol identity"
                );
            }
        }
    }
    assert_eq!(
        names,
        std::collections::BTreeSet::from([
            ("#a", "shared_alpha"),
            ("#a", "shared_one"),
            ("#b", "shared_beta"),
            ("#b", "shared_two"),
        ])
    );
    Ok(())
}

#[tokio::test]
async fn two_profiles_evaluate_once_and_retain_pages_and_original_sources() -> TestResult {
    let root = tempfile::tempdir()?;
    for (name, text) in [
        ("a", "pub fn shared_alpha() {}\npub fn shared_one() {}\n"),
        ("b", "pub fn shared_beta() {}\npub fn shared_two() {}\n"),
    ] {
        std::fs::create_dir(root.path().join(name))?;
        std::fs::write(root.path().join(name).join("lib.rs"), text)?;
    }
    let configuration: WorkspaceConfiguration =
        serde_json::from_value(json!({"profiles":{"a":{"root":"a"},"b":{"root":"b"}}}))?;
    let registrations = extra::Registrations::accept(root.path(), &configuration.mcp, &[], None)?
        .with_profiles(root.path(), &configuration)?;
    let mut proxy = RiftProxy::with_configuration(
        root.path(),
        crate::proxy::tests::test_identity(),
        OutputPolicy::All,
        &configuration.server,
        registrations,
    );
    proxy.query_configuration.page_size = 1;
    let a = attach(&proxy, "#a").await?;
    let b = attach(&proxy, "#b").await?;
    let local = attach(&proxy, "local").await?;
    let query = "SELECT ?a ?b ?unitA ?unitB ?nameA ?nameB { SERVICE :search { ?ha :pattern 'shared'; :scope 'local#a'; :target 'symbol'; :symbol ?a; :unit ?unitA } SERVICE :search { ?hb :pattern 'shared'; :scope 'local#b'; :target 'symbol'; :symbol ?b; :unit ?unitB } GRAPH <urn:rift:graph:extra:#a> { ?domainA :name ?nameA; :declarations ?declarationA. ?declarationA :unit ?unitA } GRAPH <urn:rift:graph:extra:#b> { ?domainB :name ?nameB; :declarations ?declarationB. ?declarationB :unit ?unitB } FILTER(sameTerm(?a, ?domainA) && sameTerm(?b, ?domainB)) } ORDER BY ?nameA ?nameB";
    let count = "SELECT (COUNT(*) AS ?count) { SERVICE :search { ?ha :pattern 'shared'; :scope 'local#a'; :target 'symbol'; :symbol ?a } SERVICE :search { ?hb :pattern 'shared'; :scope 'local#b'; :target 'symbol'; :symbol ?b } }";
    assert_count(&decoded(proxy.query_forward(&request(count)?).await?.response?)?.answer)?;
    let first = decoded(proxy.query_forward(&request(query)?).await?.response?)?;
    assert_eq!(first.pagination.page.total_pages, 4);
    let QueryAnswer::Bindings { bindings, .. } = &first.answer else {
        return Err("binding table".into());
    };
    assert_eq!(bindings.len(), 1);
    let view = first.view.clone().ok_or("retained query view")?;
    assert_original_symbols(&proxy, &view)?;
    for (variable, name_variable, name) in [
        ("a", "nameA", "shared_alpha"),
        ("b", "nameB", "shared_beta"),
    ] {
        assert!(
            matches!(&bindings[0][variable], RdfTerm::BlankNode { .. }),
            "original physical symbol"
        );
        let RdfTerm::Literal { value, .. } = &bindings[0][name_variable] else {
            return Err("original selected symbol name".into());
        };
        assert_eq!(value, name);
    }
    let (RdfTerm::Iri { value: unit_a }, RdfTerm::Iri { value: unit_b }) =
        (&bindings[0]["unitA"], &bindings[0]["unitB"])
    else {
        return Err("source identities".into());
    };
    assert_ne!(unit_a, unit_b);
    assert_ne!(unit_a, "rift://source/project/lib.rs");
    let next = first
        .pagination
        .next_query
        .clone()
        .ok_or("query continuation")?;
    let source = CallToolRequestParams::new("get_source").with_arguments(serde_json::from_value(
        json!({"unit":unit_a,"view":view.id}),
    )?);
    std::fs::write(root.path().join("a/lib.rs"), "pub fn changed() {}\n")?;
    let CallToolResponse::Complete(read) = proxy
        .query_material_read(&source)
        .await?
        .ok_or("held source")??
    else {
        return Err("source result".into());
    };
    let GetSourceResult::Found { source, .. } =
        serde_json::from_value(read.structured_content.ok_or("source bytes")?)?
    else {
        return Err("original source".into());
    };
    assert_eq!(
        source.text,
        "pub fn shared_alpha() {}\npub fn shared_one() {}\n"
    );
    let page = decoded(proxy.query_forward(&request(&next)?).await?.response?)?;
    assert_eq!(page.view, Some(view.clone()));
    assert_ne!(page.answer, first.answer);
    assert_eq!(
        decoded(proxy.query_forward(&request(&next)?).await?.response?)?.answer,
        page.answer
    );
    proxy.backends = std::sync::Arc::new(std::collections::BTreeMap::new());
    let error = proxy
        .query_forward(&request(&next)?)
        .await
        .err()
        .ok_or("removed profile refusal")?;
    assert_eq!(error.slug().as_str(), "rift.server.read_unavailable");
    a.cancel().await?;
    b.cancel().await?;
    local.cancel().await?;
    Ok(())
}
