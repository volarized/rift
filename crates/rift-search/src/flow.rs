//! Discover and rank paths of call edges around ranked symbols.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use rift_protocol::search::HopDirection;

type PathsByLength<Subject> = BTreeMap<usize, BTreeSet<Vec<Subject>>>;

const SECOND_SEED_WEIGHT: f64 = 0.25;
const FLOW_SEEDS_MAX: usize = 12;

/// Read one more ranked row than discovery can admit so it can report the cut.
pub(crate) const FLOW_RANKED_ROWS_MAX: u64 = FLOW_SEEDS_MAX as u64 + 1;

/// Bounds for one flow discovery.
#[derive(Clone, Copy, Debug)]
pub struct FlowLimits {
    seeds: usize,
    subjects: usize,
    evidence: usize,
    fan_out: usize,
    hops: usize,
    candidates: usize,
    steps: usize,
}

impl Default for FlowLimits {
    fn default() -> Self {
        Self {
            seeds: FLOW_SEEDS_MAX,
            subjects: 128,
            evidence: 512,
            fan_out: 12,
            hops: 4,
            candidates: 4096,
            steps: 128,
        }
    }
}

/// A ranked symbol and its score from one retrieval.
#[derive(Clone, Copy, Debug)]
pub struct FlowSeed<Subject> {
    /// Stable identity of the ranked symbol.
    pub subject: Subject,
    /// Finite nonnegative score from the ranked retrieval.
    pub score: f64,
}

/// One direction of call adjacency to acquire.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FlowAdjacency<Subject> {
    /// Symbol whose adjacency is requested.
    pub subject: Subject,
    /// Direction in which discovery reaches neighbors.
    pub direction: HopDirection,
}

/// Whether discovery can continue from a neighbor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlowContinuation {
    /// The captured selection supports acquiring the neighbor's adjacency.
    Traverse,
    /// The captured selection establishes the endpoint alone.
    Terminal,
}

/// An accepted call to or from the requested symbol.
#[derive(Clone, Debug)]
pub struct FlowNeighbor<Subject, Evidence> {
    /// Stable identity of the other endpoint.
    pub subject: Subject,
    /// Whether the other endpoint can be expanded.
    pub continuation: FlowContinuation,
    /// Identity or payload of the original call-site evidence.
    pub evidence: Evidence,
}

/// Discovery bound reached while constructing the result.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum FlowCut {
    /// Additional ranked rows exceeded the seed allowance.
    Seeds,
    /// Additional endpoints exceeded the subject allowance.
    Subjects,
    /// Additional call sites exceeded the evidence allowance.
    Evidence,
    /// Additional neighbors exceeded the fan-out allowance.
    FanOut,
    /// Discovery reached a traversable endpoint at the hop allowance.
    Hops,
    /// Additional path expansions exceeded the candidate allowance.
    Candidates,
    /// The step allowance left a complete flow out of the result.
    Steps,
}

/// One path of directed call edges and its aggregate ranking score.
#[derive(Debug, PartialEq)]
pub struct FlowPath<Subject> {
    /// Ordered identities from the earliest reached caller to the final callee.
    /// Each adjacent pair is a directed call edge.
    pub steps: Vec<Subject>,
    /// The strongest seed score plus one quarter of the second distinct seed score.
    pub score: f64,
}

/// The complete paths selected from one bounded discovery.
#[derive(Debug)]
pub struct FlowResult<Subject, Evidence> {
    /// Paths in descending score order, with ties ordered by step identities.
    pub paths: Vec<FlowPath<Subject>>,
    /// Bounds reached during discovery or path selection.
    pub cuts: BTreeSet<FlowCut>,
    /// Distinct admitted subjects, including seeds and terminal endpoints.
    pub visited: usize,
    evidence: BTreeMap<(Subject, Subject), BTreeSet<Evidence>>,
}

impl<Subject: Ord, Evidence> FlowResult<Subject, Evidence> {
    /// Returns the original evidence associated with a directed hop.
    #[must_use]
    pub fn evidence(&self, from: Subject, to: Subject) -> Option<&BTreeSet<Evidence>> {
        self.evidence.get(&(from, to))
    }
}

#[derive(Debug)]
struct CallGraph<Subject, Evidence> {
    evidence: BTreeMap<(Subject, Subject), BTreeSet<Evidence>>,
    incoming: BTreeMap<Subject, BTreeSet<Subject>>,
    outgoing: BTreeMap<Subject, BTreeSet<Subject>>,
}

impl<Subject, Evidence> Default for CallGraph<Subject, Evidence> {
    fn default() -> Self {
        Self {
            evidence: BTreeMap::new(),
            incoming: BTreeMap::new(),
            outgoing: BTreeMap::new(),
        }
    }
}

impl<Subject: Clone + Ord, Evidence: Ord> CallGraph<Subject, Evidence> {
    fn exceeds_fan_out(&self, from: &Subject, to: &Subject, limit: usize) -> bool {
        !self.evidence.contains_key(&(from.clone(), to.clone()))
            && (self.outgoing.get(from).map_or(0, BTreeSet::len) >= limit
                || self.incoming.get(to).map_or(0, BTreeSet::len) >= limit)
    }

    fn insert(&mut self, from: Subject, to: Subject, evidence: Evidence) {
        self.evidence
            .entry((from.clone(), to.clone()))
            .or_default()
            .insert(evidence);
        self.incoming
            .entry(to.clone())
            .or_default()
            .insert(from.clone());
        self.outgoing.entry(from).or_default().insert(to);
    }

    fn neighbors(
        &self,
        subject: &Subject,
        direction: HopDirection,
    ) -> impl Iterator<Item = Subject> {
        let adjacency = match direction {
            HopDirection::Incoming => &self.incoming,
            HopDirection::Outgoing => &self.outgoing,
        };
        adjacency.get(subject).into_iter().flatten().cloned()
    }
}

/// A bounded state machine for call acquisition and path selection.
///
/// Each admitted subject is requested at most once per direction. Callers feed
/// adjacency in a deterministic order so a cut retains the same subset on replay.
#[derive(Debug)]
pub struct FlowDiscovery<Subject, Evidence> {
    limits: FlowLimits,
    seeds: BTreeMap<Subject, f64>,
    seed_order: Vec<Subject>,
    continuations: BTreeMap<Subject, FlowContinuation>,
    visited: BTreeSet<(Subject, HopDirection)>,
    frontier: VecDeque<(FlowAdjacency<Subject>, usize)>,
    pending: Option<(FlowAdjacency<Subject>, usize)>,
    graph: CallGraph<Subject, Evidence>,
    evidence_count: usize,
    cuts: BTreeSet<FlowCut>,
}

impl<Subject: Clone + Ord, Evidence: Ord> FlowDiscovery<Subject, Evidence> {
    /// Admits ranked rows in their original ranking order.
    #[must_use]
    pub fn new(seeds: impl IntoIterator<Item = FlowSeed<Subject>>, limits: FlowLimits) -> Self {
        let mut discovery = Self {
            limits,
            seeds: BTreeMap::new(),
            seed_order: Vec::new(),
            continuations: BTreeMap::new(),
            visited: BTreeSet::new(),
            frontier: VecDeque::new(),
            pending: None,
            graph: CallGraph::default(),
            evidence_count: 0,
            cuts: BTreeSet::new(),
        };
        for (index, seed) in seeds.into_iter().enumerate() {
            if index == limits.seeds {
                discovery.cuts.insert(FlowCut::Seeds);
                break;
            }
            discovery.admit_seed(seed);
        }
        discovery
    }

    fn admit_seed(&mut self, seed: FlowSeed<Subject>) {
        if !seed.score.is_finite() || seed.score < 0.0 {
            return;
        }
        if !self.admit_subject(seed.subject.clone(), FlowContinuation::Traverse) {
            return;
        }
        match self.seeds.entry(seed.subject.clone()) {
            Entry::Occupied(mut known) => {
                *known.get_mut() = known.get().max(seed.score);
            }
            Entry::Vacant(entry) => {
                entry.insert(seed.score);
                self.seed_order.push(seed.subject.clone());
                self.schedule(seed.subject.clone(), HopDirection::Incoming, 0);
                self.schedule(seed.subject, HopDirection::Outgoing, 0);
            }
        }
    }

    fn admit_subject(&mut self, subject: Subject, continuation: FlowContinuation) -> bool {
        if !self.continuations.contains_key(&subject)
            && self.continuations.len() == self.limits.subjects
        {
            self.cuts.insert(FlowCut::Subjects);
            return false;
        }
        let captured = self.continuations.entry(subject).or_insert(continuation);
        if continuation == FlowContinuation::Traverse {
            *captured = continuation;
        }
        true
    }

    fn schedule(&mut self, subject: Subject, direction: HopDirection, depth: usize) {
        if self.continuations.get(&subject) != Some(&FlowContinuation::Traverse) {
            return;
        }
        if self.visited.contains(&(subject.clone(), direction)) {
            return;
        }
        if depth == self.limits.hops {
            self.cuts.insert(FlowCut::Hops);
            return;
        }
        self.visited.insert((subject.clone(), direction));
        self.frontier
            .push_back((FlowAdjacency { subject, direction }, depth));
    }

    /// Selects the next unacquired direction, or returns the outstanding request.
    pub fn next_adjacency(&mut self) -> Option<FlowAdjacency<Subject>> {
        if self.pending.is_none() {
            self.pending = self.frontier.pop_front();
        }
        self.pending.as_ref().map(|(request, _)| request.clone())
    }

    /// Records neighbors in one acquired direction and schedules reachable subjects.
    ///
    /// # Panics
    /// The request must be the outstanding request returned by the discovery.
    #[track_caller]
    pub fn accept_adjacency(
        &mut self,
        request: FlowAdjacency<Subject>,
        neighbors: impl IntoIterator<Item = FlowNeighbor<Subject, Evidence>>,
    ) {
        let Some((pending, depth)) = self.pending.take() else {
            panic!("flow adjacency requires an outstanding discovery request");
        };
        let FlowAdjacency { subject, direction } = request;
        assert!(
            pending.subject == subject && pending.direction == direction,
            "flow adjacency must answer the outstanding request: pending_direction={:?}, supplied_direction={:?}",
            pending.direction,
            direction,
        );
        for (index, neighbor) in neighbors.into_iter().enumerate() {
            if index == self.limits.evidence {
                self.cuts.insert(FlowCut::Evidence);
                break;
            }
            self.admit_neighbor(&pending, neighbor, depth + 1);
        }
    }

    fn admit_neighbor(
        &mut self,
        request: &FlowAdjacency<Subject>,
        neighbor: FlowNeighbor<Subject, Evidence>,
        depth: usize,
    ) {
        let (from, to) = match request.direction {
            HopDirection::Incoming => (neighbor.subject.clone(), request.subject.clone()),
            HopDirection::Outgoing => (request.subject.clone(), neighbor.subject.clone()),
        };
        if from == to {
            return;
        }
        if self.graph.exceeds_fan_out(&from, &to, self.limits.fan_out) {
            self.cuts.insert(FlowCut::FanOut);
            return;
        }
        let repeated = self
            .graph
            .evidence
            .get(&(from.clone(), to.clone()))
            .is_some_and(|sites| sites.contains(&neighbor.evidence));
        if !repeated && self.evidence_count == self.limits.evidence {
            self.cuts.insert(FlowCut::Evidence);
            return;
        }
        if !self.admit_subject(neighbor.subject.clone(), neighbor.continuation) {
            return;
        }
        if !repeated {
            self.graph.insert(from, to, neighbor.evidence);
            self.evidence_count += 1;
        }
        self.schedule(neighbor.subject, request.direction, depth);
    }

    /// Ranks the discovered paths and selects whole records within both output bounds.
    ///
    /// # Panics
    /// All scheduled adjacency requests must have been answered.
    #[must_use]
    #[track_caller]
    pub fn finish(mut self, flow_limit: usize) -> FlowResult<Subject, Evidence> {
        assert!(
            self.pending.is_none() && self.frontier.is_empty(),
            "flow selection requires completed acquisition: pending={}, queued={}",
            self.pending.is_some(),
            self.frontier.len(),
        );
        let candidates = self.candidates();
        let ranked = self.rank(candidates);
        let mut steps = 0;
        let paths = ranked
            .into_iter()
            .take(flow_limit)
            .take_while(|path| {
                steps += path.steps.len();
                if steps > self.limits.steps {
                    self.cuts.insert(FlowCut::Steps);
                    return false;
                }
                true
            })
            .collect();
        FlowResult {
            paths,
            cuts: self.cuts,
            visited: self.continuations.len(),
            evidence: self.graph.evidence,
        }
    }
}

#[derive(Debug)]
struct PathExpansion<Subject> {
    steps: Vec<Subject>,
    direction: HopDirection,
}

impl<Subject: Clone + Ord, Evidence: Ord> FlowDiscovery<Subject, Evidence> {
    fn candidates(&mut self) -> PathsByLength<Subject> {
        let mut paths = BTreeMap::<_, BTreeSet<_>>::new();
        let mut queue: VecDeque<_> = self
            .seed_order
            .iter()
            .cloned()
            .map(|seed| PathExpansion {
                steps: vec![seed],
                direction: HopDirection::Outgoing,
            })
            .collect();
        let mut work = self.limits.candidates.saturating_sub(queue.len());
        while let Some(expansion) = queue.pop_front() {
            if let Some(cut) = self.expand_path(&mut queue, &mut work, &expansion) {
                self.cuts.insert(cut);
            }
            if expansion.direction == HopDirection::Incoming && expansion.steps.len() > 1 {
                paths
                    .entry(expansion.steps.len())
                    .or_default()
                    .insert(expansion.steps);
            }
        }
        paths
    }

    fn expand_path(
        &self,
        queue: &mut VecDeque<PathExpansion<Subject>>,
        work: &mut usize,
        expansion: &PathExpansion<Subject>,
    ) -> Option<FlowCut> {
        if expansion.direction == HopDirection::Outgoing {
            if *work == 0 {
                return Some(FlowCut::Candidates);
            }
            *work -= 1;
            queue.push_back(PathExpansion {
                steps: expansion.steps.clone(),
                direction: HopDirection::Incoming,
            });
        }
        let endpoint = match expansion.direction {
            HopDirection::Incoming => expansion.steps.first(),
            HopDirection::Outgoing => expansion.steps.last(),
        }?;
        if self.continuations.get(endpoint) != Some(&FlowContinuation::Traverse) {
            return None;
        }
        for neighbor in self.graph.neighbors(endpoint, expansion.direction) {
            if expansion.steps.contains(&neighbor) {
                continue;
            }
            if expansion.steps.len() > self.limits.hops {
                return Some(FlowCut::Hops);
            }
            if *work == 0 {
                return Some(FlowCut::Candidates);
            }
            *work -= 1;
            let mut steps = expansion.steps.clone();
            match expansion.direction {
                HopDirection::Incoming => steps.insert(0, neighbor),
                HopDirection::Outgoing => steps.push(neighbor),
            }
            queue.push_back(PathExpansion {
                steps,
                direction: expansion.direction,
            });
        }
        None
    }

    fn rank(&self, mut candidates: PathsByLength<Subject>) -> Vec<FlowPath<Subject>> {
        let mut ranked = Vec::new();
        while let Some((_, sequences)) = candidates.pop_last() {
            for steps in sequences {
                let Some(relevance) = self.relevance(&steps) else {
                    continue;
                };
                relevance.suppress_subpaths(&steps, &mut candidates);
                ranked.push(FlowPath {
                    steps,
                    score: relevance.score(),
                });
            }
        }
        ranked.sort_unstable_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.steps.cmp(&right.steps))
        });
        ranked
    }

    fn relevance(&self, steps: &[Subject]) -> Option<PathRelevance> {
        let mut matches = steps
            .iter()
            .enumerate()
            .filter_map(|(index, step)| self.seeds.get(step).map(|&score| (index, score)));
        let (first, strongest) = matches.next()?;
        let mut relevance = PathRelevance {
            first,
            last: first,
            strongest,
            second: 0.0,
        };
        for (index, score) in matches {
            relevance.last = index;
            if score > relevance.strongest {
                relevance.second = relevance.strongest;
                relevance.strongest = score;
            } else {
                relevance.second = relevance.second.max(score);
            }
        }
        Some(relevance)
    }
}

#[derive(Debug)]
struct PathRelevance {
    first: usize,
    last: usize,
    strongest: f64,
    second: f64,
}

impl PathRelevance {
    fn score(&self) -> f64 {
        (self.strongest + SECOND_SEED_WEIGHT * self.second).min(f64::MAX)
    }

    fn suppress_subpaths<Subject: Ord>(
        &self,
        steps: &[Subject],
        candidates: &mut PathsByLength<Subject>,
    ) {
        for start in 0..=self.first {
            self.suppress_suffixes(steps, start, candidates);
        }
    }

    fn suppress_suffixes<Subject: Ord>(
        &self,
        steps: &[Subject],
        start: usize,
        candidates: &mut PathsByLength<Subject>,
    ) {
        let end_min = (self.last + 1).max(start + 2);
        for end in end_min..=steps.len() {
            if let Some(same_length) = candidates.get_mut(&(end - start)) {
                same_length.remove(&steps[start..end]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FlowContinuation, FlowCut, FlowDiscovery, FlowLimits, FlowNeighbor, FlowPath, FlowResult,
        FlowSeed,
    };
    use rift_protocol::search::HopDirection;
    use std::collections::BTreeSet;

    type Call = (char, char, u16);

    fn discover(
        seed_scores: &[(char, f64)],
        calls: &[Call],
        terminals: &[char],
        limits: FlowLimits,
        flow_limit: usize,
    ) -> FlowResult<char, u16> {
        let seeds = seed_scores
            .iter()
            .map(|&(subject, score)| FlowSeed { subject, score });
        let mut discovery = FlowDiscovery::new(seeds, limits);
        let mut requested = BTreeSet::new();
        while let Some(request) = discovery.next_adjacency() {
            assert!(
                requested.insert((request.subject, request.direction)),
                "{request:?}"
            );
            assert_eq!(discovery.next_adjacency(), Some(request));
            assert!(!terminals.contains(&request.subject), "{request:?}");
            let neighbors = calls.iter().filter_map(|&(from, to, evidence)| {
                let subject = match request.direction {
                    HopDirection::Incoming if request.subject == to => from,
                    HopDirection::Outgoing if request.subject == from => to,
                    _ => return None,
                };
                let continuation = if terminals.contains(&subject) {
                    FlowContinuation::Terminal
                } else {
                    FlowContinuation::Traverse
                };
                Some(FlowNeighbor {
                    subject,
                    continuation,
                    evidence,
                })
            });
            discovery.accept_adjacency(request, neighbors);
        }
        discovery.finish(flow_limit)
    }

    fn path(steps: &str, score: f64) -> FlowPath<char> {
        FlowPath {
            steps: steps.chars().collect(),
            score,
        }
    }

    #[test]
    fn middle_seed_connects_both_directions_with_original_evidence() {
        let seeds = [('b', 4.0)];
        let calls = [
            ('a', 'b', 10),
            ('b', 'c', 20),
            ('b', 'c', 21),
            ('x', 'y', 30),
        ];
        let result = discover(&seeds, &calls, &[], FlowLimits::default(), 5);
        assert_eq!(result.paths, vec![path("abc", 4.0)]);
        assert_eq!(result.evidence('a', 'b'), Some(&BTreeSet::from([10])));
        assert_eq!(result.evidence('b', 'c'), Some(&BTreeSet::from([20, 21])));
        assert_eq!(result.evidence('x', 'y'), None);
        assert!(result.cuts.is_empty());
        assert_eq!(result.visited, 3);
    }

    #[test]
    fn distinct_paths_survive_deduplication_and_keep_stable_ties() {
        let seeds = [('a', 4.0), ('a', 4.0)];
        let calls = [('c', 'd', 3), ('b', 'd', 2), ('a', 'c', 1), ('a', 'b', 0)];
        let result = discover(&seeds, &calls, &[], FlowLimits::default(), 2);
        assert_eq!(result.paths, vec![path("abd", 4.0), path("acd", 4.0)]);
    }

    #[test]
    fn second_distinct_seed_breaks_a_strongest_score_tie() {
        let seeds = [('a', 4.0), ('d', 2.0)];
        let calls = [('a', 'b', 0), ('a', 'c', 1), ('c', 'd', 2)];
        let result = discover(&seeds, &calls, &[], FlowLimits::default(), 3);
        assert_eq!(
            result.paths,
            vec![path("acd", 4.5), path("ab", 4.0), path("ac", 4.0)]
        );
    }

    #[test]
    fn paths_never_repeat_a_symbol_and_ignore_self_calls() {
        let seeds = [('b', 2.0)];
        let calls = [('a', 'a', 0), ('a', 'b', 1), ('b', 'c', 2), ('c', 'a', 3)];
        let result = discover(&seeds, &calls, &[], FlowLimits::default(), 8);
        assert_eq!(
            result.paths,
            vec![path("abc", 2.0), path("bca", 2.0), path("cab", 2.0)]
        );
        assert_eq!(result.evidence('a', 'a'), None);
        assert!(result.cuts.is_empty());
    }

    #[test]
    fn external_terminal_stays_at_the_boundary_even_with_two_seeds() {
        let seeds = [('a', 4.0), ('c', 2.0)];
        let calls = [('a', 'b', 0), ('b', 'c', 1)];
        let result = discover(&seeds, &calls, &['b'], FlowLimits::default(), 5);
        assert_eq!(result.paths, vec![path("ab", 4.0), path("bc", 2.0)]);
    }

    #[test]
    fn disconnected_and_unrankable_seeds_have_no_paths() {
        let seeds = [('a', 2.0), ('b', f64::NAN), ('c', -1.0)];
        let calls = [('b', 'c', 0)];
        let result = discover(&seeds, &calls, &[], FlowLimits::default(), 5);
        assert!(result.paths.is_empty());
        assert!(result.cuts.is_empty());
    }

    #[test]
    fn discovery_cuts_are_observable_without_splitting_flows() {
        let seeds = [('b', 2.0), ('c', 1.0)];
        let calls = [('a', 'b', 0), ('b', 'c', 1), ('b', 'd', 2)];
        let limits = FlowLimits {
            seeds: 1,
            subjects: 3,
            ..FlowLimits::default()
        };
        let result = discover(&seeds, &calls, &[], limits, 5);
        assert_eq!(result.paths, vec![path("abc", 2.0)]);
        assert_eq!(
            result.cuts,
            BTreeSet::from([FlowCut::Seeds, FlowCut::Subjects])
        );

        let limits = FlowLimits {
            fan_out: 1,
            hops: 1,
            ..FlowLimits::default()
        };
        let result = discover(&seeds[..1], &calls, &[], limits, 5);
        assert_eq!(result.paths, vec![path("ab", 2.0), path("bc", 2.0)]);
        assert_eq!(
            result.cuts,
            BTreeSet::from([FlowCut::FanOut, FlowCut::Hops])
        );

        let limits = FlowLimits {
            steps: 2,
            ..FlowLimits::default()
        };
        let result = discover(&seeds[..1], &calls, &[], limits, 5);
        assert!(result.paths.is_empty());
        assert_eq!(result.cuts, BTreeSet::from([FlowCut::Steps]));
    }

    #[test]
    fn equal_evidence_does_not_spend_an_extra_record() {
        let seeds = [('a', 2.0), ('b', 1.0)];
        let calls = [('a', 'b', 0)];
        let limits = FlowLimits {
            evidence: 1,
            ..FlowLimits::default()
        };
        let result = discover(&seeds, &calls, &[], limits, 5);
        assert_eq!(result.paths, vec![path("ab", 2.25)]);
        assert_eq!(result.evidence('a', 'b'), Some(&BTreeSet::from([0])));
        assert!(result.cuts.is_empty());

        let calls = [('a', 'b', 0), ('a', 'c', 1)];
        let result = discover(&seeds, &calls, &[], limits, 5);
        assert_eq!(result.paths, vec![path("ab", 2.25)]);
        assert_eq!(result.cuts, BTreeSet::from([FlowCut::Evidence]));
    }

    #[test]
    fn candidate_budget_stops_branching_with_an_explicit_cut() {
        let seeds = [('a', 2.0)];
        let calls = [('a', 'b', 0), ('a', 'c', 1)];
        let limits = FlowLimits {
            candidates: 2,
            ..FlowLimits::default()
        };
        let result = discover(&seeds, &calls, &[], limits, 5);
        assert!(result.paths.is_empty());
        assert_eq!(result.cuts, BTreeSet::from([FlowCut::Candidates]));
    }

    #[test]
    fn combined_hop_cut_is_visible_when_both_directions_fit_separately() {
        let seeds = [('b', 2.0)];
        let calls = [
            ('a', 'b', 0),
            ('b', 'c', 1),
            ('c', 'd', 2),
            ('d', 'e', 3),
            ('e', 'f', 4),
        ];
        let result = discover(&seeds, &calls, &['f'], FlowLimits::default(), 5);
        assert_eq!(result.paths, vec![path("abcde", 2.0), path("bcdef", 2.0)]);
        assert_eq!(result.cuts, BTreeSet::from([FlowCut::Hops]));
    }

    #[test]
    fn every_three_symbol_graph_matches_a_permutation_oracle() {
        const EDGES: [Call; 6] = [
            ('a', 'b', 0),
            ('a', 'c', 1),
            ('b', 'a', 2),
            ('b', 'c', 3),
            ('c', 'a', 4),
            ('c', 'b', 5),
        ];
        const PATHS: [&str; 10] = [
            "ab", "abc", "acb", "ba", "bac", "bc", "bca", "cab", "cb", "cba",
        ];
        for mask in 0..(1 << EDGES.len()) {
            let calls: Vec<_> = EDGES
                .iter()
                .enumerate()
                .filter_map(|(index, &call)| (mask & (1 << index) != 0).then_some(call))
                .collect();
            let present = existing_paths(&PATHS, &calls);
            let expected: Vec<_> = present
                .iter()
                .filter(|candidate| {
                    !present
                        .iter()
                        .any(|other| other.len() > candidate.len() && other.contains(**candidate))
                })
                .map(|candidate| path(candidate, 1.0))
                .collect();
            let result = discover(&[('b', 1.0)], &calls, &[], FlowLimits::default(), 10);
            assert_eq!(result.paths, expected, "edge mask {mask:#08b}");
            assert!(result.cuts.is_empty(), "edge mask {mask:#08b}");
        }
    }

    fn existing_paths<'path>(paths: &[&'path str], calls: &[Call]) -> Vec<&'path str> {
        paths
            .iter()
            .copied()
            .filter(|path| {
                path.as_bytes().windows(2).all(|hop| {
                    calls.iter().any(|&(from, to, _)| {
                        from == char::from(hop[0]) && to == char::from(hop[1])
                    })
                })
            })
            .collect()
    }
}
