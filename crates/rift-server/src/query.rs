//! Immutable domain facts projected from the existing workspace publication.

use crate::QueryFactBudget;
use crate::read::{LocalSymbolViewInput, ReadService};
use rift_error::{RiftError, errors};
use rift_index::WorkspaceSymbolView;
use rift_protocol::read::SourceUnitId;
use rift_search::query::{
    FactRequirements, QueryFactFamily, QueryFacts, QueryLimits, QuerySource, QuerySymbol,
};

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
        project_query_facts(
            &view,
            &FactRequirements {
                source,
                complete_domain: relationships,
                ..FactRequirements::default()
            },
            limits,
        )
    }
}

impl LocalSymbolViewInput {
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
        if requirements.domain && !self.ready {
            return errors::server::read_unavailable()
                .operation("query acquisition")
                .detail("selected workspace publication is not ready")
                .fail();
        }
        let mut budget = QueryFactBudget::new(limits, cancelled)?;
        let mut facts =
            project_query_facts_with_budget(self.index(), requirements, limits, &mut budget)?;
        budget.check()?;
        if let Some(bytes) = self.captured_history_bytes() {
            budget.charge_bytes(bytes)?;
        }
        if let Some(history) = self.captured_history()? {
            if history.complete
                && !requirements.all_histories
                && !requirements.history_symbols.is_empty()
                && requirements
                    .history_symbols
                    .iter()
                    .all(|identity| *identity == history.symbol)
            {
                facts
                    .unavailable_families
                    .retain(|family| *family != QueryFactFamily::History);
            }
            facts.histories.push(history);
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
            if requirements.source {
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
        if requirements.complete_domain || requirements.description {
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
    if requirements.symbol_ids.is_none() {
        for declaration in view.unestablished_declarations() {
            budget.check()?;
            let (object, matched, declaration) = declaration?;
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
            budget.charge(declaration)?;
            let symbol = object.to_protocol_symbol();
            budget.charge(&symbol)?;
            let mut declaration = declaration.clone();
            if requirements.source {
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
    let mut source_bytes = 0usize;
    for (path, source) in view.sources() {
        budget.check()?;
        if requirements.symbol_name.is_some() || requirements.symbol_ids.is_some() {
            break;
        }
        if facts.sources.len() >= limits.quads_max {
            return query_bound("search.query.statements");
        }
        if requirements.source {
            source_bytes = source_bytes
                .checked_add(source.len())
                .ok_or_else(|| query_bound_error("search.query.bytes"))?;
            if source_bytes > limits.rdf_bytes_max {
                return query_bound("search.query.bytes");
            }
        }
        if requirements.source {
            budget.charge(&source)?;
        }
        let mut record = QuerySource {
            unit: SourceUnitId(rift_syntax::source_unit_for_path(path)?.to_string()),
            digest: Some(rift_protocol::identity::SourceDigest::parse(
                &data_encoding::HEXLOWER
                    .encode(rift_core::FileDigest::of(source.as_bytes()).as_bytes()),
            )?),
            text: None,
        };
        budget.charge(&record)?;
        record.text = requirements.source.then(|| source.to_owned());
        facts.sources.push(record);
    }
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
    Ok(facts)
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
        }
    }

    #[test]
    fn captured_query_facts_preserve_selected_source_and_narrow_names()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname='beacon'\nversion='1.0.0'\n[lib]\npath='lib.rs'\n",
        )?;
        let original = "/// Original.\npub fn kept() {}\npub fn other() {}\n";
        std::fs::write(root.path().join("lib.rs"), original)?;
        let index = WorkspaceIndex::build(
            root.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )?;
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
                source: true,
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
