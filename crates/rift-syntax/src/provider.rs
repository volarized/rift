//! The contract every language syntax provider serves.

use rift_core::{Error, ProjectPath};
use rift_protocol::configuration::{
    SYNTAX_DEPTH_DEFAULT, SYNTAX_FILE_BYTES_DEFAULT, SYNTAX_NODES_DEFAULT, SyntaxConfiguration,
};
use rift_protocol::read::{Language, NodeFacet};

use crate::document::SyntaxDocument;
use crate::failure::{SyntaxBound, SyntaxError, SyntaxFault};

/// Bytes accepted from one source under the default `[providers.syntax]` table.
pub(crate) const SOURCE_BYTES_MAX_DEFAULT: usize = bound(SYNTAX_FILE_BYTES_DEFAULT);
/// Syntax nodes accepted from one source under the default `[providers.syntax]` table.
pub(crate) const SYNTAX_NODES_MAX_DEFAULT: usize = bound(SYNTAX_NODES_DEFAULT);
/// Syntax depth accepted from one source under the default `[providers.syntax]` table.
pub(crate) const SYNTAX_DEPTH_MAX_DEFAULT: usize = bound(SYNTAX_DEPTH_DEFAULT);

/// One configured bound as an in-memory count. A bound past the address space
/// saturates: no source larger than memory can be parsed anyway, and
/// configuration acceptance keeps every key far below it.
const fn bound(value: u64) -> usize {
    if value > usize::MAX as u64 {
        usize::MAX
    } else {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the branch above proves the value fits in usize"
        )]
        let fitted = value as usize;
        fitted
    }
}

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

    /// The bounds every provider parses under unless the caller supplies others.
    pub const DEFAULT: Self = Self::declared(
        SOURCE_BYTES_MAX_DEFAULT,
        SYNTAX_NODES_MAX_DEFAULT,
        SYNTAX_DEPTH_MAX_DEFAULT,
    );

    /// The bounds one `[providers.syntax]` table states.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxError`] naming the first zero bound; configuration
    /// acceptance refuses such a table before it reaches this call.
    pub fn from_configuration(configuration: &SyntaxConfiguration) -> Result<Self, SyntaxError> {
        Self::new(
            bound(configuration.max_file.bytes()),
            bound(configuration.max_nodes),
            bound(configuration.max_depth),
        )
    }

    /// Constructs bounds from compile-time constants.
    ///
    /// # Panics
    ///
    /// Panics when a bound is zero. [`Self::DEFAULT`] evaluates it in a
    /// `const` item, so a zero default fails the build, not a request.
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
    #[must_use]
    pub const fn source_bytes_max(self) -> usize {
        self.source_bytes_max
    }

    /// Returns maximum accepted syntax nodes.
    #[must_use]
    pub const fn syntax_nodes_max(self) -> usize {
        self.syntax_nodes_max
    }

    /// Returns maximum accepted syntax depth.
    #[must_use]
    pub const fn syntax_depth_max(self) -> usize {
        self.syntax_depth_max
    }

    /// Refuses a source larger than these bounds accept, before any parse.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxError`] naming the source's size and the bound when it is larger.
    pub(crate) fn admit_source(self, source: SyntaxSource<'_>) -> Result<(), SyntaxError> {
        if source.text.len() > self.source_bytes_max {
            return Err(Error::new(SyntaxFault::SourceTooLarge {
                path: Some(source.path.clone()),
                source_bytes: source.text.len(),
                source_bytes_max: self.source_bytes_max,
            }));
        }
        Ok(())
    }
}

impl Default for SyntaxLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// One language's bounded syntax fact provider.
///
/// Object safe: the registry serves shipped providers as trait objects,
/// selected by file extension or language identity.
pub trait SyntaxProvider: std::fmt::Debug + Send + Sync {
    /// The language identity this provider files facts under.
    fn language(&self) -> &Language;

    /// Parses source under `limits` and extracts named nodes and declarations.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxError`] for incompatible grammar, cancellation, or an
    /// exceeded bound.
    fn analyze(
        &self,
        source: SyntaxSource<'_>,
        limits: SyntaxLimits,
    ) -> Result<SyntaxDocument, SyntaxError>;

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

    #[test]
    fn test_from_configuration_reads_every_key_and_defaults_match_the_table() {
        use rift_protocol::configuration::ByteSize;

        assert_eq!(
            SyntaxLimits::from_configuration(&SyntaxConfiguration::default()),
            Ok(SyntaxLimits::DEFAULT)
        );
        let configured = SyntaxLimits::from_configuration(&SyntaxConfiguration {
            max_file: ByteSize::from_bytes(16 << 20),
            max_nodes: 5_000_000,
            max_depth: 2_048,
        })
        .expect("positive bounds");
        assert_eq!(
            configured,
            SyntaxLimits::new(16 << 20, 5_000_000, 2_048).expect("bounds")
        );
        let zero = SyntaxConfiguration {
            max_nodes: 0,
            ..SyntaxConfiguration::default()
        };
        assert!(SyntaxLimits::from_configuration(&zero).is_err());
    }
}
