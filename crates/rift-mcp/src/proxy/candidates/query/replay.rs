//! Retained evaluations use their original admitted facts and recorded provider views.

use rift_error::{RiftError, errors};
use rift_protocol::read::SymbolId;
use rift_search::sparql::{FactRequirements, QueryIdentities};

use super::retained::{self, Metadata};
use crate::server::QueryFactSelection;

type Body = (String, QueryFactSelection, QueryIdentities);
type PreparedBody = (
    String,
    QueryFactSelection,
    rift_search::sparql::QueryServices,
    Option<QueryIdentities>,
);
type ServiceGroup = (rift_server::CapturedQueryView, Vec<(String, Option<u64>)>);

pub(crate) fn conflicting_local_graphs<'selection>(
    selected: impl Iterator<Item = &'selection QueryFactSelection>,
) -> bool {
    selected
        .filter(|body| body.graph.as_deref() == Some(rift_search::sparql::LOCAL_GRAPH))
        .count()
        > 1
}

pub(crate) fn restore(
    captured: &rift_server::CapturedQueryView,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<(Metadata, Vec<Body>), RiftError> {
    budget.charge_bytes(captured.material().metadata())?;
    let mut metadata = retained::metadata(captured)?;
    budget.charge_bytes(captured.material().facts())?;
    let mut bodies: Vec<Body> =
        serde_json::from_slice(captured.material().facts()).map_err(encoding_failure)?;
    if bodies.is_empty() || bodies.len() != metadata.providers.len() {
        return Err(retained::unavailable());
    }
    let provider_keys = retained::provider_keys(&metadata.providers, budget)?;
    for ((backend, selected, identities), provider) in
        bodies.iter_mut().zip(&mut metadata.providers)
    {
        budget.check()?;
        if backend != &provider.backend
            || selected.root != provider.root
            || selected.owner != provider.owner
            || selected.view != provider.view
            || selected.selection != provider.selection
            || !identities.symbols.is_empty()
            || !selected.upstream.is_empty()
        {
            return Err(retained::unavailable());
        }
        selected.validate_coverage(&selected.requirements)?;
        identities.symbols = std::mem::take(&mut provider.symbols);
        selected.upstream = std::mem::take(&mut provider.upstream);
        selected.requests = 0;
    }
    restore_sources(
        &mut bodies,
        &metadata,
        captured.material().sources(),
        &provider_keys,
        budget,
    )?;
    Ok((metadata, bodies))
}

fn restore_sources(
    bodies: &mut [Body],
    metadata: &Metadata,
    bytes: &[u8],
    provider_keys: &[rift_protocol::identity::SourceDigest],
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<(), RiftError> {
    for (provider, (_, body, identities)) in bodies.iter_mut().enumerate() {
        for source in &mut body.facts.sources {
            budget.check()?;
            let public = identities
                .sources
                .iter()
                .find(|(original, _)| original == &source.unit)
                .map_or(&source.unit, |(_, public)| public);
            let index = metadata
                .sources
                .binary_search_by(|saved| saved.public.as_str().cmp(public.as_str()))
                .map_err(|_| retained::unavailable())?;
            let saved = &metadata.sources[index];
            if saved.original != source.unit
                || source.text.is_some()
                || provider_keys
                    .get(saved.provider)
                    .zip(provider_keys.get(provider))
                    .is_none_or(|(left, right)| left != right)
            {
                return Err(retained::unavailable());
            }
            rift_search::query::require_source_agreement(
                source.digest.as_ref(),
                None,
                saved.digest.as_ref(),
                None,
            )?;
            if let Some(range) = &saved.bytes {
                let text = bytes.get(range.clone()).ok_or_else(retained::unavailable)?;
                budget.charge_bytes(text)?;
                let text = std::str::from_utf8(text).map_err(|_| retained::unavailable())?;
                source.text = Some(text.to_owned());
            }
            let digest = rift_search::query::source_digest(source)?;
            if digest != saved.digest {
                return Err(retained::unavailable());
            }
        }
    }
    for saved in &metadata.sources {
        if saved.provider >= bodies.len() {
            return Err(retained::unavailable());
        }
    }
    Ok(())
}

pub(crate) fn retained_answer_only(requested: &FactRequirements) -> bool {
    requested.required.is_empty()
        && requested.source_units.is_none()
        && requested.source_reads.is_empty()
        && requested.symbol_name.is_none()
        && requested.symbol_ids.is_none()
        && requested.occurrences.is_empty()
        && requested.adjacency.is_empty()
        && requested.history_symbols.is_empty()
        && requested.facets.is_empty()
        && requested.datasets.is_empty()
        && requested.retrievals.is_empty()
        && !requested.services.is_empty()
        && requested
            .services
            .iter()
            .all(|service| service.starts_with(rift_search::sparql::RESULT_SERVICE_PREFIX))
}

pub(crate) fn selected_node_providers(
    metadata: &Metadata,
    requested: &FactRequirements,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<Option<Vec<bool>>, RiftError> {
    if !requested.selects_node_sources() {
        return Ok(None);
    }
    if metadata
        .sources
        .windows(2)
        .any(|pair| pair[0].public >= pair[1].public)
    {
        return Err(retained::unavailable());
    }
    budget.charge_material(
        metadata
            .providers
            .len()
            .checked_mul(size_of::<bool>())
            .ok_or_else(retained::unavailable)?,
    )?;
    let mut selected = Vec::new();
    selected
        .try_reserve_exact(metadata.providers.len())
        .map_err(|_| retained::unavailable())?;
    selected.resize(metadata.providers.len(), false);
    for unit in requested
        .source_units
        .as_deref()
        .ok_or_else(retained::unavailable)?
    {
        budget.check()?;
        let index = metadata
            .sources
            .binary_search_by(|source| source.public.as_str().cmp(unit.as_str()))
            .map_err(|_| retained::unavailable())?;
        let provider = selected
            .get_mut(metadata.sources[index].provider)
            .ok_or_else(retained::unavailable)?;
        *provider = true;
    }
    Ok(Some(selected))
}

pub(crate) fn include_empty_node_domain(
    services: &mut rift_search::sparql::QueryServices,
    body: &QueryFactSelection,
    identities: &QueryIdentities,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<(), RiftError> {
    budget.charge(identities)?;
    budget.charge(&body.graph)?;
    budget.charge_material(size_of::<rift_search::sparql::QueryDataset>())?;
    services
        .datasets
        .try_reserve_exact(1)
        .map_err(|_| retained::unavailable())?;
    services.datasets.push(rift_search::sparql::QueryDataset {
        graph: body.graph.clone(),
        facts: rift_search::sparql::QueryFacts::default(),
        identities: identities.clone(),
        default: true,
    });
    Ok(())
}

#[cfg(test)]
pub(crate) fn covers_requested(
    bodies: &[Body],
    requested: &FactRequirements,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<bool, RiftError> {
    covers_requested_sources(bodies, requested, None, budget)
}

fn covers_requested_sources(
    bodies: &[Body],
    requested: &FactRequirements,
    selected_sources: Option<&[bool]>,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<bool, RiftError> {
    if let Some(selected) = selected_sources
        && (!requested.selects_node_sources()
            || selected.len() != bodies.len()
            || !selected.iter().any(|selected| *selected))
    {
        return Err(retained::unavailable());
    }
    if selected_sources.is_none() && !requested_seeds_present(bodies, requested, budget)? {
        return Ok(false);
    }
    for (index, (_, body, identities)) in bodies.iter().enumerate() {
        if selected_sources.is_some_and(|selected| !selected.get(index).copied().unwrap_or(false)) {
            continue;
        }
        budget.charge(requested)?;
        let mut translated = translate(requested.clone(), identities);
        if !requested.source_reads.is_empty() {
            translated
                .services
                .retain(|service| service != rift_search::sparql::SOURCE_SERVICE);
        }
        if selected_sources.is_none()
            && requested.selects_node_sources()
            && translated.source_units.as_ref().map(Vec::len)
                != requested.source_units.as_ref().map(Vec::len)
        {
            return Ok(false);
        }
        if !body.covers(&translated)
            && (requested.source_reads.is_empty() || !retained_answer_only(&translated))
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn requested_seeds_present(
    bodies: &[Body],
    requested: &FactRequirements,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<bool, RiftError> {
    if let Some(units) = &requested.source_units {
        for unit in units {
            budget.check()?;
            let mut matches = bodies
                .iter()
                .flat_map(|(_, _, identities)| &identities.sources)
                .filter(|(_, public)| public == unit);
            if matches.next().is_none() || matches.next().is_some() {
                return Ok(false);
            }
        }
    }
    let seeds = requested
        .symbol_ids
        .iter()
        .flatten()
        .chain(&requested.occurrences)
        .chain(&requested.history_symbols)
        .chain(requested.adjacency.iter().map(|selected| &selected.seed));
    for seed in seeds {
        budget.check()?;
        if !bodies
            .iter()
            .any(|(_, _, identities)| identities.symbols.iter().any(|(_, public)| public == seed))
        {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
pub(crate) fn covers_retained_bindings(
    bodies: &[Body],
    requested: &FactRequirements,
    services: &rift_search::sparql::QueryServices,
    selection: Option<&rift_search::sparql::RetainedSubjectSelection>,
    limits: rift_search::sparql::QueryLimits,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<bool, RiftError> {
    covers_retained_source_bindings(bodies, requested, services, selection, limits, None, budget)
}

pub(crate) fn covers_retained_source_bindings(
    bodies: &[Body],
    requested: &FactRequirements,
    services: &rift_search::sparql::QueryServices,
    selection: Option<&rift_search::sparql::RetainedSubjectSelection>,
    limits: rift_search::sparql::QueryLimits,
    selected_sources: Option<&[bool]>,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<bool, RiftError> {
    if covers_requested_sources(bodies, requested, selected_sources, budget)? {
        return Ok(true);
    }
    if selected_sources.is_some() || requested.selects_node_sources() {
        return Ok(false);
    }
    let Some(selection) = selection else {
        return Ok(false);
    };
    let mut matching = services
        .bindings
        .iter()
        .filter(|result| result.service == selection.service());
    let Some(result) = matching.next() else {
        return Ok(false);
    };
    if matching.next().is_some() || !requested_seeds_present(bodies, requested, budget)? {
        return Ok(false);
    }
    let rift_protocol::query::QueryAnswer::Bindings {
        variables,
        bindings,
    } = &result.answer
    else {
        return Ok(false);
    };
    if variables
        .iter()
        .filter(|variable| variable.as_str() == selection.variable())
        .count()
        != 1
    {
        return Ok(false);
    }
    let mut resources = original_resources(bodies, limits, budget)?;
    if !select_original_rows(&mut resources, bindings, selection.variable(), budget)? {
        return Ok(false);
    }
    for ((_, body, identities), (_, selected)) in bodies.iter().zip(resources) {
        budget.check()?;
        budget.charge(requested)?;
        let requested = translate(requested.clone(), identities);
        if !body.covers_original_subjects(&requested, &selected) {
            return Ok(false);
        }
    }
    Ok(true)
}

type OriginalResources = (Vec<rift_search::sparql::QuerySubjectResource>, Vec<usize>);

fn original_resources(
    bodies: &[Body],
    limits: rift_search::sparql::QueryLimits,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<Vec<OriginalResources>, RiftError> {
    let mut original = Vec::new();
    budget.charge_material(
        bodies
            .len()
            .checked_mul(size_of::<OriginalResources>())
            .ok_or_else(retained::unavailable)?,
    )?;
    original
        .try_reserve_exact(bodies.len())
        .map_err(|_| retained::unavailable())?;
    let cancellation = rift_search::sparql::QueryCancellation::default();
    for (_, body, identities) in bodies {
        budget.check()?;
        budget.charge(identities)?;
        let mut resources = rift_search::sparql::retained_subject_resources(
            &body.facts,
            identities.clone(),
            &body.subjects,
            limits,
            &cancellation,
            &mut |bytes| {
                budget.check()?;
                budget.charge_material(bytes)
            },
        )?;
        budget.check()?;
        resources.sort_unstable_by(|left, right| {
            resource_key(&left.resource).cmp(&resource_key(&right.resource))
        });
        original.push((resources, Vec::new()));
    }
    Ok(original)
}

fn select_original_rows(
    resources: &mut [OriginalResources],
    rows: &[std::collections::BTreeMap<String, rift_protocol::query::RdfTerm>],
    variable: &str,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<bool, RiftError> {
    for row in rows {
        budget.check()?;
        let Some(resource) = row.get(variable).and_then(resource_key) else {
            return Ok(false);
        };
        let mut selected = None;
        for (owner, (known, _)) in resources.iter().enumerate() {
            budget.check()?;
            let Ok(index) =
                known.binary_search_by(|known| resource_key(&known.resource).cmp(&Some(resource)))
            else {
                continue;
            };
            let matched = &known[index];
            if !matched.original
                || selected.is_some()
                || index.checked_sub(1).is_some_and(|previous| {
                    resource_key(&known[previous].resource) == Some(resource)
                })
                || known
                    .get(index + 1)
                    .is_some_and(|next| resource_key(&next.resource) == Some(resource))
            {
                return Ok(false);
            }
            selected = Some((owner, matched.symbol));
        }
        let Some((owner, symbol)) = selected else {
            return Ok(false);
        };
        budget.charge_material(size_of::<usize>())?;
        resources[owner]
            .1
            .try_reserve_exact(1)
            .map_err(|_| retained::unavailable())?;
        resources[owner].1.push(symbol);
    }
    for (_, selected) in resources {
        selected.sort_unstable();
        selected.dedup();
    }
    Ok(true)
}

fn resource_key(term: &rift_protocol::query::RdfTerm) -> Option<(u8, &str)> {
    match term {
        rift_protocol::query::RdfTerm::Iri { value } => Some((0, value)),
        rift_protocol::query::RdfTerm::BlankNode { value } => Some((1, value)),
        rift_protocol::query::RdfTerm::Literal { .. }
        | rift_protocol::query::RdfTerm::LanguageLiteral { .. } => None,
    }
}

pub(crate) fn source_requirements(
    body: &QueryFactSelection,
    identities: &QueryIdentities,
    requested: &FactRequirements,
    selected_node_provider: bool,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<Option<FactRequirements>, RiftError> {
    if selected_node_provider && !requested.selects_node_sources() {
        return Err(retained::unavailable());
    }
    budget.charge(requested)?;
    let mut requested = translate(requested.clone(), identities);
    requested
        .services
        .retain(|service| !service.starts_with(rift_search::sparql::RESULT_SERVICE_PREFIX));
    requested.source_reads.clear();
    if selected_node_provider && !requested.selects_node_sources() {
        return Err(retained::unavailable());
    }
    if body.covers(&requested) {
        return Ok(None);
    }
    if selected_node_provider {
        if !requested.selects_node_sources() {
            return Err(retained::unavailable());
        }
        return Ok(Some(requested));
    }
    budget.charge(&requested)?;
    let mut baseline = requested.clone();
    baseline
        .required
        .remove(&rift_search::query::FactRequirement::Nodes);
    baseline
        .required
        .remove(&rift_search::query::FactRequirement::Source);
    if !requested.selects_node_sources() {
        baseline.source_units = None;
    }
    Ok(body.covers(&baseline).then_some(requested))
}

pub(crate) fn original_identities(
    backend: &str,
    added: &QueryFactSelection,
    original: &QueryFactSelection,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<QueryIdentities, RiftError> {
    if crate::proxy::captured_expiry(&original.view)? <= std::time::SystemTime::now()
        || added.root != original.root
        || added.owner != original.owner
        || added.selection != original.selection
    {
        return Err(retained::unavailable());
    }
    super::identity::identities_for(backend, added, original, budget)
}

fn translate(mut requested: FactRequirements, identities: &QueryIdentities) -> FactRequirements {
    let symbols = &identities.symbols;
    if let Some(units) = &mut requested.source_units {
        *units = units
            .iter()
            .filter_map(|public| {
                identities
                    .sources
                    .iter()
                    .find(|(_, unit)| unit == public)
                    .map(|(original, _)| original.clone())
            })
            .collect();
    }
    if requested.source_units.as_ref().is_some_and(Vec::is_empty) {
        requested.source_units = None;
        requested
            .required
            .remove(&rift_search::query::FactRequirement::Nodes);
        requested
            .required
            .remove(&rift_search::query::FactRequirement::Source);
    }
    requested.source_reads.clear();
    let original = |public: &SymbolId| {
        symbols
            .iter()
            .find(|(_, advertised)| advertised == public)
            .map(|(original, _)| original.clone())
    };
    if let Some(selected) = &mut requested.symbol_ids {
        *selected = selected.iter().filter_map(original).collect();
    }
    requested.occurrences = requested.occurrences.iter().filter_map(original).collect();
    requested.history_symbols = requested
        .history_symbols
        .iter()
        .filter_map(original)
        .collect();
    requested.adjacency.retain_mut(|selected| {
        if let Some(seed) = original(&selected.seed) {
            selected.seed = seed;
            true
        } else {
            false
        }
    });
    requested
}

fn encoding_failure(source: serde_json::Error) -> RiftError {
    errors::mcp::forward_failed().source(source).error()
}

impl super::super::RiftProxy {
    fn retained_service_groups(
        &self,
        plan: &rift_search::sparql::QueryPlan,
        budget: &rift_server::QueryFactBudget<'static>,
    ) -> Result<Vec<ServiceGroup>, RiftError> {
        let selected = plan
            .requirements()
            .services
            .iter()
            .filter(|service| service.starts_with(rift_search::sparql::RESULT_SERVICE_PREFIX));
        let mut groups: Vec<ServiceGroup> = Vec::new();
        let mut admission = budget.clone();
        for service in selected {
            let Some((view, page)) = result_service(service)? else {
                return Err(retained::unavailable());
            };
            let index = if let Some(index) = groups
                .iter()
                .position(|(captured, _)| captured.view().id == view)
            {
                index
            } else {
                let views = self.query_views.get().ok_or_else(retained::unavailable)?;
                let root = self
                    .accepted_root
                    .as_deref()
                    .ok_or_else(retained::unavailable)?;
                let captured = views
                    .query_view(&view, root, &self.owner)
                    .map_err(|_| retained::unavailable())?;
                admission.charge_material(size_of::<ServiceGroup>())?;
                groups
                    .try_reserve_exact(1)
                    .map_err(|_| retained::unavailable())?;
                groups.push((captured, Vec::new()));
                groups.len() - 1
            };
            if groups[index].1.iter().any(|(known, _)| known == service) {
                continue;
            }
            admission.charge(&(service, page))?;
            groups[index]
                .1
                .try_reserve_exact(1)
                .map_err(|_| retained::unavailable())?;
            groups[index].1.push((service.clone(), page));
        }
        Ok(groups)
    }

    async fn restore_source_bodies(
        &self,
        bodies: Vec<Body>,
        selected_sources: Option<&[bool]>,
        requirements: &std::sync::Arc<FactRequirements>,
        budget: &rift_server::QueryFactBudget<'static>,
        requests: &rift_server::ProviderRequests,
        deadline: tokio::time::Instant,
    ) -> Result<Result<Vec<Body>, super::acquire::Acquired>, RiftError> {
        let mut updated = Vec::new();
        updated
            .try_reserve_exact(bodies.len())
            .map_err(|_| retained::unavailable())?;
        for (index, (backend, body, identities)) in bodies.into_iter().enumerate() {
            if selected_sources
                .as_ref()
                .is_some_and(|selected| !selected[index])
            {
                updated.push((backend, body, identities));
                continue;
            }
            let selected_node_provider = selected_sources
                .as_ref()
                .is_some_and(|selected| selected[index]);
            let requested = std::sync::Arc::clone(requirements);
            let (backend, body, identities, requested) = self
                .query_work(budget, deadline, move |budget| {
                    let requested = source_requirements(
                        &body,
                        &identities,
                        &requested,
                        selected_node_provider,
                        budget,
                    )?;
                    Ok((backend, body, identities, requested))
                })
                .await?;
            let Some(requested) = requested else {
                updated.push((backend, body, identities));
                continue;
            };
            let context = crate::global::candidates::SavedCandidateBackend {
                backend: backend.clone(),
                root: body.root.clone(),
                owner: Some(body.owner.clone()),
                view: body.view.clone(),
                project: None,
                upstream_view: None,
            };
            let acquired = self
                .query_input(
                    &backend,
                    requested,
                    (Some(body.view.id.clone()), Some(context)),
                    budget,
                    requests,
                    deadline,
                )
                .await?;
            let super::acquire::Acquired::Facts(mut acquired) = acquired else {
                return Ok(Err(acquired));
            };
            if acquired.len() != 1 {
                return Err(retained::unavailable());
            }
            let (name, added, _, _) = acquired.pop().ok_or_else(retained::unavailable)?;
            let row = self
                .query_work(budget, deadline, move |budget| {
                    if name != backend {
                        return Err(retained::unavailable());
                    }
                    let identities = original_identities(&backend, &added, &body, budget)?;
                    let mut added = added;
                    added.graph = body.graph;
                    added.view = body.view;
                    Ok((backend, added, identities))
                })
                .await?;
            updated.push(row);
        }
        Ok(Ok(updated))
    }

    pub(super) async fn retained_query_inputs(
        &self,
        plan: &rift_search::sparql::QueryPlan,
        budget: &rift_server::QueryFactBudget<'static>,
        requests: &rift_server::ProviderRequests,
        deadline: tokio::time::Instant,
    ) -> Result<Option<super::acquire::Acquired>, RiftError> {
        let groups = self.retained_service_groups(plan, budget)?;
        if groups.is_empty() {
            return Ok(None);
        }
        let mut admission = budget.clone();
        admission.charge(plan.requirements())?;
        let answer_only = retained_answer_only(plan.requirements());
        let selection = plan.retained_subject_selection();
        admission.charge(&selection)?;
        let selection = selection.cloned();
        let limits = rift_search::sparql::QueryLimits::try_from(&self.query_configuration)?;
        admission.charge_material(
            size_of::<FactRequirements>()
                .checked_add(2 * size_of::<usize>())
                .ok_or_else(retained::unavailable)?,
        )?;
        let requirements = std::sync::Arc::new(plan.requirements().clone());
        admission.charge_material(
            size_of::<std::collections::BTreeMap<String, String>>()
                .checked_add(2 * size_of::<usize>())
                .ok_or_else(retained::unavailable)?,
        )?;
        let backends = std::sync::Arc::new(self.query_roots());
        let mut selected = Vec::new();
        for (captured, services) in groups {
            let requested = std::sync::Arc::clone(&requirements);
            let backends = std::sync::Arc::clone(&backends);
            let (metadata, mut bodies, bindings, selected_sources) = self
                .query_work(budget, deadline, move |budget| {
                    let (metadata, bodies) = restore(&captured, budget)?;
                    super::source::validate_backends(&metadata, &backends)?;
                    let bindings = restore_bindings(&captured, &metadata, services, budget)?;
                    budget.charge(requested.as_ref())?;
                    let selected_sources = selected_node_providers(&metadata, &requested, budget)?;
                    Ok((metadata, bodies, bindings, selected_sources))
                })
                .await?;
            if !answer_only {
                admission.charge_material(
                    bodies
                        .len()
                        .checked_mul(size_of::<Body>())
                        .ok_or_else(retained::unavailable)?,
                )?;
                bodies = match self
                    .restore_source_bodies(
                        bodies,
                        selected_sources.as_deref(),
                        &requirements,
                        budget,
                        requests,
                        deadline,
                    )
                    .await?
                {
                    Ok(updated) => updated,
                    Err(response) => return Ok(Some(response)),
                };
            }
            let requested = std::sync::Arc::clone(&requirements);
            let selection = selection.clone();
            let mut added = self
                .query_work(budget, deadline, move |budget| {
                    let _ = metadata;
                    prepare_retained_bodies(
                        bodies,
                        bindings,
                        &requested,
                        selection.as_ref(),
                        limits,
                        selected_sources.as_deref(),
                        budget,
                    )
                })
                .await?;
            let Some(added) = &mut added else {
                return Ok(Some(super::acquire::unavailable_response(rift_protocol::search_read::SearchUnavailable::Unavailable { reason: rift_protocol::symbol_read::SymbolUnavailableReason::InsufficientCoverage, view: None, warnings: Vec::new() })));
            };
            super::acquire::append_selections(&mut selected, added, &mut budget.clone())?;
        }
        self.query_work(budget, deadline, move |_budget| {
            if conflicting_local_graphs(
                selected
                    .iter()
                    .filter(|row| row.0 == "local")
                    .map(|row| &row.1),
            ) {
                return Ok(Some(super::acquire::unavailable_response(
                    rift_protocol::search_read::SearchUnavailable::Unavailable {
                        reason: rift_protocol::symbol_read::SymbolUnavailableReason::ViewContext,
                        view: None,
                        warnings: Vec::new(),
                    },
                )));
            }
            Ok(Some(super::acquire::Acquired::Facts(selected)))
        })
        .await
    }
}

fn prepare_retained_bodies(
    bodies: Vec<Body>,
    bindings: rift_search::sparql::QueryServices,
    requested: &FactRequirements,
    selection: Option<&rift_search::sparql::RetainedSubjectSelection>,
    limits: rift_search::sparql::QueryLimits,
    selected_sources: Option<&[bool]>,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<Option<Vec<PreparedBody>>, RiftError> {
    if !retained_answer_only(requested)
        && !covers_retained_source_bindings(
            &bodies,
            requested,
            &bindings,
            selection,
            limits,
            selected_sources,
            budget,
        )?
    {
        return Ok(None);
    }
    let mut added = Vec::new();
    budget.charge_material(
        bodies
            .len()
            .checked_mul(size_of::<(
                String,
                QueryFactSelection,
                rift_search::sparql::QueryServices,
                Option<QueryIdentities>,
            )>())
            .ok_or_else(retained::unavailable)?,
    )?;
    added
        .try_reserve_exact(bodies.len())
        .map_err(|_| retained::unavailable())?;
    let mut bindings = bindings;
    for (index, (backend, body, identities)) in bodies.into_iter().enumerate() {
        let mut services = if index == 0 {
            std::mem::take(&mut bindings)
        } else {
            rift_search::sparql::QueryServices::default()
        };
        if selected_sources
            .as_ref()
            .is_some_and(|selected| !selected[index])
        {
            include_empty_node_domain(&mut services, &body, &identities, budget)?;
        }
        added.push((backend, body, services, Some(identities)));
    }
    Ok(Some(added))
}

pub(crate) fn restore_bindings(
    captured: &rift_server::CapturedQueryView,
    metadata: &Metadata,
    services: Vec<(String, Option<u64>)>,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<rift_search::sparql::QueryServices, RiftError> {
    let mut bindings = rift_search::sparql::QueryServices::default();
    for (service, page) in services {
        budget.charge_bytes(captured.material().records())?;
        let answer = metadata
            .answer
            .decode_bytes(captured.material().records(), page)?;
        budget.charge(&answer)?;
        bindings
            .bindings
            .try_reserve_exact(1)
            .map_err(|_| retained::unavailable())?;
        bindings
            .bindings
            .push(rift_search::sparql::RetainedResult { service, answer });
    }
    Ok(bindings)
}

pub(crate) fn result_service(
    service: &str,
) -> Result<Option<(rift_protocol::symbol_read::CapturedViewId, Option<u64>)>, RiftError> {
    let Some(value) = service.strip_prefix(rift_search::sparql::RESULT_SERVICE_PREFIX) else {
        return Ok(None);
    };
    let (view, page) = match value.rsplit_once("?page=") {
        Some((view, page)) => (
            view,
            Some(page.parse::<u64>().map_err(|_| retained::unavailable())?),
        ),
        None => (value, None),
    };
    Ok(Some((
        rift_protocol::symbol_read::CapturedViewId::parse(view)?,
        page,
    )))
}
