//! Documentation metadata and reverse references committed with the lexical corpus.
//!
//! The metadata is stored per source: one row carries a source's facts, blocks, links,
//! references, and unresolved spellings, under the digest of those bytes, and one manifest
//! row carries what describes the whole collection. A write compares the digests it is
//! about to store with the ones the rows carry and rewrites only the sources that moved,
//! so an edit to one documented file writes that file's row, not the collection.

use std::collections::BTreeMap;

use rift_analysis::documentation::{DocumentationCollection, keyed_changes};
use rift_protocol::documentation::{
    DOCUMENTATION_SOURCES_MAX, DocumentationBlock, DocumentationContentIdentity,
    DocumentationCoverage, DocumentationDigest, DocumentationIndex, DocumentationLink,
    DocumentationReference, DocumentationReferenceCandidate, DocumentationSource,
    DocumentationWarning,
};
use rift_protocol::read::Digest;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use toasty::Executor;

use crate::lexical::{
    LexicalIndexError, LexicalIndexViolation, batch_limit_error, lexical_error_caused_by,
    storage_error,
};

/// Encoded documentation metadata one workspace publication stores, at most.
///
/// The fastapi corpus tree, whose documentation is translated into a dozen languages,
/// encodes 103 MB for 124,000 blocks; the bound holds it with room for the collection's
/// block bound to fill.
pub(crate) const METADATA_BYTES_MAX: usize = 256 * 1_024 * 1_024;
const MANIFEST_ID: i64 = 1;

/// What describes the whole collection: its revisions, its coverage, and its warnings.
#[derive(Debug, toasty::Model)]
#[table = "documentation_manifest"]
pub(crate) struct DocumentationManifestRecord {
    #[key]
    id: i64,
    payload: String,
}

/// One source's records, under the digest of the bytes they encode to.
#[derive(Debug, toasty::Model)]
#[table = "documentation_sources"]
pub(crate) struct DocumentationSourceRecord {
    #[key]
    identity: String,
    digest: Vec<u8>,
    payload: String,
}

/// One exact reference from a documentation block to a declaration, filed under the source
/// whose row holds it.
#[derive(Debug, toasty::Model)]
#[table = "documentation_references"]
pub(crate) struct DocumentationReferenceRecord {
    #[key]
    identity: String,
    #[index]
    source: String,
    target: String,
    block: String,
}

/// The collection-wide fields one manifest row carries, as written.
#[derive(Serialize)]
struct CollectionFields<'index> {
    documentation_revision: &'index Digest,
    selection_digest: &'index DocumentationDigest,
    coverage: &'index DocumentationCoverage,
    warnings: &'index [DocumentationWarning],
}

/// The collection-wide fields one manifest row carries, as read back.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCollectionFields {
    documentation_revision: Digest,
    selection_digest: DocumentationDigest,
    coverage: DocumentationCoverage,
    warnings: Vec<DocumentationWarning>,
}

/// One source's records, as written: every record filed under the source directly or
/// through the block it sits in.
#[derive(Serialize)]
struct SourceRecords<'index> {
    source: &'index DocumentationSource,
    blocks: Vec<&'index DocumentationBlock>,
    links: Vec<&'index DocumentationLink>,
    references: Vec<&'index DocumentationReference>,
    unresolved_references: Vec<&'index DocumentationReferenceCandidate>,
}

/// One source's records, as read back.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredSourceRecords {
    source: DocumentationSource,
    blocks: Vec<DocumentationBlock>,
    links: Vec<DocumentationLink>,
    references: Vec<DocumentationReference>,
    unresolved_references: Vec<DocumentationReferenceCandidate>,
}

/// One source's encoded row and the reverse references it files.
pub(crate) struct EncodedSource {
    identity: String,
    digest: Vec<u8>,
    payload: String,
    references: Vec<(String, String, String)>,
}

/// One collection's metadata, encoded per source.
pub(crate) struct EncodedDocumentation {
    manifest: String,
    sources: Vec<EncodedSource>,
}

impl EncodedDocumentation {
    /// Encodes one collection's metadata within `bytes_max` encoded bytes, counted over the
    /// manifest and every source row together.
    ///
    /// Encoding runs to the end even past the bound, keeping no source row beyond it, so a
    /// refusal reports the size the metadata measured.
    fn within(
        collection: &DocumentationCollection,
        bytes_max: usize,
    ) -> Result<Self, LexicalIndexError> {
        let index = collection.index();
        let manifest = serde_json::to_string(&CollectionFields {
            documentation_revision: &index.documentation_revision,
            selection_digest: &index.selection_digest,
            coverage: &index.coverage,
            warnings: &index.warnings,
        })
        .map_err(invalid_metadata)?;
        let mut written = manifest.len();
        let mut sources = Vec::with_capacity(index.sources.len());
        for records in source_records(index).into_values() {
            let encoded = encode_source(&records)?;
            written = written.saturating_add(encoded.payload.len());
            if written <= bytes_max {
                sources.push(encoded);
            }
        }
        if written > bytes_max {
            return Err(batch_limit_error(
                "documentation.bytes",
                written as u64,
                bytes_max as u64,
            ));
        }
        Ok(Self { manifest, sources })
    }
}

/// Every source's records, keyed by the row identity each source's row takes.
///
/// A block names its source, and a link, a reference, and an unresolved spelling name
/// their block, so every record reaches exactly one source. A collection validated them
/// all against its own blocks before this runs.
fn source_records(
    index: &DocumentationIndex,
) -> BTreeMap<&DocumentationContentIdentity, SourceRecords<'_>> {
    let mut grouped: BTreeMap<&DocumentationContentIdentity, SourceRecords<'_>> = index
        .sources
        .iter()
        .map(|source| {
            let records = SourceRecords {
                source,
                blocks: Vec::new(),
                links: Vec::new(),
                references: Vec::new(),
                unresolved_references: Vec::new(),
            };
            (&source.identity, records)
        })
        .collect();
    let owners: BTreeMap<&DocumentationDigest, &DocumentationContentIdentity> = index
        .blocks
        .iter()
        .map(|block| (&block.identity, &block.source))
        .collect();
    for block in &index.blocks {
        if let Some(records) = grouped.get_mut(&block.source) {
            records.blocks.push(block);
        }
    }
    for link in &index.links {
        if let Some(records) = owners
            .get(&link.block)
            .and_then(|owner| grouped.get_mut(owner))
        {
            records.links.push(link);
        }
    }
    for reference in &index.references {
        if let Some(records) = owners
            .get(&reference.block)
            .and_then(|owner| grouped.get_mut(owner))
        {
            records.references.push(reference);
        }
    }
    for candidate in &index.unresolved_references {
        if let Some(records) = owners
            .get(&candidate.block)
            .and_then(|owner| grouped.get_mut(owner))
        {
            records.unresolved_references.push(candidate);
        }
    }
    grouped
}

/// Encodes one source's records into its row and the reverse references it files.
fn encode_source(records: &SourceRecords<'_>) -> Result<EncodedSource, LexicalIndexError> {
    let identity = serde_json::to_string(&records.source.identity).map_err(invalid_metadata)?;
    let payload = serde_json::to_string(records).map_err(invalid_metadata)?;
    let digest = Sha256::digest(payload.as_bytes()).to_vec();
    let references = records
        .references
        .iter()
        .map(|reference| {
            (
                reference.identity.0.clone(),
                reference.target.0.clone(),
                reference.block.0.clone(),
            )
        })
        .collect();
    Ok(EncodedSource {
        identity,
        digest,
        payload,
        references,
    })
}

/// Encodes the metadata one commit stores, leaving it out when it crosses `bytes_max`.
///
/// Metadata past the bound never fails the commit it rides: the lexical documents commit
/// without it, the stored metadata is cleared instead of left describing an older tree,
/// and a warning names the size the encoding measured.
///
/// # Errors
///
/// Returns [`LexicalIndexError`] when the metadata cannot be encoded at all.
pub(crate) fn encode_within(
    documentation: Option<&DocumentationCollection>,
    bytes_max: usize,
) -> Result<Option<EncodedDocumentation>, LexicalIndexError> {
    let Some(collection) = documentation else {
        return Ok(None);
    };
    match EncodedDocumentation::within(collection, bytes_max) {
        Ok(encoded) => Ok(Some(encoded)),
        Err(error) if error.fault().violation() == LexicalIndexViolation::RecordLimit => {
            tracing::warn!(
                component = "search",
                operation = "search.commit",
                %error,
                "documentation metadata crossed its byte bound and was left out of the commit"
            );
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Replaces metadata using the lexical writer's existing transaction.
///
/// The digests the source rows carry are compared with the ones `encoded` is about to
/// store, through the same keyed comparison every documentation record family uses: a
/// source only the store holds is deleted, one only `encoded` holds is inserted, one whose
/// digest moved is replaced, and an unchanged source's row and references are not touched.
/// The manifest row, which carries the collection-wide fields alone, is always written.
/// No metadata clears every row.
pub(crate) async fn replace(
    executor: &mut dyn Executor,
    encoded: Option<&EncodedDocumentation>,
) -> Result<(), LexicalIndexError> {
    let Some(encoded) = encoded else {
        return clear(executor).await;
    };
    let recorded = recorded_sources(executor).await?;
    let current: BTreeMap<&String, &Vec<u8>> = encoded
        .sources
        .iter()
        .map(|source| (&source.identity, &source.digest))
        .collect();
    let stored: BTreeMap<&String, &Vec<u8>> = recorded.iter().collect();
    let changes = keyed_changes(&current, &stored);
    for identity in changes.removed.iter().chain(&changes.replaced) {
        delete_source(executor, identity).await?;
    }
    let by_identity: BTreeMap<&String, &EncodedSource> = encoded
        .sources
        .iter()
        .map(|source| (&source.identity, source))
        .collect();
    for identity in changes.added.iter().chain(&changes.replaced) {
        if let Some(source) = by_identity.get(identity) {
            insert_source(executor, source).await?;
        }
    }
    DocumentationManifestRecord::all()
        .delete()
        .exec(executor)
        .await
        .map_err(storage_error)?;
    toasty::create!(DocumentationManifestRecord {
        id: MANIFEST_ID,
        payload: encoded.manifest.clone()
    })
    .exec(executor)
    .await
    .map_err(storage_error)?;
    Ok(())
}

/// Deletes every documentation row.
async fn clear(executor: &mut dyn Executor) -> Result<(), LexicalIndexError> {
    DocumentationReferenceRecord::all()
        .delete()
        .exec(&mut *executor)
        .await
        .map_err(storage_error)?;
    DocumentationSourceRecord::all()
        .delete()
        .exec(&mut *executor)
        .await
        .map_err(storage_error)?;
    DocumentationManifestRecord::all()
        .delete()
        .exec(executor)
        .await
        .map_err(storage_error)?;
    Ok(())
}

/// The digest each stored source row carries, keyed by its identity.
async fn recorded_sources(
    executor: &mut dyn Executor,
) -> Result<BTreeMap<String, Vec<u8>>, LexicalIndexError> {
    let rows = stored_source_rows(executor).await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.identity, row.digest))
        .collect())
}

/// Every stored source row, refusing a store holding more than a collection may.
async fn stored_source_rows(
    executor: &mut dyn Executor,
) -> Result<Vec<DocumentationSourceRecord>, LexicalIndexError> {
    let bound = DOCUMENTATION_SOURCES_MAX as usize;
    let rows = DocumentationSourceRecord::all()
        .limit(bound + 1)
        .exec(executor)
        .await
        .map_err(storage_error)?;
    if rows.len() > bound {
        return Err(batch_limit_error(
            "documentation.sources",
            rows.len() as u64,
            u64::from(DOCUMENTATION_SOURCES_MAX),
        ));
    }
    Ok(rows)
}

/// Deletes one source's row and every reverse reference it filed.
async fn delete_source(
    executor: &mut dyn Executor,
    identity: &str,
) -> Result<(), LexicalIndexError> {
    DocumentationReferenceRecord::filter_by_source(identity)
        .delete()
        .exec(&mut *executor)
        .await
        .map_err(storage_error)?;
    DocumentationSourceRecord::filter_by_identity(identity)
        .delete()
        .exec(executor)
        .await
        .map_err(storage_error)?;
    Ok(())
}

/// Inserts one source's row and the reverse references it files.
async fn insert_source(
    executor: &mut dyn Executor,
    source: &EncodedSource,
) -> Result<(), LexicalIndexError> {
    toasty::create!(DocumentationSourceRecord {
        identity: source.identity.clone(),
        digest: source.digest.clone(),
        payload: source.payload.clone(),
    })
    .exec(&mut *executor)
    .await
    .map_err(storage_error)?;
    for (identity, target, block) in &source.references {
        toasty::create!(DocumentationReferenceRecord {
            identity: identity.clone(),
            source: source.identity.clone(),
            target: target.clone(),
            block: block.clone(),
        })
        .exec(&mut *executor)
        .await
        .map_err(storage_error)?;
    }
    Ok(())
}

/// Decodes and revalidates metadata within the caller's revision-scoped transaction.
///
/// The collection is assembled from the manifest row and every source row in source order,
/// which keeps sources and blocks in the order a collection holds them. References are
/// ordered again by the key a collection validates them under, and links and unresolved
/// spellings come back grouped by source.
pub(crate) async fn read(
    executor: &mut dyn Executor,
    bytes_max: usize,
) -> Result<Option<DocumentationCollection>, LexicalIndexError> {
    let record = DocumentationManifestRecord::filter_by_id(MANIFEST_ID)
        .first()
        .exec(&mut *executor)
        .await
        .map_err(storage_error)?;
    let Some(record) = record else {
        return Ok(None);
    };
    let rows = stored_source_rows(executor).await?;
    let measured = rows
        .iter()
        .map(|row| row.payload.len())
        .fold(record.payload.len(), usize::saturating_add);
    if measured > bytes_max {
        return Err(batch_limit_error(
            "documentation.bytes",
            measured as u64,
            bytes_max as u64,
        ));
    }
    let fields: StoredCollectionFields =
        serde_json::from_str(&record.payload).map_err(invalid_metadata)?;
    let mut sources = rows
        .iter()
        .map(|row| serde_json::from_str::<StoredSourceRecords>(&row.payload))
        .collect::<Result<Vec<_>, _>>()
        .map_err(invalid_metadata)?;
    sources.sort_by(|left, right| left.source.identity.cmp(&right.source.identity));
    DocumentationCollection::new(assembled(fields, sources))
        .map(Some)
        .map_err(invalid_metadata)
}

/// One collection's index from its collection-wide fields and its sources' records, in
/// source order.
fn assembled(
    fields: StoredCollectionFields,
    sources: Vec<StoredSourceRecords>,
) -> DocumentationIndex {
    let mut index = DocumentationIndex {
        documentation_revision: fields.documentation_revision,
        selection_digest: fields.selection_digest,
        sources: Vec::with_capacity(sources.len()),
        blocks: Vec::new(),
        links: Vec::new(),
        references: Vec::new(),
        unresolved_references: Vec::new(),
        coverage: fields.coverage,
        warnings: fields.warnings,
    };
    for records in sources {
        index.sources.push(records.source);
        index.blocks.extend(records.blocks);
        index.links.extend(records.links);
        index.references.extend(records.references);
        index
            .unresolved_references
            .extend(records.unresolved_references);
    }
    index.references.sort_by(|left, right| {
        (left.evidence, &left.block, left.range.start, &left.identity).cmp(&(
            right.evidence,
            &right.block,
            right.range.start,
            &right.identity,
        ))
    });
    index
}

fn invalid_metadata(error: impl std::error::Error + Send + Sync + 'static) -> LexicalIndexError {
    lexical_error_caused_by(LexicalIndexViolation::StoredKindInvalid, None, error)
}

#[cfg(test)]
mod tests {
    use super::{
        DocumentationCollection, DocumentationReferenceRecord, EncodedDocumentation,
        METADATA_BYTES_MAX, replace,
    };
    use crate::LexicalStamp;

    /// One reverse reference row: its identity, its source row, its target, and its block.
    type ReferenceRow = (String, String, String, String);

    /// The bytes one collection's manifest and source rows encode to together.
    fn encoded_bytes(encoded: &EncodedDocumentation) -> usize {
        encoded
            .sources
            .iter()
            .map(|source| source.payload.len())
            .fold(encoded.manifest.len(), usize::saturating_add)
    }

    /// Regression for #363: the refusal reports the encoded size it measured, not the
    /// bound plus one.
    #[test]
    fn metadata_encoding_past_the_bound_reports_the_measured_size() {
        let metadata = collection("Guide.\n");
        let measured = encoded_bytes(
            &EncodedDocumentation::within(&metadata, usize::MAX).expect("fixture metadata encodes"),
        );
        let Err(error) = EncodedDocumentation::within(&metadata, measured - 1) else {
            panic!("serialized metadata over bound must be refused");
        };
        assert_eq!(
            error.fault().violation(),
            crate::LexicalIndexViolation::RecordLimit
        );
        assert!(
            error.to_string().contains(&format!("observed {measured}")),
            "refusal must name the measured size {measured}: {error}"
        );
        assert!(EncodedDocumentation::within(&metadata, measured).is_ok());
    }

    /// Regression for #363: metadata past the bound leaves the commit's lexical documents
    /// stored and clears the metadata an earlier commit stored.
    #[tokio::test]
    async fn metadata_past_the_bound_commits_lexical_documents_without_it()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::{LexicalIndexLimits, LexicalSearchIndex, RevisionScoped};

        let temp = tempfile::tempdir()?;
        let database = crate::WorkspaceDatabase::open(
            &temp.path().join("index.db"),
            crate::DatabasePool::new(2, 1000),
        )
        .await?;
        let path = rift_core::ProjectPath::new("README.md")?;
        let text = "quartzterm guide\n";
        let metadata = collection(text);
        let document = index_document(&path, text, "digest")?;
        let fits = LexicalSearchIndex::attached(
            std::sync::Arc::clone(&database),
            LexicalIndexLimits::default(),
        );
        fits.replace_all_with_documentation(std::slice::from_ref(&document), "tree1", &metadata)
            .await?;
        let store = LexicalSearchIndex::attached(
            std::sync::Arc::clone(&database),
            LexicalIndexLimits::default().with_documentation_bytes_max(64),
        );
        store
            .replace_all_with_documentation(std::slice::from_ref(&document), "tree2", &metadata)
            .await?;
        assert!(matches!(
            store.documentation("tree2").await?,
            RevisionScoped::Matched(None)
        ));
        assert!(read_reference_rows(&database).await?.is_empty());
        let identity = rift_ranking::DocumentIdentity::new("README.md")?;
        assert_eq!(store.content(&identity).await?.as_deref(), Some(text));
        store
            .apply_with_documentation(
                &crate::LexicalChange::default(),
                &LexicalStamp::published("tree3", ""),
                &metadata,
            )
            .await?;
        assert!(matches!(
            store.documentation("tree3").await?,
            RevisionScoped::Matched(None)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn metadata_replace_refuses_excess_stored_source_rows()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let database = crate::WorkspaceDatabase::open(
            &directory.path().join("index.db"),
            crate::DatabasePool::new(2, 1000),
        )
        .await?;
        let encoded = EncodedDocumentation::within(&collection("Guide.\n"), METADATA_BYTES_MAX)?;
        let mut access = database.writing().await?;
        let mut transaction = access.transaction().await?;
        let row_limit = i64::from(rift_protocol::documentation::DOCUMENTATION_SOURCES_MAX);
        toasty::sql::statement(
            "WITH RECURSIVE rows(number) AS (\
               SELECT 1 UNION ALL SELECT number + 1 FROM rows WHERE number < ?1\
             ) \
             INSERT INTO documentation_sources(identity, digest, payload) \
             SELECT 'source-' || number, x'00', '{}' FROM rows",
        )
        .bind(row_limit + 1)
        .exec(&mut transaction)
        .await?;

        let error = replace(&mut transaction, Some(&encoded))
            .await
            .expect_err("replacement must refuse more stored sources than a collection holds");
        assert_eq!(
            error.fault().violation(),
            crate::LexicalIndexViolation::RecordLimit
        );
        Ok(())
    }

    fn collection(text: &str) -> rift_analysis::documentation::DocumentationCollection {
        use rift_analysis::documentation::{
            DocumentationInput, DocumentationSourceSet, collect_documentation, content_digest,
        };
        use rift_protocol::documentation::{
            DocumentationChunk, DocumentationContentIdentity, DocumentationSelectionReason,
            DocumentationSource, DocumentationSourceFormat, DocumentationSourceIdentity,
        };
        use rift_protocol::read::{
            ProjectPath, SourceKind, SourceLocationKind, SymbolOrigin, TextRange,
        };
        let source = DocumentationSource {
            identity: DocumentationContentIdentity {
                source: DocumentationSourceIdentity::Project {
                    path: ProjectPath("README.md".into()),
                },
                cell: None,
            },
            revision: content_digest(b"tree"),
            content_digest: content_digest(text.as_bytes()),
            origin: SymbolOrigin {
                location: Some(SourceLocationKind::Project),
                package: None,
                source_kind: SourceKind::Authored,
            },
            format: DocumentationSourceFormat::Markdown,
            media_type: "text/markdown".into(),
            selection: DocumentationSelectionReason::Workspace,
            byte_length: text.len() as u64,
            language: None,
            physical_ranges: Vec::new(),
            license: None,
        };
        let chunks = vec![DocumentationChunk {
            identity: "README.md".into(),
            range: TextRange {
                start: 0,
                end: text.len() as u64,
            },
        }];
        let input = DocumentationInput::new(source, text)
            .expect("source")
            .with_chunks(chunks)
            .expect("chunks");
        collect_documentation(
            &DocumentationSourceSet::new(vec![input]).expect("sources"),
            &[],
        )
        .expect("metadata")
    }

    #[tokio::test]
    async fn metadata_read_requires_a_current_corpus_revision()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::{
            LexicalIndexLimits, LexicalIndexViolation, LexicalSearchIndex, RevisionScoped,
        };

        let directory = tempfile::tempdir()?;
        let database = crate::WorkspaceDatabase::open(
            &directory.path().join("index.db"),
            crate::DatabasePool::new(2, 1000),
        )
        .await?;
        let bytes_max = 4_096;
        let store = LexicalSearchIndex::attached(
            std::sync::Arc::clone(&database),
            LexicalIndexLimits::default().with_documentation_bytes_max(bytes_max),
        );
        assert!(matches!(
            store.documentation("tree").await?,
            RevisionScoped::NoRevision
        ));
        let metadata = collection("Guide.\n");
        store
            .replace_all_with_documentation(&[], "tree", &metadata)
            .await?;
        assert!(matches!(
            store.documentation("tree").await?,
            RevisionScoped::Matched(Some(_))
        ));
        let oversized_payload = "x".repeat(bytes_max + 1);
        let mut access = database.writing().await?;
        let mut transaction = access.transaction().await?;
        toasty::sql::statement("UPDATE documentation_manifest SET payload = ?1 WHERE id = 1")
            .bind(oversized_payload)
            .exec(&mut transaction)
            .await?;
        transaction.commit().await?;
        drop(access);
        let error = store
            .documentation("tree")
            .await
            .expect_err("oversized stored metadata must be refused");
        assert_eq!(
            error.fault().violation(),
            LexicalIndexViolation::RecordLimit
        );
        store
            .replace_all_with_documentation(&[], "tree", &metadata)
            .await?;
        let mut access = database.writing().await?;
        let mut transaction = access.transaction().await?;
        toasty::sql::statement("UPDATE lexical_index_state SET corpus_revision = 'older-corpus'")
            .exec(&mut transaction)
            .await?;
        transaction.commit().await?;
        drop(access);
        assert!(matches!(
            store.documentation("tree").await?,
            RevisionScoped::NoRevision
        ));
        Ok(())
    }

    #[tokio::test]
    async fn metadata_and_lexical_revision_replace_atomically()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::{LexicalChange, LexicalIndexLimits, LexicalSearchIndex, RevisionScoped};
        use rift_ranking::{
            DocumentFields, DocumentIdentity, DocumentKind, DocumentLocation, IndexDocument,
            SearchableField,
        };
        let temp = tempfile::tempdir()?;
        let database = crate::WorkspaceDatabase::open(
            &temp.path().join("index.db"),
            crate::DatabasePool::new(2, 1000),
        )
        .await?;
        let store = LexicalSearchIndex::attached(database, LexicalIndexLimits::default());
        let path = rift_core::ProjectPath::new("README.md")?;
        let text = "# Guide\n\nDocumentation.\n";
        let document = IndexDocument::new(
            DocumentIdentity::new("README.md")?,
            DocumentLocation::Project(path.clone()),
            DocumentKind::TextFile,
            "digest",
            DocumentFields::empty().with(SearchableField::FileContent, text),
        )?;
        let metadata = collection(text);
        store
            .replace_all_with_documentation(std::slice::from_ref(&document), "tree1", &metadata)
            .await?;
        let RevisionScoped::Matched(Some(loaded)) = store.documentation("tree1").await? else {
            panic!("same revision metadata must load");
        };
        assert_eq!(loaded.index(), metadata.index());
        assert!(
            matches!(store.documentation("other").await?, RevisionScoped::OtherRevision(revision) if revision == "tree1")
        );
        assert!(
            store
                .replace_all_with_documentation(&[document.clone(), document], "tree2", &metadata)
                .await
                .is_err()
        );
        assert!(matches!(
            store.documentation("tree1").await?,
            RevisionScoped::Matched(Some(_))
        ));
        let empty = rift_analysis::documentation::collect_documentation(
            &rift_analysis::documentation::DocumentationSourceSet::new(Vec::new())?,
            &[],
        )?;
        store
            .apply_with_documentation(
                &LexicalChange::new(vec![path], Vec::new()),
                &LexicalStamp::published("tree2", ""),
                &empty,
            )
            .await?;
        let RevisionScoped::Matched(Some(loaded)) = store.documentation("tree2").await? else {
            panic!("new metadata must load");
        };
        assert!(loaded.index().blocks.is_empty());
        assert!(
            store
                .content(&DocumentIdentity::new("README.md")?)
                .await?
                .is_none()
        );
        store.replace_all(&[], "legacy").await?;
        assert!(matches!(
            store.documentation("legacy").await?,
            RevisionScoped::Matched(None)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn metadata_write_failure_rolls_back_lexical_rows_and_revision()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::{
            LexicalIndexLimits, LexicalIndexViolation, LexicalSearchIndex, RevisionScoped,
        };
        use rift_ranking::{DocumentIdentity, ParsedQuery, QueryPhase};

        let temp = tempfile::tempdir()?;
        let database = crate::WorkspaceDatabase::open(
            &temp.path().join("index.db"),
            crate::DatabasePool::new(2, 1000),
        )
        .await?;
        let store = LexicalSearchIndex::attached(
            std::sync::Arc::clone(&database),
            LexicalIndexLimits::default(),
        );
        let path = rift_core::ProjectPath::new("README.md")?;
        let old_text = "oldquartzterm guide\n";
        let old_document = index_document(&path, old_text, "old-digest")?;
        let old_metadata = reference_collection("api.md", "Use `Compass`, then `Compass`.\n")?;
        store
            .replace_all_with_documentation(&[old_document], "tree-old", &old_metadata)
            .await?;
        let old_reference_rows = read_reference_rows(&database).await?;

        let mut access = database.writing().await?;
        let mut transaction = access.transaction().await?;
        toasty::sql::statement(
            "CREATE TRIGGER reject_documentation_manifest BEFORE INSERT ON documentation_manifest \
             BEGIN SELECT RAISE(ABORT, 'fixture rejection'); END",
        )
        .exec(&mut transaction)
        .await?;
        transaction.commit().await?;
        drop(access);

        let new_text = "newquartzterm guide\n";
        let new_document = index_document(&path, new_text, "new-digest")?;
        let new_metadata = reference_collection("api.md", "Use `Apex`, then `Compass`.\n")?;
        let error = store
            .replace_all_with_documentation(&[new_document], "tree-new", &new_metadata)
            .await
            .expect_err("metadata trigger refuses after lexical writes");
        assert_eq!(error.fault().violation(), LexicalIndexViolation::Storage);
        assert_eq!(read_reference_rows(&database).await?, old_reference_rows);

        let RevisionScoped::Matched(Some(loaded)) = store.documentation("tree-old").await? else {
            panic!("the prior metadata revision remains readable");
        };
        assert_eq!(loaded.index(), old_metadata.index());
        assert!(matches!(
            store.documentation("tree-new").await?,
            RevisionScoped::OtherRevision(revision) if revision == "tree-old"
        ));
        assert_eq!(
            store
                .content(&DocumentIdentity::new("README.md")?)
                .await?
                .as_deref(),
            Some(old_text)
        );
        let RevisionScoped::Matched(old_rank) = store
            .rank(
                "tree-old",
                &ParsedQuery::parse("oldquartzterm")?,
                QueryPhase::Precise,
                10,
            )
            .await?
        else {
            panic!("the prior lexical revision remains searchable");
        };
        assert_eq!(old_rank.order().len(), 1);
        let RevisionScoped::Matched(new_rank) = store
            .rank(
                "tree-old",
                &ParsedQuery::parse("newquartzterm")?,
                QueryPhase::Precise,
                10,
            )
            .await?
        else {
            panic!("the prior lexical revision remains searchable");
        };
        assert!(new_rank.order().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn an_unchanged_source_row_is_not_written_during_a_metadata_change()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::{LexicalIndexLimits, LexicalSearchIndex, RevisionScoped};

        let temp = tempfile::tempdir()?;
        let database = crate::WorkspaceDatabase::open(
            &temp.path().join("index.db"),
            crate::DatabasePool::new(2, 1000),
        )
        .await?;
        let store = LexicalSearchIndex::attached(
            std::sync::Arc::clone(&database),
            LexicalIndexLimits::default(),
        );
        let first =
            reference_collection_of(&[("a.md", "Use `Compass`.\n"), ("b.md", "Other guide.\n")])?;
        let kept = source_key(&first, "a.md")?;
        store
            .replace_all_with_documentation(&[], "tree1", &first)
            .await?;

        let mut access = database.writing().await?;
        let mut transaction = access.transaction().await?;
        for (table, column, event, row) in [
            ("documentation_sources", "identity", "DELETE", "OLD"),
            ("documentation_sources", "identity", "UPDATE", "OLD"),
            ("documentation_sources", "identity", "INSERT", "NEW"),
            ("documentation_references", "source", "DELETE", "OLD"),
            ("documentation_references", "source", "INSERT", "NEW"),
        ] {
            let trigger = format!(
                "CREATE TRIGGER reject_{table}_{event} BEFORE {event} ON {table} \
                 WHEN {row}.{column} = '{kept}' \
                 BEGIN SELECT RAISE(ABORT, 'unchanged source touched'); END"
            );
            toasty::sql::statement(&trigger)
                .exec(&mut transaction)
                .await?;
        }
        transaction.commit().await?;
        drop(access);

        let updated = reference_collection_of(&[
            ("a.md", "Use `Compass`.\n"),
            ("b.md", "Other guide, rewritten.\n"),
        ])?;
        store
            .apply_with_documentation(
                &crate::LexicalChange::default(),
                &LexicalStamp::published("tree2", ""),
                &updated,
            )
            .await?;
        let RevisionScoped::Matched(Some(loaded)) = store.documentation("tree2").await? else {
            panic!("updated metadata publication must be available");
        };
        assert_eq!(loaded.index(), updated.index());
        Ok(())
    }

    #[tokio::test]
    async fn reference_rows_follow_the_source_that_files_them()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::{LexicalIndexLimits, LexicalSearchIndex, RevisionScoped};

        let temp = tempfile::tempdir()?;
        let database = crate::WorkspaceDatabase::open(
            &temp.path().join("index.db"),
            crate::DatabasePool::new(2, 1000),
        )
        .await?;
        let store = LexicalSearchIndex::attached(
            std::sync::Arc::clone(&database),
            LexicalIndexLimits::default(),
        );
        let prior = reference_collection_of(&[
            ("api.md", "Use `Compass`, then `Compass`.\n"),
            ("gone.md", "Use `Apex`.\n"),
        ])?;
        store
            .replace_all_with_documentation(&[], "tree1", &prior)
            .await?;
        assert_eq!(
            read_reference_rows(&database).await?,
            expected_rows(&prior)?
        );

        let current = reference_collection_of(&[("api.md", "Use `Apex`, then `Compass`.\n")])?;
        store
            .apply_with_documentation(
                &crate::LexicalChange::default(),
                &LexicalStamp::published("tree2", ""),
                &current,
            )
            .await?;
        assert_eq!(
            read_reference_rows(&database).await?,
            expected_rows(&current)?,
            "a source only the store held leaves with every reference it filed"
        );
        let RevisionScoped::Matched(Some(loaded)) = store.documentation("tree2").await? else {
            panic!("current metadata must load");
        };
        assert_eq!(loaded.index(), current.index());
        Ok(())
    }

    /// The row identity `path`'s source takes in `collection`.
    fn source_key(
        collection: &DocumentationCollection,
        path: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        use rift_protocol::documentation::DocumentationSourceIdentity;
        let source = collection
            .index()
            .sources
            .iter()
            .find(|source| {
                matches!(
                    &source.identity.source,
                    DocumentationSourceIdentity::Project { path: held } if held.0 == path
                )
            })
            .ok_or("the fixture holds the source")?;
        Ok(serde_json::to_string(&source.identity)?)
    }

    /// The reference rows `collection` files, as [`read_reference_rows`] reads them back.
    fn expected_rows(
        collection: &DocumentationCollection,
    ) -> Result<Vec<ReferenceRow>, Box<dyn std::error::Error>> {
        let encoded = EncodedDocumentation::within(collection, METADATA_BYTES_MAX)?;
        let mut rows: Vec<_> = encoded
            .sources
            .iter()
            .flat_map(|source| {
                source.references.iter().map(|(identity, target, block)| {
                    (
                        identity.clone(),
                        source.identity.clone(),
                        target.clone(),
                        block.clone(),
                    )
                })
            })
            .collect();
        rows.sort();
        Ok(rows)
    }

    async fn read_reference_rows(
        database: &crate::WorkspaceDatabase,
    ) -> Result<Vec<ReferenceRow>, Box<dyn std::error::Error>> {
        let mut connection = database.connection().await?;
        let mut rows: Vec<_> = DocumentationReferenceRecord::all()
            .limit(rift_protocol::documentation::DOCUMENTATION_REFERENCES_MAX as usize + 1)
            .exec(&mut connection)
            .await?
            .into_iter()
            .map(|row| (row.identity, row.source, row.target, row.block))
            .collect();
        rows.sort();
        Ok(rows)
    }

    #[tokio::test]
    async fn replacement_recovers_from_corrupt_prior_manifest()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::{LexicalIndexLimits, LexicalSearchIndex, RevisionScoped};

        let temp = tempfile::tempdir()?;
        let database = crate::WorkspaceDatabase::open(
            &temp.path().join("index.db"),
            crate::DatabasePool::new(2, 1000),
        )
        .await?;
        let store = LexicalSearchIndex::attached(
            std::sync::Arc::clone(&database),
            LexicalIndexLimits::default(),
        );
        let prior = collection("old metadata");
        store
            .replace_all_with_documentation(&[], "tree1", &prior)
            .await?;

        let mut access = database.writing().await?;
        let mut transaction = access.transaction().await?;
        toasty::sql::statement(
            "UPDATE documentation_manifest SET payload = 'corrupt' WHERE id = 1",
        )
        .exec(&mut transaction)
        .await?;
        transaction.commit().await?;
        drop(access);

        let error = store
            .documentation("tree1")
            .await
            .expect_err("read must refuse corrupt prior metadata");
        assert_eq!(
            error.fault().violation(),
            crate::LexicalIndexViolation::StoredKindInvalid
        );

        let current = collection("current metadata");
        store
            .apply_with_documentation(
                &crate::LexicalChange::default(),
                &LexicalStamp::published("tree2", ""),
                &current,
            )
            .await?;
        let RevisionScoped::Matched(Some(loaded)) = store.documentation("tree2").await? else {
            panic!("replacement must publish validated current metadata");
        };
        assert_eq!(loaded.index(), current.index());
        Ok(())
    }

    fn reference_collection(
        path: &str,
        content: &str,
    ) -> Result<DocumentationCollection, Box<dyn std::error::Error>> {
        reference_collection_of(&[(path, content)])
    }

    /// One collection over Markdown sources at `sources`, resolving references to the
    /// `Compass` and `Apex` declarations of `lib.rs`.
    fn reference_collection_of(
        sources: &[(&str, &str)],
    ) -> Result<DocumentationCollection, Box<dyn std::error::Error>> {
        use rift_analysis::documentation::{
            DocumentationDeclaration, DocumentationInput, DocumentationSourceSet,
            collect_documentation, content_digest,
        };
        use rift_protocol::documentation::{
            DocumentationChunk, DocumentationContentIdentity, DocumentationSelectionReason,
            DocumentationSource, DocumentationSourceFormat, DocumentationSourceIdentity,
        };
        use rift_protocol::read::{
            Language, ProjectPath, SourceKind, SourceLocationKind, SymbolId, SymbolOrigin,
            TextRange,
        };

        let identity = DocumentationContentIdentity {
            source: DocumentationSourceIdentity::Project {
                path: ProjectPath("lib.rs".into()),
            },
            cell: None,
        };
        let language = Language::from_identity_segment("rust")?;
        let names = ["Compass", "Apex"];
        let symbols =
            names.map(|name| SymbolId(rift_core::symbol_identity("rust", "lib.rs", name)));
        let mut declarations = Vec::new();
        for (symbol, name) in symbols.iter().zip(names) {
            declarations.push(DocumentationDeclaration::new(
                symbol,
                &language,
                name,
                name,
                &identity,
                TextRange { start: 0, end: 19 },
            )?);
        }
        let mut inputs = Vec::new();
        for (path, content) in sources {
            let source = DocumentationSource {
                identity: DocumentationContentIdentity {
                    source: DocumentationSourceIdentity::Project {
                        path: ProjectPath((*path).into()),
                    },
                    cell: None,
                },
                revision: content_digest(content.as_bytes()),
                content_digest: content_digest(content.as_bytes()),
                origin: SymbolOrigin {
                    location: Some(SourceLocationKind::Project),
                    package: None,
                    source_kind: SourceKind::Authored,
                },
                format: DocumentationSourceFormat::Markdown,
                media_type: "text/markdown".into(),
                selection: DocumentationSelectionReason::Workspace,
                byte_length: content.len() as u64,
                language: None,
                physical_ranges: Vec::new(),
                license: None,
            };
            inputs.push(DocumentationInput::new(source, content)?.with_chunks(vec![
                DocumentationChunk {
                    identity: (*path).into(),
                    range: TextRange {
                        start: 0,
                        end: content.len() as u64,
                    },
                },
            ])?);
        }
        Ok(collect_documentation(
            &DocumentationSourceSet::new(inputs)?,
            &declarations,
        )?)
    }

    fn index_document(
        path: &rift_core::ProjectPath,
        text: &str,
        digest: &str,
    ) -> Result<rift_ranking::IndexDocument, Box<dyn std::error::Error>> {
        use rift_ranking::{
            DocumentFields, DocumentIdentity, DocumentKind, DocumentLocation, IndexDocument,
            SearchableField,
        };
        Ok(IndexDocument::new(
            DocumentIdentity::new(path.as_str())?,
            DocumentLocation::Project(path.clone()),
            DocumentKind::TextFile,
            digest,
            DocumentFields::empty().with(SearchableField::FileContent, text),
        )?)
    }
}
