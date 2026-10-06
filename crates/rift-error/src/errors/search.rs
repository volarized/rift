use rift_error::__rift_error_definition;

__rift_error_definition!(
    encode_failed,
    slug = "rift.search.encode_failed",
    message = "text encoding failed",
    action = "check model input and retry",
    fields = {
        source: optional(source),
        stage: optional(string),
    },
);

__rift_error_definition!(
    model_cache_unavailable,
    slug = "rift.search.model_cache_unavailable",
    message = "model cache directory could not be resolved from environment variables {variables}",
    action = "set a model cache directory and retry",
    fields = {
        variables: required(string),
    },
);

__rift_error_definition!(
    model_configuration_invalid,
    slug = "rift.search.model_configuration_invalid",
    message = "model configuration is invalid",
    action = "provide a model configuration this encoder serves and retry",
    fields = {
        path: optional(path),
        source: optional(source),
    },
);

__rift_error_definition!(
    model_download_failed,
    slug = "rift.search.model_download_failed",
    message = "model file download failed: {subject}",
    action = "check network access and retry",
    fields = {
        source: optional(source),
        subject: required(string),
    },
);

__rift_error_definition!(
    model_download_too_large,
    slug = "rift.search.model_download_too_large",
    message = "model file body at {url} is empty or exceeds accepted byte limit {bytes_max}",
    action = "provide a nonempty model file within its byte limit and retry",
    fields = {
        bytes_max: required(unsigned),
        url: required(string),
    },
);

__rift_error_definition!(
    model_file_missing,
    slug = "rift.search.model_file_missing",
    message = "model directory is missing file {subject}",
    action = "supply the missing model file and retry",
    fields = {
        subject: required(path),
    },
);

__rift_error_definition!(
    model_source_invalid,
    slug = "rift.search.model_source_invalid",
    message = "model source {model} has invalid form; expected {expected}",
    action = "use a model source in the expected form and retry",
    fields = {
        expected: required(string),
        model: required(string),
        source: optional(source),
    },
);

__rift_error_definition!(
    task_failed,
    slug = "rift.search.task_failed",
    message = "search task did not return",
    action = "retry search",
    fields = {
        source: optional(source),
    },
);

__rift_error_definition!(
    text_limit,
    slug = "rift.search.text_limit",
    message = "encoder received {observed} texts, exceeding accepted limit {limit}",
    action = "reduce input texts below {limit} and retry",
    fields = {
        limit: required(unsigned),
        observed: required(unsigned),
    },
);

__rift_error_definition!(
    tokenizer_unreadable,
    slug = "rift.search.tokenizer_unreadable",
    message = "model tokenizer is unreadable",
    action = "repair the model tokenizer file and retry",
    fields = {
        path: optional(path),
        source: optional(source),
    },
);

__rift_error_definition!(
    vector_coordinate_invalid,
    slug = "rift.search.vector_coordinate_invalid",
    message = "embedded coordinate {coordinate} cannot be represented in the stored f32 format",
    action = "use an embedding model that returns finite coordinates within the stored range",
    fields = {
        coordinate: required(string),
    },
);

__rift_error_definition!(
    vector_width_mismatch,
    slug = "rift.search.vector_width_mismatch",
    message = "query vector width {query_width} does not match stored vector width {stored_width}",
    action = "use vectors with the stored width and retry",
    fields = {
        query_width: required(unsigned),
        stored_width: required(unsigned),
    },
);

__rift_error_definition!(
    weights_unreadable,
    slug = "rift.search.weights_unreadable",
    message = "model weights are unreadable",
    action = "repair the model weights file and retry",
    fields = {
        path: optional(path),
        source: optional(source),
    },
);
