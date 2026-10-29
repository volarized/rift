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
    graph_dataset_limit,
    slug = "rift.search.graph_dataset_limit",
    message = "graph dataset needs {required} {bound}, exceeding accepted limit {limit}",
    action = "narrow the facts the dataset is built from below {limit} {bound} and retry",
    fields = {
        bound: required(string),
        limit: required(unsigned),
        required: required(unsigned),
    },
);

__rift_error_definition!(
    graph_identity_invalid,
    slug = "rift.search.graph_identity_invalid",
    message = "graph fact identity {identity} is not an IRI of the form {expected}",
    action = "pass identities in their canonical wire form and retry",
    fields = {
        expected: required(string),
        identity: required(string),
    },
);

__rift_error_definition!(
    graph_limit_zero,
    slug = "rift.search.graph_limit_zero",
    message = "graph query bound {bound} is zero",
    action = "set every graph query bound above zero",
    fields = {
        bound: required(string),
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
    query_answer_limit,
    slug = "rift.search.query_answer_limit",
    message = "SPARQL answer needs more than {limit} {bound}",
    action = "narrow the query with LIMIT or tighter patterns and resend the request",
    fields = {
        bound: required(string),
        limit: required(unsigned),
    },
);

__rift_error_definition!(
    query_cancelled,
    slug = "rift.search.query_cancelled",
    message = "SPARQL evaluation was cancelled",
    action = "resend the request",
    fields = {},
);

__rift_error_definition!(
    query_deadline_exceeded,
    slug = "rift.search.query_deadline_exceeded",
    message = "SPARQL evaluation exceeded its deadline of {timeout_ms} ms",
    action = "narrow the query with tighter patterns and resend the request",
    fields = {
        timeout_ms: required(unsigned),
    },
);

__rift_error_definition!(
    query_evaluation_failed,
    slug = "rift.search.query_evaluation_failed",
    message = "SPARQL evaluation failed: {detail}",
    action = "correct the query and resend the request",
    fields = {
        detail: required(string),
        source: optional(source),
    },
);

__rift_error_definition!(
    query_feature_refused,
    slug = "rift.search.query_feature_refused",
    message = "SPARQL query uses {feature}, which the read-only graph does not serve",
    action = "remove {feature} from the query and resend the request",
    fields = {
        feature: required(string),
    },
);

__rift_error_definition!(
    query_nesting_limit,
    slug = "rift.search.query_nesting_limit",
    message = "SPARQL query nests brackets {depth} deep, exceeding accepted limit {limit}",
    action = "flatten the query below {limit} nested brackets and resend the request",
    fields = {
        depth: required(unsigned),
        limit: required(unsigned),
    },
);

__rift_error_definition!(
    query_operator_limit,
    slug = "rift.search.query_operator_limit",
    message = "SPARQL query writes {count} operator characters, exceeding accepted limit {limit}",
    action = "split the expression below {limit} operators and resend the request",
    fields = {
        count: required(unsigned),
        limit: required(unsigned),
    },
);

__rift_error_definition!(
    query_syntax_invalid,
    slug = "rift.search.query_syntax_invalid",
    message = "SPARQL query does not parse: {detail}",
    action = "correct the SPARQL 1.1 query syntax at the reported position and resend the request",
    fields = {
        detail: required(string),
    },
);

__rift_error_definition!(
    query_too_large,
    slug = "rift.search.query_too_large",
    message = "SPARQL query of {observed} bytes exceeds accepted limit {limit}",
    action = "shorten the query below {limit} bytes and resend the request",
    fields = {
        limit: required(unsigned),
        observed: required(unsigned),
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
