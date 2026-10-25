//! Original normalized associations retained by one captured local selection.

use get_size2::GetSize as _;
use std::collections::{BTreeMap, BTreeSet};

use rift_core::{ContributionKey, ProjectPath, SymbolRecord};
use rift_error::{RiftError, errors};
use rift_index::{ReadableSymbol, WorkspaceSymbolView};
use rift_protocol::identity::{SourceDigest, SymbolIdentity, parse_source_unit_identity};
use rift_protocol::read::SourceUnitId;
use rift_protocol::symbol_read::SymbolDeclaration;
use rift_provider::NormalizedTarget;
use rift_search::query::{
    FactRequirement, FactRequirements, QueryFacts, QueryLimits, QueryProviderContext,
    QueryProviderDeclaration, QueryProviderInput, QueryProviderRecord, QueryProviderReference,
    QueryProviderRelationship, QueryProviderSelection, QueryProviderSnapshot, QuerySource,
};

use super::QueryFactBudget;

struct Group<'view> {
    record: &'view SymbolRecord,
    symbols: BTreeSet<usize>,
    container: Option<NormalizedTarget>,
}

type Binding = (String, u64, u64);

pub(super) fn capture(
    view: &WorkspaceSymbolView,
    selection: &QueryProviderSelection,
    requirements: &FactRequirements,
    facts: &mut QueryFacts,
    limits: QueryLimits,
    budget: &mut QueryFactBudget<'_>,
) -> Result<(), RiftError> {
    if facts
        .symbols
        .iter()
        .all(|symbol| symbol.symbol.id.is_some())
    {
        return Ok(());
    }
    let groups = selected_groups(view, facts, budget)?;
    let (records, units) = capture_records(view, facts, &groups, budget)?;
    let source_indices = capture_sources(view, selection, requirements, facts, units, budget)?;
    let context = capture_context(selection, facts, &source_indices, budget)?;
    let mut snapshot = QueryProviderSnapshot {
        context,
        records,
        source_indices,
        references: Vec::new(),
        relationships: Vec::new(),
    };
    capture_topology(view, requirements, &mut snapshot, limits, budget)?;
    facts.providers.push(snapshot);
    facts.validate_providers()
}

fn selected_groups<'view>(
    view: &'view WorkspaceSymbolView,
    facts: &QueryFacts,
    budget: &mut QueryFactBudget<'_>,
) -> Result<BTreeMap<&'view [ContributionKey], Group<'view>>, RiftError> {
    let mut physical = BTreeMap::<Binding, Vec<usize>>::new();
    let mut canonical = BTreeMap::new();
    for (index, symbol) in facts.symbols.iter().enumerate() {
        budget.check()?;
        if let Some(identity) = &symbol.symbol.id {
            canonical.insert(identity.as_str(), index);
        } else {
            for declaration in &symbol.declarations {
                let key = declaration_binding(declaration)?;
                physical.entry(key).or_default().push(index);
            }
        }
    }
    let mut groups = BTreeMap::<&[ContributionKey], Group<'_>>::new();
    for (identity, object) in view.objects() {
        budget.check()?;
        let Some(index) = canonical.get(identity.as_str()) else {
            continue;
        };
        associate(view, &mut groups, object?, *index)?;
    }
    for declaration in view.unestablished_declarations() {
        budget.check()?;
        let (object, _, declaration) = declaration?;
        let Some(indices) = physical.get(&declaration_binding(declaration)?) else {
            continue;
        };
        for index in indices {
            associate(view, &mut groups, &object, *index)?;
        }
    }
    Ok(groups)
}

fn capture_records(
    view: &WorkspaceSymbolView,
    facts: &mut QueryFacts,
    groups: &BTreeMap<&[ContributionKey], Group<'_>>,
    budget: &mut QueryFactBudget<'_>,
) -> Result<(Vec<QueryProviderRecord>, BTreeSet<String>), RiftError> {
    let remap = consolidate(facts, groups)?;
    let mut records = Vec::new();
    let mut units = BTreeSet::new();
    let graph = view.normalized_graph();
    for group in groups.values() {
        budget.check()?;
        let index = *group.symbols.first().ok_or_else(missing)?;
        let index = remap[index];
        let symbol = facts.symbols.get(index).ok_or_else(missing)?;
        let mut declarations = Vec::new();
        for key in group.record.contributions() {
            let Some(binding) = graph
                .contribution(key)
                .and_then(|contribution| contribution.source())
            else {
                continue;
            };
            let unit = binding.unit().to_string();
            for (declaration_index, declaration) in symbol.declarations.iter().enumerate() {
                if declaration_binding(declaration)?
                    == (unit.clone(), binding.range().start(), binding.range().end())
                    && declaration.node.as_ref() == binding.node()
                {
                    budget.charge_material(key.get_size())?;
                    budget.charge_material(binding.get_size())?;
                    budget.charge_material(std::mem::size_of::<usize>())?;
                    declarations.push(QueryProviderDeclaration {
                        declaration_index,
                        contribution: key.clone(),
                        binding: binding.clone(),
                    });
                    units.insert(unit.clone());
                }
            }
        }
        if declarations.is_empty() {
            if symbol.symbol.id.is_none() {
                return Err(missing());
            }
            continue;
        }
        budget.charge_material(group.record.get_size())?;
        budget.charge_material(group.container.get_size())?;
        records.push(QueryProviderRecord {
            symbol_index: index,
            identity: group.record.identity().cloned(),
            contributions: group.record.contributions().to_vec(),
            declarations,
            container: group.container.clone(),
        });
    }
    if records.is_empty() {
        return Err(missing());
    }
    Ok((records, units))
}

fn capture_sources(
    view: &WorkspaceSymbolView,
    selection: &QueryProviderSelection,
    requirements: &FactRequirements,
    facts: &mut QueryFacts,
    units: BTreeSet<String>,
    budget: &mut QueryFactBudget<'_>,
) -> Result<Vec<usize>, RiftError> {
    let mut source_indices = Vec::new();
    for unit in units {
        budget.check()?;
        if let Some(index) = facts
            .sources
            .iter()
            .position(|source| source.unit.as_str() == unit)
        {
            source_indices.push(index);
            continue;
        }
        let (owner, path) = parse_source_unit_identity(&unit)?;
        if owner
            .as_ref()
            .is_some_and(|owner| owner != &selection.owner)
        {
            return Err(missing());
        }
        let text = view.source(&ProjectPath::new(path)?).ok_or_else(missing)?;
        let digest = SourceDigest::parse(
            &data_encoding::HEXLOWER.encode(rift_core::FileDigest::of(text.as_bytes()).as_bytes()),
        )?;
        let mut record = QuerySource {
            unit: SourceUnitId(unit),
            digest: Some(digest),
            text: None,
        };
        if requirements.requires(FactRequirement::Source) {
            budget.charge(&text)?;
            record.text = Some(text.to_owned());
        }
        budget.charge(&record)?;
        source_indices.push(facts.sources.len());
        facts.sources.push(record);
    }
    Ok(source_indices)
}

fn capture_context(
    selection: &QueryProviderSelection,
    facts: &QueryFacts,
    source_indices: &[usize],
    budget: &mut QueryFactBudget<'_>,
) -> Result<QueryProviderContext, RiftError> {
    let source_digest = source_indices
        .iter()
        .find_map(|index| facts.sources[*index].digest.as_ref())
        .cloned()
        .ok_or_else(missing)?;
    budget.charge_material(selection.get_size())?;
    budget.charge_material(selection.root.get_size())?;
    budget.charge_material(selection.owner.get_size())?;
    budget.charge_material(source_digest.get_size())?;
    let mut context = QueryProviderContext {
        selection: selection.clone(),
        root: selection.root.clone(),
        owner: selection.owner.clone(),
        index_revision: selection.index_revision,
        source_revision: selection.source_revision,
        tree_revision: selection.tree_revision,
        source_digest,
        inputs: Vec::new(),
    };
    for index in source_indices {
        let source = &facts.sources[*index];
        let digest = source.digest.as_ref().ok_or_else(missing)?;
        budget.charge(&source.unit)?;
        budget.charge(digest)?;
        context.inputs.push(QueryProviderInput {
            uri: source.unit.as_str().to_owned(),
            digest: digest.clone(),
        });
    }
    Ok(context)
}

fn capture_topology(
    view: &WorkspaceSymbolView,
    requirements: &FactRequirements,
    snapshot: &mut QueryProviderSnapshot,
    limits: QueryLimits,
    budget: &mut QueryFactBudget<'_>,
) -> Result<(), RiftError> {
    let graph = view.normalized_graph();
    if requirements.requires(FactRequirement::CompleteDomain)
        || requirements.requires(FactRequirement::Description)
        || !requirements.facets.is_empty()
    {
        let supplied = snapshot
            .records
            .iter()
            .flat_map(|record| record.contributions.iter())
            .collect::<BTreeSet<_>>();
        let established = |target: &NormalizedTarget| match target {
            NormalizedTarget::Symbol(identity) => SymbolIdentity::parse(identity.as_str()).is_ok(),
            NormalizedTarget::Contribution(reference) => graph
                .identity_for(reference)
                .is_some_and(|identity| SymbolIdentity::parse(identity.as_str()).is_ok()),
        };
        for reference in graph.references() {
            budget.check()?;
            if !supplied.contains(reference.source()) {
                continue;
            }
            let source = graph
                .identity_for(reference.source().reference())
                .is_some_and(|identity| SymbolIdentity::parse(identity.as_str()).is_ok());
            let targets = reference
                .targets()
                .iter()
                .filter(|target| !source || !established(target))
                .collect::<Vec<_>>();
            if targets.is_empty() {
                continue;
            }
            if snapshot.references.len() >= limits.quads_max {
                return super::query_bound("search.query.statements");
            }
            budget.charge_material(reference.get_size())?;
            snapshot.references.push(QueryProviderReference {
                source: reference.source().clone(),
                binding: reference.binding().clone(),
                role: reference.role(),
                facet: rift_analysis::role_facet(reference.role()),
                targets: targets.into_iter().cloned().collect(),
            });
        }
        for relationship in graph.relationships() {
            budget.check()?;
            if !supplied.contains(relationship.source()) {
                continue;
            }
            let source = graph
                .identity_for(relationship.source().reference())
                .is_some_and(|identity| SymbolIdentity::parse(identity.as_str()).is_ok());
            if source && established(relationship.target()) {
                continue;
            }
            if snapshot.relationships.len() >= limits.quads_max {
                return super::query_bound("search.query.statements");
            }
            budget.charge_material(relationship.get_size())?;
            snapshot.relationships.push(QueryProviderRelationship {
                source: relationship.source().clone(),
                kind: relationship.kind(),
                facet: rift_analysis::relationship_facet(relationship.kind()),
                target: relationship.target().clone(),
                derivation: relationship.derivation(),
                occurrence: relationship.occurrence().cloned(),
            });
        }
    }
    Ok(())
}

fn associate<'view>(
    view: &'view WorkspaceSymbolView,
    groups: &mut BTreeMap<&'view [ContributionKey], Group<'view>>,
    object: &ReadableSymbol,
    index: usize,
) -> Result<(), RiftError> {
    let keys = object.assembled().contributions();
    let Some(first) = keys.first() else {
        return Ok(());
    };
    let record = view
        .normalized_graph()
        .record_for(first.reference())
        .ok_or_else(missing)?;
    if keys.len() != record.contributions().len()
        || keys.iter().any(|key| !record.contributions().contains(key))
    {
        return Err(missing());
    }
    let container = object
        .facts()
        .container_reference()
        .cloned()
        .map(NormalizedTarget::Contribution);
    let group = groups
        .entry(record.contributions())
        .or_insert_with(|| Group {
            record,
            symbols: BTreeSet::new(),
            container: container.clone(),
        });
    if group.container != container {
        return Err(missing());
    }
    group.symbols.insert(index);
    Ok(())
}

fn declaration_binding(declaration: &SymbolDeclaration) -> Result<Binding, RiftError> {
    let unit = match (&declaration.unit, &declaration.path) {
        (Some(unit), None) => unit.as_str().to_owned(),
        (None, Some(path)) => {
            rift_syntax::source_unit_for_path(&ProjectPath::new(path.0.as_str())?)?.to_string()
        }
        _ => return Err(missing()),
    };
    Ok((unit, declaration.range.start, declaration.range.end))
}

fn consolidate(
    facts: &mut QueryFacts,
    groups: &BTreeMap<&[ContributionKey], Group<'_>>,
) -> Result<Vec<usize>, RiftError> {
    let mut destinations = (0..facts.symbols.len()).collect::<Vec<_>>();
    let mut assigned = BTreeMap::new();
    for (keys, group) in groups {
        let first = *group.symbols.first().ok_or_else(missing)?;
        for index in &group.symbols {
            if assigned
                .insert(*index, *keys)
                .is_some_and(|previous| previous != *keys)
            {
                return Err(missing());
            }
            destinations[*index] = first;
        }
    }
    for (index, first) in destinations.iter().copied().enumerate() {
        if first == index {
            continue;
        }
        if first > index {
            return Err(missing());
        }
        let (before, after) = facts.symbols.split_at_mut(index);
        let original = &mut before[first];
        let added = &mut after[0];
        if original.symbol != added.symbol {
            return Err(missing());
        }
        for declaration in std::mem::take(&mut added.declarations) {
            if !original.declarations.contains(&declaration) {
                original.declarations.push(declaration);
            }
        }
    }
    let mut next = 0usize;
    let mut index = 0usize;
    let mut remap = vec![0; destinations.len()];
    facts.symbols.retain(|_| {
        let first = destinations[index];
        let keep = first == index;
        if keep {
            remap[index] = next;
            next += 1;
        } else {
            remap[index] = remap[first];
        }
        index += 1;
        keep
    });
    Ok(remap)
}

fn missing() -> RiftError {
    errors::server::read_unavailable()
        .operation("query acquisition")
        .detail("captured declaration has no original provider association")
        .error()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_core::{SourceVisibility, TextFileInclusion, WorkspaceId};
    use rift_index::{WorkspaceIndex, WorkspaceIndexLimits};
    use rift_protocol::query::{QueryAnswer, RdfTerm};
    use rift_search::sparql::{PreparedQuery, QueryCancellation};

    #[test]
    fn captured_python_provider_positions_use_original_declaration_bindings()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let text = "def leaf():\n    return 1\n\ndef root():\n    return leaf()\n";
        std::fs::write(root.path().join("graph.py"), text)?;
        let index = WorkspaceIndex::build(
            root.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )?;
        let view = index.symbol_view()?;
        let graph = view.normalized_graph();
        let selection = QueryProviderSelection {
            root: WorkspaceId::new(
                rift_lsp::uri::TreeRoot::new(root.path())?
                    .root_uri()?
                    .as_str(),
            )?,
            owner: view.owner().clone(),
            index_revision: graph.index_revision(),
            source_revision: graph.source_revision(),
            tree_revision: graph.tree_revision(),
            fingerprint: SourceDigest::parse(
                &data_encoding::HEXLOWER.encode(view.fingerprint().as_bytes()),
            )?,
            configuration: QueryProviderSelection::configuration_digest(
                &rift_protocol::configuration::WorkspaceConfiguration::default(),
            )?,
        };
        let limits = QueryLimits::try_from(&rift_protocol::query::QueryConfiguration::default())?;
        let requirements = FactRequirements {
            required: [
                FactRequirement::Domain,
                FactRequirement::CompleteDomain,
                FactRequirement::Source,
            ]
            .into(),
            ..FactRequirements::default()
        };
        let mut budget = QueryFactBudget::new(limits, &|| false)?;
        let mut facts = super::super::project_query_facts_with_budget(
            &view,
            &requirements,
            limits,
            &mut budget,
        )?;
        capture(
            &view,
            &selection,
            &requirements,
            &mut facts,
            limits,
            &mut budget,
        )?;
        let file = index
            .file(&ProjectPath::new("graph.py")?)
            .ok_or("original Python source")?;
        let mut names = BTreeSet::new();
        for record in facts
            .providers
            .iter()
            .flat_map(|provider| &provider.records)
        {
            let symbol = &facts.symbols[record.symbol_index].symbol;
            for declaration in &record.declarations {
                let binding = &declaration.binding;
                let syntax = file.syntax().symbols().iter().find(|symbol| {
                    symbol.item_range.start == binding.range().start()
                        && symbol.item_range.end == binding.range().end()
                });
                let position = syntax.and_then(|symbol| symbol.name_range.as_ref());
                eprintln!(
                    "captured Python declaration {:?} {:?} {}..{} name {:?}",
                    symbol.name,
                    symbol.kind,
                    binding.range().start(),
                    binding.range().end(),
                    position
                );
                if ["root", "leaf"].contains(&symbol.name.as_str()) {
                    assert!(
                        position.is_some(),
                        "function binding keeps its original name position"
                    );
                    names.insert(symbol.name.as_str());
                }
            }
        }
        assert_eq!(names, BTreeSet::from(["leaf", "root"]));
        Ok(())
    }

    fn captured_selection(
        view: &WorkspaceSymbolView,
        root: &std::path::Path,
    ) -> Result<QueryProviderSelection, Box<dyn std::error::Error>> {
        let graph = view.normalized_graph();
        let selection = QueryProviderSelection {
            root: WorkspaceId::new(rift_lsp::uri::TreeRoot::new(root)?.root_uri()?.as_str())?,
            owner: view.owner().clone(),
            index_revision: graph.index_revision(),
            source_revision: graph.source_revision(),
            tree_revision: graph.tree_revision(),
            fingerprint: SourceDigest::parse(
                &data_encoding::HEXLOWER.encode(view.fingerprint().as_bytes()),
            )?,
            configuration: QueryProviderSelection::configuration_digest(
                &rift_protocol::configuration::WorkspaceConfiguration::default(),
            )?,
        };
        Ok(selection)
    }

    #[test]
    fn captured_local_provider_keeps_physical_binding_and_context()
    -> Result<(), Box<dyn std::error::Error>> {
        let limits = QueryLimits::try_from(&rift_protocol::query::QueryConfiguration::default())?;
        let mut answers = Vec::new();
        let mut keys = Vec::new();
        for text in [
            "int beacon(int value) { return value; }\n",
            "int beacon(int value) { return value+0; }\n",
        ] {
            let root = tempfile::tempdir()?;
            std::fs::write(root.path().join("beacon.c"), text)?;
            let index = WorkspaceIndex::build(
                root.path(),
                WorkspaceIndexLimits::default(),
                &SourceVisibility::default(),
                &TextFileInclusion::default(),
            )?;
            let view = index.symbol_view()?;
            let selection = captured_selection(&view, root.path())?;
            let query = PreparedQuery::parse(
                "SELECT ?symbol ?unit ?start ?end { ?symbol a :Symbol; :name 'beacon'; :declarations ?declaration . ?declaration :unit ?unit; :range ?range . ?range :start ?start; :end ?end }",
                limits,
            )?;
            let requirements = FactRequirements {
                required: [FactRequirement::Source].into(),
                ..query.requirements().clone()
            };
            let mut budget = QueryFactBudget::new(limits, &|| false)?;
            let mut facts = super::super::project_query_facts_with_budget(
                &view,
                &requirements,
                limits,
                &mut budget,
            )?;
            assert!(
                facts
                    .symbols
                    .iter()
                    .any(|symbol| symbol.symbol.id.is_none() && symbol.symbol.name == "beacon")
            );
            capture(
                &view,
                &selection,
                &requirements,
                &mut facts,
                limits,
                &mut budget,
            )?;
            facts.validate_providers()?;
            let record = facts
                .providers
                .iter()
                .flat_map(|snapshot| &snapshot.records)
                .find(|record| facts.symbols[record.symbol_index].symbol.name == "beacon")
                .ok_or("captured beacon")?;
            keys.push(record.contributions[0].reference().clone());
            let original = facts
                .sources
                .iter()
                .find(|source| source.unit.as_str() == "rift://source/project/beacon.c")
                .ok_or("captured source")?;
            assert_eq!(original.text.as_deref(), Some(text));
            std::fs::write(
                root.path().join("beacon.c"),
                "int changed(void) { return 0; }\n",
            )?;
            assert_eq!(original.text.as_deref(), Some(text));
            let QueryAnswer::Bindings { bindings, .. } =
                query.evaluate(facts, limits, QueryCancellation::default())?
            else {
                panic!("SELECT bindings");
            };
            assert_eq!(bindings.len(), 1);
            assert!(matches!(
                bindings[0].get("symbol"),
                Some(RdfTerm::BlankNode { .. })
            ));
            assert!(
                matches!(bindings[0].get("unit"), Some(RdfTerm::Iri { value }) if value == "rift://source/project/beacon.c")
            );
            answers.push(bindings[0].get("symbol").cloned());
        }
        assert_eq!(
            keys[0], keys[1],
            "same provider-local identity remains scoped to its original context"
        );
        assert_ne!(
            answers[0], answers[1],
            "different captured roots must not share physical RDF identity"
        );
        Ok(())
    }
}
