use std::collections::BTreeMap;

use rift_error::{RiftError, errors};
use rift_protocol::read::{
    ReadWarning, SearchHit, SearchHitTarget, SearchInclude, SearchParams, SearchResult,
};
use rift_protocol::symbol_read::CapturedView;
use rift_ranking::{
    DocumentIdentity, QueryPhase, RankingInput, RankingInputKind, RankingWeights, fuse,
};
use rift_server::SearchCandidateInputs;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(crate) const CANDIDATE_BYTES_MAX: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SearchCandidatePool {
    pub(crate) owner: Option<rift_protocol::identity::SymbolOwner>,
    pub(crate) backend: String,
    pub(crate) root: String,
    pub(crate) view: CapturedView,
    pub(crate) inputs: SearchCandidateInputs,
}

/// Retains package input orders and their original captured selection.
pub(crate) fn package_candidate_pool(
    params: &SearchParams,
    root: String,
    captured: super::CapturedGlobalSearchCandidates,
) -> Result<Option<SearchCandidatePool>, RiftError> {
    let Some(view) = captured.view else {
        return Ok(None);
    };
    let query = params
        .query
        .as_deref()
        .map(rift_ranking::ParsedQuery::parse)
        .transpose()?;
    let project = |candidates: Vec<rift_cloud_client::PackageSearchCandidate>| {
        candidates
            .into_iter()
            .map(|candidate| {
                let mut ranked = super::ranked_search_hit(
                    candidate.identity,
                    super::candidate_fields(&candidate.hit),
                    query.as_ref(),
                    &candidate.hit,
                )?;
                if let Some(class) = candidate.match_class {
                    ranked = ranked.with_identifier_match(class.into());
                }
                Ok(rift_server::SearchCandidateItem {
                    ranked,
                    hit: candidate.hit,
                })
            })
            .collect::<Result<Vec<_>, RiftError>>()
    };
    Ok(Some(SearchCandidatePool {
        backend: "global".to_owned(),
        owner: None,
        root,
        view,
        inputs: rift_server::SearchCandidateInputs {
            precise: vec![rift_server::SearchCandidateInput {
                kind: RankingInputKind::Lexical,
                order: project(captured.candidates.precise)?,
            }],
            broad: vec![rift_server::SearchCandidateInput {
                kind: RankingInputKind::Lexical,
                order: project(captured.candidates.broad)?,
            }],
            warnings: captured.candidates.warnings,
        },
    }))
}

/// Combines captured input orders once, then applies one result bound and page.
pub(crate) fn merge_candidate_pools(
    params: &SearchParams,
    pools: &[SearchCandidatePool],
    weights: RankingWeights,
    results_max: usize,
) -> Result<SearchResult, RiftError> {
    crate::proxy::encode_bounded(&pools, CANDIDATE_BYTES_MAX)
        .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
    let limit = rift_server::search_page_limit(params)?;
    if results_max == 0 {
        return errors::server::read_invalid()
            .field("results_max")
            .violation("result bound must be positive")
            .fail();
    }
    let mut payloads = BTreeMap::new();
    let mut precise = Vec::new();
    let mut broad = Vec::new();
    let mut warnings = Vec::new();
    for pool in pools {
        warnings.extend(pool.inputs.warnings.iter().cloned());
        for (inputs, destination) in [
            (&pool.inputs.precise, &mut precise),
            (&pool.inputs.broad, &mut broad),
        ] {
            for input in inputs {
                let mut order = Vec::with_capacity(input.order.len());
                for item in &input.order {
                    let identity = captured_identity(pool, &item.hit)?;
                    payloads.entry(identity.clone()).or_insert_with(|| {
                        let mut hit = item.hit.clone();
                        hit.view = Some(pool.view.clone());
                        hit
                    });
                    order.push(item.ranked.clone().with_identity(identity));
                }
                destination.push(RankingInput::new(input.kind, order));
            }
        }
    }
    let mut ranked = fuse(&precise, weights, QueryPhase::Precise, results_max);
    if ranked.len() < results_max {
        ranked.append_phase(
            fuse(&broad, weights, QueryPhase::Broad, results_max),
            results_max,
        );
    }
    let include_score = params
        .include
        .as_deref()
        .unwrap_or_default()
        .contains(&SearchInclude::Score);
    let mut hits = ranked
        .candidates()
        .iter()
        .filter_map(|candidate| {
            payloads.get(candidate.identity()).cloned().map(|mut hit| {
                hit.score = include_score.then(|| candidate.score());
                hit
            })
        })
        .collect::<Vec<_>>();
    if ranked.len() == results_max {
        warnings.push(ReadWarning::ResultsTruncated {
            results_max: u64::try_from(results_max).unwrap_or(u64::MAX),
        });
    }
    super::order_search_hits(&mut hits, params.order);
    let (results, pagination) = super::page_window(hits, params.page_index, limit);
    Ok(SearchResult {
        view: None,
        results,
        pagination,
        warnings,
    })
}

fn captured_identity(
    pool: &SearchCandidatePool,
    hit: &SearchHit,
) -> Result<DocumentIdentity, RiftError> {
    let object = match &hit.hit {
        SearchHitTarget::Symbol { symbol } => {
            if let Some(id) = &symbol.id {
                serde_json::json!(["symbol", id])
            } else {
                if hit.range.is_none() || (hit.path.is_none() && hit.unit.is_none()) {
                    return errors::mcp::project_hit_identity_missing()
                        .hit(&symbol.name)
                        .fail();
                }
                serde_json::json!([
                    "symbol",
                    symbol.language,
                    symbol.kind,
                    symbol.name,
                    hit.path,
                    hit.unit,
                    hit.range
                ])
            }
        }
        _ => serde_json::json!([super::ranking_identity(hit)?.as_str()]),
    };
    let key = (&pool.backend, &pool.root, &pool.view, object);
    let encoded = crate::proxy::encode_bounded(&key, CANDIDATE_BYTES_MAX)
        .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
    DocumentIdentity::new(format!("{:x}", Sha256::digest(encoded)))
}

#[cfg(test)]
mod tests {
    use super::{CANDIDATE_BYTES_MAX, SearchCandidatePool, merge_candidate_pools};
    use rift_protocol::read::{SearchHit, SearchParams};
    use rift_protocol::symbol_read::{CapturedView, CapturedViewId};
    use rift_ranking::{
        DocumentIdentity, FieldSet, RankedIdentity, RankingInputKind, RankingWeights,
        SearchableField,
    };
    use rift_server::{SearchCandidateInput, SearchCandidateInputs, SearchCandidateItem};

    fn pool(backend: &str, start: u64, id: Option<&str>) -> SearchCandidatePool {
        let mut symbol = serde_json::json!({"language":"rust","name":"beacon","kind":"function"});
        if let Some(id) = id {
            symbol["id"] = serde_json::json!(id);
        }
        let hit: SearchHit = serde_json::from_value(serde_json::json!({
            "hit":{"target":"symbol","symbol":symbol},"path":"lib.rs",
            "range":{"start":start,"end":start+10},"line":1
        }))
        .expect("physical captured hit");
        SearchCandidatePool {
            backend: backend.to_owned(),
            owner: (backend != "global").then_some(rift_protocol::identity::SymbolOwner::Local),
            root: format!("/workspace/{backend}"),
            view: CapturedView {
                id: CapturedViewId::parse(&if backend == "local" { "1" } else { "2" }.repeat(64))
                    .expect("view"),
                expires_at: "2026-10-11T01:00:00Z".to_owned(),
            },
            inputs: SearchCandidateInputs {
                precise: vec![SearchCandidateInput {
                    kind: RankingInputKind::Identifier,
                    order: vec![SearchCandidateItem {
                        ranked: RankedIdentity::new(
                            DocumentIdentity::new("provider-record").expect("document"),
                            FieldSet::of(SearchableField::Name),
                        ),
                        hit,
                    }],
                }],
                ..SearchCandidateInputs::default()
            },
        }
    }

    #[test]
    fn captured_pool_merge_keeps_identical_local_ids_from_distinct_backends() {
        let params: SearchParams =
            serde_json::from_value(serde_json::json!({"query":"beacon"})).expect("query");
        let id = rift_protocol::identity::SymbolIdentity::new(
            rift_protocol::identity::SymbolOwner::Local,
            serde_json::from_value(serde_json::json!("rust")).expect("language"),
            vec!["fixture".to_owned(), "beacon".to_owned()],
        )
        .expect("modeled logical identity")
        .wire_identity();
        let pools = [pool("local", 0, Some(&id)), pool("extra", 0, Some(&id))];
        let result = merge_candidate_pools(
            &params,
            &pools,
            RankingWeights::fixed(0.5, 0.5, 0.0, 60),
            10,
        )
        .expect("combined pools");
        assert_eq!(result.results.len(), 2);
        assert_eq!(
            result
                .results
                .iter()
                .map(|hit| hit
                    .view
                    .as_ref()
                    .expect("original captured view")
                    .id
                    .as_str())
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            2
        );
    }

    #[test]
    fn captured_pool_merge_keeps_raw_ranges_and_deduplicates_same_physical_fact() {
        let params: SearchParams =
            serde_json::from_value(serde_json::json!({"query":"beacon"})).expect("query");
        let first = pool("local", 0, None);
        let second = pool("local", 20, None);
        let pools = [first.clone(), first, second];
        let result = merge_candidate_pools(
            &params,
            &pools,
            RankingWeights::fixed(0.5, 0.5, 0.0, 60),
            10,
        )
        .expect("raw facts");
        assert_eq!(result.results.len(), 2);
        assert!(result.results.iter().all(|hit| matches!(&hit.hit, rift_protocol::read::SearchHitTarget::Symbol { symbol } if symbol.id.is_none())));
    }

    #[test]
    fn candidate_aggregate_bound_counts_json_escaping() {
        let params: SearchParams =
            serde_json::from_value(serde_json::json!({"query":"beacon"})).expect("query");
        let mut pool = pool("local", 0, None);
        pool.inputs.precise[0].order[0].hit.source =
            Some("\u{0001}".repeat(CANDIDATE_BYTES_MAX / 6));
        let failure = merge_candidate_pools(
            &params,
            &[pool],
            RankingWeights::fixed(0.5, 0.5, 0.0, 60),
            10,
        )
        .expect_err("escaped aggregate exceeds bound");
        assert_eq!(
            failure.slug(),
            rift_error::errors::mcp::forward_failed::SLUG
        );
        assert!(std::error::Error::source(&failure).is_some());
    }
}
