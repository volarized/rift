error_namespace! {
    analysis;

    context7_entry_invalid {
        message = "context7.json key {key} contains an invalid entry",
        action = "correct the entry for key {key} and retry",
        fields = {
            entry: optional(string),
            file: required(string),
            key: optional(string),
            source: optional(source),
        },
    }

    context7_malformed {
        message = "context7.json has an invalid JSON shape",
        action = "correct context7.json shape and retry",
        fields = {
            file: required(string),
            key: optional(string),
            source: optional(source),
        },
    }

    context7_oversized {
        message = "context7.json exceeds its accepted byte limit",
        action = "reduce context7.json below its accepted byte limit and retry",
        fields = {
            file: required(string),
            key: optional(string),
            source: optional(source),
        },
    }

    context7_too_many_entries {
        message = "context7.json key {key} exceeds its entry limit",
        action = "reduce entries for key {key} below its accepted limit and retry",
        fields = {
            file: required(string),
            key: optional(string),
            source: optional(source),
        },
    }

    documentation_digest_mismatch {
        message = "documentation field {field} has a digest mismatch",
        action = "supply bytes matching the recorded digest and retry",
        fields = {
            field: required(string),
            source: optional(source),
        },
    }

    documentation_duplicate_source {
        message = "documentation field {field} names a duplicate source",
        action = "send each documentation source once and retry",
        fields = {
            field: required(string),
            source: optional(source),
        },
    }

    documentation_encoding_failed {
        message = "documentation field {field} could not be encoded as canonical JSON",
        action = "report this internal failure with its full context",
        fields = {
            field: required(string),
            source: optional(source),
        },
    }

    documentation_format_invalid {
        message = "documentation field {field} has an invalid format",
        action = "correct documentation field {field} and retry",
        fields = {
            field: required(string),
            source: optional(source),
        },
    }

    documentation_identity_invalid {
        message = "documentation field {field} has an invalid identity",
        action = "correct documentation field {field} and retry",
        fields = {
            field: required(string),
            source: optional(source),
        },
    }

    documentation_limit_exceeded {
        message = "documentation field {field} exceeds its accepted limit",
        action = "reduce documentation field {field} below its accepted limit and retry",
        fields = {
            field: required(string),
            source: optional(source),
        },
    }

    documentation_notebook_invalid {
        message = "documentation field {field} has an invalid notebook shape",
        action = "correct notebook field {field} and retry",
        fields = {
            field: required(string),
            source: optional(source),
        },
    }

    documentation_order_invalid {
        message = "documentation field {field} is not in required order",
        action = "order documentation field {field} and retry",
        fields = {
            field: required(string),
            source: optional(source),
        },
    }

    documentation_origin_invalid {
        message = "documentation field {field} conflicts with its source address",
        action = "correct documentation field {field} and retry",
        fields = {
            field: required(string),
            source: optional(source),
        },
    }

    documentation_range_invalid {
        message = "documentation field {field} has a range outside source bytes",
        action = "correct documentation field {field} and retry",
        fields = {
            field: required(string),
            source: optional(source),
        },
    }

    documentation_revision_invalid {
        message = "documentation field {field} has an unsupported publication revision",
        action = "use the supported publication revision and retry",
        fields = {
            field: required(string),
            source: optional(source),
        },
    }

    documentation_target_missing {
        message = "documentation field {field} names a missing target",
        action = "supply an existing target for documentation field {field}",
        fields = {
            field: required(string),
            source: optional(source),
        },
    }

    package_artifact_invalid {
        message = "selected artifact field {field} is invalid: {violation}",
        action = "replace or rebuild the invalid publication, then retry",
        fields = {
            bound: optional(unsigned),
            cause: optional(cause),
            field: required(string),
            observed: optional(unsigned),
            violation: required(string),
        },
    }

    package_declarations_exceeded {
        message = "package declaration count exceeds its accepted limit",
        action = "reduce package declarations below the accepted limit and retry",
        fields = {
            package: required(string),
            path: optional(path),
        },
    }

    package_identity_invalid {
        message = "package identity cannot form a source unit, resolver, or symbol",
        action = "correct package identity and retry",
        fields = {
            cause: optional(cause),
            package: required(string),
            path: optional(path),
        },
    }

    package_input_duplicate_path {
        message = "package source path appears more than once",
        action = "send each package source path once and retry",
        fields = {
            path: optional(path),
        },
    }

    package_input_identity_invalid {
        message = "package identity cannot form a source unit",
        action = "correct package identity and retry",
        fields = {
            cause: optional(cause),
            path: optional(path),
        },
    }

    package_input_import_limit_exceeded {
        message = "package import roots exceed their accepted limit",
        action = "reduce package import roots below their accepted limit and retry",
        fields = {
            bound: required(unsigned),
            field: required(string),
            observed: required(unsigned),
        },
    }

    package_input_origin_invalid {
        message = "package source origin does not identify this package",
        action = "correct package source origin and retry",
        fields = {
            path: optional(path),
        },
    }

    package_input_too_many_bytes {
        message = "package source bytes exceed their accepted limit",
        action = "reduce package source bytes below its accepted limit and retry",
        fields = {
            bound: required(unsigned),
            field: required(string),
            observed: required(unsigned),
        },
    }

    package_input_too_many_files {
        message = "package source count exceeds its accepted limit",
        action = "reduce package source count below its accepted limit and retry",
        fields = {
            bound: required(unsigned),
            field: required(string),
            observed: required(unsigned),
        },
    }

    package_provider_failed {
        message = "package semantic publication failed",
        action = "report this internal failure with its full context",
        fields = {
            cause: optional(cause),
            package: required(string),
            path: optional(path),
            source: optional(source),
        },
    }

    package_retained_source_bytes_exceeded {
        message = "retained source strings and their copies exceed their accepted byte limit",
        action = "raise the retained source total within the caller's memory bound or reduce retained source bytes",
        fields = {
            bound: required(unsigned),
            observed: required(unsigned),
        },
    }

    package_retained_source_limits_invalid {
        message = "retained source limits are invalid",
        action = "choose a positive record bound at or below its ceiling and a total at least as large",
        fields = {
            record_bytes_ceiling: required(unsigned),
            record_bytes_max: required(unsigned),
            total_bytes_max: required(unsigned),
        },
    }

    package_syntax_unavailable {
        message = "no shipped syntax provider accepts package file extension",
        action = "use a supported package file extension and retry",
        fields = {
            package: required(string),
            path: required(path),
        },
    }

    producer_fact_invalid {
        message = "producer fact does not match the publication: field {field}, violation {violation}",
        action = "correct the reported field of the producer facts and retry",
        fields = {
            field: required(string),
            violation: required(string),
        },
    }

    producer_range_unresolved {
        message = "producer fact range resolves to no syntax element of its source unit: unit {unit}",
        action = "name a byte range inside a source unit the publication carries and retry",
        fields = {
            end: required(unsigned),
            start: required(unsigned),
            unit: required(string),
        },
    }

    source_pattern_invalid {
        message = "source path pattern is invalid",
        action = "correct the source path pattern and retry",
        fields = {
            pattern: optional(string),
            source: required(source),
        },
    }
}
