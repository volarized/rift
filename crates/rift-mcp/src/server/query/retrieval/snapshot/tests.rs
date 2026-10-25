use super::*;
use rift_search::query::{FactRequirement, FactRequirements};

#[tokio::test]
async fn pattern_pages_and_exact_source_keep_the_original_publication()
-> Result<(), Box<dyn std::error::Error>> {
    original_pages("pattern").await
}

#[tokio::test]
async fn query_pages_and_exact_source_keep_the_original_publication()
-> Result<(), Box<dyn std::error::Error>> {
    original_pages("query").await
}

async fn original_pages(selector: &str) -> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let original = "pub fn shared_alpha() {}\npub fn shared_beta() {}\n";
    std::fs::write(root.path().join("lib.rs"), original)?;
    crate::server::hermetic_workspace(root.path(), "")?;
    let server =
        RiftMcp::build_settled(root.path(), rift_index::WorkspaceIndexLimits::default()).await?;
    let mut selected = serde_json::json!({"target":"symbol", "scope":"local", "limit":1});
    selected[selector] = serde_json::json!("shared");
    let mut request: SearchParams = serde_json::from_value(selected)?;
    let mut snapshot = server
        .retrieval_snapshot(&request)
        .await
        .map_err(|failure| format!("original pattern selection: {failure:?}"))?
        .ok_or("original pattern publication")?;
    let limits = rift_search::query::QueryLimits::try_from(
        &rift_protocol::query::QueryConfiguration::default(),
    )?;
    let deadline = server.request_deadline().await;
    let mut budget = QueryBudget::for_request(limits, deadline.at().into_std(), &|| false)?;
    let (first, first_contributions) = server
        .retrieval_snapshot_page(&mut snapshot, request.clone(), &budget, deadline)
        .await
        .map_err(|failure| format!("first original page: {failure:?}"))?;
    assert_eq!(first.results.len(), 1);
    assert_eq!(first.pagination.total_pages, 2);
    let view = first.view.clone().ok_or("issued original view")?;
    std::fs::write(root.path().join("lib.rs"), "pub fn changed() {}\n")?;
    request.page_index = 1;
    request.view = Some(view.id.clone());
    let (second, second_contributions) = server
        .retrieval_snapshot_page(&mut snapshot, request, &budget, deadline)
        .await
        .map_err(|failure| format!("second original page: {failure:?}"))?;
    assert_eq!(second.view, Some(view.clone()));
    assert_eq!(second.pagination.total_pages, 2);
    assert_eq!(second.results.len(), 1);
    assert_eq!(first_contributions.len(), first.results.len());
    assert_eq!(second_contributions.len(), second.results.len());
    assert!(!first_contributions[0].is_empty());
    assert!(!second_contributions[0].is_empty());
    assert_ne!(first_contributions, second_contributions);
    assert_ne!(first.results[0].range, second.results[0].range);
    let original_input = snapshot
        .resolved
        .published
        .reads
        .local_search_view_input(true)?;
    let selected = server
        .local_views
        .get()
        .ok_or("retained material owner")?
        .selected_input(&view.id, original_input.root(), original_input.owner())
        .map_err(|reason| format!("original view: {reason:?}"))?;
    let mut requirements = FactRequirements::default();
    requirements.require(FactRequirement::Domain);
    requirements.require(FactRequirement::Source);
    let facts = selected.query_facts_with_selection(
        &requirements,
        limits,
        &mut budget,
        selected
            .captured_provider_selection()
            .ok_or("original provider selection")?,
    )?;
    let source = facts
        .sources
        .iter()
        .find(|source| source.text.as_deref() == Some(original));
    assert!(source.is_some(), "exact original source bytes");
    assert_eq!(
        std::fs::read_to_string(root.path().join("lib.rs"))?,
        "pub fn changed() {}\n"
    );
    Ok(())
}
