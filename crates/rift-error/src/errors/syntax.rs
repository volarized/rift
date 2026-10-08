use rift_error::__rift_error_definition;

__rift_error_definition!(
    facts_count_exceeded,
    slug = "rift.syntax.facts_count_exceeded",
    message = "syntax facts exceed their node bound",
    action = "reduce facts below the accepted node bound and parse again",
    fields = {},
);

__rift_error_definition!(
    facts_depth_exceeded,
    slug = "rift.syntax.facts_depth_exceeded",
    message = "syntax facts exceed their parent depth bound",
    action = "reduce parent depth below the accepted syntax bound and parse again",
    fields = {},
);

__rift_error_definition!(
    facts_kind_invalid,
    slug = "rift.syntax.facts_kind_invalid",
    message = "syntax facts carry an unknown provider kind",
    action = "parse the source with the selected provider again",
    fields = {},
);

__rift_error_definition!(
    facts_name_invalid,
    slug = "rift.syntax.facts_name_invalid",
    message = "syntax facts carry an invalid declaration name",
    action = "parse the supplied source again",
    fields = {},
);

__rift_error_definition!(
    facts_order_invalid,
    slug = "rift.syntax.facts_order_invalid",
    message = "syntax facts are not in source order",
    action = "parse the supplied source again",
    fields = {},
);

__rift_error_definition!(
    facts_range_invalid,
    slug = "rift.syntax.facts_range_invalid",
    message = "syntax facts carry an invalid source range",
    action = "parse the supplied source again",
    fields = {},
);

__rift_error_definition!(
    facts_reference_invalid,
    slug = "rift.syntax.facts_reference_invalid",
    message = "syntax facts carry an invalid parent or reference",
    action = "parse the supplied source again",
    fields = {},
);

__rift_error_definition!(
    facts_source_mismatch,
    slug = "rift.syntax.facts_source_mismatch",
    message = "syntax facts source digest does not match",
    action = "parse the supplied source again",
    fields = {},
);

__rift_error_definition!(
    facts_source_too_large,
    slug = "rift.syntax.facts_source_too_large",
    message = "syntax facts source exceeds its byte bound",
    action = "reduce source bytes below the accepted bound and parse again",
    fields = {},
);

__rift_error_definition!(
    facts_structure_invalid,
    slug = "rift.syntax.facts_structure_invalid",
    message = "syntax facts carry an invalid source structure",
    action = "parse the supplied source again",
    fields = {},
);

__rift_error_definition!(
    facts_text_exceeded,
    slug = "rift.syntax.facts_text_exceeded",
    message = "syntax facts text exceeds its source byte bound",
    action = "reduce text below the accepted source byte bound and parse again",
    fields = {},
);

__rift_error_definition!(
    incompatible_grammar,
    slug = "rift.syntax.incompatible_grammar",
    message = "grammar ABI {grammar_abi_version} is outside runtime range {runtime_abi_min} to {runtime_abi_max}",
    action = "use a grammar built for the accepted runtime ABI range",
    fields = {
        grammar_abi_version: required(unsigned),
        runtime_abi_max: required(unsigned),
        runtime_abi_min: required(unsigned),
    },
);

__rift_error_definition!(
    invalid_markdown_ranges,
    slug = "rift.syntax.invalid_markdown_ranges",
    message = "Tree-sitter rejected Markdown inline ranges",
    action = "report this internal failure with its full context",
    fields = {
        path: required(path),
    },
);

__rift_error_definition!(
    invalid_query,
    slug = "rift.syntax.invalid_query",
    message = "syntax query is invalid at line {line_number}: {line_text}",
    action = "correct the query line and retry",
    fields = {
        line_number: required(unsigned),
        line_text: required(string),
        source: required(source),
    },
);

__rift_error_definition!(
    markdown_progress_exceeded,
    slug = "rift.syntax.markdown_progress_exceeded",
    message = "Markdown parser exceeded progress callback limit {progress_callbacks_max}",
    action = "reduce Markdown source size and retry",
    fields = {
        path: required(path),
        progress_callbacks_max: required(unsigned),
    },
);

__rift_error_definition!(
    parse_cancelled,
    slug = "rift.syntax.parse_cancelled",
    message = "syntax parser returned no tree",
    action = "retry syntax parsing",
    fields = {
        path: optional(path),
    },
);

__rift_error_definition!(
    position_overflow,
    slug = "rift.syntax.position_overflow",
    message = "syntax node position does not fit the accepted byte width",
    action = "report this internal failure with its full context",
    fields = {
        end_byte: required(unsigned),
        node_kind: required(string),
        source: required(source),
        start_byte: required(unsigned),
    },
);

__rift_error_definition!(
    source_too_large,
    slug = "rift.syntax.source_too_large",
    message = "source bytes {source_bytes} exceed accepted limit {source_bytes_max}",
    action = "reduce source bytes below {source_bytes_max} and retry",
    fields = {
        path: optional(path),
        source_bytes: required(unsigned),
        source_bytes_max: required(unsigned),
    },
);

__rift_error_definition!(
    too_deep,
    slug = "rift.syntax.too_deep",
    message = "syntax tree depth exceeds accepted limit {syntax_depth_max}",
    action = "reduce syntax tree depth below {syntax_depth_max} and retry",
    fields = {
        path: required(path),
        syntax_depth_max: required(unsigned),
    },
);

__rift_error_definition!(
    too_many_captures,
    slug = "rift.syntax.too_many_captures",
    message = "syntax query produced more than {captures_max} captures",
    action = "reduce query captures below {captures_max} and retry",
    fields = {
        captures_max: required(unsigned),
    },
);

__rift_error_definition!(
    too_many_markdown_inline_ranges,
    slug = "rift.syntax.too_many_markdown_inline_ranges",
    message = "Markdown inline ranges {observed} exceed accepted limit {inline_ranges_max}",
    action = "reduce Markdown inline ranges below {inline_ranges_max} and retry",
    fields = {
        inline_ranges_max: required(unsigned),
        observed: required(unsigned),
        path: required(path),
    },
);

__rift_error_definition!(
    too_many_nodes,
    slug = "rift.syntax.too_many_nodes",
    message = "syntax tree node count exceeds accepted limit {syntax_nodes_max}",
    action = "reduce syntax tree node count below {syntax_nodes_max} and retry",
    fields = {
        path: required(path),
        syntax_nodes_max: required(unsigned),
    },
);

__rift_error_definition!(
    unknown_node_kind,
    slug = "rift.syntax.unknown_node_kind",
    message = "node kind {node_kind} is outside interpreted grammar vocabulary",
    action = "report this internal failure with its full context",
    fields = {
        node_kind: required(string),
    },
);

__rift_error_definition!(
    zero_limit,
    slug = "rift.syntax.zero_limit",
    message = "syntax bound {bound} is zero",
    action = "set syntax bounds above zero",
    fields = {
        bound: required(string),
    },
);
