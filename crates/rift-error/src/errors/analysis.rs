use rift_error::__rift_error_definition;

__rift_error_definition!(
    context7_entry_invalid,
    slug = "rift.analysis.context7_entry_invalid",
    message = "context7.json key {key} contains an invalid entry",
    action = "correct the entry for key {key} and retry",
    fields = {
        entry: optional(string),
        file: required(string),
        key: optional(string),
        source: optional(source),
    },
);

__rift_error_definition!(
    context7_malformed,
    slug = "rift.analysis.context7_malformed",
    message = "context7.json has an invalid JSON shape",
    action = "correct context7.json shape and retry",
    fields = {
        file: required(string),
        key: optional(string),
        source: optional(source),
    },
);

__rift_error_definition!(
    context7_oversized,
    slug = "rift.analysis.context7_oversized",
    message = "context7.json exceeds its accepted byte limit",
    action = "reduce context7.json below its accepted byte limit and retry",
    fields = {
        file: required(string),
        key: optional(string),
        source: optional(source),
    },
);

__rift_error_definition!(
    context7_too_many_entries,
    slug = "rift.analysis.context7_too_many_entries",
    message = "context7.json key {key} exceeds its entry limit",
    action = "reduce entries for key {key} below its accepted limit and retry",
    fields = {
        file: required(string),
        key: optional(string),
        source: optional(source),
    },
);

__rift_error_definition!(
    documentation_digest_mismatch,
    slug = "rift.analysis.documentation_digest_mismatch",
    message = "documentation field {field} has a digest mismatch",
    action = "supply bytes matching the recorded digest and retry",
    fields = {
        field: required(string),
        source: optional(source),
    },
);

__rift_error_definition!(
    documentation_duplicate_source,
    slug = "rift.analysis.documentation_duplicate_source",
    message = "documentation field {field} names a duplicate source",
    action = "send each documentation source once and retry",
    fields = {
        field: required(string),
        source: optional(source),
    },
);

__rift_error_definition!(
    documentation_encoding_failed,
    slug = "rift.analysis.documentation_encoding_failed",
    message = "documentation field {field} could not be encoded as canonical JSON",
    action = "report this internal failure with its full context",
    fields = {
        field: required(string),
        source: optional(source),
    },
);

__rift_error_definition!(
    documentation_format_invalid,
    slug = "rift.analysis.documentation_format_invalid",
    message = "documentation field {field} has an invalid format",
    action = "correct documentation field {field} and retry",
    fields = {
        field: required(string),
        source: optional(source),
    },
);

__rift_error_definition!(
    documentation_identity_invalid,
    slug = "rift.analysis.documentation_identity_invalid",
    message = "documentation field {field} has an invalid identity",
    action = "correct documentation field {field} and retry",
    fields = {
        field: required(string),
        source: optional(source),
    },
);

__rift_error_definition!(
    documentation_limit_exceeded,
    slug = "rift.analysis.documentation_limit_exceeded",
    message = "documentation field {field} exceeds its accepted limit",
    action = "reduce documentation field {field} below its accepted limit and retry",
    fields = {
        field: required(string),
        source: optional(source),
    },
);

__rift_error_definition!(
    documentation_notebook_invalid,
    slug = "rift.analysis.documentation_notebook_invalid",
    message = "documentation field {field} has an invalid notebook shape",
    action = "correct notebook field {field} and retry",
    fields = {
        field: required(string),
        source: optional(source),
    },
);

__rift_error_definition!(
    documentation_order_invalid,
    slug = "rift.analysis.documentation_order_invalid",
    message = "documentation field {field} is not in required order",
    action = "order documentation field {field} and retry",
    fields = {
        field: required(string),
        source: optional(source),
    },
);

__rift_error_definition!(
    documentation_origin_invalid,
    slug = "rift.analysis.documentation_origin_invalid",
    message = "documentation field {field} conflicts with its source address",
    action = "correct documentation field {field} and retry",
    fields = {
        field: required(string),
        source: optional(source),
    },
);

__rift_error_definition!(
    documentation_range_invalid,
    slug = "rift.analysis.documentation_range_invalid",
    message = "documentation field {field} has a range outside source bytes",
    action = "correct documentation field {field} and retry",
    fields = {
        field: required(string),
        source: optional(source),
    },
);

__rift_error_definition!(
    documentation_revision_invalid,
    slug = "rift.analysis.documentation_revision_invalid",
    message = "documentation field {field} has an unsupported publication revision",
    action = "use the supported publication revision and retry",
    fields = {
        field: required(string),
        source: optional(source),
    },
);

__rift_error_definition!(
    documentation_target_missing,
    slug = "rift.analysis.documentation_target_missing",
    message = "documentation field {field} names a missing target",
    action = "supply an existing target for documentation field {field}",
    fields = {
        field: required(string),
        source: optional(source),
    },
);

__rift_error_definition!(
    package_declarations_exceeded,
    slug = "rift.analysis.package_declarations_exceeded",
    message = "package declaration count exceeds its accepted limit",
    action = "reduce package declarations below the accepted limit and retry",
    fields = {
        package: required(string),
        path: optional(path),
    },
);

__rift_error_definition!(
    package_identity_invalid,
    slug = "rift.analysis.package_identity_invalid",
    message = "package identity cannot form a source unit, resolver, or symbol",
    action = "correct package identity and retry",
    fields = {
        cause: optional(cause),
        package: required(string),
        path: optional(path),
    },
);

__rift_error_definition!(
    package_input_duplicate_path,
    slug = "rift.analysis.package_input_duplicate_path",
    message = "package source path appears more than once",
    action = "send each package source path once and retry",
    fields = {
        path: optional(path),
    },
);

__rift_error_definition!(
    package_input_identity_invalid,
    slug = "rift.analysis.package_input_identity_invalid",
    message = "package identity cannot form a source unit",
    action = "correct package identity and retry",
    fields = {
        cause: optional(cause),
        path: optional(path),
    },
);

__rift_error_definition!(
    package_input_origin_invalid,
    slug = "rift.analysis.package_input_origin_invalid",
    message = "package source origin does not identify this package",
    action = "correct package source origin and retry",
    fields = {
        path: optional(path),
    },
);

__rift_error_definition!(
    package_input_too_many_bytes,
    slug = "rift.analysis.package_input_too_many_bytes",
    message = "package source bytes exceed their accepted limit",
    action = "reduce package source bytes below its accepted limit and retry",
    fields = {
        bound: required(unsigned),
        field: required(string),
        observed: required(unsigned),
    },
);

__rift_error_definition!(
    package_input_too_many_files,
    slug = "rift.analysis.package_input_too_many_files",
    message = "package source count exceeds its accepted limit",
    action = "reduce package source count below its accepted limit and retry",
    fields = {
        bound: required(unsigned),
        field: required(string),
        observed: required(unsigned),
    },
);

__rift_error_definition!(
    package_provider_failed,
    slug = "rift.analysis.package_provider_failed",
    message = "package semantic publication failed",
    action = "report this internal failure with its full context",
    fields = {
        cause: optional(cause),
        package: required(string),
        path: optional(path),
        source: optional(source),
    },
);

__rift_error_definition!(
    package_syntax_unavailable,
    slug = "rift.analysis.package_syntax_unavailable",
    message = "no shipped syntax provider accepts package file extension",
    action = "use a supported package file extension and retry",
    fields = {
        package: required(string),
        path: required(path),
    },
);

__rift_error_definition!(
    source_pattern_invalid,
    slug = "rift.analysis.source_pattern_invalid",
    message = "source path pattern is invalid",
    action = "correct the source path pattern and retry",
    fields = {
        pattern: optional(string),
        source: required(source),
    },
);
