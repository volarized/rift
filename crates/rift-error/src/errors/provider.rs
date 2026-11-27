error_namespace! {
    provider;

    cache_too_many_keys {
        message = "cache key count exceeds its configured limit",
        action = "reduce cache keys to its configured limit and retry",
        fields = {},
    }

    composition_dangling_input {
        message = "composition stage still has a consumer",
        action = "remove consumers before removing stage and retry",
        fields = {
            stage: required(string),
        },
    }

    composition_duplicate_stage {
        message = "composition stage path already exists",
        action = "use a unique stage path and retry",
        fields = {
            stage: required(string),
        },
    }

    composition_foreign_flow {
        message = "composition flow belongs to another builder",
        action = "use flow handles from this builder and retry",
        fields = {},
    }

    composition_invalid_name {
        message = "composition stage name is invalid",
        action = "use a canonical stage name and retry",
        fields = {},
    }

    composition_missing_output {
        message = "composition has no selected output",
        action = "select one output stage and retry",
        fields = {},
    }

    composition_stage_not_found {
        message = "composition stage does not exist",
        action = "use an existing stage path and retry",
        fields = {
            stage: required(string),
        },
    }

    composition_type_mismatch {
        message = "composition stage input and output types do not match",
        action = "connect stages with matching types and retry",
        fields = {
            stage: required(string),
        },
    }

    publication_contribution_limit {
        message = "total contribution count exceeds its configured limit",
        action = "reduce total contributions to configured limit and retry",
        fields = {
            field: required(string),
        },
    }

    publication_duplicate_symbol {
        message = "provider publication repeats a provider symbol",
        action = "publish each provider symbol once and retry",
        fields = {
            field: required(string),
        },
    }

    publication_provider_contribution_limit {
        message = "provider contribution count exceeds its configured limit",
        action = "reduce provider contributions to configured limit and retry",
        fields = {
            field: required(string),
        },
    }

    publication_provider_limit {
        message = "provider publication count exceeds its configured limit",
        action = "reduce provider publications to configured limit and retry",
        fields = {
            field: required(string),
        },
    }

    publication_provider_mismatch {
        message = "contribution provider does not match publication provider",
        action = "publish contributions under matching provider identity and retry",
        fields = {
            field: required(string),
        },
    }

    publication_revision_mismatch {
        message = "contribution revision does not match publication revision",
        action = "publish contributions under matching revision and retry",
        fields = {
            field: required(string),
        },
    }

    publication_zero_limit {
        message = "provider publication limit is zero",
        action = "set each provider publication limit above zero and retry",
        fields = {
            field: required(string),
        },
    }
}
