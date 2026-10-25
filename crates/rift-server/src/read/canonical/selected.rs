//! Original source selections retained with one immutable local publication.

use super::*;
use rift_index::WorkspaceSymbolView;

impl LocalSymbolViewInput {
    /// Retains the original normalized owner selected by the producer.
    ///
    /// # Errors
    /// Refuses different roots, owners, source bytes or accepted bounds.
    pub fn with_selected_sources(
        self,
        sources: &rift_index::WorkspaceIndex,
    ) -> Result<Self, RiftError> {
        self.with_selected_view(sources.symbol_view()?)
    }

    pub(super) fn with_selected_view(
        self,
        selected: Arc<WorkspaceSymbolView>,
    ) -> Result<Self, RiftError> {
        for held in self.material.indices() {
            held.validate_source_selection(&selected)?;
        }
        if self
            .material
            .indices()
            .any(|held| std::ptr::eq(held, selected.as_ref()))
        {
            return Ok(self);
        }
        let count = self.material.selected.len().checked_add(1).ok_or_else(|| {
            material_error(
                "capture source selection",
                &"source selection exceeds the platform bound",
            )
        })?;
        let mut indices = Vec::new();
        indices
            .try_reserve_exact(count)
            .map_err(|error| material_error("capture source selection", &error))?;
        indices.extend(self.material.selected.iter().map(Arc::clone));
        indices.push(selected);
        Ok(self.with_selected_indices(Arc::from(indices)))
    }

    pub(super) fn with_selected_indices(
        mut self,
        selected: Arc<[Arc<WorkspaceSymbolView>]>,
    ) -> Self {
        let bytes = |indices: &[Arc<WorkspaceSymbolView>]| {
            std::mem::size_of_val(indices).saturating_add(2 * std::mem::size_of::<usize>())
        };
        self.retained_bytes = self
            .retained_bytes
            .saturating_sub(bytes(&self.material.selected))
            .saturating_add(bytes(&selected));
        self.material = Arc::new(LocalSymbolMaterial {
            index: Arc::clone(&self.material.index),
            selected,
            metadata: Arc::clone(&self.material.metadata),
            history: Arc::clone(&self.material.history),
            traversal: self.material.traversal.clone(),
            query_result: self.material.query_result.clone(),
            package_facts: self.material.package_facts.clone(),
            provider_selection: self.material.provider_selection.clone(),
            query_references: self.material.query_references.clone(),
        });
        self
    }

    pub(crate) fn indices(&self) -> impl Iterator<Item = &WorkspaceSymbolView> {
        self.material.indices()
    }

    /// Resolves an original normalized provider without selecting current source.
    ///
    /// # Errors
    /// Refuses provider context that differs from its original serving selection.
    pub fn provider_view(
        &self,
        context: &rift_search::query::QueryProviderContext,
    ) -> Result<Option<&WorkspaceSymbolView>, RiftError> {
        self.validate_provider_selection(&context.selection)?;
        if self.captured_provider_selection() != Some(&context.selection)
            || context.root != context.selection.root
            || context.owner != context.selection.owner
            || context.inputs.is_empty()
            || !context
                .inputs
                .iter()
                .any(|input| input.digest == context.source_digest)
        {
            return Err(invalid(
                "view",
                "captured query provider selection does not match",
            ));
        }
        Ok(self.indices().find(|view| {
            let graph = view.normalized_graph();
            context.owner == *view.owner()
                && context.index_revision == graph.index_revision()
                && context.source_revision == graph.source_revision()
                && context.tree_revision == graph.tree_revision()
                && context.inputs.iter().all(|input| {
                    rift_protocol::identity::parse_source_unit_identity(&input.uri)
                        .ok()
                        .and_then(|(_, path)| rift_core::ProjectPath::new(path).ok())
                        .and_then(|path| view.source(&path))
                        .is_some_and(|text| {
                            data_encoding::HEXLOWER
                                .encode(rift_core::FileDigest::of(text.as_bytes()).as_bytes())
                                == input.digest.as_str()
                        })
                })
        }))
    }
}

impl LocalSymbolMaterial {
    pub(super) fn indices(&self) -> impl Iterator<Item = &WorkspaceSymbolView> {
        std::iter::once(self.index.as_ref()).chain(self.selected.iter().map(Arc::as_ref))
    }

    pub(super) fn selected_object(
        &self,
        identity: &rift_core::SymbolId,
    ) -> Result<Option<rift_index::ReadableSymbol>, RiftError> {
        let mut selected = None;
        for index in self.indices() {
            if let Some(object) = index.object_by_identity(identity)? {
                if selected
                    .as_ref()
                    .is_some_and(|held: &rift_index::ReadableSymbol| {
                        held.to_protocol_symbol() != object.to_protocol_symbol()
                    })
                {
                    return Err(invalid("view", "captured symbol facts disagree"));
                }
                selected.get_or_insert(object);
            }
        }
        Ok(selected)
    }

    pub(super) fn source(&self, path: &rift_core::ProjectPath) -> Option<&str> {
        self.indices().find_map(|view| view.source(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_protocol::source_read::{GetSourceParams, GetSourceResult};
    use rift_search::query::{FactRequirement, FactRequirements, QueryLimits};

    #[test]
    fn selected_sources_keep_original_provider_records_and_held_bytes()
    -> Result<(), Box<dyn std::error::Error>> {
        let (directory, _, _) = super::super::tests::fixture(true);
        let original = "def phantom_signal():\n    return 3\n";
        std::fs::write(directory.path().join("hidden.py"), original)?;
        std::fs::write(directory.path().join(".gitignore"), "hidden.py\n")?;
        let reads = ReadService::build(
            directory.path(),
            rift_index::WorkspaceIndexLimits::default(),
            &rift_core::SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            rift_protocol::configuration::HistoryConfiguration::default(),
        )?;
        let path = rift_core::ProjectPath::new("hidden.py")?;
        assert!(reads.index.file(&path).is_none());
        let reads = Arc::new(reads);
        let params = serde_json::from_value(serde_json::json!({
            "query":"phantom_signal", "target":"symbol", "scope":"local",
            "paths":{"force_include":["hidden.py"]}
        }))?;
        let snapshot = Arc::clone(&reads).capture_search(
            params,
            Arc::new(crate::StoreAnswer::identifier_only()),
            Arc::new(crate::EngineReferences::default()),
        )?;
        let configuration = rift_protocol::configuration::WorkspaceConfiguration::default();
        let input = snapshot.view_input(true)?;
        let selection = input.provider_selection(&configuration)?;
        let input = input.with_provider_selection(selection.clone())?;
        std::fs::write(
            directory.path().join("hidden.py"),
            "def replaced():\n    return 0\n",
        )?;
        let mut requirements = FactRequirements::default();
        requirements.require(FactRequirement::Domain);
        requirements.require(FactRequirement::Source);
        let limits = QueryLimits::try_from(&configuration.search.query)?;
        let mut budget = crate::QueryFactBudget::new(limits, &|| false)?;
        let facts =
            input.query_facts_with_selection(&requirements, limits, &mut budget, &selection)?;
        let (provider, record) = facts
            .providers
            .iter()
            .find_map(|provider| {
                provider
                    .records
                    .iter()
                    .find(|record| {
                        facts.symbols[record.symbol_index].symbol.name == "phantom_signal"
                    })
                    .map(|record| (provider, record))
            })
            .ok_or("forced original provider record")?;
        assert!(facts.symbols[record.symbol_index].symbol.id.is_none());
        assert_eq!(provider.context.selection, selection);
        let selected = input.indices().nth(1).ok_or("original selected provider")?;
        assert_eq!(
            provider.context.index_revision,
            selected.normalized_graph().index_revision()
        );
        assert_eq!(
            provider.context.source_revision,
            selected.normalized_graph().source_revision()
        );
        let (page, contributions) = snapshot.page(0, &mut budget)?;
        let hit = page.results.iter().position(|hit| {
            matches!(&hit.hit, rift_protocol::read::SearchHitTarget::Symbol { symbol } if symbol.name == "phantom_signal")
        }).ok_or("original selected producer row")?;
        assert_eq!(record.contributions, contributions[hit]);
        let source = facts
            .sources
            .iter()
            .find(|source| source.unit.as_str().ends_with("hidden.py"))
            .ok_or("original forced source")?;
        assert_eq!(source.text.as_deref(), Some(original));
        let unit = source.unit.clone();
        let store = LocalSymbolViews::new(1 << 20, [47; 32]);
        let view = store.capture(input)?.ok_or("admitted original material")?;
        std::fs::remove_file(directory.path().join("hidden.py"))?;
        let answer = store.get_source(
            &GetSourceParams {
                unit,
                range: None,
                view: Some(view.id.clone()),
                rev: None,
            },
            reads.index.root(),
            reads.index.owner(),
        )?;
        let GetSourceResult::Found {
            source,
            view: answered,
            source_complete,
            ..
        } = answer
        else {
            return Err("forced source must remain captured".into());
        };
        assert_eq!(source.text, original);
        assert_eq!(answered, view);
        assert!(source_complete);
        Ok(())
    }

    #[test]
    fn selected_original_directories_keep_canonical_pages_without_duplicate_bindings()
    -> Result<(), Box<dyn std::error::Error>> {
        let (directory, reads, id) = super::super::tests::fixture(true);
        let original = reads.index.symbol_view()?;
        let input = reads
            .local_search_view_input(true)?
            .with_selected_view(Arc::new(original.as_ref().clone()))?;
        let store = LocalSymbolViews::new(1 << 20, [48; 32]);
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let view = store
            .capture_at(input, now)?
            .ok_or("admitted original material")?;
        let mut request = GetSymbolParams {
            id: id.clone(),
            include: vec![GetSymbolInclude::Source],
            rev: None,
            view: Some(view.id.clone()),
            declaration_limit: 1,
            declaration_cursor: None,
        };
        std::fs::write(
            directory.path().join("index.js"),
            "export function replaced() {}\n",
        )?;
        let mut bindings = BTreeSet::new();
        for page in 0..3 {
            let result =
                store.resolve_at(&request, reads.index.root(), reads.index.owner(), now)?;
            let GetSymbolResult::Found {
                symbol,
                declarations,
                declaration_cursor,
                view: answered,
                ..
            } = result
            else {
                return Err("original canonical object must be retained".into());
            };
            assert_eq!(answered, view);
            assert_eq!(symbol.signatures.len(), 3);
            assert_eq!(symbol.documentation.len(), 3);
            assert_eq!(declarations.len(), 1);
            let declaration = &declarations[0];
            assert!(declaration.source_complete);
            assert!(
                declaration
                    .source
                    .as_ref()
                    .is_some_and(|source| source.contains("open"))
            );
            assert!(bindings.insert((declaration.path.clone(), declaration.range.start)));
            request.declaration_cursor = declaration_cursor;
            assert_eq!(request.declaration_cursor.is_some(), page < 2);
        }
        Ok(())
    }
}
