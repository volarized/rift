error_namespace! {
    history_store;

    database {
        message = "history store database operation failed: {operation}: {detail}",
        action = "check the history store database and retry",
        fields = {
            detail: required(source),
            operation: required(string),
        },
    }

    folder {
        message = "history store folder operation failed: {operation} {path}: {detail}",
        action = "check the history store path and permissions",
        fields = {
            detail: required(source),
            operation: required(string),
            path: required(path),
        },
    }

    lock_unstable {
        message = "history store live lock changed during {attempts} attempts: {path}",
        action = "retry after concurrent history store cleanup finishes",
        fields = {
            attempts: required(unsigned),
            path: required(path),
        },
    }
}
