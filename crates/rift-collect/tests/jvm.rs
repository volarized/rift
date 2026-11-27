//! Syntax collection through the shared collector contract.

#![cfg(feature = "runtime")]

use rift_collect::{
    Category, CollectionInput, CollectionSource, Collector, Coverage, Evidence, Inventory,
    JvmSyntaxCollector,
};
use rift_core::SourceUnitId;

#[tokio::test]
async fn jvm_syntax_records_valid_utf8_sites_from_parser_filenames() {
    let unit = SourceUnitId::parse("rift://source/project/Client.java").expect("fixture unit");
    let source = "public class Client { void run() { café(); } void café() {} }";
    let admitted = CollectionSource::new(
        unit.clone(),
        r"C:\repo\Client.java".to_owned(),
        source.into(),
    )
    .expect("fixture source");
    let mut inventory = Inventory::default();
    inventory.insert(admitted).expect("fixture inventory");
    let mut input = CollectionInput::new(&inventory, &unit).expect("fixture input");
    let batch = JvmSyntaxCollector
        .collect(&mut input, Category::Calls)
        .await
        .expect("syntax batch");
    input.validate(&batch).expect("admitted syntax batch");
    assert_eq!(batch.coverage, Coverage::Complete);
    let [call] = batch.observations.as_slice() else {
        panic!("expected one call site");
    };
    assert_eq!(call.evidence, Evidence::Syntax);
    assert!(call.target.is_none());
    let start = usize::try_from(call.occurrence.start).expect("fixture offset");
    let end = usize::try_from(call.occurrence.end).expect("fixture offset");
    assert_eq!(source.get(start..end), Some("café"));
}

#[tokio::test]
async fn unsupported_source_does_not_emit_observations() {
    let unit = SourceUnitId::parse("rift://source/project/Readme.md").expect("fixture unit");
    let admitted = CollectionSource::new(unit.clone(), "Readme.md".to_owned(), "# Readme".into())
        .expect("fixture source");
    let mut inventory = Inventory::default();
    inventory.insert(admitted).expect("fixture inventory");
    let mut input = CollectionInput::new(&inventory, &unit).expect("fixture input");
    let batch = JvmSyntaxCollector
        .collect(&mut input, Category::References)
        .await
        .expect("syntax batch");
    input.validate(&batch).expect("admitted syntax batch");
    assert_eq!(batch.coverage, Coverage::Unsupported);
    assert!(batch.observations.is_empty());
}
