//! Edge operations over the framed in-process engine transport.

use super::{join, path, started};
use lsp_types::{Position, TypeHierarchyItem};
use serde_json::json;

#[tokio::test]
async fn test_edge_unadvertised_methods_send_no_request() {
    let (_workspace, mut session, engine) = started(|mut engine| async move {
        engine.handshake(json!({})).await;
        let (id, _) = engine.expect_request("shutdown").await;
        engine.respond(&id, serde_json::Value::Null).await;
        let message = engine.next_message().await;
        assert_eq!(message["method"], "exit");
    })
    .await;
    let path = path("entry.ts");
    assert!(
        session
            .implementation_edges(&path, Position::new(0, 0))
            .await
            .is_err()
    );
    assert!(
        session
            .prepare_type_edges(&path, Position::new(0, 0))
            .await
            .is_err()
    );
    let _stderr = session.shutdown().await;
    join(engine).await;
}

#[tokio::test]
async fn test_edge_advertised_operations_preserve_locations_and_hierarchy_items() {
    let (_workspace, mut session, engine) = started(|mut engine| async move {
        engine.handshake(json!({"implementationProvider":true,"typeHierarchyProvider":{}})).await;
        let (id, _) = engine.expect_request("textDocument/implementation").await;
        let range = json!({"start":{"line":0,"character":0},"end":{"line":0,"character":4}});
        let uri = "file:///workspace/entry.ts";
        engine.respond(&id, json!([{"targetUri":uri,"targetRange":range,"targetSelectionRange":range}])).await;
        let (id, _) = engine.expect_request("textDocument/prepareTypeHierarchy").await;
        let item = json!({"name":"Base","kind":5,"uri":uri,"range":range,"selectionRange":range,"data":{"id":3}});
        engine.respond(&id, json!([item])).await;
        for method in ["typeHierarchy/supertypes", "typeHierarchy/subtypes"] {
            let (id, params) = engine.expect_request(method).await;
            assert_eq!(params["item"], item);
            engine.respond(&id, json!([item])).await;
        }
        let (id, _) = engine.expect_request("shutdown").await;
        engine.respond(&id, serde_json::Value::Null).await;
        assert_eq!(engine.next_message().await["method"], "exit");
    }).await;
    let path = path("entry.ts");
    let locations = session
        .implementation_edges(&path, Position::new(0, 0))
        .await
        .expect("implementation edges");
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].range.end.character, 4);
    let items = session
        .prepare_type_edges(&path, Position::new(0, 0))
        .await
        .expect("type edges");
    let item: TypeHierarchyItem = items[0].clone();
    assert_eq!(
        session
            .supertype_edges(item.clone())
            .await
            .expect("supertypes"),
        items
    );
    assert_eq!(session.subtype_edges(item).await.expect("subtypes"), items);
    let _stderr = session.shutdown().await;
    join(engine).await;
}
