//! Documentation metadata and reverse references committed with the lexical corpus.

use std::collections::BTreeMap;
use std::io::Write;

use rift_analysis::documentation::DocumentationCollection;
use rift_protocol::documentation::DocumentationIndex;
use toasty::Executor;

use crate::lexical::{
    LexicalIndexError, LexicalIndexViolation, batch_limit_error, lexical_error_caused_by,
    storage_error,
};

/// Maximum encoded documentation metadata retained by one workspace publication.
const METADATA_BYTES_MAX: usize = 64 * 1_024 * 1_024;
const MANIFEST_ID: i64 = 1;

#[derive(Debug, toasty::Model)]
#[table = "documentation_manifest"]
pub(crate) struct DocumentationManifestRecord {
    #[key]
    id: i64,
    payload: String,
}

#[derive(Debug, toasty::Model)]
#[table = "documentation_references"]
pub(crate) struct DocumentationReferenceRecord {
    #[key]
    identity: String,
    target: String,
    block: String,
    position: i64,
}

pub(crate) struct EncodedDocumentation {
    payload: String,
    references: Vec<(String, String, String, i64)>,
}

impl EncodedDocumentation {
    pub(crate) fn new(collection: &DocumentationCollection) -> Result<Self, LexicalIndexError> {
        let mut writer = MetadataWriter {
            bytes: Vec::new(),
            limit: METADATA_BYTES_MAX,
            exceeded: false,
        };
        let encoded = serde_json::to_writer(&mut writer, collection.index());
        if writer.exceeded {
            return Err(batch_limit_error(
                "documentation.bytes",
                METADATA_BYTES_MAX as u64 + 1,
                METADATA_BYTES_MAX as u64,
            ));
        }
        encoded.map_err(invalid_metadata)?;
        let payload = String::from_utf8(writer.bytes).map_err(invalid_metadata)?;
        let references = collection
            .index()
            .references
            .iter()
            .enumerate()
            .map(|(position, reference)| {
                (
                    reference.identity.0.clone(),
                    reference.target.0.clone(),
                    reference.block.0.clone(),
                    i64::try_from(position).unwrap_or(i64::MAX),
                )
            })
            .collect();
        Ok(Self {
            payload,
            references,
        })
    }
}

struct MetadataWriter {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl Write for MetadataWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(std::io::Error::other(
                "documentation metadata exceeds byte bound",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Replaces metadata using the lexical writer's existing transaction.
pub(crate) async fn replace(
    executor: &mut dyn Executor,
    encoded: Option<&EncodedDocumentation>,
) -> Result<(), LexicalIndexError> {
    match encoded {
        Some(encoded) => {
            let previous = DocumentationReferenceRecord::all()
                .limit(rift_protocol::documentation::DOCUMENTATION_REFERENCES_MAX as usize + 1)
                .exec(executor)
                .await
                .map_err(storage_error)?;
            if previous.len() > rift_protocol::documentation::DOCUMENTATION_REFERENCES_MAX as usize
            {
                return Err(batch_limit_error(
                    "documentation.references",
                    previous.len() as u64,
                    u64::from(rift_protocol::documentation::DOCUMENTATION_REFERENCES_MAX),
                ));
            }
            update_references(executor, &encoded.references, &previous).await?;
        }
        None => {
            DocumentationReferenceRecord::all()
                .delete()
                .exec(executor)
                .await
                .map_err(storage_error)?;
        }
    }
    DocumentationManifestRecord::all()
        .delete()
        .exec(executor)
        .await
        .map_err(storage_error)?;
    let Some(encoded) = encoded else {
        return Ok(());
    };
    toasty::create!(DocumentationManifestRecord {
        id: MANIFEST_ID,
        payload: encoded.payload.clone()
    })
    .exec(executor)
    .await
    .map_err(storage_error)?;
    Ok(())
}

async fn update_references(
    executor: &mut dyn Executor,
    current: &[(String, String, String, i64)],
    previous: &[DocumentationReferenceRecord],
) -> Result<(), LexicalIndexError> {
    let current = current_reference_rows(current);
    let previous = previous_reference_rows(previous);
    for (identity, row) in &previous {
        if current.get(identity) == Some(row) {
            continue;
        }
        DocumentationReferenceRecord::filter_by_identity(identity)
            .delete()
            .exec(executor)
            .await
            .map_err(storage_error)?;
    }
    for (identity, (target, block, position)) in &current {
        let unchanged =
            previous
                .get(identity)
                .is_some_and(|(prior_target, prior_block, prior_position)| {
                    prior_target == target && prior_block == block && prior_position == position
                });
        if unchanged {
            continue;
        }
        toasty::create!(DocumentationReferenceRecord {
            identity: identity.clone(),
            target: target.clone(),
            block: block.clone(),
            position: *position
        })
        .exec(executor)
        .await
        .map_err(storage_error)?;
    }
    Ok(())
}

type ReferenceRow = (String, String, i64);

fn current_reference_rows(
    references: &[(String, String, String, i64)],
) -> BTreeMap<String, ReferenceRow> {
    references
        .iter()
        .map(|(identity, target, block, position)| {
            (identity.clone(), (target.clone(), block.clone(), *position))
        })
        .collect()
}

fn previous_reference_rows(
    previous: &[DocumentationReferenceRecord],
) -> BTreeMap<String, ReferenceRow> {
    previous
        .iter()
        .map(|reference| {
            (
                reference.identity.clone(),
                (
                    reference.target.clone(),
                    reference.block.clone(),
                    reference.position,
                ),
            )
        })
        .collect()
}

/// Decodes and revalidates metadata within the caller's revision-scoped transaction.
pub(crate) async fn read(
    executor: &mut dyn Executor,
) -> Result<Option<DocumentationCollection>, LexicalIndexError> {
    let record = DocumentationManifestRecord::filter_by_id(MANIFEST_ID)
        .first()
        .exec(executor)
        .await
        .map_err(storage_error)?;
    let Some(record) = record else {
        return Ok(None);
    };
    if record.payload.len() > METADATA_BYTES_MAX {
        return Err(batch_limit_error(
            "documentation.bytes",
            record.payload.len() as u64,
            METADATA_BYTES_MAX as u64,
        ));
    }
    let index: DocumentationIndex =
        serde_json::from_str(&record.payload).map_err(invalid_metadata)?;
    DocumentationCollection::new(index)
        .map(Some)
        .map_err(invalid_metadata)
}

fn invalid_metadata(error: impl std::error::Error + Send + Sync + 'static) -> LexicalIndexError {
    lexical_error_caused_by(LexicalIndexViolation::StoredKindInvalid, None, error)
}

#[cfg(test)]
mod tests {
    use super::{
        DocumentationCollection, DocumentationReferenceRecord, EncodedDocumentation, MetadataWriter,
    };
    use std::io::Write;

    #[test]
    fn metadata_encoding_accepts_exact_bound_and_refuses_growth() {
        let mut writer = MetadataWriter {
            bytes: Vec::new(),
            limit: 4,
            exceeded: false,
        };
        writer.write_all(b"text").expect("exact bound");
        assert!(!writer.exceeded);
        assert!(writer.write_all(b"!").is_err());
        assert!(writer.exceeded);
        assert_eq!(writer.bytes, b"text");
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
            .apply_with_documentation(&LexicalChange::new(vec![path], Vec::new()), "tree2", &empty)
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
    async fn unchanged_reference_rows_are_not_written_during_metadata_delta()
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
        let first = reference_collection("a.md", "Use `Compass`.\n")?;
        let unchanged = first.index().references[0].identity.0.clone();
        store
            .replace_all_with_documentation(&[], "tree1", &first)
            .await?;

        let mut access = database.writing().await?;
        let mut transaction = access.transaction().await?;
        let delete_trigger = format!(
            "CREATE TRIGGER reject_unchanged_documentation_reference_delete BEFORE DELETE ON documentation_references WHEN OLD.identity = '{unchanged}' BEGIN SELECT RAISE(ABORT, 'unchanged reference touched'); END"
        );
        toasty::sql::statement(&delete_trigger)
            .exec(&mut transaction)
            .await?;
        let update_trigger = format!(
            "CREATE TRIGGER reject_unchanged_documentation_reference_update BEFORE UPDATE ON documentation_references WHEN OLD.identity = '{unchanged}' BEGIN SELECT RAISE(ABORT, 'unchanged reference touched'); END"
        );
        toasty::sql::statement(&update_trigger)
            .exec(&mut transaction)
            .await?;
        transaction.commit().await?;
        drop(access);

        let updated = reference_collection("a.md", "Use `Compass`.\n\nAdditional paragraph.\n")?;
        store
            .apply_with_documentation(&crate::LexicalChange::default(), "tree2", &updated)
            .await?;
        let RevisionScoped::Matched(Some(loaded)) = store.documentation("tree2").await? else {
            panic!("updated metadata publication must be available");
        };
        assert!(
            loaded
                .index()
                .references
                .iter()
                .any(|reference| reference.identity.0 == unchanged)
        );
        assert_eq!(loaded.index().references.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn reference_delta_adds_removes_and_shifts_exact_rows()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::{LexicalIndexLimits, LexicalSearchIndex};

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
        let prior = reference_collection("api.md", "Use `Compass`, then `Compass`.\n")?;
        let current = reference_collection("api.md", "Use `Apex`, then `Compass`.\n")?;
        let prior_encoded = EncodedDocumentation::new(&prior)?;
        let current_encoded = EncodedDocumentation::new(&current)?;
        let mut prior_rows = prior_encoded.references.clone();
        prior_rows.sort_by(|left, right| left.0.cmp(&right.0));
        store
            .replace_all_with_documentation(&[], "tree1", &prior)
            .await?;
        assert_eq!(read_reference_rows(&database).await?, prior_rows);

        let mut current_rows = current_encoded.references.clone();
        current_rows.sort_by(|left, right| left.0.cmp(&right.0));
        store
            .apply_with_documentation(&crate::LexicalChange::default(), "tree2", &current)
            .await?;
        assert_eq!(read_reference_rows(&database).await?, current_rows);

        let prior_ids: std::collections::BTreeSet<_> = prior_encoded
            .references
            .iter()
            .map(|row| row.0.as_str())
            .collect();
        let current_ids: std::collections::BTreeSet<_> = current_encoded
            .references
            .iter()
            .map(|row| row.0.as_str())
            .collect();
        assert_eq!(prior_ids.difference(&current_ids).count(), 1);
        assert_eq!(current_ids.difference(&prior_ids).count(), 1);
        let prior_compass = prior_encoded
            .references
            .iter()
            .find(|row| row.3 == 0)
            .expect("first prior reference");
        let current_compass = current_encoded
            .references
            .iter()
            .find(|row| row.0 == prior_compass.0)
            .expect("same reference after earlier insertion");
        assert_eq!(prior_compass.1, current_compass.1);
        assert_eq!(prior_compass.2, current_compass.2);
        assert_eq!(prior_compass.3, 0);
        assert_eq!(current_compass.3, 1);
        Ok(())
    }

    async fn read_reference_rows(
        database: &crate::WorkspaceDatabase,
    ) -> Result<Vec<(String, String, String, i64)>, Box<dyn std::error::Error>> {
        let mut connection = database.connection().await?;
        let mut rows: Vec<_> = DocumentationReferenceRecord::all()
            .limit(rift_protocol::documentation::DOCUMENTATION_REFERENCES_MAX as usize + 1)
            .exec(&mut connection)
            .await?
            .into_iter()
            .map(|row| (row.identity, row.target, row.block, row.position))
            .collect();
        rows.sort_by(|left, right| left.0.cmp(&right.0));
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
            .apply_with_documentation(&crate::LexicalChange::default(), "tree2", &current)
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
        let symbol = SymbolId(rift_core::symbol_identity("rust", "lib.rs", "Compass"));
        let declaration = DocumentationDeclaration::new(
            &symbol,
            &language,
            "Compass",
            "Compass",
            &identity,
            TextRange { start: 0, end: 19 },
        )?;
        let additional_symbol = SymbolId(rift_core::symbol_identity("rust", "lib.rs", "Apex"));
        let additional_declaration = DocumentationDeclaration::new(
            &additional_symbol,
            &language,
            "Apex",
            "Apex",
            &identity,
            TextRange { start: 0, end: 19 },
        )?;
        let source = DocumentationSource {
            identity: DocumentationContentIdentity {
                source: DocumentationSourceIdentity::Project {
                    path: ProjectPath(path.into()),
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
        let input =
            DocumentationInput::new(source, content)?.with_chunks(vec![DocumentationChunk {
                identity: path.into(),
                range: TextRange {
                    start: 0,
                    end: content.len() as u64,
                },
            }])?;
        Ok(collect_documentation(
            &DocumentationSourceSet::new(vec![input])?,
            &[declaration, additional_declaration],
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
