//! Syntax failure classification shared by every provider.

use rift_core::{Error, ErrorCode, ErrorContext, ErrorName, Fault, ProjectPath, fault_label};
use serde::Serialize;
use tree_sitter::{Node, QueryError};

/// Stable syntax failure classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyntaxViolation {
    /// Limit was configured as zero.
    ZeroLimit,
    /// Source exceeds byte bound.
    SourceTooLarge,
    /// Syntax tree exceeds node bound.
    TooManyNodes,
    /// Syntax tree exceeds depth bound.
    TooDeep,
    /// Grammar cannot be loaded by runtime.
    IncompatibleGrammar,
    /// Parser produced no tree.
    ParseCancelled,
    /// Platform position cannot fit wire width.
    PositionOverflow,
    /// Query is invalid for the pinned grammar.
    InvalidQuery,
    /// Query produced more captures than accepted.
    TooManyCaptures,
    /// Node kind is outside interpreted grammar vocabulary.
    UnknownNodeKind,
}

/// Configurable syntax bound named in diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyntaxBound {
    /// Accepted source bytes.
    SourceBytesMax,
    /// Accepted syntax nodes.
    SyntaxNodesMax,
    /// Accepted syntax depth.
    SyntaxDepthMax,
    /// Accepted query captures.
    CapturesMax,
}

/// Failure kind behind one [`SyntaxError`].
///
/// `path` is `None` when the failing source reached
/// [`RustQuery::captures`](crate::RustQuery::captures), which accepts raw
/// text without a project path.
#[derive(Debug, PartialEq, Eq)]
pub enum SyntaxFault {
    /// Limit was configured as zero.
    ZeroLimit {
        /// Bound configured as zero.
        bound: SyntaxBound,
    },
    /// Source exceeds byte bound.
    SourceTooLarge {
        /// Failing source path.
        path: Option<ProjectPath>,
        /// Observed source bytes.
        source_bytes: usize,
        /// Configured byte bound.
        source_bytes_max: usize,
    },
    /// Syntax tree exceeds node bound.
    TooManyNodes {
        /// Failing source path.
        path: ProjectPath,
        /// Configured node bound.
        syntax_nodes_max: usize,
    },
    /// Syntax tree exceeds depth bound.
    TooDeep {
        /// Failing source path.
        path: ProjectPath,
        /// Configured depth bound.
        syntax_depth_max: usize,
    },
    /// Grammar cannot be loaded by runtime.
    IncompatibleGrammar {
        /// ABI version compiled into grammar.
        grammar_abi_version: usize,
        /// Oldest ABI version runtime accepts.
        runtime_abi_min: usize,
        /// Newest ABI version runtime accepts.
        runtime_abi_max: usize,
    },
    /// Parser produced no tree.
    ParseCancelled {
        /// Failing source path.
        path: Option<ProjectPath>,
    },
    /// Platform position cannot fit wire width.
    PositionOverflow {
        /// Grammar kind of overflowing node.
        node_kind: &'static str,
        /// Node start byte.
        start_byte: usize,
        /// Node end byte.
        end_byte: usize,
        /// Failed integer conversion.
        source: std::num::TryFromIntError,
    },
    /// Query is invalid for the pinned grammar.
    InvalidQuery {
        /// One-based failing line number.
        line_number: usize,
        /// Failing query line text.
        line_text: String,
        /// Underlying Tree-sitter rejection.
        source: QueryError,
    },
    /// Query produced more captures than accepted.
    TooManyCaptures {
        /// Configured capture bound.
        captures_max: usize,
    },
    /// Node kind is outside interpreted grammar vocabulary.
    UnknownNodeKind {
        /// Unrecognized grammar kind string.
        kind: String,
    },
}

impl SyntaxFault {
    /// Returns stable failure classification.
    #[must_use]
    pub const fn violation(&self) -> SyntaxViolation {
        match self {
            Self::ZeroLimit { .. } => SyntaxViolation::ZeroLimit,
            Self::SourceTooLarge { .. } => SyntaxViolation::SourceTooLarge,
            Self::TooManyNodes { .. } => SyntaxViolation::TooManyNodes,
            Self::TooDeep { .. } => SyntaxViolation::TooDeep,
            Self::IncompatibleGrammar { .. } => SyntaxViolation::IncompatibleGrammar,
            Self::ParseCancelled { .. } => SyntaxViolation::ParseCancelled,
            Self::PositionOverflow { .. } => SyntaxViolation::PositionOverflow,
            Self::InvalidQuery { .. } => SyntaxViolation::InvalidQuery,
            Self::TooManyCaptures { .. } => SyntaxViolation::TooManyCaptures,
            Self::UnknownNodeKind { .. } => SyntaxViolation::UnknownNodeKind,
        }
    }
}

impl Fault for SyntaxFault {
    fn name(&self) -> ErrorName {
        match self {
            Self::ZeroLimit { .. } => ErrorName::Wire(ErrorCode::ConfigurationInvalid),
            Self::SourceTooLarge { .. }
            | Self::TooManyNodes { .. }
            | Self::TooDeep { .. }
            | Self::TooManyCaptures { .. } => ErrorName::Wire(ErrorCode::LimitExceeded),
            Self::ParseCancelled { .. } => ErrorName::Wire(ErrorCode::Cancelled),
            Self::IncompatibleGrammar { .. }
            | Self::PositionOverflow { .. }
            | Self::InvalidQuery { .. }
            | Self::UnknownNodeKind { .. } => ErrorName::Wire(ErrorCode::InternalError),
        }
    }

    fn context(&self) -> Vec<ErrorContext> {
        match self {
            Self::ZeroLimit { bound } => {
                vec![ErrorContext::new("bound", fault_label(bound))]
            }
            Self::SourceTooLarge {
                path,
                source_bytes,
                source_bytes_max,
            } => vec![
                ErrorContext::new(
                    "path",
                    path.as_ref()
                        .map_or_else(|| "<raw text>".to_string(), ToString::to_string),
                ),
                ErrorContext::new("source_bytes", source_bytes.to_string()),
                ErrorContext::new("source_bytes_max", source_bytes_max.to_string()),
            ],
            Self::TooManyNodes {
                path,
                syntax_nodes_max,
            } => vec![
                ErrorContext::new("path", path.to_string()),
                ErrorContext::new("syntax_nodes_max", syntax_nodes_max.to_string()),
            ],
            Self::TooDeep {
                path,
                syntax_depth_max,
            } => vec![
                ErrorContext::new("path", path.to_string()),
                ErrorContext::new("syntax_depth_max", syntax_depth_max.to_string()),
            ],
            Self::IncompatibleGrammar {
                grammar_abi_version,
                runtime_abi_min,
                runtime_abi_max,
            } => vec![
                ErrorContext::new("grammar_abi_version", grammar_abi_version.to_string()),
                ErrorContext::new("runtime_abi_min", runtime_abi_min.to_string()),
                ErrorContext::new("runtime_abi_max", runtime_abi_max.to_string()),
            ],
            Self::ParseCancelled { path } => path.as_ref().map_or_else(Vec::new, |path| {
                vec![ErrorContext::new("path", path.to_string())]
            }),
            Self::PositionOverflow {
                node_kind,
                start_byte,
                end_byte,
                source: _,
            } => vec![
                ErrorContext::new("node_kind", *node_kind),
                ErrorContext::new("start_byte", start_byte.to_string()),
                ErrorContext::new("end_byte", end_byte.to_string()),
            ],
            Self::InvalidQuery {
                line_number,
                line_text,
                source: _,
            } => vec![
                ErrorContext::new("line_number", line_number.to_string()),
                ErrorContext::new("line_text", line_text.clone()),
            ],
            Self::TooManyCaptures { captures_max } => {
                vec![ErrorContext::new("captures_max", captures_max.to_string())]
            }
            Self::UnknownNodeKind { kind } => {
                vec![ErrorContext::new("node_kind", kind.clone())]
            }
        }
    }

    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::PositionOverflow { source, .. } => Some(source),
            Self::InvalidQuery { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Opaque syntax failure.
pub type SyntaxError = Error<SyntaxFault>;

/// Classifies a grammar the pinned Tree-sitter runtime refuses to load.
pub(crate) fn incompatible_grammar(language: &tree_sitter::Language) -> SyntaxError {
    Error::new(SyntaxFault::IncompatibleGrammar {
        grammar_abi_version: language.abi_version(),
        runtime_abi_min: tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION,
        runtime_abi_max: tree_sitter::LANGUAGE_VERSION,
    })
}

/// Classifies a node position that cannot fit the wire byte width.
pub(crate) fn position_overflow(node: Node<'_>, source: std::num::TryFromIntError) -> SyntaxError {
    Error::new(SyntaxFault::PositionOverflow {
        node_kind: node.kind(),
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        source,
    })
}

/// Classifies a query the pinned grammar rejects, naming the failing line.
pub(crate) fn invalid_query(query_source: &str, source: QueryError) -> SyntaxError {
    Error::new(SyntaxFault::InvalidQuery {
        line_number: source.row + 1,
        line_text: query_source.lines().nth(source.row).unwrap_or("").into(),
        source,
    })
}
