use rift_error::__rift_error_definition;

__rift_error_definition!(
    capabilities_incompatible,
    slug = "rift.ranking.capabilities_incompatible",
    message = "index capabilities cannot be combined for ranking",
    action = "report this internal failure with its full context",
    fields = {},
);

__rift_error_definition!(
    document_field_length,
    slug = "rift.ranking.document_field_length",
    message = "document field {subject} exceeds its accepted byte limit of {limit} bytes",
    action = "report this internal failure with its full context",
    fields = {
        field: required(string),
        limit: required(unsigned),
        required: required(unsigned),
        subject: required(string),
    },
);

__rift_error_definition!(
    document_identity_empty,
    slug = "rift.ranking.document_identity_empty",
    message = "document identity is empty",
    action = "report this internal failure with its full context",
    fields = {
        subject: optional(string),
    },
);

__rift_error_definition!(
    fusion_constant_invalid,
    slug = "rift.ranking.fusion_constant_invalid",
    message = "ranking constant is outside its accepted range",
    action = "set the ranking constant within its accepted range, then retry",
    fields = {
        subject: required(string),
    },
);

__rift_error_definition!(
    pattern_size,
    slug = "rift.ranking.pattern_size",
    message = "compiled pattern exceeds its accepted size: {subject}",
    action = "reduce the compiled pattern size and retry",
    fields = {
        subject: required(string),
    },
);

__rift_error_definition!(
    pattern_syntax,
    slug = "rift.ranking.pattern_syntax",
    message = "pattern syntax is invalid: {subject}",
    action = "correct the pattern syntax and retry",
    fields = {
        subject: required(string),
    },
);

__rift_error_definition!(
    query_empty,
    slug = "rift.ranking.query_empty",
    message = "query is empty",
    action = "provide query text and resend the request",
    fields = {
        subject: optional(string),
    },
);

__rift_error_definition!(
    query_length,
    slug = "rift.ranking.query_length",
    message = "query exceeds its accepted byte limit of {limit} bytes",
    action = "shorten the query below {limit} bytes and resend the request",
    fields = {
        field: required(string),
        limit: required(unsigned),
        required: required(unsigned),
        subject: required(string),
    },
);

__rift_error_definition!(
    query_phrase_limit,
    slug = "rift.ranking.query_phrase_limit",
    message = "query contains more quoted phrases than the accepted limit of {limit}",
    action = "reduce quoted phrases to {limit} and resend the request",
    fields = {
        field: required(string),
        limit: required(unsigned),
        required: required(unsigned),
        subject: required(string),
    },
);

__rift_error_definition!(
    query_quote_unterminated,
    slug = "rift.ranking.query_quote_unterminated",
    message = "query contains an unclosed quote",
    action = "close every quoted phrase and resend the request",
    fields = {
        subject: optional(string),
    },
);

__rift_error_definition!(
    query_term_length,
    slug = "rift.ranking.query_term_length",
    message = "query term exceeds its accepted byte limit of {limit} bytes",
    action = "shorten the query term below {limit} bytes and resend the request",
    fields = {
        field: required(string),
        limit: required(unsigned),
        required: required(unsigned),
        subject: required(string),
    },
);

__rift_error_definition!(
    ranking_weights_invalid,
    slug = "rift.ranking.ranking_weights_invalid",
    message = "ranking shares must be finite values from zero to one with a positive sum",
    action = "set finite ranking shares from zero to one with a positive sum, then retry",
    fields = {
        subject: required(string),
    },
);

__rift_error_definition!(
    reader_failed,
    slug = "rift.ranking.reader_failed",
    message = "search reader failed",
    action = "check filesystem permissions and free space, then retry",
    fields = {
        source: required(source),
        subject: optional(string),
    },
);
