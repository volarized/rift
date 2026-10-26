//! Retained evaluations restore only their original admitted fact bodies.

use rift_error::{RiftError, errors};
use rift_protocol::read::SymbolId;
use rift_search::sparql::{FactRequirements, QueryIdentities};

use super::retained::{self, Metadata};
use crate::server::QueryFactSelection;

type Body = (String, QueryFactSelection, QueryIdentities);
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
        budget,
    )?;
    Ok((metadata, bodies))
}

fn restore_sources(
    bodies: &mut [Body],
    metadata: &Metadata,
    bytes: &[u8],
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<(), RiftError> {
    for (provider, (_, body, identities)) in bodies.iter_mut().enumerate() {
        for source in &mut body.facts.sources {
            budget.check()?;
            let mut matching = metadata
                .sources
                .iter()
                .filter(|saved| saved.provider == provider && saved.original == source.unit);
            let saved = matching.next().ok_or_else(retained::unavailable)?;
            if matching.next().is_some() || source.text.is_some() {
                return Err(retained::unavailable());
            }
            let advertised = identities
                .sources
                .iter()
                .find(|(original, _)| original == &source.unit)
                .map_or(&source.unit, |(_, public)| public);
            if advertised != &saved.public {
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

pub(crate) fn covers_requested(
    bodies: &[Body],
    requested: &FactRequirements,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<bool, RiftError> {
    if !requested_seeds_present(bodies, requested, budget)? {
        return Ok(false);
    }
    for (_, body, identities) in bodies {
        budget.charge(requested)?;
        let translated = translate(requested.clone(), &identities.symbols);
        if !body.covers(&translated) {
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

pub(crate) fn covers_retained_bindings(
    bodies: &[Body],
    requested: &FactRequirements,
    services: &rift_search::sparql::QueryServices,
    selection: Option<&rift_search::sparql::RetainedSubjectSelection>,
    limits: rift_search::sparql::QueryLimits,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<bool, RiftError> {
    if covers_requested(bodies, requested, budget)? {
        return Ok(true);
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
        let requested = translate(requested.clone(), &identities.symbols);
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

fn translate(
    mut requested: FactRequirements,
    symbols: &[(SymbolId, SymbolId)],
) -> FactRequirements {
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

    pub(super) async fn retained_query_inputs(
        &self,
        plan: &rift_search::sparql::QueryPlan,
        budget: &rift_server::QueryFactBudget<'static>,
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
        let requirements = plan.requirements().clone();
        let backends = self.query_roots();
        self.query_work(budget, deadline, move |budget| {
            let mut selected = Vec::new();
            for (captured, services) in groups {
                let (metadata, bodies) = restore(&captured, budget)?;
                super::source::validate_backends(&metadata, &backends)?;
                let mut bindings = restore_bindings(&captured, &metadata, services, budget)?;
                let covered = answer_only || covers_retained_bindings(
                    &bodies, &requirements, &bindings, selection.as_ref(), limits, budget,
                )?;
                if !covered {
                    return Ok(Some(super::acquire::unavailable_response(
                        rift_protocol::search_read::SearchUnavailable::Unavailable {
                            reason: rift_protocol::symbol_read::SymbolUnavailableReason::InsufficientCoverage,
                            view: None, warnings: Vec::new(),
                        },
                    )));
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
                for (index, (backend, body, identities)) in bodies.into_iter().enumerate() {
                    added.push((
                        backend,
                        body,
                        if index == 0 {
                            std::mem::take(&mut bindings)
                        } else {
                            rift_search::sparql::QueryServices::default()
                        },
                        Some(identities),
                    ));
                }
                super::acquire::append_selections(&mut selected, &mut added, budget)?;
            }
            if conflicting_local_graphs(
                selected.iter().filter(|row| row.0 == "local").map(|row| &row.1),
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
