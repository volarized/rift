//! Immutable domain facts projected from the existing workspace publication.

use crate::QueryFactBudget;
use crate::read::{LocalSymbolViewInput, ReadService};
use rift_error::{RiftError, errors};
use rift_index::WorkspaceSymbolView;
use rift_protocol::read::SourceUnitId;
use rift_search::query::FactRequirement;
use rift_search::query::{
    FactRequirements, QueryFactFamily, QueryFacts, QueryLimits, QuerySource, QuerySymbol,
};

mod history;
mod local_provider;
mod nodes;

/// Combines admitted facts while preserving their original source and provider associations.
///
/// # Errors
/// Refuses conflicting source bytes, record identities or captured selectors.
pub fn merge_query_facts(held: &mut QueryFacts, added: QueryFacts) -> Result<(), RiftError> {
    crate::engine_read::merge_package_facts(held, added)
}

/// Returns the serving root URI through the existing workspace path codec.
///
/// # Errors
/// Refuses a relative or non-Unicode root or an invalid workspace identity.
pub fn query_provider_root(root: &std::path::Path) -> Result<rift_core::WorkspaceId, RiftError> {
    let uri = rift_lsp::uri::TreeRoot::new(root)?.root_uri()?;
    rift_core::WorkspaceId::new(uri.as_str())
}

impl ReadService {
    /// Projects selected published facts without reading or analyzing source again.
    ///
    /// # Errors
    /// Returns unavailable facts, an invalid identity, or the crossed acquisition bound.
    pub fn query_facts(
        &self,
        source: bool,
        relationships: bool,
        limits: QueryLimits,
    ) -> Result<QueryFacts, RiftError> {
        let view = self.index().symbol_view()?;
        let mut requirements = FactRequirements::default();
        if source {
            requirements.require(FactRequirement::Source);
        }
        if relationships {
            requirements.require(FactRequirement::CompleteDomain);
        }
        project_query_facts(&view, &requirements, limits)
    }
}

impl LocalSymbolViewInput {
    /// Captures provider context from this original immutable source selection.
    ///
    /// The caller runs configuration encoding on its existing bounded worker.
    ///
    /// # Errors
    /// Returns the original root, source identity or configuration encoding failure.
    pub fn provider_selection(
        &self,
        configuration: &rift_protocol::configuration::WorkspaceConfiguration,
    ) -> Result<rift_search::query::QueryProviderSelection, RiftError> {
        let graph = self.index().normalized_graph();
        let root = query_provider_root(self.root())?;
        Ok(rift_search::query::QueryProviderSelection {
            root,
            owner: self.owner().clone(),
            index_revision: graph.index_revision(),
            source_revision: graph.source_revision(),
            tree_revision: graph.tree_revision(),
            fingerprint: rift_protocol::identity::SourceDigest::parse(
                &data_encoding::HEXLOWER.encode(self.index().fingerprint().as_bytes()),
            )?,
            configuration: rift_search::query::QueryProviderSelection::configuration_digest(
                configuration,
            )?,
        })
    }

    /// Validates a provider selection against this original immutable publication.
    ///
    /// # Errors
    /// Refuses a different root, owner, source fingerprint or normalized revision.
    pub fn validate_provider_selection(
        &self,
        selection: &rift_search::query::QueryProviderSelection,
    ) -> Result<(), RiftError> {
        let graph = self.index().normalized_graph();
        let root = query_provider_root(self.root())?;
        if selection.root.as_str() != root.as_str()
            || &selection.owner != self.owner()
            || selection.index_revision != graph.index_revision()
            || selection.source_revision != graph.source_revision()
            || selection.tree_revision != graph.tree_revision()
            || selection.fingerprint.as_str()
                != data_encoding::HEXLOWER.encode(self.index().fingerprint().as_bytes())
        {
            return errors::server::read_unavailable()
                .operation("query provider selection")
                .detail("captured provider selection differs from its original publication")
                .fail();
        }
        Ok(())
    }

    /// Projects facts under their original captured provider selection.
    ///
    /// # Errors
    /// Refuses a different selection, missing facts or exhausted request admission.
    pub fn query_facts_with_selection(
        &self,
        requirements: &FactRequirements,
        limits: QueryLimits,
        budget: &mut QueryFactBudget<'_>,
        selection: &rift_search::query::QueryProviderSelection,
    ) -> Result<QueryFacts, RiftError> {
        self.project_with_original_subjects(requirements, limits, budget, selection, |_, _| Ok(()))
            .map(|(facts, ())| facts)
    }

    /// Projects facts and retains selectors for every original symbol row.
    ///
    /// Selectors include anonymous symbols and are captured before acquired targets.
    ///
    /// # Errors
    /// Returns the existing projection or material admission refusal.
    pub fn query_facts_with_subjects(
        &self,
        requirements: &FactRequirements,
        limits: QueryLimits,
        budget: &mut QueryFactBudget<'_>,
        selection: &rift_search::query::QueryProviderSelection,
    ) -> Result<(QueryFacts, Vec<usize>), RiftError> {
        self.project_with_original_subjects(
            requirements,
            limits,
            budget,
            selection,
            |facts, budget| {
                let bytes = facts
                    .symbols
                    .len()
                    .checked_mul(size_of::<usize>())
                    .and_then(|bytes| bytes.checked_add(size_of::<Vec<usize>>()))
                    .ok_or_else(|| {
                        errors::server::read_unavailable()
                            .operation("query acquisition")
                            .detail("original subject selectors exceed the platform bound")
                            .error()
                    })?;
                budget.charge_material(bytes)?;
                let mut subjects = Vec::new();
                subjects
                    .try_reserve_exact(facts.symbols.len())
                    .map_err(|error| {
                        errors::server::read_unavailable()
                            .operation("query acquisition")
                            .detail(error.to_string())
                            .error()
                    })?;
                subjects.extend(0..facts.symbols.len());
                Ok(subjects)
            },
        )
    }

    fn project_with_original_subjects<T>(
        &self,
        requirements: &FactRequirements,
        limits: QueryLimits,
        budget: &mut QueryFactBudget<'_>,
        selection: &rift_search::query::QueryProviderSelection,
        capture: impl FnOnce(&QueryFacts, &mut QueryFactBudget<'_>) -> Result<T, RiftError>,
    ) -> Result<(QueryFacts, T), RiftError> {
        self.validate_provider_selection(selection)?;
        let include_acquired = provider_relationships(requirements);
        let mut projection = requirements.clone();
        if include_acquired && self.query_references().is_some() {
            projection.symbol_ids = None;
            projection.symbol_name = None;
        }
        let mut facts =
            self.project_selected_facts(&projection, limits, budget, Some(selection))?;
        if let Some(package) = self.package_facts() {
            budget.charge(package.as_ref())?;
            merge_query_facts(&mut facts, package.as_ref().clone())?;
        }
        let original = capture(&facts, budget)?;
        if let Some(references) = self.query_references().filter(|_| include_acquired) {
            budget.charge_material(references.retained_material_bytes())?;
            for relationship in references.query_local_relationships() {
                budget.charge(relationship)?;
                facts.relationships.push(relationship.clone());
            }
            for occurrence in references.query_occurrences() {
                budget.charge(occurrence)?;
                facts.occurrences.push(occurrence.clone());
            }
            references.project_provider_edges(&mut facts)?;
        }
        history::require_foreign_histories(self.owner(), requirements, &mut facts);
        Ok((facts, original))
    }

    /// Projects facts from this input's original publication and source selection.
    ///
    /// # Errors
    /// Returns unavailable facts, invalid captured identities, or an acquisition bound.
    pub fn query_facts(
        &self,
        requirements: &FactRequirements,
        limits: QueryLimits,
    ) -> Result<QueryFacts, RiftError> {
        self.query_facts_with_cancellation(requirements, limits, &|| false)
    }

    /// Projects selected facts with cancellation checked before each owned record.
    ///
    /// # Errors
    /// Returns cancellation, unavailable facts or exhausted acquisition admission.
    pub fn query_facts_with_cancellation(
        &self,
        requirements: &FactRequirements,
        limits: QueryLimits,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<QueryFacts, RiftError> {
        let mut budget = QueryFactBudget::new(limits, cancelled)?;
        self.query_facts_with_budget(requirements, limits, &mut budget)
    }

    /// Projects selected facts using the caller's remaining request admission.
    ///
    /// # Errors
    /// Returns cancellation, unavailable facts or exhausted acquisition admission.
    pub fn query_facts_with_budget(
        &self,
        requirements: &FactRequirements,
        limits: QueryLimits,
        budget: &mut QueryFactBudget<'_>,
    ) -> Result<QueryFacts, RiftError> {
        self.project_selected_facts(requirements, limits, budget, None)
    }

    fn project_selected_facts(
        &self,
        requirements: &FactRequirements,
        limits: QueryLimits,
        budget: &mut QueryFactBudget<'_>,
        selection: Option<&rift_search::query::QueryProviderSelection>,
    ) -> Result<QueryFacts, RiftError> {
        if requirements.requires(FactRequirement::Domain) && !self.ready {
            return errors::server::read_unavailable()
                .operation("query acquisition")
                .detail("selected workspace publication is not ready")
                .fail();
        }
        let mut facts = QueryFacts::default();
        for view in self.indices() {
            budget.check()?;
            let mut selected = project_query_facts_with_budget(view, requirements, limits, budget)?;
            if let Some(selection) = selection {
                local_provider::capture(
                    view,
                    selection,
                    requirements,
                    &mut selected,
                    limits,
                    budget,
                )?;
            }
            merge_query_facts(&mut facts, selected)?;
        }
        budget.check()?;
        if requirements.requires(FactRequirement::History)
            || requirements.requires(FactRequirement::Description)
        {
            for (_, bytes) in self.captured_histories() {
                budget.check()?;
                if facts.histories.len() >= limits.quads_max {
                    return query_bound("search.query.statements");
                }
                budget.charge_bytes(bytes)?;
                let history = serde_json::from_slice(bytes).map_err(|error| {
                    errors::server::read_unavailable()
                        .operation("decode captured history")
                        .detail(error.to_string())
                        .error()
                })?;
                facts.histories.push(history);
            }
        }
        let captured = if requirements.requires(FactRequirement::AllHistories) {
            self.captured_histories_complete()
        } else {
            !requirements.history_symbols.is_empty()
                && requirements.history_symbols.iter().all(|identity| {
                    facts
                        .histories
                        .iter()
                        .any(|history| history.symbol == *identity && history.complete)
                })
        };
        if captured {
            facts
                .unavailable_families
                .retain(|family| *family != QueryFactFamily::History);
        }
        Ok(facts)
    }
}

fn project_query_facts(
    view: &WorkspaceSymbolView,
    requirements: &FactRequirements,
    limits: QueryLimits,
) -> Result<QueryFacts, RiftError> {
    let mut budget = QueryFactBudget::new(limits, &|| false)?;
    project_query_facts_with_budget(view, requirements, limits, &mut budget)
}

fn project_query_facts_with_budget(
    view: &WorkspaceSymbolView,
    requirements: &FactRequirements,
    limits: QueryLimits,
    budget: &mut QueryFactBudget<'_>,
) -> Result<QueryFacts, RiftError> {
    let mut facts = QueryFacts {
        unavailable_families: vec![QueryFactFamily::Nodes, QueryFactFamily::History],
        ..QueryFacts::default()
    };
    let node_sources_only = requirements.selects_node_sources();
    if !node_sources_only {
        for (identity, object) in view.objects() {
            budget.check()?;
            if requirements.symbol_ids.as_ref().is_some_and(|identities| {
                !identities
                    .iter()
                    .any(|selected| selected.as_str() == identity.as_str())
            }) {
                continue;
            }
            let object = object?;
            if requirements
                .symbol_name
                .as_deref()
                .is_some_and(|name| object.facts().name() != name)
            {
                continue;
            }
            if facts.symbols.len() >= limits.quads_max {
                return query_bound("search.query.statements");
            }
            let (first, total) = view.declarations_by_identity_page(identity, 0, 1)?;
            if total > view.results_max() {
                return query_bound("search.results");
            }
            let symbol = object.to_protocol_symbol();
            budget.charge(&symbol)?;
            let mut declarations = Vec::new();
            let mut first = Some(first);
            for offset in 0..total {
                budget.check()?;
                let page = match first.take() {
                    Some(page) => page,
                    None => view.declarations_by_identity_page(identity, offset, 1)?.0,
                };
                let Some(mut declaration) = page.into_iter().next() else {
                    return query_bound("search.results");
                };
                budget.charge(&declaration)?;
                if requirements.requires(FactRequirement::Source) {
                    let (bindings, _) = view.symbols_by_identity_page(identity, offset, 1)?;
                    if let Some(matched) = bindings.first() {
                        budget.charge(&crate::read::excerpt_ref(
                            matched.file,
                            matched.symbol.range,
                        ))?;
                        declaration.source =
                            Some(crate::read::excerpt(matched.file, matched.symbol.range));
                        declaration.source_complete = true;
                    }
                }
                declarations.push(declaration);
            }
            facts.symbols.push(QuerySymbol {
                symbol,
                declarations,
            });
            if requirements.requires(FactRequirement::CompleteDomain)
                || requirements.requires(FactRequirement::Description)
            {
                for edge in view.relationships().outgoing(identity) {
                    budget.check()?;
                    if facts.relationships.len() >= limits.quads_max {
                        return query_bound("search.query.statements");
                    }
                    let edge = edge.to_protocol();
                    budget.charge(&edge)?;
                    facts.relationships.push(edge);
                }
            }
        }
        project_physical_symbols(view, requirements, &mut facts, limits, budget)?;
    }
    project_sources(view, requirements, &mut facts, limits, budget)?;
    project_documentation(view, requirements, &mut facts, limits, budget)?;
    Ok(facts)
}

fn project_physical_symbols(
    view: &WorkspaceSymbolView,
    requirements: &FactRequirements,
    facts: &mut QueryFacts,
    limits: QueryLimits,
    budget: &mut QueryFactBudget<'_>,
) -> Result<(), RiftError> {
    let physical_targets = provider_relationships(requirements);
    if requirements.symbol_ids.is_none() || physical_targets {
        for declaration in view.unestablished_declarations() {
            budget.check()?;
            let (object, matched, declaration) = declaration?;
            if !physical_targets
                && requirements
                    .symbol_name
                    .as_deref()
                    .is_some_and(|name| object.facts().name() != name)
            {
                continue;
            }
            if facts.symbols.len() >= limits.quads_max {
                return query_bound("search.query.statements");
            }
            budget.charge(declaration)?;
            let symbol = object.to_protocol_symbol();
            budget.charge(&symbol)?;
            let mut declaration = declaration.clone();
            if requirements.requires(FactRequirement::Source) {
                budget.charge(&crate::read::excerpt_ref(
                    matched.file,
                    matched.symbol.range,
                ))?;
                declaration.source = Some(crate::read::excerpt(matched.file, matched.symbol.range));
                declaration.source_complete = true;
            }
            facts.symbols.push(QuerySymbol {
                symbol,
                declarations: vec![declaration],
            });
        }
    }
    Ok(())
}

fn project_sources(
    view: &WorkspaceSymbolView,
    requirements: &FactRequirements,
    facts: &mut QueryFacts,
    limits: QueryLimits,
    budget: &mut QueryFactBudget<'_>,
) -> Result<(), RiftError> {
    let mut source_bytes = 0usize;
    let mut node_count = 0usize;
    let mut nodes_complete = true;
    for (path, source) in view.sources() {
        budget.check()?;
        if (requirements.symbol_name.is_some() || requirements.symbol_ids.is_some())
            && !requirements.requires(FactRequirement::Nodes)
            && requirements.source_units.is_none()
        {
            break;
        }
        let unit = SourceUnitId(rift_syntax::source_unit_for_path(path)?.to_string());
        if requirements
            .source_units
            .as_ref()
            .is_some_and(|units| !units.contains(&unit))
        {
            continue;
        }
        if facts.sources.len() >= limits.quads_max {
            return query_bound("search.query.statements");
        }
        if requirements.requires(FactRequirement::Source) {
            source_bytes = source_bytes
                .checked_add(source.len())
                .ok_or_else(|| query_bound_error("search.query.bytes"))?;
            if source_bytes > limits.rdf_bytes_max {
                return query_bound("search.query.bytes");
            }
        }
        if requirements.requires(FactRequirement::Source) {
            budget.charge(&source)?;
        }
        let mut record = QuerySource {
            unit,
            digest: Some(rift_protocol::identity::SourceDigest::parse(
                &data_encoding::HEXLOWER
                    .encode(rift_core::FileDigest::of(source.as_bytes()).as_bytes()),
            )?),
            text: None,
        };
        budget.charge(&record)?;
        if requirements.requires(FactRequirement::Nodes) {
            if let Some(file) = view.file(path) {
                node_count = node_count
                    .checked_add(file.syntax().nodes().map_or(0, <[_]>::len))
                    .ok_or_else(|| query_bound_error("search.query.statements"))?;
                if node_count > limits.quads_max {
                    return query_bound("search.query.statements");
                }
                nodes_complete &= nodes::project(view, file, &record.unit, facts, budget)?;
            } else if requirements.source_units.is_some() {
                nodes_complete = false;
            }
        }
        record.text = requirements
            .requires(FactRequirement::Source)
            .then(|| source.to_owned());
        facts.sources.push(record);
    }
    if requirements.requires(FactRequirement::Nodes)
        && nodes_complete
        && requirements.source_units.as_ref().is_none_or(|units| {
            units
                .iter()
                .all(|unit| facts.nodes.iter().any(|source| &source.unit == unit))
        })
    {
        facts
            .unavailable_families
            .retain(|family| *family != QueryFactFamily::Nodes);
    }
    Ok(())
}

fn project_documentation(
    view: &WorkspaceSymbolView,
    requirements: &FactRequirements,
    facts: &mut QueryFacts,
    limits: QueryLimits,
    budget: &mut QueryFactBudget<'_>,
) -> Result<(), RiftError> {
    let documentation = view.documentation().index();
    if requirements.symbol_name.is_none() && requirements.symbol_ids.is_none() {
        if documentation.coverage.omitted != 0
            || documentation.coverage.truncated != 0
            || !documentation.warnings.is_empty()
        {
            facts
                .unavailable_families
                .push(QueryFactFamily::Documentation);
        }
        let count = documentation
            .sources
            .len()
            .checked_add(documentation.blocks.len())
            .and_then(|count| count.checked_add(documentation.links.len()))
            .and_then(|count| count.checked_add(documentation.references.len()))
            .and_then(|count| count.checked_add(documentation.unresolved_references.len()))
            .ok_or_else(|| query_bound_error("search.query.statements"))?;
        if count > limits.quads_max {
            return query_bound("search.query.statements");
        }
        for record in &documentation.sources {
            budget.charge(record)?;
            facts.documentation_sources.push(record.clone());
        }
        for record in &documentation.blocks {
            budget.charge(record)?;
            facts.documentation_blocks.push(record.clone());
        }
        for record in &documentation.links {
            budget.charge(record)?;
            facts.documentation_links.push(record.clone());
        }
        for record in &documentation.references {
            budget.charge(record)?;
            facts.documentation_references.push(record.clone());
        }
        for record in &documentation.unresolved_references {
            budget.charge(record)?;
            facts
                .documentation_reference_candidates
                .push(record.clone());
        }
    }
    Ok(())
}

fn provider_relationships(requirements: &FactRequirements) -> bool {
    requirements.requires(FactRequirement::CompleteDomain)
        || requirements.requires(FactRequirement::Description)
        || requirements.requires(FactRequirement::AllOccurrences)
        || !requirements.facets.is_empty()
        || !requirements.adjacency.is_empty()
        || !requirements.occurrences.is_empty()
}

fn query_bound<T>(bound: &'static str) -> Result<T, RiftError> {
    Err(query_bound_error(bound))
}

fn query_bound_error(bound: &'static str) -> RiftError {
    errors::server::read_unavailable()
        .operation("query acquisition")
        .detail(format!(
            "query exceeds {bound}; narrow the query or increase the accepted bound"
        ))
        .error()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_core::{SourceVisibility, TextFileInclusion};
    use rift_index::{WorkspaceIndex, WorkspaceIndexLimits};
    use rift_protocol::query::QueryAnswer;
    use rift_search::sparql::{PreparedQuery, QueryCancellation};

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
    fn captured_query_facts_preserve_selected_source_and_narrow_names()
    -> Result<(), Box<dyn std::error::Error>> {
        let original = "/// Original.\npub fn kept() {}\npub fn other() {}\n";
        let (root, index) = query_fixture(original)?;
        let view = index.symbol_view()?;
        let prepared = PreparedQuery::parse(
            "SELECT ?symbol { ?symbol a :Symbol; :name \"kept\" }",
            limits(),
        )?;
        let selected = project_query_facts(&view, prepared.requirements(), limits())?;
        assert_eq!(selected.symbols.len(), 1);
        assert!(selected.sources.is_empty());
        let complete = project_query_facts(&view, &FactRequirements::default(), limits())?;
        assert!(complete.symbols.len() > selected.symbols.len());
        assert_eq!(
            prepared.evaluate(selected, limits(), QueryCancellation::default())?,
            prepared.evaluate(complete, limits(), QueryCancellation::default())?
        );
        drop(index);
        std::fs::write(root.path().join("lib.rs"), "pub fn changed() {}\n")?;
        let selected = project_query_facts(
            &view,
            &FactRequirements {
                required: [FactRequirement::Source].into(),
                ..FactRequirements::default()
            },
            limits(),
        )?;
        assert!(
            selected
                .sources
                .iter()
                .any(|source| source.text.as_deref() == Some(original))
        );
        let nodes = PreparedQuery::parse("ASK { ?node a :Node }", limits())?;
        assert!(
            nodes
                .evaluate(selected.clone(), limits(), QueryCancellation::default())
                .is_err()
        );
        assert_eq!(
            PreparedQuery::parse("ASK { ?symbol :name \"kept\" }", limits())?.evaluate(
                selected,
                limits(),
                QueryCancellation::default()
            )?,
            QueryAnswer::Boolean { value: true }
        );
        Ok(())
    }

    #[test]
    fn captured_nodes_keep_original_tree_declaration_and_source_after_edit()
    -> Result<(), Box<dyn std::error::Error>> {
        let original = "pub fn kept() {}";
        let (root, index) = query_fixture(original)?;
        let view = index.symbol_view()?;
        let path = rift_core::ProjectPath::new("lib.rs")?;
        let file = view.file(&path).ok_or("captured file")?;
        let rows = file.syntax().nodes().ok_or("complete captured rows")?;
        let count = rows.len();
        let same_span = rows
            .iter()
            .enumerate()
            .find_map(|(ordinal, node)| {
                (node.kind == "function_item" && node.range == rows[0].range).then_some(ordinal)
            })
            .ok_or("same-span root and function")?;
        drop(index);
        std::fs::write(root.path().join("lib.rs"), "pub fn changed() {}")?;
        let mut requirements = FactRequirements {
            required: [
                FactRequirement::Domain,
                FactRequirement::Nodes,
                FactRequirement::Source,
            ]
            .into(),
            source_units: Some(vec![SourceUnitId::parse("rift://source/project/lib.rs")?]),
            ..FactRequirements::default()
        };
        let facts = project_query_facts(&view, &requirements, limits())?;
        assert!(!facts.unavailable_families.contains(&QueryFactFamily::Nodes));
        assert_eq!(facts.nodes.len(), 1);
        let source = &facts.nodes[0];
        assert_eq!(source.nodes.len(), count);
        assert_eq!(source.bytes, u64::try_from(original.len())?);
        assert_eq!(source.nodes[0].node.id, source.nodes[same_span].node.id);
        assert_ne!(source.nodes[0].ordinal, source.nodes[same_span].ordinal);
        assert!(source.nodes[0].node.symbol.is_none());
        assert!(source.nodes[same_span].node.symbol.is_some());
        assert_eq!(source.nodes[same_span].parent, Some(0));
        assert_eq!(facts.sources[0].text.as_deref(), Some(original));
        let joined = PreparedQuery::parse(
            "ASK { ?node a :Node; :kind 'function_item'; :symbol ?symbol . ?symbol a :Symbol; :name 'kept' }",
            limits(),
        )?;
        assert!(joined.requirements().source_units.is_none());
        assert!(joined.requirements().requires(FactRequirement::Domain));
        assert!(
            !joined.requirements().selects_node_sources(),
            "a joined Symbol pattern must preserve the generic projection"
        );
        for (statement, expected) in [
            (
                "ASK { ?node a :Node; :kind 'function_item'; :symbol ?symbol . ?symbol a :Symbol; :name 'kept' }",
                true,
            ),
            (
                "ASK { ?node a :Node; :kind 'source_file'; :symbol ?symbol }",
                false,
            ),
        ] {
            assert_eq!(
                PreparedQuery::parse(statement, limits())?.evaluate(
                    facts.clone(),
                    limits(),
                    QueryCancellation::default()
                )?,
                QueryAnswer::Boolean { value: expected },
                "{statement}"
            );
        }
        requirements.source_units = Some(vec![SourceUnitId::parse(
            "rift://source/project/missing.rs",
        )?]);
        let missing = project_query_facts(&view, &requirements, limits())?;
        assert!(missing.nodes.is_empty());
        assert!(
            missing
                .unavailable_families
                .contains(&QueryFactFamily::Nodes)
        );
        let mut bound = limits();
        bound.quads_max = count - 1;
        requirements.source_units =
            Some(vec![SourceUnitId::parse("rift://source/project/lib.rs")?]);
        assert!(project_query_facts(&view, &requirements, bound).is_err());
        Ok(())
    }

    #[test]
    fn unit_bound_nodes_omit_unrelated_symbol_source_under_the_original_budget()
    -> Result<(), Box<dyn std::error::Error>> {
        let original = "pub fn kept() {}";
        let unrelated = format!(
            "pub fn unrelated() -> &'static str {{ \"{}\" }}",
            "x".repeat(32_768)
        );
        let root = tempfile::tempdir()?;
        for (path, source) in [
            (
                "Cargo.toml",
                "[package]\nname='beacon'\nversion='1.0.0'\n[lib]\npath='lib.rs'\n",
            ),
            ("lib.rs", original),
            ("unrelated.rs", unrelated.as_str()),
        ] {
            std::fs::write(root.path().join(path), source)?;
        }
        let index = WorkspaceIndex::build(
            root.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )?;
        let view = index.symbol_view()?;
        let unit = SourceUnitId::parse("rift://source/project/lib.rs")?;
        let requirements = FactRequirements {
            required: [FactRequirement::Nodes, FactRequirement::Source].into(),
            source_units: Some(vec![unit.clone()]),
            ..Default::default()
        };
        let mut generic = requirements.clone();
        generic.require(FactRequirement::Domain);
        generic.require(FactRequirement::Description);
        let complete = project_query_facts(&view, &generic, limits())?;
        assert!(
            complete
                .symbols
                .iter()
                .any(|symbol| symbol.symbol.name == "unrelated")
        );
        let symbol = complete
            .symbols
            .iter()
            .find(|symbol| symbol.symbol.name == "kept")
            .and_then(|symbol| symbol.symbol.id.clone())
            .ok_or("original canonical kept symbol")?;
        drop(index);
        std::fs::remove_file(root.path().join("lib.rs"))?;
        std::fs::remove_file(root.path().join("unrelated.rs"))?;
        let mut bounded = limits();
        bounded.rdf_bytes_max = 8192;
        bounded.material_bytes_max = Some(16_384);
        let facts = project_query_facts(&view, &requirements, bounded)?;
        assert!(
            facts.symbols.is_empty(),
            "unit-bound Node patterns require no Symbol payload"
        );
        assert_eq!(facts.sources.len(), 1);
        assert_eq!(facts.sources[0].unit, unit);
        assert_eq!(facts.sources[0].text.as_deref(), Some(original));
        assert_eq!(facts.nodes.len(), 1);
        let node = facts.nodes[0]
            .nodes
            .iter()
            .find(|row| row.node.symbol.as_ref() == Some(&symbol))
            .ok_or("Node keeps original canonical symbol without Symbol payload")?;
        assert_eq!(
            node.node.range,
            rift_protocol::read::TextRange {
                start: 0,
                end: u64::try_from(original.len())?
            }
        );
        assert!(node.node.id.0.contains('#'));
        let answer = PreparedQuery::parse("SELECT ?symbol { ?node a :Node; :unit <rift://source/project/lib.rs>; :symbol ?symbol }", bounded)?.evaluate(facts, bounded, QueryCancellation::default())?;
        let value = serde_json::to_value(answer)?;
        let rows = value["bindings"].as_array().ok_or("Node symbol bindings")?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["symbol"]["type"], "iri");
        assert_eq!(rows[0]["symbol"]["value"], symbol.as_str());
        assert!(
            project_query_facts(&view, &generic, bounded).is_err(),
            "the unchanged generic Symbol projection still accounts for unrelated source"
        );
        Ok(())
    }

    fn query_fixture(
        source: &str,
    ) -> Result<(tempfile::TempDir, WorkspaceIndex), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname='beacon'\nversion='1.0.0'\n[lib]\npath='lib.rs'\n",
        )?;
        std::fs::write(root.path().join("lib.rs"), source)?;
        let index = WorkspaceIndex::build(
            root.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )?;
        Ok((root, index))
    }

    fn answer_bag(answer: QueryAnswer) -> Result<serde_json::Value, serde_json::Error> {
        let mut value = serde_json::to_value(answer)?;
        if let Some(serde_json::Value::Array(rows)) = value.get_mut("bindings") {
            rows.sort_by_cached_key(serde_json::Value::to_string);
        }
        Ok(value)
    }

    #[test]
    fn selected_query_facts_match_complete_domain_for_general_queries()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_root, index) = query_fixture("pub fn kept() {}\npub fn other(kept: bool) {}\n")?;
        let view = index.symbol_view()?;
        let statements = [
            "SELECT ?name { ?s a :Symbol; :name 'kept'; :name ?name }",
            "SELECT ?name ?missing { ?s a :Symbol; :name 'kept'; :name ?name OPTIONAL { ?s :absent ?missing } }",
            "SELECT ?name { { ?s a :Symbol; :name 'kept' } UNION { ?s a :Symbol; :name 'other' } ?s :name ?name }",
            "SELECT ?name { ?s a :Symbol; :kind 'function'; :name ?name FILTER EXISTS { ?s :language 'rust' } }",
            "SELECT ?name { { SELECT ?s { ?s a :Symbol; :name 'kept' } } ?s :name ?name }",
            "SELECT (COUNT(?s) AS ?count) { ?s a :Symbol; :kind 'function'; :name ?name }",
            "SELECT ?name { VALUES ?wanted { 'kept' 'kept' } ?s a :Symbol; :name ?wanted; :name ?name }",
            "SELECT ?name { ?s a :Symbol; :kind 'function'; :name ?name MINUS { ?s :name 'other' } }",
            "ASK { ?s a :Symbol; :name 'kept' FILTER NOT EXISTS { ?s :name 'absent' } }",
            "SELECT ?source { ?s a :Symbol; :name 'kept'; :declarations ?declaration . ?declaration :source ?source }",
        ];
        for statement in statements {
            let prepared = PreparedQuery::parse(statement, limits())?;
            let selected = project_query_facts(&view, prepared.requirements(), limits())?;
            let mut complete_requirements = prepared.requirements().clone();
            complete_requirements.symbol_name = None;
            complete_requirements.symbol_ids = None;
            complete_requirements.adjacency.clear();
            let complete = project_query_facts(&view, &complete_requirements, limits())?;
            assert!(
                complete.symbols.len() >= selected.symbols.len(),
                "{statement}"
            );
            let selected = prepared.evaluate(selected, limits(), QueryCancellation::default())?;
            let complete = prepared.evaluate(complete, limits(), QueryCancellation::default())?;
            assert_eq!(answer_bag(selected)?, answer_bag(complete)?, "{statement}");
        }
        let guarded = PreparedQuery::parse("SELECT ?s { ?s a :Symbol; :name 'kept' }", limits())?;
        assert_eq!(guarded.requirements().symbol_name.as_deref(), Some("kept"));
        let selected = project_query_facts(&view, guarded.requirements(), limits())?;
        let complete = project_query_facts(&view, &FactRequirements::default(), limits())?;
        assert!(selected.symbols.len() < complete.symbols.len());
        let variable = PreparedQuery::parse(
            "SELECT ?predicate ?value { ?s ?predicate ?value }",
            limits(),
        )?;
        assert!(
            variable
                .requirements()
                .requires(FactRequirement::CompleteDomain)
        );
        assert!(variable.requirements().requires(FactRequirement::Nodes));
        for requirements in [variable.requirements(), &FactRequirements::default()] {
            let facts = project_query_facts(&view, requirements, limits())?;
            assert!(
                variable
                    .evaluate(facts, limits(), QueryCancellation::default())
                    .is_err()
            );
        }
        Ok(())
    }

    #[test]
    #[ignore = "manual query fact projection measurement"]
    fn query_facts_measure_existing_workspace() -> Result<(), Box<dyn std::error::Error>> {
        let root = std::env::var_os("RIFT_QUERY_MEASUREMENT_ROOT").ok_or("measurement root")?;
        let name = std::env::var("RIFT_QUERY_MEASUREMENT_NAME")?;
        let root = std::path::PathBuf::from(root);
        let index = WorkspaceIndex::build(
            &root,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )?;
        let view = index.symbol_view()?;
        let prepared = PreparedQuery::parse(
            &format!(
                "SELECT ?symbol {{ ?symbol a :Symbol; :name {} }}",
                serde_json::to_string(&name)?
            ),
            limits(),
        )?;
        let selected = project_query_facts(&view, prepared.requirements(), limits())?;
        let bytes = serde_json::to_vec(&selected.symbols)?.len();
        println!(
            "query_symbols={} query_symbol_bytes={bytes} captured_objects={} captured_sources={}",
            selected.symbols.len(),
            view.objects().count(),
            view.sources().count()
        );
        assert!(!selected.symbols.is_empty());
        assert!(selected.sources.is_empty());
        Ok(())
    }
}
