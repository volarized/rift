error_namespace! {
    tracing;

    log_batch_limit {
        message = "log batch holds more records than its accepted limit of {maximum}",
        action = "split the batch below {maximum} records and retry",
        fields = {
            maximum: required(unsigned),
            observed: required(unsigned),
        },
    }

    log_queue_limit {
        message = "log queue records are outside their accepted range of 1 through {maximum}",
        action = "set queue_records within the accepted range and retry",
        fields = {
            maximum: required(unsigned),
            observed: required(unsigned),
        },
    }

    log_store_failed {
        message = "metrics database operation failed: {operation} {path}: {detail}",
        action = "check the metrics database file and its permissions, then retry",
        fields = {
            detail: required(source),
            operation: required(string),
            path: required(path),
        },
    }

    log_stream_unavailable {
        message = "log capture is disabled or closed",
        action = "enable log capture and subscribe before tracing shutdown",
        fields = {},
    }

    log_subscription_limit {
        message = "log subscriptions exceed their accepted limit of {maximum}",
        action = "drop an existing log subscription and retry",
        fields = {
            maximum: required(unsigned),
            observed: required(unsigned),
        },
    }
}
