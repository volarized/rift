use rift_error::__rift_error_definition;

__rift_error_definition!(
    log_batch_limit,
    slug = "rift.tracing.log_batch_limit",
    message = "log batch holds more records than its accepted limit of {maximum}",
    action = "split the batch below {maximum} records and retry",
    fields = {
        maximum: required(unsigned),
        observed: required(unsigned),
    },
);

__rift_error_definition!(
    log_store_failed,
    slug = "rift.tracing.log_store_failed",
    message = "metrics database operation failed: {operation} {path}: {detail}",
    action = "check the metrics database file and its permissions, then retry",
    fields = {
        detail: required(source),
        operation: required(string),
        path: required(path),
    },
);
