// Toasty emits positional helpers for the existing composite indexes.
#![allow(clippy::too_many_arguments)]

#[derive(Debug, toasty::Model)]
#[table = "collection_batch_v1"]
#[index(
    producer, category, unit, contract, revision, context, source, sequence
)]
#[unique(producer, category, unit, contract, revision, context, source, witness)]
pub(super) struct Checkpoint {
    #[key]
    #[auto]
    sequence: i64,
    producer: String,
    category: String,
    unit: String,
    contract: i64,
    revision: Vec<u8>,
    context: Vec<u8>,
    source: Vec<u8>,
    witness: Vec<u8>,
    dependencies: Vec<u8>,
    checksum: Vec<u8>,
    record: Vec<u8>,
}
