use rift_error::{RiftError, errors};
use rift_protocol::read::{ReadWarning, SearchHit, SearchParams, SearchParamsTarget, SearchScope};
use rift_ranking::{QueryPhase, RankedCandidates, RankedIdentity, RankingInputKind};

use super::{
    BodyMatching, CandidateScreen, HitPayloads, IdentifierSources, ReadService, Resolution,
    SearchCriteria, SearchDocumentation, StoreAnswer, accepted_query, identifier_input,
    parsed_query, populate_symbol_lines, resolve_candidate, retain_public_symbol_hits,
    search_page_limit,
};
use crate::engine_read::EngineReferences;
use crate::read::results_truncation_warning;

/// One projected candidate and its original ranking evidence.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchCandidateItem {
    /// Index-local identity, fields, file row and identifier comparison.
    pub ranked: RankedIdentity,
    /// Readable facts from the same captured index.
    pub hit: SearchHit,
}

/// One bounded input order before combined ranking.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchCandidateInput {
    /// The index that produced this order.
    pub kind: RankingInputKind,
    /// Candidates in the original input order.
    pub order: Vec<SearchCandidateItem>,
}

/// Projected input orders from one immutable publication, before fusion or pagination.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchCandidateInputs {
    /// Exact query input orders.
    pub precise: Vec<SearchCandidateInput>,
    /// Widened query input orders.
    pub broad: Vec<SearchCandidateInput>,
    /// Refusals and cuts recorded by this publication.
    pub warnings: Vec<ReadWarning>,
}

impl ReadService {
    /// Collects bounded lexical candidate inputs without combining or paging them.
    ///
    /// Every input retains the index's result bound and the request's path and target
    /// filters. The caller binds these inputs to this publication before final ranking.
    ///
    /// # Errors
    ///
    /// Returns the existing search refusal or rejects selectors that require their own
    /// comparison, pattern or traversal operation.
    pub fn search_candidate_inputs(
        &self,
        params: &SearchParams,
        store: &StoreAnswer,
        references: &EngineReferences,
    ) -> Result<SearchCandidateInputs, RiftError> {
        self.selected_candidate_inputs(params, store, references)
            .map(|(inputs, _sources)| inputs)
    }

    /// Captures candidate inputs and original source from one bounded file selection.
    ///
    /// # Errors
    /// Returns the existing search, source selection or captured material refusal.
    pub fn search_candidate_view_input(
        &self,
        params: &SearchParams,
        store: &StoreAnswer,
        references: &EngineReferences,
        ready: bool,
    ) -> Result<(SearchCandidateInputs, crate::LocalSymbolViewInput), RiftError> {
        let (inputs, sources) = self.selected_candidate_inputs(params, store, references)?;
        let material = self.local_search_view_input(ready)?;
        let material = match sources {
            Some(sources) if params.scope != SearchScope::Global => {
                material.with_sources(&sources)?
            }
            _ => material,
        };
        Ok((inputs, material))
    }

    fn selected_candidate_inputs(
        &self,
        params: &SearchParams,
        store: &StoreAnswer,
        references: &EngineReferences,
    ) -> Result<(SearchCandidateInputs, Option<rift_index::WorkspaceIndex>), RiftError> {
        let query = self.candidate_query(params, store, references)?;
        let selected = self.selected_paths(params.paths.as_ref())?;
        let index = self.index();
        let fetch_limit = index.results_max();
        let sources = IdentifierSources {
            project: params.scope != SearchScope::Global,
            force_include: (params.scope != SearchScope::Global)
                .then_some(selected.force_include.as_ref())
                .flatten(),
        };
        let resolution = Resolution {
            force_include: sources.force_include,
        };
        let documentation = matches!(
            params.target,
            SearchParamsTarget::Documentation | SearchParamsTarget::All
        )
        .then(|| SearchDocumentation::new(index, &params.scope, resolution));
        let body = BodyMatching::for_request(params.target, store, &query);
        let screen = CandidateScreen {
            index,
            matcher: selected.matcher.as_ref(),
            root: index.root(),
            target: params.target,
            resolution,
            documentation: documentation.as_ref(),
            body: body.as_ref(),
        };
        let mut answer = SearchCandidateInputs {
            warnings: self.initial_search_warnings(&selected, params.target, references),
            ..SearchCandidateInputs::default()
        };
        if query.is_narrowed() {
            answer.warnings.push(super::query_narrowing_warning());
        }
        if let Some(documentation) = &documentation {
            answer
                .warnings
                .extend(documentation.warnings().iter().cloned());
        }
        let mut precise = vec![identifier_input(
            index,
            selected.matcher.as_ref(),
            index.root(),
            &query,
            sources,
            fetch_limit,
        )?];
        precise.extend(store.precise().iter().cloned());
        let criteria = SearchCriteria {
            query: &query,
            target: params.target,
            payloads: HitPayloads::requested(params),
        };
        answer.precise = projected_inputs(
            &screen,
            &precise,
            &query,
            QueryPhase::Precise,
            criteria,
            &mut answer.warnings,
        )?;
        answer.broad = projected_inputs(
            &screen,
            store.broad(),
            &query,
            QueryPhase::Broad,
            criteria,
            &mut answer.warnings,
        )?;
        Ok((answer, selected.force_include))
    }

    fn candidate_query(
        &self,
        params: &SearchParams,
        store: &StoreAnswer,
        references: &EngineReferences,
    ) -> Result<rift_ranking::ParsedQuery, RiftError> {
        self.accept_search_revision(params)?;
        self.validated_pattern(params, store, references)?;
        search_page_limit(params)?;
        if params.pattern.is_some()
            || params.change.is_some()
            || params.traversal.is_some()
            || params.target == SearchParamsTarget::Commit
        {
            return errors::server::read_unsupported()
                .capability("lexical candidate inputs for a non-lexical selector")
                .fail();
        }
        if self.revision().is_some() && super::force_include_requested(params) {
            return errors::server::read_unsupported()
                .capability("force_include at a revision")
                .fail();
        }
        let query = accepted_query(params)?.ok_or_else(|| {
            errors::server::read_invalid()
                .field("query")
                .violation("lexical candidate inputs require a query")
                .error()
        })?;
        parsed_query(query)
    }
}

fn projected_inputs(
    screen: &CandidateScreen<'_>,
    inputs: &[rift_ranking::RankingInput],
    query: &rift_ranking::ParsedQuery,
    phase: QueryPhase,
    criteria: SearchCriteria<'_>,
    warnings: &mut Vec<ReadWarning>,
) -> Result<Vec<SearchCandidateInput>, RiftError> {
    let inputs = screen.projected(inputs, query, phase)?;
    let mut results = Vec::with_capacity(inputs.len());
    for input in inputs {
        let projected = RankedCandidates::input(&input, phase);
        let projected = screen.with_body_matches(projected, screen.index.results_max());
        if let Some(bound) = projected.truncated_at() {
            warnings.push(results_truncation_warning(bound));
        }
        let mut order = Vec::with_capacity(projected.len());
        for candidate in projected.candidates() {
            let hit = if let Some(hit) = screen
                .documentation
                .and_then(|documentation| documentation.hit(candidate))
            {
                hit
            } else {
                let Some(resolved) =
                    resolve_candidate(screen.index, screen.resolution, candidate.identity())?
                else {
                    continue;
                };
                resolved.into_hit(criteria, candidate)?
            };
            let mut hit = vec![hit];
            retain_public_symbol_hits(
                &mut hit,
                warnings,
                &[Some(screen.index), screen.resolution.force_include],
            )?;
            populate_symbol_lines(&mut hit, screen.index, screen.resolution.force_include)?;
            if criteria.payloads.source {
                warnings.extend(super::documentation::populate_sources(
                    &mut hit,
                    screen.index,
                    screen.resolution,
                ));
            }
            if let Some(mut hit) = hit.pop() {
                hit.score = None;
                order.push(SearchCandidateItem {
                    ranked: candidate.ranked_identity(),
                    hit,
                });
            }
        }
        results.push(SearchCandidateInput {
            kind: input.kind(),
            order,
        });
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::{SearchCandidateInput, SearchCandidateInputs, SearchCandidateItem};
    use rift_protocol::read::{SearchHit, SearchHitTarget};
    use rift_ranking::{
        DocumentIdentity, FieldSet, RankedIdentity, RankingInputKind, SearchableField,
    };

    #[test]
    fn candidate_inputs_preserve_raw_physical_facts_without_a_canonical_id() {
        let hit: SearchHit = serde_json::from_value(serde_json::json!({
            "hit": {"target": "symbol", "symbol": {
                "language": "toml", "name": "lib", "kind": "table"
            }},
            "path": "Cargo.toml", "range": {"start": 0, "end": 4}
        }))
        .expect("a retained physical declaration");
        let ranked = RankedIdentity::new(
            DocumentIdentity::new("captured-provider-record").expect("a provider identity"),
            FieldSet::of(SearchableField::Name),
        );
        let input = SearchCandidateInputs {
            precise: vec![SearchCandidateInput {
                kind: RankingInputKind::Identifier,
                order: vec![SearchCandidateItem { ranked, hit }],
            }],
            ..SearchCandidateInputs::default()
        };
        let bytes = serde_json::to_vec(&input).expect("candidate inputs serialize");
        let decoded: SearchCandidateInputs =
            serde_json::from_slice(&bytes).expect("candidate inputs restore");
        let row = &decoded.precise[0].order[0];
        assert_eq!(row.ranked, input.precise[0].order[0].ranked);
        assert_eq!(row.hit, input.precise[0].order[0].hit);
        let SearchHitTarget::Symbol { symbol } = &row.hit.hit else {
            panic!("the physical symbol remains a symbol");
        };
        assert_eq!(symbol.id, None);
    }

    #[test]
    fn candidate_input_decode_refuses_unknown_fields_and_invalid_document_identity() {
        let valid = serde_json::json!({
            "precise": [], "broad": [], "warnings": []
        });
        let mut unknown = valid.clone();
        unknown["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<SearchCandidateInputs>(unknown).is_err());
        for identity in [
            String::new(),
            "x".repeat(rift_ranking::IDENTITY_BYTES_MAX + 1),
        ] {
            let mut invalid = valid.clone();
            invalid["precise"] = serde_json::json!([{
                "kind": "identifier", "order": [{
                    "ranked": {"identity": identity, "fields": ["name"]},
                    "hit": {"hit": {"target": "symbol", "symbol": {
                        "language": "toml", "name": "lib", "kind": "table"
                    }}, "path": "Cargo.toml", "range": {"start": 0, "end": 4}}
                }]
            }]);
            assert!(serde_json::from_value::<SearchCandidateInputs>(invalid).is_err());
        }
    }
}
