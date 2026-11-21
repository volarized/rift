//! Bounded acquisition of paths of call edges from ranked local symbols.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rift_error::RiftError;
use rift_protocol::identity::SymbolIdentity;
use rift_protocol::read::{HopDirection, ReadWarning, SearchHitTarget, SearchResult};
use rift_search::flow::{
    FlowAdjacency, FlowContinuation, FlowCut, FlowDiscovery, FlowLimits, FlowNeighbor, FlowSeed,
};
use rift_search::sparql::{FlowCallSite, QueryFlows};

use super::{
    AdjacencyStep, AdjacencyWalk, EdgeKind, GraphLimits, RequestDeadline, ResolvedWorkspace,
    RiftMcp, TraversalDirection, coverage_failure,
};

type Subject = Arc<rift_protocol::read::SymbolId>;
type Neighbor = FlowNeighbor<Subject, FlowCallSite>;

struct FlowNeighbors {
    neighbors: Vec<Neighbor>,
    coverage: FlowCoverage,
}

enum FlowCoverage {
    Complete,
    ProviderIncomplete,
    RequestLimit,
}

pub(in super::super) struct FlowAcquisition<'request> {
    walk: AdjacencyWalk<'request>,
    cache: BTreeMap<(Subject, HopDirection), FlowNeighbors>,
}

pub(in super::super) struct AcquiredFlows {
    discovery: FlowDiscovery<Subject, FlowCallSite>,
    partial: bool,
    request_limit: bool,
}

impl<'request> FlowAcquisition<'request> {
    pub(in super::super) fn new(
        server: &'request RiftMcp,
        resolved: &'request ResolvedWorkspace,
        limits: GraphLimits,
        deadline: RequestDeadline,
    ) -> Self {
        let step = AdjacencyStep {
            direction: TraversalDirection::Incoming,
            kind: EdgeKind::Calls,
        };
        Self {
            walk: AdjacencyWalk::new(server, resolved, None, step, limits, deadline),
            cache: BTreeMap::new(),
        }
    }

    pub(in super::super) async fn acquire(
        &mut self,
        ranked: &SearchResult,
    ) -> Result<AcquiredFlows, RiftError> {
        let mut partial = false;
        let seeds = ranked.results.iter().filter_map(|hit| {
            if let SearchHitTarget::Symbol { symbol } = &hit.hit
                && let (Some(subject), Some(score)) = (&symbol.id, hit.score)
                && score.is_finite()
                && score >= 0.0
            {
                return Some(FlowSeed {
                    subject: Arc::new(subject.clone()),
                    score,
                });
            }
            partial = true;
            None
        });
        let mut discovery = FlowDiscovery::new(seeds, FlowLimits::default());
        let mut request_limit = false;
        let initial_requests = self.walk.remaining.requests_max;
        while let Some(request) = discovery.next_adjacency() {
            let key = (Arc::clone(&request.subject), request.direction);
            if !self.cache.contains_key(&key) {
                let neighbors = Box::pin(self.read_neighbors(&request)).await?;
                self.cache.insert(key.clone(), neighbors);
            }
            let neighbors = self.cache.get(&key).ok_or_else(coverage_failure)?;
            match neighbors.coverage {
                FlowCoverage::Complete => {}
                FlowCoverage::ProviderIncomplete => partial = true,
                FlowCoverage::RequestLimit => request_limit = true,
            }
            discovery.accept_adjacency(request, neighbors.neighbors.iter().cloned());
        }
        let requests = initial_requests - self.walk.remaining.requests_max;
        rift_tracing::debug!(
            component = "search",
            requests,
            partial,
            "flow adjacency acquired"
        );
        Ok(AcquiredFlows {
            discovery,
            partial,
            request_limit,
        })
    }

    async fn read_neighbors(
        &mut self,
        request: &FlowAdjacency<Subject>,
    ) -> Result<FlowNeighbors, RiftError> {
        if self.walk.remaining.requests_max == 0 || self.walk.reached.is_some() {
            return Ok(FlowNeighbors {
                neighbors: Vec::new(),
                coverage: FlowCoverage::RequestLimit,
            });
        }
        self.walk.acquired.incomplete = false;
        self.walk.step.direction = match request.direction {
            HopDirection::Incoming => TraversalDirection::Incoming,
            HopDirection::Outgoing => TraversalDirection::Outgoing,
        };
        let mut reached = BTreeSet::new();
        let step = Box::pin(self.walk.local_step(&request.subject, &mut reached)).await;
        if self.walk.spent(step)? {
            return Ok(FlowNeighbors {
                neighbors: Vec::new(),
                coverage: FlowCoverage::RequestLimit,
            });
        }
        self.walk.acquired.local_edges.clear();
        let mut sites = BTreeMap::<_, BTreeMap<_, BTreeSet<_>>>::new();
        for edge in std::mem::take(&mut self.walk.acquired.edge_facts) {
            let neighbor =
                super::endpoint((&edge.from, &edge.to), self.walk.step.direction).clone();
            let span = edge
                .occurrence
                .map(|span| (span.unit, span.range.start, span.range.end));
            sites
                .entry(neighbor)
                .or_default()
                .entry(span)
                .or_default()
                .insert(edge.provider);
        }
        for occurrence in std::mem::take(&mut self.walk.acquired.local) {
            let neighbor = match request.direction {
                HopDirection::Incoming => occurrence.enclosing.ok_or_else(coverage_failure)?,
                HopDirection::Outgoing => occurrence.target,
            };
            let span = Some((
                occurrence.source.unit,
                occurrence.source.range.start,
                occurrence.source.range.end,
            ));
            sites.entry(neighbor).or_default().entry(span).or_default();
        }
        let mut neighbors = Vec::new();
        for symbol in reached {
            let identity = SymbolIdentity::parse(symbol.as_str())?;
            let continuation = if identity.owner().is_local() {
                FlowContinuation::Traverse
            } else {
                FlowContinuation::Terminal
            };
            let evidence = sites
                .remove(&symbol)
                .unwrap_or_else(|| BTreeMap::from([(None, BTreeSet::new())]));
            let subject = Arc::new(symbol);
            neighbors.extend(
                evidence
                    .into_iter()
                    .map(|(occurrence, providers)| FlowNeighbor {
                        subject: Arc::clone(&subject),
                        continuation,
                        evidence: FlowCallSite {
                            occurrence,
                            providers,
                        },
                    }),
            );
        }
        let coverage = if self.walk.acquired.incomplete {
            FlowCoverage::ProviderIncomplete
        } else {
            FlowCoverage::Complete
        };
        Ok(FlowNeighbors {
            neighbors,
            coverage,
        })
    }

    pub(in super::super) async fn validate(&self) -> Result<(), RiftError> {
        self.walk.verify_installed().await
    }
}

impl AcquiredFlows {
    pub(in super::super) fn finish(
        self,
        limit: usize,
        warnings: &mut Vec<ReadWarning>,
    ) -> (QueryFlows, bool) {
        let flows = rift_tracing::traced!(component = "search", operation = "search.flows.rank", {
            self.discovery.finish(limit)
        });
        if self.partial {
            warnings.push(ReadWarning::EdgeCoverageMissing {
                kinds: vec![EdgeKind::Calls],
                detail: "some selected call directions have incomplete provider coverage"
                    .to_owned(),
            });
        }
        if self.request_limit || !flows.cuts.is_empty() {
            let cuts = flows
                .cuts
                .iter()
                .copied()
                .map(cut_name)
                .chain(self.request_limit.then_some("provider requests"))
                .collect::<Vec<_>>()
                .join(", ");
            warnings.push(ReadWarning::TraversalTruncated {
                visited: u64::try_from(flows.visited).unwrap_or(u64::MAX),
                detail: format!("flow discovery reached these bounds: {cuts}"),
            });
        }
        let paths = flows.paths.len();
        let steps = flows
            .paths
            .iter()
            .map(|path| path.steps.len())
            .sum::<usize>();
        rift_tracing::debug!(
            component = "search",
            visited = flows.visited,
            paths,
            steps,
            cuts = ?flows.cuts,
            "flow paths ranked"
        );
        (flows, self.partial)
    }
}

fn cut_name(cut: FlowCut) -> &'static str {
    match cut {
        FlowCut::Seeds => "ranked seeds",
        FlowCut::Subjects => "visited symbols",
        FlowCut::Evidence => "call sites",
        FlowCut::FanOut => "fan-out",
        FlowCut::Hops => "hops",
        FlowCut::Candidates => "candidate paths",
        FlowCut::Steps => "returned steps",
    }
}
