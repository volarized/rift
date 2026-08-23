//! The contract every language syntax provider serves.

use rift_core::{Error, ProjectPath};
use rift_protocol::read::{Language, NodeFacet};

use crate::document::SyntaxDocument;
use crate::failure::{SyntaxBound, SyntaxError, SyntaxFault};

/// Default maximum syntax nodes accepted from one parsed source.
pub(crate) const SYNTAX_NODES_MAX_DEFAULT: usize = 250_000;
/// Default maximum syntax depth accepted from one parsed source.
pub(crate) const SYNTAX_DEPTH_MAX_DEFAULT: usize = 512;

/// Source accepted to sans-I/O syntax analysis.
#[derive(Debug, Clone, Copy)]
pub struct SyntaxSource<'a> {
    /// Canonical project-relative path.
    pub path: &'a ProjectPath,
    /// UTF-8 source text.
    pub text: &'a str,
}

/// Bounded syntax acceptance limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_field_names)]
pub struct SyntaxLimits {
    source_bytes_max: usize,
    syntax_nodes_max: usize,
    syntax_depth_max: usize,
}

impl SyntaxLimits {
    /// Constructs positive syntax bounds.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxError`] naming the first zero bound.
    pub fn new(
        source_bytes_max: usize,
        syntax_nodes_max: usize,
        syntax_depth_max: usize,
    ) -> Result<Self, SyntaxError> {
        let bounds = [
            (source_bytes_max, SyntaxBound::SourceBytesMax),
            (syntax_nodes_max, SyntaxBound::SyntaxNodesMax),
            (syntax_depth_max, SyntaxBound::SyntaxDepthMax),
        ];
        for (value, bound) in bounds {
            if value == 0 {
                return Err(Error::new(SyntaxFault::ZeroLimit { bound }));
            }
        }
        Ok(Self {
            source_bytes_max,
            syntax_nodes_max,
            syntax_depth_max,
        })
    }

    /// Constructs bounds from a provider's declared constants.
    ///
    /// # Panics
    ///
    /// Panics when a declared bound is zero. A provider declares its default
    /// bounds in a `const` item, so the check evaluates at compile time and
    /// a zero constant fails the build, not a request.
    pub(crate) const fn declared(
        source_bytes_max: usize,
        syntax_nodes_max: usize,
        syntax_depth_max: usize,
    ) -> Self {
        assert!(
            source_bytes_max > 0,
            "a provider's declared source byte bound must be positive"
        );
        assert!(
            syntax_nodes_max > 0,
            "a provider's declared syntax node bound must be positive"
        );
        assert!(
            syntax_depth_max > 0,
            "a provider's declared syntax depth bound must be positive"
        );
        Self {
            source_bytes_max,
            syntax_nodes_max,
            syntax_depth_max,
        }
    }

    /// Returns maximum accepted source bytes.
    pub(crate) const fn source_bytes_max(self) -> usize {
        self.source_bytes_max
    }

    /// Returns maximum accepted syntax nodes.
    pub(crate) const fn syntax_nodes_max(self) -> usize {
        self.syntax_nodes_max
    }

    /// Returns maximum accepted syntax depth.
    pub(crate) const fn syntax_depth_max(self) -> usize {
        self.syntax_depth_max
    }
}

/// One language's bounded syntax fact provider.
///
/// Object safe: the registry serves shipped providers as trait objects,
/// selected by file extension or language identity.
pub trait SyntaxProvider: std::fmt::Debug + Send + Sync {
    /// The language identity this provider files facts under.
    fn language(&self) -> &Language;

    /// File extensions this provider claims, without their leading dot.
    /// Extensions are unique across the registry: the workspace walk
    /// includes a file as source exactly when some provider claims its
    /// extension.
    fn extensions(&self) -> &'static [&'static str];

    /// Maximum source bytes this provider accepts in one analysis.
    fn source_bytes_max(&self) -> usize;

    /// Parses source and extracts named nodes and declarations.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxError`] for incompatible grammar, cancellation, or an
    /// exceeded bound.
    fn analyze(&self, source: SyntaxSource<'_>) -> Result<SyntaxDocument, SyntaxError>;

    /// Portable structural facets for one grammar node kind.
    fn node_facets(&self, kind: &str) -> Vec<NodeFacet>;
}

#[cfg(test)]
mod tests {
    use rift_core::{ErrorCode, ErrorContext, ErrorName};

    use super::*;
    use crate::failure::SyntaxViolation;

    #[test]
    fn test_limits_reject_each_zero_bound_with_its_configuration_name() {
        let cases = [
            (SyntaxLimits::new(0, 1, 1), "source_bytes_max"),
            (SyntaxLimits::new(1, 0, 1), "syntax_nodes_max"),
            (SyntaxLimits::new(1, 1, 0), "syntax_depth_max"),
        ];
        for (result, bound_name) in cases {
            let error = result.expect_err("zero bound");
            assert_eq!(error.fault().violation(), SyntaxViolation::ZeroLimit);
            assert_eq!(
                error.descriptor().name(),
                ErrorName::Wire(ErrorCode::ConfigurationInvalid)
            );
            assert_eq!(
                error.context(),
                vec![ErrorContext::new("bound", bound_name)]
            );
        }
    }

    #[test]
    fn test_limits_accept_positive_bounds_from_both_constructors() {
        let accepted = SyntaxLimits::new(3, 4, 5).expect("positive bounds");
        assert_eq!(accepted, SyntaxLimits::declared(3, 4, 5));
        assert_eq!(accepted.source_bytes_max(), 3);
        assert_eq!(accepted.syntax_nodes_max(), 4);
        assert_eq!(accepted.syntax_depth_max(), 5);
    }
}
