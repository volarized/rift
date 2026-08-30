//! Bounds every binding phase enforces, and the limit each breach names.

use rift_core::CONTRIBUTION_FACTS_MAX;
use serde::Serialize;

use crate::failure::{BindingError, BindingViolation, binding_error};

/// Default scopes one unit may hold.
pub const UNIT_SCOPES_MAX_DEFAULT: usize = 4096;
/// Default definitions one unit may hold.
pub const UNIT_DEFINITIONS_MAX_DEFAULT: usize = 16_384;
/// Default references one unit may hold.
pub const UNIT_REFERENCES_MAX_DEFAULT: usize = 65_536;
/// Default links one unit may hold.
pub const UNIT_LINKS_MAX_DEFAULT: usize = 4096;
/// Default scopes, definitions, and references one graph may hold together.
pub const GRAPH_NODES_MAX_DEFAULT: usize = 2_000_000;
/// Default links one graph may hold.
pub const GRAPH_LINKS_MAX_DEFAULT: usize = 500_000;
/// Default work items one reference may enqueue.
pub const REFERENCE_WORK_MAX_DEFAULT: usize = 4096;
/// Default steps one work item may accumulate.
pub const PATH_DEPTH_MAX_DEFAULT: usize = 64;
/// Default complete paths one reference may collect.
pub const REFERENCE_TARGETS_MAX_DEFAULT: usize = 64;
/// Default work items one publication may enqueue across every reference.
pub const PUBLICATION_WORK_MAX_DEFAULT: usize = 50_000_000;

const _: () = assert!(
    REFERENCE_TARGETS_MAX_DEFAULT <= CONTRIBUTION_FACTS_MAX,
    "the targets of one reference must fit one Contribution's reference facts"
);

/// The bound one binding phase ran out of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExhaustedLimit {
    /// Scopes in one unit.
    UnitScopes,
    /// Definitions in one unit.
    UnitDefinitions,
    /// References in one unit.
    UnitReferences,
    /// Links in one unit.
    UnitLinks,
    /// Scopes, definitions, and references in the whole graph.
    GraphNodes,
    /// Links in the whole graph.
    GraphLinks,
    /// Work items enqueued for one reference.
    ReferenceWork,
    /// Steps accumulated by one work item, or the module chain above one scope.
    PathDepth,
    /// Complete paths collected for one reference.
    ReferenceTargets,
    /// Work items enqueued across one publication.
    PublicationWork,
}

/// Every bound the builder, the linker, and the resolver enforce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindingLimits {
    unit_scopes: usize,
    unit_definitions: usize,
    unit_references: usize,
    unit_links: usize,
    graph_nodes: usize,
    graph_links: usize,
    reference_work: usize,
    path_depth: usize,
    reference_targets: usize,
    publication_work: usize,
}

impl BindingLimits {
    /// Starts from the default bounds.
    #[must_use]
    pub fn builder() -> BindingLimitsBuilder {
        BindingLimitsBuilder::default()
    }

    /// Most scopes one unit may hold.
    #[must_use]
    pub const fn unit_scopes_max(&self) -> usize {
        self.unit_scopes
    }

    /// Most definitions one unit may hold.
    #[must_use]
    pub const fn unit_definitions_max(&self) -> usize {
        self.unit_definitions
    }

    /// Most references one unit may hold.
    #[must_use]
    pub const fn unit_references_max(&self) -> usize {
        self.unit_references
    }

    /// Most links one unit may hold.
    #[must_use]
    pub const fn unit_links_max(&self) -> usize {
        self.unit_links
    }

    /// Most scopes, definitions, and references the graph may hold together.
    #[must_use]
    pub const fn graph_nodes_max(&self) -> usize {
        self.graph_nodes
    }

    /// Most links the graph may hold.
    #[must_use]
    pub const fn graph_links_max(&self) -> usize {
        self.graph_links
    }

    /// Most work items one reference may enqueue.
    #[must_use]
    pub const fn reference_work_max(&self) -> usize {
        self.reference_work
    }

    /// Most steps one work item may accumulate, and the deepest module chain.
    #[must_use]
    pub const fn path_depth_max(&self) -> usize {
        self.path_depth
    }

    /// Most complete paths one reference may collect.
    #[must_use]
    pub const fn reference_targets_max(&self) -> usize {
        self.reference_targets
    }

    /// Most work items one publication may enqueue across every reference.
    #[must_use]
    pub const fn publication_work_max(&self) -> usize {
        self.publication_work
    }

    /// The first bound that is zero, in declaration order.
    fn zero_limit(&self) -> Option<ExhaustedLimit> {
        let table = [
            (ExhaustedLimit::UnitScopes, self.unit_scopes),
            (ExhaustedLimit::UnitDefinitions, self.unit_definitions),
            (ExhaustedLimit::UnitReferences, self.unit_references),
            (ExhaustedLimit::UnitLinks, self.unit_links),
            (ExhaustedLimit::GraphNodes, self.graph_nodes),
            (ExhaustedLimit::GraphLinks, self.graph_links),
            (ExhaustedLimit::ReferenceWork, self.reference_work),
            (ExhaustedLimit::PathDepth, self.path_depth),
            (ExhaustedLimit::ReferenceTargets, self.reference_targets),
            (ExhaustedLimit::PublicationWork, self.publication_work),
        ];
        table
            .into_iter()
            .find(|(_, value)| *value == 0)
            .map(|(limit, _)| limit)
    }
}

impl Default for BindingLimits {
    fn default() -> Self {
        Self {
            unit_scopes: UNIT_SCOPES_MAX_DEFAULT,
            unit_definitions: UNIT_DEFINITIONS_MAX_DEFAULT,
            unit_references: UNIT_REFERENCES_MAX_DEFAULT,
            unit_links: UNIT_LINKS_MAX_DEFAULT,
            graph_nodes: GRAPH_NODES_MAX_DEFAULT,
            graph_links: GRAPH_LINKS_MAX_DEFAULT,
            reference_work: REFERENCE_WORK_MAX_DEFAULT,
            path_depth: PATH_DEPTH_MAX_DEFAULT,
            reference_targets: REFERENCE_TARGETS_MAX_DEFAULT,
            publication_work: PUBLICATION_WORK_MAX_DEFAULT,
        }
    }
}

/// Composes [`BindingLimits`] from the defaults, one bound at a time.
#[derive(Debug, Clone, Copy, Default)]
pub struct BindingLimitsBuilder {
    limits: BindingLimits,
}

impl BindingLimitsBuilder {
    /// Sets the most scopes one unit may hold.
    #[must_use]
    pub const fn unit_scopes_max(mut self, value: usize) -> Self {
        self.limits.unit_scopes = value;
        self
    }

    /// Sets the most definitions one unit may hold.
    #[must_use]
    pub const fn unit_definitions_max(mut self, value: usize) -> Self {
        self.limits.unit_definitions = value;
        self
    }

    /// Sets the most references one unit may hold.
    #[must_use]
    pub const fn unit_references_max(mut self, value: usize) -> Self {
        self.limits.unit_references = value;
        self
    }

    /// Sets the most links one unit may hold.
    #[must_use]
    pub const fn unit_links_max(mut self, value: usize) -> Self {
        self.limits.unit_links = value;
        self
    }

    /// Sets the most scopes, definitions, and references the graph may hold together.
    #[must_use]
    pub const fn graph_nodes_max(mut self, value: usize) -> Self {
        self.limits.graph_nodes = value;
        self
    }

    /// Sets the most links the graph may hold.
    #[must_use]
    pub const fn graph_links_max(mut self, value: usize) -> Self {
        self.limits.graph_links = value;
        self
    }

    /// Sets the most work items one reference may enqueue.
    #[must_use]
    pub const fn reference_work_max(mut self, value: usize) -> Self {
        self.limits.reference_work = value;
        self
    }

    /// Sets the most steps one work item may accumulate.
    #[must_use]
    pub const fn path_depth_max(mut self, value: usize) -> Self {
        self.limits.path_depth = value;
        self
    }

    /// Sets the most complete paths one reference may collect.
    #[must_use]
    pub const fn reference_targets_max(mut self, value: usize) -> Self {
        self.limits.reference_targets = value;
        self
    }

    /// Sets the most work items one publication may enqueue.
    #[must_use]
    pub const fn publication_work_max(mut self, value: usize) -> Self {
        self.limits.publication_work = value;
        self
    }

    /// Validates every bound.
    ///
    /// # Errors
    ///
    /// Returns [`BindingViolation::ZeroLimit`] naming the first bound that is zero.
    pub fn build(self) -> Result<BindingLimits, BindingError> {
        match self.limits.zero_limit() {
            Some(limit) => Err(binding_error(
                BindingViolation::ZeroLimit(limit),
                "every bound must be at least 1",
            )),
            None => Ok(self.limits),
        }
    }
}

#[cfg(test)]
mod tests {
    use rift_core::fault_label;

    use super::{
        BindingLimits, BindingLimitsBuilder, ExhaustedLimit, GRAPH_LINKS_MAX_DEFAULT,
        GRAPH_NODES_MAX_DEFAULT, PATH_DEPTH_MAX_DEFAULT, PUBLICATION_WORK_MAX_DEFAULT,
        REFERENCE_TARGETS_MAX_DEFAULT, REFERENCE_WORK_MAX_DEFAULT, UNIT_DEFINITIONS_MAX_DEFAULT,
        UNIT_LINKS_MAX_DEFAULT, UNIT_REFERENCES_MAX_DEFAULT, UNIT_SCOPES_MAX_DEFAULT,
    };
    use crate::failure::BindingViolation;

    type Setter = fn(BindingLimitsBuilder, usize) -> BindingLimitsBuilder;

    const SETTERS: [(ExhaustedLimit, Setter); 10] = [
        (
            ExhaustedLimit::UnitScopes,
            BindingLimitsBuilder::unit_scopes_max,
        ),
        (
            ExhaustedLimit::UnitDefinitions,
            BindingLimitsBuilder::unit_definitions_max,
        ),
        (
            ExhaustedLimit::UnitReferences,
            BindingLimitsBuilder::unit_references_max,
        ),
        (
            ExhaustedLimit::UnitLinks,
            BindingLimitsBuilder::unit_links_max,
        ),
        (
            ExhaustedLimit::GraphNodes,
            BindingLimitsBuilder::graph_nodes_max,
        ),
        (
            ExhaustedLimit::GraphLinks,
            BindingLimitsBuilder::graph_links_max,
        ),
        (
            ExhaustedLimit::ReferenceWork,
            BindingLimitsBuilder::reference_work_max,
        ),
        (
            ExhaustedLimit::PathDepth,
            BindingLimitsBuilder::path_depth_max,
        ),
        (
            ExhaustedLimit::ReferenceTargets,
            BindingLimitsBuilder::reference_targets_max,
        ),
        (
            ExhaustedLimit::PublicationWork,
            BindingLimitsBuilder::publication_work_max,
        ),
    ];

    #[test]
    fn test_limits_default_matches_named_constants() {
        let limits = BindingLimits::default();
        assert_eq!(limits.unit_scopes_max(), UNIT_SCOPES_MAX_DEFAULT);
        assert_eq!(limits.unit_definitions_max(), UNIT_DEFINITIONS_MAX_DEFAULT);
        assert_eq!(limits.unit_references_max(), UNIT_REFERENCES_MAX_DEFAULT);
        assert_eq!(limits.unit_links_max(), UNIT_LINKS_MAX_DEFAULT);
        assert_eq!(limits.graph_nodes_max(), GRAPH_NODES_MAX_DEFAULT);
        assert_eq!(limits.graph_links_max(), GRAPH_LINKS_MAX_DEFAULT);
        assert_eq!(limits.reference_work_max(), REFERENCE_WORK_MAX_DEFAULT);
        assert_eq!(limits.path_depth_max(), PATH_DEPTH_MAX_DEFAULT);
        assert_eq!(
            limits.reference_targets_max(),
            REFERENCE_TARGETS_MAX_DEFAULT
        );
        assert_eq!(limits.publication_work_max(), PUBLICATION_WORK_MAX_DEFAULT);
    }

    #[test]
    fn test_limits_builder_zero_refused_for_every_bound() {
        for (limit, setter) in SETTERS {
            let error = setter(BindingLimits::builder(), 0).build();
            let violation = error.as_ref().map_err(|error| error.fault().violation());
            assert_eq!(violation, Err(BindingViolation::ZeroLimit(limit)));
        }
    }

    #[test]
    fn test_limits_builder_one_accepted_for_every_bound() {
        for (_, setter) in SETTERS {
            let limits = setter(BindingLimits::builder(), 1).build();
            assert!(
                limits.is_ok(),
                "a bound of one is the smallest accepted value"
            );
        }
    }

    #[test]
    fn test_limits_builder_default_build_accepted() {
        let built = BindingLimits::builder().build();
        assert_eq!(built.ok(), Some(BindingLimits::default()));
    }

    #[test]
    fn test_exhausted_limit_labels_are_snake_case() {
        assert_eq!(fault_label(&ExhaustedLimit::UnitScopes), "unit_scopes");
        assert_eq!(
            fault_label(&ExhaustedLimit::PublicationWork),
            "publication_work"
        );
        assert!(ExhaustedLimit::UnitScopes < ExhaustedLimit::PublicationWork);
    }
}
